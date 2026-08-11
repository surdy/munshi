//! Streaming, lossless interpretation of coding-agent transcripts (ADR 0011, issue #26).
//!
//! What an archived transcript *means* is decided when it is read, not when it is captured
//! (`docs/adr/0011-interpret-transcripts-at-read-time-through-a-shared-streaming-crate.md`).
//! This crate holds the version-pinned format knowledge for the Copilot CLI, Claude Code,
//! and Codex rollout transcript envelopes, keyed by [`Source`] and the
//! `artifact_set_version` that capture provenance already carries, so format knowledge
//! stays in exactly one place as agents and artifact sets evolve.
//!
//! # The lossless contract
//!
//! [`TranscriptStream`] takes any [`std::io::BufRead`] and yields exactly one item per
//! non-empty transcript line — every record is accounted for exactly once:
//!
//! - a parseable record becomes a [`Record`] whose [`Classification`] is either typed
//!   [`Event`]s carrying **complete** content (nothing is elided or truncated here; size
//!   policies such as claim-ticket elision belong to consumers), a recognized-but-empty
//!   marker ([`Classification::Empty`]), a deliberately-unarchived record kind
//!   ([`Classification::Ignored`]), or a record the parser does not recognize at all
//!   ([`Classification::Unknown`], carrying the raw record so interpretation gaps stay
//!   inspectable);
//! - a malformed line — including an incomplete trailing record — becomes a per-record
//!   [`RecordError`] item, never an aborted parse; the stream continues past it.
//!
//! [`SessionSummary`] restates the legacy `NormalizedSession` counting fold on top of the
//! stream: `user_requests` / `assistant_messages` / `tool_activities` count content events
//! by kind, `ignored_events` counts [`Classification::Ignored`] plus
//! [`Classification::Unknown`] records (matching the historical lumped count), and
//! `started_at` / `updated_at` are the minimum/maximum top-level record `timestamp`.
//!
//! [`envelope_matches`], [`claude_origin_cwd`], [`claude_git_branch`], and
//! [`claude_agent_version`] expose the pure, privacy-safe envelope predicates behind `munshi`'s
//! transcript validation, Claude Code origin recovery (issues #27, #40), and resume restore
//! (issue #71); the bounded-I/O wrappers around them stay in `munshi`.

use std::collections::BTreeMap;
use std::io::{self, BufRead};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

mod classify;
mod envelope;

pub use envelope::{claude_agent_version, claude_git_branch, claude_origin_cwd, envelope_matches};

/// The oldest artifact-set version any source supports.
pub const MIN_SUPPORTED_ARTIFACT_SET_VERSION: u16 = 1;
/// The newest artifact-set version any source supports. Version 2 (munshi issue #23) added
/// optional `sidecar/<path>` snapshot artifacts without changing transcript interpretation, so
/// versions 1..=2 share one interpreter per source. [`TranscriptStream::new`] rejects versions
/// outside the range so future artifact sets fail loudly instead of being misinterpreted with
/// stale assumptions.
pub const SUPPORTED_ARTIFACT_SET_VERSION: u16 = 2;

/// The [`Classification::Ignored`] kind recorded for a line that is valid JSON but not a
/// JSON object. No transcript envelope produces such records, but the historical
/// normalizer tolerated and counted them as ignored, and so does this crate.
pub const NON_OBJECT_JSON_KIND: &str = "non-object-json";

/// Vendor-neutral identity of the coding-agent harness that produced a transcript.
///
/// Deliberately defined here rather than borrowed from the `munshi` crate so that this
/// crate stays standalone for external consumers; the serialized form (kebab-case) matches
/// `munshi::SourceKind`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// GitHub Copilot CLI (`events.jsonl`), version-pinned to 1.0.70.
    #[default]
    Copilot,
    /// Anthropic Claude Code session transcripts (`<uuid>.jsonl`), pinned to 2.1.44 and
    /// structurally re-validated at 2.1.205.
    ClaudeCode,
    /// OpenAI Codex CLI rollout files (`rollout-*.jsonl`).
    Codex,
}

