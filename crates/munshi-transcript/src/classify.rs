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
/// archive-worthy content (`docs/phase-0-findings.md`), plus the
/// `session.usage_checkpoint` bookkeeping kind surfaced by archived 1.0.5x
/// snapshots (issue #34) and the historical bookkeeping kinds surfaced by the
/// full-archive census (issue #45: `permission.requested`,
/// `permission.completed`, `session.binary_asset`, `subagent.started`,
/// `subagent.completed`, `system.notification`, `session.permissions_changed`,
/// `session.mode_changed`, `session.compaction_start`), and the census's
/// second wave surfaced once the chunked-marathon giants uploaded (issue #45:
/// `abort`, `session.compaction_complete`, `session.context_changed`,
/// `session.error`, `session.info`, `session.plan_changed`,
/// `session.task_complete`, `session.workspace_file_changed`).
const COPILOT_BOOKKEEPING: &[&str] = &[
    "abort",
    "assistant.turn_end",
    "assistant.turn_start",
    "hook.end",
    "hook.start",
    "permission.completed",
    "permission.requested",
    "session.binary_asset",
    "session.compaction_complete",
    "session.compaction_start",
    "session.context_changed",
    "session.error",
    "session.info",
    "session.mode_changed",
    "session.model_change",
    "session.permissions_changed",
    "session.plan_changed",
    "session.resume",
    "session.shutdown",
    "session.start",
    "session.task_complete",
    "session.truncation",
    "session.usage_checkpoint",
    "session.warning",
    "session.workspace_file_changed",
    "subagent.completed",
    "subagent.started",
    "system.message",
    "system.notification",
];

