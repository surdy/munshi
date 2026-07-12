use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tempfile::Builder;
use thiserror::Error;

use crate::project::ProjectIdentity;
use crate::source::NormalizedSession;
use crate::summary::StructuredSummary;

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
}

pub fn render_markdown(metadata: &ArchiveMetadata<'_>, summary: &StructuredSummary) -> String {
    let mut output = String::new();
    output.push_str("---\n");
    line_number(&mut output, "schema_version", 1);
    line_string(
        &mut output,
        "id",
        &format!("copilot:{}", metadata.session.session_id),
    );
    line_string(&mut output, "agent", "copilot-cli");
    line_string(&mut output, "session_id", &metadata.session.session_id);
    line_string(&mut output, "project", &metadata.project.project);
    line_string(&mut output, "project_identity", &metadata.project.identity);
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
    line_number(&mut output, "summary_revision", 1);
    line_number(&mut output, "source_cursor", metadata.session.source_cursor);
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
    output_directory
        .join(&metadata.project.component)
        .join(format!("{}.md", metadata.session.session_id))
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
