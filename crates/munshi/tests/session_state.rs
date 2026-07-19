use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use munshi::{
    ArchiveMetadata, CompletionReason, SessionReference, StateStore, StructuredSummary,
    atomic_replace, content_hash, inspect_project, load_session, parse_archive_markdown,
    render_markdown, render_revision_markdown, resolve_session_reference,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;

const SESSION_A: &str = "11111111-1111-4111-8111-111111111111";
const SESSION_B: &str = "22222222-2222-4222-8222-222222222222";
const SESSION_PREFIX_SHORT: &str = "session-1";
const SESSION_PREFIX_LONG: &str = "session-10";

#[test]
fn registration_migrates_schema_idempotently_and_uses_wal() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("schema-count");
    assert_success(&harness.register(&summarizer, 10_000));
    assert_success(&harness.register(&summarizer, 10_000));

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let version: i64 = connection
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    let journal: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let foreign_keys: i64 = connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    assert_eq!(journal, "wal");
    assert_eq!(foreign_keys, 1);
    assert!(!harness.state.join("pending").exists());
    assert!(!harness.state.join("workers").exists());
}

#[test]
fn resumed_delta_revises_stable_path_and_same_source_is_a_noop() {
    let harness = Harness::new();
    let count = "resume-count";
    let summarizer = harness.revision_summarizer(count);
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");

    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let archive_path = harness.archive_path(SESSION_A);
    let first = parse_archive_markdown(&fs::read_to_string(&archive_path).unwrap()).unwrap();
    assert_eq!(first.summary_revision, 1);

    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 20, 21);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let second = parse_archive_markdown(&fs::read_to_string(&archive_path).unwrap()).unwrap();
    assert_eq!(second.summary_revision, 2);
    assert_eq!(second.summary.title, "Revised stable session title");
    assert_eq!(second.cursor.as_ref().unwrap().record_count, 5);
    assert_eq!(
        fs::read_to_string(harness.root().join(count)).unwrap(),
        "xx"
    );

    harness.complete_lifecycle(SESSION_A, &transcript, 30, 31);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let unchanged = parse_archive_markdown(&fs::read_to_string(&archive_path).unwrap()).unwrap();
    assert_eq!(unchanged.summary_revision, 2);
    assert_eq!(
        fs::read_to_string(harness.root().join(count)).unwrap(),
        "xx"
    );
    assert_eq!(archive_path, harness.archive_path(SESSION_A));
}

#[test]
fn git_history_commits_revisions_in_archive_repo_only() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("git-history-count");
    fs::write(harness.project.join("source.md"), "source baseline\n").unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&harness.project)
            .args(["add", "source.md"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&harness.project)
            .args([
                "-c",
                "user.name=Source",
                "-c",
                "user.email=source@example.com",
                "commit",
                "-q",
                "-m",
                "source baseline",
            ])
            .status()
            .unwrap()
            .success()
    );
    let source_commits_before = harness.commit_count(&harness.project);

    assert_success(&harness.register_with_git_history(&summarizer, 10_000, true));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert_eq!(harness.archive_commit_count(), 1);
    let relative_archive = harness
        .archive_path(SESSION_A)
        .strip_prefix(&harness.output)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        harness.archive_latest_commit_files(),
        vec![relative_archive.clone()]
    );
    let first_message = harness.archive_latest_commit_message();
    assert!(first_message.contains(&format!("session_id: {SESSION_A}")));
    assert!(first_message.contains("summary_revision: 1"));

    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 20, 21);
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert_eq!(harness.archive_commit_count(), 2);
    let second_message = harness.archive_latest_commit_message();
    assert!(second_message.contains("summary_revision: 2"));

    harness.complete_lifecycle(SESSION_A, &transcript, 30, 31);
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert_eq!(harness.archive_commit_count(), 2);

    assert_eq!(
        harness.commit_count(&harness.project),
        source_commits_before
    );
    assert!(harness.git_status_porcelain(&harness.project).is_empty());
}

#[test]
fn git_history_rejects_origin_source_repository_output() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("source-repo-reject-count");
    assert_success(&harness.register_with_output(&summarizer, 10_000, &harness.project, true));
    let source_commits_before = harness.commit_count(&harness.project);
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    let waited = harness.wait(SESSION_A, 5_000);
    assert!(!waited.status.success());
    assert!(String::from_utf8_lossy(&waited.stdout).contains("archive-git-source-repo"));
    assert_eq!(
        harness.commit_count(&harness.project),
        source_commits_before
    );
}

#[test]
fn truncation_and_rewrite_force_full_resummaries_with_safe_reasons() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("fallback-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));

    harness.replace_transcript(SESSION_A, "TRUNCATED_REQUEST", "short");
    harness.complete_lifecycle(SESSION_A, &transcript, 20, 21);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let archive_path = harness.archive_path(SESSION_A);
    let truncated = parse_archive_markdown(&fs::read_to_string(&archive_path).unwrap()).unwrap();
    assert_eq!(truncated.summary_revision, 2);
    assert_eq!(
        truncated.cursor_fallback_reason.as_deref(),
        Some("source-truncated")
    );
    assert_eq!(truncated.summary.title, "Truncation fallback summary");

    harness.replace_transcript(
        SESSION_A,
        "REWRITE_REQUEST",
        &"rewritten answer ".repeat(100),
    );
    harness.complete_lifecycle(SESSION_A, &transcript, 30, 31);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let rewritten = parse_archive_markdown(&fs::read_to_string(&archive_path).unwrap()).unwrap();
    assert_eq!(rewritten.summary_revision, 3);
    assert_eq!(
        rewritten.cursor_fallback_reason.as_deref(),
        Some("cursor-mismatch")
    );
    assert_eq!(rewritten.summary.title, "Rewrite fallback summary");
    assert_eq!(
        fs::read_to_string(harness.root().join("fallback-count")).unwrap(),
        "xxx"
    );
}

