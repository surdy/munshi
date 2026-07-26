use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::Builder;
use thiserror::Error;

use crate::project::ProjectIdentity;
use crate::source::{ArtifactIndexEntry, NormalizedSession, SourceKind};
use crate::summary::{StructuredSummary, validate_structured_summary};

#[derive(Debug)]
pub struct ArchiveMetadata<'a> {
    pub session: &'a NormalizedSession,
    pub project: &'a ProjectIdentity,
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

pub fn render_markdown(metadata: &ArchiveMetadata<'_>, summary: &StructuredSummary) -> String {
    render_markdown_version(
        metadata,
        summary,
        &ArchiveVersion {
            schema_version: 1,
            summary_revision: 1,
            completion_reason: None,
            cursor_fallback_reason: None,
        },
    )
}

#[derive(Debug, Clone, Copy)]
pub struct ArchiveVersion<'a> {
    pub schema_version: u32,
    pub summary_revision: u64,
    pub completion_reason: Option<&'a str>,
    pub cursor_fallback_reason: Option<&'a str>,
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

pub fn render_revision_markdown(
    metadata: &ArchiveMetadata<'_>,
    summary: &StructuredSummary,
    summary_revision: u64,
    completion_reason: &str,
    cursor_fallback_reason: Option<&str>,
) -> String {
    render_markdown_version(
        metadata,
        summary,
        &ArchiveVersion {
            schema_version: 2,
            summary_revision,
            completion_reason: Some(completion_reason),
            cursor_fallback_reason,
        },
    )
}

fn render_markdown_version(
    metadata: &ArchiveMetadata<'_>,
    summary: &StructuredSummary,
    version: &ArchiveVersion<'_>,
) -> String {
    let mut output = String::new();
    output.push_str("---\n");
    line_number(&mut output, "schema_version", version.schema_version);
    line_string(
        &mut output,
        "id",
        &format!(
            "{}:{}",
            metadata.session.source.id_prefix(),
            metadata.session.session_id
        ),
    );
    line_string(&mut output, "agent", metadata.session.source.agent_label());
    line_string(&mut output, "session_id", &metadata.session.session_id);
    line_string(&mut output, "project", &metadata.project.project);
    line_string(&mut output, "project_identity", &metadata.project.identity);
    if version.schema_version >= 2 {
        line_string(
            &mut output,
            "project_component",
            &metadata.project.component,
        );
    }
    if let Some(repository) = &metadata.project.repository {
        line_string(&mut output, "repository", repository);
    }
    if let Some(branch) = &metadata.project.branch {
        line_string(&mut output, "branch", branch);
    }
    if let Some(started_at) = &metadata.session.started_at {
        line_string(&mut output, "started_at", started_at);
    }
    if let Some(updated_at) = &metadata.session.updated_at {
        line_string(&mut output, "updated_at", updated_at);
    }
    if let Some(completion_reason) = version.completion_reason {
        line_string(&mut output, "completion_reason", completion_reason);
    }
    if let Some(fallback_reason) = version.cursor_fallback_reason {
        line_string(&mut output, "cursor_fallback_reason", fallback_reason);
    }
    line_number(&mut output, "summary_revision", version.summary_revision);
    line_number(&mut output, "source_cursor", metadata.session.source_cursor);
    if version.schema_version >= 2 {
        line_number(
            &mut output,
            "normalizer_version",
            crate::source::NORMALIZER_VERSION,
        );
        line_number(
            &mut output,
            "source_cursor_records",
            metadata.session.source_cursor,
        );
        line_number(
            &mut output,
            "source_cursor_bytes",
            metadata.session.source_byte_cursor,
        );
        line_string(
            &mut output,
            "source_prefix_hash",
            &metadata.session.source_prefix_hash,
        );
        line_number(&mut output, "source_bytes", metadata.session.source_bytes);
    }
    line_string(&mut output, "source_hash", &metadata.session.source_hash);
    if version.schema_version >= 2 {
        render_artifact_index(&mut output, metadata);
    }
    if summary.tags.is_empty() {
        output.push_str("tags: []\n");
    } else {
        output.push_str("tags:\n");
        for tag in &summary.tags {
            output.push_str("  - ");
            output.push_str(&yaml_string(tag));
            output.push('\n');
        }
    }
    output.push_str("---\n\n# ");
    output.push_str(&summary.title);
    output.push_str("\n\n## Goal\n\n");
    output.push_str(&summary.goal);
    output.push_str("\n\n");
    render_list(&mut output, "Work completed", &summary.work_completed);
    render_list(&mut output, "Decisions", &summary.decisions);
    render_list(&mut output, "Files changed", &summary.files_changed);
    render_list(
        &mut output,
        "Commands and validation",
        &summary.commands_and_validation,
    );
    render_list(&mut output, "Open items", &summary.open_items);
    output.pop();
    output
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

    let summary = parse_summary_body(body, tags).and_then(|summary| {
        validate_structured_summary(summary).map_err(|_| RenderError::InvalidSummary)
    })?;
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
        summary,
    })
}

