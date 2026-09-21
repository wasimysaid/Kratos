//! Real ACP decoding, native controls, and replacement child docs through public engine APIs.
#![cfg(unix)]

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use kratos_doc::{MessagePart, MessageRole, MessageStatus, SessionCommandPayload, SubagentStatus};
use kratos_engine::{EngineCore, HarnessRegistry};
use kratos_harness::{AcpHarness, Harness, HarnessError, RunControls};
use kratos_proto::{
    AgentChild, AgentEvent, ChatConfig, ChildTranscript, DoneStatus, GoalPhase, HarnessId, Model,
    ReasoningLevel, RunRequest, SandboxLevel, SessionStatus, SteeringMode, ToolCall,
};
const CHAT: &str = "native-goal";

fn core(path: &Path) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(
        AcpHarness::mimir().with_executable(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../harness/tests/fixtures/mimir-extension.py"),
        ),
    ));
    EngineCore::assemble(path, Arc::new(registry), HarnessId::Mimir, None).unwrap()
}
fn request(path: &Path, prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: Some(HarnessId::Mimir),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: path.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: vec![],
        worktree: None,
    }
}
fn create(core: &EngineCore, path: &Path) {
    core.workspace
        .create_chat(
            CHAT,
            None,
            Some(&core.device_id),
            Some(ChatConfig {
                harness: HarnessId::Mimir,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            }),
            Some(path.display().to_string()),
        )
        .unwrap();
}
async fn wait(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("public engine state advances");
}
fn child_ref(core: &EngineCore) -> Option<String> {
    core.doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap()
        .into_iter()
        .flat_map(|e| e.parts)
        .find_map(|p| match p {
            MessagePart::Tool {
                id,
                subagent_ref,
                call,
                ..
            } if id == "launch-1" && call.is_subagent_spawn() => subagent_ref,
            _ => None,
        })
}
fn text(core: &EngineCore, doc: &str) -> String {
    core.doc_host
        .open(doc)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap()
        .into_iter()
        .flat_map(|e| e.parts)
        .filter_map(|p| match p {
            MessagePart::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

struct LifecycleMimir {
    negotiate_first: bool,
    runs: AtomicUsize,
}

impl LifecycleMimir {
    fn extension_absent() -> Self {
        Self {
            negotiate_first: false,
            runs: AtomicUsize::new(0),
        }
    }

    fn negotiated_once() -> Self {
        Self {
            negotiate_first: true,
            runs: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Harness for LifecycleMimir {
    fn id(&self) -> HarnessId {
        HarnessId::Mimir
    }

    fn display_name(&self) -> &str {
        "Mimir lifecycle fixture"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        let mut initial = vec![Ok(AgentEvent::SessionStarted {
            harness: HarnessId::Mimir,
            model: "fixture".into(),
            tools: Vec::new(),
            cwd: request.cwd,
            session_id: format!("lifecycle-{run}"),
            assistant_message_id: format!("assistant-{run}"),
        })];
        if self.negotiate_first && run == 0 {
            initial.push(Ok(AgentEvent::GoalState { goal: None }));
        }
        let tail = futures::stream::unfold(Some(controls), move |state| async move {
            let mut controls = state?;
            tokio::select! {
                _ = controls.interrupt.cancelled() => Some((Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(format!("lifecycle-{run}")),
                }), None)),
                steer = controls.steering.recv() => steer.map(|steer| (Ok(AgentEvent::Steered {
                    assistant_message_id: None,
                    next_assistant_message_id: steer.message_id,
                }), Some(controls))),
            }
        });
        Ok(futures::stream::iter(initial).chain(tail).boxed())
    }
}

struct StatusOnlyChildMimir {
    terminal_snapshot: Arc<tokio::sync::Notify>,
}

impl StatusOnlyChildMimir {
    fn child(status: &str) -> AgentChild {
        AgentChild {
            child_id: "status-child".into(),
            tool_call_id: Some("launch-1".into()),
            title: "Inspect files".into(),
            agent: "explore".into(),
            status: status.into(),
            transcript: true,
        }
    }

    fn snapshot(revision: &str, text: &str) -> ChildTranscript {
        ChildTranscript {
            revision: revision.into(),
            events: vec![AgentEvent::TextDelta { text: text.into() }],
            omitted_updates: 0,
        }
    }
}

#[async_trait]
impl Harness for StatusOnlyChildMimir {
    fn id(&self) -> HarnessId {
        HarnessId::Mimir
    }

    fn display_name(&self) -> &str {
        "Mimir status-only child fixture"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let initial = vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mimir,
                model: "fixture".into(),
                tools: Vec::new(),
                cwd: request.cwd,
                session_id: "status-only-child".into(),
                assistant_message_id: "assistant-status-only".into(),
            }),
            Ok(AgentEvent::ToolCall {
                id: "launch-1".into(),
                call: ToolCall::Unknown {
                    name: "Agent: Inspect files".into(),
                    input: None,
                },
            }),
            Ok(AgentEvent::SubagentView {
                child: Self::child("running"),
                snapshot: Some(Self::snapshot("running", "RUNNING_PUBLIC_CHILD")),
            }),
            Ok(AgentEvent::SubagentView {
                child: Self::child("completed"),
                snapshot: None,
            }),
        ];
        let notify = self.terminal_snapshot.clone();
        let terminal = futures::stream::once(async move {
            notify.notified().await;
            Ok(AgentEvent::SubagentView {
                child: Self::child("completed"),
                snapshot: Some(Self::snapshot("completed", "FINAL_PUBLIC_CHILD")),
            })
        });
        let done = futures::stream::once(async {
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("status-only-child".into()),
            })
        });
        Ok(futures::stream::iter(initial)
            .chain(terminal)
            .chain(done)
            .boxed())
    }
}

