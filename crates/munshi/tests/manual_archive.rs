use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use munshi::{
    ArchiveConfig, ArchiveError, ArchiveMetadata, ArchiveOutcome, NormalizedEvent,
    NormalizedSession, ProjectIdentity, SessionReference, SourceError, StructuredSummary,
    archive_session, atomic_replace, inspect_project, normalize_git_remote, render_markdown,
    resolve_session_reference,
};
use munshi_runner::RunnerError;
use tempfile::TempDir;

const NORMAL_ID: &str = "11111111-1111-4111-8111-111111111111";
const UNKNOWN_ID: &str = "22222222-2222-4222-8222-222222222222";
const MALFORMED_ID: &str = "33333333-3333-4333-8333-333333333333";
const CANCELLED_ID: &str = "44444444-4444-4444-8444-444444444444";

#[test]
fn normal_session_archives_to_deterministic_golden_markdown() {
    let directory = test_directory();
    let project = git_project(directory.path());
    let output = directory.path().join("archives");

    let outcome = archive_session(&config(
        NORMAL_ID,
        fixture_events(NORMAL_ID),
        &project,
        &output,
        fake("success.sh"),
    ))
    .unwrap();

    let ArchiveOutcome::Archived { id, relative_path } = outcome else {
        panic!("expected archived outcome");
    };
    assert_eq!(id, format!("copilot:{NORMAL_ID}"));
    let markdown = fs::read_to_string(output.join(relative_path)).unwrap();
    assert_eq!(
        markdown,
        include_str!("../../../fixtures/manual/expected/normal.md")
    );
    assert!(!markdown.contains(directory.path().to_string_lossy().as_ref()));
}

#[test]
fn unknown_events_are_ignored_without_exposing_their_content() {
    let directory = test_directory();
    let project = git_project(directory.path());
    let output = directory.path().join("archives");

    let outcome = archive_session(&config(
        UNKNOWN_ID,
        fixture_events(UNKNOWN_ID),
        &project,
        &output,
        fake("success.sh"),
    ))
    .unwrap();

    let ArchiveOutcome::Archived { relative_path, .. } = outcome else {
        panic!("expected archived outcome");
    };
    let markdown = fs::read_to_string(output.join(relative_path)).unwrap();
    assert!(!markdown.contains("unknown event content"));
}

#[test]
fn malformed_jsonl_fails_closed_and_writes_nothing() {
    let directory = test_directory();
    let output = directory.path().join("archives");
    let error = archive_session(&config(
        MALFORMED_ID,
        fixture_events(MALFORMED_ID),
        directory.path(),
        &output,
        fake("success.sh"),
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        ArchiveError::Source(SourceError::MalformedJson { line: 2 })
    ));
    assert!(!output.exists());
}

#[test]
fn non_archive_worthy_session_is_an_explicit_noop_before_summarization() {
    let directory = test_directory();
    let output = directory.path().join("archives");
    let outcome = archive_session(&config(
        CANCELLED_ID,
        fixture_events(CANCELLED_ID),
        directory.path(),
        &output,
        directory.path().join("does-not-exist"),
    ))
    .unwrap();

    assert_eq!(
        outcome,
        ArchiveOutcome::NotArchiveWorthy {
            id: format!("copilot:{CANCELLED_ID}")
        }
    );
    assert!(!output.exists());
}

#[test]
fn archive_predicate_uses_only_authoritative_content_and_tool_events() {
    let directory = test_directory();
    for (session_id, lines) in [
        (
            "55555555-5555-4555-8555-555555555555",
            concat!(
                "{\"type\":\"user.message\",\"data\":{\"content\":\"A real request.\"}}\n",
                "{\"type\":\"assistant.message\",\"data\":{\"content\":\" \",\"messageId\":\"m\",\"toolRequests\":[{\"name\":\"fake\"}]}}\n"
            ),
        ),
        (
            "66666666-6666-4666-8666-666666666666",
            concat!(
                "{\"type\":\"user.message\",\"data\":{\"prompt\":\"An unsupported alias.\"}}\n",
                "{\"type\":\"assistant.message\",\"data\":{\"content\":\"Agent text.\",\"messageId\":\"m\"}}\n"
            ),
        ),
    ] {
        let events = directory.path().join(session_id).join("events.jsonl");
        fs::create_dir_all(events.parent().unwrap()).unwrap();
        fs::write(&events, lines).unwrap();
        let outcome = archive_session(&config(
            session_id,
            events,
            directory.path(),
            &directory.path().join(format!("archives-{session_id}")),
            directory.path().join("does-not-exist"),
        ))
        .unwrap();
        assert!(matches!(outcome, ArchiveOutcome::NotArchiveWorthy { .. }));
    }
}

