//! Standard Mimir ACP lifecycle behavior through the public harness boundary.
//! The strict subprocess peer rejects altered prompts and fabricated answers.
#![cfg(unix)]

use std::{path::PathBuf, time::Duration};

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use kratos_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use kratos_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, UserInputAnswer, UserInputQuestion,
};

const SESSION: &str = "mimir-lifecycle-session";
const WAIT: Duration = Duration::from_secs(10);
type Input = (
    Vec<UserInputQuestion>,
    oneshot::Sender<Vec<UserInputAnswer>>,
);

fn harness() -> AcpHarness {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mimir-lifecycle.py");
    AcpHarness::mimir()
        .with_executable(fixture)
        .with_handshake_timeout(WAIT)
        .with_graces(Duration::from_secs(1), Duration::from_secs(1))
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: Some(HarnessId::Mimir),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: env!("CARGO_MANIFEST_DIR").into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: Vec::new(),
        worktree: None,
    }
}

struct Running {
    events: tokio::task::JoinHandle<Vec<AgentEvent>>,
    questions: mpsc::UnboundedReceiver<Input>,
    interrupt: CancellationToken,
}

impl Running {
    async fn start(request: RunRequest) -> Self {
        let (input_tx, questions) = mpsc::unbounded_channel();
        let (steer_tx, steering) = mpsc::channel(8);
        drop(steer_tx);
        let interrupt = CancellationToken::new();
        let controls = RunControls {
            request_input: Box::new(move |questions| {
                let (answer, receiver) = oneshot::channel();
                input_tx
                    .send((questions, answer))
                    .expect("question observer alive");
                receiver
            }),
            steering,
            interrupt: interrupt.clone(),
        };
        let stream = harness()
            .run(request, controls)
            .await
            .expect("harness starts");
        let events = tokio::spawn(async move {
            tokio::time::timeout(
                WAIT,
                stream.map(|event| event.expect("public event")).collect(),
            )
            .await
            .expect("public ACP stream settles")
        });
        Self {
            events,
            questions,
            interrupt,
        }
    }

    async fn question(&mut self) -> Input {
        tokio::time::timeout(WAIT, self.questions.recv())
            .await
            .expect("question arrives")
            .expect("question channel remains open")
    }

    async fn finish(self) -> Vec<AgentEvent> {
        self.events.await.expect("public stream collection task")
    }
}

fn terminal_statuses(events: &[AgentEvent]) -> Vec<DoneStatus> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Done { status, .. } => Some(*status),
            _ => None,
        })
        .collect()
}

fn assert_text(events: &[AgentEvent], expected: &str) {
    assert!(
        events.iter().any(|event| matches!(event,
            AgentEvent::TextDelta { text } if text == expected
        )),
        "expected {expected:?}: {events:?}"
    );
}

fn respond(question: &UserInputQuestion, labels: &[&str]) -> UserInputAnswer {
    UserInputAnswer {
        question_id: question.id.clone(),
        labels: labels.iter().map(|label| (*label).to_string()).collect(),
        note: None,
    }
}

fn respond_with_note(question: &UserInputQuestion, labels: &[&str], note: &str) -> UserInputAnswer {
    UserInputAnswer {
        note: Some(note.to_string()),
        ..respond(question, labels)
    }
}

#[tokio::test]
async fn forms_round_trip_real_mimir_schema_through_public_input_bridge() {
    let mut run = Running::start(request("forms")).await;
    let (questions, sender) = run.question().await;
    // The merged Mimir shape: one page per question — the two "Optional
    // note" companions folded into their choice questions.
    assert_eq!(questions.len(), 3);
    assert!(questions[0].question.contains("Do not enter credentials"));
    assert!(questions[0].options.is_empty());
    assert!(!questions[0].note);
    assert_eq!(
        questions[1].options,
        [
            "1. Same — First",
            "2. Same — Second",
            "3. None of the above"
        ]
    );
    assert!(questions[1].note);
    assert!(questions[2].multi_select);
    assert!(questions[2].note);
    sender
        .send(vec![
            respond(&questions[0], &["  Casey  "]),
            respond_with_note(&questions[1], &["2. Same — Second"], "Keep this note"),
            respond(&questions[2], &["1. Unit — Fast", "2. Process — Wire"]),
        ])
        .unwrap();
    let events = run.finish().await;
    assert_text(&events, "FORM_ACCEPTED_EXACT");
    assert_eq!(terminal_statuses(&events), [DoneStatus::Completed]);
}