#[tokio::test]
async fn terminal_status_only_settles_existing_public_child_until_final_snapshot_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let registry = HarnessRegistry::new();
    let terminal_snapshot = Arc::new(tokio::sync::Notify::new());
    registry.register(Arc::new(StatusOnlyChildMimir {
        terminal_snapshot: terminal_snapshot.clone(),
    }));
    let core =
        EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mimir, None).unwrap();
    create(&core, dir.path());

    core.sessions
        .dispatch(
            CHAT,
            HarnessId::Mimir,
            request(dir.path(), "status-only child"),
            None,
        )
        .await
        .unwrap();
    wait(|| {
        core.doc_host
            .open(CHAT)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .iter()
            .flat_map(|entry| &entry.parts)
            .any(|part| {
                matches!(part,
                    MessagePart::Tool { id, subagent_status: Some(SubagentStatus::Done), .. }
                        if id == "launch-1"
                )
            })
    })
    .await;

    let child = child_ref(&core).expect("running snapshot linked a public child doc");
    let entries = core
        .doc_host
        .open(&child)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "public-child");
    assert_eq!(entries[0].status, Some(MessageStatus::Complete));
    assert_eq!(text(&core, &child), "RUNNING_PUBLIC_CHILD");

    terminal_snapshot.notify_one();
    wait(|| text(&core, &child) == "FINAL_PUBLIC_CHILD").await;
    let entries = core
        .doc_host
        .open(&child)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, Some(MessageStatus::Complete));
    assert_eq!(text(&core, &child), "FINAL_PUBLIC_CHILD");
    core.shutdown().await;
}

#[tokio::test]
async fn extension_absent_goal_control_stays_in_the_visible_turn_boundary_queue() {
    let dir = tempfile::tempdir().unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(LifecycleMimir::extension_absent()));
    let core =
        EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mimir, None).unwrap();
    create(&core, dir.path());
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::Mimir,
            request(dir.path(), "start without extension"),
            None,
        )
        .await
        .unwrap();
    wait(|| {
        core.sessions
            .session_status(CHAT)
            .is_some_and(|session| session.status == SessionStatus::Working)
    })
    .await;

    let command_id = core
        .doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "/goal pause".into(),
                message_id: Some("pause-without-extension".into()),
            },
        )
        .unwrap();
    wait(|| {
        core.doc_host
            .open(CHAT)
            .unwrap()
            .doc()
            .read_commands()
            .unwrap()
            .iter()
            .any(|command| {
                command.id == command_id
                    && command.status == kratos_doc::SessionCommandStatus::Applied
            })
    })
    .await;

    let handle = core.doc_host.open(CHAT).unwrap();
    assert_eq!(
        handle
            .doc()
            .read_queue()
            .unwrap()
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["/goal pause"],
        "without negotiated goalControl, the command must remain visible and editable"
    );
    assert!(
        handle
            .doc()
            .read_entries()
            .unwrap()
            .iter()
            .all(|entry| entry.id != "pause-without-extension"),
        "a held control is not shown as already sent"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn negotiated_capability_survives_a_same_run_steered_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(LifecycleMimir::negotiated_once()));
    let core =
        EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mimir, None).unwrap();
    create(&core, dir.path());
    core.sessions
        .dispatch(CHAT, HarnessId::Mimir, request(dir.path(), "start"), None)
        .await
        .unwrap();
    wait(|| {
        core.sessions
            .session_status(CHAT)
            .is_some_and(|s| s.goal_control)
    })
    .await;

    core.sessions
        .steer(CHAT, "ordinary follow-up", Some("ordinary".into()))
        .await
        .unwrap();
    wait(|| {
        core.sessions
            .subscribe(CHAT, 0)
            .unwrap()
            .0
            .iter()
            .any(|event| matches!(event.event, AgentEvent::Steered { .. }))
    })
    .await;
    assert!(core.sessions.session_status(CHAT).unwrap().goal_control);

    let command = core
        .doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "/goal pause".into(),
                message_id: Some("pause-after-boundary".into()),
            },
        )
        .unwrap();
    wait(|| {
        core.doc_host
            .open(CHAT)
            .unwrap()
            .doc()
            .read_commands()
            .unwrap()
            .iter()
            .any(|entry| {
                entry.id == command && entry.status == kratos_doc::SessionCommandStatus::Applied
            })
    })
    .await;
    assert!(
        core.doc_host
            .open(CHAT)
            .unwrap()
            .doc()
            .read_queue()
            .unwrap()
            .is_empty()
    );
    core.shutdown().await;
}

