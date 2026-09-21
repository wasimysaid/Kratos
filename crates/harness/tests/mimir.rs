//! Mimir's native ACP contract through the public harness, without provider access.
#![cfg(unix)]

use futures::StreamExt;
use std::{path::PathBuf, time::Duration};
use tokio::sync::{mpsc, oneshot};
use kratos_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use kratos_proto::{AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel};

fn harness() -> AcpHarness {
    AcpHarness::mimir().with_executable(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mimir-acp.py"),
    )
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: Some(HarnessId::Mimir),
        model: Some("chatgpt/gpt-5.6-luna".into()),
        reasoning: Some(ReasoningLevel::Medium),
        model_options: Default::default(),
        cwd: env!("CARGO_MANIFEST_DIR").into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: Vec::new(),
        worktree: None,
    }
}

async fn run(prompt: &str) -> Vec<AgentEvent> {
    run_request(request(prompt)).await
}

async fn run_request(request: RunRequest) -> Vec<AgentEvent> {
    let (tx, rx) = mpsc::channel(8);
    drop(tx);
    let controls = RunControls {
        request_input: Box::new(|_| {
            let (_tx, rx) = oneshot::channel();
            rx
        }),
        steering: rx,
        interrupt: CancellationToken::new(),
    };
    let stream = harness().run(request, controls).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.map(Result::unwrap).collect(),
    )
    .await
    .expect("ACP turn finishes")
}

#[tokio::test]
async fn explicit_unavailable_or_rejected_settings_never_prompt() {
    let mut cases = Vec::new();
    let mut small = request("config");
    small.model = Some("chatgpt/small".into());
    cases.push(small);
    let mut unsupported = request("config");
    unsupported.reasoning = Some(ReasoningLevel::XHigh);
    cases.push(unsupported);
    let mut rejected = request("config");
    rejected.reasoning = Some(ReasoningLevel::High);
    cases.push(rejected);
    let mut mode = request("config");
    mode.model_options
        .insert("mimir.mode".into(), "mode-plan".into());
    cases.push(mode);
    let mut unknown = request("config");
    unknown
        .model_options
        .insert("mimir.mode".into(), "not-a-mode".into());
    cases.push(unknown);
    for request in cases {
        let description = format!(
            "{:?} {:?} {:?}",
            request.model, request.reasoning, request.model_options
        );
        let events = run_request(request).await;
        assert!(events.iter().any(|event| matches!(event,
            AgentEvent::Done { status: DoneStatus::Errored, error: Some(error), .. } if error.contains("requested")
        )), "{description}: {events:?}");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::TextDelta { .. })),
            "must fail before the fixture is prompted: {description}: {events:?}"
        );
    }
}

#[test]
fn native_identity_uses_turn_boundaries() {
    let harness = AcpHarness::mimir();
    assert_eq!(harness.id(), HarnessId::Mimir);
    assert_eq!(harness.display_name(), "Mimir");
    assert_eq!(
        harness.steering_mode(),
        kratos_proto::SteeringMode::TurnBoundary
    );
    assert!(harness.authoritative_prompt_end());
}

#[tokio::test]
async fn catalog_skips_unauthenticated_providers_and_uses_each_models_actual_ladder() {
    let harness = harness();
    let catalog = harness.models().await.unwrap();
    assert!(
        catalog
            .iter()
            .find(|m| m.id == "chatgpt/gpt-5.6-luna")
            .unwrap()
            .reasoning_levels
            .is_empty(),
        "unselected models must not borrow the default model's effort ladder"
    );
    let models = harness
        .models_for_selection(Some("chatgpt/gpt-5.6-luna"))
        .await
        .unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["other/other-model", "chatgpt/small", "chatgpt/gpt-5.6-luna"]
    );
    let luna = &models[2];
    assert!(luna.reasoning_levels.contains(&ReasoningLevel::Medium));
    assert!(!models[1].reasoning_levels.contains(&ReasoningLevel::Medium));
    assert_eq!(
        luna.options
            .iter()
            .map(|o| o.id.as_str())
            .collect::<Vec<_>>(),
        ["mimir.mode", "mimir.fast", "mimir.rejected"]
    );
}

#[tokio::test]
async fn selected_provider_model_and_effort_are_effective_before_prompt() {
    let events = run("config").await;
    assert!(
        events.iter().any(|event| matches!(event,
            AgentEvent::TextDelta { text } if text == "chatgpt/gpt-5.6-luna medium"
        )),
        "{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn advertised_boolean_setting_is_effective_before_prompt() {
    let mut request = request("boolean");
    request
        .model_options
        .insert("mimir.fast".into(), "on".into());

    let events = run_request(request).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta { text } if text == "fast=on"
        )),
        "{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn rejected_boolean_setting_fails_before_prompt() {
    let mut request = request("boolean");
    request
        .model_options
        .insert("mimir.rejected".into(), "on".into());

    let events = run_request(request).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::Done {
                status: DoneStatus::Errored,
                error: Some(error),
                ..
            } if error.contains("requested mimir.rejected true")
        )),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TextDelta { .. })),
        "must fail before the fixture is prompted: {events:?}"
    );
}
