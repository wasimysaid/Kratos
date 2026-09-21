//! ACP `session/update` → [`AgentEvent`] mapping (protocol v1, the line the
//! claude/codex adapters and Grok Build speak today).
//!
//! Tolerant by construction, like the codex normalizer: decoded from raw
//! [`Value`]s, unknown `sessionUpdate` kinds and content types map to nothing,
//! and missing fields degrade to empty strings rather than errors. Wire shapes
//! verified against `agent-client-protocol-schema` 1.3.0 (`SessionUpdate` is
//! tagged `sessionUpdate`/snake_case; structs are camelCase; tool kinds and
//! statuses are snake_case).

use std::collections::VecDeque;

use serde_json::Value;
use kratos_proto::{AgentEvent, SlashCommand, TodoItem, ToolCall, ToolDiff};

/// Full public payload budget for normalized events, journal replay and output
/// retrieval. Keep Mimir's 64 KiB proposal body plus outline/diagnostics intact;
/// the doc fold independently derives its tiny synced preview from this text.
pub(crate) const OUTPUT_CAP: usize = 256 * 1024;

/// Tool identity/argument text has a separate small budget, not the output one.
const SHAPE_TEXT_CAP: usize = 16 * 1024;

/// Byte cap for each side of an inline diff crossing the event stream.
pub(crate) const DIFF_TEXT_CAP: usize = 64 * 1024;

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_owned()
}

/// Truncate on a char boundary, marking the cut so the UI can say "truncated".
pub(crate) fn cap_text(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_owned();
    out.push_str("\n… [truncated]");
    out
}

/// Public text in an ACP `ContentBlock`, including embedded text resources.
/// Binary attachments and resource links are not fetched or decoded here.
fn content_block_text(block: &Value) -> Option<&str> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => block.get("text")?.as_str(),
        "resource" => block.get("resource")?.get("text")?.as_str(),
        _ => None,
    }
}

/// A `ContentChunk`'s streamed text (`{content: {type: "text", ...}}`).
pub(crate) fn chunk_text(update: &Value) -> Option<String> {
    let text = content_block_text(update.get("content")?)?;
    (!text.is_empty()).then(|| text.to_owned())
}

/// A public embedded Markdown resource is a document, not a title convention.
fn markdown_resource_text(block: &Value) -> Option<&str> {
    if block.get("type").and_then(Value::as_str) != Some("resource") {
        return None;
    }
    let resource = block.get("resource")?;
    (resource.get("mimeType").and_then(Value::as_str) == Some("text/markdown"))
        .then(|| resource.get("text").and_then(Value::as_str))
        .flatten()
}

/// Join only bounded public content, never an unbounded intermediate string.
/// Documents omit the agent's redundant plain-text outline and retain just the
/// Markdown resources. Other tools retain all public text, including resources.
fn tool_output(update: &Value, document: bool) -> Option<String> {
    let parts = update
        .get("content")?
        .as_array()?
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("content"))
        .filter_map(|c| {
            let block = c.get("content")?;
            if document {
                markdown_resource_text(block)
            } else {
                content_block_text(block)
            }
        })
        .filter(|text| !text.is_empty());
    let mut output = String::new();
    for text in parts {
        if !output.is_empty() {
            if output.len() == OUTPUT_CAP {
                output.push_str("\n… [truncated]");
                break;
            }
            output.push('\n');
        }
        let remaining = OUTPUT_CAP - output.len();
        if text.len() > remaining {
            output.push_str(&cap_text(text, remaining));
            break;
        }
        output.push_str(text);
    }
    (!output.is_empty()).then_some(output)
}

/// First `{type: "diff"}` entry of a tool call's `content` array.
fn tool_diff(update: &Value) -> Option<ToolDiff> {
    let diff = update
        .get("content")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("diff"))?;
    let path = str_field(diff, "path");
    if path.is_empty() {
        return None;
    }
    Some(ToolDiff {
        path: cap_text(&path, SHAPE_TEXT_CAP),
        old_text: diff
            .get("oldText")
            .and_then(Value::as_str)
            .map(|t| cap_text(t, DIFF_TEXT_CAP)),
        new_text: cap_text(
            diff.get("newText").and_then(Value::as_str).unwrap_or(""),
            DIFF_TEXT_CAP,
        ),
    })
}

/// The grok-native tool name stamped on a tool call's `_meta` (`x.ai/tool`,
/// present on every grok tool_call — verified live, 1.0.4).
pub(crate) fn xai_tool_name(update: &Value) -> Option<&str> {
    update.get("_meta")?.get("x.ai/tool")?.get("name")?.as_str()
}

/// First location path (`locations: [{path, line?}]`), for read/edit calls.
fn first_location(update: &Value) -> Option<String> {
    let path = update.get("locations")?.as_array()?.first()?.get("path")?;
    path.as_str().filter(|p| !p.is_empty()).map(str::to_owned)
}

/// Cursor (and similar ACP agents) put a human summary in `title` — generic
/// labels ("Read File", "grep") before args arrive, or a markdown-wrapped
/// command (`` `ls -la` `` with inner backticks escaped as `\``). Those are
/// display strings, not typed arguments; using them as path/pattern/command
/// dumps the label (and its escapes) into the transcript chip.
fn is_placeholder_title(title: &str) -> bool {
    matches!(
        title.trim(),
        "grep"
            | "Find"
            | "Terminal"
            | "Read File"
            | "Edit File"
            | "Delete File"
            | "Web Search"
            | "Web Fetch"
            | "Codebase Search"
            | "Read TODOs"
            | "Update TODOs"
            | "Read Lints"
            | "Task: Subagent task"
            | "Subagent task"
            | "List MCP Resources"
            | "Fetch MCP Resource"
    )
}

/// Unwrap a single markdown code span used as an ACP exec title.
fn unwrap_command_title(title: &str) -> Option<String> {
    let inner = title.trim().strip_prefix('`')?.strip_suffix('`')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'`') {
            chars.next();
            out.push('`');
        } else {
            out.push(c);
        }
    }
    let out = out.trim();
    if out.is_empty() || is_placeholder_title(out) {
        None
    } else {
        Some(out.to_owned())
    }
}

fn arg_from_title(title: &str) -> Option<String> {
    let t = title.trim();
    if t.is_empty() || is_placeholder_title(t) {
        None
    } else {
        Some(t.to_owned())
    }
}

/// Reduce an ACP tool call (kind + title + rawInput + locations + diff
/// content) to the typed [`ToolCall`] kratos renders. Best-effort: agents vary
/// in how much structure they put in `rawInput`, so every arm has a fallback.
/// Title is only used when it looks like a real arg — never a placeholder
/// label or markdown-escaped summary.
const SEMANTIC_CONTENT_META_KEY: &str = "mimir.dev/tool-content";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemanticContent {
    Answer,
    Report,
}

