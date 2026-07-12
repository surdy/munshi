use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::Builder;
use thiserror::Error;

use crate::project::ProjectIdentity;
use crate::source::{NormalizedSession, SourceKind};
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
    let (fields, tags) = parse_frontmatter(frontmatter)?;

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

fn parse_frontmatter(
    frontmatter: &str,
) -> Result<(BTreeMap<String, String>, Vec<String>), RenderError> {
    let mut fields = BTreeMap::new();
    let mut tags = Vec::new();
    let mut in_tags = false;
    for line in frontmatter.lines() {
        if line == "tags:" {
            in_tags = true;
            continue;
        }
        if in_tags {
            if let Some(value) = line.strip_prefix("  - ") {
                tags.push(parse_string(value)?);
                continue;
            }
            return Err(RenderError::InvalidArchive);
        }
        let (key, value) = line.split_once(": ").ok_or(RenderError::InvalidArchive)?;
        if key == "tags" {
            if value == "[]" {
                continue;
            }
            return Err(RenderError::InvalidArchive);
        }
        fields.insert(key.to_owned(), value.to_owned());
    }
    Ok((fields, tags))
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

fn valid_hash(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}