#[tokio::test]
async fn exact_cancel_closes_old_waiter_and_late_cancel_cannot_answer_successor() {
    let mut run = Running::start(request("cancel-successor")).await;
    let (old_questions, mut old_sender) = run.question().await;
    tokio::time::timeout(WAIT, old_sender.closed())
        .await
        .expect("cancel drops exact receiver");
    let (questions, sender) = run.question().await;
    assert_ne!(old_questions[0].id, questions[0].id);
    assert!(!sender.is_closed());
    assert!(
        old_sender
            .send(vec![respond(&old_questions[0], &["Stale answer"])])
            .is_err()
    );
    sender
        .send(vec![respond(&questions[0], &["Fresh answer"])])
        .unwrap();
    let events = run.finish().await;
    assert_text(&events, "SUCCESSOR_ACCEPTED_WITHOUT_STALE_ANSWER");
    assert_eq!(terminal_statuses(&events), [DoneStatus::Completed]);
}

#[tokio::test]
async fn foreign_and_unsupported_forms_never_request_input_or_autoanswer() {
    for prompt in ["foreign", "unsupported"] {
        let mut run = Running::start(request(prompt)).await;
        let events = (&mut run.events).await.expect("public stream");
        assert!(
            run.questions.try_recv().is_err(),
            "unsupported form must not reach UI"
        );
        assert_text(&events, "REJECTED_WITHOUT_ASKING");
        assert!(
            !events.iter().any(|event| matches!(event,
                AgentEvent::TextDelta { text } if text.contains("WRONG_SESSION_TEXT")
            )),
            "{events:?}"
        );
        assert_eq!(terminal_statuses(&events), [DoneStatus::Completed]);
    }
}

#[tokio::test]
async fn explicit_decline_and_dropped_answer_channel_are_distinct_wire_actions() {
    for prompt in ["decline", "dismiss"] {
        let mut run = Running::start(request(prompt)).await;
        let (_, sender) = run.question().await;
        if prompt == "decline" {
            sender.send(Vec::new()).unwrap();
        } else {
            drop(sender);
        }
        let events = run.finish().await;
        assert_text(
            &events,
            if prompt == "decline" {
                "DECLINED_WITHOUT_ANSWER"
            } else {
                "DISMISSED_WITHOUT_ANSWER"
            },
        );
        assert_eq!(terminal_statuses(&events), [DoneStatus::Completed]);
    }
}

#[tokio::test]
async fn local_interrupt_cancels_held_form_and_never_reports_success() {
    let mut run = Running::start(request("interrupt")).await;
    let (_, mut sender) = run.question().await;
    run.interrupt.cancel();
    tokio::time::timeout(WAIT, sender.closed())
        .await
        .expect("interrupt closes form waiter");
    let events = run.finish().await;
    assert_eq!(terminal_statuses(&events), [DoneStatus::Interrupted]);
}

#[tokio::test]
async fn authoritative_prompt_end_closes_unfinished_form() {
    let mut run = Running::start(request("prompt-ended")).await;
    let (_, mut sender) = run.question().await;
    tokio::time::timeout(WAIT, sender.closed())
        .await
        .expect("settlement closes form waiter");
    let events = run.finish().await;
    assert_eq!(terminal_statuses(&events), [DoneStatus::Completed]);
}

