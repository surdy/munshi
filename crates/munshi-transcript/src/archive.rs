//! The Munshi-owned archive Markdown record: what a `summary.md` says, and how to read one back
//! (munshi issue #79).
//!
//! Every snapshot Munshi captures carries a `summary.md` — YAML frontmatter naming the session,
//! its project, its cursor and its snapshot artifact index, followed by the
//! [`StructuredSummary`] rendered as headed Markdown (ADR 0009/0010). Reading that file back is
//! the only way a downstream consumer recovers what a session *was* without a database.
//!
//! # Why the reader lives here and the writer does not
//!
//! Format knowledge belongs in the crate its consumers pin (ADR 0011). `qanungo standup` pins
//! `munshi-transcript` and nothing else; before this move, parsing an archive meant depending on
//! the `munshi` app crate, which brings rusqlite, rustls, clap and `munshi-runner` along for a
//! string parse. The alternative — a second parser downstream — is the one outcome nobody wants,
//! because the format would then have two owners and drift silently between them.
//!
//! The *rendering* direction stays in `munshi`. It is not a matter of taste: rendering reads a
//! `NormalizedSession`, which is the output of the whole capture-side normalizer, plus the live
//! `NORMALIZER_VERSION` and `CURRENT_ARTIFACT_SET_VERSION` constants that describe what this
//! build captures. None of that is knowable — or meaningful — to a reader. The two directions
//! share no private helper (the writer emits YAML scalars through `serde_json::to_string`, the
//! reader reads them through `from_str`), so the split cuts along a real seam rather than through
//! one, and the round-trip tests stay in `munshi`, the one place both halves are visible.
//!
//! [`RenderError`] nevertheless moves whole, keeping its writer-only `InvalidPath` and `Io`
//! variants. It is the single error type both directions have always returned, `munshi`'s
//! archival path matches and constructs its variants and converts it into two enclosing error
//! enums, and splitting it would have changed that public type for the sake of tidiness in a
//! change whose whole point is that nothing observable changes.

use std::collections::BTreeMap;
use std::io;

use serde::Serialize;
use thiserror::Error;

use crate::Source;
use crate::summary::{StructuredSummary, validate_structured_summary};

/// Vendor-neutral identity of the coding-agent harness that produced a session.
///
/// The variants form the adapter boundary: each maps a source-specific transcript
/// envelope to the same `NormalizedSession` model so that summarizer, renderer,
/// state, and delivery paths remain independent from the capturing harness.
///
/// Distinct from [`Source`], which names the same three harnesses for the read-time
/// interpreters. This is the capture-side identity, and it is what an archive's `agent`
/// frontmatter key spells; the two are kept separate — with the conversion below — because
/// `SourceKind` also carries the CLI/config selector vocabulary that read-time interpretation
/// has no use for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// GitHub Copilot CLI (`events.jsonl`), the version-pinned default adapter.
    #[default]
    Copilot,
    /// Anthropic Claude Code session transcripts (`<uuid>.jsonl`).
    ClaudeCode,
    /// OpenAI Codex CLI rollout files (`rollout-*.jsonl`).
    Codex,
}

impl SourceKind {
    /// Stable identity prefix used in the durable archive `id` (`<prefix>:<session>`).
    pub fn id_prefix(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// Human- and machine-readable agent label recorded in archive frontmatter,
    /// summarizer input, and the SQLite `source_kind` column.
    pub fn agent_label(self) -> &'static str {
        match self {
            Self::Copilot => "copilot-cli",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex-cli",
        }
    }

