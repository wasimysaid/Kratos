//! Shared ACP events exercised through public engine/doc APIs, without a provider.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use futures::{StreamExt, stream::BoxStream};
use tokio::sync::mpsc;
use kratos_doc::{MessagePart, MessageRole, MessageStatus};
use kratos_engine::{EngineCore, HarnessRegistry};
use kratos_harness::{Harness, HarnessError, RunControls};
use kratos_proto::{
    AgentEvent, ChatConfig, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode, ToolCall, UserInputQuestion,
};

const CHAT: &str = "shared-acp";

struct Started {
    request: RunRequest,
    controls: RunControls,
    events: mpsc::Sender<Result<AgentEvent, HarnessError>>,
}

struct ControlledHarness {
    started: mpsc::Sender<Started>,
    title_runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Harness for ControlledHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Controlled ACP"
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
    fn native_titles(&self) -> bool {
        true
    }
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run_title(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.title_runs.fetch_add(1, Ordering::SeqCst);
        Ok(futures::stream::iter(vec![
            Ok(AgentEvent::TextDelta {
                text: "Generated fallback".into(),
            }),
            Ok(done(DoneStatus::Completed)),
        ])
        .boxed())
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (events, rx) = mpsc::channel(32);
        self.started
            .send(Started {
                request,
                controls,
                events,
            })
            .await
            .unwrap();
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

fn setup(path: &std::path::Path) -> (EngineCore, mpsc::Receiver<Started>, Arc<AtomicUsize>) {
    let (started, rx) = mpsc::channel(4);
    let title_runs = Arc::new(AtomicUsize::new(0));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ControlledHarness {
        started,
        title_runs: title_runs.clone(),
    }));
    let core = EngineCore::assemble(path, Arc::new(registry), HarnessId::Mock, None).unwrap();
    core.workspace
        .create_chat(
            CHAT,
            None,
            Some(&core.device_id),
            Some(ChatConfig {
                harness: HarnessId::Mock,
                model: None,
                reasoning: None,
                model_options: serde_json::from_value(
                    serde_json::json!({"mode": "build", "effort": "medium"}),
                )
                .unwrap(),
                sandbox: SandboxLevel::WorkspaceWrite,
            }),
            Some(path.display().to_string()),
        )
        .unwrap();
    (core, rx, title_runs)
}

fn request(core: &EngineCore, prompt: &str) -> RunRequest {
    let chat = core.workspace.chat(CHAT).unwrap().unwrap();
    let config = chat.config.unwrap();
    RunRequest {
        prompt: prompt.into(),
        harness: Some(config.harness),
        model: config.model,
        reasoning: config.reasoning,
        model_options: config.model_options,
        cwd: chat.cwd.unwrap(),
        sandbox: config.sandbox,
        auto_approve: true,
        resume: None,
        attachments: vec![],
        worktree: None,
    }
}

fn done(status: DoneStatus) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some("native-session".into()),
    }
}

fn question(id: &str) -> Vec<UserInputQuestion> {
    vec![UserInputQuestion {
        id: id.into(),
        header: "Question".into(),
        question: id.into(),
        options: vec![],
        multi_select: false,
        note: false,
    }]
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("engine state must advance within five seconds");
}

fn input_id(core: &EngineCore, question_id: &str) -> Option<String> {
    core.doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap()
        .iter()
        .find_map(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Input {
                    request_id,
                    questions,
                    resolved: false,
                    ..
                } if questions[0].id == question_id => Some(request_id.clone()),
                _ => None,
            })
        })
}

