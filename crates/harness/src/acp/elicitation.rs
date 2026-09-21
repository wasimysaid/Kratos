//! Standard ACP form elicitation through the existing question UI.
//!
//! Deliberately supports flat text and labelled choice fields, not arbitrary JSON
//! Schema. Unknown constraints are rejected rather than silently ignored. Choice
//! labels include an index so duplicate titles still map to distinct opaque IDs.
//! Optional fields offer an explicit skip choice; no default answer is invented.
//! Mimir's optional `<key>_note` companions merge into their choice question so
//! the UI collects the note on the same page instead of a follow-up note page.
//!
//! The session loop owns every waiter and polls `next_response`. No task is
//! spawned: exact cancellation, shutdown and Drop release the input receiver,
//! allowing the engine's input bridge to retire its corresponding UI question.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use kratos_proto::{UserInputAnswer, UserInputQuestion};

use crate::jsonrpc::RpcClient;

use super::RequestInputFn;

const SKIP: &str = "Skip (optional)";

#[derive(Default)]
pub(super) struct Elicitations {
    pending: Vec<Pending>,
}

struct Pending {
    id: Value,
    form: Form,
    answers: oneshot::Receiver<Vec<UserInputAnswer>>,
}

/// A completed request, removed from the pending set before it can be sent.
/// The caller should send it immediately in the same session-loop select arm.
pub(super) struct Response {
    id: Value,
    result: Result<Value, String>,
}

impl Response {
    pub(super) fn send(self, client: &RpcClient) {
        match self.result {
            Ok(result) => client.respond(&self.id, result),
            Err(message) => client.respond_error(&self.id, -32602, &message),
        }
    }

    fn cancelled(id: Value) -> Self {
        Self {
            id,
            result: Ok(json!({ "action": "cancel" })),
        }
    }
}

impl Elicitations {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Route only `elicitation/create` here. Unsupported/malformed forms are
    /// answered immediately and never reach the human input bridge.
    pub(super) fn handle_request(
        &mut self,
        client: &RpcClient,
        session_id: &str,
        id: Value,
        params: &Value,
        request_input: &Arc<RequestInputFn>,
    ) {
        if let Err(message) = self.start(session_id, id.clone(), params, request_input) {
            Response {
                id,
                result: Err(message),
            }
            .send(client);
        }
    }

    fn start(
        &mut self,
        session_id: &str,
        id: Value,
        params: &Value,
        request_input: &Arc<RequestInputFn>,
    ) -> Result<(), String> {
        if !(id.is_string() || id.is_i64() || id.is_u64()) {
            return Err("Elicitation request ID must be a string or integer".into());
        }
        if self.pending.iter().any(|pending| pending.id == id) {
            return Err("Elicitation request ID is already pending".into());
        }
        let form = Form::parse(session_id, params)?;
        let answers = request_input(form.questions());
        self.pending.push(Pending { id, form, answers });
        Ok(())
    }

    /// Route `$/cancel_request`'s `params.requestId` here, preserving its JSON
    /// type. An unknown/late ID does not cancel another form or create a reply.
    pub(super) fn cancel(&mut self, client: &RpcClient, request_id: &Value) {
        if let Some(response) = self.cancel_response(request_id) {
            response.send(client);
        }
    }

    fn cancel_response(&mut self, request_id: &Value) -> Option<Response> {
        let index = self
            .pending
            .iter()
            .position(|pending| &pending.id == request_id)?;
        let pending = self.pending.remove(index);
        // Drop even an already-ready answer: cancellation was observed first.
        Some(Response::cancelled(pending.id))
    }

    /// Call on local interrupt, session close, EOF and normal session-loop exit.
    pub(super) fn cancel_all(&mut self, client: &RpcClient) {
        for pending in self.pending.drain(..) {
            Response::cancelled(pending.id).send(client);
        }
    }