pub fn content_hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn atomic_replace(output: &Path, bytes: &[u8]) -> Result<(), RenderError> {
    let parent = output.parent().ok_or(RenderError::InvalidPath)?;
    fs::create_dir_all(parent).map_err(RenderError::Io)?;
    let mut temporary = Builder::new()
        .prefix(".munshi-")
        .tempfile_in(parent)
        .map_err(RenderError::Io)?;
    temporary.write_all(bytes).map_err(RenderError::Io)?;
    temporary.flush().map_err(RenderError::Io)?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(RenderError::Io)?;
    let file = temporary
        .persist(output)
        .map_err(|error| RenderError::Io(error.error))?;
    file.sync_all().map_err(RenderError::Io)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(RenderError::Io)?;
    Ok(())
}

pub fn archive_path(output_directory: &Path, metadata: &ArchiveMetadata<'_>) -> PathBuf {
    output_directory.join(archive_relative_path(
        metadata.session.source,
        &metadata.project.component,
        &metadata.session.session_id,
    ))
}

/// Durable archive path relative to the output directory, scoped by source.
///
/// Different harnesses that share a project component and session ID must never
/// resolve to the same Markdown file. Copilot keeps its original
/// `<component>/<session_id>.md` layout for backward compatibility; every other
/// source nests its records under a `<source-prefix>/` segment.
pub(crate) fn archive_relative_path(
    source: SourceKind,
    component: &str,
    session_id: &str,
) -> PathBuf {
    let file = format!("{session_id}.md");
    match source {
        SourceKind::Copilot => Path::new(component).join(file),
        other => Path::new(component).join(other.id_prefix()).join(file),
    }
}

/// Writes this revision's snapshot artifact index into the frontmatter (ADR 0010, issue #21):
/// `artifact_set_version`, the `transcript_sha256` of the transcript bytes this revision summarized
/// (the same value as `source_hash`, named here per CONTEXT.md "snapshot artifact set" language),
/// and each extracted output as `sha256:<hex> bytes:<n> label:<label>` — the bare-hex address of the
/// `outputs/<sha256>` artifact, mirroring the claim ticket the summarizer saw. Every value comes from
/// local extraction of the summarized bytes, never from an upload result, so a summary renders and
/// delivers with Patwari unreachable (the ordering guarantee).
///
/// The capture id is deliberately omitted. Minting it here would require embedding a Patwari upload
/// identity into `summary.md`, but that artifact's hash is itself part of the manifest whose
/// idempotency the capture id keys — a self-referential coupling that would also break rendering when
/// upload is disabled or unreachable and destabilize #19's manifest-identical-retry invariant. The
/// content-addressed hashes are the load-bearing part; a retriever resolves them through Patwari
/// without needing the capture id in the summary.
fn render_artifact_index(output: &mut String, metadata: &ArchiveMetadata<'_>) {
    line_number(
        output,
        "artifact_set_version",
        crate::patwari::INITIAL_ARTIFACT_SET_VERSION,
    );
    line_string(output, "transcript_sha256", &metadata.session.source_hash);
    let outputs = &metadata.session.artifact_index.extracted_outputs;
    if outputs.is_empty() {
        output.push_str("extracted_outputs: []\n");
        return;
    }
    output.push_str("extracted_outputs:\n");
    for entry in outputs {
        output.push_str("  - ");
        output.push_str(&yaml_string(&format!(
            "sha256:{} bytes:{} label:{}",
            entry.sha256, entry.bytes, entry.label
        )));
        output.push('\n');
    }
}

fn line_number(output: &mut String, key: &str, value: impl std::fmt::Display) {
    output.push_str(key);
    output.push_str(": ");
    output.push_str(&value.to_string());
    output.push('\n');
}