#[tokio::test]
async fn cancelled_receiver_resolves_exact_input_without_ending_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (core, mut started, _) = setup(dir.path());
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, request(&core, "ask"), None)
        .await
        .unwrap();
    let run = started.recv().await.unwrap();
    let first = (run.controls.request_input)(question("first"));
    let second = (run.controls.request_input)(question("second"));
    wait_for(|| input_id(&core, "first").is_some() && input_id(&core, "second").is_some()).await;
    let first_id = input_id(&core, "first").unwrap();
    let second_id = input_id(&core, "second").unwrap();
    let (_, mut live) = core.sessions.subscribe(CHAT, 0).unwrap();

    drop(first); // Standard ACP request cancellation drops this receiver.
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(live.recv().await.unwrap().event, AgentEvent::InputResolved { request_id } if request_id == first_id) { break; }
        }
    }).await.expect("cancel must promptly emit InputResolved without polling");
    wait_for(|| input_id(&core, "first").is_none()).await;
    assert_eq!(input_id(&core, "second"), Some(second_id.clone()));
    assert_eq!(
        core.sessions.session_status(CHAT).unwrap().status,
        SessionStatus::AwaitingInput
    );
    assert!(
        !core
            .sessions
            .respond_input(CHAT, &first_id, vec![])
            .unwrap()
    );
    assert!(
        core.sessions
            .respond_input(CHAT, &second_id, vec![])
            .unwrap()
    );
    assert!(second.await.unwrap().is_empty());
    wait_for(|| core.sessions.session_status(CHAT).unwrap().status == SessionStatus::Working).await;
    assert!(
        !core
            .sessions
            .subscribe(CHAT, 0)
            .unwrap()
            .0
            .iter()
            .any(|e| matches!(e.event, AgentEvent::Done { .. }))
    );
    run.events
        .send(Ok(done(DoneStatus::Interrupted)))
        .await
        .unwrap();
}

#[tokio::test]
async fn native_title_mode_progress_and_boundaries_survive_next_prompt_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let (core, mut started, titles) = setup(dir.path());
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, request(&core, "/plan"), None)
        .await
        .unwrap();
    let mut run = started.recv().await.unwrap();
    for event in [
        AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "test".into(),
            tools: vec![],
            cwd: run.request.cwd.clone(),
            session_id: "native-session".into(),
            assistant_message_id: "assistant".into(),
        },
        AgentEvent::SessionTitle {
            title: "Native title".into(),
        },
        AgentEvent::SessionMode {
            id: "mode".into(),
            value: "plan".into(),
        },
        AgentEvent::TextDelta {
            text: "First message".into(),
        },
        AgentEvent::MessageBoundary,
        AgentEvent::TextDelta {
            text: "Second message".into(),
        },
        AgentEvent::ToolCall {
            id: "tool".into(),
            call: ToolCall::Exec {
                command: "cargo test".into(),
            },
        },
        AgentEvent::ToolProgress {
            id: "tool".into(),
            output: Some("running test 1".into()),
        },
    ] {
        run.events.send(Ok(event)).await.unwrap();
    }
    let handle = core.doc_host.open(CHAT).unwrap();
    wait_for(|| {
        handle.doc().read_entries().unwrap().iter().any(|e| {
            e.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Tool { id, .. } if id == "tool"))
        })
    })
    .await;
    assert_eq!(
        core.workspace.chat(CHAT).unwrap().unwrap().title.as_deref(),
        Some("Native title")
    );
    assert_eq!(
        core.workspace.chat_config(CHAT).unwrap().model_options["mode"],
        "plan"
    );
    assert_eq!(
        core.workspace.chat_config(CHAT).unwrap().model_options["effort"],
        "medium"
    );
    assert_eq!(
        core.sessions.session_status(CHAT).unwrap().status,
        SessionStatus::Working
    );
    assert_eq!(titles.load(Ordering::SeqCst), 0);
    let entries = handle.doc().read_entries().unwrap();
    let assistant = entries
        .iter()
        .find(|e| e.role == MessageRole::Assistant)
        .unwrap();
    assert_eq!(assistant.status, Some(MessageStatus::Streaming));
    assert!(assistant.parts.iter().any(
        |p| matches!(p, MessagePart::Text { text, .. } if text == "First message\n\nSecond message")
    ));
    assert!(assistant.parts.iter().any(|p| matches!(
        p,
        MessagePart::Tool {
            resolved: false,
            is_error: false,
            ..
        }
    )));
    assert!(assistant.parts.iter().any(
        |p| matches!(p, MessagePart::Tool { output: Some(text), .. } if text == "running test 1")
    ));
    let full_output = format!(
        "stdout: tests passed\nstderr: warning\n{}\nProcess completed, exit code 0",
        "public output\n".repeat(200)
    );
    run.events
        .send(Ok(AgentEvent::ToolResult {
            id: "tool".into(),
            is_error: false,
            output: Some(full_output.clone()),
            diff: None,
        }))
        .await
        .unwrap();
    wait_for(|| handle.doc().read_entries().unwrap().iter().any(|e| e.parts.iter().any(|p| matches!(p, MessagePart::Tool { output_ref: Some(key), resolved: true, .. } if key == "shared-acp/tool")))).await;
    assert_eq!(
        core.doc_host
            .fetch_tool_blob("shared-acp/tool")
            .await
            .unwrap(),
        full_output
    );
    let persisted = serde_json::to_string(&handle.doc().read_entries().unwrap()).unwrap();
    assert!(
        !persisted.contains(&"public output\\n".repeat(10)),
        "full output must stay out of synced docs"
    );

    core.workspace.rename_chat(CHAT, "User rename").unwrap();
    run.events
        .send(Ok(AgentEvent::SessionTitle {
            title: "Agent retitle".into(),
        }))
        .await
        .unwrap();
    run.events
        .send(Ok(done(DoneStatus::Completed)))
        .await
        .unwrap();
    wait_for(|| core.sessions.session_status(CHAT).unwrap().status == SessionStatus::Idle).await;
    assert_eq!(
        core.workspace.chat(CHAT).unwrap().unwrap().title.as_deref(),
        Some("User rename")
    );
    assert_eq!(titles.load(Ordering::SeqCst), 0);

    // Routing a Plan follow-up must reuse the live runtime, not restart with Build.
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::Mock,
            request(&core, "continue planning"),
            None,
        )
        .await
        .unwrap();
    let steer = tokio::time::timeout(Duration::from_secs(1), run.controls.steering.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(steer.prompt, "continue planning");
    assert!(
        started.try_recv().is_err(),
        "mode update must update RuntimeConfig too"
    );
    run.events
        .send(Ok(AgentEvent::Steered {
            assistant_message_id: None,
            next_assistant_message_id: Some("followup".into()),
        }))
        .await
        .unwrap();
    run.events
        .send(Ok(AgentEvent::TextDelta {
            text: "Goal paused. What should change? Send /goal resume to continue.".into(),
        }))
        .await
        .unwrap();
    run.events
        .send(Ok(done(DoneStatus::Interrupted)))
        .await
        .unwrap();
    wait_for(|| core.sessions.session_status(CHAT).unwrap().status == SessionStatus::Idle).await;
    assert_eq!(
        core.sessions.session_status(CHAT).unwrap().status,
        SessionStatus::Idle
    );
    let entries = handle.doc().read_entries().unwrap();
    let paused = entries.iter().find(|e| e.id == "followup").unwrap();
    assert_eq!(
        paused.status,
        Some(MessageStatus::Aborted),
        "paused goals must not read as successful completion"
    );

    core.sessions
        .dispatch(CHAT, HarnessId::Mock, request(&core, "/goal resume"), None)
        .await
        .unwrap();
    let resumed = started.recv().await.unwrap();
    assert_eq!(resumed.request.model_options["mode"], "plan");
    assert_eq!(resumed.request.resume.as_deref(), Some("native-session"));
    resumed
        .events
        .send(Ok(done(DoneStatus::Interrupted)))
        .await
        .unwrap();
}