    /// Safe to place directly in `tokio::select!` without an emptiness guard.
    /// Dropping this future leaves all unfinished waiters owned by the session.
    pub(super) async fn next_response(&mut self) -> Response {
        poll_fn(|cx| {
            for index in 0..self.pending.len() {
                if let Poll::Ready(answers) = Pin::new(&mut self.pending[index].answers).poll(cx) {
                    let pending = self.pending.remove(index);
                    let result = match answers {
                        Ok(answers) => pending.form.answer(&answers),
                        Err(_) => Ok(json!({ "action": "cancel" })),
                    };
                    return Poll::Ready(Response {
                        id: pending.id,
                        result,
                    });
                }
            }
            Poll::Pending
        })
        .await
    }
}

struct Form {
    fields: Vec<Field>,
}

struct Field {
    key: String,
    /// Mimir ask_user note companion merged into this question
    /// (`<key>_note`); `None` on questions asked without one.
    note_key: Option<String>,
    /// Character budget carried over from the merged note companion.
    note_max: usize,
    question: UserInputQuestion,
    optional: bool,
    kind: FieldKind,
}

enum FieldKind {
    Text {
        min: usize,
        max: usize,
    },
    Choices {
        choices: Vec<Choice>,
        multiple: bool,
        min: usize,
        max: usize,
    },
}

struct Choice {
    label: String,
    value: String,
    exclusive: bool,
}

impl Form {
    fn parse(session_id: &str, params: &Value) -> Result<Self, String> {
        if params.get("mode").and_then(Value::as_str) != Some("form") {
            return Err("Only form elicitation is supported".into());
        }
        if params.get("sessionId").and_then(Value::as_str) != Some(session_id) {
            return Err("Elicitation does not belong to the active session".into());
        }
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .ok_or("Elicitation message must be text")?;
        let schema = object(
            params
                .get("requestedSchema")
                .ok_or("Missing elicitation schema")?,
        )?;
        keys(
            schema,
            &[
                "type",
                "properties",
                "required",
                "title",
                "description",
                "additionalProperties",
            ],
        )?;
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err("Elicitation schema must be a flat object".into());
        }
        if schema
            .get("additionalProperties")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err("Nested additional-property schemas are unsupported".into());
        }
        let properties = object(schema.get("properties").ok_or("Missing form properties")?)?;
        if properties.is_empty() {
            return Err("Empty elicitation forms are unsupported".into());
        }
        let mut required = Vec::new();
        if let Some(value) = schema.get("required") {
            for key in value.as_array().ok_or("Form required must be an array")? {
                let key = key.as_str().ok_or("Required field names must be strings")?;
                if !properties.contains_key(key) || required.contains(&key) {
                    return Err("Required fields must be unique declared properties".into());
                }
                required.push(key);
            }
        }
        let identity = uuid::Uuid::new_v4();
        let mut fields = Vec::new();
        for (index, (key, schema)) in properties.iter().enumerate() {
            let schema = object(schema)?;
            let title = text(schema, "title")?.unwrap_or(key);
            let description = text(schema, "description")?;
            let optional = !required.contains(&key.as_str());
            let kind = FieldKind::parse(schema)?;
            let mut options = match &kind {
                FieldKind::Text { .. } => Vec::new(),
                FieldKind::Choices { choices, .. } => {
                    choices.iter().map(|c| c.label.clone()).collect()
                }
            };
            if optional {
                options.push(SKIP.into());
            }
            let mut question = String::new();
            if index == 0 && !message.is_empty() {
                question.push_str(message);
                question.push_str("\n\n");
            }
            question.push_str(title);
            if let Some(description) = description.filter(|text| !text.is_empty()) {
                question.push_str("\n");
                question.push_str(description);
            }
            if optional {
                question.push_str("\nOptional: choose Skip (optional) to leave this field out.");
            }
            fields.push(Field {
                key: key.clone(),
                note_key: None,
                note_max: usize::MAX,
                question: UserInputQuestion {
                    id: format!("acp-form-{identity}-{index}"),
                    header: "Agent question".into(),
                    question,
                    options,
                    multi_select: matches!(&kind, FieldKind::Choices { multiple: true, .. }),
                    note: false,
                },
                optional,
                kind,
            });
        }
        Ok(Self {
            fields: merge_note_companions(fields),
        })
    }

    fn questions(&self) -> Vec<UserInputQuestion> {
        self.fields
            .iter()
            .map(|field| field.question.clone())
            .collect()
    }

    fn answer(&self, answers: &[UserInputAnswer]) -> Result<Value, String> {
        // An explicit empty response declines; losing the answer channel cancels.
        if answers.is_empty() {
            return Ok(json!({ "action": "decline" }));
        }
        let mut seen = std::collections::HashSet::new();
        for answer in answers {
            if !seen.insert(&answer.question_id)
                || !self
                    .fields
                    .iter()
                    .any(|field| field.question.id == answer.question_id)
            {
                return Err("Elicitation answer contains an unknown or repeated question".into());
            }
        }
        let mut content = Map::new();
        for field in &self.fields {
            let answer = answers
                .iter()
                .find(|answer| answer.question_id == field.question.id);
            if answer.is_some_and(|answer| answer.note.is_some()) && field.note_key.is_none() {
                return Err("This question does not accept a note".into());
            }
            let labels = answer
                .map(|answer| answer.labels.as_slice())
                .unwrap_or_default();
            if field.optional && (labels.is_empty() || labels == [SKIP]) {
                continue;
            }
            content.insert(field.key.clone(), field.kind.answer(labels)?);
            let note = answer
                .and_then(|answer| answer.note.as_deref())
                .map(str::trim)
                .filter(|note| !note.is_empty());
            if let Some(note) = note {
                if note.chars().count() > field.note_max {
                    return Err("Note answer exceeds the requested length".into());
                }
                content.insert(
                    field
                        .note_key
                        .clone()
                        .expect("note answers only land on merged companions"),
                    json!(note),
                );
            }
        }
        Ok(json!({ "action": "accept", "content": content }))
    }
}

