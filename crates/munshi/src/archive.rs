use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

use crate::project::{ProjectIdentityError, inspect_project};
use crate::render::{ArchiveMetadata, RenderError, archive_path, atomic_replace, render_markdown};
pub use crate::source::SessionReference;
use crate::source::{SourceError, load_session_update, resolve_session_reference};
use crate::summary::{
    SummarizerConfig, SummaryError, SummaryPhase, build_summary_input, run_summary,
};

#[derive(Debug, Clone)]
pub struct ArchiveConfig {
    pub reference: SessionReference,
    pub project_directory: PathBuf,
    pub output_directory: PathBuf,
    pub summarizer_binary: PathBuf,
    pub summarizer_args: Vec<OsString>,
    /// Environment variables set on the summarizer invocation (repeatable `--summarizer-env
    /// KEY=VALUE`), matching the hook path's `summarizer.env` semantics: opaque to Munshi and
    /// merged before Munshi's own per-invocation variables, which win on conflict.
    pub summarizer_env: Vec<(String, String)>,
    pub timeout: Duration,
    pub max_source_bytes: usize,
    pub max_input_bytes: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    /// Per-event extraction threshold: content larger than this is preserved as an extracted output
    /// and elided from summarizer input (ADR 0010). Threaded from the registered stored config so
    /// manual `munshi archive` elides on exactly the same threshold the hook path uses; it falls
    /// back to the built-in default when no registration is present.
    pub max_event_text_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveOutcome {
    Archived { id: String, relative_path: PathBuf },
    NotArchiveWorthy { id: String },
}

#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Project(#[from] ProjectIdentityError),
    #[error(transparent)]
    Summary(#[from] SummaryError),
    #[error(transparent)]
    Render(#[from] RenderError),
}

pub fn archive_session(config: &ArchiveConfig) -> Result<ArchiveOutcome, ArchiveError> {
    let resolved = resolve_session_reference(&config.reference)?;
    let session = load_session_update(
        &resolved,
        config.max_source_bytes,
        None,
        config.max_event_text_bytes,
    )?
    .session;
    let id = format!("{}:{}", session.source.id_prefix(), session.session_id);
    if !session.is_archive_worthy() {
        return Ok(ArchiveOutcome::NotArchiveWorthy { id });
    }

    let project = inspect_project(&config.project_directory)?;
    let input = build_summary_input(&session, &project, config.max_input_bytes)?;
    let summary = run_summary(
        &SummarizerConfig {
            binary: config.summarizer_binary.clone(),
            args: config.summarizer_args.clone(),
            env: config.summarizer_env.clone(),
            timeout: config.timeout,
            stdout_limit: config.max_stdout_bytes,
            stderr_limit: config.max_stderr_bytes,
        },
        SummaryPhase::Complete,
        input,
    )?;
    let metadata = ArchiveMetadata {
        session: &session,
        project: &project,
    };
    let markdown = render_markdown(&metadata, &summary);
    let output = archive_path(&config.output_directory, &metadata);
    atomic_replace(&output, markdown.as_bytes())?;
    let relative_path = output
        .strip_prefix(&config.output_directory)
        .unwrap_or(Path::new(&output))
        .to_path_buf();
    // Stage this revision's harness sidecar set beside the Markdown (issue #23). Best-effort:
    // sidecars are optional snapshot artifacts, so staging failure never fails the archive.
    if session.source == crate::source::SourceKind::Copilot {
        let sidecars = crate::source::collect_copilot_sidecars(&resolved.events_path);
        let _ = crate::render::stage_sidecar_files(
            &config.output_directory,
            &relative_path,
            &sidecars,
        );
    }
    Ok(ArchiveOutcome::Archived { id, relative_path })
}