#[tokio::test]
async fn paused_and_blocked_goals_do_not_invent_notices_or_report_success() {
    for prompt in ["goal-paused", "goal-blocked"] {
        let run = Running::start(request(prompt)).await;
        let events = run.finish().await;
        let notices: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let notice = notices.join("\n");
        assert!(
            !notice.contains("Goal paused")
                && !notice.contains("Goal blocked")
                && !notice.contains("Continue with"),
            "goal lifecycle is typed metadata, not synthesized assistant text: {events:?}"
        );
        assert!(
            !notice.contains("PRIVATE_") && !notice.contains("987") && !notice.contains("1800"),
            "{notice}"
        );
        assert_eq!(
            terminal_statuses(&events),
            [DoneStatus::Interrupted],
            "{events:?}"
        );
    }
}

#[tokio::test]
async fn advertised_native_commands_reach_peer_without_prompt_wrapping() {
    for command in ["/plan", "/goal", "/goal resume"] {
        let events = Running::start(request(command)).await.finish().await;
        assert_text(&events, &format!("COMMAND_UNCHANGED:{command}"));
        assert!(events.iter().any(|event| matches!(event,
            AgentEvent::AvailableCommands { commands } if commands.iter().any(|c| c.name == "plan")
                && commands.iter().any(|c| c.name == "goal" && c.input_hint.as_deref() == Some("objective|resume|pause|clear"))
        )), "{events:?}");
        assert_eq!(terminal_statuses(&events), [DoneStatus::Completed]);
    }
}

#[tokio::test]
async fn negotiated_resume_avoids_replay_and_load_fallback_drops_saved_history() {
    for (prefix, expected) in [
        ("mimir-resume", "session/resume"),
        ("mimir-load-only", "session/load"),
    ] {
        let cwd = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let mut req = request("resume-check");
        req.resume = Some(SESSION.into());
        req.cwd = cwd.path().to_string_lossy().into_owned();
        let events = Running::start(req).await.finish().await;
        assert_text(&events, &format!("OPENED_WITH:{expected}"));
        assert!(!events.iter().any(|event| matches!(event,
            AgentEvent::TextDelta { text } if text.contains("REPLAYED_HISTORY_MUST_NOT_DUPLICATE")
        )), "{events:?}");
        assert!(
            events.iter().any(|event| matches!(event,
                AgentEvent::SessionStarted { session_id, .. } if session_id == SESSION
            )),
            "{events:?}"
        );
        assert_eq!(terminal_statuses(&events), [DoneStatus::Completed]);
    }
}

#[tokio::test]
async fn paused_goal_can_resume_in_same_public_stream_without_session_cancel() {
    let (steer_tx, steering) = mpsc::channel(2);
    let controls = RunControls {
        request_input: Box::new(|_| panic!("goal metadata must not manufacture an input form")),
        steering,
        interrupt: CancellationToken::new(),
    };
    let mut stream = harness()
        .run(request("goal-paused"), controls)
        .await
        .unwrap();
    let mut events = Vec::new();
    tokio::time::timeout(WAIT, async {
        while let Some(event) = stream.next().await {
            let event = event.unwrap();
            let paused = matches!(
                event,
                AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    ..
                }
            );
            events.push(event);
            if paused {
                break;
            }
        }
    })
    .await
    .expect("paused goal ends its turn");
    assert_eq!(terminal_statuses(&events), [DoneStatus::Interrupted]);
    steer_tx
        .send(kratos_harness::SteerMessage {
            prompt: "/goal resume".into(),
            message_id: None,
        })
        .await
        .unwrap();
    drop(steer_tx);
    let remaining: Vec<AgentEvent> =
        tokio::time::timeout(WAIT, stream.map(|event| event.unwrap()).collect())
            .await
            .expect("resume settles");
    events.extend(remaining);
    assert_text(&events, "COMMAND_UNCHANGED:/goal resume");
    assert_eq!(
        terminal_statuses(&events),
        [DoneStatus::Interrupted, DoneStatus::Completed]
    );
}