/// Mimir's ask_user schema appends an optional free-text companion property
/// (`<key>_note`) after every choice field, which used to cost a whole extra
/// "Optional note" page per question. Merge each companion into its choice
/// question (`question.note`) so the UI collects it in the composer input
/// already on screen — one page per question, the note still delivered under
/// the original property key. Only optional plain-text companions with a
/// usable bound merge; anything else stays its own question.
fn merge_note_companions(mut fields: Vec<Field>) -> Vec<Field> {
    const NOTE_SUFFIX: &str = "_note";
    let mut consumed = vec![false; fields.len()];
    for ix in 0..fields.len() {
        let (candidate_key, note_max) = match &fields[ix] {
            Field {
                key,
                optional: true,
                kind: FieldKind::Text { min: 0, max, .. },
                ..
            } if key.ends_with(NOTE_SUFFIX) => (key.clone(), *max),
            _ => continue,
        };
        let Some(parent_key) = candidate_key.strip_suffix(NOTE_SUFFIX) else {
            continue;
        };
        let Some(parent) = fields.iter_mut().find(|field| {
            field.key == parent_key && matches!(field.kind, FieldKind::Choices { .. })
        }) else {
            continue;
        };
        parent.question.note = true;
        parent.note_key = Some(candidate_key);
        parent.note_max = note_max;
        consumed[ix] = true;
    }
    let mut merged = Vec::with_capacity(fields.len());
    for (ix, field) in fields.into_iter().enumerate() {
        if !consumed[ix] {
            merged.push(field);
        }
    }
    merged
}

