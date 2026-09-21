//! Explicit, bounded live ACP form check through the public Mimir harness.
//!
//! Build normally, then run with an isolated HOME and an existing login-managed
//! MIMIR_CODING_AGENT_DIR; never copy credentials. Requires a disposable cwd and
//! the absolute Mimir executable as arguments. Uses chatgpt/gpt-5.6-luna medium.
//! The answers below are declared test data, not guessed answers to user work.

use std::{path::PathBuf, time::Duration};

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use kratos_harness::{AcpHarness, CancellationToken, Harness, RunControls};
use kratos_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, UserInputAnswer,
    UserInputQuestion,
};

const PROMPT: &str = "This is an ACP form protocol verification in a disposable workspace. \
Do not use shell, edit/write tools, subagents or other tools; do not create or modify files. \
Call ask_user exactly once with these exact JSON arguments: \
{\"questions\":[{\"id\":\"sample_name\",\"prompt\":\"What sample name?\"},\
{\"id\":\"language\",\"prompt\":\"Which implementation language?\",\"options\":[\
{\"label\":\"Rust\",\"description\":\"Compiled\"},{\"label\":\"Python\",\"description\":\"Scripting\"}]},\
{\"id\":\"checks\",\"prompt\":\"Which verification checks?\",\"allow_multiple\":true,\"options\":[\
{\"label\":\"Unit\",\"description\":\"Fast\"},{\"label\":\"Integration\",\"description\":\"Thorough\"}]}]}. \
The first question intentionally omits the optional options property, which is how this tool \
represents free text. Do not invent Free text/Custom choices or add an options property there. \
Wait for the supplied answers. Then report every accepted name, selection and optional note \
verbatim in one short final response and stop. Do not request credentials. \
If ask_user is unavailable, state that explicitly instead of simulating the interaction.";

// Separate coverage, not a fallback: `full` continues to require a free-text
// root question; `choices` isolates single/multiple selection plus text notes.
const CHOICES_PROMPT: &str = "This is an ACP form protocol verification in a disposable workspace. \
Do not use shell, edit/write tools, subagents or other tools; do not modify files. \
Call ask_user exactly once with these two questions: id language, prompt 'Which implementation \
language?', single choice Rust (description 'Compiled') and Python (description 'Scripting'); \
id checks, prompt 'Which verification checks?', allow_multiple true, choices Unit (description \
'Fast') and Integration (description 'Thorough'). Wait for supplied answers, then repeat all \
accepted selections and optional notes verbatim in a short final response and stop. \
Do not request credentials or invent answers.";

fn sample_answers(questions: &[UserInputQuestion]) -> Result<Vec<UserInputAnswer>, String> {
    let mut notes = 0;
    questions
        .iter()
        .map(|question| {
            let label = |needle: &str| {
                question
                    .options
                    .iter()
                    .find(|option| option.contains(needle))
                    .cloned()
            };
            let labels = if question.question.contains("What sample name?") {
                if !question.options.is_empty() {
                    return Err("Expected free-text sample name, but agent invented choices; no answer supplied".into());
                }
                vec!["ACP_SAMPLE_CASEY".into()]
            } else if question.question.contains("Which implementation language?") {
                vec![label(". Rust").ok_or("Missing declared Rust choice")?]
            } else if question.question.contains("Which verification checks?") {
                vec![
                    label(". Unit").ok_or("Missing declared Unit choice")?,
                    label(". Integration").ok_or("Missing declared Integration choice")?,
                ]
            } else {
                return Err(
                    "Agent asked an unexpected question; no automatic answer supplied".into(),
                );
            };
            // The merged Mimir shape carries the optional note on the choice
            // question itself: the first note-capable answer carries the note
            // text, later ones leave it blank (no separate note pages).
            let note = if question.note {
                notes += 1;
                (notes == 1).then(|| "ACP_NOTE_EXACT".to_string())
            } else {
                None
            };
            Ok(UserInputAnswer {
                question_id: question.id.clone(),
                labels,
                note,
            })
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let cwd = PathBuf::from(
        args.next()
            .ok_or("pass disposable cwd then absolute Mimir binary")?,
    )
    .canonicalize()?;
    let binary = PathBuf::from(args.next().ok_or("pass absolute Mimir binary")?).canonicalize()?;
    let scenario = args.next().unwrap_or_else(|| "full".into());
    let prompt = match scenario.as_str() {
        "full" => PROMPT,
        "choices" => CHOICES_PROMPT,
        _ => return Err("scenario must be full or choices".into()),
    };
    if !cwd.is_dir() {
        return Err("disposable cwd must already exist".into());
    }
    let (input_tx, mut inputs) = mpsc::unbounded_channel();
    let (steer_tx, steering) = mpsc::channel(1);
    drop(steer_tx);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (sender, receiver) = oneshot::channel();
            let _ = input_tx.send((questions, sender));
            receiver
        }),
        steering,
        interrupt: interrupt.clone(),
    };
    let request = RunRequest {
        prompt: prompt.into(),
        harness: Some(HarnessId::Mimir),
        model: Some("chatgpt/gpt-5.6-luna".into()),
        reasoning: Some(ReasoningLevel::Medium),
        model_options: Default::default(),
        cwd: cwd.to_string_lossy().into_owned(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        resume: None,
        attachments: Vec::new(),
        worktree: None,
    };
    let mut stream = AcpHarness::mimir()
        .with_executable(binary)
        .with_handshake_timeout(Duration::from_secs(30))
        .run(request, controls)
        .await?;
    let deadline = tokio::time::sleep(Duration::from_secs(180));
    tokio::pin!(deadline);
    let mut forms = 0;
    let mut final_text = String::new();
    let mut statuses = Vec::new();
    loop {
        tokio::select! {
            biased;
            () = &mut deadline => {
                interrupt.cancel();
                let _ = tokio::time::timeout(Duration::from_secs(5), async { while stream.next().await.is_some() {} }).await;
                return Err("live form check exceeded 180 seconds".into());
            }
            Some((questions, sender)) = inputs.recv() => {
                forms += 1;
                if forms != 1 {
                    drop(sender);
                    interrupt.cancel();
                    return Err("Agent requested more than the declared single form".into());
                }
                println!("{}", serde_json::json!({"event":"questions", "questions":questions}));
                let answers = sample_answers(&questions)?;
                println!("{}", serde_json::json!({"event":"test_answers", "answers":answers}));
                sender.send(answers).map_err(|_| "form cancelled before answers could be delivered")?;
            }
            event = stream.next() => match event {
                None => break,
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(AgentEvent::TextDelta { text })) => {
                    println!("{}", serde_json::json!({"event":"public_text", "text":text}));
                    final_text.push_str(&text);
                }
                Some(Ok(AgentEvent::Done { status, error, .. })) => {
                    println!("{}", serde_json::json!({"event":"done", "status":status, "error":error}));
                    statuses.push(status);
                }
                Some(Ok(_)) => {}
            }
        }
    }
    let verified = forms == 1
        && statuses == [DoneStatus::Completed]
        && ["ACP_NOTE_EXACT", "Rust", "Unit", "Integration"]
            .iter()
            .all(|value| final_text.contains(value))
        && (scenario != "full" || final_text.contains("ACP_SAMPLE_CASEY"));
    println!(
        "{}",
        serde_json::json!({"event":"verification", "scenario":scenario, "forms":forms, "verified":verified})
    );
    if !verified {
        return Err("live form round-trip verification failed; inspect public output".into());
    }
    Ok(())
}