/// A parser-selection error: the requested `(source, artifact_set_version)` pair has no
/// interpreter in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error(
    "artifact set version {version} of source {source_kind:?} is not supported \
     (supported: {MIN_SUPPORTED_ARTIFACT_SET_VERSION}..={SUPPORTED_ARTIFACT_SET_VERSION})"
)]
pub struct UnsupportedVersion {
    // Named `source_kind` rather than `source` so thiserror does not treat the field as
    // the error's source.
    pub source_kind: Source,
    pub version: u16,
}

/// A per-record failure. Errors are stream items, not aborts: the malformed line is
/// carried raw for inspection and iteration continues with the next line (except after an
/// I/O error, which ends the stream).
#[derive(Debug, Error)]
pub enum RecordError {
    /// The line is not valid JSON — including a truncated, incomplete trailing record.
    #[error("transcript record {record} (line {line}) is not valid JSON")]
    MalformedJson {
        /// 1-based physical line number.
        line: u64,
        /// 1-based ordinal among non-empty lines (the legacy record numbering).
        record: u64,
        /// The raw line bytes, without the trailing newline.
        raw: Vec<u8>,
    },
    /// Reading from the underlying reader failed. The stream ends after this item.
    #[error("transcript I/O failed at line {line}")]
    Io {
        /// 1-based physical line number at which the read failed.
        line: u64,
        #[source]
        source: io::Error,
    },
}

impl RecordError {
    /// The 1-based physical line number the error occurred on.
    pub fn line(&self) -> u64 {
        match self {
            Self::MalformedJson { line, .. } | Self::Io { line, .. } => *line,
        }
    }
}

/// One successfully parsed transcript record.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// 1-based physical line number in the transcript.
    pub line: u64,
    /// 1-based ordinal among non-empty lines (the legacy record numbering).
    pub record: u64,
    /// The record's top-level `timestamp` value, verbatim, when present.
    pub raw_timestamp: Option<Value>,
    /// `raw_timestamp` parsed as RFC 3339 and normalized to UTC, when it is one.
    pub timestamp: Option<DateTime<Utc>>,
    /// What the record means to the source's version-pinned envelope.
    pub classification: Classification,
}

/// The read-time meaning of one transcript record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// The record carries archive-worthy conversation content.
    Content { events: Vec<Event> },
    /// The record is a recognized content record whose content is empty or blank. The
    /// legacy normalizer counted these nowhere — neither as events nor as ignored.
    Empty,
    /// The format recognizes this record kind but deliberately does not archive it
    /// (bookkeeping, session metadata, model reasoning, non-object JSON). `kind` names
    /// the record type so consumers can see *what* was set aside.
    Ignored { kind: String },
    /// The parser does not recognize this record at all. The raw record is carried so a
    /// lossless reader surfaces interpretation gaps instead of hiding them.
    Unknown { raw: String },
}

/// One fully-typed, full-content conversation event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A user request; `text` is the complete user-authored content.
    User { text: String },
    /// An assistant reply; `text` is the complete assistant-authored content.
    Assistant { text: String },
    /// Tool activity (invocation or result), with structured fields.
    Tool(ToolEvent),
}

impl Event {
    /// The legacy normalized-event kind string (`user` / `assistant` / `tool`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::Tool(_) => "tool",
        }
    }

    /// The exact content string the legacy `munshi` normalizer builds for this event:
    /// the message text for user/assistant events, and the sorted `key=value` rendering
    /// for tool events. Byte-identical to `NormalizedEvent.content` (pre-elision).
    pub fn legacy_content(&self) -> String {
        match self {
            Self::User { text } | Self::Assistant { text } => text.clone(),
            Self::Tool(tool) => tool.rendered(),
        }
    }
}