/// Mimir's public, versioned ACP extension is the sole semantic-content signal.
/// Its small exact shape survives bounded raw input, sparse completion, and replay.
fn semantic_content(update: &Value) -> Option<SemanticContent> {
    let marker = update
        .get("_meta")?
        .get(SEMANTIC_CONTENT_META_KEY)?
        .as_object()?;
    if marker.len() != 3
        || marker.get("version").and_then(Value::as_u64) != Some(1)
        || marker.get("format").and_then(Value::as_str) != Some("markdown")
    {
        return None;
    }
    match marker.get("kind").and_then(Value::as_str)? {
        "answer" => Some(SemanticContent::Answer),
        "report" => Some(SemanticContent::Report),
        _ => None,
    }
}

fn typed_call(update: &Value) -> ToolCall {
    let kind = update
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("other");
    let title = str_field(update, "title");
    let raw = update.get("rawInput").filter(|v| !v.is_null());
    let raw_str = |key: &str| -> Option<String> {
        raw.and_then(|r| r.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    if let Some(semantic) = semantic_content(update) {
        let title = if title.is_empty() {
            match semantic {
                SemanticContent::Answer => "Answer",
                SemanticContent::Report => "Report",
            }
            .into()
        } else {
            title
        };
        return match semantic {
            SemanticContent::Answer => ToolCall::Answer { title },
            SemanticContent::Report => ToolCall::Report { title },
        };
    }

    match kind {
        "execute" => ToolCall::Exec {
            command: raw_str("command")
                .or_else(|| unwrap_command_title(&title))
                .or_else(|| arg_from_title(&title))
                .unwrap_or_default(),
        },
        "read" => ToolCall::ReadFile {
            path: raw_str("path")
                .or_else(|| raw_str("file_path"))
                .or_else(|| raw_str("filePath"))
                .or_else(|| first_location(update))
                .or_else(|| arg_from_title(&title))
                .unwrap_or_default(),
        },
        "edit" | "delete" | "move" => {
            // A diff pins down the file and shape; otherwise fall back to the
            // location/rawInput path with unknown content.
            if let Some(diff) = tool_diff(update) {
                if diff.old_text.is_none() {
                    ToolCall::WriteFile {
                        path: diff.path,
                        content: None,
                    }
                } else {
                    ToolCall::EditFile {
                        path: diff.path,
                        old_string: None,
                        new_string: None,
                    }
                }
            } else {
                match raw_str("path")
                    .or_else(|| raw_str("file_path"))
                    .or_else(|| raw_str("filePath"))
                    .or_else(|| first_location(update))
                {
                    Some(path) if kind == "edit" => ToolCall::EditFile {
                        path,
                        old_string: None,
                        new_string: None,
                    },
                    path => ToolCall::ApplyPatch { path },
                }
            }
        }
        "search" => {
            // Cursor web search is kind "search" + rawInput.searchTerm; grep
            // / glob / codebase search use pattern or query.
            if let Some(query) = raw_str("searchTerm") {
                ToolCall::WebSearch { query }
            } else {
                ToolCall::Search {
                    pattern: raw_str("pattern")
                        .or_else(|| raw_str("globPattern"))
                        .or_else(|| raw_str("query"))
                        .or_else(|| arg_from_title(&title))
                        .unwrap_or_default(),
                    path: raw_str("path"),
                }
            }
        }
        "fetch" => match raw_str("url") {
            Some(url) => ToolCall::WebFetch { url, prompt: None },
            None => ToolCall::WebSearch {
                query: raw_str("searchTerm")
                    .or_else(|| raw_str("query"))
                    .or_else(|| arg_from_title(&title))
                    .unwrap_or_default(),
            },
        },
        // Kindless (or unknown-kind) update carrying a diff: an edit in all
        // but name — some agents only attach the diff on the completion
        // update without repeating the kind.
        _ if tool_diff(update).is_some() => {
            let diff = tool_diff(update).expect("guarded");
            if diff.old_text.is_none() {
                ToolCall::WriteFile {
                    path: diff.path,
                    content: None,
                }
            } else {
                ToolCall::EditFile {
                    path: diff.path,
                    old_string: None,
                    new_string: None,
                }
            }
        }
        // Grok's subagent spawn: name the chip — and the subagent tab it
        // opens — after the task, matching the claude driver's "Agent: {d}"
        // (the bare tool name says nothing in a tab strip).
        _ if xai_tool_name(update) == Some("spawn_subagent") => ToolCall::Unknown {
            name: raw_str("description")
                .map(|d| format!("Agent: {d}"))
                .unwrap_or_else(|| "Agent".into()),
            input: raw.cloned(),
        },
        // Devin's run_subagent tool uses a coarse ACP kind; its private meta
        // field is the stable identity across pending/in-progress frames.
        _ if update
            .get("_meta")
            .and_then(|m| m.get("cognition.ai/inferenceToolName"))
            .and_then(Value::as_str)
            == Some("run_subagent") =>
        {
            ToolCall::Unknown {
                name: raw_str("title")
                    .map(|d| format!("Agent: {d}"))
                    .unwrap_or_else(|| "Agent".into()),
                input: raw.cloned(),
            }
        }
        // opencode's subagent spawn (`task` tool — rawInput carries
        // description/prompt/subagent_type): same naming as grok's, so the
        // chip and its subagent tab say what the agent is doing.
        _ if raw.is_some_and(|r| r.get("subagent_type").is_some() && r.get("prompt").is_some()) => {
            ToolCall::Unknown {
                name: raw_str("description")
                    .map(|d| format!("Agent: {d}"))
                    .unwrap_or_else(|| "Agent".into()),
                input: raw.cloned(),
            }
        }
        // Its completion drops rawInput but keeps a title (the description),
        // which would re-type the chip to a bare Unknown and cost it the
        // Agent icon/label. The rawOutput metadata (child + parent session
        // ids) still marks the spawn — keep the naming.
        _ if update
            .get("rawOutput")
            .and_then(|r| r.get("metadata"))
            .is_some_and(|m| {
                m.get("sessionId").is_some() && m.get("parentSessionId").is_some()
            }) =>
        {
            ToolCall::Unknown {
                name: if title.is_empty() {
                    "Agent".into()
                } else {
                    format!("Agent: {title}")
                },
                input: raw.cloned(),
            }
        }
        _ if raw_str("_toolName").as_deref() == Some("task") => ToolCall::Unknown {
            name: raw_str("description")
                .filter(|d| d != "Subagent task")
                .map(|d| format!("Task: {d}"))
                .or_else(|| arg_from_title(&title))
                .unwrap_or_else(|| {
                    if title.is_empty() {
                        "Task".into()
                    } else {
                        title
                    }
                }),
            input: raw.cloned(),
        },
        _ => ToolCall::Unknown {
            name: if title.is_empty() { kind.into() } else { title },
            input: raw.cloned(),
        },
    }
}

/// Keep bounded recent tool snapshots for the session, including completed
/// calls: canonical agent tools can receive further updates on later turns.
/// Raw output and journal data are never retained; the least recently updated
/// snapshot is evicted when the session reaches the tool limit.
const MAX_TRACKED_TOOLS: usize = 128;
const SHAPE_FIELD_CAP: usize = 64 * 1024;
const SHAPE_FIELDS: [&str; 5] = ["kind", "title", "rawInput", "locations", "_meta"];

#[derive(Default)]
struct ToolSnapshot {
    shape: serde_json::Map<String, Value>,
    document: bool,
    output: Option<String>,
    diff: Option<ToolDiff>,
}

/// Stateful projection for one ACP session. ACP update fields are optional;
/// omitted fields retain their previous value, while content arrays replace it.
#[derive(Default)]
pub(crate) struct UpdateNormalizer {
    tools: VecDeque<(String, ToolSnapshot)>,
    message_id: Option<String>,
    has_message_text: bool,
}

impl UpdateNormalizer {
    pub(crate) fn map_update(&mut self, update: &Value) -> Vec<AgentEvent> {
        let kind = update.get("sessionUpdate").and_then(Value::as_str);
        if matches!(kind, Some("tool_call" | "tool_call_update")) {
            let id = str_field(update, "toolCallId");
            let previous = self
                .tools
                .iter()
                .position(|(key, _)| key == &id)
                .and_then(|index| self.tools.remove(index));
            let mut snapshot = previous.map(|(_, tool)| tool).unwrap_or_default();
            let events = map_tool_update(update, &mut snapshot);
            if !id.is_empty() && id.len() <= SHAPE_TEXT_CAP {
                if self.tools.len() == MAX_TRACKED_TOOLS {
                    self.tools.pop_front();
                }
                self.tools.push_back((id, snapshot));
            }
            return events;
        }

        let mut events = map_update(update);
        if kind == Some("agent_message_chunk") && !events.is_empty() {
            if let Some(id) = update
                .get("messageId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= SHAPE_TEXT_CAP)
            {
                if self.has_message_text && self.message_id.as_deref() != Some(id) {
                    events.insert(0, AgentEvent::MessageBoundary);
                }
                self.message_id = Some(id.to_owned());
            }
            self.has_message_text = true;
        }
        events
    }
}

/// Oversized arguments need not evict useful typed fields (for example a shell
/// command alongside a large unrelated value). Retain only fields we interpret.
fn bounded_input(input: &Value) -> Value {
    if input.to_string().len() <= SHAPE_FIELD_CAP {
        return input.clone();
    }
    let mut projected = serde_json::Map::new();
    for key in [
        "command",
        "path",
        "file_path",
        "filePath",
        "searchTerm",
        "pattern",
        "globPattern",
        "query",
        "url",
        "title",
        "description",
        "_toolName",
        "subagent_type",
        "prompt",
    ] {
        if let Some(text) = input.get(key).and_then(Value::as_str) {
            projected.insert(key.into(), Value::String(cap_text(text, SHAPE_TEXT_CAP)));
        }
    }
    Value::Object(projected)
}

fn map_tool_update(update: &Value, snapshot: &mut ToolSnapshot) -> Vec<AgentEvent> {
    let id = str_field(update, "toolCallId");
    let has_shape = update.get("sessionUpdate").and_then(Value::as_str) == Some("tool_call")
        || ["kind", "title", "rawInput", "locations", "_meta"]
            .iter()
            .any(|key| update.get(*key).is_some_and(|v| !v.is_null()))
        || tool_diff(update).is_some();

    // Use the current frame unmodified for fresh arguments; only fill omissions
    // from the bounded cache. In particular, a completion title is not a command.
    let mut merged = update.clone();
    for key in SHAPE_FIELDS {
        if let Some(value) = update.get(key).filter(|value| !value.is_null()) {
            let bounded = match key {
                "rawInput" => Some(bounded_input(value)),
                "kind" | "title" => value
                    .as_str()
                    .map(|text| Value::String(cap_text(text, SHAPE_TEXT_CAP))),
                _ => (value.to_string().len() <= SHAPE_FIELD_CAP).then(|| value.clone()),
            };
            if let Some(value) = bounded {
                snapshot.shape.insert(key.into(), value);
            } else {
                snapshot.shape.remove(key);
            }
        } else if let Some(value) = snapshot.shape.get(key) {
            merged[key] = value.clone();
        }
    }
    let content_changed = update.get("content").is_some_and(Value::is_array);
    let was_document = snapshot.document;
    if !matches!(
        merged
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("other"),
        "other" | "think"
    ) {
        snapshot.document = false;
    } else if content_changed {
        snapshot.document = update["content"]
            .as_array()
            .expect("checked above")
            .iter()
            .any(|part| {
                part.get("type").and_then(Value::as_str) == Some("content")
                    && part
                        .get("content")
                        .and_then(markdown_resource_text)
                        .is_some()
            });
    }
    let had_output = snapshot.output.is_some();
    if content_changed {
        snapshot.output = tool_output(&merged, snapshot.document);
        snapshot.diff = tool_diff(update);
    } else if let Some(diff) = &snapshot.diff {
        // A sparse shape update must not lose the file identity of an earlier
        // diff. This projected content is never treated as a new output frame.
        merged["content"] = serde_json::json!([{
            "type": "diff", "path": diff.path, "oldText": diff.old_text, "newText": diff.new_text,
        }]);
    }
    let mut events = Vec::new();
    if has_shape || snapshot.document != was_document {
        let call = if snapshot.document {
            let title = str_field(&merged, "title");
            ToolCall::Document {
                title: if title.is_empty() {
                    "Document".into()
                } else {
                    title
                },
            }
        } else {
            typed_call(&merged)
        };
        events.push(AgentEvent::ToolCall {
            id: id.clone(),
            call,
        });
    }
    match update.get("status").and_then(Value::as_str) {
        Some(status @ ("completed" | "failed")) => events.push(AgentEvent::ToolResult {
            id,
            is_error: status == "failed",
            output: snapshot.output.clone(),
            diff: snapshot.diff.clone(),
        }),
        _ if content_changed && (had_output || snapshot.output.is_some()) => {
            events.push(AgentEvent::ToolProgress {
                id,
                output: snapshot.output.clone(),
            })
        }
        _ => {}
    }
    events
}

/// Map one `session/update` payload's `update` object to events.
/// Message/thought chunks are handled here too (unlike codex, ACP has no
/// separate delta channel).
pub(crate) fn map_update(update: &Value) -> Vec<AgentEvent> {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "agent_message_chunk" => chunk_text(update)
            .map(|text| vec![AgentEvent::TextDelta { text }])
            .unwrap_or_default(),
        "agent_thought_chunk" => chunk_text(update)
            .map(|text| vec![AgentEvent::ReasoningDelta { text }])
            .unwrap_or_default(),
        // Replayed history on session/load is filtered by the session loop
        // before this map; a live user chunk is our own prompt echoed back.
        "user_message_chunk" => Vec::new(),
        "tool_call" | "tool_call_update" => map_tool_update(update, &mut ToolSnapshot::default()),
        "plan" => {
            let items = update
                .get("entries")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|e| TodoItem {
                    text: str_field(e, "content"),
                    done: e.get("status").and_then(Value::as_str) == Some("completed"),
                })
                .collect();
            // The plan has no wire id; a stable synthetic id makes every
            // update refresh the same chip (fold refreshes in place by id).
            vec![
                AgentEvent::ToolCall {
                    id: kratos_proto::LIVE_PLAN_TOOL_ID.into(),
                    call: ToolCall::Todo { items },
                },
                AgentEvent::ToolResult {
                    id: kratos_proto::LIVE_PLAN_TOOL_ID.into(),
                    is_error: false,
                    output: None,
                    diff: None,
                },
            ]
        }
        "available_commands_update" => {
            let commands = parse_commands(update.get("availableCommands"));
            vec![AgentEvent::AvailableCommands { commands }]
        }
        "usage_update" => {
            let tokens = update.get("used").and_then(Value::as_u64);
            let window = ["max", "limit", "size", "contextWindow", "context_window"]
                .iter()
                .find_map(|key| update.get(*key).and_then(Value::as_u64))
                .filter(|n| *n > 0);
            if tokens.is_some() || window.is_some() {
                vec![AgentEvent::ContextUsage { tokens, window }]
            } else {
                Vec::new()
            }
        }
        "current_mode_update" | "config_option_update" | "session_info_update" => Vec::new(),
        _ => Vec::new(),
    }
}

/// Decode an `availableCommands` array (`{name, description, input: {hint}}`).
pub(crate) fn parse_commands(value: Option<&Value>) -> Vec<SlashCommand> {
    value
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let name = str_field(c, "name");
            (!name.is_empty()).then(|| SlashCommand {
                name,
                description: str_field(c, "description"),
                input_hint: c
                    .get("input")
                    .and_then(|i| i.get("hint"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

/// `session/request_permission` options (`{optionId, name, kind}`) → the
/// preferred auto-approve choice: `allow_always` > `allow_once` > first.
pub(crate) fn preferred_allow_option(options: &[Value]) -> Option<String> {
    let by_kind = |kind: &str| {
        options
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
    };
    by_kind("allow_always")
        .or_else(|| by_kind("allow_once"))
        .or_else(|| options.first())
        .map(|o| str_field(o, "optionId"))
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_and_thought_chunks_map_to_deltas() {
        let update = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hello" },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::TextDelta {
                text: "hello".into()
            }]
        );
        let thought = json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "hmm" },
        });
        assert_eq!(
            map_update(&thought),
            vec![AgentEvent::ReasoningDelta { text: "hmm".into() }]
        );
        // Non-text blocks render as nothing.
        let image = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "image", "data": "...", "mimeType": "image/png" },
        });
        assert_eq!(map_update(&image), Vec::new());
    }

    #[test]
    fn semantic_marker_survives_truncated_input_and_sparse_completion() {
        let markdown = "### Findings\n\n- **Owner:** Mimir";
        let marker = json!({
            "mimir.dev/tool-content": {"version": 1, "format": "markdown", "kind": "report"}
        });
        let mut normalizer = UpdateNormalizer::default();
        let started = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "report-1",
            "kind": "other",
            "title": "Report findings",
            "rawInput": {"padding": "x".repeat(SHAPE_FIELD_CAP * 2)},
            "_meta": marker
        }));
        assert!(matches!(
            started.as_slice(),
            [AgentEvent::ToolCall {
                call: ToolCall::Report { .. },
                ..
            }]
        ));

        let completed = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "report-1",
            "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": markdown}}]
        }));
        assert!(matches!(
            completed.as_slice(),
            [AgentEvent::ToolResult { is_error: false, output: Some(output), .. }] if output == markdown
        ));
    }

    #[test]
    fn semantic_answer_uses_public_markdown_without_input_schema_inference() {
        let answer = "### Result\n\nUse **bold** and `code`.";
        let events = map_update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "answer-1",
            "kind": "think",
            "title": "Answered",
            "status": "completed",
            "rawInput": {"not_the_answer_schema": true},
            "_meta": {"mimir.dev/tool-content": {"version": 1, "format": "markdown", "kind": "answer"}},
            "content": [{"type": "content", "content": {"type": "text", "text": answer}}]
        }));
        assert!(matches!(
            events.as_slice(),
            [
                AgentEvent::ToolCall { call: ToolCall::Answer { .. }, .. },
                AgentEvent::ToolResult { is_error: false, output: Some(output), .. }
            ] if output == answer
        ));
    }

    #[test]
    fn failed_semantic_report_preserves_the_actual_diagnostic() {
        let mut normalizer = UpdateNormalizer::default();
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "report-failed",
            "kind": "other",
            "title": "Report findings",
            "rawInput": {"summary": "Submitted content must not be rendered"},
            "_meta": {"mimir.dev/tool-content": {"version": 1, "format": "markdown", "kind": "report"}}
        }));
        let events = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "report-failed",
            "status": "failed",
            "content": [{"type": "content", "content": {"type": "text", "text": "Invalid subagent report: missing field `payload`"}}]
        }));
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::ToolResult { is_error: true, output: Some(output), .. }]
                if output == "Invalid subagent report: missing field `payload`"
        ));
    }

    #[test]
    fn absent_or_future_semantic_markers_remain_unknown() {
        for marker in [
            Value::Null,
            json!({"mimir.dev/tool-content": {"version": 2, "format": "markdown", "kind": "report"}}),
            json!({"mimir.dev/tool-content": {"version": 1, "format": "markdown", "kind": "future"}}),
            json!({"mimir.dev/tool-content": {"version": 1, "format": "markdown", "kind": "report", "extra": true}}),
        ] {
            let mut update = json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "ordinary",
                "kind": "other",
                "title": "Report findings",
                "rawInput": {"role": "explore", "payload": {"kind": "investigation"}}
            });
            if !marker.is_null() {
                update["_meta"] = marker;
            }
            let events = map_update(&update);
            assert!(matches!(
                events.as_slice(),
                [AgentEvent::ToolCall {
                    call: ToolCall::Unknown { .. },
                    ..
                }]
            ));
        }
    }

    #[test]
    fn large_public_proposal_and_final_diagnostic_survive_normalization() {
        let proposal = format!(
            "# Proposal\n{}\nFINAL DELIVERY STEP",
            "Design details ✓\n".repeat(3000)
        );
        let stdout = "public stdout line\n".repeat(3000);
        let diagnostic = "STDERR: final compiler error E0425 — unknown symbol";
        let expected = format!("Plan outline\n{proposal}\n{stdout}\n{diagnostic}");
        let mut normalizer = UpdateNormalizer::default();
        // Resource attachments on execute tools remain ordinary public output.
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "large-public", "kind": "execute",
            "rawInput": {"command": "cargo check"}, "title": "Run command"
        }));
        let events = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "large-public", "status": "failed",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "Plan outline"}},
                {"type": "content", "content": {"type": "resource", "resource": {
                    "uri": "urn:mimir:proposal:large-public", "mimeType": "text/markdown", "text": proposal
                }}},
                {"type": "content", "content": {"type": "text", "text": stdout}},
                {"type": "content", "content": {"type": "text", "text": diagnostic}}
            ],
            "rawOutput": {"result": "PRIVATE RAW DATA MUST NOT BE USED"}
        }));
        let [
            AgentEvent::ToolResult {
                output: Some(output),
                is_error: true,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("expected failed public result");
        };
        assert!(
            output.ends_with(diagnostic),
            "the final public diagnostic was lost"
        );
        assert!(
            output.contains("FINAL DELIVERY STEP"),
            "proposal body was lost"
        );
        assert_eq!(output, &expected);
    }

    #[test]
    fn markdown_resources_become_documents_and_survive_sparse_completions() {
        let body = format!(
            "# Proposal\n{}\nLast step",
            "Detailed design.\n".repeat(3000)
        );
        for kind in ["other", "think"] {
            let mut normalizer = UpdateNormalizer::default();
            normalizer.map_update(&json!({
                "sessionUpdate": "tool_call", "toolCallId": "proposal", "kind": kind, "title": "Design"
            }));
            assert_eq!(normalizer.map_update(&json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "proposal",
                "content": [
                    {"type": "content", "content": {"type": "text", "text": "Redundant outline"}},
                    {"type": "content", "content": {"type": "resource", "resource": {
                        "uri": "urn:proposal", "mimeType": "text/markdown", "text": body
                    }}}
                ]
            })), vec![
                AgentEvent::ToolCall { id: "proposal".into(), call: ToolCall::Document { title: "Design".into() } },
                AgentEvent::ToolProgress { id: "proposal".into(), output: Some(body.clone()) },
            ]);
            assert_eq!(
                normalizer.map_update(&json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": "proposal",
                    "title": "Final design", "status": "completed"
                })),
                vec![
                    AgentEvent::ToolCall {
                        id: "proposal".into(),
                        call: ToolCall::Document {
                            title: "Final design".into()
                        }
                    },
                    AgentEvent::ToolResult {
                        id: "proposal".into(),
                        is_error: false,
                        output: Some(body.clone()),
                        diff: None
                    },
                ]
            );
        }
    }

    #[test]
    fn execute_and_read_resource_content_is_not_reinterpreted_as_a_document() {
        for (kind, expected_call) in [
            (
                "execute",
                ToolCall::Exec {
                    command: "cat plan.md".into(),
                },
            ),
            (
                "read",
                ToolCall::ReadFile {
                    path: "plan.md".into(),
                },
            ),
        ] {
            let mut normalizer = UpdateNormalizer::default();
            normalizer.map_update(&json!({
                "sessionUpdate": "tool_call", "toolCallId": "t", "kind": kind,
                "rawInput": {"command": "cat plan.md", "path": "plan.md"}, "title": "Plan"
            }));
            assert_eq!(normalizer.map_update(&json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "t", "title": "Finished", "status": "completed",
                "content": [
                    {"type": "content", "content": {"type": "text", "text": "Public stdout"}},
                    {"type": "content", "content": {"type": "resource", "resource": {
                        "uri": "urn:read", "mimeType": "text/markdown", "text": "# File contents"
                    }}}
                ]
            })), vec![
                AgentEvent::ToolCall { id: "t".into(), call: expected_call },
                AgentEvent::ToolResult { id: "t".into(), is_error: false, output: Some("Public stdout\n# File contents".into()), diff: None },
            ]);
        }
        let plain = map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "plain", "kind": "other", "title": "Plan",
            "content": [{"type": "content", "content": {"type": "text", "text": "# Not an embedded document"}}]
        }));
        assert!(matches!(
            plain.first(),
            Some(AgentEvent::ToolCall {
                call: ToolCall::Unknown { .. },
                ..
            })
        ));
    }

    #[test]
    fn public_output_budget_is_distinct_from_shapes_and_utf8_safe_between_blocks() {
        let mut normalizer = UpdateNormalizer::default();
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "t", "kind": "execute",
            "rawInput": {"command": "x".repeat(SHAPE_TEXT_CAP + 10), "unused": "x".repeat(SHAPE_FIELD_CAP)}
        }));
        let events = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t", "title": "Finished", "status": "completed",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "x".repeat(OUTPUT_CAP - 2)}},
                {"type": "content", "content": {"type": "text", "text": "éé"}},
                {"type": "content", "content": {"type": "text", "text": "ignored after bound"}}
            ]
        }));
        let [
            AgentEvent::ToolCall {
                call: ToolCall::Exec { command },
                ..
            },
            AgentEvent::ToolResult {
                output: Some(output),
                ..
            },
        ] = events.as_slice()
        else {
            panic!("expected bounded command and public result");
        };
        assert_eq!(command.len(), SHAPE_TEXT_CAP + "\n… [truncated]".len());
        assert!(output.len() <= OUTPUT_CAP + "\n… [truncated]".len());
        assert!(output.ends_with("\n… [truncated]"));
        assert!(output.len() > 16 * 1024);
        assert!(!output.contains('�'));
        assert!(!output.contains("ignored after bound"));
    }

    #[test]
    fn embedded_proposal_markdown_is_public_tool_output() {
        let events = map_update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "submit-1",
            "status": "completed",
            "content": [
                {"type": "content", "content": {"type": "text", "text": "Plan: safer releases"}},
                {"type": "content", "content": {"type": "resource", "resource": {
                    "uri": "urn:mimir:proposal:submit-1", "mimeType": "text/markdown",
                    "text": "# Safer releases\n\n## Design\nKeep the rollback path."
                }}}
            ],
            "rawOutput": {"details": {"private_trace": "not public output"}}
        }));
        assert_eq!(
            events,
            vec![
                AgentEvent::ToolCall {
                    id: "submit-1".into(),
                    call: ToolCall::Document {
                        title: "Document".into()
                    }
                },
                AgentEvent::ToolResult {
                    id: "submit-1".into(),
                    is_error: false,
                    output: Some("# Safer releases\n\n## Design\nKeep the rollback path.".into()),
                    diff: None,
                }
            ]
        );
    }

    #[test]
    fn completion_title_does_not_replace_original_shell_command() {
        let mut normalizer = UpdateNormalizer::default();
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "shell-1",
            "kind": "execute", "title": "Run tests", "status": "in_progress",
            "rawInput": {"command": "printf 'hello\\n'", "timeout_seconds": 120}
        }));
        let events = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "shell-1",
            "kind": "execute", "title": "Process completed, exit code 0", "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "hello\n"}}],
            "rawOutput": {"result": "raw internal result", "details": {"private_trace": "private"}}
        }));
        assert_eq!(
            events,
            vec![
                AgentEvent::ToolCall {
                    id: "shell-1".into(),
                    call: ToolCall::Exec {
                        command: "printf 'hello\\n'".into()
                    }
                },
                AgentEvent::ToolResult {
                    id: "shell-1".into(),
                    is_error: false,
                    output: Some("hello\n".into()),
                    diff: None
                },
            ]
        );
    }

    #[test]
    fn public_progress_replaces_snapshots_and_only_terminal_status_resolves() {
        let mut normalizer = UpdateNormalizer::default();
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "agent-1",
            "title": "Review the change", "kind": "other", "status": "in_progress"
        }));
        for text in ["Inspecting · 1 turn", "Testing · 2 turns"] {
            assert_eq!(
                normalizer.map_update(&json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": "agent-1",
                    "content": [{"type": "content", "content": {"type": "text", "text": text}}],
                    "rawOutput": {"private_trace": "must not appear"}
                })),
                vec![AgentEvent::ToolProgress {
                    id: "agent-1".into(),
                    output: Some(text.into())
                }]
            );
        }
        assert!(normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "agent-1", "status": "in_progress",
            "rawOutput": {"result": "not public output"}
        })).is_empty());
        assert_eq!(
            normalizer.map_update(&json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "agent-1", "status": "completed"
            })),
            vec![AgentEvent::ToolResult {
                id: "agent-1".into(),
                is_error: false,
                output: Some("Testing · 2 turns".into()),
                diff: None
            }]
        );
    }

    #[test]
    fn empty_content_snapshot_clears_public_progress() {
        let mut normalizer = UpdateNormalizer::default();
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "t1", "kind": "execute",
            "rawInput": {"command": "cargo test"},
            "content": [{"type": "content", "content": {"type": "text", "text": "Building"}}]
        }));
        assert_eq!(
            normalizer.map_update(&json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "t1", "content": []
            })),
            vec![AgentEvent::ToolProgress {
                id: "t1".into(),
                output: None
            }]
        );
        assert_eq!(
            normalizer.map_update(&json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed"
            })),
            vec![AgentEvent::ToolResult {
                id: "t1".into(),
                is_error: true,
                output: None,
                diff: None
            }]
        );
    }

    #[test]
    fn nonterminal_proposal_and_progress_are_bounded_public_text_only() {
        let mut normalizer = UpdateNormalizer::default();
        let events = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "submit-1", "status": "in_progress",
            "content": [
                {"type": "content", "content": {"type": "resource", "resource": {
                    "uri": "urn:mimir:proposal:submit-1", "mimeType": "text/markdown",
                    "text": "é".repeat(OUTPUT_CAP)
                }}},
                {"type": "content", "content": {"type": "resource", "resource": {"blob": "secret blob"}}},
                {"type": "content", "content": {"type": "resource_link", "uri": "file:///private"}}
            ],
            "rawOutput": {"result": "not the preview", "private": "secret"}
        }));
        let [
            AgentEvent::ToolCall {
                call: ToolCall::Document { .. },
                ..
            },
            AgentEvent::ToolProgress {
                output: Some(output),
                ..
            },
        ] = events.as_slice()
        else {
            panic!("expected public progress without completion: {events:?}");
        };
        assert!(output.starts_with("ééé"));
        assert!(output.ends_with("\n… [truncated]"));
        assert_eq!(output.len(), OUTPUT_CAP + "\n… [truncated]".len());
        assert!(!output.contains("secret"));
        assert!(!output.contains("private"));
    }

    #[test]
    fn partial_arguments_locations_and_concurrent_calls_keep_their_identity() {
        let mut normalizer = UpdateNormalizer::default();
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "shell", "kind": "execute", "title": "Terminal"
        }));
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "read", "kind": "read",
            "title": "Read File", "locations": [{"path": "/repo/a.rs"}]
        }));
        assert_eq!(normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "shell", "rawInput": {"command": "pwd"}
        })), vec![AgentEvent::ToolCall { id: "shell".into(), call: ToolCall::Exec { command: "pwd".into() } }]);
        assert_eq!(
            normalizer.map_update(&json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "read", "title": "Read 10 lines"
            })),
            vec![AgentEvent::ToolCall {
                id: "read".into(),
                call: ToolCall::ReadFile {
                    path: "/repo/a.rs".into()
                }
            }]
        );
        let events = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "shell", "title": "Finished", "rawInput": null
        }));
        assert_eq!(
            events,
            vec![AgentEvent::ToolCall {
                id: "shell".into(),
                call: ToolCall::Exec {
                    command: "pwd".into()
                }
            }]
        );
    }

    #[test]
    fn distinct_message_ids_separate_committed_messages_without_ending_the_turn() {
        let mut normalizer = UpdateNormalizer::default();
        let message = |id: &str, text: &str| {
            json!({
                "sessionUpdate": "agent_message_chunk", "messageId": id,
                "content": {"type": "text", "text": text}
            })
        };
        assert!(normalizer.map_update(&message("empty", "")).is_empty());
        assert_eq!(
            normalizer.map_update(&message("entry-1", "First ")),
            vec![AgentEvent::TextDelta {
                text: "First ".into()
            }]
        );
        assert_eq!(
            normalizer.map_update(&message("entry-1", "message.")),
            vec![AgentEvent::TextDelta {
                text: "message.".into()
            }]
        );
        assert_eq!(
            normalizer.map_update(&message("entry-2", "Second message.")),
            vec![
                AgentEvent::MessageBoundary,
                AgentEvent::TextDelta {
                    text: "Second message.".into()
                }
            ]
        );
        assert_eq!(
            normalizer.map_update(&message("", " Legacy delta")),
            vec![AgentEvent::TextDelta {
                text: " Legacy delta".into()
            }]
        );
    }

    #[test]
    fn embedded_plain_text_is_read_but_blobs_and_links_are_not() {
        let events = map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "read-1", "status": "completed",
            "content": [
                {"type": "content", "content": {"type": "resource", "resource": {
                    "uri": "file:///repo/notes.txt", "mimeType": "text/plain", "text": "Public notes"
                }}},
                {"type": "content", "content": {"type": "resource", "resource": {
                    "uri": "file:///repo/image.png", "mimeType": "image/png", "blob": "binary data"
                }}},
                {"type": "content", "content": {"type": "resource_link", "uri": "file:///private"}}
            ],
            "rawOutput": {"details": "not public output"}
        }));
        assert_eq!(
            events,
            vec![AgentEvent::ToolResult {
                id: "read-1".into(),
                is_error: false,
                output: Some("Public notes".into()),
                diff: None
            }]
        );
    }

    #[test]
    fn diff_snapshot_survives_omission_but_not_explicit_replacement() {
        let mut normalizer = UpdateNormalizer::default();
        for (id, clear) in [("keep", false), ("clear", true)] {
            normalizer.map_update(&json!({
                "sessionUpdate": "tool_call", "toolCallId": id, "kind": "edit",
                "rawInput": {"path": "/repo/a.rs"},
                "content": [{"type": "diff", "path": "/repo/a.rs", "oldText": "old", "newText": "new"}]
            }));
            if clear {
                normalizer.map_update(&json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": id, "content": []
                }));
            }
            assert_eq!(
                normalizer.map_update(&json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"
                })),
                vec![AgentEvent::ToolResult {
                    id: id.into(),
                    is_error: false,
                    output: None,
                    diff: (!clear).then(|| ToolDiff {
                        path: "/repo/a.rs".into(),
                        old_text: Some("old".into()),
                        new_text: "new".into()
                    })
                }]
            );
        }
    }

    #[test]
    fn large_input_keeps_bounded_typed_shape_across_completed_turns() {
        let mut normalizer = UpdateNormalizer::default();
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "shell-1", "kind": "execute",
            "rawInput": {"command": "pwd", "unrelated": "x".repeat(SHAPE_FIELD_CAP)}
        }));
        let events = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "shell-1", "title": "Finished", "status": "completed"
        }));
        assert_eq!(
            events.first(),
            Some(&AgentEvent::ToolCall {
                id: "shell-1".into(),
                call: ToolCall::Exec {
                    command: "pwd".into()
                }
            })
        );
        // Canonical tool IDs can receive more updates on a later turn. The
        // normalizer lives with the session, not with any one prompt response.
        normalizer.map_update(&json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "next-turn",
            "content": {"type": "text", "text": "Continuing the work."}
        }));
        assert_eq!(normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "shell-1", "title": "Running again", "status": "in_progress"
        })), vec![AgentEvent::ToolCall { id: "shell-1".into(), call: ToolCall::Exec { command: "pwd".into() } }]);
    }

    #[test]
    fn canonical_subagent_progress_can_resume_on_later_turns() {
        let mut normalizer = UpdateNormalizer::default();
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "agent:review", "kind": "other",
            "title": "Review changes", "rawInput": {"description": "Audit changes", "prompt": "Review", "subagent_type": "review"}
        }));
        normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "agent:review", "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "Reviewed first pass"}}]
        }));
        normalizer.map_update(&json!({
            "sessionUpdate": "agent_message_chunk", "messageId": "next-turn",
            "content": {"type": "text", "text": "Review the follow-up too."}
        }));
        let events = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "agent:review", "title": "Reviewing again", "status": "in_progress",
            "content": [{"type": "content", "content": {"type": "text", "text": "Reviewing follow-up"}}]
        }));
        assert!(matches!(events.as_slice(), [
            AgentEvent::ToolCall { call: ToolCall::Unknown { name, .. }, .. },
            AgentEvent::ToolProgress { id, output: Some(output) },
        ] if name == "Agent: Audit changes" && id == "agent:review" && output == "Reviewing follow-up"));
    }

    #[test]
    fn recent_tool_cache_is_bounded_without_losing_recent_shapes() {
        let mut normalizer = UpdateNormalizer::default();
        for index in 0..=MAX_TRACKED_TOOLS {
            normalizer.map_update(&json!({
                "sessionUpdate": "tool_call", "toolCallId": index.to_string(), "kind": "execute",
                "rawInput": {"command": "pwd"}, "status": "completed"
            }));
        }
        let recent = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": MAX_TRACKED_TOOLS.to_string(), "title": "Finished"
        }));
        assert!(
            matches!(recent.as_slice(), [AgentEvent::ToolCall { call: ToolCall::Exec { command }, .. }] if command == "pwd")
        );
        let oldest = normalizer.map_update(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "0", "title": "Finished"
        }));
        assert!(
            matches!(oldest.as_slice(), [AgentEvent::ToolCall { call: ToolCall::Unknown { name, input: None }, .. }] if name == "Finished")
        );
    }

    #[test]
    fn execute_tool_call_with_terminal_status_resolves_inline() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "ls -la",
            "kind": "execute",
            "status": "completed",
            "rawInput": { "command": "ls -la" },
            "content": [
                { "type": "content", "content": { "type": "text", "text": "total 0" } },
            ],
        });
        assert_eq!(
            map_update(&update),
            vec![
                AgentEvent::ToolCall {
                    id: "t1".into(),
                    call: ToolCall::Exec {
                        command: "ls -la".into()
                    },
                },
                AgentEvent::ToolResult {
                    id: "t1".into(),
                    is_error: false,
                    output: Some("total 0".into()),
                    diff: None,
                },
            ]
        );
    }

    #[test]
    fn edit_with_diff_content_maps_to_edit_file_and_carries_diff() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t2",
            "kind": "edit",
            "status": "completed",
            "content": [{
                "type": "diff",
                "path": "/w/src/main.rs",
                "oldText": "fn old() {}",
                "newText": "fn new() {}",
            }],
        });
        let events = map_update(&update);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            AgentEvent::ToolCall {
                id: "t2".into(),
                call: ToolCall::EditFile {
                    path: "/w/src/main.rs".into(),
                    old_string: None,
                    new_string: None,
                },
            }
        );
        assert_eq!(
            events[1],
            AgentEvent::ToolResult {
                id: "t2".into(),
                is_error: false,
                output: None,
                diff: Some(ToolDiff {
                    path: "/w/src/main.rs".into(),
                    old_text: Some("fn old() {}".into()),
                    new_text: "fn new() {}".into(),
                }),
            }
        );
    }

    #[test]
    fn new_file_diff_maps_to_write_file() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t3",
            "kind": "edit",
            "content": [{ "type": "diff", "path": "/w/new.rs", "newText": "x" }],
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t3".into(),
                call: ToolCall::WriteFile {
                    path: "/w/new.rs".into(),
                    content: None
                },
            }]
        );
    }

    #[test]
    fn status_only_update_resolves_without_refreshing_call() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t4",
            "status": "failed",
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolResult {
                id: "t4".into(),
                is_error: true,
                output: None,
                diff: None,
            }]
        );
    }

    #[test]
    fn plan_maps_to_stable_todo_chip() {
        let update = json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "read code", "priority": "high", "status": "completed" },
                { "content": "write fix", "priority": "high", "status": "in_progress" },
            ],
        });
        let events = map_update(&update);
        assert_eq!(
            events[0],
            AgentEvent::ToolCall {
                id: kratos_proto::LIVE_PLAN_TOOL_ID.into(),
                call: ToolCall::Todo {
                    items: vec![
                        TodoItem {
                            text: "read code".into(),
                            done: true
                        },
                        TodoItem {
                            text: "write fix".into(),
                            done: false
                        },
                    ]
                },
            }
        );
    }

    #[test]
    fn available_commands_parse_with_hint() {
        let update = json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": "compact", "description": "Compact the session" },
                { "name": "goal", "description": "Set a goal", "input": { "hint": "the goal" } },
                { "description": "nameless is dropped" },
            ],
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::AvailableCommands {
                commands: vec![
                    SlashCommand {
                        name: "compact".into(),
                        description: "Compact the session".into(),
                        input_hint: None,
                    },
                    SlashCommand {
                        name: "goal".into(),
                        description: "Set a goal".into(),
                        input_hint: Some("the goal".into()),
                    },
                ]
            }]
        );
    }

    #[test]
    fn output_and_diff_caps_apply() {
        let big = "x".repeat(OUTPUT_CAP + 100);
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t5",
            "status": "completed",
            "content": [
                { "type": "content", "content": { "type": "text", "text": big } },
            ],
        });
        let events = map_update(&update);
        // The content-bearing update refreshes the call, then resolves it.
        let Some(AgentEvent::ToolResult {
            output: Some(output),
            ..
        }) = events.last()
        else {
            panic!("expected resolved result, got {events:?}");
        };
        assert_eq!(output.len(), OUTPUT_CAP + "\n… [truncated]".len());
        assert!(output.ends_with("… [truncated]"));
    }

    #[test]
    fn permission_options_prefer_allow_always() {
        let options = vec![
            json!({ "optionId": "once", "name": "Allow once", "kind": "allow_once" }),
            json!({ "optionId": "always", "name": "Always", "kind": "allow_always" }),
            json!({ "optionId": "no", "name": "Reject", "kind": "reject_once" }),
        ];
        assert_eq!(preferred_allow_option(&options), Some("always".into()));
        let only_reject = vec![json!({ "optionId": "no", "kind": "reject_once" })];
        assert_eq!(preferred_allow_option(&only_reject), Some("no".into()));
        assert_eq!(preferred_allow_option(&[]), None);
    }

    /// Cursor ACP opens tool cards with a display `title` before `rawInput`
    /// is filled (Search "grep", Read "Read File", Web "Web Fetch"). Those
    /// labels must not become typed args.
    #[test]
    fn cursor_placeholder_titles_are_not_typed_args() {
        let grep = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "grep",
            "kind": "search",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&grep),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Search {
                    pattern: String::new(),
                    path: None,
                },
            }]
        );

        let read = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t2",
            "title": "Read File",
            "kind": "read",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&read),
            vec![AgentEvent::ToolCall {
                id: "t2".into(),
                call: ToolCall::ReadFile {
                    path: String::new(),
                },
            }]
        );

        let fetch = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t3",
            "title": "Web Fetch",
            "kind": "fetch",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&fetch),
            vec![AgentEvent::ToolCall {
                id: "t3".into(),
                call: ToolCall::WebSearch {
                    query: String::new(),
                },
            }]
        );

        let web = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t4",
            "title": "Web Search",
            "kind": "search",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&web),
            vec![AgentEvent::ToolCall {
                id: "t4".into(),
                call: ToolCall::Search {
                    pattern: String::new(),
                    path: None,
                },
            }]
        );
    }

    #[test]
    fn cursor_search_term_maps_to_web_search() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Web Search: \"multitask cli\"",
            "kind": "search",
            "rawInput": { "searchTerm": "multitask cli" },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::WebSearch {
                    query: "multitask cli".into(),
                },
            }]
        );
    }

    #[test]
    fn cursor_exec_title_unwraps_markdown_backtick_escapes() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "`echo \\`hi\\``",
            "kind": "execute",
            "rawInput": {},
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Exec {
                    command: "echo `hi`".into(),
                },
            }]
        );
    }

    #[test]
    fn cursor_task_uses_description_not_placeholder_title() {
        let update = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Task: Subagent task",
            "kind": "other",
            "rawInput": {
                "_toolName": "task",
                "description": "Look up multitask docs",
                "prompt": "find the reminder",
            },
        });
        assert_eq!(
            map_update(&update),
            vec![AgentEvent::ToolCall {
                id: "t1".into(),
                call: ToolCall::Unknown {
                    name: "Task: Look up multitask docs".into(),
                    input: Some(json!({
                        "_toolName": "task",
                        "description": "Look up multitask docs",
                        "prompt": "find the reminder",
                    })),
                },
            }]
        );
    }

    #[test]
    fn opencode_task_keeps_agent_naming_across_frames() {
        // The rawInput frame (in_progress) names the chip off the spawn args.
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "in_progress",
            "kind": "think",
            "title": "Viz probe",
            "rawInput": {
                "description": "Viz probe",
                "prompt": "run the probe",
                "subagent_type": "general",
            },
        });
        assert!(matches!(
            map_update(&update).as_slice(),
            [AgentEvent::ToolCall { call: ToolCall::Unknown { name, .. }, .. }]
                if name == "Agent: Viz probe"
        ));
        // The completion drops rawInput (title = the bare description); the
        // rawOutput metadata still marks the spawn — naming survives.
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "title": "Viz probe",
            "content": [{"type": "content", "content": {"type": "text",
                "text": "<task id=\"ses_c\" state=\"completed\">\n<task_result>\nfinished\n</task_result>\n</task>"}}],
            "rawOutput": {
                "output": "<task id=\"ses_c\" state=\"completed\">\n<task_result>\nfinished\n</task_result>\n</task>",
                "metadata": {"parentSessionId": "ses_p", "sessionId": "ses_c"},
            },
        });
        assert!(matches!(
            map_update(&update).as_slice(),
            [
                AgentEvent::ToolCall { call: ToolCall::Unknown { name, .. }, .. },
                AgentEvent::ToolResult { is_error: false, .. },
            ] if name == "Agent: Viz probe"
        ));
    }
}
