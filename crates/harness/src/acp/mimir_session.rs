//! Negotiated public session state and read-only child revisions. No disk access,
//! child execution owners, inferred agent identities, or model prompt requests.
use super::normalize::UpdateNormalizer;
use crate::{HarnessError, jsonrpc::RpcClient};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use kratos_proto::{AgentChild, AgentEvent, ChildTranscript, GoalState, ToolCall};

pub(super) const KEY: &str = "mimir.dev/session";

#[derive(Clone, Copy, Default)]
pub(super) struct Capabilities {
    pub goal: bool,
    pub children: bool,
}
impl Capabilities {
    pub fn parse(init: &Value) -> Self {
        let caps = &init["agentCapabilities"]["_meta"][KEY];
        if caps["version"] != 1 {
            return Self::default();
        }
        Self {
            goal: caps["goalControl"] == true,
            children: caps["childTranscript"] == true,
        }
    }
    pub fn enabled(self) -> bool {
        self.goal || self.children
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    session_id: String,
    goal: Option<GoalState>,
    children: Vec<AgentChild>,
    children_truncated: bool,
}

#[derive(Default)]
struct SharedState {
    current: Option<State>,
    child_epochs: HashMap<String, u64>,
}

type Shared = Arc<Mutex<SharedState>>;

const MAX_CHILD_FETCH_ATTEMPTS: u8 = 3;

struct ChildFetchState {
    child: AgentChild,
    epoch: u64,
    revision: Option<String>,
    failures: u8,
}

impl ChildFetchState {
    fn same_lifecycle(&self, child: &AgentChild, epoch: u64) -> bool {
        self.epoch == epoch
            && self.child.tool_call_id == child.tool_call_id
            && self.child.status == child.status
            && self.child.transcript == child.transcript
    }
}

pub(super) struct NativeSession {
    shared: Shared,
    session_id: String,
    pub caps: Capabilities,
    wake: watch::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for NativeSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl NativeSession {
    pub fn new(
        client: RpcClient,
        session_id: String,
        caps: Capabilities,
        tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    ) -> Self {
        let shared: Shared = Arc::new(Mutex::new(SharedState::default()));
        let (wake, mut rx) = watch::channel(());
        let state = shared.clone();
        let parent = session_id.clone();
        let task = tokio::spawn(async move {
            if let Ok(value) = client
                .request("_mimir/session/state", json!({"sessionId": parent}))
                .await
            {
                for event in apply_state(&state, &parent, caps, &value, true) {
                    if tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            let mut fetched: HashMap<String, ChildFetchState> = HashMap::new();
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {},
                    changed = rx.changed() => { if changed.is_err() { return; } },
                    _ = tx.closed() => return,
                }
                if !caps.children {
                    continue;
                }
                let children = {
                    let shared = state.lock().expect("session state");
                    shared
                        .current
                        .as_ref()
                        .map(|state| {
                            state
                                .children
                                .iter()
                                .map(|child| {
                                    (
                                        child.clone(),
                                        shared
                                            .child_epochs
                                            .get(&child.child_id)
                                            .copied()
                                            .unwrap_or(0),
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                };
                fetched.retain(|id, _| children.iter().any(|(c, _)| &c.child_id == id));
                for (child, epoch) in children {
                    if !child.transcript {
                        fetched.remove(&child.child_id);
                        continue;
                    }
                    let terminal = !matches!(child.status.as_str(), "running" | "queued");
                    if let Some(previous) = fetched.get(&child.child_id)
                        && previous.same_lifecycle(&child, epoch)
                        && (previous.failures >= MAX_CHILD_FETCH_ATTEMPTS
                            || terminal && previous.revision.is_some() && previous.child == child)
                    {
                        continue;
                    }
                    // Queued children can legitimately have no readable history yet.
                    let snapshot = match fetch_child(&client, &parent, &child.child_id).await {
                        Ok(snapshot) => snapshot,
                        Err(_) => {
                            // Do not charge an obsolete lifecycle when a state update raced the RPC.
                            if current_child(&state, &child.child_id)
                                == Some((child.clone(), epoch))
                            {
                                let entry =
                                    fetched.entry(child.child_id.clone()).or_insert_with(|| {
                                        ChildFetchState {
                                            child: child.clone(),
                                            epoch,
                                            revision: None,
                                            failures: 0,
                                        }
                                    });
                                if !entry.same_lifecycle(&child, epoch) {
                                    *entry = ChildFetchState {
                                        child: child.clone(),
                                        epoch,
                                        revision: None,
                                        failures: 1,
                                    };
                                } else {
                                    entry.child = child.clone();
                                    entry.revision = None;
                                    entry.failures = entry.failures.saturating_add(1);
                                }
                            }
                            continue;
                        }
                    };
                    // A lifecycle/attempt notification can race pagination.
                    // Never publish an older running state over a newer terminal
                    // one; the pending watch change performs its final fetch.
                    if current_child(&state, &child.child_id) != Some((child.clone(), epoch)) {
                        continue;
                    }
                    let changed = fetched.get(&child.child_id).is_none_or(|previous| {
                        previous.child != child
                            || previous.revision.as_deref() != Some(&snapshot.revision)
                    });
                    fetched.insert(
                        child.child_id.clone(),
                        ChildFetchState {
                            child: child.clone(),
                            epoch,
                            revision: Some(snapshot.revision.clone()),
                            failures: 0,
                        },
                    );
                    if changed
                        && tx
                            .send(Ok(AgentEvent::SubagentView {
                                child,
                                snapshot: Some(snapshot),
                            }))
                            .await
                            .is_err()
                    {
                        return;
                    }
                }
            }
        });
        Self {
            shared,
            session_id,
            caps,
            wake,
            task,
        }
    }

    pub fn state(&mut self, value: &Value) -> Vec<AgentEvent> {
        let events = apply_state(&self.shared, &self.session_id, self.caps, value, false);
        self.wake.send_replace(());
        events
    }

    pub fn map(&self, update: &Value, normalizer: &mut UpdateNormalizer) -> Vec<AgentEvent> {
        let mut events = normalizer.map_update(update);
        let state = self.shared.lock().expect("session state");
        for event in &mut events {
            if let AgentEvent::ToolCall { id, call } = event
                && let Some(child) = state.current.as_ref().and_then(|s| {
                    s.children
                        .iter()
                        .find(|child| child.tool_call_id.as_ref() == Some(id))
                })
            {
                *call = ToolCall::Unknown {
                    name: format!("Agent: {}", child.title),
                    input: None,
                };
            }
        }
        events
    }
}

fn current_child(shared: &Shared, child_id: &str) -> Option<(AgentChild, u64)> {
    let shared = shared.lock().expect("session state");
    let child = shared
        .current
        .as_ref()?
        .children
        .iter()
        .find(|child| child.child_id == child_id)?
        .clone();
    let epoch = shared.child_epochs.get(child_id).copied().unwrap_or(0);
    Some((child, epoch))
}

fn apply_state(
    shared: &Shared,
    parent: &str,
    caps: Capabilities,
    value: &Value,
    initial_pull: bool,
) -> Vec<AgentEvent> {
    let Ok(state) = serde_json::from_value::<State>(value.clone()) else {
        return vec![];
    };
    if state.session_id != parent {
        return vec![];
    }
    let mut shared = shared.lock().expect("session state");
    // RPC responses bypass the notification queue. A late initial pull must
    // not roll back a newer pushed goal or child linkage snapshot.
    if initial_pull && shared.current.is_some() {
        return Vec::new();
    }
    let previous = shared.current.clone();
    if caps.children && previous.is_some() {
        for child in &state.children {
            let changed = previous.as_ref().is_some_and(|old| {
                old.children
                    .iter()
                    .find(|old| old.child_id == child.child_id)
                    .is_none_or(|old| {
                        old.tool_call_id != child.tool_call_id
                            || old.status != child.status
                            || old.transcript != child.transcript
                    })
            });
            if changed {
                let epoch = shared
                    .child_epochs
                    .entry(child.child_id.clone())
                    .or_default();
                *epoch = epoch.wrapping_add(1);
            }
        }
    }
    let mut events = Vec::new();
    if caps.goal && previous.as_ref().is_none_or(|old| old.goal != state.goal) {
        events.push(AgentEvent::GoalState {
            goal: state.goal.clone(),
        });
    }
    if caps.children {
        for child in &state.children {
            if previous
                .as_ref()
                .is_none_or(|old| !old.children.contains(child))
            {
                events.push(AgentEvent::SubagentView {
                    child: child.clone(),
                    snapshot: None,
                });
            }
        }
        if state.children_truncated && previous.as_ref().is_none_or(|s| !s.children_truncated) {
            tracing::warn!(session = parent, "agent's public child list is truncated");
        }
    }
    shared.current = Some(state);
    events
}

async fn fetch_child(
    client: &RpcClient,
    parent: &str,
    child: &str,
) -> Result<ChildTranscript, HarnessError> {
    // A revision changing mid-pagination is expected during live work. Restart
    // a bounded number of times, then let the next refresh try a fresh revision.
    'revision: for _ in 0..3 {
        let mut cursor = 0u64;
        let mut revision: Option<String> = None;
        let mut updates = Vec::new();
        let mut omitted = 0;
        loop {
            let mut params = json!({"sessionId": parent, "childId": child, "cursor": cursor});
            if let Some(revision) = &revision {
                params["revision"] = revision.clone().into();
            }
            let page = match client.request("_mimir/session/child", params).await {
                Ok(page) => page,
                Err(_) if cursor > 0 => continue 'revision,
                Err(error) => return Err(error),
            };
            if page["sessionId"] != parent || page["childId"] != child {
                return Err(HarnessError::Protocol(
                    "child snapshot identity mismatch".into(),
                ));
            }
            let next_revision = page["revision"]
                .as_str()
                .ok_or_else(|| HarnessError::Protocol("child snapshot missing revision".into()))?;
            if revision
                .as_ref()
                .is_some_and(|previous| previous != next_revision)
            {
                continue 'revision;
            }
            revision = Some(next_revision.into());
            let page_updates = page["updates"]
                .as_array()
                .ok_or_else(|| HarnessError::Protocol("child snapshot missing updates".into()))?;
            updates.extend(page_updates.iter().cloned());
            omitted += page["omittedUpdates"].as_u64().unwrap_or(0);
            if let Some(next) = page["nextCursor"].as_u64() {
                if next <= cursor {
                    return Err(HarnessError::Protocol(
                        "child cursor did not advance".into(),
                    ));
                }
                cursor = next;
            } else {
                let mut normalizer = UpdateNormalizer::default();
                let events = updates
                    .iter()
                    .flat_map(|update| normalizer.map_update(update))
                    .filter(|event| {
                        !matches!(
                            event,
                            AgentEvent::ReasoningDelta { .. } | AgentEvent::UserMessage { .. }
                        )
                    })
                    .collect();
                return Ok(ChildTranscript {
                    revision: revision.expect("page revision"),
                    events,
                    omitted_updates: omitted,
                });
            }
        }
    }
    Err(HarnessError::Protocol(
        "child revision changed during pagination".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negotiation_requires_exact_version_and_individual_capabilities() {
        assert!(!Capabilities::parse(&json!({})).enabled());
        assert!(
            !Capabilities::parse(
                &json!({"agentCapabilities":{"_meta":{KEY:{"version":2,"goalControl":true}}}})
            )
            .enabled()
        );
        let caps = Capabilities::parse(
            &json!({"agentCapabilities":{"_meta":{KEY:{"version":1,"goalControl":true,"childTranscript":false}}}}),
        );
        assert!(caps.goal);
        assert!(!caps.children);
    }
    #[test]
    fn state_is_scoped_replacement_and_semantic_not_title_inference() {
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let caps = Capabilities {
            goal: true,
            children: true,
        };
        let state = json!({"sessionId":"parent", "goal":null,"children":[{"childId":"child","toolCallId":"tool","title":"Explore files","agent":"explore","status":"running","transcript":true}],"childrenTruncated":false});
        assert!(apply_state(&shared, "foreign", caps, &state, false).is_empty());
        let events = apply_state(&shared, "parent", caps, &state, false);
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[1], AgentEvent::SubagentView { child, snapshot: None } if child.child_id == "child" && child.tool_call_id.as_deref() == Some("tool"))
        );
        assert!(apply_state(&shared, "parent", caps, &state, false).is_empty());
        let initial =
            json!({"sessionId":"parent", "goal":null, "children":[], "childrenTruncated":false});
        assert!(apply_state(&shared, "parent", caps, &initial, true).is_empty());
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .current
                .as_ref()
                .unwrap()
                .children
                .len(),
            1
        );
    }
}