    /// Selector accepted on the command line and in configuration.
    pub fn as_selector(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// Parse a user- or config-provided selector into a source kind.
    pub fn parse_selector(value: &str) -> Option<Self> {
        match value {
            "copilot" | "copilot-cli" => Some(Self::Copilot),
            "claude-code" | "claude" => Some(Self::ClaudeCode),
            "codex" | "codex-cli" => Some(Self::Codex),
            _ => None,
        }
    }

    /// Recover the source kind from a persisted agent label.
    pub fn from_agent_label(label: &str) -> Option<Self> {
        match label {
            "copilot-cli" => Some(Self::Copilot),
            "claude-code" => Some(Self::ClaudeCode),
            "codex-cli" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// The capture-side [`SourceKind`] and the read-time [`Source`] name the same three-harness
/// identity, so an archive's recorded `agent` selects the interpreter that reads its transcript.
impl From<SourceKind> for Source {
    fn from(source: SourceKind) -> Self {
        match source {
            SourceKind::Copilot => Self::Copilot,
            SourceKind::ClaudeCode => Self::ClaudeCode,
            SourceKind::Codex => Self::Codex,
        }
    }
}

/// How a session's project identity was derived (issue #40): from the live origin directory
/// (canonicalization plus git inspection) or, when that directory no longer exists, from the
/// origin evidence the source records themselves carry. The distinction is provenance only —
/// a recorded identity routes and archives exactly like a live one — but it is preserved in
/// state, archive frontmatter (`project_origin: "recorded"`), and upload metadata so a
/// recorded-evidence identity stays distinguishable from a live-resolved one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectOrigin {
    #[default]
    Live,
    Recorded,
}

impl ProjectOrigin {
    /// The marker persisted for a recorded identity; a live identity stores nothing, so
    /// records written before issue #40 keep meaning "live" unchanged.
    pub fn recorded_marker(self) -> Option<&'static str> {
        match self {
            Self::Live => None,
            Self::Recorded => Some("recorded"),
        }
    }
}

/// The project a session belongs to, as the archive records it.
///
/// Only the shape travels with the parser: *deriving* an identity from a directory is a
/// capture-side act needing git and the filesystem, and stays in `munshi`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    pub identity: String,
    pub component: String,
    pub project: String,
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub origin: ProjectOrigin,
}

/// One entry in a revision's snapshot artifact index (ADR 0010, CONTEXT.md "snapshot artifact set"):
/// the content address, original size, and short label of one extracted output. Mirrors the
/// claim-ticket marker the summarizer saw in its place, and points at the `outputs/<sha256>` artifact
/// the snapshot uploads. Derived purely from local extraction, never from any upload result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIndexEntry {
    /// Bare lowercase-hex sha256, matching the `outputs/<sha256>` logical path and the claim ticket.
    pub sha256: String,
    /// The original (uncompressed) content size in bytes.
    pub bytes: u64,
    /// The extracted event's kind label (`user`/`assistant`/`tool`).
    pub label: String,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("archive path has no usable parent directory")]
    InvalidPath,
    #[error("archive I/O failed")]
    Io(#[source] io::Error),
    #[error("archive Markdown is not a valid Munshi-owned record")]
    InvalidArchive,
    #[error("archive summary cache is invalid")]
    InvalidSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedCursor {
    pub normalizer_version: u32,
    pub record_count: u64,
    pub byte_offset: u64,
    pub prefix_hash: String,
    pub source_hash: String,
    pub source_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ArchivedMarkdown {
    pub schema_version: u32,
    pub source: SourceKind,
    pub session_id: String,
    pub project: ProjectIdentity,
    pub summary_revision: u64,
    pub completion_reason: String,
    pub cursor_fallback_reason: Option<String>,
    pub cursor: Option<ArchivedCursor>,
    pub source_cursor: u64,
    pub source_hash: String,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    /// Whether this revision's summary is a machine-generated placeholder (issue #43): a real
    /// summary is still owed and a later retry replaces it. Read from the explicit
    /// `summary_placeholder` frontmatter flag, falling back to the placeholder tag so the verdict
    /// survives even a hand-stripped frontmatter line. `false` for every real summary.
    pub summary_placeholder: bool,
    /// This revision's snapshot artifact-set version, when the archive carries the index (issue #21).
    /// `None` for pre-#21 archives written before the frontmatter index existed.
    pub artifact_set_version: Option<u16>,
    /// The `transcript.jsonl` artifact hash recorded in the artifact index (`sha256:<hex>`), when
    /// present. Equals `source_hash` for archives Munshi writes.
    pub transcript_sha256: Option<String>,
    /// The extracted-output entries of the snapshot artifact index, empty for pre-#21 archives and
    /// for revisions with no oversized events.
    pub extracted_outputs: Vec<ArtifactIndexEntry>,
    pub summary: StructuredSummary,
}

pub fn parse_archive_markdown(markdown: &str) -> Result<ArchivedMarkdown, RenderError> {
    let content = markdown
        .strip_prefix("---\n")
        .ok_or(RenderError::InvalidArchive)?;
    let (frontmatter, body) = content
        .split_once("\n---\n\n")
        .ok_or(RenderError::InvalidArchive)?;
    let Frontmatter {
        fields,
        tags,
        extracted_outputs,
    } = parse_frontmatter(frontmatter)?;

    let schema_version = parse_u32(field(&fields, "schema_version")?)?;
    if !matches!(schema_version, 1 | 2) {
        return Err(RenderError::InvalidArchive);
    }
    let session_id = parse_string(field(&fields, "session_id")?)?;
    let source = SourceKind::from_agent_label(&parse_string(field(&fields, "agent")?)?)
        .ok_or(RenderError::InvalidArchive)?;
    if parse_string(field(&fields, "id")?)? != format!("{}:{session_id}", source.id_prefix()) {
        return Err(RenderError::InvalidArchive);
    }
    let project_name = parse_string(field(&fields, "project")?)?;
    let project_identity = parse_string(field(&fields, "project_identity")?)?;
    let project_component = fields
        .get("project_component")
        .map(|value| parse_string(value))
        .transpose()?
        .unwrap_or_default();
    let repository = fields
        .get("repository")
        .map(|value| parse_string(value))
        .transpose()?;
    let branch = fields
        .get("branch")
        .map(|value| parse_string(value))
        .transpose()?;
    // Absent means live: archives written before issue #40 carry no `project_origin` key.
    let project_origin = match fields
        .get("project_origin")
        .map(|value| parse_string(value))
        .transpose()?
        .as_deref()
    {
        None | Some("live") => ProjectOrigin::Live,
        Some("recorded") => ProjectOrigin::Recorded,
        Some(_) => return Err(RenderError::InvalidArchive),
    };
    let summary_revision = parse_u64(field(&fields, "summary_revision")?)?;
    if summary_revision == 0 {
        return Err(RenderError::InvalidArchive);
    }
    let source_cursor = parse_u64(field(&fields, "source_cursor")?)?;
    let source_hash = parse_string(field(&fields, "source_hash")?)?;
    let completion_reason = fields
        .get("completion_reason")
        .map(|value| parse_string(value))
        .transpose()?
        .unwrap_or_else(|| "complete".to_owned());
    if project_name.is_empty()
        || project_identity.is_empty()
        || !valid_hash(&source_hash)
        || !matches!(
            completion_reason.as_str(),
            "complete" | "interrupted" | "unknown"
        )
    {
        return Err(RenderError::InvalidArchive);
    }
    if !project_component.is_empty()
        && (project_component == "."
            || project_component == ".."
            || project_component.contains(['/', '\\']))
    {
        return Err(RenderError::InvalidArchive);
    }
    let cursor_fallback_reason = fields
        .get("cursor_fallback_reason")
        .map(|value| parse_string(value))
        .transpose()?;
    if cursor_fallback_reason.as_deref().is_some_and(|reason| {
        !matches!(
            reason,
            "cursor-mismatch" | "normalizer-changed" | "source-truncated"
        )
    }) {
        return Err(RenderError::InvalidArchive);
    }
    let cursor = if schema_version >= 2 {
        let cursor = ArchivedCursor {
            normalizer_version: parse_u32(field(&fields, "normalizer_version")?)?,
            record_count: parse_u64(field(&fields, "source_cursor_records")?)?,
            byte_offset: parse_u64(field(&fields, "source_cursor_bytes")?)?,
            prefix_hash: parse_string(field(&fields, "source_prefix_hash")?)?,
            source_hash: source_hash.clone(),
            source_bytes: parse_u64(field(&fields, "source_bytes")?)?,
        };
        if cursor.normalizer_version == 0
            || cursor.byte_offset != cursor.source_bytes
            || !valid_hash(&cursor.prefix_hash)
            || cursor.prefix_hash != cursor.source_hash
        {
            return Err(RenderError::InvalidArchive);
        }
        Some(cursor)
    } else {
        None
    };
    if cursor
        .as_ref()
        .is_some_and(|cursor| cursor.record_count != source_cursor)
    {
        return Err(RenderError::InvalidArchive);
    }

    // Snapshot artifact index (issue #21). Optional so pre-#21 archives without the index still
    // parse; shape-validated when present so a corrupt index is rejected on the DB-rebuild path.
    let artifact_set_version = fields
        .get("artifact_set_version")
        .map(|value| parse_u16(value))
        .transpose()?;
    let transcript_sha256 = fields
        .get("transcript_sha256")
        .map(|value| parse_string(value))
        .transpose()?;
    if transcript_sha256
        .as_deref()
        .is_some_and(|hash| !valid_hash(hash))
    {
        return Err(RenderError::InvalidArchive);
    }

    let summary_placeholder_field = fields
        .get("summary_placeholder")
        .map(|value| match value.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(RenderError::InvalidArchive),
        })
        .transpose()?;

    let summary = parse_summary_body(body, tags).and_then(|summary| {
        validate_structured_summary(summary).map_err(|_| RenderError::InvalidSummary)
    })?;
    let summary_placeholder =
        summary_placeholder_field.unwrap_or(false) || summary.is_placeholder();
    Ok(ArchivedMarkdown {
        schema_version,
        source,
        session_id,
        project: ProjectIdentity {
            identity: project_identity,
            component: project_component,
            project: project_name,
            repository,
            branch,
            origin: project_origin,
        },
        summary_revision,
        completion_reason,
        cursor_fallback_reason,
        cursor,
        source_cursor,
        source_hash,
        started_at: fields
            .get("started_at")
            .map(|value| parse_string(value))
            .transpose()?,
        updated_at: fields
            .get("updated_at")
            .map(|value| parse_string(value))
            .transpose()?,
        artifact_set_version,
        transcript_sha256,
        extracted_outputs,
        summary_placeholder,
        summary,
    })
}

/// The block-scalar list keys the frontmatter parser understands, each rendered as a sequence of
/// `  - <json-scalar>` items. `tags` predates issue #21; `extracted_outputs` is the artifact index.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKey {
    Tags,
    ExtractedOutputs,
}

