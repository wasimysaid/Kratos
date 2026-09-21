//! Mimir-specific identity of provider-qualified models. Everything is configured
//! through standard ACP selects; no native settings/journals are read by Kratos.

use super::{boolean_config_value, config, models_from_session};
use crate::{
    HarnessError,
    jsonrpc::{Incoming, RpcClient},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use kratos_proto::Model;

const PROVIDER: &str = "mimir.provider";
const DISCONNECTED: &str = "mimir.disconnected";

fn provider(state: &Value) -> Option<&Value> {
    config::options(state).iter().find(|o| o["id"] == PROVIDER)
}

pub(super) async fn select_provider(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    session: &str,
    state: &mut Value,
    requested: Option<&str>,
) -> Result<Option<String>, HarnessError> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    let (provider_id, model) = requested
        .split_once('/')
        .map_or((None, requested), |(p, m)| (Some(p), m));
    if let Some(id) = provider_id {
        let option = provider(state).ok_or_else(|| {
            HarnessError::Protocol("Mimir did not advertise a provider setting".into())
        })?;
        if !option["options"]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v["value"] == id))
        {
            return Err(HarnessError::Protocol(format!(
                "Mimir does not advertise provider {id}"
            )));
        }
        if option["currentValue"] != id {
            config::set(
                client,
                incoming,
                session,
                state,
                PROVIDER,
                json!({"value": id}),
            )
            .await?;
        }
    }
    // Never family-match a requested model to an unrelated GPT/Claude model.
    if !config::options(state)
        .iter()
        .filter(|o| o["category"] == "model")
        .any(|o| {
            o["options"].as_array().is_some_and(|v| {
                v.iter()
                    .any(|v| v["value"] == model && v["value"] != DISCONNECTED)
            })
        })
    {
        return Err(HarnessError::Protocol(format!(
            "Mimir does not advertise requested model {requested}"
        )));
    }
    Ok(Some(model.to_owned()))
}

/// Explicit user settings are requirements, not suggestions. Verify the final
/// dependent state: an agent may reject a choice or reset it while changing models.
pub(super) fn validate_settings(
    state: &Value,
    model: Option<&str>,
    reasoning: Option<kratos_proto::ReasoningLevel>,
    traits: &serde_json::Map<String, Value>,
) -> Result<(), HarnessError> {
    let verify = |option: Option<&Value>, expected: Value, label: &str| {
        let expected = if option.is_some_and(|o| o["type"] == "boolean") {
            boolean_config_value(&expected).unwrap_or(expected)
        } else {
            expected
        };
        let actual = option
            .and_then(|o| o.get("currentValue"))
            .unwrap_or(&Value::Null);
        if actual == &expected {
            return Ok(());
        }
        Err(HarnessError::Protocol(format!(
            "Mimir did not apply requested {label} {expected}; effective value is {actual}. Choose an advertised setting."
        )))
    };
    if let Some(model) = model {
        verify(
            config::options(state)
                .iter()
                .find(|o| o["category"] == "model"),
            model.into(),
            "model",
        )?;
    }
    if let Some(reasoning) = reasoning {
        verify(
            config::options(state)
                .iter()
                .find(|o| o["category"] == "thought_level"),
            json!(reasoning),
            "reasoning",
        )?;
    }
    for (id, expected) in traits {
        verify(
            config::options(state).iter().find(|o| o["id"] == *id),
            expected.clone(),
            id,
        )?;
    }
    Ok(())
}

pub(super) async fn discover_models(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    state: &mut Value,
) -> Result<Vec<Model>, HarnessError> {
    let session = state["sessionId"].as_str().unwrap_or_default().to_owned();
    let current = provider(state)
        .and_then(|o| o["currentValue"].as_str())
        .map(str::to_owned);
    let mut providers: Vec<String> = provider(state)
        .and_then(|o| o["options"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|o| o["value"].as_str())
        .filter(|id| *id != DISCONNECTED)
        .map(str::to_owned)
        .collect();
    providers.sort_by_key(|id| Some(id) != current.as_ref());
    let mut models = Vec::new();
    for id in providers {
        // Mimir advertises its whole provider catalog, including providers
        // without credentials. Let the agent validate access; never read auth.json.
        if config::set(
            client,
            incoming,
            &session,
            state,
            PROVIDER,
            json!({"value":id}),
        )
        .await
        .is_err()
        {
            continue;
        }
        let selected = config::options(state)
            .iter()
            .find(|o| o["category"] == "model")
            .and_then(|o| o["currentValue"].as_str());
        let mut available = models_from_session(state, &[]);
        available.sort_by_key(|model| Some(model.id.as_str()) != selected);
        for mut model in available {
            if model.id == DISCONNECTED {
                continue;
            }
            // ACP exposes effort for the currently selected model only.
            // Thousands of per-model config mutations make catalog discovery
            // unusable. Resolve other ladders when the user selects that row.
            if Some(model.id.as_str()) != selected {
                model.reasoning_levels.clear();
            }
            models.push(qualified_model(model, &id));
        }
    }
    Ok(models)
}

fn qualified_model(mut model: Model, provider: &str) -> Model {
    model.id = format!("{provider}/{}", model.id);
    model.description = Some(match model.description {
        Some(description) => format!("{provider} · {description}"),
        None => provider.to_owned(),
    });
    model.options.retain(|o| o.id != PROVIDER);
    model
}

pub(super) async fn selected_model(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    state: &mut Value,
    requested: &str,
) -> Result<Model, HarnessError> {
    let session = state["sessionId"].as_str().unwrap_or_default().to_owned();
    let id = select_provider(client, incoming, &session, state, Some(requested))
        .await?
        .expect("explicit requested model");
    config::apply(
        client,
        incoming,
        &session,
        state,
        Some(&id),
        &[],
        &Default::default(),
    )
    .await?;
    let provider = provider(state)
        .and_then(|o| o["currentValue"].as_str())
        .unwrap_or_default();
    models_from_session(state, &[])
        .into_iter()
        .find(|model| model.id == id)
        .map(|model| qualified_model(model, provider))
        .ok_or_else(|| {
            HarnessError::Protocol(format!("Mimir did not return selected model {requested}"))
        })
}