#[test]
fn git_commit_failure_does_not_advance_state_or_leave_staged_changes() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("git-failure-count");
    assert_success(&harness.register_with_git_history(&summarizer, 10_000, true));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let before_cursor = session_cursor(&harness, SESSION_A);
    let archive_path = harness.archive_path(SESSION_A);
    let before_markdown = fs::read(&archive_path).unwrap();
    assert_eq!(harness.archive_commit_count(), 1);

    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.queue_direct(SESSION_A, &transcript, 20, 21);
    let git_directory = harness.output.join(".git");
    fs::set_permissions(&git_directory, fs::Permissions::from_mode(0o500)).unwrap();
    let worker = harness.worker(SESSION_A);
    fs::set_permissions(&git_directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(worker.status.success());

    let waited = harness.wait(SESSION_A, 5_000);
    assert!(!waited.status.success());
    assert!(String::from_utf8_lossy(&waited.stdout).contains("archive-git"));
    assert_eq!(session_cursor(&harness, SESSION_A), before_cursor);
    assert_eq!(fs::read(&archive_path).unwrap(), before_markdown);
    assert_eq!(harness.archive_commit_count(), 1);
    assert!(harness.git_status_porcelain(&harness.output).is_empty());
}

#[test]
fn malformed_delta_fails_without_advancing_revision_or_cursor() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("malformed-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let archive_path = harness.archive_path(SESSION_A);
    let before_markdown = fs::read(&archive_path).unwrap();
    let before = session_cursor(&harness, SESSION_A);

    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(b"{malformed\n")
        .unwrap();
    harness.complete_lifecycle(SESSION_A, &transcript, 20, 21);
    let waited = harness.wait(SESSION_A, 5_000);
    assert!(!waited.status.success());
    assert!(String::from_utf8_lossy(&waited.stdout).contains("\"status\":\"failed\""));
    assert_eq!(session_cursor(&harness, SESSION_A), before);
    assert_eq!(fs::read(&archive_path).unwrap(), before_markdown);
}

#[test]
fn unterminated_trailing_record_is_retryable_without_cursor_advancement() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("partial-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let archive_path = harness.archive_path(SESSION_A);
    let before_markdown = fs::read(&archive_path).unwrap();
    let before = session_cursor(&harness, SESSION_A);

    fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap()
        .write_all(br#"{"id":"partial""#)
        .unwrap();
    harness.complete_lifecycle(SESSION_A, &transcript, 20, 21);
    let waited = harness.wait(SESSION_A, 5_000);
    assert!(!waited.status.success());
    assert!(String::from_utf8_lossy(&waited.stdout).contains("source-incomplete"));
    assert_eq!(session_cursor(&harness, SESSION_A), before);
    assert_eq!(fs::read(&archive_path).unwrap(), before_markdown);
}

#[test]
fn unworthy_full_fallback_preserves_archive_with_a_distinct_failure() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("unworthy-rewrite-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let archive_path = harness.archive_path(SESSION_A);
    let before_markdown = fs::read(&archive_path).unwrap();
    let before = session_cursor(&harness, SESSION_A);
    fs::write(
        &transcript,
        format!(
            "{}\n",
            json!({
                "id": "rewritten-start",
                "timestamp": "2026-07-12T00:02:00.000Z",
                "parentId": null,
                "type": "session.start",
                "data": {"sessionId": SESSION_A},
            })
        ),
    )
    .unwrap();

    harness.complete_lifecycle(SESSION_A, &transcript, 20, 21);
    let waited = harness.wait(SESSION_A, 5_000);
    assert!(!waited.status.success());
    assert!(String::from_utf8_lossy(&waited.stdout).contains("source-not-archive-worthy"));
    assert_eq!(session_cursor(&harness, SESSION_A), before);
    assert_eq!(fs::read(&archive_path).unwrap(), before_markdown);
}

#[test]
fn interrupted_session_end_uses_guarded_fallback_and_records_reason() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("interrupted-count");
    assert_success(&harness.register(&summarizer, 10_000));
    harness.write_transcript(SESSION_A, "INTERRUPTED_REQUEST", "interrupted answer");

    let payload = json!({
        "sessionId": SESSION_A,
        "timestamp": 11_u64,
        "cwd": harness.project,
        "reason": "user_exit",
    });
    assert_success(&harness.hook("session-end", &payload));
    assert_success(&harness.wait(SESSION_A, 5_000));

    let archived =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_A)).unwrap())
            .unwrap();
    assert_eq!(archived.completion_reason, "interrupted");
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let source: String = connection
        .query_row(
            "SELECT transcript_source FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source, "version-pinned-fallback");
}

#[test]
fn unresolved_interrupted_session_is_retried_when_the_guarded_path_appears() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("late-fallback-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let payload = json!({
        "sessionId": SESSION_A,
        "timestamp": 11_u64,
        "cwd": harness.project,
        "reason": "user_exit",
    });
    assert_success(&harness.hook("session-end", &payload));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let unresolved: (String, Option<String>) = connection
        .query_row(
            "SELECT lifecycle_state,transcript_path
             FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(unresolved, ("interrupted".to_owned(), None));
    drop(connection);

    harness.write_transcript(SESSION_A, "LATE_REQUEST", "late answer");
    assert_success(&harness.recover(0, true, false));
    assert_success(&harness.wait(SESSION_A, 5_000));
    let archived =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_A)).unwrap())
            .unwrap();
    assert_eq!(archived.completion_reason, "interrupted");
}

#[test]
fn explicit_recovery_archives_a_known_force_closed_session() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("force-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "FORCE_REQUEST", "force answer");
    assert_success(&harness.hook(
        "agent-stop",
        &harness.agent_stop_payload(SESSION_A, &transcript, 10),
    ));

    assert_success(&harness.recover(0, true, false));
    assert_success(&harness.wait(SESSION_A, 5_000));
    let archived =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_A)).unwrap())
            .unwrap();
    assert_eq!(archived.completion_reason, "unknown");
}

