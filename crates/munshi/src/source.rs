use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_EVENT_TEXT_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone)]
pub struct SessionReference {
    pub session_id: Option<String>,
    pub events_path: Option<PathBuf>,
    pub copilot_home: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSession {
    pub session_id: String,
    pub events_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedEvent {
    pub kind: &'static str,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct NormalizedSession {
    pub session_id: String,
    pub events: Vec<NormalizedEvent>,
    pub user_requests: usize,
    pub assistant_messages: usize,
    pub tool_activities: usize,
    pub ignored_events: usize,
    pub source_cursor: u64,
    pub source_hash: String,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
}

impl NormalizedSession {
    pub fn is_archive_worthy(&self) -> bool {
        self.user_requests > 0 && (self.assistant_messages > 0 || self.tool_activities > 0)
    }
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("provide a session ID, an explicit events.jsonl path, or both")]
    MissingReference,
    #[error("session ID is not a safe supported identifier")]
    InvalidSessionId,
    #[error("explicit transcript path must name a regular events.jsonl file")]
    UnsupportedTranscriptPath,
    #[error("session ID does not match the events.jsonl parent directory")]
    SessionIdMismatch,
    #[error("COPILOT_HOME or HOME is required when resolving a session ID")]
    MissingCopilotHome,
    #[error("session transcript could not be resolved")]
    TranscriptNotFound,
    #[error("resolved session transcript escapes the supported Copilot session-state directory")]
    UnsafeResolvedPath,
    #[error("transcript I/O failed")]
    Io(#[source] io::Error),
    #[error("transcript exceeds the configured {limit}-byte source limit")]
    SourceLimit { limit: usize },
    #[error("transcript line {line} is not valid JSON")]
    MalformedJson { line: u64 },
    #[error("normalized event content exceeds the per-event safety limit")]
    EventContentLimit,
}

pub fn resolve_session_reference(
    reference: &SessionReference,
) -> Result<ResolvedSession, SourceError> {
    if reference.session_id.is_none() && reference.events_path.is_none() {
        return Err(SourceError::MissingReference);
    }

    let supplied_id = reference
        .session_id
        .as_deref()
        .map(validate_session_id)
        .transpose()?;

    if let Some(path) = &reference.events_path {
        if path.file_name().and_then(|name| name.to_str()) != Some("events.jsonl") {
            return Err(SourceError::UnsupportedTranscriptPath);
        }
        let metadata = std::fs::symlink_metadata(path).map_err(SourceError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SourceError::UnsupportedTranscriptPath);
        }
        let canonical = path.canonicalize().map_err(SourceError::Io)?;
        let derived_id = canonical
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .ok_or(SourceError::InvalidSessionId)
            .and_then(validate_session_id)?;
        if supplied_id.is_some_and(|session_id| session_id != derived_id) {
            return Err(SourceError::SessionIdMismatch);
        }
        return Ok(ResolvedSession {
            session_id: supplied_id.unwrap_or(derived_id).to_owned(),
            events_path: canonical,
        });
    }

    let session_id = supplied_id.expect("a session ID is present when no path was supplied");
    let home = reference
        .copilot_home
        .clone()
        .or_else(|| std::env::var_os("COPILOT_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".copilot")))
        .ok_or(SourceError::MissingCopilotHome)?;
    let session_state = home.join("session-state");
    let expected_directory = session_state.join(session_id);
    let candidate = expected_directory.join("events.jsonl");
    if !candidate.exists() {
        return Err(SourceError::TranscriptNotFound);
    }
    let canonical_state = session_state
        .canonicalize()
        .map_err(|_| SourceError::TranscriptNotFound)?;
    let canonical_directory = expected_directory
        .canonicalize()
        .map_err(|_| SourceError::TranscriptNotFound)?;
    let canonical = candidate
        .canonicalize()
        .map_err(|_| SourceError::TranscriptNotFound)?;
    if !canonical_directory.starts_with(&canonical_state)
        || !canonical.starts_with(&canonical_directory)
        || canonical.file_name().and_then(|name| name.to_str()) != Some("events.jsonl")
        || !canonical.is_file()
    {
        return Err(SourceError::UnsafeResolvedPath);
    }

    Ok(ResolvedSession {
        session_id: session_id.to_owned(),
        events_path: canonical,
    })
}

pub fn load_session(
    resolved: &ResolvedSession,
    max_source_bytes: usize,
) -> Result<NormalizedSession, SourceError> {
    let mut bytes = Vec::new();
    File::open(&resolved.events_path)
        .map_err(SourceError::Io)?
        .take(max_source_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(SourceError::Io)?;
    if bytes.len() > max_source_bytes {
        return Err(SourceError::SourceLimit {
            limit: max_source_bytes,
        });
    }

    let source_hash = format!("sha256:{:x}", Sha256::digest(&bytes));
    let mut events = Vec::new();
    let mut user_requests = 0;
    let mut assistant_messages = 0;
    let mut tool_activities = 0;
    let mut ignored_events = 0;
    let mut line_count = 0_u64;
    let mut first_timestamp: Option<DateTime<Utc>> = None;
    let mut last_timestamp: Option<DateTime<Utc>> = None;

    for raw_line in bytes.split(|byte| *byte == b'\n') {
        if raw_line.is_empty() {
            continue;
        }
        line_count += 1;
        let value: Value = serde_json::from_slice(raw_line)
            .map_err(|_| SourceError::MalformedJson { line: line_count })?;
        let Some(object) = value.as_object() else {
            ignored_events += 1;
            continue;
        };

        if let Some(timestamp) = object.get("timestamp").and_then(parse_timestamp) {
            first_timestamp = Some(first_timestamp.map_or(timestamp, |old| old.min(timestamp)));
            last_timestamp = Some(last_timestamp.map_or(timestamp, |old| old.max(timestamp)));
        }

        let Some(event_type) = object.get("type").and_then(Value::as_str) else {
            ignored_events += 1;
            continue;
        };
        match event_type {
            "user.message" => {
                let Some(data) = event_data(object) else {
                    ignored_events += 1;
                    continue;
                };
                let Some(content) = data.get("content").and_then(Value::as_str) else {
                    ignored_events += 1;
                    continue;
                };
                if let Some(content) = nonempty(content) {
                    user_requests += 1;
                    events.push(NormalizedEvent {
                        kind: "user",
                        content: validate_content(content)?,
                    });
                }
            }
            "assistant.message" => {
                let Some(data) = event_data(object) else {
                    ignored_events += 1;
                    continue;
                };
                if !data.get("messageId").is_some_and(Value::is_string) {
                    ignored_events += 1;
                    continue;
                }
                let Some(content) = data.get("content").and_then(Value::as_str) else {
                    ignored_events += 1;
                    continue;
                };
                if let Some(content) = nonempty(content) {
                    assistant_messages += 1;
                    events.push(NormalizedEvent {
                        kind: "assistant",
                        content: validate_content(content)?,
                    });
                }
            }
            "tool.execution_start" => {
                let Some(data) = event_data(object).filter(|data| valid_tool_start(data)) else {
                    ignored_events += 1;
                    continue;
                };
                tool_activities += 1;
                events.push(NormalizedEvent {
                    kind: "tool",
                    content: extract_tool_start(data)?,
                });
            }
            "tool.execution_complete" => {
                let Some(data) = event_data(object).filter(|data| valid_tool_complete(data)) else {
                    ignored_events += 1;
                    continue;
                };
                tool_activities += 1;
                events.push(NormalizedEvent {
                    kind: "tool",
                    content: extract_tool_complete(data)?,
                });
            }
            _ => ignored_events += 1,
        }
    }

    Ok(NormalizedSession {
        session_id: resolved.session_id.clone(),
        events,
        user_requests,
        assistant_messages,
        tool_activities,
        ignored_events,
        source_cursor: line_count,
        source_hash,
        started_at: first_timestamp.map(format_timestamp),
        updated_at: last_timestamp.map(format_timestamp),
    })
}

fn event_data(object: &Map<String, Value>) -> Option<&Map<String, Value>> {
    object.get("data").and_then(Value::as_object)
}

fn validate_session_id(value: &str) -> Result<&str, SourceError> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Err(SourceError::InvalidSessionId)
    } else {
        Ok(value)
    }
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn nonempty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn validate_content(content: String) -> Result<String, SourceError> {
    if content.len() > MAX_EVENT_TEXT_BYTES {
        Err(SourceError::EventContentLimit)
    } else {
        Ok(content)
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

fn extract_tool_start(data: &Map<String, Value>) -> Result<String, SourceError> {
    let mut fields = BTreeMap::new();
    fields.insert("event", "tool.execution_start".to_owned());
    fields.insert(
        "tool_call_id",
        data["toolCallId"]
            .as_str()
            .expect("validated tool call ID")
            .to_owned(),
    );
    fields.insert(
        "name",
        data["toolName"]
            .as_str()
            .expect("validated tool name")
            .to_owned(),
    );
    if let Some(arguments) = data.get("arguments").and_then(compact_value) {
        fields.insert("arguments", arguments);
    }
    render_tool_fields(fields)
}

fn extract_tool_complete(data: &Map<String, Value>) -> Result<String, SourceError> {
    let mut fields = BTreeMap::new();
    fields.insert("event", "tool.execution_complete".to_owned());
    fields.insert(
        "tool_call_id",
        data["toolCallId"]
            .as_str()
            .expect("validated tool call ID")
            .to_owned(),
    );
    fields.insert(
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
        fields.insert("error", message);
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
            fields.insert("output", output.join("\n"));
        }
    }
    render_tool_fields(fields)
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

fn render_tool_fields(fields: BTreeMap<&str, String>) -> Result<String, SourceError> {
    validate_content(
        fields
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" "),
    )
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
