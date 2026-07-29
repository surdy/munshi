use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use munshi_runner::{RunnerConfig, RunnerError, run_bounded};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::project::ProjectIdentity;
use crate::source::NormalizedSession;

const TITLE_LIMIT: usize = 200;
const GOAL_LIMIT: usize = 4_000;
const ITEM_LIMIT: usize = 4_000;
const TAG_LIMIT: usize = 100;
const LIST_LIMIT: usize = 200;

#[derive(Debug, Clone)]
pub struct SummarizerConfig {
    pub binary: PathBuf,
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
}

/// Tag carried by every machine-generated placeholder summary (issue #43), so a placeholder is
/// recognizable from the summary alone — in the archive frontmatter, the state database, and any
/// delivered note — without consulting operational state.
pub const PLACEHOLDER_SUMMARY_TAG: &str = "munshi-placeholder-summary";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredSummary {
    pub title: String,
    pub goal: String,
    pub work_completed: Vec<String>,
    pub decisions: Vec<String>,
    pub files_changed: Vec<String>,
    pub commands_and_validation: Vec<String>,
    pub open_items: Vec<String>,
    pub tags: Vec<String>,
}

impl StructuredSummary {
    /// Whether this is a machine-generated placeholder (issue #43) rather than a real summary.
    pub fn is_placeholder(&self) -> bool {
        self.tags.iter().any(|tag| tag == PLACEHOLDER_SUMMARY_TAG)
    }
}

/// The deterministic input-capacity class that triggered a placeholder archival (issue #43):
/// either the summarizer process itself rejected the input (a repeat-failure park, issue #38) or
/// Munshi's own `max_input_bytes` cap refused to build the input at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderReason {
    SummarizerRejected,
    InputCapExceeded,
}

impl PlaceholderReason {
    /// The human marker line carried in the placeholder's goal and work list.
    pub fn marker(self) -> &'static str {
        match self {
            Self::SummarizerRejected => {
                "Summary unavailable: summarizer rejected oversized input (munshi#43)."
            }
            Self::InputCapExceeded => concat!(
                "Summary unavailable: normalized input exceeds the configured summarizer ",
                "input limit (munshi#43)."
            ),
        }
    }
}

/// Builds the machine-generated placeholder summary the durability floor archives (issue #43).
/// The title comes from session metadata only, the lists carry an explicit self-describing
/// marker, and the [`PLACEHOLDER_SUMMARY_TAG`] makes the placeholder recognizable everywhere the
/// summary travels. Always passes [`validate_structured_summary`] so it renders, re-parses, and
/// delivers exactly like a real summary.
pub fn placeholder_summary(
    agent_label: &str,
    session_id: &str,
    reason: PlaceholderReason,
) -> StructuredSummary {
    let marker = reason.marker();
    StructuredSummary {
        title: format!("{agent_label} session {session_id} (summary unavailable)"),
        goal: format!(
            "{marker} The full transcript is archived unchanged; run `munshi retry {session_id}` \
             to attempt a real summary, which replaces this placeholder."
        ),
        work_completed: vec![marker.to_owned()],
        decisions: Vec::new(),
        files_changed: Vec::new(),
        commands_and_validation: Vec::new(),
        open_items: Vec::new(),
        tags: vec![PLACEHOLDER_SUMMARY_TAG.to_owned()],
    }
}

#[derive(Debug, Serialize)]
struct SummaryRequest<'a> {
    instruction: &'static str,
    required_schema: RequiredSchema,
    session: SummarySession<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_summary: Option<&'a StructuredSummary>,
    events: &'a [crate::source::NormalizedEvent],
    ignored_unknown_event_count: usize,
}

#[derive(Debug, Serialize)]
struct RequiredSchema {
    title: &'static str,
    goal: &'static str,
    work_completed: &'static str,
    decisions: &'static str,
    files_changed: &'static str,
    commands_and_validation: &'static str,
    open_items: &'static str,
    tags: &'static str,
}

#[derive(Debug, Serialize)]
struct SummarySession<'a> {
    id: String,
    source_agent: &'static str,
    session_id: &'a str,
    project_identity: &'a str,
    repository: Option<&'a str>,
}