#[test]
fn stale_processing_claim_is_recovered_and_retried() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("stale-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "STALE_REQUEST", "stale answer");
    harness.queue_direct(SESSION_A, &transcript, 10, 11);

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let (database_id, generation): (i64, i64) = connection
        .query_row(
            "SELECT id,state_generation FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO processing_attempts(
                session_id,state_generation,retry_state,lease_token,owner_pid,
                started_at_ms,lease_expires_at_ms,outcome
             ) VALUES (?1,?2,'summary-pending','stale-token',999999,1,2,'processing')",
            params![database_id, generation],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET lifecycle_state='processing',retry_state='summary-pending',
                claim_token='stale-token',claim_started_at_ms=1,
                worker_generation=NULL,worker_spawned_at_ms=NULL
             WHERE id=?1",
            [database_id],
        )
        .unwrap();
    drop(connection);

    assert_success(&harness.recover(0, true, false));
    assert_success(&harness.wait(SESSION_A, 5_000));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let outcomes: Vec<String> = connection
        .prepare("SELECT outcome FROM processing_attempts ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(outcomes, ["failed", "succeeded"]);
}

#[test]
fn post_persist_worker_crash_is_reconciled_without_second_summary_call() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("reconcile-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.queue_direct(SESSION_A, &transcript, 20, 21);

    let resolved = resolve_session_reference(&SessionReference {
        source: munshi::SourceKind::Copilot,
        session_id: Some(SESSION_A.to_owned()),
        events_path: Some(transcript),
        copilot_home: None,
    })
    .unwrap();
    let session = load_session(&resolved, 1024 * 1024).unwrap();
    let project = inspect_project(&harness.project).unwrap();
    let summary = StructuredSummary {
        title: "Recovered persisted revision".to_owned(),
        goal: "Finalize a revision written before a worker crash.".to_owned(),
        work_completed: vec!["Persisted Markdown before the simulated crash.".to_owned()],
        decisions: Vec::new(),
        files_changed: Vec::new(),
        commands_and_validation: Vec::new(),
        open_items: Vec::new(),
        tags: vec!["recovery".to_owned()],
    };
    let markdown = render_revision_markdown(
        &ArchiveMetadata {
            session: &session,
            project: &project,
        },
        &summary,
        2,
        "complete",
        None,
    );
    let archive_path = harness.archive_path(SESSION_A);
    atomic_replace(&archive_path, markdown.as_bytes()).unwrap();
    let relative = archive_path.strip_prefix(&harness.output).unwrap();
    let markdown_hash = content_hash(markdown.as_bytes());

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let (database_id, generation): (i64, i64) = connection
        .query_row(
            "SELECT id,state_generation FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO processing_attempts(
                session_id,state_generation,retry_state,lease_token,owner_pid,
                started_at_ms,lease_expires_at_ms,outcome,
                planned_revision,planned_record_count,planned_byte_offset,
                planned_prefix_hash,planned_source_hash,planned_source_bytes,
                planned_markdown_relative_path,planned_markdown_hash,
                planned_completion_reason
             ) VALUES (
                ?1,?2,'revision-pending','post-persist-token',999999,1,2,'processing',
                2,?3,?4,?5,?6,?7,?8,?9,'complete'
             )",
            params![
                database_id,
                generation,
                session.source_cursor as i64,
                session.source_byte_cursor as i64,
                session.source_prefix_hash,
                session.source_hash,
                session.source_bytes as i64,
                relative.to_string_lossy(),
                markdown_hash,
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET lifecycle_state='processing',retry_state='revision-pending',
                claim_token='post-persist-token',claim_started_at_ms=1,
                worker_generation=NULL,worker_spawned_at_ms=NULL
             WHERE id=?1",
            [database_id],
        )
        .unwrap();
    drop(connection);

    assert_success(&harness.recover(0, true, false));
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert_eq!(
        fs::read_to_string(harness.root().join("reconcile-count")).unwrap(),
        "x"
    );
    let archived = parse_archive_markdown(&fs::read_to_string(archive_path).unwrap()).unwrap();
    assert_eq!(archived.summary_revision, 2);
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let outcome: String = connection
        .query_row(
            "SELECT outcome FROM processing_attempts ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outcome, "recovered");
}

#[test]
fn post_persist_recovery_creates_missing_git_commit() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("reconcile-missing-git-count");
    assert_success(&harness.register_with_git_history(&summarizer, 10_000, true));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.queue_direct(SESSION_A, &transcript, 20, 21);

    let resolved = resolve_session_reference(&SessionReference {
        source: munshi::SourceKind::Copilot,
        session_id: Some(SESSION_A.to_owned()),
        events_path: Some(transcript),
        copilot_home: None,
    })
    .unwrap();
    let session = load_session(&resolved, 1024 * 1024).unwrap();
    let project = inspect_project(&harness.project).unwrap();
    let summary = StructuredSummary {
        title: "Recovered persisted revision".to_owned(),
        goal: "Finalize a revision written before a worker crash.".to_owned(),
        work_completed: vec!["Persisted Markdown before the simulated crash.".to_owned()],
        decisions: Vec::new(),
        files_changed: Vec::new(),
        commands_and_validation: Vec::new(),
        open_items: Vec::new(),
        tags: vec!["recovery".to_owned()],
    };
    let markdown = render_revision_markdown(
        &ArchiveMetadata {
            session: &session,
            project: &project,
        },
        &summary,
        2,
        "complete",
        None,
    );
    let archive_path = harness.archive_path(SESSION_A);
    atomic_replace(&archive_path, markdown.as_bytes()).unwrap();
    let relative = archive_path.strip_prefix(&harness.output).unwrap();
    let markdown_hash = content_hash(markdown.as_bytes());
    assert_eq!(harness.archive_commit_count(), 1);

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let (database_id, generation): (i64, i64) = connection
        .query_row(
            "SELECT id,state_generation FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO processing_attempts(
                session_id,state_generation,retry_state,lease_token,owner_pid,
                started_at_ms,lease_expires_at_ms,outcome,
                planned_revision,planned_record_count,planned_byte_offset,
                planned_prefix_hash,planned_source_hash,planned_source_bytes,
                planned_markdown_relative_path,planned_markdown_hash,
                planned_archive_git_history,planned_completion_reason
             ) VALUES (
                ?1,?2,'revision-pending','post-persist-git-missing-token',999999,1,2,'processing',
                2,?3,?4,?5,?6,?7,?8,?9,1,'complete'
             )",
            params![
                database_id,
                generation,
                session.source_cursor as i64,
                session.source_byte_cursor as i64,
                session.source_prefix_hash,
                session.source_hash,
                session.source_bytes as i64,
                relative.to_string_lossy(),
                markdown_hash,
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET lifecycle_state='processing',retry_state='revision-pending',
                claim_token='post-persist-git-missing-token',claim_started_at_ms=1,
                worker_generation=NULL,worker_spawned_at_ms=NULL
             WHERE id=?1",
            [database_id],
        )
        .unwrap();
    drop(connection);

    assert_success(&harness.recover(0, true, false));
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert_eq!(
        fs::read_to_string(harness.root().join("reconcile-missing-git-count")).unwrap(),
        "x"
    );
    assert_eq!(harness.archive_commit_count(), 2);
    let latest = harness.archive_latest_commit_message();
    assert!(latest.contains(&format!("session_id: {SESSION_A}")));
    assert!(latest.contains("summary_revision: 2"));
}

