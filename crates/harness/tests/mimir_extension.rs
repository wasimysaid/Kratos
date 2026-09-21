//! Negotiated native controls and public child paging through a strict stdio peer.
#![cfg(unix)]
use futures::StreamExt;
use std::{path::PathBuf, time::Duration};
use tokio::sync::{mpsc, oneshot};
use kratos_harness::{AcpHarness, CancellationToken, Harness, RunControls, SteerMessage};
use kratos_proto::{AgentEvent, DoneStatus, GoalPhase, HarnessId, RunRequest, SandboxLevel};

#[tokio::test]
async fn goal_control_owns_no_model_turn_and_child_revisions_replace_and_stop() {
    let dir = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mimir-extension.py");
    let harness = AcpHarness::mimir()
        .with_executable(fixture)
        .with_handshake_timeout(Duration::from_secs(10));
    let (tx, steering) = mpsc::channel(8);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        steering,
        interrupt: interrupt.clone(),
        request_input: Box::new(|_| {
            let (_, rx) = oneshot::channel();
            rx
        }),
    };
    let request = RunRequest {
        prompt: "start native goal".into(),
        harness: Some(HarnessId::Mimir),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: dir.path().display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: vec![],
        worktree: None,
    };
    let mut stream = harness.run(request, controls).await.unwrap();
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = stream.next().await.unwrap().unwrap();
            let ready = matches!(&event, AgentEvent::SubagentView { snapshot: Some(snapshot), .. } if snapshot.revision == "r2");
            events.push(event);
            if ready { break; }
        }
    }).await.expect("first complete child revision arrives");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::GoalState { goal: Some(g) } if g.phase == GoalPhase::Active && g.objective == "Inspect public files")));
    let snapshot = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubagentView {
                snapshot: Some(s), ..
            } => Some(s),
            _ => None,
        })
        .unwrap();
    assert_eq!(snapshot.omitted_updates, 2);
    let public = serde_json::to_string(snapshot).unwrap();
    assert!(public.contains("PUBLIC_CHILD") && !public.contains("STALE_REVISION"));
    tx.send(SteerMessage {
        prompt: "/goal pause".into(),
        message_id: Some("pause-command".into()),
    })
    .await
    .unwrap();
    tx.send(SteerMessage {
        prompt: "/goal pause".into(),
        message_id: Some("duplicate-command".into()),
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            events.push(stream.next().await.unwrap().unwrap());
            if events.iter().any(|e| matches!(e, AgentEvent::Done { status: DoneStatus::Interrupted, .. })) && events.iter().any(|e| matches!(e, AgentEvent::SubagentView { child, snapshot: Some(s) } if s.revision == "final" && child.status == "completed")) { break; }
        }
    }).await.expect("pause settles its own control and final child fetch");
    let ack = events.iter().position(|e| matches!(e, AgentEvent::ControlResolved { prompt, message_id: Some(id), error: None } if prompt == "/goal pause" && id == "pause-command")).unwrap();
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ControlResolved { message_id: Some(id), error: Some(_), .. } if id == "duplicate-command")), "bounded control rejection must identify the exact duplicate command");
    let done = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .unwrap();
    assert!(
        ack < done,
        "even a raced prompt response must not orphan/replay the control"
    );
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::GoalState { goal: Some(g) } if g.phase == GoalPhase::Paused)
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered { .. })),
        "a control must not rotate the parent transcript"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("Continue with"))),
        "no synthetic goal notice"
    );
    let requests = || {
        std::fs::read_to_string(dir.path().join("extension-requests.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    let reads = requests()
        .iter()
        .filter(|r| r["method"] == "_mimir/session/child" && r["paused"] == true)
        .count();
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(
        requests()
            .iter()
            .filter(|r| r["method"] == "_mimir/session/child" && r["paused"] == true)
            .count(),
        reads,
        "terminal child fetches stop"
    );
    tx.send(SteerMessage {
        prompt: "/goal show".into(),
        message_id: None,
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            stream.next().await.unwrap().unwrap(),
            AgentEvent::ControlResolved { error: None, .. }
        ) {}
    })
    .await
    .unwrap();
    tx.send(SteerMessage {
        prompt: "/goal resume".into(),
        message_id: None,
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            stream.next().await.unwrap().unwrap(),
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        ) {}
    })
    .await
    .unwrap();
    assert_eq!(
        requests()
            .iter()
            .filter(|r| r["method"] == "session/prompt")
            .count(),
        2,
        "only start and resume acquire model prompts"
    );
    interrupt.cancel();
    drop(stream);
}

#[tokio::test]
async fn unavailable_child_transcripts_recover_stop_and_reset_after_availability_toggle() {
    let dir = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mimir-child-unavailable.py");
    let harness = AcpHarness::mimir()
        .with_executable(fixture)
        .with_handshake_timeout(Duration::from_secs(10));
    let (tx, steering) = mpsc::channel(8);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        steering,
        interrupt: interrupt.clone(),
        request_input: Box::new(|_| {
            let (_, rx) = oneshot::channel();
            rx
        }),
    };
    let request = RunRequest {
        prompt: "inspect unavailable child transcripts".into(),
        harness: Some(HarnessId::Mimir),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: dir.path().display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: vec![],
        worktree: None,
    };
    let mut stream = harness.run(request, controls).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                stream.next().await.unwrap().unwrap(),
                AgentEvent::SubagentView {
                    child,
                    snapshot: Some(snapshot),
                } if child.child_id == "recover-child" && snapshot.revision == "available"
            ) {
                break;
            }
        }
    })
    .await
    .expect("a running child recovers after temporary transcript unavailability");

    let requests = || {
        std::fs::read_to_string(dir.path().join("child-unavailable-requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    let unavailable_reads = || {
        requests()
            .iter()
            .filter(|request| {
                request["method"] == "_mimir/session/child"
                    && request["childId"] == "unavailable-child"
            })
            .count()
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while unavailable_reads() < 3 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("terminal unavailable child reaches its retry bound");
    let stopped = unavailable_reads();
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert_eq!(
        unavailable_reads(),
        stopped,
        "an unavailable terminal transcript must not be polled forever"
    );

    tx.send(SteerMessage {
        prompt: "/goal show".into(),
        message_id: Some("toggle-transcript".into()),
    })
    .await
    .unwrap();
    let mut hidden = false;
    let mut restored = false;
    let mut acknowledged = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !hidden || !restored || !acknowledged {
            match stream.next().await.unwrap().unwrap() {
                AgentEvent::SubagentView { child, .. }
                    if child.child_id == "unavailable-child" && !child.transcript =>
                {
                    hidden = true;
                }
                AgentEvent::SubagentView { child, .. }
                    if child.child_id == "unavailable-child" && child.transcript =>
                {
                    restored = true;
                }
                AgentEvent::ControlResolved {
                    message_id: Some(id),
                    error: None,
                    ..
                } if id == "toggle-transcript" => acknowledged = true,
                _ => {}
            }
        }
    })
    .await
    .expect("back-to-back transcript availability updates are observed");
    tokio::time::timeout(Duration::from_secs(3), async {
        while unavailable_reads() == stopped {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("coalesced false-to-true availability restores fetch attempts");
    interrupt.cancel();
    drop(stream);
}
