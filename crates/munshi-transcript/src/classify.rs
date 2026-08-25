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

use crate::{AssistantMeta, Compaction, CompactionPhase, Event, TokenUsage, ToolEvent};

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

/// What a record says about the API message behind it (issue #77): the model, the token
/// usage, and the message id, read from the one record shape per source that records them.
///
/// Deliberately a second pass over the record rather than an output of [`classify`]: what a
/// message was billed does not depend on what its content classified as, and reading it
/// separately is what lets an assistant record whose content is only tool calls — or is
/// empty, or is unreadable — still account for its tokens. Codex records no per-message
/// model or usage anywhere, so nothing is read for it.
pub(crate) fn assistant_meta(
    source: crate::Source,
    object: &Map<String, Value>,
) -> Option<AssistantMeta> {
    let record_type = object.get("type").and_then(Value::as_str)?;
    match source {
        crate::Source::Copilot => (record_type == "assistant.message")
            .then(|| event_data(object))
            .flatten()
            .and_then(copilot_assistant_meta),
        crate::Source::ClaudeCode => (record_type == "assistant")
            .then(|| object.get("message").and_then(Value::as_object))
            .flatten()
            .and_then(claude_assistant_meta),
        crate::Source::Codex => None,
    }
}

/// What a record says about a context compaction (issue #77): which half of one it marks,
/// and the context-size figures the source states around it.
///
/// A third independent pass, for the same reason [`assistant_meta`] is a second one and one
/// more besides. Every record read here is [`Class::Ignored`] bookkeeping and stays that
/// way: a compaction is a fact about the session's context window, not conversation content,
/// so typing it as an event would move records out of a census and into a legacy rendering
/// they have never had.
///
/// Claude Code's post-compaction summary — the `user` record flagged `isCompactSummary` that
/// follows a boundary — is deliberately *not* read, though it is the one compaction-adjacent
/// record that already is content. It carries no figure the boundary beside it does not, and
/// flagging it would put a second marker on one compaction for a consumer to count twice.
///
/// Codex records nothing. Its rollout schema does have a `compacted` metadata record — named
/// in [`CODEX_METADATA`] — but the archive holds zero Codex sessions, so this crate has never
/// seen one payload of it. Guessing at the shape would be inventing an interpretation, and
/// an absent field is an under-claim a consumer can see.
pub(crate) fn compaction(source: crate::Source, object: &Map<String, Value>) -> Option<Compaction> {
    let record_type = object.get("type").and_then(Value::as_str)?;
    match source {
        crate::Source::Copilot => {
            let phase = match record_type {
                "session.compaction_start" => CompactionPhase::Start,
                "session.compaction_complete" => CompactionPhase::Complete,
                _ => return None,
            };
            Some(copilot_compaction(phase, event_data(object)))
        }
        crate::Source::ClaudeCode => (record_type == "system")
            .then(|| claude_compaction(object))
            .flatten(),
        crate::Source::Codex => None,
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

/// The model and token usage an `assistant.message` records (issue #77). The CLI reports a
/// single `outputTokens` count per message and nothing else — no input, cache, thinking, or
/// tier figures exist in this envelope, so the promoted usage carries only that one, and
/// the rest stay absent rather than being invented from the session-level
/// `session.shutdown` `modelMetrics` roll-up, which is a different quantity.
///
/// The other kinds naming a model (`session.model_change`, `session.shutdown`) stay
/// bookkeeping: they describe the session, not a message, so no message's cost can be
/// attributed from them.
fn copilot_assistant_meta(data: &Map<String, Value>) -> Option<AssistantMeta> {
    AssistantMeta {
        model: data.get("model").and_then(Value::as_str).and_then(nonempty),
        usage: TokenUsage {
            output_tokens: token_count(data.get("outputTokens")),
            ..TokenUsage::default()
        }
        .recorded(),
        message_id: data
            .get("messageId")
            .and_then(Value::as_str)
            .and_then(nonempty),
    }
    .recorded()
}

/// The context-size figures a Copilot compaction marker states (issue #77), read per phase
/// because the same key names mean different things on the two halves.
///
/// `session.compaction_start` states the pre-compaction context as a three-way breakdown
/// (`systemTokens` / `conversationTokens` / `toolDefinitionsTokens`, on all 367 of the mirror
/// cache's starts) and, in the newer envelope only, that breakdown's total as
/// `currentTokens` alongside a `tokenLimit` and a `trigger` (5 of 367). The total is read as
/// `pre_tokens` where the source writes it, and never computed from the components where it
/// does not — the archive holds a pair whose components sum to 400,754 against a recorded
/// 403,971.
///
/// `session.compaction_complete` states `preCompactionTokens` and `success`, plus
/// `postCompactionTokens` on 3 records. Its `systemTokens` / `conversationTokens` /
/// `toolDefinitionsTokens` — spelled identically, present on 1 record — describe the context
/// the compaction *left*, so they are deliberately not read: filing them as the pre-compaction
/// breakdown would report a post-compaction figure as its own opposite.
///
/// Deliberately left in the raw record. `summaryContent`, the compaction summary itself:
/// this promotion types the *fact and size* of a compaction, and hanging a multi-kilobyte
/// body off a record-level analysis field to serve a lane that folds counts and tokens is
/// not a size figure. `compactionTokensUsed`, the summarizer call's own bill: cost is
/// [`crate::AssistantMeta`]'s surface, where it is per API message and deduplicated by
/// message id, and a second differently-keyed cost object here would be a parallel truth
/// that folds differently. `checkpointPath` (a filesystem path, hence a privacy surface),
/// `checkpointNumber`, `requestId`, `serviceRequestId`: bookkeeping no metric names.
/// `preCompactionMessagesLength`, `messagesRemoved`, `tokensRemoved`: message counts and a
/// difference the promoted figures already state, promotable later under the same
/// one-at-a-time rule if a consumer ever names them. `model`: it names the model the
/// *summarizer* ran under, and the usage promotion already refused session-level model
/// records for attributing no message's cost.
fn copilot_compaction(phase: CompactionPhase, data: Option<&Map<String, Value>>) -> Compaction {
    let mut compaction = Compaction {
        phase,
        trigger: None,
        succeeded: None,
        pre_tokens: None,
        post_tokens: None,
        system_tokens: None,
        conversation_tokens: None,
        tool_definition_tokens: None,
        token_limit: None,
    };
    // A marker whose `data` is missing or malformed still marks a compaction: the record's
    // existence is the claim, and only the figures are lost.
    let Some(data) = data else {
        return compaction;
    };
    compaction.trigger = data
        .get("trigger")
        .and_then(Value::as_str)
        .and_then(nonempty);
    compaction.token_limit = token_count(data.get("tokenLimit"));
    match phase {
        CompactionPhase::Start => {
            compaction.pre_tokens = token_count(data.get("currentTokens"));
            compaction.system_tokens = token_count(data.get("systemTokens"));
            compaction.conversation_tokens = token_count(data.get("conversationTokens"));
            compaction.tool_definition_tokens = token_count(data.get("toolDefinitionsTokens"));
        }
        CompactionPhase::Complete => {
            compaction.succeeded = data.get("success").and_then(Value::as_bool);
            compaction.pre_tokens = token_count(data.get("preCompactionTokens"));
            compaction.post_tokens = token_count(data.get("postCompactionTokens"));
        }
    }
    compaction
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

/// The Claude Code tool whose `tool_use.input` carries a shell command (issue #77), and the
/// only one: across the archive every `Bash` invocation records a string `input.command`,
/// and no other tool name records one at all. `BashOutput` / `KillShell` address an
/// already-running shell by id, so there is no command of theirs to promote.
const CLAUDE_SHELL_TOOL: &str = "Bash";

/// Claude Code record types known to be metadata/bookkeeping with no archive-worthy
/// content: the 2.1.44 `summary`/`system` records, the 2.1.205 additions (`ai-title`,
/// `attachment`, `last-prompt`, `mode`, `queue-operation`), `file-history-snapshot`,
/// and the newer session-bookkeeping kinds observed in live archives (issue #30:
/// `file-history-delta`, `frame-link`, `permission-mode`, `pr-link`; issue #46:
/// `agent-name`).
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

/// The pinned `message` keys an assistant record records its API message under: `model`,
/// `id`, and `usage` (issue #77). Each is optional — the archive holds records with none of
/// them — and each is read only in the one shape the envelope pins it to.
fn claude_assistant_meta(message: &Map<String, Value>) -> Option<AssistantMeta> {
    AssistantMeta {
        model: message
            .get("model")
            .and_then(Value::as_str)
            .and_then(nonempty),
        usage: message
            .get("usage")
            .and_then(Value::as_object)
            .and_then(claude_token_usage),
        message_id: message.get("id").and_then(Value::as_str).and_then(nonempty),
    }
    .recorded()
}

/// The `system` record subtype Claude Code writes at a compaction boundary (issue #77), and
/// the object it states the compaction's figures in.
///
/// The subtype is required, not merely preferred: `system` is a general bookkeeping kind
/// this crate has always ignored wholesale (7,895 records across the mirror cache), and only
/// its `compact_boundary` subtype marks a compaction (9 records, in 8 of 346 sessions).
/// Reading `compactMetadata` off any `system` record would be trusting a key name on a kind
/// whose other subtypes are free to spell anything.
const CLAUDE_COMPACT_BOUNDARY: &str = "compact_boundary";

/// The figures a Claude Code `compact_boundary` states (issue #77).
///
/// Claude Code writes one record per compaction, *after* the fact: it states `preTokens` and
/// `postTokens` together, which only a finished compaction knows, so it reads as
/// [`CompactionPhase::Complete`]. All 9 of the mirror cache's boundaries carry `trigger`,
/// `preTokens` and `postTokens`, every one of them as an integer and every `trigger` reading
/// `manual` — the envelope's automatic trigger exists but this archive has never recorded
/// one, so the value passes through verbatim rather than being folded into a two-state flag
/// this crate has only ever seen one side of.
///
/// The record's own `content` (`"Conversation compacted"`) is a fixed English label, not a
/// figure, and the summary the compaction produced is the *next* record — a `user` message
/// flagged `isCompactSummary`, which this crate already classifies as a user event and
/// already renders. There is nothing to promote from it that the stream does not already
/// carry, and flagging that record would put a second marker on the same compaction for a
/// consumer to double-count.
///
/// Deliberately left in the raw record. `cumulativeDroppedTokens`, which is a *session*
/// running total and not this compaction's reclaim — the cache proves the difference, one
/// session's second boundary reading 1,317,965 against its own 328,757 of `preTokens` minus
/// `postTokens` — so summing it across a session's compactions would multiply-count.
/// `durationMs`, how long compacting took rather than how large the context was.
/// `preservedSegment`, `preservedMessages` and `preCompactDiscoveredTools`, which are
/// message uuids and tool names describing the compaction's internal mechanics.
fn claude_compaction(object: &Map<String, Value>) -> Option<Compaction> {
    if object.get("subtype").and_then(Value::as_str) != Some(CLAUDE_COMPACT_BOUNDARY) {
        return None;
    }
    let metadata = object.get("compactMetadata").and_then(Value::as_object);
    Some(Compaction {
        phase: CompactionPhase::Complete,
        trigger: metadata
            .and_then(|metadata| metadata.get("trigger"))
            .and_then(Value::as_str)
            .and_then(nonempty),
        // Claude Code writes a boundary only for a compaction that happened, and states no
        // outcome of its own; `None` says exactly that, and is not a failure.
        succeeded: None,
        pre_tokens: metadata.and_then(|metadata| token_count(metadata.get("preTokens"))),
        post_tokens: metadata.and_then(|metadata| token_count(metadata.get("postTokens"))),
        // The envelope states no breakdown of the pre-compaction context and no window size
        // to measure it against; absence here is the archive's, not a reading failure.
        system_tokens: None,
        conversation_tokens: None,
        tool_definition_tokens: None,
        token_limit: None,
    })
}

/// The token figures of a Claude Code `message.usage`, taking exactly the keys whose
/// meaning is pinned across every key set the archive holds.
///
/// Deliberately left in the raw record: `server_tool_use`, which counts vendor tool
/// invocations rather than this message's tokens, and `iterations`, which describes how the
/// message was served rather than what it was billed at. `speed` and `inference_geo` *are*
/// promoted, despite reading like serving detail, because both are rate multipliers a cost
/// consumer cannot recover from anywhere else — as are `cache_creation`'s two ephemeral
/// buckets, which the two cache TTLs bill at different rates and which therefore price a
/// cache write that its total cannot.
///
/// Nothing here is summed or derived — a figure is promoted as recorded or not at all, and
/// the older key sets that predate a key (no `output_tokens_details`; no `cache_creation`;
/// no `speed`) simply leave it absent. In particular the buckets are not reconciled against
/// `cache_creation_input_tokens`, nor it against them: they are two statements the source
/// makes, and the archive holds a message where they disagree.
fn claude_token_usage(usage: &Map<String, Value>) -> Option<TokenUsage> {
    let details = usage
        .get("output_tokens_details")
        .and_then(Value::as_object);
    let cache_creation = usage.get("cache_creation").and_then(Value::as_object);
    TokenUsage {
        input_tokens: token_count(usage.get("input_tokens")),
        output_tokens: token_count(usage.get("output_tokens")),
        cache_creation_input_tokens: token_count(usage.get("cache_creation_input_tokens")),
        cache_5m_input_tokens: cache_creation
            .and_then(|buckets| token_count(buckets.get("ephemeral_5m_input_tokens"))),
        cache_1h_input_tokens: cache_creation
            .and_then(|buckets| token_count(buckets.get("ephemeral_1h_input_tokens"))),
        cache_read_input_tokens: token_count(usage.get("cache_read_input_tokens")),
        thinking_tokens: details.and_then(|details| token_count(details.get("thinking_tokens"))),
        service_tier: usage
            .get("service_tier")
            .and_then(Value::as_str)
            .and_then(nonempty),
        speed: usage
            .get("speed")
            .and_then(Value::as_str)
            .and_then(nonempty),
        inference_geo: usage
            .get("inference_geo")
            .and_then(Value::as_str)
            .and_then(nonempty),
    }
    .recorded()
}

/// A token count, read only where the source recorded a non-negative integer. A string, a
/// float, a negative number, `null`, or an absent key all yield nothing: a wrong figure
/// corrupts a cost total silently, while an absent one is an under-claim the consumer sees.
fn token_count(value: Option<&Value>) -> Option<u64> {
    value?.as_u64()
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

    /// The meta a record promotes, read the way the stream reads it: independently of what
    /// the record's content classified as.
    fn meta_of(source: Source, json: &str) -> Option<AssistantMeta> {
        let value: Value = serde_json::from_str(json).unwrap();
        assistant_meta(source, value.as_object().unwrap())
    }

    /// The kinds of the events a record classifies to, for the cases that pin what its
    /// content became alongside what it was billed.
    fn event_kinds(source: Source, json: &str) -> Vec<&'static str> {
        match classify_json(source, json) {
            Class::Content(events) => events.iter().map(Event::kind).collect(),
            _ => Vec::new(),
        }
    }

    /// The meta a record is expected to promote, for the cases naming a model and an id.
    fn meta(model: &str, message_id: &str, usage: Option<TokenUsage>) -> Option<AssistantMeta> {
        Some(AssistantMeta {
            model: Some(model.to_owned()),
            usage,
            message_id: Some(message_id.to_owned()),
        })
    }

    #[test]
    fn claude_assistant_records_promote_the_whole_pinned_usage_key_set() {
        assert_eq!(
            meta_of(
                Source::ClaudeCode,
                r#"{"type":"assistant","message":{"role":"assistant","id":"msg_1","model":"claude-opus-synthetic","content":[{"type":"text","text":"Priced."}],"usage":{"input_tokens":120,"output_tokens":48,"cache_creation_input_tokens":1024,"cache_read_input_tokens":8192,"service_tier":"standard","speed":"fast","inference_geo":"us","cache_creation":{"ephemeral_5m_input_tokens":1024},"server_tool_use":{"web_search_requests":2},"iterations":[{"duration_ms":900}],"output_tokens_details":{"thinking_tokens":32}}}}"#,
            ),
            meta(
                "claude-opus-synthetic",
                "msg_1",
                Some(TokenUsage {
                    input_tokens: Some(120),
                    output_tokens: Some(48),
                    cache_creation_input_tokens: Some(1024),
                    // This record's `cache_creation` names only the 5-minute tier, so the
                    // 1-hour half stays absent rather than being inferred as the remainder.
                    cache_5m_input_tokens: Some(1024),
                    cache_1h_input_tokens: None,
                    cache_read_input_tokens: Some(8192),
                    thinking_tokens: Some(32),
                    service_tier: Some("standard".to_owned()),
                    speed: Some("fast".to_owned()),
                    inference_geo: Some("us".to_owned()),
                }),
            )
        );

        // An older key set, and the `<synthetic>` model placeholder passed through as the
        // model id it is. Nulls read as absent; zeroes are counts the source did report.
        assert_eq!(
            meta_of(
                Source::ClaudeCode,
                r#"{"type":"assistant","message":{"role":"assistant","id":"msg_2","model":"<synthetic>","content":[{"type":"text","text":"Local."}],"usage":{"input_tokens":0,"output_tokens":5,"service_tier":null,"speed":null,"output_tokens_details":null}}}"#,
            ),
            meta(
                "<synthetic>",
                "msg_2",
                Some(TokenUsage {
                    input_tokens: Some(0),
                    output_tokens: Some(5),
                    ..TokenUsage::default()
                }),
            )
        );
    }

    #[test]
    fn claude_usage_values_that_are_not_counts_are_left_absent_rather_than_guessed() {
        // A stringified count, a negative, a float, a null, and a details object that is
        // not an object: each field is dropped on its own, leaving the readable ones.
        assert_eq!(
            meta_of(
                Source::ClaudeCode,
                r#"{"type":"assistant","message":{"role":"assistant","id":"msg_3","model":"claude-opus-synthetic","content":[{"type":"text","text":"Odd."}],"usage":{"input_tokens":"120","output_tokens":-5,"cache_creation_input_tokens":1.5,"cache_read_input_tokens":null,"service_tier":"  ","output_tokens_details":7,"speed":"standard"}}}"#,
            ),
            meta(
                "claude-opus-synthetic",
                "msg_3",
                Some(TokenUsage {
                    speed: Some("standard".to_owned()),
                    ..TokenUsage::default()
                }),
            )
        );

        // A usage carrying nothing readable is no usage; a record carrying nothing at all
        // is no meta, which is how a consumer tells "not recorded" from "recorded as zero".
        assert_eq!(
            meta_of(
                Source::ClaudeCode,
                r#"{"type":"assistant","message":{"role":"assistant","id":"msg_4","model":"claude-opus-synthetic","content":[{"type":"text","text":"Empty."}],"usage":{}}}"#,
            ),
            meta("claude-opus-synthetic", "msg_4", None)
        );
        assert_eq!(
            meta_of(
                Source::ClaudeCode,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Bare."}],"usage":"none"}}"#,
            ),
            None
        );
    }

    /// The two cache tiers bill at different multiples of the base input rate, so both
    /// halves of `usage.cache_creation` are read — under the same discipline as every other
    /// figure, one key at a time, and never reconciled against their total.
    #[test]
    fn claude_cache_creation_buckets_are_read_per_tier() {
        let usage = |json: &str| {
            meta_of(Source::ClaudeCode, json)
                .and_then(|meta| meta.usage)
                .unwrap_or_default()
        };
        let with_cache_creation = |buckets: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","id":"msg_1","model":"claude-opus-synthetic","content":[{{"type":"text","text":"Cached."}}],"usage":{{"cache_creation_input_tokens":1024{buckets}}}}}}}"#
            )
        };

        // Both tiers, each from its own key: a transposition fails this.
        let both = usage(&with_cache_creation(
            r#","cache_creation":{"ephemeral_5m_input_tokens":300,"ephemeral_1h_input_tokens":724}"#,
        ));
        assert_eq!(both.cache_creation_input_tokens, Some(1024));
        assert_eq!(both.cache_5m_input_tokens, Some(300));
        assert_eq!(both.cache_1h_input_tokens, Some(724));

        // The archive's own shape — every write on the 1-hour tier — where the 5-minute
        // zero is a figure the source reported and not an absence.
        let archive_shape = usage(&with_cache_creation(
            r#","cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":1024}"#,
        ));
        assert_eq!(archive_shape.cache_5m_input_tokens, Some(0));
        assert_eq!(archive_shape.cache_1h_input_tokens, Some(1024));

        // The buckets are never reconciled with the total, in either direction: a message
        // whose total says 0 and whose 1-hour bucket says 2,277 is promoted as it stands,
        // which is a shape the archive holds.
        let drift = usage(
            r#"{"type":"assistant","message":{"role":"assistant","id":"msg_2","model":"claude-opus-synthetic","content":[{"type":"text","text":"Drift."}],"usage":{"cache_creation_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2277}}}}"#,
        );
        assert_eq!(drift.cache_creation_input_tokens, Some(0));
        assert_eq!(drift.cache_1h_input_tokens, Some(2277));

        // An absent object, an object missing a key, values that are not counts, and a
        // `cache_creation` that is not an object at all: each unreadable half is left
        // absent on its own, and the total beside it is unaffected.
        for buckets in [
            "",
            r#","cache_creation":{}"#,
            r#","cache_creation":{"ephemeral_5m_input_tokens":null,"ephemeral_1h_input_tokens":"724"}"#,
            r#","cache_creation":{"ephemeral_5m_input_tokens":-1,"ephemeral_1h_input_tokens":1.5}"#,
            r#","cache_creation":7"#,
            r#","cache_creation":null"#,
        ] {
            let usage = usage(&with_cache_creation(buckets));
            assert_eq!(usage.cache_creation_input_tokens, Some(1024), "{buckets}");
            assert_eq!(usage.cache_5m_input_tokens, None, "{buckets}");
            assert_eq!(usage.cache_1h_input_tokens, None, "{buckets}");
        }

        // One readable half does not require the other.
        let half = usage(&with_cache_creation(
            r#","cache_creation":{"ephemeral_1h_input_tokens":724}"#,
        ));
        assert_eq!(half.cache_5m_input_tokens, None);
        assert_eq!(half.cache_1h_input_tokens, Some(724));

        // Buckets alone, with no total, are still a reading: the two are independent.
        let bucketed = usage(
            r#"{"type":"assistant","message":{"role":"assistant","id":"msg_3","model":"claude-opus-synthetic","content":[{"type":"text","text":"Bucketed."}],"usage":{"cache_creation":{"ephemeral_5m_input_tokens":16,"ephemeral_1h_input_tokens":0}}}}"#,
        );
        assert_eq!(bucketed.cache_creation_input_tokens, None);
        assert_eq!(bucketed.cache_5m_input_tokens, Some(16));
        assert_eq!(bucketed.cache_1h_input_tokens, Some(0));
    }

    /// The reason the meta hangs off the record: a message is billed for producing its
    /// content, whatever that content turns out to be, so nothing about how the content
    /// classifies may gate whether its tokens are reachable.
    #[test]
    fn a_records_usage_is_reachable_whatever_its_content_classifies_as() {
        let usage = Some(TokenUsage {
            input_tokens: Some(64),
            output_tokens: Some(16),
            ..TokenUsage::default()
        });
        for (json, kinds) in [
            // Text, a tool call, and more text: three events, one message, one meta. There
            // is no principled way to divide a message's cost across its content blocks,
            // and with the meta on the record there is nothing to divide.
            (
                r#"{"type":"assistant","message":{"role":"assistant","id":"msg_5","model":"claude-opus-synthetic","content":[{"type":"text","text":"First."},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}},{"type":"text","text":"Second."}],"usage":{"input_tokens":64,"output_tokens":16}}}"#,
                vec!["assistant", "tool", "assistant"],
            ),
            // The common agentic shape: a message that only calls tools. It yields no
            // assistant event at all, and messages like it hold most of the archive's
            // token mass.
            (
                r#"{"type":"assistant","message":{"role":"assistant","id":"msg_5","model":"claude-opus-synthetic","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"src/lib.rs"}}],"usage":{"input_tokens":64,"output_tokens":16}}}"#,
                vec!["tool"],
            ),
            // Recognized but empty (a thinking-only message), and unreadable content: no
            // events either way, and both were still billed.
            (
                r#"{"type":"assistant","message":{"role":"assistant","id":"msg_5","model":"claude-opus-synthetic","content":[{"type":"thinking","thinking":"..."}],"usage":{"input_tokens":64,"output_tokens":16}}}"#,
                vec![],
            ),
            (
                r#"{"type":"assistant","message":{"role":"assistant","id":"msg_5","model":"claude-opus-synthetic","content":7,"usage":{"input_tokens":64,"output_tokens":16}}}"#,
                vec![],
            ),
        ] {
            assert_eq!(event_kinds(Source::ClaudeCode, json), kinds, "{json}");
            // Every one of the four is the same `msg_5` repeating its usage: a consumer
            // adding them up bills 64 input tokens four times over.
            assert_eq!(
                meta_of(Source::ClaudeCode, json),
                meta("claude-opus-synthetic", "msg_5", usage.clone()),
                "{json}"
            );
        }

        // A user record is read as a user record, whatever its message names: usage on it,
        // if a future envelope ever writes one, describes no assistant message.
        assert_eq!(
            meta_of(
                Source::ClaudeCode,
                r#"{"type":"user","message":{"role":"user","id":"msg_6","model":"claude-opus-synthetic","content":"Do it.","usage":{"input_tokens":9}}}"#,
            ),
            None
        );
    }

    #[test]
    fn copilot_assistant_messages_promote_the_model_and_the_one_count_they_record() {
        assert_eq!(
            meta_of(
                Source::Copilot,
                r#"{"type":"assistant.message","data":{"content":"Priced.","messageId":"m1","model":"claude-opus-4.8","outputTokens":128,"turnId":"t1"}}"#,
            ),
            meta(
                "claude-opus-4.8",
                "m1",
                Some(TokenUsage {
                    output_tokens: Some(128),
                    ..TokenUsage::default()
                }),
            )
        );
        // No count, and a count that is not a number: the message id still identifies the
        // message, and no input or cache figure is invented for an envelope that has none.
        assert_eq!(
            meta_of(
                Source::Copilot,
                r#"{"type":"assistant.message","data":{"content":"Uncounted.","messageId":"m2","model":"gpt-5.6-sol"}}"#,
            ),
            meta("gpt-5.6-sol", "m2", None)
        );
        assert_eq!(
            meta_of(
                Source::Copilot,
                r#"{"type":"assistant.message","data":{"content":"Stringy.","messageId":"m3","outputTokens":"128"}}"#,
            ),
            Some(AssistantMeta {
                model: None,
                usage: None,
                message_id: Some("m3".to_owned()),
            })
        );

        // The kinds that also name a model stay bookkeeping *and* promote nothing: a
        // session-wide token roll-up attributes to no message, and the tool records the CLI
        // writes between messages carry no usage of their own to read.
        for json in [
            r#"{"type":"session.model_change","data":{"model":"claude-opus-4.8","previousModel":"gpt-5.6-sol"}}"#,
            r#"{"type":"session.shutdown","data":{"shutdownType":"routine","modelMetrics":{"claude-opus-4.8":{"outputTokens":900}}}}"#,
        ] {
            assert!(matches!(
                classify_json(Source::Copilot, json),
                Class::Ignored(_)
            ));
            assert_eq!(meta_of(Source::Copilot, json), None, "{json}");
        }
        assert_eq!(
            meta_of(
                Source::Copilot,
                r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"bash","arguments":{"command":"ls"},"model":"claude-opus-4.8"}}"#,
            ),
            None
        );
    }

    /// The compaction a record promotes, read the way the stream reads it.
    fn compaction_of(source: Source, json: &str) -> Option<Compaction> {
        let value: Value = serde_json::from_str(json).unwrap();
        compaction(source, value.as_object().unwrap())
    }

    /// A marker with no readable figure: what a compaction record still states.
    fn marker(phase: CompactionPhase) -> Compaction {
        Compaction {
            phase,
            trigger: None,
            succeeded: None,
            pre_tokens: None,
            post_tokens: None,
            system_tokens: None,
            conversation_tokens: None,
            tool_definition_tokens: None,
            token_limit: None,
        }
    }

    /// The `system` record kind is bookkeeping wholesale — 7,895 records across the mirror
    /// cache — and only its `compact_boundary` subtype marks a compaction. The subtype is
    /// what certifies the meaning of `compactMetadata`, exactly as a tool's *name* and not
    /// its keys certifies the meaning of `command`.
    #[test]
    fn only_the_compact_boundary_subtype_of_a_claude_system_record_is_a_compaction() {
        assert_eq!(
            compaction_of(
                Source::ClaudeCode,
                r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted","compactMetadata":{"trigger":"manual","preTokens":214864,"postTokens":8156,"cumulativeDroppedTokens":206708,"durationMs":97954}}"#,
            ),
            Some(Compaction {
                trigger: Some("manual".to_owned()),
                pre_tokens: Some(214864),
                post_tokens: Some(8156),
                ..marker(CompactionPhase::Complete)
            })
        );

        // Another subtype, no subtype, and a subtype that is not a string: none of them is a
        // boundary, whatever key they carry.
        for json in [
            r#"{"type":"system","subtype":"local_command_stdout","compactMetadata":{"preTokens":9}}"#,
            r#"{"type":"system","compactMetadata":{"preTokens":9}}"#,
            r#"{"type":"system","subtype":7,"compactMetadata":{"preTokens":9}}"#,
        ] {
            assert_eq!(compaction_of(Source::ClaudeCode, json), None, "{json}");
        }

        // Neither is the post-compaction summary that follows a boundary: it is already a
        // user event, and flagging it would put a second marker on one compaction.
        assert_eq!(
            compaction_of(
                Source::ClaudeCode,
                r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context."}}"#,
            ),
            None
        );
    }

    /// A boundary states that a compaction happened whether or not its figures are readable,
    /// which is the difference between this type and [`AssistantMeta`]: there, a record with
    /// nothing readable made no claim; here, the record *is* the claim.
    #[test]
    fn a_claude_boundary_with_unreadable_metadata_still_marks_a_compaction() {
        for json in [
            // A string count, a negative, a float, and a blank trigger.
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"   ","preTokens":"214864","postTokens":-1,"cumulativeDroppedTokens":1.5}}"#,
            // `compactMetadata` that is not an object, and none at all.
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":7}"#,
            r#"{"type":"system","subtype":"compact_boundary","compactMetadata":null}"#,
            r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted"}"#,
        ] {
            assert_eq!(
                compaction_of(Source::ClaudeCode, json),
                Some(marker(CompactionPhase::Complete)),
                "{json}"
            );
        }

        // One figure readable and the other not: each is dropped on its own.
        assert_eq!(
            compaction_of(
                Source::ClaudeCode,
                r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"preTokens":214864,"postTokens":"8156"}}"#,
            ),
            Some(Compaction {
                pre_tokens: Some(214864),
                ..marker(CompactionPhase::Complete)
            })
        );
    }

    /// Copilot spells `systemTokens` / `conversationTokens` / `toolDefinitionsTokens` on both
    /// halves, meaning the pre-compaction context on a start and the post-compaction one on a
    /// completion. Reading them by key alone would file a figure as its own opposite, so the
    /// promotion is keyed per phase — and a transposition fails this test.
    #[test]
    fn copilot_context_components_are_read_on_a_start_and_never_on_a_completion() {
        assert_eq!(
            compaction_of(
                Source::Copilot,
                r#"{"type":"session.compaction_start","data":{"systemTokens":19138,"conversationTokens":129018,"toolDefinitionsTokens":12783}}"#,
            ),
            Some(Compaction {
                system_tokens: Some(19138),
                conversation_tokens: Some(129018),
                tool_definition_tokens: Some(12783),
                ..marker(CompactionPhase::Start)
            })
        );

        // The archive's one completion carrying the same keys: 11,189 conversation tokens
        // are what it was *left* with, against the 403,971 that went in.
        assert_eq!(
            compaction_of(
                Source::Copilot,
                r#"{"type":"session.compaction_complete","data":{"success":true,"preCompactionTokens":403971,"postCompactionTokens":11193,"systemTokens":10271,"conversationTokens":11189,"toolDefinitionsTokens":16516}}"#,
            ),
            Some(Compaction {
                succeeded: Some(true),
                pre_tokens: Some(403971),
                post_tokens: Some(11193),
                ..marker(CompactionPhase::Complete)
            })
        );

        // Symmetrically, a start's `preCompactionTokens` is not a key that envelope writes,
        // and is not read as the start's total either — only `currentTokens` is.
        assert_eq!(
            compaction_of(
                Source::Copilot,
                r#"{"type":"session.compaction_start","data":{"systemTokens":11422,"conversationTokens":188577,"toolDefinitionsTokens":18151,"currentTokens":218150,"tokenLimit":272000,"trigger":"threshold","model":"claude-synthetic","preCompactionTokens":999999}}"#,
            ),
            Some(Compaction {
                trigger: Some("threshold".to_owned()),
                pre_tokens: Some(218150),
                system_tokens: Some(11422),
                conversation_tokens: Some(188577),
                tool_definition_tokens: Some(18151),
                token_limit: Some(272000),
                ..marker(CompactionPhase::Start)
            })
        );
    }

    #[test]
    fn a_failed_copilot_compaction_is_marked_as_one_and_invents_no_figures() {
        assert_eq!(
            compaction_of(
                Source::Copilot,
                r#"{"type":"session.compaction_complete","data":{"success":false,"error":"background compaction summarizer did not settle within 60s","tokenLimit":200000,"trigger":"threshold"}}"#,
            ),
            Some(Compaction {
                trigger: Some("threshold".to_owned()),
                succeeded: Some(false),
                token_limit: Some(200000),
                ..marker(CompactionPhase::Complete)
            })
        );

        // The oldest failure shape names nothing but the error.
        assert_eq!(
            compaction_of(
                Source::Copilot,
                r#"{"type":"session.compaction_complete","data":{"success":false,"error":"Error: fetch failed"}}"#,
            ),
            Some(Compaction {
                succeeded: Some(false),
                ..marker(CompactionPhase::Complete)
            })
        );

        // A `success` that is not a boolean is not an outcome; `None` is neither failure nor
        // success, and the attempt is still marked.
        assert_eq!(
            compaction_of(
                Source::Copilot,
                r#"{"type":"session.compaction_complete","data":{"success":"true"}}"#,
            ),
            Some(marker(CompactionPhase::Complete))
        );
        // As is a marker whose `data` is missing or unreadable.
        for json in [
            r#"{"type":"session.compaction_complete","data":"unreadable"}"#,
            r#"{"type":"session.compaction_complete"}"#,
        ] {
            assert_eq!(
                compaction_of(Source::Copilot, json),
                Some(marker(CompactionPhase::Complete)),
                "{json}"
            );
        }
    }

    /// Neighbouring kinds carrying compaction-shaped keys are not compaction markers, and
    /// every marker stays the ignored bookkeeping it has always been.
    #[test]
    fn compaction_markers_are_read_without_leaving_their_census() {
        for json in [
            r#"{"type":"session.compaction_start","data":{"systemTokens":1}}"#,
            r#"{"type":"session.compaction_complete","data":{"success":true,"preCompactionTokens":1}}"#,
        ] {
            assert!(
                matches!(classify_json(Source::Copilot, json), Class::Ignored(_)),
                "{json}"
            );
            assert!(compaction_of(Source::Copilot, json).is_some(), "{json}");
        }
        assert!(matches!(
            classify_json(
                Source::ClaudeCode,
                r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"preTokens":1}}"#
            ),
            Class::Ignored(kind) if kind == "system"
        ));

        // Kinds that describe the context without marking a compaction.
        for json in [
            r#"{"type":"session.context_changed","data":{"reason":"compaction","preCompactionTokens":9,"tokenLimit":272000}}"#,
            r#"{"type":"session.usage_checkpoint","data":{"tokensUsed":123}}"#,
            r#"{"type":"session.truncation","data":{"tokensRemoved":9}}"#,
        ] {
            assert_eq!(compaction_of(Source::Copilot, json), None, "{json}");
        }
    }

    #[test]
    fn codex_records_promote_no_compaction() {
        // The rollout schema names a `compacted` metadata record and the archive holds zero
        // Codex sessions, so this crate has never seen one of its payloads. Nothing is
        // guessed at: an absent field is an under-claim a consumer can see, an invented one
        // is a wrong number it cannot.
        for json in [
            r#"{"type":"compacted","payload":{"message":"Prior context compacted."}}"#,
            r#"{"type":"turn_context","payload":{"cwd":"/work","model":"gpt-synthetic"}}"#,
        ] {
            assert_eq!(compaction_of(Source::Codex, json), None, "{json}");
        }
    }

    #[test]
    fn codex_records_promote_nothing() {
        // The rollout records its model on `turn_context`, a turn-level record this crate
        // ignores, and no per-message usage anywhere: nothing attributes to a message.
        for json in [
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}}"#,
            r#"{"type":"turn_context","payload":{"cwd":"/work","model":"gpt-synthetic","effort":"medium"}}"#,
        ] {
            assert_eq!(meta_of(Source::Codex, json), None, "{json}");
        }
    }
}