#[tokio::test]
async fn authoritative_retirement_clears_capability_before_an_extension_absent_run() {
    let dir = tempfile::tempdir().unwrap();
    let registry = HarnessRegistry::new();
    let harness = Arc::new(LifecycleMimir::negotiated_once());
    registry.register(harness.clone());
    let core =
        EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mimir, None).unwrap();
    create(&core, dir.path());
    core.sessions
        .dispatch(CHAT, HarnessId::Mimir, request(dir.path(), "first"), None)
        .await
        .unwrap();
    wait(|| {
        core.sessions
            .session_status(CHAT)
            .is_some_and(|s| s.goal_control)
    })
    .await;

    core.sessions.interrupt(CHAT).await.unwrap();
    wait(|| {
        core.sessions
            .session_status(CHAT)
            .is_some_and(|s| !s.goal_control)
            && core
                .workspace
                .read_sessions()
                .unwrap()
                .iter()
                .any(|session| session.chat_id == CHAT && !session.goal_control)
    })
    .await;

    core.sessions
        .dispatch(CHAT, HarnessId::Mimir, request(dir.path(), "second"), None)
        .await
        .unwrap();
    wait(|| harness.runs.load(Ordering::SeqCst) == 2).await;
    assert!(!core.sessions.session_status(CHAT).unwrap().goal_control);
    assert!(!core.sessions.is_control_prompt(CHAT, "/goal pause"));
    core.shutdown().await;
}

