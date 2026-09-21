//! Large public ACP payloads survive the real harness, normalized journal, and
//! the public "Show full output" retrieval API while the synced doc stays small.
#![cfg(unix)]

use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use kratos_doc::MessagePart;
use kratos_engine::{EngineCore, HarnessRegistry};
use kratos_harness::AcpHarness;
use kratos_proto::{AgentEvent, ChatConfig, HarnessId, RunRequest, SandboxLevel, ToolCall};

const CHAT: &str = "acp-large-public";
const PEER: &str = r#"#!/usr/bin/env python3
import json, sys

def send(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)

def update(value):
    send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {'sessionId': 'large-output', 'update': value}})

def text(value):
    return {'type': 'content', 'content': {'type': 'text', 'text': value}}

def document(value):
    return {'type': 'content', 'content': {'type': 'resource', 'resource': {
        'uri': 'urn:mimir:proposal:proposal', 'mimeType': 'text/markdown', 'text': value}}}

for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if 'id' not in request:
        continue
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentCapabilities': {}}
    elif method == 'session/new':
        result = {'sessionId': 'large-output'}
    elif method == 'session/prompt':
        update({'sessionUpdate': 'tool_call', 'toolCallId': 'proposal', 'kind': 'other',
                'title': 'Output preservation', 'status': 'in_progress'})
        update({'sessionUpdate': 'tool_call_update', 'toolCallId': 'proposal',
                'content': [text('Plan outline should not be duplicated'), document('# Draft\nInitial draft')]})
        proposal = '# Design\n' + 'proposal line ✓\n' * 4000 + '\nFINAL DELIVERY STEP'
        update({'sessionUpdate': 'tool_call_update', 'toolCallId': 'proposal', 'status': 'completed',
                'content': [text('Plan outline should not be duplicated'), document(proposal)],
                'rawOutput': {'details': 'private-secret'}})
        update({'sessionUpdate': 'tool_call', 'toolCallId': 'shell', 'kind': 'execute',
                'title': 'Run command', 'status': 'in_progress', 'rawInput': {'command': 'cargo check'}})
        stdout = 'stdout line\n' * 5000
        diagnostic = 'STDERR: FINAL E0425 unknown symbol — diagnostic tail'
        update({'sessionUpdate': 'tool_call_update', 'toolCallId': 'shell', 'kind': 'execute',
                'title': 'Command failed', 'status': 'failed',
                'content': [text(stdout), text(diagnostic)],
                'rawOutput': {'result': 'private-secret: not a fallback'}})
        result = {'stopReason': 'end_turn'}
    else:
        result = {}
    send({'jsonrpc': '2.0', 'id': request['id'], 'result': result})
"#;

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("ACP output must reach the document");
}

#[tokio::test]
async fn full_markdown_and_final_stderr_are_retrievable_after_restart() {
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let peer = dir.path().join("public-output-peer.py");
    std::fs::write(&peer, PEER).unwrap();
    std::fs::set_permissions(&peer, std::fs::Permissions::from_mode(0o755)).unwrap();
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(Arc::new(AcpHarness::mimir().with_executable(peer)));
    let core = EngineCore::assemble(dir.path(), registry.clone(), HarnessId::Mimir, None).unwrap();
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
            Some(dir.path().display().to_string()),
        )
        .unwrap();
    core.sessions
        .dispatch(
            CHAT,
            HarnessId::Mimir,
            RunRequest {
                prompt: "Exercise public document and command output".into(),
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
            },
            None,
        )
        .await
        .unwrap();
    wait_for(|| {
        core.doc_host
            .open(CHAT)
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .iter()
            .flat_map(|entry| &entry.parts)
            .filter(|part| {
                matches!(
                    part,
                    MessagePart::Tool {
                        resolved: true,
                        output_ref: Some(_),
                        ..
                    }
                )
            })
            .count()
            == 2
    })
    .await;

    let proposal = format!(
        "# Design\n{}\nFINAL DELIVERY STEP",
        "proposal line ✓\n".repeat(4000)
    );
    let diagnostic = "STDERR: FINAL E0425 unknown symbol — diagnostic tail";
    let shell = format!("{}\n{diagnostic}", "stdout line\n".repeat(5000));
    let entries = core
        .doc_host
        .open(CHAT)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    let mut saved = Vec::new();
    for part in entries.iter().flat_map(|entry| &entry.parts) {
        let MessagePart::Tool {
            id,
            call,
            output: Some(summary),
            output_ref: Some(reference),
            output_bytes: Some(bytes),
            is_error,
            resolved: true,
            ..
        } = part
        else {
            continue;
        };
        let expected = if id == "proposal" { &proposal } else { &shell };
        let fetched = core.doc_host.fetch_tool_blob(reference).await.unwrap();
        assert!(
            fetched.ends_with(if id == "proposal" {
                "FINAL DELIVERY STEP"
            } else {
                diagnostic
            }),
            "public output tail was lost for {id}"
        );
        assert_eq!(&fetched, expected);
        assert_eq!(*bytes, expected.len() as u64);
        assert!(summary.chars().count() <= kratos_doc::TOOL_OUTPUT_SUMMARY_MAX + 1);
        if id == "proposal" {
            assert_eq!(
                call,
                &ToolCall::Document {
                    title: "Output preservation".into()
                }
            );
            assert!(!is_error);
            assert!(!fetched.contains("Initial draft"));
            assert!(!fetched.contains("Plan outline"));
        } else {
            assert_eq!(
                call,
                &ToolCall::Exec {
                    command: "cargo check".into()
                }
            );
            assert!(*is_error);
        }
        saved.push((reference.clone(), expected.clone()));
    }
    assert_eq!(saved.len(), 2);
    let journal = core.sessions.subscribe(CHAT, 0).unwrap().0;
    assert!(journal.iter().any(|event| matches!(&event.event,
        AgentEvent::ToolResult { id, output: Some(output), .. } if id == "shell" && output == &shell)));
    assert!(journal.iter().all(|entry| {
        !serde_json::to_string(&entry.event)
            .unwrap()
            .contains("private-secret")
    }));
    core.shutdown().await;
    drop(core);

    let reopened = EngineCore::assemble(dir.path(), registry, HarnessId::Mimir, None).unwrap();
    for (reference, expected) in saved {
        assert_eq!(
            reopened.doc_host.fetch_tool_blob(&reference).await.unwrap(),
            expected
        );
    }
    reopened.shutdown().await;
}