/// Structured fields of a tool event, keyed exactly as the legacy renderer keys them
/// (`event`, `name`, `tool_use_id` / `tool_call_id` / `call_id`, `arguments` / `input`,
/// `output`, `success`, `error`, `is_error`, `command`), plus the Copilot tool-activity
/// keys added by issue #51 (`request_id` correlating `external_tool.requested` /
/// `external_tool.completed`, and the `skill.invoked` card fields `path`, `description`,
/// `source`, `trigger`, `model`, `content`). The map is ordered so the legacy rendering
/// is reproducible byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEvent {
    pub fields: BTreeMap<String, String>,
}

impl ToolEvent {
    /// The source-specific event discriminator (`tool_use`, `tool_result`,
    /// `tool.execution_start`, `tool.user_requested`, `skill.invoked`,
    /// `external_tool.requested`, `external_tool.completed`, `function_call`,
    /// `local_shell_call`, ...). Always present.
    pub fn event(&self) -> Option<&str> {
        self.fields.get("event").map(String::as_str)
    }

    /// The tool name, when the source records one.
    pub fn name(&self) -> Option<&str> {
        self.fields.get("name").map(String::as_str)
    }

    /// The tool invocation correlation id, whichever key the source uses.
    pub fn call_id(&self) -> Option<&str> {
        ["tool_use_id", "tool_call_id", "call_id"]
            .iter()
            .find_map(|key| self.fields.get(*key).map(String::as_str))
    }

    /// The legacy space-joined `key=value` rendering, sorted by key — byte-identical to
    /// the `NormalizedEvent.content` string the `munshi` normalizer produces.
    pub fn rendered(&self) -> String {
        self.fields
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A streaming transcript reader: an [`Iterator`] of `Result<Record, RecordError>` over
/// any buffered reader, yielding one item per non-empty transcript line.
pub struct TranscriptStream<R> {
    reader: R,
    source: Source,
    buffer: Vec<u8>,
    line: u64,
    record: u64,
    done: bool,
}

impl<R: BufRead> TranscriptStream<R> {
    /// Selects the interpreter for `(source, artifact_set_version)` over `reader`.
    /// Unknown artifact-set versions are rejected up front.
    pub fn new(
        source: Source,
        artifact_set_version: u16,
        reader: R,
    ) -> Result<Self, UnsupportedVersion> {
        if !(MIN_SUPPORTED_ARTIFACT_SET_VERSION..=SUPPORTED_ARTIFACT_SET_VERSION)
            .contains(&artifact_set_version)
        {
            return Err(UnsupportedVersion {
                source_kind: source,
                version: artifact_set_version,
            });
        }
        Ok(Self {
            reader,
            source,
            buffer: Vec::new(),
            line: 0,
            record: 0,
            done: false,
        })
    }

    /// Collect-to-`Vec` convenience over the streaming iterator.
    pub fn collect_records(self) -> Vec<Result<Record, RecordError>> {
        self.collect()
    }
}

impl<R: BufRead> Iterator for TranscriptStream<R> {
    type Item = Result<Record, RecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            self.buffer.clear();
            match self.reader.read_until(b'\n', &mut self.buffer) {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(_) => {}
                Err(source) => {
                    self.done = true;
                    return Some(Err(RecordError::Io {
                        line: self.line + 1,
                        source,
                    }));
                }
            }
            self.line += 1;
            if self.buffer.last() == Some(&b'\n') {
                self.buffer.pop();
            }
            // Empty segments are skipped, exactly as the legacy normalizer skips them; a
            // lone carriage return is deliberately *not* stripped, matching the legacy
            // byte-level `\n` split.
            if self.buffer.is_empty() {
                continue;
            }
            self.record += 1;
            let (line, record) = (self.line, self.record);
            let value: Value = match serde_json::from_slice(&self.buffer) {
                Ok(value) => value,
                Err(_) => {
                    return Some(Err(RecordError::MalformedJson {
                        line,
                        record,
                        raw: self.buffer.clone(),
                    }));
                }
            };
            let Some(object) = value.as_object() else {
                return Some(Ok(Record {
                    line,
                    record,
                    raw_timestamp: None,
                    timestamp: None,
                    classification: Classification::Ignored {
                        kind: NON_OBJECT_JSON_KIND.to_owned(),
                    },
                }));
            };
            let raw_timestamp = object.get("timestamp").cloned();
            let timestamp = raw_timestamp.as_ref().and_then(parse_timestamp);
            let classification = match classify::classify(self.source, object) {
                classify::Class::Content(events) => Classification::Content { events },
                classify::Class::Empty => Classification::Empty,
                classify::Class::Ignored(kind) => Classification::Ignored { kind },
                classify::Class::Unknown => Classification::Unknown {
                    // The line parsed as JSON, so it is valid UTF-8.
                    raw: String::from_utf8_lossy(&self.buffer).into_owned(),
                },
            };
            return Some(Ok(Record {
                line,
                record,
                raw_timestamp,
                timestamp,
                classification,
            }));
        }
    }
}

