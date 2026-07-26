//! Version-pinned per-record classification for the three transcript envelopes,
//! extracted from `munshi`'s source adapters (ADR 0011). The event content built here —
//! message text and the sorted `key=value` tool rendering — is byte-identical to the
//! legacy `NormalizedEvent.content` strings.
//!
//! Where the legacy normalizer lumped everything non-content into one ignored count, this
//! module distinguishes record kinds each envelope *knows* and deliberately sets aside
//! ([`Class::Ignored`], enumerated below per adapter from `docs/harness-adapters.md` and
//! the phase-0 findings) from records it does not recognize at all ([`Class::Unknown`]).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::{Event, ToolEvent};

/// Classification outcome before the raw record is attached by the stream.
pub(crate) enum Class {
    Content(Vec<Event>),
    /// Recognized content record with empty/blank content (counted nowhere).
    Empty,
    /// Recognized, deliberately not archived; carries the record/item kind.
    Ignored(String),
    Unknown,
}

impl Class {
    fn ignored(kind: &str) -> Self {
        Self::Ignored(kind.to_owned())
    }

    fn event(event: Event) -> Self {
        Self::Content(vec![event])
    }
}

pub(crate) fn classify(source: crate::Source, object: &Map<String, Value>) -> Class {
    match source {
        crate::Source::Copilot => classify_copilot(object),
        crate::Source::ClaudeCode => classify_claude(object),
        crate::Source::Codex => classify_codex(object),
    }
}

// ---------------------------------------------------------------------------
// Copilot CLI (version-pinned to 1.0.70)
// ---------------------------------------------------------------------------

/// Copilot event types observed in the pinned 1.0.70 envelope that carry no
/// archive-worthy content (`docs/phase-0-findings.md`).
const COPILOT_BOOKKEEPING: &[&str] = &[
    "assistant.turn_end",
    "assistant.turn_start",
    "hook.end",
    "hook.start",
    "session.model_change",
    "session.resume",
    "session.shutdown",
    "session.start",
    "system.message",
];

fn classify_copilot(object: &Map<String, Value>) -> Class {
    let Some(event_type) = object.get("type").and_then(Value::as_str) else {
        return Class::Unknown;
    };
    match event_type {
        "user.message" => {
            let Some(data) = event_data(object) else {
                return Class::ignored(event_type);
            };
            let Some(content) = data.get("content").and_then(Value::as_str) else {
                return Class::ignored(event_type);
            };
            match nonempty(content) {
                Some(text) => Class::event(Event::User { text }),
                None => Class::Empty,
            }
        }
        "assistant.message" => {
            let Some(data) = event_data(object) else {
                return Class::ignored(event_type);
            };
            if !data.get("messageId").is_some_and(Value::is_string) {
                return Class::ignored(event_type);
            }
            let Some(content) = data.get("content").and_then(Value::as_str) else {
                return Class::ignored(event_type);
            };
            match nonempty(content) {
                Some(text) => Class::event(Event::Assistant { text }),
                None => Class::Empty,
            }
        }
        "tool.execution_start" => match event_data(object).filter(|data| valid_tool_start(data)) {
            Some(data) => Class::event(Event::Tool(extract_tool_start(data))),
            None => Class::ignored(event_type),
        },
        "tool.execution_complete" => {
            match event_data(object).filter(|data| valid_tool_complete(data)) {
                Some(data) => Class::event(Event::Tool(extract_tool_complete(data))),
                None => Class::ignored(event_type),
            }
        }
        _ if COPILOT_BOOKKEEPING.contains(&event_type) => Class::ignored(event_type),
        _ => Class::Unknown,
    }
}

fn valid_tool_start(data: &Map<String, Value>) -> bool {
    data.get("toolCallId").is_some_and(Value::is_string)
        && data.get("toolName").is_some_and(Value::is_string)
}

