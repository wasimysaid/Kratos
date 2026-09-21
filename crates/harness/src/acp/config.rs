//! Apply dependent ACP settings against the latest advertised state, not a
//! precomputed batch: changing a provider/model can replace the next choices.

use super::{config_option_sets, first_class_model_change, request_draining};
use crate::{
    HarnessError,
    jsonrpc::{Incoming, RpcClient},
};
use serde_json::{Value, json};
use std::collections::HashSet;
use tokio::sync::mpsc;

pub(super) fn options(state: &Value) -> &[Value] {
    state
        .get("configOptions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Session metadata is state, not replayed transcript. Keep it across setup
/// replies even when an agent sends the notification before the RPC response.
pub(super) fn merge_state(state: &mut Value, update: &Value) {
    for key in ["configOptions", "availableCommands", "title"] {
        if let Some(value) = update.get(key) {
            state[key] = value.clone();
        }
    }
}

pub(super) fn metadata_events(state: &Value) -> Vec<kratos_proto::AgentEvent> {
    use kratos_proto::AgentEvent;
    let mut events = Vec::new();
    if let Some(title) = state
        .get("title")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
    {
        events.push(AgentEvent::SessionTitle {
            title: title.to_owned(),
        });
    }
    for mode in options(state).iter().filter(|o| o["category"] == "mode") {
        if let (Some(id), Some(value)) = (mode["id"].as_str(), mode["currentValue"].as_str()) {
            events.push(AgentEvent::SessionMode {
                id: id.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    if let Some(value) = state.get("currentModeId").and_then(Value::as_str) {
        events.push(AgentEvent::SessionMode {
            id: "mode".into(),
            value: value.to_owned(),
        });
    }
    events
}

pub(super) async fn set(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    session: &str,
    state: &mut Value,
    id: &str,
    payload: Value,
) -> Result<(), HarnessError> {
    let mut params = payload.as_object().cloned().unwrap_or_default();
    params.insert("sessionId".into(), session.into());
    params.insert("configId".into(), id.into());
    let response = request_draining(
        client,
        incoming,
        "session/set_config_option",
        Value::Object(params),
    )
    .await?;
    merge_state(state, &response);
    Ok(())
}

pub(super) async fn apply(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    session: &str,
    state: &mut Value,
    model: Option<&str>,
    efforts: &[&'static str],
    traits: &serde_json::Map<String, Value>,
) -> Result<(), HarnessError> {
    let mut applied = HashSet::new();
    loop {
        let mut changes = config_option_sets(state, model, efforts, traits);
        // A model_config can select a provider and replace the entire model
        // catalog. Effort depends on the selected model, never the reverse.
        changes.sort_by_key(|(id, _)| {
            match options(state)
                .iter()
                .find(|o| o["id"] == *id)
                .and_then(|o| o["category"].as_str())
            {
                Some("model_config") => 0,
                Some("model") => 1,
                Some("thought_level") => 3,
                _ => 2,
            }
        });
        let Some((id, payload)) = changes.into_iter().find(|(id, _)| !applied.contains(id)) else {
            break;
        };
        let is_model = options(state)
            .iter()
            .any(|o| o["id"] == id && o["category"] == "model");
        applied.insert(id.clone());
        if let Err(error) = set(client, incoming, session, state, &id, payload).await {
            if is_model {
                return Err(HarnessError::Protocol(format!(
                    "agent rejected requested model {model:?}: {error}"
                )));
            }
            tracing::debug!(target: "kratos_harness::acp", %id, %error, "agent rejected auxiliary setting");
        }
    }
    if let Some(model) = first_class_model_change(state, model)? {
        request_draining(
            client,
            incoming,
            "session/set_model",
            json!({"sessionId":session,"modelId":model}),
        )
        .await
        .map_err(|error| {
            HarnessError::Protocol(format!("agent rejected model switch to {model}: {error}"))
        })?;
    }
    Ok(())
}