/// Copilot built-in tool names whose `arguments.command` is a shell command line, and
/// nothing else (issue #77). `bash` is the CLI's terminal tool — every one of the archive's
/// tens of thousands of `tool.execution_start` `bash` records carries a string `command` —
/// and `local_shell` is the user-requested sibling the archive records under
/// `tool.user_requested`.
///
/// Deliberately *not* listed, because their `command` would mean something else or nothing:
/// `str_replace_editor`, whose `command` argument names an editor operation (`view`,
/// `str_replace`) rather than anything a shell runs; and the shell-management tools
/// `read_bash` / `stop_bash` / `list_bash`, which address a running shell by `shellId` and
/// carry no command at all.
const COPILOT_SHELL_TOOLS: &[&str] = &["bash", "local_shell"];

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
            Some(data) => Class::event(Event::Tool(extract_builtin_tool_invocation(
                event_type, data,
            ))),
            None => Class::ignored(event_type),
        },
        "tool.execution_complete" => {
            match event_data(object).filter(|data| valid_tool_complete(data)) {
                Some(data) => Class::event(Event::Tool(extract_tool_complete(data))),
                None => Class::ignored(event_type),
            }
        }
        // Archive-observed tool-activity kinds (issue #51): real session content, not
        // bookkeeping. `tool.user_requested` is a user-initiated sibling of
        // `tool.execution_start` with the identical payload shape.
        "tool.user_requested" => match event_data(object).filter(|data| valid_tool_start(data)) {
            Some(data) => Class::event(Event::Tool(extract_builtin_tool_invocation(
                event_type, data,
            ))),
            None => Class::ignored(event_type),
        },
        "skill.invoked" => match event_data(object).and_then(extract_skill_invoked) {
            Some(tool) => Class::event(Event::Tool(tool)),
            None => Class::ignored(event_type),
        },
        "external_tool.requested" => {
            match event_data(object).filter(|data| valid_tool_start(data)) {
                Some(data) => Class::event(Event::Tool(extract_external_tool_request(data))),
                None => Class::ignored(event_type),
            }
        }
        "external_tool.completed" => {
            match event_data(object).and_then(extract_external_tool_completed) {
                Some(tool) => Class::event(Event::Tool(tool)),
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

/// The shared tool-invocation extraction for the `valid_tool_start`-shaped payloads
/// (`tool.execution_start`, `tool.user_requested`, `external_tool.requested`): the event
/// discriminator, the validated call id and name, and the compacted arguments.
fn extract_tool_invocation(event_type: &str, data: &Map<String, Value>) -> ToolEvent {
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", event_type.to_owned());
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
    ToolEvent::legacy(fields)
}

/// [`extract_tool_invocation`] for the CLI's *built-in* tools (`tool.execution_start` and
/// its user-initiated sibling `tool.user_requested`), which additionally promotes the
/// shell command out of the `arguments` blob (issue #77). The blob itself is untouched.
///
/// `external_tool.requested` deliberately does not go through here: its names come from
/// the MCP/extension namespace, where a tool called `bash` is somebody else's tool and not
/// the CLI shell, so the name would no longer certify the meaning of `arguments.command`.
fn extract_builtin_tool_invocation(event_type: &str, data: &Map<String, Value>) -> ToolEvent {
    let mut tool = extract_tool_invocation(event_type, data);
    if tool
        .name()
        .is_some_and(|name| COPILOT_SHELL_TOOLS.contains(&name))
        && let Some(command) = data
            .get("arguments")
            .and_then(Value::as_object)
            .and_then(|arguments| arguments.get("command"))
            .and_then(compact_value)
    {
        tool.insert_derived("command", command);
    }
    tool
}

/// `skill.invoked` (issue #51): the agent loaded a skill — activity comparable to Claude
/// Code's `Skill` tool use. Requires a nonempty skill `name`; carries the skill's path,
/// card metadata, and full SKILL.md `content` (size policy belongs to consumers).
fn extract_skill_invoked(data: &Map<String, Value>) -> Option<ToolEvent> {
    let name = data
        .get("name")
        .and_then(Value::as_str)
        .and_then(nonempty)?;
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", "skill.invoked".to_owned());
    insert(&mut fields, "name", name);
    for key in [
        "path",
        "description",
        "source",
        "trigger",
        "model",
        "content",
    ] {
        if let Some(value) = data.get(key).and_then(Value::as_str).and_then(nonempty) {
            insert(&mut fields, key, value);
        }
    }
    Some(ToolEvent::legacy(fields))
}

/// `external_tool.requested` (issue #51): an MCP/external tool invocation. The payload is
/// the `tool.execution_start` shape plus a `requestId` correlating the eventual
/// `external_tool.completed` marker.
fn extract_external_tool_request(data: &Map<String, Value>) -> ToolEvent {
    let mut tool = extract_tool_invocation("external_tool.requested", data);
    if let Some(request_id) = data
        .get("requestId")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        insert(&mut tool.fields, "request_id", request_id);
    }
    tool
}

/// `external_tool.completed` (issue #51): the completion marker for an external tool
/// call. Archived records carry only the correlating `requestId`, which is therefore
/// required — without it the record marks nothing.
fn extract_external_tool_completed(data: &Map<String, Value>) -> Option<ToolEvent> {
    let request_id = data
        .get("requestId")
        .and_then(Value::as_str)
        .and_then(nonempty)?;
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", "external_tool.completed".to_owned());
    insert(&mut fields, "request_id", request_id);
    Some(ToolEvent::legacy(fields))
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
    ToolEvent::legacy(fields)
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
/// `attachment`, `last-prompt`, `mode`, `queue-operation`), `file-history-snapshot`,
/// and the newer session-bookkeeping kinds observed in live archives (issue #30:
/// `file-history-delta`, `frame-link`, `permission-mode`, `pr-link`; issue #46:
/// `agent-name`).
/// The Claude Code tool whose `tool_use.input` carries a shell command (issue #77), and the
/// only one: across the archive every `Bash` invocation records a string `input.command`,
/// and no other tool name records one at all. `BashOutput` / `KillShell` address an
/// already-running shell by id, so there is no command of theirs to promote.
const CLAUDE_SHELL_TOOL: &str = "Bash";

const CLAUDE_BOOKKEEPING: &[&str] = &[
    "agent-name",
    "ai-title",
    "attachment",
    "file-history-delta",
    "file-history-snapshot",
    "frame-link",
    "last-prompt",
    "mode",
    "permission-mode",
    "pr-link",
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
    let shell = name == CLAUDE_SHELL_TOOL;
    let mut fields = BTreeMap::new();
    insert(&mut fields, "event", "tool_use".to_owned());
    if let Some(id) = block.get("id").and_then(Value::as_str).and_then(nonempty) {
        insert(&mut fields, "tool_use_id", id);
    }
    insert(&mut fields, "name", name);
    if let Some(input) = block.get("input").and_then(compact_value) {
        insert(&mut fields, "input", input);
    }
    let mut tool = ToolEvent::legacy(fields);
    // Issue #77: the shell command is promoted out of the `input` blob, which is kept
    // beside it verbatim. Only `Bash` is read; every other tool's `input` keys mean
    // whatever that tool decides.
    if shell
        && let Some(command) = block
            .get("input")
            .and_then(Value::as_object)
            .and_then(|input| input.get("command"))
            .and_then(compact_value)
    {
        tool.insert_derived("command", command);
    }
    Some(Event::Tool(tool))
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
    Some(Event::Tool(ToolEvent::legacy(fields)))
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

/// The Codex `function_call` tool name whose `arguments` is the pinned shell schema
/// (issue #77) — the rollout's more common shell shape than `local_shell_call`. Every other
/// function name carries a tool-defined argument object this crate makes no claims about.
const CODEX_SHELL_TOOL: &str = "shell";

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
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .and_then(nonempty);
    if let Some(name) = name.clone() {
        insert(&mut fields, "name", name);
    }
    let arguments = payload
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(nonempty);
    if let Some(arguments) = arguments.clone() {
        insert(&mut fields, "arguments", arguments);
    } else if let Some(input) = payload
        .get("input")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        insert(&mut fields, "input", input);
    }
    let mut tool = ToolEvent::legacy(fields);
    // Issue #77: the shell tool's `arguments` is a JSON *string*; only that one name's
    // encoding is pinned, so only it is read. `custom_tool_call`'s free-form `input`
    // stays where it is.
    if item_type == "function_call"
        && name.as_deref() == Some(CODEX_SHELL_TOOL)
        && let Some(command) = arguments.as_deref().and_then(codex_shell_command)
    {
        tool.insert_derived("command", command);
    }
    Some(Event::Tool(tool))
}

/// The `command` inside a Codex `shell` `function_call`'s `arguments`, which the rollout
/// records as a JSON string encoding `{"command": [...], "workdir": ..., "timeout_ms": ...}`.
/// The command is argv, rendered exactly as [`codex_local_shell_call`] renders
/// `action.command` — compact JSON array text — so the two Codex shapes agree with each
/// other. Arguments that are not a JSON object, or carry no `command`, yield nothing.
fn codex_shell_command(arguments: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(arguments).ok()?;
    parsed.as_object()?.get("command").and_then(compact_value)
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
    Some(Event::Tool(ToolEvent::legacy(fields)))
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

/// `local_shell_call`: the rollout's other shell shape, whose `action.command` this crate
/// has always typed as `command`. That key predates the derived-field split (issue #77), so
/// it stays part of the legacy rendering — see [`crate::ToolEvent`]; nothing already inside
/// `rendered()` is ever moved out of it.
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
    Some(Event::Tool(ToolEvent::legacy(fields)))
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

    /// The single tool event a record classifies to, for the `command`-promotion tests.
    fn tool_of(source: Source, json: &str) -> ToolEvent {
        let Class::Content(events) = classify_json(source, json) else {
            panic!("expected content: {json}");
        };
        let [Event::Tool(tool)] = events.as_slice() else {
            panic!("expected one tool event: {json}");
        };
        tool.clone()
    }

    /// Asserts what a record promotes as `command` (or that it promotes nothing), and — in
    /// every case — that the promotion stayed out of the legacy rendering while the blob it
    /// came from stayed in it untouched.
    fn assert_command(source: Source, json: &str, expected: Option<&str>, rendered: &str) {
        let tool = tool_of(source, json);
        assert_eq!(tool.command(), expected, "command for {json}");
        assert_eq!(
            tool.derived.iter().map(String::as_str).collect::<Vec<_>>(),
            expected.map(|_| vec!["command"]).unwrap_or_default(),
            "derived keys for {json}"
        );
        assert_eq!(tool.rendered(), rendered, "legacy rendering for {json}");
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
        // Archive-observed bookkeeping kind absent from the pinned 1.0.70 tables (#34).
        assert!(matches!(
            classify_json(
                Source::Copilot,
                r#"{"type":"session.usage_checkpoint","data":{"tokensUsed":123}}"#
            ),
            Class::Ignored(kind) if kind == "session.usage_checkpoint"
        ));
        // Historical bookkeeping kinds surfaced by the full-archive census (#45).
        for kind in [
            "permission.requested",
            "permission.completed",
            "session.binary_asset",
            "subagent.started",
            "subagent.completed",
            "system.notification",
            "session.permissions_changed",
            "session.mode_changed",
            "session.compaction_start",
            "abort",
            "session.compaction_complete",
            "session.context_changed",
            "session.error",
            "session.info",
            "session.plan_changed",
            "session.task_complete",
            "session.workspace_file_changed",
            "session.truncation",
            "session.warning",
        ] {
            let json = format!(r#"{{"type":"{kind}","data":{{}}}}"#);
            assert!(matches!(
                classify_json(Source::Copilot, &json),
                Class::Ignored(ignored) if ignored == kind
            ));
        }
    }

    #[test]
    fn copilot_tool_activity_kinds_map_to_tool_events() {
        // The four archive-observed content kinds (issue #51), shaped exactly as the
        // live archive records them.
        let expect_tool = |json: &str, rendered: &str| {
            let Class::Content(events) = classify_json(Source::Copilot, json) else {
                panic!("expected content: {json}");
            };
            let [Event::Tool(tool)] = events.as_slice() else {
                panic!("expected one tool event: {json}");
            };
            assert_eq!(tool.rendered(), rendered, "rendering for {json}");
        };

        expect_tool(
            r##"{"type":"skill.invoked","data":{"name":"synthetic-skill","path":"/home/u/.copilot/skills/synthetic-skill/SKILL.md","content":"# Synthetic Skill\nBody.","source":"personal-copilot","description":"A fixture skill.","trigger":"agent-invoked","model":"fixture-model"}}"##,
            "content=# Synthetic Skill\nBody. description=A fixture skill. \
             event=skill.invoked model=fixture-model name=synthetic-skill \
             path=/home/u/.copilot/skills/synthetic-skill/SKILL.md \
             source=personal-copilot trigger=agent-invoked",
        );
        expect_tool(
            r#"{"type":"tool.user_requested","data":{"toolCallId":"call-1","toolName":"local_shell","arguments":{"command":"git remote -v"}}}"#,
            "arguments={\"command\":\"git remote -v\"} event=tool.user_requested \
             name=local_shell tool_call_id=call-1",
        );
        expect_tool(
            r#"{"type":"external_tool.requested","data":{"requestId":"req-1","sessionId":"s","toolCallId":"toolu_01","toolName":"extensions_manage","arguments":{"operation":"guide"},"workingDirectory":"/w"}}"#,
            "arguments={\"operation\":\"guide\"} event=external_tool.requested \
             name=extensions_manage request_id=req-1 tool_call_id=toolu_01",
        );
        expect_tool(
            r#"{"type":"external_tool.completed","data":{"requestId":"req-1"}}"#,
            "event=external_tool.completed request_id=req-1",
        );

        // Recognized kinds with a missing required field degrade to ignored, not unknown.
        for json in [
            r#"{"type":"skill.invoked","data":{"path":"/p","content":"c"}}"#,
            r#"{"type":"skill.invoked","data":{"name":"  "}}"#,
            r#"{"type":"tool.user_requested","data":{"toolName":"local_shell"}}"#,
            r#"{"type":"external_tool.requested","data":{"requestId":"req-1","toolName":"t"}}"#,
            r#"{"type":"external_tool.completed","data":{}}"#,
            r#"{"type":"external_tool.completed","data":{"requestId":42}}"#,
        ] {
            let expected = serde_json::from_str::<Value>(json).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(
                matches!(
                    classify_json(Source::Copilot, json),
                    Class::Ignored(kind) if kind == expected
                ),
                "expected ignored: {json}"
            );
        }
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
        // Archive-observed bookkeeping kinds the pinned 2.1.44/2.1.205 schema predates (#30).
        for json in [
            r#"{"type":"permission-mode","permissionMode":"default","sessionId":"s"}"#,
            r#"{"type":"permission-mode","permissionMode":"auto","sessionId":"s"}"#,
            r#"{"type":"pr-link","sessionId":"s"}"#,
            r#"{"type":"file-history-delta","sessionId":"s"}"#,
            r#"{"type":"frame-link","sessionId":"s"}"#,
            // Full-archive census (#46).
            r#"{"type":"agent-name","name":"lively-crimson-otter","sessionId":"s"}"#,
        ] {
            let expected = serde_json::from_str::<Value>(json).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(matches!(
                classify_json(Source::ClaudeCode, json),
                Class::Ignored(kind) if kind == expected
            ));
        }
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

    #[test]
    fn claude_bash_tool_use_promotes_its_command_and_no_other_tool_does() {
        // The shell tool: the command is promoted, and `input` is kept verbatim beside it.
        assert_command(
            Source::ClaudeCode,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test --all","description":"Run the suite"}}]}}"#,
            Some("cargo test --all"),
            "event=tool_use input={\"command\":\"cargo test --all\",\
             \"description\":\"Run the suite\"} name=Bash tool_use_id=toolu_1",
        );
        // A non-shell tool whose input happens to carry a `command` key promotes nothing:
        // the name, not the key, certifies the meaning.
        assert_command(
            Source::ClaudeCode,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Skill","input":{"command":"/review","skill":"code-review"}}]}}"#,
            None,
            "event=tool_use input={\"command\":\"/review\",\"skill\":\"code-review\"} \
             name=Skill tool_use_id=toolu_2",
        );
        // Absent, blank, and non-object inputs all leave the field off; the record still
        // classifies as the same tool event it always did.
        assert_command(
            Source::ClaudeCode,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_3","name":"Bash","input":{"description":"no command key"}}]}}"#,
            None,
            "event=tool_use input={\"description\":\"no command key\"} name=Bash \
             tool_use_id=toolu_3",
        );
        assert_command(
            Source::ClaudeCode,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_4","name":"Bash","input":{"command":"   "}}]}}"#,
            None,
            "event=tool_use input={\"command\":\"   \"} name=Bash tool_use_id=toolu_4",
        );
        assert_command(
            Source::ClaudeCode,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_5","name":"Bash","input":"cargo test"}]}}"#,
            None,
            "event=tool_use input=cargo test name=Bash tool_use_id=toolu_5",
        );
    }

    #[test]
    fn copilot_shell_tools_promote_their_command_and_editor_subcommands_do_not() {
        assert_command(
            Source::Copilot,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"bash","arguments":{"command":"cargo fmt --check","description":"Check formatting"}}}"#,
            Some("cargo fmt --check"),
            "arguments={\"command\":\"cargo fmt --check\",\"description\":\"Check formatting\"} \
             event=tool.execution_start name=bash tool_call_id=c1",
        );
        assert_command(
            Source::Copilot,
            r#"{"type":"tool.user_requested","data":{"toolCallId":"c2","toolName":"local_shell","arguments":{"command":"git remote -v"}}}"#,
            Some("git remote -v"),
            "arguments={\"command\":\"git remote -v\"} event=tool.user_requested \
             name=local_shell tool_call_id=c2",
        );
        // `str_replace_editor`'s `command` names an editor operation, not a shell command:
        // deliberately not promoted.
        assert_command(
            Source::Copilot,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"c3","toolName":"str_replace_editor","arguments":{"command":"view","path":"src/lib.rs"}}}"#,
            None,
            "arguments={\"command\":\"view\",\"path\":\"src/lib.rs\"} \
             event=tool.execution_start name=str_replace_editor tool_call_id=c3",
        );
        // A non-shell tool, and a shell tool whose arguments are not an object.
        assert_command(
            Source::Copilot,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"c4","toolName":"view","arguments":{"path":"src/lib.rs"}}}"#,
            None,
            "arguments={\"path\":\"src/lib.rs\"} event=tool.execution_start name=view \
             tool_call_id=c4",
        );
        assert_command(
            Source::Copilot,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"c5","toolName":"bash","arguments":"cargo test"}}"#,
            None,
            "arguments=cargo test event=tool.execution_start name=bash tool_call_id=c5",
        );
        // `external_tool.requested` names MCP/extension tools, where `bash` would be
        // somebody else's tool: the name no longer certifies the argument shape.
        assert_command(
            Source::Copilot,
            r#"{"type":"external_tool.requested","data":{"requestId":"r1","toolCallId":"c6","toolName":"bash","arguments":{"command":"echo hi"}}}"#,
            None,
            "arguments={\"command\":\"echo hi\"} event=external_tool.requested name=bash \
             request_id=r1 tool_call_id=c6",
        );
    }

    #[test]
    fn codex_shell_function_calls_promote_their_argv_command() {
        // Argv, rendered as `local_shell_call` renders it: compact JSON array text.
        assert_command(
            Source::Codex,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"ls -la\"],\"workdir\":\"/work\"}","call_id":"call_1"}}"#,
            Some(r#"["bash","-lc","ls -la"]"#),
            "arguments={\"command\":[\"bash\",\"-lc\",\"ls -la\"],\"workdir\":\"/work\"} \
             call_id=call_1 event=function_call name=shell",
        );
        // Another function's arguments are that tool's business, even with a `command` key.
        assert_command(
            Source::Codex,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"command\":\"step 1\"}","call_id":"call_2"}}"#,
            None,
            "arguments={\"command\":\"step 1\"} call_id=call_2 event=function_call \
             name=update_plan",
        );
        // Arguments that do not parse, or parse to something other than an object with a
        // command, leave the field off.
        for (arguments, rendered) in [
            ("not json", "arguments=not json"),
            ("[1,2]", "arguments=[1,2]"),
            (
                r#"{\"workdir\":\"/work\"}"#,
                "arguments={\"workdir\":\"/work\"}",
            ),
        ] {
            let json = format!(
                r#"{{"type":"response_item","payload":{{"type":"function_call","name":"shell","arguments":"{arguments}","call_id":"call_3"}}}}"#
            );
            assert_command(
                Source::Codex,
                &json,
                None,
                &format!("{rendered} call_id=call_3 event=function_call name=shell"),
            );
        }
        // `local_shell_call`'s `command` predates the split: it stays in the rendering.
        let tool = tool_of(
            Source::Codex,
            r#"{"type":"response_item","payload":{"type":"local_shell_call","call_id":"call_4","action":{"type":"exec","command":["bash","-lc","echo hi"]}}}"#,
        );
        assert_eq!(tool.command(), Some(r#"["bash","-lc","echo hi"]"#));
        assert!(tool.derived.is_empty());
        assert_eq!(
            tool.rendered(),
            "call_id=call_4 command=[\"bash\",\"-lc\",\"echo hi\"] event=local_shell_call"
        );
    }
}