fn valid_tool_complete(data: &Map<String, Value>) -> bool {
    if !data.get("toolCallId").is_some_and(Value::is_string)
        || !data.get("success").is_some_and(Value::is_boolean)
    {
        return false;
    }
    if let Some(result) = data.get("result") {
        if !valid_tool_result(result) {
            return false;
        }
    }
    if let Some(error) = data.get("error") {
        let Some(error) = error.as_object() else {
            return false;
        };
        if !error.get("message").is_some_and(Value::is_string) {
            return false;
        }
    }
    true
}

fn valid_tool_result(result: &Value) -> bool {
    let Some(result) = result.as_object() else {
        return false;
    };
    let has_content = match result.get("content") {
        Some(Value::String(_)) => true,
        Some(_) => return false,
        None => false,
    };
    let has_textual_contents = match result.get("contents") {
        Some(Value::Array(contents)) => contents
            .iter()
            .any(|content| extract_tool_result_text(content).is_some()),
        Some(_) => return false,
        None => false,
    };
    has_content || has_textual_contents
}

fn extract_tool_start(data: &Map<String, Value>) -> ToolEvent {
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", "tool.execution_start".to_owned());
    insert(
        &mut fields,
        "tool_call_id",
        data["toolCallId"]
            .as_str()
            .expect("validated tool call ID")
            .to_owned(),
    );
    insert(
        &mut fields,
        "name",
        data["toolName"]
            .as_str()
            .expect("validated tool name")
            .to_owned(),
    );
    if let Some(arguments) = data.get("arguments").and_then(compact_value) {
        insert(&mut fields, "arguments", arguments);
    }
    ToolEvent { fields }
}

fn extract_tool_complete(data: &Map<String, Value>) -> ToolEvent {
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", "tool.execution_complete".to_owned());
    insert(
        &mut fields,
        "tool_call_id",
        data["toolCallId"]
            .as_str()
            .expect("validated tool call ID")
            .to_owned(),
    );
    insert(
        &mut fields,
        "success",
        data["success"]
            .as_bool()
            .expect("validated tool success")
            .to_string(),
    );
    if let Some(message) = data
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        insert(&mut fields, "error", message);
    }
    if let Some(result) = data.get("result").and_then(Value::as_object) {
        let mut output = Vec::new();
        if let Some(content) = result
            .get("content")
            .and_then(Value::as_str)
            .and_then(nonempty)
        {
            output.push(content);
        }
        if let Some(contents) = result.get("contents").and_then(Value::as_array) {
            output.extend(contents.iter().filter_map(extract_tool_result_text));
        }
        if !output.is_empty() {
            insert(&mut fields, "output", output.join("\n"));
        }
    }
    ToolEvent { fields }
}

fn extract_tool_result_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => nonempty(text),
        Value::Array(items) => {
            let parts: Vec<_> = items.iter().filter_map(extract_tool_result_text).collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text" | "terminal") => object
                .get("text")
                .and_then(Value::as_str)
                .and_then(nonempty),
            Some("shell_exit") => object
                .get("outputPreview")
                .and_then(Value::as_str)
                .and_then(nonempty),
            Some("resource") => object
                .get("resource")
                .and_then(Value::as_object)
                .and_then(|resource| resource.get("text"))
                .and_then(Value::as_str)
                .and_then(nonempty),
            Some("resource_link" | "image" | "audio") => None,
            _ => None,
        },
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Claude Code (version-pinned to 2.1.44, re-validated structurally at 2.1.205)
// ---------------------------------------------------------------------------

/// Claude Code record types known to be metadata/bookkeeping with no archive-worthy
/// content: the 2.1.44 `summary`/`system` records, the 2.1.205 additions (`ai-title`,
/// `attachment`, `last-prompt`, `mode`, `queue-operation`), and `file-history-snapshot`.
const CLAUDE_BOOKKEEPING: &[&str] = &[
    "ai-title",
    "attachment",
    "file-history-snapshot",
    "last-prompt",
    "mode",
    "queue-operation",
    "summary",
    "system",
];

