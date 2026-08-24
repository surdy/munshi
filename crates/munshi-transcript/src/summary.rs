//! The structured session summary Munshi archives, and the validation every summary passes
//! before it is written or believed (munshi issue #79).
//!
//! This type is what a `summary.md` artifact *says*; [`crate::ArchivedMarkdown`] is the record
//! that carries it. It lives beside the transcript interpreters for the same reason they do
//! (ADR 0011): the meaning of an archived artifact belongs in the crate its readers pin, not in
//! the app that happened to write it. A read-side consumer — `qanungo standup` reading the
//! `summary.md` every snapshot carries — needs to parse a summary back out of an archive without
//! taking on the capture-side machinery that produced it.
//!
//! Validation travels with the type rather than staying behind it because the two are one rule.
//! [`validate_structured_summary`] is not a courtesy check on summarizer output; it is the
//! normalization that decides what a summary *is* — trimmed text, `\r\n` folded to `\n`, control
//! characters refused, list and length ceilings applied. A parser that re-read an archive without
//! it would admit summaries the writer would never have emitted, so the round trip would stop
//! being a round trip. The limits are deliberately duplicated nowhere: these constants are the
//! only copy.

use serde::{Deserialize, Serialize};
use thiserror::Error;

const TITLE_LIMIT: usize = 200;
const GOAL_LIMIT: usize = 4_000;
const ITEM_LIMIT: usize = 4_000;
const TAG_LIMIT: usize = 100;
const LIST_LIMIT: usize = 200;

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

/// Why a candidate summary is not a valid [`StructuredSummary`].
///
/// Narrower than `munshi`'s `SummaryError`, and deliberately so: that enum also names how a
/// summarizer *invocation* can fail — a non-zero exit from `munshi-runner`, unparseable stdout,
/// an input over the configured byte cap — none of which a reader parsing an already-written
/// archive can encounter, and all of which would drag the app crate's dependencies across this
/// seam. `munshi` converts these three variants back into its own so its public error surface,
/// and every message it prints, are unchanged.
#[derive(Debug, Error)]
pub enum SummaryValidationError {
    #[error("summary field {field} must contain between 1 and {max} characters")]
    InvalidText { field: &'static str, max: usize },
    #[error("summary list {field} exceeds its {max}-item limit")]
    InvalidListLength { field: &'static str, max: usize },
    #[error("summary list {field} must contain at least one item")]
    MissingListItems { field: &'static str },
}

pub fn validate_structured_summary(
    mut summary: StructuredSummary,
) -> Result<StructuredSummary, SummaryValidationError> {
    summary.title = validate_text("title", summary.title, TITLE_LIMIT, true)?;
    summary.goal = validate_text("goal", summary.goal, GOAL_LIMIT, false)?;
    summary.work_completed = validate_list("work_completed", summary.work_completed, ITEM_LIMIT)?;
    if summary.work_completed.is_empty() {
        return Err(SummaryValidationError::MissingListItems {
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
) -> Result<Vec<String>, SummaryValidationError> {
    if values.len() > LIST_LIMIT {
        return Err(SummaryValidationError::InvalidListLength {
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
) -> Result<String, SummaryValidationError> {
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
        Err(SummaryValidationError::InvalidText { field, max })
    } else {
        Ok(value)
    }
}