impl FieldKind {
    fn parse(schema: &Map<String, Value>) -> Result<Self, String> {
        match schema.get("type").and_then(Value::as_str) {
            Some("string") if schema.contains_key("oneOf") => {
                keys(schema, &["type", "title", "description", "oneOf"])?;
                Ok(Self::Choices {
                    choices: choices(&schema["oneOf"])?,
                    multiple: false,
                    min: 1,
                    max: 1,
                })
            }
            Some("string") => {
                keys(
                    schema,
                    &["type", "title", "description", "minLength", "maxLength"],
                )?;
                let min = bound(schema, "minLength", 0)?;
                let max = bound(schema, "maxLength", usize::MAX)?;
                if min > max || max == 0 {
                    return Err("Unsupported text length bounds".into());
                }
                Ok(Self::Text { min, max })
            }
            Some("array") => {
                keys(
                    schema,
                    &[
                        "type",
                        "title",
                        "description",
                        "items",
                        "minItems",
                        "maxItems",
                        "uniqueItems",
                    ],
                )?;
                if schema
                    .get("uniqueItems")
                    .is_some_and(|value| !value.is_boolean())
                {
                    return Err("uniqueItems must be a boolean".into());
                }
                let items = object(schema.get("items").ok_or("Missing choice items")?)?;
                keys(items, &["anyOf"])?;
                let choices = choices(
                    items
                        .get("anyOf")
                        .ok_or("Only labelled choice arrays are supported")?,
                )?;
                let min = bound(schema, "minItems", 0)?;
                let max = bound(schema, "maxItems", choices.len())?;
                if min == 0 || min > max || min > choices.len() {
                    return Err(
                        "Only nonempty choice arrays with satisfiable bounds are supported".into(),
                    );
                }
                Ok(Self::Choices {
                    choices,
                    multiple: true,
                    min,
                    max,
                })
            }
            _ => Err("Only text, oneOf choices and anyOf choice arrays are supported".into()),
        }
    }

    fn answer(&self, labels: &[String]) -> Result<Value, String> {
        match self {
            Self::Text { min, max } => {
                if labels.len() != 1 {
                    return Err("A text field needs exactly one answer".into());
                }
                let text = &labels[0];
                let length = text.chars().count();
                if length < *min || length > *max || (*min > 0 && text.trim().is_empty()) {
                    return Err("Text answer does not satisfy the requested length".into());
                }
                Ok(Value::String(text.clone()))
            }
            Self::Choices {
                choices,
                multiple,
                min,
                max,
            } => {
                if labels.len() < *min || labels.len() > *max {
                    return Err("Wrong number of choices for this field".into());
                }
                let mut selected = Vec::new();
                for label in labels {
                    let choice = choices
                        .iter()
                        .find(|choice| &choice.label == label)
                        .ok_or("Answer must be one of the displayed choices")?;
                    if selected.contains(&choice.value) || (choice.exclusive && labels.len() != 1) {
                        return Err(
                            "Choices must be distinct; None of the above must stand alone".into(),
                        );
                    }
                    selected.push(choice.value.clone());
                }
                Ok(if *multiple {
                    json!(selected)
                } else {
                    json!(selected[0])
                })
            }
        }
    }
}

fn choices(value: &Value) -> Result<Vec<Choice>, String> {
    let options = value.as_array().ok_or("Choices must be an array")?;
    if options.is_empty() {
        return Err("Choice fields must have at least one option".into());
    }
    let mut choices: Vec<Choice> = Vec::new();
    for (index, option) in options.iter().enumerate() {
        let option = object(option)?;
        keys(option, &["const", "title", "description"])?;
        let value = text(option, "const")?.ok_or("Choice IDs must be strings")?;
        let title = text(option, "title")?.ok_or("Choices need a display title")?;
        let description = text(option, "description")?.filter(|text| !text.is_empty());
        if choices.iter().any(|choice| choice.value == value) {
            return Err("Choice IDs must be distinct".into());
        }
        let label = match description {
            Some(description) => format!("{}. {title} — {description}", index + 1),
            None => format!("{}. {title}", index + 1),
        };
        choices.push(Choice {
            label,
            value: value.into(),
            // Mimir's none-of-the-above option is exclusive even though the
            // flat standard schema cannot express this cross-item constraint.
            exclusive: value == "none" && title == "None of the above",
        });
    }
    Ok(choices)
}

fn object(value: &Value) -> Result<&Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| "Form schema entries must be objects".into())
}

fn keys(object: &Map<String, Value>, supported: &[&str]) -> Result<(), String> {
    if let Some(key) = object.keys().find(|key| !supported.contains(&key.as_str())) {
        return Err(format!("Unsupported form schema keyword: {key}"));
    }
    Ok(())
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    object
        .get(key)
        .map(|value| value.as_str().ok_or_else(|| format!("{key} must be text")))
        .transpose()
}