fn classify_claude(object: &Map<String, Value>) -> Class {
    let Some(record_type) = object.get("type").and_then(Value::as_str) else {
        return Class::Unknown;
    };
    match record_type {
        "user" | "assistant" => {
            let assistant = record_type == "assistant";
            let Some(message) = object.get("message").and_then(Value::as_object) else {
                return Class::ignored(record_type);
            };
            let Some(content) = message.get("content") else {
                return Class::ignored(record_type);
            };
            classify_claude_content(content, assistant, record_type)
        }
        _ if CLAUDE_BOOKKEEPING.contains(&record_type) => Class::ignored(record_type),
        _ => Class::Unknown,
    }
}

fn classify_claude_content(content: &Value, assistant: bool, record_type: &str) -> Class {
    match content {
        Value::String(text) => match nonempty(text) {
            Some(text) => Class::event(message_event(assistant, text)),
            None => Class::Empty,
        },
        Value::Array(blocks) => {
            let mut events = Vec::new();
            // The legacy "recognized" flag: an array producing no events but containing
            // at least one typed block is a recognized-but-empty record, while an array
            // with nothing recognized falls through to ignored.
            let mut recognized = false;
            for block in blocks {
                let Some(block) = block.as_object() else {
                    continue;
                };
                let Some(block_type) = block.get("type").and_then(Value::as_str) else {
                    continue;
                };
                recognized = true;
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            if let Some(text) = nonempty(text) {
                                events.push(message_event(assistant, text));
                            }
                        }
                    }
                    "tool_use" if assistant => {
                        if let Some(event) = extract_claude_tool_use(block) {
                            events.push(event);
                        }
                    }
                    "tool_result" if !assistant => {
                        if let Some(event) = extract_claude_tool_result(block) {
                            events.push(event);
                        }
                    }
                    _ => {}
                }
            }
            if events.is_empty() {
                if recognized {
                    Class::Empty
                } else {
                    Class::ignored(record_type)
                }
            } else {
                Class::Content(events)
            }
        }
        _ => Class::ignored(record_type),
    }
}

fn extract_claude_tool_use(block: &Map<String, Value>) -> Option<Event> {
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .and_then(nonempty)?;
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", "tool_use".to_owned());
    if let Some(id) = block.get("id").and_then(Value::as_str).and_then(nonempty) {
        insert(&mut fields, "tool_use_id", id);
    }
    insert(&mut fields, "name", name);
    if let Some(input) = block.get("input").and_then(compact_value) {
        insert(&mut fields, "input", input);
    }
    Some(Event::Tool(ToolEvent { fields }))
}

fn extract_claude_tool_result(block: &Map<String, Value>) -> Option<Event> {
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", "tool_result".to_owned());
    if let Some(id) = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        insert(&mut fields, "tool_use_id", id);
    }
    if block.get("is_error").and_then(Value::as_bool) == Some(true) {
        insert(&mut fields, "is_error", "true".to_owned());
    }
    if let Some(output) = block.get("content").and_then(extract_claude_result_text) {
        insert(&mut fields, "output", output);
    }
    // A result carrying nothing beyond its `event` discriminator is dropped, leaving the
    // record recognized-but-empty (or carried by its sibling blocks).
    if fields.len() == 1 {
        return None;
    }
    Some(Event::Tool(ToolEvent { fields }))
}

fn extract_claude_result_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => nonempty(text),
        Value::Array(items) => {
            let parts: Vec<_> = items
                .iter()
                .filter_map(extract_claude_result_text)
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text") => object
                .get("text")
                .and_then(Value::as_str)
                .and_then(nonempty),
            _ => None,
        },
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Codex CLI (version-pinned to the rollout schema in openai/codex)
// ---------------------------------------------------------------------------

/// Codex rollout record types that wrap no conversation content
/// (`docs/harness-adapters.md`): everything except `response_item`.
const CODEX_METADATA: &[&str] = &["compacted", "event_msg", "session_meta", "turn_context"];

