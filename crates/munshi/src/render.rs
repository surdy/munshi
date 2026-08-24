//! Writing the Munshi-owned archive Markdown record, and the durable placement of the files
//! around it.
//!
//! The *reading* half of this format — [`parse_archive_markdown`] and the [`ArchivedMarkdown`] it
//! produces — moved to `munshi-transcript` in issue #79, where read-side consumers can reach it
//! without pinning this crate. Writing stayed, because a render is a statement about *this
//! build's* capture: it reads a [`NormalizedSession`], the output of the whole normalizer, and
//! stamps the live `NORMALIZER_VERSION` and `CURRENT_ARTIFACT_SET_VERSION` into the frontmatter.
//! None of that is knowable to a reader, and none of it belongs in a crate whose whole promise is
//! that it interprets what was already written.
//!
//! The two directions never shared a helper — the writer emits YAML scalars through
//! `serde_json::to_string` and the reader reads them through `from_str` — so the split cut along
//! a seam that was already there. The round-trip tests below stay here, this being the one place
//! both halves are in scope, and they exercise the promoted parser exactly as any other caller
//! does.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::Builder;

pub use munshi_transcript::{
    ArchivedCursor, ArchivedMarkdown, RenderError, parse_archive_markdown,
};

use crate::project::ProjectIdentity;
use crate::source::{NormalizedSession, SidecarFile, SourceKind};
use crate::summary::StructuredSummary;

#[derive(Debug)]
pub struct ArchiveMetadata<'a> {
    pub session: &'a NormalizedSession,
    pub project: &'a ProjectIdentity,
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
    // Written only for recorded-evidence identities (issue #40), so every archive rendered
    // from a live origin stays byte-identical to what pre-#40 builds produced.
    if let Some(marker) = metadata.project.origin.recorded_marker() {
        line_string(&mut output, "project_origin", marker);
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
    // The visible durability-floor flag (issue #43): derived from the summary itself (its
    // placeholder tag) so renderer callers cannot disagree with the summary content. Written only
    // when true, keeping every real summary's frontmatter byte-identical to pre-#43 output.
    if summary.is_placeholder() {
        output.push_str("summary_placeholder: true\n");
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
/// The staged-sidecar directory for an archive Markdown path (issue #23): the sibling
/// `<session-id>.sidecar/` directory, derived by swapping the `.md` extension. Staging lives in
/// the archive output directory because the sidecar set is part of the durable record (ADR 0002)
/// and because upload retries must re-serialize a byte-identical manifest — the staged copies, not
/// the live session-state files, are what snapshots assemble from.
pub(crate) fn sidecar_directory(output_directory: &Path, markdown_relative: &Path) -> PathBuf {
    output_directory.join(sidecar_relative_directory(markdown_relative))
}

/// [`sidecar_directory`] relative to the output directory, for callers that plan paths before they
/// resolve them (record restore reports every write as an output-relative path).
pub(crate) fn sidecar_relative_directory(markdown_relative: &Path) -> PathBuf {
    markdown_relative.with_extension("sidecar")
}

/// The directory holding the snapshot artifacts a record restore (issue #70) brings back that local
/// archival never writes: the verbatim `transcript.jsonl` and the `outputs/<sha256>` extracted
/// outputs, which live in the harness home and in the transcript respectively rather than in the
/// archive. Derived by swapping the `.md` extension exactly as [`sidecar_relative_directory`] does,
/// so a session's whole restored set sits together beside its Markdown — and so restore writes into
/// a directory the archival path never touches, and can never race it.
pub(crate) fn restored_relative_directory(markdown_relative: &Path) -> PathBuf {
    markdown_relative.with_extension("restored")
}

/// Replaces the staged sidecar set for one archive revision: the directory is cleared and
/// rewritten so it holds exactly `files`, never a union across revisions (a checkpoint deleted by
/// the harness disappears from the next revision's snapshot too). An empty set removes the
/// directory entirely. Relative paths come from the capture allowlist
/// ([`crate::source::collect_copilot_sidecars`]), never from arbitrary directory content, so they
/// cannot traverse outside the sidecar directory.
pub(crate) fn stage_sidecar_files(
    output_directory: &Path,
    markdown_relative: &Path,
    files: &[SidecarFile],
) -> io::Result<()> {
    let directory = sidecar_directory(output_directory, markdown_relative);
    match fs::remove_dir_all(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if files.is_empty() {
        return Ok(());
    }
    for file in files {
        let target = directory.join(&file.relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, &file.bytes)?;
    }
    Ok(())
}

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
        crate::patwari::CURRENT_ARTIFACT_SET_VERSION,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectOrigin;
    use crate::source::{ArtifactIndexEntry, SnapshotArtifactIndex};

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
            opening_summary_request: false,
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
            origin: ProjectOrigin::Live,
        }
    }

    #[test]
    fn recorded_project_origin_round_trips_and_live_stays_unmarked() {
        let session = session_with_outputs(Vec::new());
        // A live identity writes no project_origin key, so pre-#40 archives stay byte-stable.
        let live = render_revision_markdown(
            &ArchiveMetadata {
                session: &session,
                project: &project(),
            },
            &summary(),
            1,
            "complete",
            None,
        );
        assert!(!live.contains("project_origin:"));
        assert_eq!(
            parse_archive_markdown(&live).unwrap().project.origin,
            ProjectOrigin::Live
        );

        // A recorded identity is flagged and the flag survives the re-parse (the DB-rebuild
        // and post-persist reconcile paths both read provenance back from the frontmatter).
        let recorded_project = ProjectIdentity {
            origin: ProjectOrigin::Recorded,
            repository: None,
            branch: Some("main".to_owned()),
            ..project()
        };
        let recorded = render_revision_markdown(
            &ArchiveMetadata {
                session: &session,
                project: &recorded_project,
            },
            &summary(),
            1,
            "complete",
            None,
        );
        assert!(recorded.contains("project_origin: \"recorded\"\n"));
        let parsed = parse_archive_markdown(&recorded).unwrap();
        assert_eq!(parsed.project.origin, ProjectOrigin::Recorded);
        assert_eq!(parsed.project.branch.as_deref(), Some("main"));

        // An unknown marker is rejected rather than silently coerced.
        let corrupted = recorded.replace(
            "project_origin: \"recorded\"",
            "project_origin: \"guessed\"",
        );
        assert!(parse_archive_markdown(&corrupted).is_err());
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
        assert!(markdown.contains("artifact_set_version: 2\n"));
        assert!(markdown.contains(&format!("transcript_sha256: \"{}\"\n", hash())));

        let parsed = parse_archive_markdown(&markdown).expect("archive with the index re-parses");
        assert_eq!(parsed.artifact_set_version, Some(2));
        assert_eq!(parsed.transcript_sha256.as_deref(), Some(hash().as_str()));
        assert_eq!(parsed.extracted_outputs, outputs);
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
        assert_eq!(parsed.artifact_set_version, Some(2));
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
