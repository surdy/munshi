//! Integration coverage for harness sidecar capture and staging (issue #23, artifact set v2).
//!
//! Proves the two halves of the sidecar contract: `collect_copilot_sidecars` captures exactly the
//! allowlisted session-state files under the read discipline (symlinks refused, oversized files
//! skipped, deterministic order), and `archive_session` stages the captured set into the archive
//! output directory beside the Markdown — replacing, never unioning, the previous revision's set.

use std::fs;
use std::path::{Path, PathBuf};

use munshi::{
    ArchiveConfig, ArchiveOutcome, SIDECAR_MAX_FILE_BYTES, SessionReference, SourceKind,
    archive_session, collect_copilot_sidecars,
};
use tempfile::TempDir;

const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";

/// Copies the manual-archive Copilot fixture into a temp session-state layout and surrounds the
/// transcript with a realistic sidecar population: allowlisted files, excluded kinds
/// (`session.db`, `rewind-file-snapshots/backups/`, `files/`), a symlink, and an oversized file.
fn seeded_session(directory: &TempDir) -> PathBuf {
    let session_dir = directory.path().join("session-state").join(SESSION_ID);
    fs::create_dir_all(&session_dir).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/manual/copilot")
        .join(SESSION_ID)
        .join("events.jsonl");
    let events_path = session_dir.join("events.jsonl");
    fs::copy(fixture, &events_path).unwrap();

    fs::write(session_dir.join("workspace.yaml"), "cwd: /work\n").unwrap();
    fs::write(session_dir.join("plan.md"), "# plan\n").unwrap();
    fs::create_dir_all(session_dir.join("checkpoints")).unwrap();
    fs::write(session_dir.join("checkpoints/index.md"), "# index\n").unwrap();
    fs::write(session_dir.join("checkpoints/002-later.md"), "later\n").unwrap();
    fs::write(session_dir.join("checkpoints/001-first.md"), "first\n").unwrap();
    // Non-portable names (Patwari's logical-path validation would reject the whole manifest).
    fs::write(session_dir.join("checkpoints/has space.md"), "no\n").unwrap();
    fs::write(session_dir.join("checkpoints/CON.md"), "no\n").unwrap();
    fs::create_dir_all(session_dir.join("rewind-file-snapshots/backups")).unwrap();
    fs::write(session_dir.join("rewind-file-snapshots/index.json"), "{}\n").unwrap();
    // Excluded kinds: the live SQLite, bulk rewind backups, arbitrary workspace trees, junk.
    fs::write(session_dir.join("session.db"), b"SQLite format 3\0").unwrap();
    fs::write(
        session_dir.join("rewind-file-snapshots/backups/deadbeef"),
        "blob",
    )
    .unwrap();
    fs::create_dir_all(session_dir.join("files/repo")).unwrap();
    fs::write(session_dir.join("files/repo/main.rs"), "fn main() {}\n").unwrap();
    fs::write(session_dir.join(".DS_Store"), "junk").unwrap();
    // A symlinked allowlisted name is refused, and an oversized allowlisted file is skipped.
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hosts", session_dir.join("vscode.metadata.json")).unwrap();
    fs::write(
        session_dir.join("rewind-file-snapshots/tracking.json"),
        vec![b'x'; SIDECAR_MAX_FILE_BYTES + 1],
    )
    .unwrap();

    events_path
}

#[test]
fn capture_honors_the_allowlist_and_read_discipline() {
    let directory = TempDir::new().unwrap();
    let events_path = seeded_session(&directory);

    let sidecars = collect_copilot_sidecars(&events_path);
    let paths: Vec<&str> = sidecars
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec![
            "workspace.yaml",
            "plan.md",
            "checkpoints/001-first.md",
            "checkpoints/002-later.md",
            "checkpoints/index.md",
            "rewind-file-snapshots/index.json",
        ],
        "allowlisted regular files only, checkpoints sorted, symlinked and oversized skipped"
    );
    assert_eq!(sidecars[0].bytes, b"cwd: /work\n");
}

#[test]
fn manual_archive_stages_the_sidecar_set_and_restages_on_revision() {
    let directory = TempDir::new().unwrap();
    let events_path = seeded_session(&directory);
    let project = directory.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let output = directory.path().join("archives");

    let archive = |events: PathBuf| {
        archive_session(&ArchiveConfig {
            reference: SessionReference {
                source: SourceKind::Copilot,
                session_id: Some(SESSION_ID.to_owned()),
                events_path: Some(events),
                copilot_home: None,
            },
            project_directory: project.clone(),
            output_directory: output.clone(),
            summarizer_binary: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/manual/fake-summarizer/success.sh"),
            summarizer_args: Vec::new(),
            summarizer_env: Vec::new(),
            timeout: std::time::Duration::from_secs(2),
            max_source_bytes: 1024 * 1024,
            max_input_bytes: 1024 * 1024,
            max_stdout_bytes: 16 * 1024,
            max_stderr_bytes: 16 * 1024,
            max_event_text_bytes: 64 * 1024,
        })
        .unwrap()
    };

    let ArchiveOutcome::Archived { relative_path, .. } = archive(events_path.clone()) else {
        panic!("expected archived outcome");
    };
    let staged = output.join(relative_path.with_extension("sidecar"));
    assert!(staged.join("workspace.yaml").is_file());
    assert!(staged.join("checkpoints/001-first.md").is_file());
    assert!(staged.join("rewind-file-snapshots/index.json").is_file());
    assert!(
        !staged.join("session.db").exists() && !staged.join("files").exists(),
        "excluded kinds are never staged"
    );

    // A later revision replaces the staged set: a checkpoint the harness deleted disappears.
    fs::remove_file(
        events_path
            .parent()
            .unwrap()
            .join("checkpoints/002-later.md"),
    )
    .unwrap();
    archive(events_path);
    assert!(staged.join("checkpoints/001-first.md").is_file());
    assert!(
        !staged.join("checkpoints/002-later.md").exists(),
        "restaging replaces the set instead of unioning revisions"
    );
}