/// `response_item` payload types the pinned rollout schema defines but Munshi
/// deliberately does not archive (model reasoning and web-search activity).
const CODEX_IGNORED_ITEMS: &[&str] = &["reasoning", "web_search_call"];

fn classify_codex(object: &Map<String, Value>) -> Class {
    let Some(record_type) = object.get("type").and_then(Value::as_str) else {
        return Class::Unknown;
    };
    if CODEX_METADATA.contains(&record_type) {
        return Class::ignored(record_type);
    }
    if record_type != "response_item" {
        return Class::Unknown;
    }
    let Some(payload) = object.get("payload").and_then(Value::as_object) else {
        return Class::ignored(record_type);
    };
    let Some(item_type) = payload.get("type").and_then(Value::as_str) else {
        return Class::ignored(record_type);
    };
    match item_type {
        "message" => {
            let Some(role) = payload.get("role").and_then(Value::as_str) else {
                return Class::ignored(item_type);
            };
            let text = payload
                .get("content")
                .and_then(Value::as_array)
                .map(|blocks| extract_codex_message_text(blocks))
                .unwrap_or_default();
            match nonempty(&text) {
                Some(text) => match role {
                    "user" => Class::event(Event::User { text }),
                    "assistant" => Class::event(Event::Assistant { text }),
                    _ => Class::ignored(item_type),
                },
                None => Class::Empty,
            }
        }
        "function_call" | "custom_tool_call" => match codex_tool_call(payload, item_type) {
            Some(event) => Class::event(event),
            None => Class::ignored(item_type),
        },
        "function_call_output" | "custom_tool_call_output" => match codex_tool_output(payload) {
            Some(event) => Class::event(event),
            None => Class::ignored(item_type),
        },
        "local_shell_call" => match codex_local_shell_call(payload) {
            Some(event) => Class::event(event),
            None => Class::ignored(item_type),
        },
        _ if CODEX_IGNORED_ITEMS.contains(&item_type) => Class::ignored(item_type),
        _ => Class::Unknown,
    }
}

fn extract_codex_message_text(blocks: &[Value]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        let Some(block) = block.as_object() else {
            continue;
        };
        if let Some("input_text" | "output_text" | "text") =
            block.get("type").and_then(Value::as_str)
        {
            if let Some(text) = block.get("text").and_then(Value::as_str).and_then(nonempty) {
                parts.push(text);
            }
        }
    }
    parts.join("\n")
}

fn codex_tool_call(payload: &Map<String, Value>, item_type: &str) -> Option<Event> {
    let call_id = payload
        .get("call_id")
        .and_then(Value::as_str)
        .and_then(nonempty)?;
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", item_type.to_owned());
    insert(&mut fields, "call_id", call_id);
    if let Some(name) = payload
        .get("name")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        insert(&mut fields, "name", name);
    }
    if let Some(arguments) = payload
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        insert(&mut fields, "arguments", arguments);
    } else if let Some(input) = payload
        .get("input")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        insert(&mut fields, "input", input);
    }
    Some(Event::Tool(ToolEvent { fields }))
}

fn codex_tool_output(payload: &Map<String, Value>) -> Option<Event> {
    let call_id = payload
        .get("call_id")
        .and_then(Value::as_str)
        .and_then(nonempty)?;
    let mut fields = BTreeMap::new();
    // Custom tool call outputs deliberately share the `function_call_output` event
    // discriminator, matching the legacy rendering.
    insert(&mut fields, "event", "function_call_output".to_owned());
    insert(&mut fields, "call_id", call_id);
    if let Some(output) = payload.get("output").and_then(extract_codex_output_text) {
        insert(&mut fields, "output", output);
    }
    Some(Event::Tool(ToolEvent { fields }))
}