#[tokio::test]
async fn active_goal_controls_bypass_queue_without_success_or_extra_model_lease() {
    let dir = tempfile::tempdir().unwrap();
    let core = core(dir.path());
    create(&core, dir.path());
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::Mimir,
            request(dir.path(), "start native goal"),
            None,
        )
        .await
        .unwrap();
    wait(|| {
        core.sessions
            .session_status(CHAT)
            .is_some_and(|s| s.goal_control && s.goal.is_some_and(|g| g.phase == GoalPhase::Active))
            && child_ref(&core).is_some()
    })
    .await;
    let child = child_ref(&core).unwrap();
    assert_eq!(text(&core, &child), "PUBLIC_CHILD");
    assert_eq!(
        core.doc_host
            .fetch_tool_blob(&format!("{child}/child-tool"))
            .await
            .unwrap(),
        "PUBLIC_OUTPUT"
    );
    let child_handle = core.doc_host.open(&child).unwrap();
    let first = child_handle.doc().doc().oplog_vv();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(
        child_handle.doc().doc().oplog_vv(),
        first,
        "same snapshot must not append history"
    );
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "/goal pause".into(),
                message_id: Some("pause-user".into()),
            },
        )
        .unwrap();
    // Establish which independently delivered durable command is in flight
    // before submitting an identical follower (delivery tasks may race).
    wait(|| {
        std::fs::read_to_string(dir.path().join("extension-requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .any(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .is_ok_and(|frame| frame["method"] == "_mimir/session/goal")
            })
    })
    .await;
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "/goal pause".into(),
                message_id: Some("duplicate-user".into()),
            },
        )
        .unwrap();
    wait(|| {
        core.sessions.session_status(CHAT).is_some_and(|s| {
            s.status == SessionStatus::Idle && s.goal.is_some_and(|g| g.phase == GoalPhase::Paused)
        }) && text(&core, &child) == "FINAL_PUBLIC_CHILD"
    })
    .await;
    assert!(
        core.sessions
            .session_status(CHAT)
            .unwrap()
            .last_completed_turn
            .is_none(),
        "goal pause is not successful completion"
    );
    let users = core
        .doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert_eq!(
        users
            .iter()
            .find(|e| e.id == "duplicate-user")
            .unwrap()
            .status,
        Some(kratos_doc::MessageStatus::Aborted)
    );
    assert_eq!(
        users.iter().find(|e| e.id == "pause-user").unwrap().status,
        Some(kratos_doc::MessageStatus::Complete)
    );
    assert_eq!(
        core.doc_host
            .fetch_tool_blob(&format!("{child}/child-tool"))
            .await
            .unwrap(),
        "FINAL_OUTPUT"
    );
    let child_entries = child_handle.doc().read_entries().unwrap();
    assert_eq!(child_entries.len(), 1);
    assert!(
        child_entries
            .iter()
            .all(|e| e.role == MessageRole::Assistant)
    );
    assert!(child_entries[0].parts.iter().any(|p| matches!(p, MessagePart::Error { message, .. } if message.contains("2 public transcript updates omitted"))));
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "/goal resume".into(),
                message_id: Some("resume-user".into()),
            },
        )
        .unwrap();
    wait(|| {
        core.sessions.session_status(CHAT).is_some_and(|s| {
            s.status == SessionStatus::Idle
                && s.goal.is_some_and(|g| g.phase == GoalPhase::Complete)
        })
    })
    .await;
    let lines = std::fs::read_to_string(dir.path().join("extension-requests.jsonl")).unwrap();
    let prompts = lines
        .lines()
        .filter(|s| {
            serde_json::from_str::<serde_json::Value>(s).unwrap()["method"] == "session/prompt"
        })
        .count();
    assert_eq!(
        prompts, 2,
        "start and resume only; pause never became a queued model prompt"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn trusted_link_after_done_and_reattach_updates_original_card_and_real_child_doc() {
    let dir = tempfile::tempdir().unwrap();
    let first = core(dir.path());
    create(&first, dir.path());
    first
        .sessions
        .dispatch(
            CHAT,
            HarnessId::Mimir,
            request(dir.path(), "late child"),
            None,
        )
        .await
        .unwrap();
    wait(|| {
        first
            .sessions
            .session_status(CHAT)
            .is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let root = first.doc_host.open(CHAT).unwrap();
    let before = root.doc().read_entries().unwrap();
    assert!(
        child_ref(&first).is_none(),
        "the strict peer delays semantic linkage until after Done"
    );
    wait(|| child_ref(&first).is_some()).await;
    let child = child_ref(&first).unwrap();
    assert_eq!(text(&first, &child), "PUBLIC_CHILD");
    assert_eq!(
        root.doc().read_entries().unwrap().len(),
        before.len(),
        "late semantics must not mint a parent turn"
    );
    let marker = first
        .sessions
        .session_status(CHAT)
        .unwrap()
        .last_completed_turn;
    first.shutdown().await;
    drop(root);
    drop(first);
    let restored = core(dir.path());
    assert_eq!(text(&restored, &child), "PUBLIC_CHILD");
    restored
        .sessions
        .dispatch(
            CHAT,
            HarnessId::Mimir,
            request(dir.path(), "reattach child"),
            None,
        )
        .await
        .unwrap();
    wait(|| {
        text(&restored, &child) == "FINAL_PUBLIC_CHILD"
            && restored
                .sessions
                .session_status(CHAT)
                .is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    assert_eq!(child_ref(&restored).as_deref(), Some(child.as_str()));
    assert_eq!(
        restored
            .doc_host
            .open(&child)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        1
    );
    let entries = restored
        .doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .flat_map(|e| &e.parts)
            .filter(|p| p.id() == "launch-1")
            .count(),
        1
    );
    assert!(marker.is_some());
    let lines = std::fs::read_to_string(dir.path().join("extension-requests.jsonl")).unwrap();
    assert!(lines.lines().any(
        |s| serde_json::from_str::<serde_json::Value>(s).unwrap()["method"] == "session/load"
    ));
    restored.shutdown().await;
}