#[test]
fn post_persist_recovery_skips_duplicate_git_commit() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("reconcile-existing-git-count");
    assert_success(&harness.register_with_git_history(&summarizer, 10_000, true));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.queue_direct(SESSION_A, &transcript, 20, 21);

    let resolved = resolve_session_reference(&SessionReference {
        source: munshi::SourceKind::Copilot,
        session_id: Some(SESSION_A.to_owned()),
        events_path: Some(transcript),
        copilot_home: None,
    })
    .unwrap();
    let session = load_session(&resolved, 1024 * 1024).unwrap();
    let project = inspect_project(&harness.project).unwrap();
    let summary = StructuredSummary {
        title: "Recovered persisted revision".to_owned(),
        goal: "Finalize a revision written before a worker crash.".to_owned(),
        work_completed: vec!["Persisted Markdown before the simulated crash.".to_owned()],
        decisions: Vec::new(),
        files_changed: Vec::new(),
        commands_and_validation: Vec::new(),
        open_items: Vec::new(),
        tags: vec!["recovery".to_owned()],
    };
    let markdown = render_revision_markdown(
        &ArchiveMetadata {
            session: &session,
            project: &project,
        },
        &summary,
        2,
        "complete",
        None,
    );
    let archive_path = harness.archive_path(SESSION_A);
    atomic_replace(&archive_path, markdown.as_bytes()).unwrap();
    let relative = archive_path.strip_prefix(&harness.output).unwrap();
    let markdown_hash = content_hash(markdown.as_bytes());
    harness.commit_archive_revision(relative, SESSION_A, 2);
    assert_eq!(harness.archive_commit_count(), 2);

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let (database_id, generation): (i64, i64) = connection
        .query_row(
            "SELECT id,state_generation FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO processing_attempts(
                session_id,state_generation,retry_state,lease_token,owner_pid,
                started_at_ms,lease_expires_at_ms,outcome,
                planned_revision,planned_record_count,planned_byte_offset,
                planned_prefix_hash,planned_source_hash,planned_source_bytes,
                planned_markdown_relative_path,planned_markdown_hash,
                planned_archive_git_history,planned_completion_reason
             ) VALUES (
                ?1,?2,'revision-pending','post-persist-git-existing-token',999999,1,2,'processing',
                2,?3,?4,?5,?6,?7,?8,?9,1,'complete'
             )",
            params![
                database_id,
                generation,
                session.source_cursor as i64,
                session.source_byte_cursor as i64,
                session.source_prefix_hash,
                session.source_hash,
                session.source_bytes as i64,
                relative.to_string_lossy(),
                markdown_hash,
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET lifecycle_state='processing',retry_state='revision-pending',
                claim_token='post-persist-git-existing-token',claim_started_at_ms=1,
                worker_generation=NULL,worker_spawned_at_ms=NULL
             WHERE id=?1",
            [database_id],
        )
        .unwrap();
    drop(connection);

    assert_success(&harness.recover(0, true, false));
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert_eq!(
        fs::read_to_string(harness.root().join("reconcile-existing-git-count")).unwrap(),
        "x"
    );
    assert_eq!(harness.archive_commit_count(), 2);
}