fn line_string(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(": ");
    output.push_str(&yaml_string(value));
    output.push('\n');
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn render_list(output: &mut String, heading: &str, items: &[String]) {
    output.push_str("## ");
    output.push_str(heading);
    output.push_str("\n\n");
    if items.is_empty() {
        output.push_str("- None.\n\n");
        return;
    }
    for item in items {
        output.push_str("- ");
        output.push_str(item);
        output.push('\n');
    }
    output.push('\n');
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
    use crate::source::SnapshotArtifactIndex;

    fn hash() -> String {
        format!("sha256:{}", "ab".repeat(32))
    }

    fn session_with_outputs(outputs: Vec<ArtifactIndexEntry>) -> NormalizedSession {
        NormalizedSession {
            source: SourceKind::Copilot,
            session_id: "sess-1".to_owned(),
            events: Vec::new(),
            user_requests: 1,
            assistant_messages: 1,
            tool_activities: 1,
            ignored_events: 0,
            source_cursor: 3,
            source_byte_cursor: 128,
            source_prefix_hash: hash(),
            source_hash: hash(),
            source_bytes: 128,
            started_at: None,
            updated_at: None,
            artifact_index: SnapshotArtifactIndex {
                extracted_outputs: outputs,
            },
        }
    }

    fn summary() -> StructuredSummary {
        StructuredSummary {
            title: "Stable title".to_owned(),
            goal: "Stable goal.".to_owned(),
            work_completed: vec!["Did the work.".to_owned()],
            decisions: Vec::new(),
            files_changed: Vec::new(),
            commands_and_validation: Vec::new(),
            open_items: Vec::new(),
            tags: vec!["build".to_owned()],
        }
    }

    fn project() -> ProjectIdentity {
        ProjectIdentity {
            identity: "github.com/surdy/munshi".to_owned(),
            component: "munshi".to_owned(),
            project: "munshi".to_owned(),
            repository: Some("surdy/munshi".to_owned()),
            branch: None,
        }
    }

    #[test]
    fn frontmatter_artifact_index_round_trips() {
        let outputs = vec![
            ArtifactIndexEntry {
                sha256: "aa".repeat(32),
                bytes: 4096,
                label: "tool".to_owned(),
            },
            ArtifactIndexEntry {
                sha256: "bb".repeat(32),
                bytes: 200_000,
                label: "assistant".to_owned(),
            },
        ];
        let session = session_with_outputs(outputs.clone());
        let metadata = ArchiveMetadata {
            session: &session,
            project: &project(),
        };
        let markdown = render_revision_markdown(&metadata, &summary(), 1, "complete", None);
        assert!(markdown.contains("artifact_set_version: 1\n"));
        assert!(markdown.contains(&format!("transcript_sha256: \"{}\"\n", hash())));

        let parsed = parse_archive_markdown(&markdown).expect("archive with the index re-parses");
        assert_eq!(parsed.artifact_set_version, Some(1));
        assert_eq!(parsed.transcript_sha256.as_deref(), Some(hash().as_str()));
        assert_eq!(parsed.extracted_outputs, outputs);
    }

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

    #[test]
    fn empty_artifact_index_round_trips() {
        let session = session_with_outputs(Vec::new());
        let metadata = ArchiveMetadata {
            session: &session,
            project: &project(),
        };
        let markdown = render_revision_markdown(&metadata, &summary(), 1, "complete", None);
        assert!(markdown.contains("extracted_outputs: []\n"));
        let parsed = parse_archive_markdown(&markdown).unwrap();
        assert_eq!(parsed.artifact_set_version, Some(1));
        assert!(parsed.extracted_outputs.is_empty());
    }

    #[test]
    fn pre_issue_21_archive_without_the_index_still_parses() {
        // A schema-2 archive predating issue #21 carries no artifact index: strip those lines and
        // confirm the DB-rebuild parse path tolerates their absence.
        let session = session_with_outputs(Vec::new());
        let metadata = ArchiveMetadata {
            session: &session,
            project: &project(),
        };
        let markdown = render_revision_markdown(&metadata, &summary(), 1, "complete", None);
        let stripped: String = markdown
            .lines()
            .filter(|line| {
                !line.starts_with("artifact_set_version:")
                    && !line.starts_with("transcript_sha256:")
                    && !line.starts_with("extracted_outputs:")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse_archive_markdown(&stripped).expect("pre-#21 archives still parse");
        assert_eq!(parsed.artifact_set_version, None);
        assert_eq!(parsed.transcript_sha256, None);
        assert!(parsed.extracted_outputs.is_empty());

        // A legacy schema-1 archive (render_markdown) likewise carries no index and parses.
        let legacy = render_markdown(&metadata, &summary());
        let legacy_parsed = parse_archive_markdown(&legacy).unwrap();
        assert_eq!(legacy_parsed.artifact_set_version, None);
        assert!(legacy_parsed.extracted_outputs.is_empty());
    }
}