#[tokio::test]
async fn native_titles_fall_back_only_after_completed_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (core, mut started, titles) = setup(dir.path());
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, request(&core, "untitled"), None)
        .await
        .unwrap();
    let run = started.recv().await.unwrap();
    assert_eq!(titles.load(Ordering::SeqCst), 0);
    assert!(core.workspace.chat(CHAT).unwrap().unwrap().title.is_none());
    run.events
        .send(Ok(AgentEvent::TextDelta {
            text: "Finished".into(),
        }))
        .await
        .unwrap();
    run.events
        .send(Ok(done(DoneStatus::Completed)))
        .await
        .unwrap();
    wait_for(|| core.workspace.chat(CHAT).unwrap().unwrap().title.is_some()).await;
    assert_eq!(titles.load(Ordering::SeqCst), 1);
    assert_eq!(
        core.workspace.chat(CHAT).unwrap().unwrap().title.as_deref(),
        Some("Generated fallback")
    );
}

#[tokio::test]
async fn native_full_prompt_title_is_short_single_line_and_cannot_replace_user_title() {
    let dir = tempfile::tempdir().unwrap();
    let (core, mut started, titles) = setup(dir.path());
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, request(&core, "title"), None)
        .await
        .unwrap();
    let run = started.recv().await.unwrap();
    let raw = format!(
        "  # {}\nRun shell: printf 'private instructions'",
        "界".repeat(350)
    );
    run.events
        .send(Ok(AgentEvent::SessionTitle { title: raw }))
        .await
        .unwrap();
    wait_for(|| core.workspace.chat(CHAT).unwrap().unwrap().title.is_some()).await;
    let title = core.workspace.chat(CHAT).unwrap().unwrap().title.unwrap();
    assert_eq!(title, "界".repeat(60));
    core.workspace.rename_chat(CHAT, "User title").unwrap();
    run.events
        .send(Ok(AgentEvent::SessionTitle {
            title: "Another native title".into(),
        }))
        .await
        .unwrap();
    run.events
        .send(Ok(done(DoneStatus::Completed)))
        .await
        .unwrap();
    wait_for(|| core.sessions.session_status(CHAT).unwrap().status == SessionStatus::Idle).await;
    assert_eq!(
        core.workspace.chat(CHAT).unwrap().unwrap().title.as_deref(),
        Some("User title")
    );
    assert_eq!(titles.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn parked_metadata_preserves_idle_and_terminal_journal() {
    let dir = tempfile::tempdir().unwrap();
    let (core, mut started, _) = setup(dir.path());
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, request(&core, "finish"), None)
        .await
        .unwrap();
    let run = started.recv().await.unwrap();
    run.events
        .send(Ok(AgentEvent::TextDelta {
            text: "Finished".into(),
        }))
        .await
        .unwrap();
    run.events
        .send(Ok(done(DoneStatus::Completed)))
        .await
        .unwrap();
    wait_for(|| core.sessions.session_status(CHAT).unwrap().status == SessionStatus::Idle).await;
    let handle = core.doc_host.open(CHAT).unwrap();
    let before = handle.doc().read_entries().unwrap();
    run.events
        .send(Ok(AgentEvent::SessionTitle {
            title: "Settled title".into(),
        }))
        .await
        .unwrap();
    run.events
        .send(Ok(AgentEvent::SessionMode {
            id: "mode".into(),
            value: "plan".into(),
        }))
        .await
        .unwrap();
    wait_for(|| core.workspace.chat_config(CHAT).unwrap().model_options["mode"] == "plan").await;
    assert_eq!(
        core.sessions.session_status(CHAT).unwrap().status,
        SessionStatus::Idle
    );
    assert_eq!(handle.doc().read_entries().unwrap(), before);
    assert_eq!(
        core.sessions.last_request(CHAT).unwrap().model_options["mode"],
        "plan"
    );
    let replay = core.sessions.subscribe(CHAT, 0).unwrap().0;
    assert!(
        matches!(replay.last().unwrap().event, AgentEvent::Done { .. }),
        "post-turn metadata must not make crash recovery revive a completed turn"
    );
}