#[test]
fn post_persist_recovery_uses_exact_commit_trailer_matching_for_prefix_session_ids() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("reconcile-prefix-collision-count");
    assert_success(&harness.register_with_git_history(&summarizer, 10_000, true));

    let transcript_long = harness.write_transcript(
        SESSION_PREFIX_LONG,
        "INITIAL_REQUEST",
        "long initial answer",
    );
    harness.complete_lifecycle(SESSION_PREFIX_LONG, &transcript_long, 10, 11);
    assert_success(&harness.wait(SESSION_PREFIX_LONG, 5_000));
    harness.append_turn(&transcript_long, "DELTA_REQUEST", "long resumed answer");
    harness.complete_lifecycle(SESSION_PREFIX_LONG, &transcript_long, 20, 21);
    assert_success(&harness.wait(SESSION_PREFIX_LONG, 5_000));
    let long_archive_path = harness.archive_path(SESSION_PREFIX_LONG);
    let long_relative = long_archive_path
        .strip_prefix(&harness.output)
        .unwrap()
        .to_path_buf();
    assert_eq!(
        harness.archive_commit_match_count(&long_relative, SESSION_PREFIX_LONG, 2),
        1
    );

    let transcript_short = harness.write_transcript(
        SESSION_PREFIX_SHORT,
        "INITIAL_REQUEST",
        "short initial answer",
    );
    harness.complete_lifecycle(SESSION_PREFIX_SHORT, &transcript_short, 30, 31);
    assert_success(&harness.wait(SESSION_PREFIX_SHORT, 5_000));
    harness.append_turn(&transcript_short, "DELTA_REQUEST", "short resumed answer");
    harness.queue_direct(SESSION_PREFIX_SHORT, &transcript_short, 40, 41);

    let resolved = resolve_session_reference(&SessionReference {
        source: munshi::SourceKind::Copilot,
        session_id: Some(SESSION_PREFIX_SHORT.to_owned()),
        events_path: Some(transcript_short),
        copilot_home: None,
    })
    .unwrap();
    let session = load_session(&resolved, 1024 * 1024).unwrap();
    let project = inspect_project(&harness.project).unwrap();
    let summary = StructuredSummary {
        title: "Recovered persisted revision".to_owned(),
        goal: "Finalize a revision written before a worker crash.".to_owned(),
        work_completed: vec!["Persisted Markdown before the simulated crash.".to_owned()],
        decisions: Vec::new(),
        files_changed: Vec::new(),
        commands_and_validation: Vec::new(),
        open_items: Vec::new(),
        tags: vec!["recovery".to_owned()],
    };
    let markdown = render_revision_markdown(
        &ArchiveMetadata {
            session: &session,
            project: &project,
        },
        &summary,
        2,
        "complete",
        None,
    );
    atomic_replace(&long_archive_path, markdown.as_bytes()).unwrap();
    let markdown_hash = content_hash(markdown.as_bytes());
    assert_eq!(
        harness.archive_commit_match_count(&long_relative, SESSION_PREFIX_SHORT, 2),
        0
    );

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let (database_id, generation): (i64, i64) = connection
        .query_row(
            "SELECT id,state_generation FROM sessions WHERE source_session_id=?1",
            [SESSION_PREFIX_SHORT],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO processing_attempts(
                session_id,state_generation,retry_state,lease_token,owner_pid,
                started_at_ms,lease_expires_at_ms,outcome,
                planned_revision,planned_record_count,planned_byte_offset,
                planned_prefix_hash,planned_source_hash,planned_source_bytes,
                planned_markdown_relative_path,planned_markdown_hash,
                planned_archive_git_history,planned_completion_reason
             ) VALUES (
                ?1,?2,'revision-pending','post-persist-prefix-collision-token',999999,1,2,'processing',
                2,?3,?4,?5,?6,?7,?8,?9,1,'complete'
             )",
            params![
                database_id,
                generation,
                session.source_cursor as i64,
                session.source_byte_cursor as i64,
                session.source_prefix_hash,
                session.source_hash,
                session.source_bytes as i64,
                long_relative.to_string_lossy(),
                markdown_hash,
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET lifecycle_state='processing',retry_state='revision-pending',
                claim_token='post-persist-prefix-collision-token',claim_started_at_ms=1,
                worker_generation=NULL,worker_spawned_at_ms=NULL
             WHERE id=?1",
            [database_id],
        )
        .unwrap();
    drop(connection);

    let commits_before = harness.archive_commit_count();
    assert_success(&harness.recover(0, true, false));
    assert_success(&harness.wait(SESSION_PREFIX_SHORT, 5_000));
    assert_eq!(harness.archive_commit_count(), commits_before + 1);
    assert_eq!(
        harness.archive_commit_match_count(&long_relative, SESSION_PREFIX_LONG, 2),
        1
    );
    assert_eq!(
        harness.archive_commit_match_count(&long_relative, SESSION_PREFIX_SHORT, 2),
        1
    );

    assert_success(&harness.recover(0, true, false));
    assert_success(&harness.wait(SESSION_PREFIX_SHORT, 5_000));
    assert_eq!(harness.archive_commit_count(), commits_before + 1);
    assert_eq!(
        harness.archive_commit_match_count(&long_relative, SESSION_PREFIX_LONG, 2),
        1
    );
    assert_eq!(
        harness.archive_commit_match_count(&long_relative, SESSION_PREFIX_SHORT, 2),
        1
    );
}

#[test]
fn two_processes_same_session_produce_one_revision() {
    let harness = Harness::new();
    let summarizer = harness.sleeping_summarizer("same-count", 1);
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "SAME_REQUEST", "same answer");
    harness.queue_direct(SESSION_A, &transcript, 10, 11);

    let mut first = harness.spawn_worker(SESSION_A);
    let mut second = harness.spawn_worker(SESSION_A);
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert_eq!(
        fs::read_to_string(harness.root().join("same-count")).unwrap(),
        "x"
    );
    let archived =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_A)).unwrap())
            .unwrap();
    assert_eq!(archived.summary_revision, 1);
}

#[test]
fn unrelated_sessions_process_concurrently() {
    let harness = Harness::new();
    let summarizer = harness.sleeping_summarizer("parallel-count", 1);
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript_a = harness.write_transcript(SESSION_A, "A_REQUEST", "a answer");
    let transcript_b = harness.write_transcript(SESSION_B, "B_REQUEST", "b answer");
    harness.queue_direct(SESSION_A, &transcript_a, 10, 11);
    harness.queue_direct(SESSION_B, &transcript_b, 20, 21);

    let mut first = harness.spawn_worker(SESSION_A);
    let mut second = harness.spawn_worker(SESSION_B);
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    let timing = fs::read_to_string(harness.root().join("parallel-count.timing")).unwrap();
    let lines: Vec<_> = timing.lines().collect();
    assert!(lines.len() >= 2);
    assert!(lines[0].starts_with("start "));
    assert!(lines[1].starts_with("start "));
    assert_eq!(
        fs::read_to_string(harness.root().join("parallel-count")).unwrap(),
        "xx"
    );
}