#[test]
fn malformed_or_partial_summary_never_creates_markdown() {
    let directory = test_directory();
    let project = git_project(directory.path());
    for (script, expected) in [
        ("malformed.sh", "not one valid JSON object"),
        ("empty-work.sh", "must contain at least one item"),
    ] {
        let output = directory.path().join(format!("archives-{script}"));
        let error = archive_session(&config(
            NORMAL_ID,
            fixture_events(NORMAL_ID),
            &project,
            &output,
            fake(script),
        ))
        .unwrap_err();

        assert!(error.to_string().contains(expected));
        assert!(!output.exists());
    }
}

#[test]
fn summary_process_timeout_nonzero_and_bounds_are_explicit_and_redacted() {
    let directory = test_directory();
    let project = git_project(directory.path());
    for (script, timeout, stdout_limit, expected) in [
        ("timeout.sh", 50, 4096, "timed out"),
        ("nonzero.sh", 2_000, 4096, "exited unsuccessfully"),
        ("oversized.sh", 2_000, 32, "stdout"),
    ] {
        let output = directory.path().join(format!("archives-{script}"));
        let mut config = config(
            NORMAL_ID,
            fixture_events(NORMAL_ID),
            &project,
            &output,
            fake(script),
        );
        config.timeout = Duration::from_millis(timeout);
        config.max_stdout_bytes = stdout_limit;

        let error = archive_session(&config).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(expected), "{message}");
        assert!(!message.contains("synthetic private stderr"));
        assert!(!output.exists());
        assert!(matches!(
            error,
            ArchiveError::Summary(munshi::SummaryError::Runner(
                RunnerError::Timeout(_)
                    | RunnerError::NonZeroExit { .. }
                    | RunnerError::OutputLimit { .. }
            ))
        ));
    }
}

#[test]
fn normalized_summarizer_input_is_bounded_before_process_spawn() {
    let directory = test_directory();
    let project = git_project(directory.path());
    let output = directory.path().join("archives");
    let mut config = config(
        NORMAL_ID,
        fixture_events(NORMAL_ID),
        &project,
        &output,
        directory.path().join("does-not-exist"),
    );
    config.max_input_bytes = 64;

    let error = archive_session(&config).unwrap_err();

    assert!(matches!(
        error,
        ArchiveError::Summary(munshi::SummaryError::InputLimit { limit: 64 })
    ));
    assert!(!output.exists());
}

#[test]
fn project_remote_normalization_and_local_fallback_are_stable_and_safe() {
    assert_eq!(
        normalize_git_remote("git@github.com:surdy/munshi.git"),
        Some("github.com/surdy/munshi".to_owned())
    );
    assert_eq!(
        normalize_git_remote("https://token@github.com/surdy/munshi.git/"),
        Some("github.com/surdy/munshi".to_owned())
    );

    let directory = test_directory();
    let local_project = directory.path().join("local-project");
    fs::create_dir_all(&local_project).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&local_project)
            .status()
            .unwrap()
            .success()
    );
    let first = inspect_project(&local_project).unwrap();
    let second = inspect_project(&local_project).unwrap();
    assert_eq!(first, second);
    assert!(first.identity.starts_with("local:sha256:"));
    assert!(
        !first
            .identity
            .contains(directory.path().to_string_lossy().as_ref())
    );
    assert!(!first.component.contains('/'));
}

#[test]
fn session_id_fallback_is_confined_to_copilot_session_state() {
    let directory = test_directory();
    let home = directory.path().join("copilot-home");
    let events = home
        .join("session-state")
        .join(NORMAL_ID)
        .join("events.jsonl");
    fs::create_dir_all(events.parent().unwrap()).unwrap();
    fs::copy(fixture_events(NORMAL_ID), &events).unwrap();

    let resolved = resolve_session_reference(&SessionReference {
        session_id: Some(NORMAL_ID.to_owned()),
        events_path: None,
        copilot_home: Some(home),
    })
    .unwrap();

    assert_eq!(resolved.session_id, NORMAL_ID);
    assert_eq!(resolved.events_path, events.canonicalize().unwrap());

    let explicit_only = resolve_session_reference(&SessionReference {
        session_id: None,
        events_path: Some(fixture_events(NORMAL_ID)),
        copilot_home: None,
    })
    .unwrap();
    assert_eq!(explicit_only.session_id, NORMAL_ID);
}