async fn canonical_child_updates_across_boundary(parked: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (core, mut started, _) = setup(dir.path());
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::Mock,
            request(&core, "launch background child"),
            None,
        )
        .await
        .unwrap();
    let run = started.recv().await.unwrap();
    let agent = |description: &str| ToolCall::Unknown {
        name: format!("Agent: {description}"),
        input: Some(
            serde_json::json!({"subagent_type":"explore", "prompt":"private launch input"}),
        ),
    };
    for event in [
        AgentEvent::ToolCall {
            id: "child-launch".into(),
            call: agent("Trace callers"),
        },
        AgentEvent::ToolProgress {
            id: "child-launch".into(),
            output: Some("Starting child".into()),
        },
        AgentEvent::ToolCall {
            id: "ordinary".into(),
            call: ToolCall::Exec {
                command: "echo original".into(),
            },
        },
        AgentEvent::ToolResult {
            id: "ordinary".into(),
            is_error: false,
            output: Some("original".into()),
            diff: None,
        },
        AgentEvent::TextDelta {
            text: "Child is running in background.".into(),
        },
    ] {
        run.events.send(Ok(event)).await.unwrap();
    }
    let boundary = if parked {
        done(DoneStatus::Completed)
    } else {
        AgentEvent::Steered {
            assistant_message_id: None,
            next_assistant_message_id: Some("next-turn".into()),
        }
    };
    run.events.send(Ok(boundary)).await.unwrap();
    let handle = core.doc_host.open(CHAT).unwrap();
    wait_for(|| {
        handle.doc().read_entries().unwrap().iter().any(|entry| {
            entry.status == Some(MessageStatus::Complete)
                && entry.parts.iter().any(|part| part.id() == "child-launch")
        })
    })
    .await;
    let original = handle
        .doc()
        .read_entries()
        .unwrap()
        .into_iter()
        .find(|entry| entry.parts.iter().any(|part| part.id() == "child-launch"))
        .unwrap();
    let status = core.sessions.session_status(CHAT).unwrap();
    if !parked {
        run.events
            .send(Ok(AgentEvent::TextDelta {
                text: "Next parent text".into(),
            }))
            .await
            .unwrap();
    }
    // Real Mimir frames refresh the canonical launch's shape, then its public
    // output. Ordinary old-tool echoes and retyping attempts remain inert.
    for event in [
        AgentEvent::ToolCall {
            id: "ordinary".into(),
            call: agent("not a real child"),
        },
        AgentEvent::ToolResult {
            id: "ordinary".into(),
            is_error: true,
            output: Some("stale echo".into()),
            diff: None,
        },
        AgentEvent::ToolCall {
            id: "child-launch".into(),
            call: agent("Trace callers updated"),
        },
        AgentEvent::ToolProgress {
            id: "child-launch".into(),
            output: Some("Tracing public callers".into()),
        },
    ] {
        run.events.send(Ok(event)).await.unwrap();
    }
    wait_for(|| handle.doc().read_entries().unwrap().iter().any(|entry| entry.id == original.id && entry.parts.iter().any(|part| matches!(part, MessagePart::Tool { id, output: Some(text), resolved: false, .. } if id == "child-launch" && text == "Tracing public callers")))).await;
    let full = format!("Public child result\n{}", "public detail\n".repeat(80));
    run.events
        .send(Ok(AgentEvent::ToolResult {
            id: "child-launch".into(),
            is_error: true,
            output: Some(full.clone()),
            diff: None,
        }))
        .await
        .unwrap();
    wait_for(|| handle.doc().read_entries().unwrap().iter().any(|entry| entry.id == original.id && entry.parts.iter().any(|part| matches!(part, MessagePart::Tool { id, resolved: true, is_error: true, .. } if id == "child-launch")))).await;
    assert_eq!(
        core.doc_host
            .fetch_tool_blob("shared-acp/child-launch")
            .await
            .unwrap(),
        full
    );
    let entries = handle.doc().read_entries().unwrap();
    let updated = entries
        .iter()
        .find(|entry| entry.id == original.id)
        .unwrap();
    assert_eq!(updated.status, Some(MessageStatus::Complete));
    assert!(updated.parts.iter().any(|part| matches!(part, MessagePart::Tool { id, call: ToolCall::Unknown { name, input }, .. } if id == "child-launch" && name == "Agent: Trace callers updated" && input.as_ref().is_some_and(|i| i.get("prompt").is_none()))));
    assert_eq!(
        updated.parts.iter().find(|p| p.id() == "ordinary"),
        original.parts.iter().find(|p| p.id() == "ordinary")
    );
    assert_eq!(
        entries
            .iter()
            .flat_map(|e| &e.parts)
            .filter(|p| p.id() == "child-launch")
            .count(),
        1,
        "no phantom launch chips"
    );
    let after = core.sessions.session_status(CHAT).unwrap();
    assert_eq!(after.status, status.status);
    assert_eq!(after.last_completed_turn, status.last_completed_turn);
    let journal = core.sessions.subscribe(CHAT, 0).unwrap().0;
    assert_eq!(
        journal
            .iter()
            .filter(|e| matches!(e.event, AgentEvent::Done { .. }))
            .count(),
        usize::from(parked),
        "child completion must not manufacture a parent completion"
    );
    if parked {
        assert_eq!(entries.len(), 2, "no phantom parent entry");
        drop(run.events);
        wait_for(|| run.controls.steering.is_closed()).await;
        assert_eq!(
            core.sessions.recover_stale().unwrap(),
            0,
            "late child output must not revive a completed parent"
        );
    } else {
        run.events
            .send(Ok(AgentEvent::TextDelta {
                text: " remains contiguous".into(),
            }))
            .await
            .unwrap();
        run.events
            .send(Ok(done(DoneStatus::Completed)))
            .await
            .unwrap();
        wait_for(|| core.sessions.session_status(CHAT).unwrap().status == SessionStatus::Idle)
            .await;
        let entries = handle.doc().read_entries().unwrap();
        let next = entries.iter().find(|e| e.id == "next-turn").unwrap();
        assert!(
            matches!(&next.parts[..], [MessagePart::Text { text, .. }] if text == "Next parent text remains contiguous")
        );
    }
}

#[tokio::test]
async fn canonical_child_updates_after_done_refresh_original_entry_without_reopening_parent() {
    canonical_child_updates_across_boundary(true).await;
}

#[tokio::test]
async fn canonical_child_updates_after_steered_do_not_split_the_next_parent_message() {
    canonical_child_updates_across_boundary(false).await;
}