#[test]
fn atomic_markdown_failure_does_not_advance_cursor_or_revision() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("atomic-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let before = session_cursor(&harness, SESSION_A);
    let archive_path = harness.archive_path(SESSION_A);
    let before_markdown = fs::read(&archive_path).unwrap();

    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    let mut state = StateStore::open(&harness.state).unwrap();
    state
        .ingest_agent_stop(SESSION_A, 20, &harness.project, &transcript)
        .unwrap();
    state
        .ingest_session_end(
            SESSION_A,
            21,
            &harness.project,
            "complete",
            CompletionReason::Complete,
            None,
        )
        .unwrap();
    drop(state);
    let parent = archive_path.parent().unwrap();
    fs::set_permissions(parent, fs::Permissions::from_mode(0o500)).unwrap();
    let worker = harness.worker(SESSION_A);
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(worker.status.success());

    let waited = harness.wait(SESSION_A, 5_000);
    assert!(!waited.status.success());
    assert_eq!(session_cursor(&harness, SESSION_A), before);
    assert_eq!(fs::read(&archive_path).unwrap(), before_markdown);
}

#[test]
fn rebuilding_a_corrupt_database_recovers_current_markdown_then_resumes() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("rebuild-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));

    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!(
            "{}{suffix}",
            harness.state.join("munshi.db").to_string_lossy()
        ));
        let _ = fs::remove_file(path);
    }
    fs::write(harness.state.join("munshi.db"), b"not sqlite").unwrap();
    assert_success(&harness.recover(u64::MAX, false, true));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let rebuilt: (i64, String, String) = connection
        .query_row(
            "SELECT current_summary_revision,current_summary_json,
                    current_markdown_relative_path
             FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(rebuilt.0, 1);
    assert!(rebuilt.1.contains("Initial stable session title"));
    assert!(rebuilt.2.ends_with(&format!("{SESSION_A}.md")));
    drop(connection);

    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 20, 21);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let revised =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_A)).unwrap())
            .unwrap();
    assert_eq!(revised.summary_revision, 2);
}

#[test]
fn schema_one_markdown_rebuild_forces_a_full_cursor_upgrade() {
    let harness = Harness::new();
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    let resolved = resolve_session_reference(&SessionReference {
        source: munshi::SourceKind::Copilot,
        session_id: Some(SESSION_A.to_owned()),
        events_path: Some(transcript.clone()),
        copilot_home: None,
    })
    .unwrap();
    let session = load_session(&resolved, 1024 * 1024).unwrap();
    let project = inspect_project(&harness.project).unwrap();
    let summary = StructuredSummary {
        title: "Imported schema one archive".to_owned(),
        goal: "Exercise the legacy cursor migration.".to_owned(),
        work_completed: vec!["Created a schema one archive.".to_owned()],
        decisions: Vec::new(),
        files_changed: Vec::new(),
        commands_and_validation: Vec::new(),
        open_items: Vec::new(),
        tags: vec!["migration".to_owned()],
    };
    let markdown = render_markdown(
        &ArchiveMetadata {
            session: &session,
            project: &project,
        },
        &summary,
    );
    let archive_path = harness
        .output
        .join(&project.component)
        .join(format!("{SESSION_A}.md"));
    atomic_replace(&archive_path, markdown.as_bytes()).unwrap();

    let summarizer = harness.revision_summarizer("schema-one-count");
    assert_success(&harness.register(&summarizer, 10_000));
    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 20, 21);
    assert_success(&harness.wait(SESSION_A, 5_000));

    let upgraded = parse_archive_markdown(&fs::read_to_string(archive_path).unwrap()).unwrap();
    assert_eq!(upgraded.schema_version, 2);
    assert_eq!(upgraded.summary_revision, 2);
    assert_eq!(
        upgraded.cursor_fallback_reason.as_deref(),
        Some("normalizer-changed")
    );
}

#[test]
fn stale_issue_three_files_migrate_to_retryable_sqlite_work() {
    let harness = Harness::new();
    let transcript = harness.write_transcript(SESSION_A, "LEGACY_REQUEST", "legacy answer");
    let session_dir = harness.state.join("sessions").join(SESSION_A);
    fs::create_dir_all(&session_dir).unwrap();
    fs::create_dir_all(harness.state.join("pending")).unwrap();
    fs::create_dir_all(harness.state.join("workers")).unwrap();
    fs::write(
        session_dir.join("latest.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "session_id": SESSION_A,
            "transcript_path": transcript,
            "origin_cwd": harness.project,
            "agent_stop_timestamp": 10_u64,
        }))
        .unwrap(),
    )
    .unwrap();
    let pending = harness
        .state
        .join("pending")
        .join(format!("{SESSION_A}.json"));
    fs::write(
        &pending,
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "session_id": SESSION_A,
            "transcript_path": transcript,
            "origin_cwd": harness.project,
            "agent_stop_timestamp": 10_u64,
            "session_end_timestamp": 11_u64,
        }))
        .unwrap(),
    )
    .unwrap();
    let worker = harness
        .state
        .join("workers")
        .join(format!("{SESSION_A}.lock"));
    fs::write(&worker, b"").unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o600)).unwrap();
    set_old_mtime(&pending);
    set_old_mtime(&worker);

    let summarizer = harness.revision_summarizer("legacy-count");
    assert_success(&harness.register(&summarizer, 10_000));
    assert!(!session_dir.join("latest.json").exists());
    assert!(!pending.exists());
    assert!(!worker.exists());
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let state: String = connection
        .query_row(
            "SELECT lifecycle_state FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "summary-pending");
    drop(connection);

    assert_success(&harness.recover(0, true, false));
    assert_success(&harness.wait(SESSION_A, 5_000));
}