#[test]
fn renderer_is_deterministic_and_atomic_replace_replaces_existing_file() {
    let session = NormalizedSession {
        session_id: "stable-session".to_owned(),
        events: vec![NormalizedEvent {
            kind: "user",
            content: "synthetic".to_owned(),
        }],
        user_requests: 1,
        assistant_messages: 1,
        tool_activities: 0,
        ignored_events: 0,
        source_cursor: 9,
        source_hash: "sha256:abc".to_owned(),
        started_at: Some("2026-07-12T00:00:00.000Z".to_owned()),
        updated_at: Some("2026-07-12T00:01:00.000Z".to_owned()),
    };
    let project = ProjectIdentity {
        identity: "github.com/surdy/munshi".to_owned(),
        component: "munshi-fixed".to_owned(),
        project: "munshi".to_owned(),
        repository: Some("surdy/munshi".to_owned()),
        branch: Some("main".to_owned()),
    };
    let summary = StructuredSummary {
        title: "Stable title".to_owned(),
        goal: "Stable goal.".to_owned(),
        work_completed: vec!["Completed work.".to_owned()],
        decisions: Vec::new(),
        files_changed: Vec::new(),
        commands_and_validation: vec!["cargo test".to_owned()],
        open_items: Vec::new(),
        tags: vec!["rust".to_owned()],
    };
    let metadata = ArchiveMetadata {
        session: &session,
        project: &project,
    };
    let first = render_markdown(&metadata, &summary);
    let second = render_markdown(&metadata, &summary);
    assert_eq!(first, second);

    let directory = test_directory();
    let output = directory.path().join("nested/archive.md");
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    fs::write(&output, "old").unwrap();
    atomic_replace(&output, first.as_bytes()).unwrap();
    assert_eq!(fs::read_to_string(&output).unwrap(), first);
    assert!(
        fs::read_dir(output.parent().unwrap())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".munshi-"))
    );
}

#[test]
fn cli_returns_distinct_noop_exit_code_without_transcript_output() {
    let directory = test_directory();
    let output = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("archive")
        .arg(CANCELLED_ID)
        .arg("--events")
        .arg(fixture_events(CANCELLED_ID))
        .arg("--project-dir")
        .arg(directory.path())
        .arg("--output-dir")
        .arg(directory.path().join("archives"))
        .arg("--summarizer")
        .arg(directory.path().join("does-not-exist"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not archive-worthy"));
    assert!(!stderr.contains("A cancelled request"));
}

fn config(
    session_id: &str,
    events_path: PathBuf,
    project_directory: &Path,
    output_directory: &Path,
    summarizer_binary: PathBuf,
) -> ArchiveConfig {
    ArchiveConfig {
        reference: SessionReference {
            session_id: Some(session_id.to_owned()),
            events_path: Some(events_path),
            copilot_home: None,
        },
        project_directory: project_directory.to_path_buf(),
        output_directory: output_directory.to_path_buf(),
        summarizer_binary,
        summarizer_args: Vec::<OsString>::new(),
        timeout: Duration::from_secs(2),
        max_source_bytes: 1024 * 1024,
        max_input_bytes: 1024 * 1024,
        max_stdout_bytes: 16 * 1024,
        max_stderr_bytes: 4 * 1024,
    }
}

fn git_project(parent: &Path) -> PathBuf {
    let project = parent.join("project");
    fs::create_dir_all(&project).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&project)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["remote", "add", "origin", "git@github.com:surdy/munshi.git"])
            .status()
            .unwrap()
            .success()
    );
    project
}

fn fixture_events(session_id: &str) -> PathBuf {
    fixture_root()
        .join("copilot")
        .join(session_id)
        .join("events.jsonl")
}

fn fake(name: &str) -> PathBuf {
    fixture_root().join("fake-summarizer").join(name)
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/manual")
}

fn test_directory() -> TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-test-artifacts");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(root)
        .unwrap()
}