#[derive(Default)]
struct Frontmatter {
    fields: BTreeMap<String, String>,
    tags: Vec<String>,
    extracted_outputs: Vec<ArtifactIndexEntry>,
}

fn parse_frontmatter(frontmatter: &str) -> Result<Frontmatter, RenderError> {
    let mut parsed = Frontmatter::default();
    let mut list: Option<ListKey> = None;
    for line in frontmatter.lines() {
        // Continue an open list while the line is an item; otherwise close it and reinterpret the
        // line as a scalar or the next block header. This lets list blocks appear in any order and
        // be followed by more frontmatter, unlike the previous tags-only terminal parse.
        if let Some(key) = list {
            if let Some(value) = line.strip_prefix("  - ") {
                match key {
                    ListKey::Tags => parsed.tags.push(parse_string(value)?),
                    ListKey::ExtractedOutputs => {
                        parsed
                            .extracted_outputs
                            .push(parse_artifact_index_entry(value)?);
                    }
                }
                continue;
            }
            list = None;
        }
        if line == "tags:" {
            list = Some(ListKey::Tags);
            continue;
        }
        if line == "extracted_outputs:" {
            list = Some(ListKey::ExtractedOutputs);
            continue;
        }
        let (key, value) = line.split_once(": ").ok_or(RenderError::InvalidArchive)?;
        if matches!(key, "tags" | "extracted_outputs") {
            // The empty-list inline form; a non-empty list uses the block header above.
            if value == "[]" {
                continue;
            }
            return Err(RenderError::InvalidArchive);
        }
        parsed.fields.insert(key.to_owned(), value.to_owned());
    }
    Ok(parsed)
}