#[test]
fn fresh_issue_three_worker_files_are_deferred_without_cleanup() {
    let harness = Harness::new();
    let transcript = harness.write_transcript(SESSION_A, "FRESH_LEGACY", "legacy answer");
    fs::create_dir_all(harness.state.join("pending")).unwrap();
    fs::create_dir_all(harness.state.join("workers")).unwrap();
    let pending = harness
        .state
        .join("pending")
        .join(format!("{SESSION_A}.json"));
    fs::write(
        &pending,
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "session_id": SESSION_A,
            "transcript_path": transcript,
            "origin_cwd": harness.project,
            "agent_stop_timestamp": 10_u64,
            "session_end_timestamp": 11_u64,
        }))
        .unwrap(),
    )
    .unwrap();
    let worker = harness
        .state
        .join("workers")
        .join(format!("{SESSION_A}.lock"));
    fs::write(&worker, b"").unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o600)).unwrap();

    let summarizer = harness.revision_summarizer("fresh-legacy-count");
    assert_success(&harness.register(&summarizer, 10_000));
    assert!(pending.is_file());
    assert!(worker.is_file());
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

struct Harness {
    directory: TempDir,
    copilot_home: PathBuf,
    state: PathBuf,
    output: PathBuf,
    project: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-state-test-artifacts");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("case-")
            .tempdir_in(root)
            .unwrap();
        let project = directory.path().join("project");
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
        let copilot_home = directory.path().join("copilot-home");
        Self {
            state: directory.path().join("munshi-home"),
            output: directory.path().join("archives"),
            copilot_home,
            project,
            directory,
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn register(&self, summarizer: &Path, timeout_ms: u64) -> Output {
        self.register_with_git_history(summarizer, timeout_ms, false)
    }

    fn register_with_git_history(
        &self,
        summarizer: &Path,
        timeout_ms: u64,
        archive_git_history: bool,
    ) -> Output {
        self.register_with_output(summarizer, timeout_ms, &self.output, archive_git_history)
    }

    fn register_with_output(
        &self,
        summarizer: &Path,
        timeout_ms: u64,
        output_directory: &Path,
        archive_git_history: bool,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
        command
            .arg("register")
            .arg("--accept-transcript-processing")
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--output-dir")
            .arg(output_directory)
            .arg("--summarizer")
            .arg(summarizer)
            .arg("--timeout-ms")
            .arg(timeout_ms.to_string())
            .stdin(Stdio::null());
        if archive_git_history {
            command.arg("--archive-git-history");
        }
        command.output().unwrap()
    }

    fn commit_count(&self, repository: &Path) -> usize {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["rev-list", "--count", "--all"])
            .output()
            .unwrap();
        assert_success(&output);
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn archive_commit_count(&self) -> usize {
        self.commit_count(&self.output)
    }

    fn archive_latest_commit_message(&self) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.output)
            .args(["log", "-1", "--format=%B"])
            .output()
            .unwrap();
        assert_success(&output);
        String::from_utf8(output.stdout).unwrap()
    }

    fn archive_latest_commit_files(&self) -> Vec<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.output)
            .args(["show", "-1", "--pretty=format:", "--name-only"])
            .output()
            .unwrap();
        assert_success(&output);
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    fn commit_archive_revision(
        &self,
        relative_path: &Path,
        session_id: &str,
        summary_revision: u64,
    ) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&self.output)
                .arg("add")
                .arg("--")
                .arg(relative_path)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&self.output)
                .args([
                    "-c",
                    "user.name=Munshi",
                    "-c",
                    "user.email=munshi@localhost",
                    "commit",
                    "-q",
                    "-m",
                    &format!("archive: copilot:{session_id} revision {summary_revision}"),
                    "-m",
                    &format!("session_id: {session_id}\nsummary_revision: {summary_revision}\n"),
                    "--",
                ])
                .arg(relative_path)
                .status()
                .unwrap()
                .success()
        );
    }

    fn archive_commit_match_count(
        &self,
        relative_path: &Path,
        session_id: &str,
        summary_revision: u64,
    ) -> usize {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.output)
            .args(["log", "--format=%B%x1e", "--"])
            .arg(relative_path)
            .output()
            .unwrap();
        assert_success(&output);
        let expected_session_line = format!("session_id: {session_id}");
        let expected_revision_line = format!("summary_revision: {summary_revision}");
        output
            .stdout
            .split(|byte| *byte == 0x1e)
            .filter(|message| {
                let body = String::from_utf8_lossy(message);
                let mut session_match = false;
                let mut revision_match = false;
                for line in body.lines() {
                    let line = line.trim_end_matches('\r');
                    if line == expected_session_line {
                        session_match = true;
                    } else if line == expected_revision_line {
                        revision_match = true;
                    }
                    if session_match && revision_match {
                        return true;
                    }
                }
                false
            })
            .count()
    }

    fn git_status_porcelain(&self, repository: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert_success(&output);
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn hook(&self, event: &str, payload: &Value) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("hook")
            .arg(event)
            .env("MUNSHI_HOME", &self.state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn wait(&self, session_id: &str, timeout_ms: u64) -> Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("hook")
            .arg("wait")
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--session-id")
            .arg(session_id)
            .arg("--timeout-ms")
            .arg(timeout_ms.to_string())
            .output()
            .unwrap()
    }

    fn recover(&self, stale_after_ms: u64, force: bool, rebuild: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
        command
            .arg("hook")
            .arg("recover")
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--stale-after-ms")
            .arg(stale_after_ms.to_string());
        if force {
            command.arg("--force-retry");
        }
        if rebuild {
            command.arg("--rebuild-state");
        }
        command.output().unwrap()
    }

    fn worker(&self, session_id: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("hook-worker")
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--session-id")
            .arg(session_id)
            .output()
            .unwrap()
    }

    fn spawn_worker(&self, session_id: &str) -> Child {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("hook-worker")
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--session-id")
            .arg(session_id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn write_transcript(&self, session_id: &str, request: &str, answer: &str) -> PathBuf {
        let path = self
            .copilot_home
            .join("session-state")
            .join(session_id)
            .join("events.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, transcript(session_id, request, answer)).unwrap();
        path.canonicalize().unwrap()
    }

    fn replace_transcript(&self, session_id: &str, request: &str, answer: &str) {
        let path = self
            .copilot_home
            .join("session-state")
            .join(session_id)
            .join("events.jsonl");
        fs::write(path, transcript(session_id, request, answer)).unwrap();
    }

    fn append_turn(&self, transcript: &Path, request: &str, answer: &str) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(transcript)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "id": format!("{request}-user"),
                "timestamp": "2026-07-12T00:01:00.000Z",
                "parentId": "initial-assistant",
                "type": "user.message",
                "data": {"content": request},
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "id": format!("{request}-assistant"),
                "timestamp": "2026-07-12T00:01:01.000Z",
                "parentId": format!("{request}-user"),
                "type": "assistant.message",
                "data": {"content": answer, "messageId": format!("{request}-message")},
            })
        )
        .unwrap();
    }

    fn agent_stop_payload(&self, session_id: &str, transcript: &Path, timestamp: u64) -> Value {
        json!({
            "sessionId": session_id,
            "timestamp": timestamp,
            "cwd": self.project,
            "transcriptPath": transcript,
            "stopReason": "end_turn",
        })
    }

    fn complete_lifecycle(
        &self,
        session_id: &str,
        transcript: &Path,
        stop_timestamp: u64,
        end_timestamp: u64,
    ) {
        assert_success(&self.hook(
            "agent-stop",
            &self.agent_stop_payload(session_id, transcript, stop_timestamp),
        ));
        assert_success(&self.hook(
            "session-end",
            &json!({
                "sessionId": session_id,
                "timestamp": end_timestamp,
                "cwd": self.project,
                "reason": "complete",
            }),
        ));
    }

    fn queue_direct(
        &self,
        session_id: &str,
        transcript: &Path,
        stop_timestamp: i64,
        end_timestamp: i64,
    ) {
        let mut state = StateStore::open(&self.state).unwrap();
        state
            .ingest_agent_stop(session_id, stop_timestamp, &self.project, transcript)
            .unwrap();
        state
            .ingest_session_end(
                session_id,
                end_timestamp,
                &self.project,
                "complete",
                CompletionReason::Complete,
                None,
            )
            .unwrap();
    }

    fn archive_path(&self, session_id: &str) -> PathBuf {
        let project = fs::read_dir(&self.output)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        project.join(format!("{session_id}.md"))
    }

    fn revision_summarizer(&self, count_name: &str) -> PathBuf {
        let script = self.root().join(format!("{count_name}.sh"));
        let count = self.root().join(count_name);
        let body = format!(
            r#"#!/bin/sh
set -eu
input=$(cat)
printf x >> '{}'
case "$input" in
  *TRUNCATED_REQUEST*)
    printf '%s' '{{"title":"Truncation fallback summary","goal":"Recover after truncation.","work_completed":["Re-read the complete source."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["recovery"]}}'
    ;;
  *REWRITE_REQUEST*)
    printf '%s' '{{"title":"Rewrite fallback summary","goal":"Recover after rewrite.","work_completed":["Re-read rewritten source."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["recovery"]}}'
    ;;
  *\"previous_summary\"*)
    case "$input" in *INITIAL_REQUEST*) exit 31 ;; esac
    case "$input" in *DELTA_REQUEST*) : ;; *) exit 32 ;; esac
    printf '%s' '{{"title":"Revised stable session title","goal":"Preserve prior work and add the delta.","work_completed":["Preserved prior work.","Added resumed work."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["resume"]}}'
    ;;
  *)
    printf '%s' '{{"title":"Initial stable session title","goal":"Archive the initial session.","work_completed":["Archived initial work."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["initial"]}}'
    ;;