fn extract_codex_output_text(value: &Value) -> Option<String> {
    match value {
        // `function_call_output.output` is either a plain string or an array of
        // structured content items on the wire.
        Value::String(text) => nonempty(text),
        Value::Array(items) => {
            let parts: Vec<_> = items.iter().filter_map(extract_codex_output_text).collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(object) => object
            .get("content")
            .and_then(extract_codex_output_text)
            .or_else(|| {
                object
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(nonempty)
            }),
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

fn codex_local_shell_call(payload: &Map<String, Value>) -> Option<Event> {
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", "local_shell_call".to_owned());
    if let Some(call_id) = payload
        .get("call_id")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        insert(&mut fields, "call_id", call_id);
    }
    if let Some(command) = payload
        .get("action")
        .and_then(Value::as_object)
        .and_then(|action| action.get("command"))
        .and_then(compact_value)
    {
        insert(&mut fields, "command", command);
    }
    if fields.len() == 1 {
        return None;
    }
    Some(Event::Tool(ToolEvent { fields }))
}

// ---------------------------------------------------------------------------
// Shared helpers (ported verbatim from the legacy normalizer)
// ---------------------------------------------------------------------------

fn message_event(assistant: bool, text: String) -> Event {
    if assistant {
        Event::Assistant { text }
    } else {
        Event::User { text }
    }
}

fn event_data(object: &Map<String, Value>) -> Option<&Map<String, Value>> {
    object.get("data").and_then(Value::as_object)
}

fn nonempty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn compact_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => nonempty(text),
        Value::Null => None,
        Value::Array(_) | Value::Object(_) | Value::Bool(_) | Value::Number(_) => {
            serde_json::to_string(value).ok()
        }
    }
}

fn insert(fields: &mut BTreeMap<String, String>, key: &str, value: String) {
    fields.insert(key.to_owned(), value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    fn classify_json(source: Source, json: &str) -> Class {
        let value: Value = serde_json::from_str(json).unwrap();
        classify(source, value.as_object().unwrap())
    }

    #[test]
    fn copilot_bookkeeping_is_ignored_while_foreign_types_are_unknown() {
        assert!(matches!(
            classify_json(Source::Copilot, r#"{"type":"session.start","data":{}}"#),
            Class::Ignored(kind) if kind == "session.start"
        ));
        assert!(matches!(
            classify_json(
                Source::Copilot,
                r#"{"type":"future.private_event","data":{}}"#
            ),
            Class::Unknown
        ));
        // A recognized content type with malformed data is ignored, not unknown.
        assert!(matches!(
            classify_json(Source::Copilot, r#"{"type":"assistant.message","data":"x"}"#),
            Class::Ignored(kind) if kind == "assistant.message"
        ));
    }

    #[test]
    fn claude_bookkeeping_is_ignored_while_foreign_types_are_unknown() {
        assert!(matches!(
            classify_json(Source::ClaudeCode, r#"{"type":"queue-operation"}"#),
            Class::Ignored(kind) if kind == "queue-operation"
        ));
        assert!(matches!(
            classify_json(Source::ClaudeCode, r#"{"type":"totally-new-type"}"#),
            Class::Unknown
        ));
        // Blank string content is recognized-but-empty, counted nowhere.
        assert!(matches!(
            classify_json(
                Source::ClaudeCode,
                r#"{"type":"user","message":{"content":"  "}}"#
            ),
            Class::Empty
        ));
    }

    #[test]
    fn codex_metadata_and_known_items_are_ignored_while_foreign_kinds_are_unknown() {
        assert!(matches!(
            classify_json(Source::Codex, r#"{"type":"session_meta","payload":{}}"#),
            Class::Ignored(kind) if kind == "session_meta"
        ));
        assert!(matches!(
            classify_json(
                Source::Codex,
                r#"{"type":"response_item","payload":{"type":"reasoning"}}"#
            ),
            Class::Ignored(kind) if kind == "reasoning"
        ));
        assert!(matches!(
            classify_json(
                Source::Codex,
                r#"{"type":"response_item","payload":{"type":"hologram_call"}}"#
            ),
            Class::Unknown
        ));
        assert!(matches!(
            classify_json(Source::Codex, r#"{"type":"world_state_2","payload":{}}"#),
            Class::Unknown
        ));
    }
}