#[derive(Debug, Error)]
pub enum SummaryError {
    #[error("failed to construct summarizer input")]
    InputSerialization(#[source] serde_json::Error),
    #[error("normalized summarizer input exceeds the configured {limit}-byte limit")]
    InputLimit { limit: usize },
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error("summary stdout was not one valid JSON object")]
    MalformedJson(#[source] serde_json::Error),
    #[error("summary field {field} must contain between 1 and {max} characters")]
    InvalidText { field: &'static str, max: usize },
    #[error("summary list {field} exceeds its {max}-item limit")]
    InvalidListLength { field: &'static str, max: usize },
    #[error("summary list {field} must contain at least one item")]
    MissingListItems { field: &'static str },
}

pub fn build_summary_input(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    input_limit: usize,
) -> Result<Vec<u8>, SummaryError> {
    build_summary_input_inner(session, project, None, input_limit)
}

pub fn build_revision_summary_input(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    previous_summary: &StructuredSummary,
    input_limit: usize,
) -> Result<Vec<u8>, SummaryError> {
    build_summary_input_inner(session, project, Some(previous_summary), input_limit)
}

fn build_summary_input_inner(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    previous_summary: Option<&StructuredSummary>,
    input_limit: usize,
) -> Result<Vec<u8>, SummaryError> {
    let request = SummaryRequest {
        instruction: if previous_summary.is_some() {
            concat!(
                "Revise previous_summary using only the new normalized events. Return a complete ",
                "replacement summary as exactly one JSON object matching required_schema, not a ",
                "patch or append-only fragment. Preserve still-correct prior work and incorporate ",
                "new goals, meaningful completed work, decisions, files changed, commands and ",
                "validation, and open items. Do not quote prompts, raw tool output, secrets, or ",
                "substantial code. Return JSON only, with no Markdown fence or commentary."
            )
        } else {
            concat!(
                "Summarize this coding session as exactly one JSON object matching required_schema. ",
                "Return every required field. Capture goals, meaningful completed work, decisions, ",
                "files changed, commands and validation, and open items. Do not quote prompts, raw ",
                "tool output, secrets, or substantial code. Use concise strings and arrays of strings. ",
                "Return JSON only, with no Markdown fence or commentary."
            )
        },
        required_schema: RequiredSchema {
            title: "non-empty string",
            goal: "non-empty string",
            work_completed: "array of strings",
            decisions: "array of strings",
            files_changed: "array of strings",
            commands_and_validation: "array of strings",
            open_items: "array of strings",
            tags: "array of strings",
        },
        session: SummarySession {
            id: format!("{}:{}", session.source.id_prefix(), session.session_id),
            source_agent: session.source.agent_label(),
            session_id: &session.session_id,
            project_identity: &project.identity,
            repository: project.repository.as_deref(),
        },
        previous_summary,
        events: &session.events,
        ignored_unknown_event_count: session.ignored_events,
    };
    let bytes = serde_json::to_vec(&request).map_err(SummaryError::InputSerialization)?;
    if bytes.len() > input_limit {
        return Err(SummaryError::InputLimit { limit: input_limit });
    }
    Ok(bytes)
}

pub fn run_summary(
    config: &SummarizerConfig,
    input: Vec<u8>,
) -> Result<StructuredSummary, SummaryError> {
    let stdout = run_bounded(
        &RunnerConfig {
            binary: config.binary.clone(),
            args: config.args.clone(),
            timeout: config.timeout,
            stdout_limit: config.stdout_limit,
            stderr_limit: config.stderr_limit,
        },
        input,
    )?;
    let summary: StructuredSummary =
        serde_json::from_slice(&stdout).map_err(SummaryError::MalformedJson)?;
    validate_structured_summary(summary)
}

pub fn validate_structured_summary(
    mut summary: StructuredSummary,
) -> Result<StructuredSummary, SummaryError> {
    summary.title = validate_text("title", summary.title, TITLE_LIMIT, true)?;
    summary.goal = validate_text("goal", summary.goal, GOAL_LIMIT, false)?;
    summary.work_completed = validate_list("work_completed", summary.work_completed, ITEM_LIMIT)?;
    if summary.work_completed.is_empty() {
        return Err(SummaryError::MissingListItems {
            field: "work_completed",
        });
    }
    summary.decisions = validate_list("decisions", summary.decisions, ITEM_LIMIT)?;
    summary.files_changed = validate_list("files_changed", summary.files_changed, ITEM_LIMIT)?;
    summary.commands_and_validation = validate_list(
        "commands_and_validation",
        summary.commands_and_validation,
        ITEM_LIMIT,
    )?;
    summary.open_items = validate_list("open_items", summary.open_items, ITEM_LIMIT)?;
    summary.tags = validate_list("tags", summary.tags, TAG_LIMIT)?;
    Ok(summary)
}

fn validate_list(
    field: &'static str,
    values: Vec<String>,
    item_limit: usize,
) -> Result<Vec<String>, SummaryError> {
    if values.len() > LIST_LIMIT {
        return Err(SummaryError::InvalidListLength {
            field,
            max: LIST_LIMIT,
        });
    }
    values
        .into_iter()
        .map(|value| validate_text(field, value, item_limit, true))
        .collect()
}

fn validate_text(
    field: &'static str,
    value: String,
    max: usize,
    single_line: bool,
) -> Result<String, SummaryError> {
    let value = value.trim().replace("\r\n", "\n").replace('\r', "\n");
    let valid_controls = value
        .chars()
        .all(|character| !character.is_control() || matches!(character, '\n' | '\t'));
    let length = value.chars().count();
    if length == 0
        || length > max
        || !valid_controls
        || (single_line && value.contains('\n'))
        || (!single_line && value.contains("\n## "))
    {
        Err(SummaryError::InvalidText { field, max })
    } else {
        Ok(value)
    }
}
