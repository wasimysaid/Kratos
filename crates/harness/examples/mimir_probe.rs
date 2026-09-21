//! Live native-ACP smoke. Uses the user's login-managed Mimir configuration.
//! Set HOME to a project-local scratch directory to isolate Mimir transcripts,
//! MIMIR_CODING_AGENT_DIR to the existing configuration directory, and
//! MIMIR_EXECUTABLE to the installed binary. Never copy credentials into fixtures.
//!
//! cargo run -p kratos-harness --example mimir_probe -- <cwd> [prompt]
//! Without a prompt this only discovers the real catalog and slash commands.

use futures::StreamExt;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use kratos_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use kratos_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, UserInputAnswer,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let cwd = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("expected disposable absolute cwd"))?;
    let prompt = args.next();
    let harness = AcpHarness::mimir();
    if prompt.is_none() {
        let models = harness
            .models_for_selection(Some("chatgpt/gpt-5.6-luna"))
            .await?;
        anyhow::ensure!(
            models.iter().any(|m| m.id == "chatgpt/gpt-5.6-luna"),
            "Luna absent from real catalog"
        );
        println!(
            "{}",
            serde_json::json!({"models":models,"commands":harness.commands().await?})
        );
        return Ok(());
    }
    let (steer, steering) = mpsc::channel(8);
    drop(steer);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|questions| {
            eprintln!("{}", serde_json::json!({"questions":questions}));
            let (tx, rx) = oneshot::channel();
            let answers = std::env::var("MIMIR_PROBE_ANSWER")
                .ok()
                .map(|answer| {
                    questions
                        .iter()
                        .map(|q| UserInputAnswer {
                            question_id: q.id.clone(),
                            labels: vec![answer.clone()],
                            note: None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let _ = tx.send(answers);
            rx
        }),
        steering,
        interrupt: interrupt.clone(),
    };
    let request = RunRequest {
        prompt: prompt.unwrap(),
        harness: Some(HarnessId::Mimir),
        model: Some("chatgpt/gpt-5.6-luna".into()),
        reasoning: Some(ReasoningLevel::Medium),
        model_options: Default::default(),
        cwd,
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: std::env::var("MIMIR_PROBE_RESUME").ok(),
        attachments: std::env::var("MIMIR_PROBE_IMAGE")
            .ok()
            .into_iter()
            .collect(),
        worktree: None,
    };
    let mut stream = harness.run(request, controls).await?;
    let result = tokio::time::timeout(Duration::from_secs(240), async {
        let mut done = None;
        while let Some(event) = stream.next().await {
            let event = event?;
            println!("{}", serde_json::to_string(&event)?);
            if let AgentEvent::Done { status, .. } = event {
                done = Some(status);
            }
        }
        anyhow::ensure!(done.is_some(), "stream closed without a terminal outcome");
        anyhow::ensure!(done != Some(DoneStatus::Errored), "Mimir run failed");
        Ok::<_, anyhow::Error>(())
    })
    .await;
    if result.is_err() {
        interrupt.cancel();
    }
    result??;
    Ok(())
}