esac
"#,
            count.display()
        );
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script.canonicalize().unwrap()
    }

    fn sleeping_summarizer(&self, count_name: &str, seconds: u64) -> PathBuf {
        let script = self.root().join(format!("{count_name}.sh"));
        let count = self.root().join(count_name);
        let timing = self.root().join(format!("{count_name}.timing"));
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\ncat >/dev/null\nprintf 'start %s %s\\n' \"$$\" \"$(date +%s)\" >> '{}'\nprintf x >> '{}'\nsleep {}\nprintf 'end %s %s\\n' \"$$\" \"$(date +%s)\" >> '{}'\nprintf '%s' '{}'\n",
                timing.display(),
                count.display(),
                seconds,
                timing.display(),
                r#"{"title":"Concurrent archive","goal":"Test independent workers.","work_completed":["Processed one session."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["concurrency"]}"#
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script.canonicalize().unwrap()
    }
}

fn transcript(session_id: &str, request: &str, answer: &str) -> String {
    [
        json!({
            "id": "initial-start",
            "timestamp": "2026-07-12T00:00:00.000Z",
            "parentId": null,
            "type": "session.start",
            "data": {"sessionId": session_id},
        }),
        json!({
            "id": "initial-user",
            "timestamp": "2026-07-12T00:00:01.000Z",
            "parentId": "initial-start",
            "type": "user.message",
            "data": {"content": request},
        }),
        json!({
            "id": "initial-assistant",
            "timestamp": "2026-07-12T00:00:02.000Z",
            "parentId": "initial-user",
            "type": "assistant.message",
            "data": {"content": answer, "messageId": "initial-message"},
        }),
    ]
    .into_iter()
    .map(|value| value.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

fn session_cursor(harness: &Harness, session_id: &str) -> (i64, i64, String) {
    Connection::open(harness.state.join("munshi.db"))
        .unwrap()
        .query_row(
            "SELECT current_summary_revision,source_cursor_records,source_hash
             FROM sessions WHERE source_session_id=?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

fn set_old_mtime(path: &Path) {
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let times = [
        libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    ];
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) },
        0
    );
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