/// The legacy `NormalizedSession` counting fold, restated over the stream so consumers
/// can derive session-level counts and the started/updated window without buffering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    /// Number of [`Event::User`] events.
    pub user_requests: usize,
    /// Number of [`Event::Assistant`] events.
    pub assistant_messages: usize,
    /// Number of [`Event::Tool`] events.
    pub tool_activities: usize,
    /// Number of [`Classification::Ignored`] plus [`Classification::Unknown`] records —
    /// the legacy normalizer lumped both into one `ignored_events` count.
    pub ignored_events: usize,
    /// Number of [`RecordError`] items observed. Always zero for transcripts the legacy
    /// whole-parse normalizer accepted.
    pub malformed_records: usize,
    /// Minimum parsed record timestamp.
    pub first_timestamp: Option<DateTime<Utc>>,
    /// Maximum parsed record timestamp.
    pub last_timestamp: Option<DateTime<Utc>>,
}

impl SessionSummary {
    /// Folds an entire stream into a summary.
    pub fn summarize<'a, I>(items: I) -> Self
    where
        I: IntoIterator<Item = &'a Result<Record, RecordError>>,
    {
        let mut summary = Self::default();
        for item in items {
            summary.observe(item);
        }
        summary
    }

    /// Folds one stream item into the summary.
    pub fn observe(&mut self, item: &Result<Record, RecordError>) {
        let record = match item {
            Ok(record) => record,
            Err(_) => {
                self.malformed_records += 1;
                return;
            }
        };
        if let Some(timestamp) = record.timestamp {
            self.first_timestamp = Some(
                self.first_timestamp
                    .map_or(timestamp, |old| old.min(timestamp)),
            );
            self.last_timestamp = Some(
                self.last_timestamp
                    .map_or(timestamp, |old| old.max(timestamp)),
            );
        }
        match &record.classification {
            Classification::Content { events } => {
                for event in events {
                    match event {
                        Event::User { .. } => self.user_requests += 1,
                        Event::Assistant { .. } => self.assistant_messages += 1,
                        Event::Tool(_) => self.tool_activities += 1,
                    }
                }
            }
            Classification::Empty => {}
            Classification::Ignored { .. } | Classification::Unknown { .. } => {
                self.ignored_events += 1;
            }
        }
    }

    /// `first_timestamp` in the legacy `started_at` format: RFC 3339 with millisecond
    /// precision and a `Z` suffix.
    pub fn started_at(&self) -> Option<String> {
        self.first_timestamp.map(format_timestamp)
    }

    /// `last_timestamp` in the legacy `updated_at` format.
    pub fn updated_at(&self) -> Option<String> {
        self.last_timestamp.map(format_timestamp)
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