fn bound(object: &Map<String, Value>, key: &str, default: usize) -> Result<usize, String> {
    object.get(key).map_or(Ok(default), |value| {
        value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| format!("{key} must be a nonnegative integer"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        json!({
            "mode": "form", "sessionId": "session", "toolCallId": "invocation",
            "message": "Answers are shared with Mimir. Do not enter credentials.",
            "requestedSchema": { "type": "object", "properties": {
                "q0": { "type": "string", "title": "What name?", "minLength": 1, "maxLength": 4096 },
                "q1": { "type": "string", "title": "Which implementation?", "oneOf": [
                    { "const": "o0", "title": "Same", "description": "First" },
                    { "const": "o1", "title": "Same", "description": "Second" },
                    { "const": "none", "title": "None of the above" }
                ] },
                "q1_note": { "type": "string", "title": "Optional note", "maxLength": 4096 },
                "q2": { "type": "array", "title": "Which checks?", "items": { "anyOf": [
                    { "const": "o0", "title": "Unit", "description": "Fast" },
                    { "const": "o1", "title": "Process", "description": "Wire" },
                    { "const": "none", "title": "None of the above" }
                ] }, "minItems": 1, "maxItems": 2, "uniqueItems": true },
                "q2_note": { "type": "string", "title": "Optional note", "maxLength": 4096 }
            }, "required": ["q0", "q1", "q2"] }
        })
    }

    fn field<'a>(form: &'a Form, key: &str) -> &'a Field {
        form.fields.iter().find(|field| field.key == key).unwrap()
    }

    fn answer(form: &Form, key: &str, labels: &[&str]) -> UserInputAnswer {
        UserInputAnswer {
            question_id: field(form, key).question.id.clone(),
            labels: labels.iter().map(|label| (*label).into()).collect(),
            note: None,
        }
    }

    fn answered_note(
        form: &Form,
        key: &str,
        labels: &[&str],
        note: Option<&str>,
    ) -> UserInputAnswer {
        UserInputAnswer {
            note: note.map(str::to_string),
            ..answer(form, key, labels)
        }
    }

    fn valid_answers(form: &Form) -> Vec<UserInputAnswer> {
        vec![
            answer(form, "q0", &["  Casey api_key=example  "]),
            answered_note(
                form,
                "q1",
                &["2. Same — Second"],
                Some("  preserve this note  "),
            ),
            answer(form, "q2", &["1. Unit — Fast", "2. Process — Wire"]),
        ]
    }

    #[test]
    fn real_mimir_schema_round_trips_opaque_ids_text_notes_and_skip() {
        let form = Form::parse("session", &fixture()).unwrap();
        // The merged Mimir shape: one page per question — text, choice +
        // note, multi-choice + note. No separate "Optional note" pages.
        assert_eq!(form.questions().len(), 3);
        assert!(
            form.questions()[0]
                .question
                .contains("Do not enter credentials")
        );
        assert!(field(&form, "q0").question.options.is_empty());
        assert!(!field(&form, "q0").question.note);
        assert!(field(&form, "q1").question.note);
        assert!(field(&form, "q2").question.note);
        assert!(field(&form, "q2").question.multi_select);
        assert_eq!(
            form.answer(&valid_answers(&form)).unwrap(),
            json!({
                "action": "accept", "content": {
                    "q0": "  Casey api_key=example  ", "q1": "o1",
                    "q1_note": "preserve this note", "q2": ["o0", "o1"]
                }
            })
        );
    }

    #[test]
    fn notes_only_land_on_merged_choice_questions() {
        let form = Form::parse("session", &fixture()).unwrap();
        // A note on the plain text question is rejected, not silently dropped.
        let mut invalid = valid_answers(&form);
        invalid[0].note = Some("stray".into());
        assert!(form.answer(&invalid).is_err());
        // A picked choice with a blank note stays just the choice.
        let answers = vec![
            answer(&form, "q0", &["Name"]),
            answered_note(&form, "q1", &["2. Same — Second"], Some("   ")),
            answer(&form, "q2", &["1. Unit — Fast"]),
        ];
        assert_eq!(
            form.answer(&answers).unwrap()["content"],
            json!({"q0":"Name", "q1":"o1", "q2":["o0"]})
        );
    }

    #[test]
    fn note_length_constraints_follow_the_merged_companion() {
        let mut params = fixture();
        params["requestedSchema"]["properties"]["q1_note"]["maxLength"] = json!(4);
        let form = Form::parse("session", &params).unwrap();
        let mut answers = valid_answers(&form);
        assert!(
            form.answer(&answers).is_err(),
            "note over the companion bound"
        );
        answers[1].note = Some("ok".into());
        assert!(form.answer(&answers).is_ok());
    }
    #[test]
    fn missing_optional_notes_are_not_invented_and_none_is_exclusive() {
        let form = Form::parse("session", &fixture()).unwrap();
        let answers = vec![
            answer(&form, "q0", &["Name"]),
            answer(&form, "q1", &["3. None of the above"]),
            answer(&form, "q2", &["3. None of the above"]),
        ];
        assert_eq!(
            form.answer(&answers).unwrap()["content"],
            json!({"q0":"Name", "q1":"none", "q2":["none"]}),
            "no q1_note/q2_note entries invented"
        );
        let mut invalid = answers.clone();
        invalid[2].labels.push("1. Unit — Fast".into());
        assert!(form.answer(&invalid).is_err());
    }

    #[test]
    fn duplicate_titles_even_with_identical_descriptions_remain_distinct() {
        let mut params = fixture();
        params["requestedSchema"]["properties"]["q1"]["oneOf"][1]["description"] = json!("First");
        let form = Form::parse("session", &params).unwrap();
        let mut answers = valid_answers(&form);
        answers[1].labels = vec!["2. Same — First".into()];
        assert_eq!(form.answer(&answers).unwrap()["content"]["q1"], "o1");
        answers[1].labels = vec!["Same".into()];
        assert!(form.answer(&answers).is_err());
    }

    #[test]
    fn malformed_answers_never_become_accepts() {
        let form = Form::parse("session", &fixture()).unwrap();
        for (key, labels) in [
            ("q0", vec![]),
            ("q0", vec![" "]),
            ("q0", vec!["a", "b"]),
            ("q1", vec!["o1"]),
            ("q1", vec!["not offered"]),
            ("q2", vec!["1. Unit — Fast", "1. Unit — Fast"]),
            (
                "q2",
                vec![
                    "1. Unit — Fast",
                    "2. Process — Wire",
                    "3. None of the above",
                ],
            ),
            ("q1", vec!["2. Same — Second", "3. None of the above"]),
        ] {
            let mut answers = valid_answers(&form);
            let id = &field(&form, key).question.id;
            *answers
                .iter_mut()
                .find(|answer| &answer.question_id == id)
                .unwrap() = answer(&form, key, &labels);
            assert!(form.answer(&answers).is_err(), "{key}: {labels:?}");
        }
        let mut answers = valid_answers(&form);
        answers.push(answers[0].clone());
        assert!(form.answer(&answers).is_err());
        answers.pop();
        answers[0].question_id = "stale-question".into();
        assert!(form.answer(&answers).is_err());
        assert!(form.answer(&valid_answers(&form)[1..]).is_err());
    }

    #[test]
    fn length_constraints_count_unicode_characters_and_preserve_whitespace() {
        let mut params = fixture();
        params["requestedSchema"]["properties"]["q0"]["maxLength"] = json!(2);
        let form = Form::parse("session", &params).unwrap();
        let mut answers = valid_answers(&form);
        answers[0].labels = vec!["界é".into()];
        assert!(form.answer(&answers).is_ok());
        answers[0].labels = vec!["界éx".into()];
        assert!(form.answer(&answers).is_err());
    }

    #[test]
    fn unsupported_schemas_and_foreign_sessions_are_rejected() {
        for schema in [
            json!({"type":"boolean"}),
            json!({"type":"number"}),
            json!({"type":"object", "properties":{}}),
            json!({"type":"string", "format":"email"}),
            json!({"type":"string", "pattern":"x"}),
            json!({"type":"string", "default":"automatic"}),
            json!({"type":"string", "minLength":4,"maxLength":2}),
            json!({"type":"string", "maxLength":-1}),
            json!({"type":"string", "oneOf":[{"const":"x","title":"X"},{"const":"x","title":"Y"}]}),
            json!({"type":"array", "items":{"type":"string"}}),
        ] {
            let mut params = fixture();
            params["requestedSchema"]["properties"]["q0"] = schema;
            assert!(Form::parse("session", &params).is_err(), "{params}");
        }
        let mut params = fixture();
        params["mode"] = json!("url");
        assert!(Form::parse("session", &params).is_err());
        assert!(Form::parse("other", &fixture()).is_err());
        params = fixture();
        params["requestedSchema"]["required"] = json!(["missing"]);
        assert!(Form::parse("session", &params).is_err());
        params["requestedSchema"]["required"] = json!(["q0", "q0"]);
        assert!(Form::parse("session", &params).is_err());
        params = fixture();
        params["requestedSchema"]["allOf"] = json!([]);
        assert!(Form::parse("session", &params).is_err());
    }

    type Input = (
        Vec<UserInputQuestion>,
        oneshot::Sender<Vec<UserInputAnswer>>,
    );

    fn input_bridge() -> (
        Arc<RequestInputFn>,
        tokio::sync::mpsc::UnboundedReceiver<Input>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(Box::new(move |questions| {
                let (answers, receiver) = oneshot::channel();
                tx.send((questions, answers)).unwrap();
                receiver
            })),
            rx,
        )
    }

    #[tokio::test]
    async fn cancellation_is_exact_typed_and_closes_waiter_without_spawning_tasks() {
        let (bridge, mut inputs) = input_bridge();
        let mut forms = Elicitations::new();
        forms
            .start("session", json!(7), &fixture(), &bridge)
            .unwrap();
        forms
            .start("session", json!("7"), &fixture(), &bridge)
            .unwrap();
        let (_, old) = inputs.recv().await.unwrap();
        let (_, current) = inputs.recv().await.unwrap();
        assert!(forms.cancel_response(&json!("unknown")).is_none());
        let cancelled = forms.cancel_response(&json!(7)).unwrap();
        assert_eq!(cancelled.result.unwrap(), json!({"action":"cancel"}));
        assert!(old.is_closed());
        assert!(!current.is_closed());
        assert!(old.send(Vec::new()).is_err());
        assert!(forms.cancel_response(&json!(7)).is_none());
        current.send(Vec::new()).unwrap();
        let response = forms.next_response().await;
        assert_eq!(response.id, "7");
        assert_eq!(response.result.unwrap(), json!({"action":"decline"}));
        assert!(forms.pending.is_empty());
    }

    #[tokio::test]
    async fn cancellation_observed_before_ready_reply_wins_and_late_reply_cannot_touch_successor() {
        let (bridge, mut inputs) = input_bridge();
        let mut forms = Elicitations::new();
        forms
            .start("session", json!("old"), &fixture(), &bridge)
            .unwrap();
        let (old_questions, old) = inputs.recv().await.unwrap();
        old.send(Vec::new()).unwrap();
        assert!(forms.cancel_response(&json!("old")).is_some());
        forms
            .start("session", json!("new"), &fixture(), &bridge)
            .unwrap();
        let (new_questions, new) = inputs.recv().await.unwrap();
        assert_ne!(old_questions[0].id, new_questions[0].id);
        assert!(forms.cancel_response(&json!("old")).is_none());
        assert!(!new.is_closed());
        drop(new);
        assert_eq!(
            forms.next_response().await.result.unwrap(),
            json!({"action":"cancel"})
        );
    }

    #[tokio::test]
    async fn dropped_select_future_keeps_waiter_but_dropping_owner_closes_it() {
        let (bridge, mut inputs) = input_bridge();
        let mut forms = Elicitations::new();
        forms
            .start("session", json!(1), &fixture(), &bridge)
            .unwrap();
        let (_, sender) = inputs.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), forms.next_response())
                .await
                .is_err()
        );
        assert!(!sender.is_closed());
        drop(forms);
        assert!(sender.is_closed());
    }

    #[tokio::test]
    async fn rejected_form_and_duplicate_request_never_open_another_ui_question() {
        let (bridge, mut inputs) = input_bridge();
        let mut forms = Elicitations::new();
        assert!(forms.start("other", json!(1), &fixture(), &bridge).is_err());
        assert!(
            forms
                .start("session", json!(null), &fixture(), &bridge)
                .is_err()
        );
        forms
            .start("session", json!(1), &fixture(), &bridge)
            .unwrap();
        assert!(
            forms
                .start("session", json!(1), &fixture(), &bridge)
                .is_err()
        );
        let _first = inputs.recv().await.unwrap();
        assert!(inputs.try_recv().is_err());
        assert_eq!(forms.pending.len(), 1);
    }

    // The test executable itself is the stdio peer, keeping this subprocess
    // evidence portable and independent of Python or a paid/model-backed agent.
    #[test]
    fn rpc_peer() {
        if std::env::var_os("KRATOS_ELICITATION_RPC_PEER").is_none() {
            return;
        }
        use std::io::{BufRead, Write};
        for line in std::io::stdin().lock().lines() {
            let frame: Value = serde_json::from_str(&line.unwrap()).unwrap();
            println!(
                "{}",
                json!({"jsonrpc":"2.0", "method":"observed", "params":frame})
            );
            std::io::stdout().flush().unwrap();
        }
    }

    async fn observed(
        incoming: &mut tokio::sync::mpsc::Receiver<crate::jsonrpc::Incoming>,
    ) -> Value {
        match tokio::time::timeout(std::time::Duration::from_secs(5), incoming.recv())
            .await
            .unwrap()
            .unwrap()
        {
            crate::jsonrpc::Incoming::Notification { method, params } => {
                assert_eq!(method, "observed");
                params
            }
            other => panic!("unexpected peer output: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stdio_responses_accept_reject_cancel_and_shutdown_exact_requests() {
        let mut child = crate::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "acp::elicitation::tests::rpc_peer",
                "--nocapture",
            ])
            .env("KRATOS_ELICITATION_RPC_PEER", "1")
            .stdin(crate::process::Stdio::piped())
            .stdout(crate::process::Stdio::piped())
            .stderr(crate::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (client, mut incoming) =
            RpcClient::new(child.stdin.take().unwrap(), child.stdout.take().unwrap());
        let (bridge, mut inputs) = input_bridge();
        let mut forms = Elicitations::new();
        forms.handle_request(&client, "session", json!("accept"), &fixture(), &bridge);
        let (_, sender) = inputs.recv().await.unwrap();
        sender.send(valid_answers(&forms.pending[0].form)).unwrap();
        forms.next_response().await.send(&client);
        let frame = observed(&mut incoming).await;
        assert_eq!(frame["id"], "accept");
        assert_eq!(frame["result"]["action"], "accept");
        assert_eq!(frame["result"]["content"]["q1"], "o1");

        forms.handle_request(&client, "foreign", json!("rejected"), &fixture(), &bridge);
        let frame = observed(&mut incoming).await;
        assert_eq!(frame["id"], "rejected");
        assert_eq!(frame["error"]["code"], -32602);
        assert!(inputs.try_recv().is_err());

        forms.handle_request(&client, "session", json!("cancel"), &fixture(), &bridge);
        let (_, sender) = inputs.recv().await.unwrap();
        forms.cancel(&client, &json!("cancel"));
        assert!(sender.is_closed());
        let frame = observed(&mut incoming).await;
        assert_eq!(frame["id"], "cancel");
        assert_eq!(frame["result"], json!({"action":"cancel"}));

        for id in [1, 2] {
            forms.handle_request(&client, "session", json!(id), &fixture(), &bridge);
        }
        let (_, first) = inputs.recv().await.unwrap();
        let (_, second) = inputs.recv().await.unwrap();
        forms.cancel_all(&client);
        assert!(first.is_closed() && second.is_closed());
        for id in [1, 2] {
            let frame = observed(&mut incoming).await;
            assert_eq!(frame["id"], id);
            assert_eq!(frame["result"], json!({"action":"cancel"}));
        }
        assert!(forms.pending.is_empty());
        drop(client);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