/// Parses one artifact-index item, the JSON-quoted scalar `"sha256:<hex> bytes:<n> label:<label>"`
/// the renderer writes. Tolerant of nothing malformed: an unrecognized token or a bad hash/size
/// fails the archive so a corrupt index never silently rebuilds.
fn parse_artifact_index_entry(value: &str) -> Result<ArtifactIndexEntry, RenderError> {
    let inner = parse_string(value)?;
    let mut sha256 = None;
    let mut bytes = None;
    let mut label = None;
    for token in inner.split(' ') {
        if let Some(hex) = token.strip_prefix("sha256:") {
            sha256 = Some(hex.to_owned());
        } else if let Some(size) = token.strip_prefix("bytes:") {
            bytes = Some(parse_u64(size)?);
        } else if let Some(name) = token.strip_prefix("label:") {
            label = Some(name.to_owned());
        } else {
            return Err(RenderError::InvalidArchive);
        }
    }
    let sha256 = sha256.ok_or(RenderError::InvalidArchive)?;
    let label = label.ok_or(RenderError::InvalidArchive)?;
    if sha256.len() != 64 || !is_lowercase_hex(&sha256) || label.is_empty() {
        return Err(RenderError::InvalidArchive);
    }
    Ok(ArtifactIndexEntry {
        sha256,
        bytes: bytes.ok_or(RenderError::InvalidArchive)?,
        label,
    })
}

fn parse_summary_body(body: &str, tags: Vec<String>) -> Result<StructuredSummary, RenderError> {
    let body = body.strip_prefix("# ").ok_or(RenderError::InvalidArchive)?;
    let (title, body) = body
        .split_once("\n\n## Goal\n\n")
        .ok_or(RenderError::InvalidArchive)?;
    let (goal, body) = body
        .split_once("\n\n## Work completed\n\n")
        .ok_or(RenderError::InvalidArchive)?;
    let (work_completed, body) = body
        .split_once("\n\n## Decisions\n\n")
        .ok_or(RenderError::InvalidArchive)?;
    let (decisions, body) = body
        .split_once("\n\n## Files changed\n\n")
        .ok_or(RenderError::InvalidArchive)?;
    let (files_changed, body) = body
        .split_once("\n\n## Commands and validation\n\n")
        .ok_or(RenderError::InvalidArchive)?;
    let (commands_and_validation, open_items) = body
        .split_once("\n\n## Open items\n\n")
        .ok_or(RenderError::InvalidArchive)?;
    Ok(StructuredSummary {
        title: title.to_owned(),
        goal: goal.to_owned(),
        work_completed: parse_list(work_completed)?,
        decisions: parse_list(decisions)?,
        files_changed: parse_list(files_changed)?,
        commands_and_validation: parse_list(commands_and_validation)?,
        open_items: parse_list(open_items)?,
        tags,
    })
}

fn parse_list(value: &str) -> Result<Vec<String>, RenderError> {
    let value = value.trim_end_matches('\n');
    if value == "- None." {
        return Ok(Vec::new());
    }
    value
        .lines()
        .map(|line| {
            line.strip_prefix("- ")
                .map(ToOwned::to_owned)
                .ok_or(RenderError::InvalidArchive)
        })
        .collect()
}

fn field<'a>(fields: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str, RenderError> {
    fields
        .get(name)
        .map(String::as_str)
        .ok_or(RenderError::InvalidArchive)
}

fn parse_string(value: &str) -> Result<String, RenderError> {
    serde_json::from_str(value).map_err(|_| RenderError::InvalidArchive)
}

fn parse_u64(value: &str) -> Result<u64, RenderError> {
    value.parse().map_err(|_| RenderError::InvalidArchive)
}

fn parse_u32(value: &str) -> Result<u32, RenderError> {
    value.parse().map_err(|_| RenderError::InvalidArchive)
}

fn parse_u16(value: &str) -> Result<u16, RenderError> {
    value.parse().map_err(|_| RenderError::InvalidArchive)
}

fn valid_hash(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|digest| digest.len() == 64 && is_lowercase_hex(digest))
}

/// Whether every byte is a lowercase hexadecimal digit (0-9, a-f). Every consumer of these hashes —
/// Patwari's hash-addressed retrieval (`retrieve::normalize_hash`) and the archive server itself —
/// requires lowercase, so parsing rejects uppercase to fail fast rather than admit a hash no
/// consumer accepts.
fn is_lowercase_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uppercase_hashes_are_rejected() {
        // Consumers (Patwari retrieval) require lowercase hex, so parsing rejects uppercase digests.
        assert!(valid_hash(&format!("sha256:{}", "ab".repeat(32))));
        assert!(!valid_hash(&format!("sha256:{}", "AB".repeat(32))));
        let lower = format!("\"sha256:{} bytes:1 label:tool\"", "ab".repeat(32));
        assert!(parse_artifact_index_entry(&lower).is_ok());
        let upper = format!("\"sha256:{} bytes:1 label:tool\"", "AB".repeat(32));
        assert!(parse_artifact_index_entry(&upper).is_err());
    }
}
