use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use munshi::{
    ArchiveMetadata, CompletionReason, SessionReference, SourceKind, StateStore, StructuredSummary,
    atomic_replace, content_hash, inspect_project, load_session, parse_archive_markdown,
    recorded_project_identity, render_markdown, render_revision_markdown,
    resolve_session_reference, session_id_matches_transcript_path,
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
    assert_eq!(version, 11);
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
fn retry_honors_raised_source_limit_after_source_failed_park() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("raised-limit-count");
    assert_success(&harness.register_with_source_limit(&summarizer, 10_000, 2_048));
    let transcript = harness.write_transcript(
        SESSION_A,
        "INITIAL_REQUEST",
        &"oversized answer ".repeat(400),
    );
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    let waited = harness.wait(SESSION_A, 5_000);
    assert!(!waited.status.success());
    assert!(String::from_utf8_lossy(&waited.stdout).contains("source-oversized"));
    // The failed-era verdict is a permanent park recorded against the old limit, under the
    // size-specific category the issue #57 split introduced.
    let (category, next_retry) = session_retry_park(&harness, SESSION_A);
    assert_eq!(category.as_deref(), Some("source-oversized"));
    assert_eq!(next_retry, Some(-1));

    // Raise the configured limit the way a user would: re-register over the same state.
    assert_success(&harness.register_with_source_limit(&summarizer, 10_000, 8_388_608));

    // A plain retry (no --force) must re-evaluate against the current configured limit.
    let retried = harness.retry(SESSION_A);
    assert_success(&retried);
    assert!(String::from_utf8_lossy(&retried.stdout).contains("\"result\": \"archived\""));
    let archived =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_A)).unwrap())
            .unwrap();
    assert_eq!(archived.summary_revision, 1);
}

#[test]
fn recovery_sweep_revives_parked_sessions_after_source_limit_raise() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("raised-limit-recovery-count");
    assert_success(&harness.register_with_source_limit(&summarizer, 10_000, 2_048));
    let transcript = harness.write_transcript(
        SESSION_B,
        "INITIAL_REQUEST",
        &"oversized answer ".repeat(400),
    );
    harness.complete_lifecycle(SESSION_B, &transcript, 10, 11);
    let waited = harness.wait(SESSION_B, 5_000);
    assert!(!waited.status.success());
    let (category, next_retry) = session_retry_park(&harness, SESSION_B);
    assert_eq!(category.as_deref(), Some("source-oversized"));
    assert_eq!(next_retry, Some(-1));

    assert_success(&harness.register_with_source_limit(&summarizer, 10_000, 8_388_608));

    // The recovery sweep must lift the stale park and archive through the normal worker path.
    // `hook wait` treats `failed` as terminal, so poll until the spawned worker flips the state.
    assert_success(&harness.recover(60_000, false, false));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waited = harness.wait(SESSION_B, 5_000);
        if waited.status.success() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session was not archived after recovery: {}",
            String::from_utf8_lossy(&waited.stdout)
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let archived =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_B)).unwrap())
            .unwrap();
    assert_eq!(archived.summary_revision, 1);
}

#[test]
fn repeated_deterministic_failure_escalates_backoff_then_parks_and_sweeps_skip_it() {
    let harness = Harness::new();
    let summarizer = harness.failing_summarizer("deterministic-fail-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    let waited = harness.wait(SESSION_A, 15_000);
    assert!(!waited.status.success());
    assert_eq!(harness.summarizer_calls("deterministic-fail-count"), 1);

    // Each consecutive same-category failure escalates the per-session backoff:
    // 10 minutes, then 30, 90, and 240 minutes (asserted against generous lower bounds).
    let minute = 60_000_i64;
    let minimum_delays = [9 * minute, 29 * minute, 89 * minute, 239 * minute];
    for (index, minimum) in minimum_delays.iter().enumerate() {
        let attempt = index + 1;
        let (category, next_retry) = session_retry_park(&harness, SESSION_A);
        assert_eq!(category.as_deref(), Some("summary-failed"));
        let next_retry = next_retry.unwrap_or_default();
        let delay = next_retry - wall_clock_ms();
        assert!(
            delay >= *minimum,
            "attempt {attempt}: backoff {delay}ms did not reach {minimum}ms"
        );
        assert_eq!(session_failure_streak(&harness, SESSION_A), attempt as i64);
        // Simulate the backoff having elapsed, without touching the streak.
        make_retry_due(&harness, SESSION_A);
        let _ = harness.worker(SESSION_A);
        assert_eq!(
            harness.summarizer_calls("deterministic-fail-count"),
            attempt + 1
        );
    }

    // The fifth consecutive failure parks the session with the real category retained.
    let (category, next_retry) = session_retry_park(&harness, SESSION_A);
    assert_eq!(category.as_deref(), Some("summary-failed"));
    assert_eq!(next_retry, Some(-1));
    assert_eq!(session_failure_streak(&harness, SESSION_A), 5);

    // A plain sweep must skip the park: no summarizer invocation, park intact.
    assert_success(&harness.recover(0, false, false));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(harness.summarizer_calls("deterministic-fail-count"), 5);
    assert_eq!(session_retry_park(&harness, SESSION_A).1, Some(-1));

    // Operators can see the park on the status surface.
    let status = harness.status_text();
    assert!(status.contains("parked=1"), "status output: {status}");
}

#[test]
fn starved_sessions_get_slots_while_the_clogger_is_backed_off() {
    let harness = Harness::new();
    let summarizer = harness.clogging_summarizer("clog-fail-count");
    assert_success(&harness.register(&summarizer, 10_000));

    // The clogger fails its (billed) summarizer call deterministically.
    let clog = harness.write_transcript(SESSION_A, "CLOG_REQUEST", "clog answer");
    harness.complete_lifecycle(SESSION_A, &clog, 10, 11);
    let waited = harness.wait(SESSION_A, 15_000);
    assert!(!waited.status.success());
    assert_eq!(harness.summarizer_calls("clog-fail-count"), 1);

    // A healthy session is queued behind it, waiting for a recovery sweep. `queue_direct`
    // reserves a worker nobody spawns; clear that reservation (exactly what the stale-claim
    // test does) so the sweep below is the first party to hand out the slot.
    let starved = harness.write_transcript(SESSION_B, "GOOD_REQUEST", "good answer");
    harness.queue_direct(SESSION_B, &starved, 20, 21);
    Connection::open(harness.state.join("munshi.db"))
        .unwrap()
        .execute(
            "UPDATE sessions SET worker_generation=NULL,worker_spawned_at_ms=NULL
             WHERE source_session_id=?1",
            [SESSION_B],
        )
        .unwrap();

    // Give any sub-minute legacy backoff time to lapse: the sweep must still skip the
    // clogger (its escalating backoff holds) and hand the slot to the starved session.
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_success(&harness.recover(0, false, false));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waited = harness.wait(SESSION_B, 5_000);
        if waited.status.success() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "starved session was never archived: {} {}",
            String::from_utf8_lossy(&waited.stdout),
            String::from_utf8_lossy(&waited.stderr)
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(harness.archive_path(SESSION_B).exists());
    assert_eq!(
        harness.summarizer_calls("clog-fail-count"),
        1,
        "the backed-off clogger must not be retried by the sweep"
    );
}

#[test]
fn targeted_retry_lifts_failure_park_and_force_resets_the_streak() {
    let harness = Harness::new();
    let summarizer = harness.failing_summarizer("park-lift-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    let waited = harness.wait(SESSION_A, 15_000);
    assert!(!waited.status.success());
    for _ in 0..4 {
        make_retry_due(&harness, SESSION_A);
        let _ = harness.worker(SESSION_A);
    }
    assert_eq!(session_retry_park(&harness, SESSION_A).1, Some(-1));
    assert_eq!(session_failure_streak(&harness, SESSION_A), 5);
    assert_eq!(harness.summarizer_calls("park-lift-count"), 5);

    // A plain targeted retry is an explicit operator action: it lifts the park, resets the
    // streak, and makes a real (failing) attempt rather than replaying the stored verdict.
    let retried = harness.retry(SESSION_A);
    let stdout = String::from_utf8_lossy(&retried.stdout);
    assert!(
        stdout.contains("\"result\": \"failed\""),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("summary-failed"), "stdout: {stdout}");
    assert_eq!(harness.summarizer_calls("park-lift-count"), 6);
    let (_, next_retry) = session_retry_park(&harness, SESSION_A);
    let next_retry = next_retry.unwrap_or_default();
    assert!(
        next_retry >= 0,
        "lifted session must be backed off, not re-parked: {next_retry}"
    );
    assert_eq!(session_failure_streak(&harness, SESSION_A), 1);

    // `retry --force` bypasses the fresh backoff and also restarts the escalation window.
    let forced = harness.retry_force(SESSION_A);
    let stdout = String::from_utf8_lossy(&forced.stdout);
    assert!(
        stdout.contains("\"result\": \"failed\""),
        "stdout: {stdout}"
    );
    assert_eq!(harness.summarizer_calls("park-lift-count"), 7);
    let (_, next_retry) = session_retry_park(&harness, SESSION_A);
    assert!(next_retry.unwrap_or_default() >= 0);
    assert_eq!(session_failure_streak(&harness, SESSION_A), 1);
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

/// Issue #39: `hook recover --rebuild-state` used to re-discover unarchived transcripts into
/// `interrupted` rows with no origin, which no worker path could ever claim — the queue
/// deadlocked silently. A rebuild on a store whose session was previously processable must
/// now leave the store in a shape a plain `hook recover` archives without intervention.
#[test]
fn rebuild_state_requeues_unarchived_sessions_through_normal_recovery() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("rebuild-requeue-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.write_workspace(SESSION_A);
    // Previously processable: observed through both hooks, but never archived, so the
    // rebuild has no archive to restore this session from.
    harness.queue_direct(SESSION_A, &transcript, 10, 11);

    assert_success(&harness.recover(0, false, true));
    assert_success(&harness.recover(0, false, false));
    assert_success(&harness.wait(SESSION_A, 15_000));

    let archived =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_A)).unwrap())
            .unwrap();
    assert_eq!(archived.completion_reason, "unknown");
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let hydrated: (String, Option<String>, Option<String>, bool) = connection
        .query_row(
            "SELECT lifecycle_state,origin_cwd,transcript_path,active
             FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(hydrated.0, "archived");
    assert_eq!(hydrated.1.as_deref(), harness.project.to_str());
    assert_eq!(
        hydrated.2.as_deref(),
        Some(transcript.to_str().unwrap()),
        "the rebuilt session must keep its recovered transcript"
    );
    assert!(!hydrated.3);
}

/// Issue #39: rows an earlier rebuild left queued in `interrupted` with `active=0`, no
/// origin, and no activity evidence must be hydrated by a plain recovery sweep — but only
/// once the transcript has passed the mtime quiet period, so a live session is never
/// captured.
#[test]
fn stuck_unhydrated_sessions_hydrate_after_the_quiet_period_and_archive() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("hydrate-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "STUCK_REQUEST", "stuck answer");
    harness.write_workspace(SESSION_A);
    // The exact row shape a pre-fix rebuild left behind on a live store.
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,transcript_path,transcript_source,
                completion_reason,source_end_reason,lifecycle_state,active,
                state_generation,last_error_category,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,'version-pinned-recovery','unknown','unknown',
                       'interrupted',0,1,'origin-unresolved',1,1)",
            params![SESSION_A, transcript.to_str().unwrap()],
        )
        .unwrap();
    drop(connection);

    // Fresh transcript: inside the quiet period the row must be left untouched.
    assert_success(&harness.recover(600_000, false, false));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let untouched: (String, Option<String>) = connection
        .query_row(
            "SELECT lifecycle_state,origin_cwd FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(untouched, ("interrupted".to_owned(), None));
    drop(connection);

    // Once quiet, the same sweep hydrates the row and the normal worker archives it.
    set_old_mtime(&transcript);
    assert_success(&harness.recover(600_000, false, false));
    assert_success(&harness.wait(SESSION_A, 15_000));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let hydrated: (String, Option<String>, Option<i64>, Option<String>) = connection
        .query_row(
            "SELECT lifecycle_state,origin_cwd,last_agent_stop_ms,last_error_category
             FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(hydrated.0, "archived");
    assert_eq!(hydrated.1.as_deref(), harness.project.to_str());
    // Activity evidence comes from the transcript mtime set above (tv_sec=1).
    assert_eq!(hydrated.2, Some(1_000));
    assert_eq!(hydrated.3, None);
}

/// Never-drop: a queued session whose origin stays underivable is parked with one visible
/// diagnostic (no per-sweep spam) and its transcript left alone, then hydrated and archived
/// as soon as an origin record appears.
#[test]
fn unhydratable_sessions_stay_queued_until_an_origin_appears() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("park-count");
    assert_success(&harness.register(&summarizer, 10_000));
    // No workspace.yaml: the Copilot origin is underivable.
    let transcript = harness.write_transcript(SESSION_A, "PARKED_REQUEST", "parked answer");
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,transcript_path,transcript_source,
                completion_reason,source_end_reason,lifecycle_state,active,
                state_generation,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,'version-pinned-recovery','unknown','unknown',
                       'interrupted',0,1,1,1)",
            params![SESSION_A, transcript.to_str().unwrap()],
        )
        .unwrap();
    drop(connection);

    assert_success(&harness.recover(0, false, false));
    assert_success(&harness.recover(0, false, false));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let parked: (String, Option<String>, Option<String>) = connection
        .query_row(
            "SELECT lifecycle_state,origin_cwd,last_error_category
             FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(parked.0, "interrupted");
    assert_eq!(parked.1, None);
    assert_eq!(parked.2.as_deref(), Some("origin-unresolved"));
    // One diagnostic on the transition, not one per sweep.
    let diagnostics: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM diagnostics
             WHERE operation='recovery' AND category='origin-unresolved'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(diagnostics, 1);
    drop(connection);
    assert!(transcript.is_file(), "never-drop: the transcript survives");

    harness.write_workspace(SESSION_A);
    assert_success(&harness.recover(0, false, false));
    assert_success(&harness.wait(SESSION_A, 15_000));
    let archived =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_A)).unwrap())
            .unwrap();
    assert_eq!(archived.completion_reason, "unknown");
}

/// Issue #42: the sweep derives its park verdict outside any transaction, so a session a
/// concurrent hook claimed or archived in the meantime must not be labelled
/// `origin-unresolved` on the way past. Parking re-checks the recovery-held shape inside its
/// own transaction, exactly as hydration does, and leaves a row that has moved on alone.
/// Issue #82: the purge must select on re-derived identity, not on a stored label, and must never
/// touch a session that produced an archive. The archived row here is the one that matters: it is
/// mismatched in exactly the same way, and is still excluded.
#[test]
fn purge_selects_only_unarchived_identity_mismatches() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("purge-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "PURGE_REQUEST", "answer");
    let path = transcript.to_str().unwrap();

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    // (id, revision, category) — only the first is eligible.
    let rows = [
        // Mismatched and never archived: the subagent case.
        ("call_ToolCallIdNotASession", 0, "source-id-mismatch"),
        // Mismatched, never archived, but still carrying the pre-fix label: must still be found.
        (SESSION_B, 0, "source-failed"),
        // Mismatched but it archived once, so a Markdown record refers back to it.
        ("cccccccc-cccc-4ccc-8ccc-cccccccccccc", 3, "source-id-mismatch"),
    ];
    for (session, revision, category) in rows {
        connection
            .execute(
                "INSERT INTO sessions(
                    source_kind,source_session_id,transcript_path,transcript_source,
                    completion_reason,source_end_reason,lifecycle_state,active,
                    last_error_category,next_retry_at_ms,current_summary_revision,
                    state_generation,created_at_ms,updated_at_ms
                 ) VALUES ('copilot-cli',?1,?2,'version-pinned-recovery','unknown','unknown',
                           'failed',0,?3,-1,?4,1,1,1)",
                params![session, path, category, revision],
            )
            .unwrap();
    }
    // The session the transcript actually belongs to, parked but correctly identified.
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,transcript_path,transcript_source,
                completion_reason,source_end_reason,lifecycle_state,active,
                last_error_category,next_retry_at_ms,current_summary_revision,
                state_generation,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,'version-pinned-recovery','unknown','unknown',
                       'failed',0,'source-failed',-1,0,1,1,1)",
            params![SESSION_A, path],
        )
        .unwrap();
    drop(connection);

    let store = StateStore::open(&harness.state).unwrap();
    let candidates = store.parked_unarchived_sessions().unwrap();
    let mismatched: Vec<&str> = candidates
        .iter()
        .filter(|(source, session_id, path)| {
            !session_id_matches_transcript_path(*source, session_id, path)
        })
        .map(|(_, session_id, _)| session_id.as_str())
        .collect();
    assert_eq!(
        mismatched,
        vec!["call_ToolCallIdNotASession", SESSION_B],
        "both mismatches are found regardless of label; the archived one and the correctly \
         identified one are not"
    );

    let mut store = StateStore::open_for_source(&harness.state, SourceKind::Copilot).unwrap();
    assert!(store.purge_parked_session("call_ToolCallIdNotASession").unwrap());
    // The archived row is refused even when named directly.
    assert!(
        !store
            .purge_parked_session("cccccccc-cccc-4ccc-8ccc-cccccccccccc")
            .unwrap(),
        "a session that archived is never purgeable"
    );

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let remaining: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 3, "exactly one row was removed");
}

/// Issue #83: the issue-#44 stale-park reactivation is a *size-cap* mechanism. It re-measures the
/// transcript and lifts the park if it now fits. `source-failed` was matched alongside
/// `source-oversized` from when the two were one lumped code — but since #57 `source-failed` is
/// the residual I/O category, unrelated to size, so the measurement passed on every sweep for any
/// small file. The park was lifted moments after being written, `failure_streak` reset to 0, and
/// deterministic failures retried forever without ever reaching RETRY_PARK_THRESHOLD.
#[test]
fn a_source_failed_park_survives_the_size_cap_reactivation() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("park-lift-count");
    assert_success(&harness.register(&summarizer, 10_000));
    // Both transcripts are small, so the size re-check passes for either row.
    let a = harness.write_transcript(SESSION_A, "OVERSIZED_REQUEST", "answer");
    let b = harness.write_transcript(SESSION_B, "RESIDUAL_REQUEST", "answer");

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    for (session, transcript, category) in [
        (SESSION_A, &a, "source-oversized"),
        (SESSION_B, &b, "source-failed"),
    ] {
        connection
            .execute(
                "INSERT INTO sessions(
                    source_kind,source_session_id,transcript_path,transcript_source,
                    completion_reason,source_end_reason,lifecycle_state,active,
                    last_error_category,next_retry_at_ms,failure_streak,
                    state_generation,created_at_ms,updated_at_ms
                 ) VALUES ('copilot-cli',?1,?2,'version-pinned-recovery','unknown','unknown',
                           'failed',0,?3,-1,1,1,1,1)",
                params![session, transcript.to_str().unwrap(), category],
            )
            .unwrap();
    }
    drop(connection);

    let store = StateStore::open_for_source(&harness.state, SourceKind::Copilot).unwrap();
    let candidates: Vec<String> = store
        .parked_source_limit_sessions()
        .unwrap()
        .into_iter()
        .map(|(_, session_id, _)| session_id)
        .collect();
    assert_eq!(
        candidates,
        vec![SESSION_A.to_string()],
        "only the size-cap park is a reactivation candidate"
    );

    let mut store = store;
    assert!(
        store.lift_source_limit_park(SESSION_A).unwrap(),
        "an oversized park is still lifted once the transcript fits"
    );
    assert!(
        !store.lift_source_limit_park(SESSION_B).unwrap(),
        "a residual source-failed park must stay parked"
    );

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let (next_retry, streak): (Option<i64>, i64) = connection
        .query_row(
            "SELECT next_retry_at_ms,failure_streak FROM sessions WHERE source_session_id=?1",
            [SESSION_B],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(next_retry, Some(-1), "the park marker is untouched");
    assert_eq!(streak, 1, "and the streak is not reset, so it can reach the threshold");
}

#[test]
fn parking_skips_a_session_that_was_archived_concurrently() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("park-race-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "PARKED_REQUEST", "parked answer");
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    // SESSION_A raced to `archived`; SESSION_B is still recovery-held. Both are otherwise
    // identical, so only the shape re-check can tell them apart.
    for (session, lifecycle) in [(SESSION_A, "archived"), (SESSION_B, "interrupted")] {
        connection
            .execute(
                "INSERT INTO sessions(
                    source_kind,source_session_id,transcript_path,transcript_source,
                    completion_reason,source_end_reason,lifecycle_state,active,
                    state_generation,created_at_ms,updated_at_ms
                 ) VALUES ('copilot-cli',?1,?2,'version-pinned-recovery','unknown','unknown',
                           ?3,0,1,1,1)",
                params![session, transcript.to_str().unwrap(), lifecycle],
            )
            .unwrap();
    }
    drop(connection);

    let mut store = StateStore::open_for_source(&harness.state, SourceKind::Copilot).unwrap();
    store
        .park_recovery_session(SESSION_A, "origin-unresolved")
        .unwrap();
    store
        .park_recovery_session(SESSION_B, "origin-unresolved")
        .unwrap();
    drop(store);

    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let label = |session: &str| -> Option<String> {
        connection
            .query_row(
                "SELECT last_error_category FROM sessions WHERE source_session_id=?1",
                [session],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(
        label(SESSION_A),
        None,
        "an archived session keeps no misleading origin-unresolved label"
    );
    assert_eq!(
        label(SESSION_B).as_deref(),
        Some("origin-unresolved"),
        "a still recovery-held session parks as before"
    );
    // The skipped park records no diagnostic either: exactly one, for the row that parked.
    let diagnostics: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM diagnostics
             WHERE operation='recovery' AND category='origin-unresolved'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(diagnostics, 1);
}

/// Issue #40: a session whose origin directory was deleted after the fact used to park as
/// `project-failed` forever. The worker must now derive the project identity from the
/// recorded evidence (the `workspace.yaml` cwd), flag the provenance as recorded, and keep
/// the derived component stable across revisions.
#[test]
fn deleted_origin_archives_via_recorded_evidence_with_flagged_provenance() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("recorded-origin-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    let gone = harness.root().join("gone-project");
    fs::create_dir_all(&gone).unwrap();
    let gone = gone.canonicalize().unwrap();
    harness.write_workspace_with_cwd(SESSION_A, &gone);
    harness.queue_direct_with_origin(SESSION_A, &transcript, &gone, 10, 11);
    fs::remove_dir_all(&gone).unwrap();

    assert_success(&harness.worker(SESSION_A));
    assert_success(&harness.wait(SESSION_A, 5_000));
    let expected = recorded_project_identity(&gone, None);
    assert!(expected.component.starts_with("gone-project-"));
    let archive_path = harness
        .output
        .join(&expected.component)
        .join(format!("{SESSION_A}.md"));
    let markdown = fs::read_to_string(&archive_path).unwrap();
    assert!(
        markdown.contains("project_origin: \"recorded\""),
        "frontmatter must flag the recorded provenance: {markdown}"
    );
    let parsed = parse_archive_markdown(&markdown).unwrap();
    assert_eq!(parsed.project.identity, expected.identity);
    assert_eq!(parsed.project.project, "gone-project");
    assert_eq!(parsed.project.repository, None);
    assert_eq!(parsed.summary_revision, 1);

    // A later revision reuses the same recorded identity: same component, same file.
    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.queue_direct_with_origin(SESSION_A, &transcript, &gone, 20, 21);
    assert_success(&harness.worker(SESSION_A));
    assert_success(&harness.wait(SESSION_A, 5_000));
    let revised_markdown = fs::read_to_string(&archive_path).unwrap();
    // Revision 2 renders from the state-cached identity, so the recorded provenance must
    // survive the database round-trip, not just the first derivation.
    assert!(revised_markdown.contains("project_origin: \"recorded\""));
    let revised = parse_archive_markdown(&revised_markdown).unwrap();
    assert_eq!(revised.summary_revision, 2);
    assert_eq!(revised.project.identity, expected.identity);
}

/// Issue #40: a parked `origin-unresolved` recovery row whose transcript evidence records a
/// since-deleted directory must hydrate through the recorded-evidence fallback instead of
/// parking forever. The quiet-period gate stays in force.
#[test]
fn parked_origin_unresolved_hydrates_from_recorded_evidence_of_a_deleted_directory() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("recorded-hydrate-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "STUCK_REQUEST", "stuck answer");
    let gone = harness.root().join("gone-project");
    fs::create_dir_all(&gone).unwrap();
    let gone = gone.canonicalize().unwrap();
    harness.write_workspace_with_cwd(SESSION_A, &gone);
    fs::remove_dir_all(&gone).unwrap();
    // The parked row shape issue #39's sweep leaves when the origin cannot be resolved.
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,transcript_path,transcript_source,
                completion_reason,source_end_reason,lifecycle_state,active,
                state_generation,last_error_category,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,'version-pinned-recovery','unknown','unknown',
                       'interrupted',0,1,'origin-unresolved',1,1)",
            params![SESSION_A, transcript.to_str().unwrap()],
        )
        .unwrap();
    drop(connection);

    set_old_mtime(&transcript);
    assert_success(&harness.recover(600_000, false, false));
    assert_success(&harness.wait(SESSION_A, 15_000));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let hydrated: (String, Option<String>, Option<String>) = connection
        .query_row(
            "SELECT lifecycle_state,origin_cwd,last_error_category
             FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(hydrated.0, "archived");
    assert_eq!(hydrated.1.as_deref(), gone.to_str());
    assert_eq!(hydrated.2, None);
    let expected = recorded_project_identity(&gone, None);
    let markdown = fs::read_to_string(
        harness
            .output
            .join(&expected.component)
            .join(format!("{SESSION_A}.md")),
    )
    .unwrap();
    assert!(markdown.contains("project_origin: \"recorded\""));
    let parsed = parse_archive_markdown(&markdown).unwrap();
    assert_eq!(parsed.project.identity, expected.identity);
    assert_eq!(parsed.completion_reason, "unknown");
}

/// Issue #49: an `observed` row with `active=0` and no session-end verdict is invisible to
/// every hand-off path (`stale_known_sessions` needs `active=1`, reservation excludes
/// `observed`, the #39 hydration is scoped to `interrupted`). A plain recovery sweep must
/// requeue it through the interrupted pipeline — deriving the missing agent-stop evidence
/// from the transcript mtime — and archive it.
#[test]
fn stuck_observed_inactive_row_with_evidence_hydrates_and_archives() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("observed-rescue-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "STUCK_REQUEST", "stuck answer");
    harness.write_workspace(SESSION_A);
    // The exact husk shape from the live census: swept into recovery, judged once by a
    // worker, and returned to `observed` with no session-end verdict and no stop evidence.
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,origin_cwd,transcript_path,transcript_source,
                completion_reason,source_end_reason,lifecycle_state,active,
                state_generation,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,?3,'version-pinned-recovery','unknown','unknown',
                       'observed',0,2,1,1)",
            params![
                SESSION_A,
                harness.project.to_str().unwrap(),
                transcript.to_str().unwrap()
            ],
        )
        .unwrap();
    drop(connection);

    set_old_mtime(&transcript);
    assert_success(&harness.recover(600_000, false, false));
    assert_success(&harness.wait(SESSION_A, 15_000));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let rescued: (String, Option<String>, Option<i64>, Option<String>) = connection
        .query_row(
            "SELECT lifecycle_state,origin_cwd,last_agent_stop_ms,last_error_category
             FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(rescued.0, "archived");
    assert_eq!(rescued.1.as_deref(), harness.project.to_str());
    // Activity evidence comes from the transcript mtime set above (tv_sec=1).
    assert_eq!(rescued.2, Some(1_000));
    assert_eq!(rescued.3, None);
    let archived =
        parse_archive_markdown(&fs::read_to_string(harness.archive_path(SESSION_A)).unwrap())
            .unwrap();
    assert_eq!(archived.completion_reason, "unknown");
}

/// Issue #49 safety case: an `observed` row whose transcript is still inside the mtime
/// quiet period is a live session and must be left completely untouched by the sweep.
#[test]
fn live_observed_inactive_row_is_left_untouched_inside_the_quiet_period() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("observed-live-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "LIVE_REQUEST", "live answer");
    harness.write_workspace(SESSION_A);
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,origin_cwd,transcript_path,transcript_source,
                completion_reason,source_end_reason,lifecycle_state,active,
                state_generation,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,?3,'version-pinned-recovery','unknown','unknown',
                       'observed',0,2,1,1)",
            params![
                SESSION_A,
                harness.project.to_str().unwrap(),
                transcript.to_str().unwrap()
            ],
        )
        .unwrap();
    drop(connection);

    // Fresh transcript mtime: the sweep must not requeue, reserve, or annotate the row.
    assert_success(&harness.recover(600_000, false, false));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let untouched: (String, bool, Option<i64>, Option<String>, i64) = connection
        .query_row(
            "SELECT lifecycle_state,active,last_agent_stop_ms,last_error_category,
                    state_generation
             FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(untouched.0, "observed");
    assert!(!untouched.1);
    assert_eq!(untouched.2, None);
    assert_eq!(untouched.3, None);
    assert_eq!(untouched.4, 2);
    let attempts: i64 = connection
        .query_row("SELECT COUNT(*) FROM processing_attempts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(attempts, 0);
}

/// Issue #49 with issue #40 composed: a stuck observed row whose recorded origin directory
/// was deleted (subagent worktrees) keeps that origin through the rescue and archives via
/// the worker's recorded-evidence project identity.
#[test]
fn stuck_observed_row_with_deleted_origin_archives_via_recorded_evidence() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("observed-recorded-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "STUCK_REQUEST", "stuck answer");
    let gone = harness.root().join("gone-worktree");
    fs::create_dir_all(&gone).unwrap();
    let gone = gone.canonicalize().unwrap();
    harness.write_workspace_with_cwd(SESSION_A, &gone);
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,origin_cwd,transcript_path,transcript_source,
                completion_reason,source_end_reason,lifecycle_state,active,
                state_generation,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,?3,'version-pinned-recovery','unknown','unknown',
                       'observed',0,2,1,1)",
            params![
                SESSION_A,
                gone.to_str().unwrap(),
                transcript.to_str().unwrap()
            ],
        )
        .unwrap();
    drop(connection);
    fs::remove_dir_all(&gone).unwrap();

    set_old_mtime(&transcript);
    assert_success(&harness.recover(600_000, false, false));
    assert_success(&harness.wait(SESSION_A, 15_000));
    let expected = recorded_project_identity(&gone, None);
    let markdown = fs::read_to_string(
        harness
            .output
            .join(&expected.component)
            .join(format!("{SESSION_A}.md")),
    )
    .unwrap();
    assert!(markdown.contains("project_origin: \"recorded\""));
    let parsed = parse_archive_markdown(&markdown).unwrap();
    assert_eq!(parsed.project.identity, expected.identity);
    assert_eq!(parsed.completion_reason, "unknown");
}

/// Issue #49 composed with issue #38's no-churn discipline: a rescued husk the worker again
/// judges not archive-worthy settles — the rescue observation is deduplicated on the
/// transcript evidence, so later sweeps do not reprocess an unchanged session forever.
#[test]
fn rescued_observed_husk_settles_without_rechurn_when_still_not_archive_worthy() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("observed-settle-count");
    assert_success(&harness.register(&summarizer, 10_000));
    // A transcript with a user request but no assistant activity: never archive-worthy.
    let transcript = harness
        .copilot_home
        .join("session-state")
        .join(SESSION_A)
        .join("events.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(
        &transcript,
        [
            json!({
                "id": "initial-start",
                "timestamp": "2026-07-12T00:00:00.000Z",
                "parentId": null,
                "type": "session.start",
                "data": {"sessionId": SESSION_A},
            }),
            json!({
                "id": "initial-user",
                "timestamp": "2026-07-12T00:00:01.000Z",
                "parentId": "initial-start",
                "type": "user.message",
                "data": {"content": "UNANSWERED_REQUEST"},
            }),
        ]
        .map(|value| value.to_string())
        .join("\n")
            + "\n",
    )
    .unwrap();
    let transcript = transcript.canonicalize().unwrap();
    harness.write_workspace(SESSION_A);
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,origin_cwd,transcript_path,transcript_source,
                completion_reason,source_end_reason,lifecycle_state,active,
                state_generation,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,?3,'version-pinned-recovery','unknown','unknown',
                       'observed',0,2,1,1)",
            params![
                SESSION_A,
                harness.project.to_str().unwrap(),
                transcript.to_str().unwrap()
            ],
        )
        .unwrap();
    drop(connection);

    set_old_mtime(&transcript);
    assert_success(&harness.recover(600_000, false, false));
    // Rescued once, judged not archive-worthy, back to the observed verdict shape. A husk
    // has no session-end verdict for `hook wait` to report, so poll the row directly.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
        let (lifecycle, attempts): (String, i64) = connection
            .query_row(
                "SELECT lifecycle_state,
                        (SELECT COUNT(*) FROM processing_attempts WHERE finished_at_ms IS NOT NULL)
                 FROM sessions WHERE source_session_id=?1",
                [SESSION_A],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        if lifecycle == "observed" && attempts == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker never returned the rescued husk to observed: {lifecycle} ({attempts})"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // An unchanged transcript is not rescued again: exactly one attempt, ever.
    assert_success(&harness.recover(600_000, false, false));
    assert_success(&harness.recover(600_000, false, false));
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let attempts: i64 = connection
        .query_row("SELECT COUNT(*) FROM processing_attempts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(attempts, 1);
    assert!(transcript.is_file(), "never-drop: the transcript survives");
}

/// Writes the user-only stub transcript from the live census (a user request with no
/// assistant or tool activity — abandoned before the first reply): never archive-worthy.
fn write_stub_transcript(harness: &Harness, session_id: &str) -> PathBuf {
    let transcript = harness
        .copilot_home
        .join("session-state")
        .join(session_id)
        .join("events.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(
        &transcript,
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
                "data": {"content": "UNANSWERED_REQUEST"},
            }),
        ]
        .map(|value| value.to_string())
        .join("\n")
            + "\n",
    )
    .unwrap();
    transcript.canonicalize().unwrap()
}

/// Inserts the exact husk row from the live census: swept into recovery, judged once by a
/// worker, and returned to `observed` with `active=0` and no session-end verdict.
fn insert_recovery_husk_row(harness: &Harness, session_id: &str, transcript: &Path) {
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,origin_cwd,transcript_path,transcript_source,
                completion_reason,source_end_reason,lifecycle_state,active,
                state_generation,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,?3,'version-pinned-recovery','unknown','unknown',
                       'observed',0,2,1,1)",
            params![
                session_id,
                harness.project.to_str().unwrap(),
                transcript.to_str().unwrap()
            ],
        )
        .unwrap();
}

/// Runs one recovery sweep over a husk row and waits until the worker has judged it not
/// archive-worthy and returned it to the settled `observed` verdict shape.
fn settle_husk(harness: &Harness, session_id: &str) {
    assert_success(&harness.recover(600_000, false, false));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
        let (lifecycle, attempts): (String, i64) = connection
            .query_row(
                "SELECT lifecycle_state,
                        (SELECT COUNT(*) FROM processing_attempts WHERE finished_at_ms IS NOT NULL)
                 FROM sessions WHERE source_session_id=?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        if lifecycle == "observed" && attempts == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "worker never settled the rescued husk: {lifecycle} ({attempts})"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Issue #50: a sweep-discovered stub the worker judges not archive-worthy must surface
/// under the `not-archive-worthy` label in `status` and `sessions` (text and JSON), not as
/// a phantom `observed` session. The stored lifecycle stays `observed` — the settled shape
/// issue #49's rescue keys on — so only the read-time label moves.
#[test]
fn settled_sweep_verdict_surfaces_as_not_archive_worthy_in_status_and_sessions() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("naw-visible-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = write_stub_transcript(&harness, SESSION_A);
    harness.write_workspace(SESSION_A);
    insert_recovery_husk_row(&harness, SESSION_A, &transcript);
    set_old_mtime(&transcript);
    settle_husk(&harness, SESSION_A);

    let status = harness.status_json();
    assert_eq!(status["sessions"]["total"], 1);
    assert_eq!(
        status["sessions"]["not_archive_worthy"], 1,
        "the settled verdict must be visible: {status}"
    );
    assert_eq!(
        status["sessions"]["observed"], 0,
        "a refused stub must not read as a live observed session: {status}"
    );

    let sessions = harness.sessions_json(None);
    let items = sessions["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["state"], "not-archive-worthy");
    assert_eq!(items[0]["lifecycle_state"], "observed");

    let filtered = harness.sessions_json(Some("not-archive-worthy"));
    assert_eq!(filtered["items"].as_array().unwrap().len(), 1);
    let observed_only = harness.sessions_json(Some("observed"));
    assert_eq!(observed_only["items"].as_array().unwrap().len(), 0);
}

/// Issue #50 reactivation guarantee: settling the verdict must not strand the stub. A
/// transcript that later grows a real reply presents new evidence, defeats the settle
/// dedupe, re-enters the normal interrupted pipeline, and archives.
#[test]
fn settled_stub_that_later_grows_reenters_the_pipeline_and_archives() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("naw-regrow-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = write_stub_transcript(&harness, SESSION_A);
    harness.write_workspace(SESSION_A);
    insert_recovery_husk_row(&harness, SESSION_A, &transcript);
    set_old_mtime(&transcript);
    settle_husk(&harness, SESSION_A);
    assert_eq!(harness.status_json()["sessions"]["not_archive_worthy"], 1);

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    writeln!(
        file,
        "{}",
        json!({
            "id": "initial-assistant",
            "timestamp": "2026-07-12T00:00:02.000Z",
            "parentId": "initial-user",
            "type": "assistant.message",
            "data": {"content": "a real answer arrived", "messageId": "initial-message"},
        })
    )
    .unwrap();
    drop(file);
    set_old_mtime(&transcript);
    assert_success(&harness.recover(600_000, false, false));
    assert_success(&harness.wait(SESSION_A, 15_000));

    let status = harness.status_json();
    assert_eq!(
        status["sessions"]["archived"], 1,
        "grown stub archives: {status}"
    );
    assert_eq!(status["sessions"]["not_archive_worthy"], 0);
    assert!(harness.archive_path(SESSION_A).is_file());
}

/// Issue #50 upgrade path: settled husk rows written before the verdict column existed
/// (`observed`, inactive, revision 0, no session-end, with a recorded succeeded attempt)
/// are relabeled by the additive-column backfill on the next open. A husk that was never
/// judged and a genuinely live observed row both keep the `observed` label.
#[test]
fn pre_upgrade_settled_husk_rows_are_relabeled_on_open() {
    let live_session = "33333333-3333-4333-8333-333333333333";
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("naw-upgrade-count");
    assert_success(&harness.register(&summarizer, 10_000));

    // The settled shape issue #49 left on the live store: judged once (a succeeded
    // attempt), returned to `observed` with no session-end verdict.
    let settled = write_stub_transcript(&harness, SESSION_A);
    insert_recovery_husk_row(&harness, SESSION_A, &settled);
    // An unjudged husk: same row shape, but no attempt was ever recorded.
    let unjudged = write_stub_transcript(&harness, SESSION_B);
    insert_recovery_husk_row(&harness, SESSION_B, &unjudged);
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "INSERT INTO processing_attempts(
                session_id,state_generation,retry_state,lease_token,
                started_at_ms,lease_expires_at_ms,finished_at_ms,outcome
             ) SELECT id,2,'interrupted','naw-upgrade-token',1,2,3,'succeeded'
               FROM sessions WHERE source_session_id=?1",
            [SESSION_A],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sessions(
                source_kind,source_session_id,origin_cwd,transcript_path,transcript_source,
                lifecycle_state,active,state_generation,created_at_ms,updated_at_ms
             ) VALUES ('copilot-cli',?1,?2,NULL,NULL,'observed',1,1,1,1)",
            params![live_session, harness.project.to_str().unwrap()],
        )
        .unwrap();
    // Strip the verdict column to simulate a database written before this release.
    connection
        .execute_batch("ALTER TABLE sessions DROP COLUMN not_archive_worthy_at_ms")
        .unwrap();
    drop(connection);

    let status = harness.status_json();
    assert_eq!(
        status["sessions"]["not_archive_worthy"], 1,
        "the pre-upgrade settled husk must be relabeled: {status}"
    );
    assert_eq!(
        status["sessions"]["observed"], 2,
        "the unjudged husk and the live session keep the observed label: {status}"
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

#[test]
fn settle_lost_settles_missing_transcripts_and_reactivates_on_restore() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("lost-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "LOST_REQUEST", "lost answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));

    // Reshape the archived row into the incident shape: permanently parked under a
    // missing-source verdict, with the transcript truly gone from disk.
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "UPDATE sessions SET lifecycle_state='failed', next_retry_at_ms=-1,
                last_error_category='source-missing'
             WHERE source_session_id=?1",
            params![SESSION_A],
        )
        .unwrap();
    drop(connection);
    let hidden = transcript.with_extension("hidden");
    fs::rename(&transcript, &hidden).unwrap();

    // Doctor names the real problem, not a size cap.
    let doctor = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["doctor", "--state-dir"])
        .arg(&harness.state)
        .output()
        .unwrap();
    let doctor_text = String::from_utf8_lossy(&doctor.stdout).into_owned();
    assert!(
        doctor_text.contains("transcript no longer exists"),
        "{doctor_text}"
    );
    assert!(
        !doctor_text.contains("raise --max-source-bytes"),
        "{doctor_text}"
    );

    // Settle: the verdict lands, and every surface reports it truthfully.
    let settle = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["settle-lost", "--all-missing", "--json", "--state-dir"])
        .arg(&harness.state)
        .output()
        .unwrap();
    assert_success(&settle);
    let report: Value = serde_json::from_slice(&settle.stdout).unwrap();
    assert_eq!(report["settled"], 1, "{report}");
    assert_eq!(report["items"][0]["result"], "settled", "{report}");

    let listed = harness.sessions_json(Some("transcript-lost"));
    assert_eq!(listed["returned"], 1, "{listed}");
    assert_eq!(listed["items"][0]["state"], "transcript-lost", "{listed}");
    assert!(harness.status_text().contains("transcript-lost=1"));

    // A second sweep finds nothing: settling is terminal while the file stays gone.
    let again = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["settle-lost", "--all-missing", "--json", "--state-dir"])
        .arg(&harness.state)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(report["candidates"], 0, "{report}");

    // Restore the transcript: retry-all's reactivation sweep lifts the verdict and the
    // normal pipeline re-archives from the restored bytes.
    fs::rename(&hidden, &transcript).unwrap();
    let retry = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["retry-all", "--json", "--state-dir"])
        .arg(&harness.state)
        .output()
        .unwrap();
    assert_success(&retry);
    let listed = harness.sessions_json(Some("transcript-lost"));
    assert_eq!(listed["returned"], 0, "{listed}");
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    let stamp: Option<i64> = connection
        .query_row(
            "SELECT transcript_lost_at_ms FROM sessions WHERE source_session_id=?1",
            params![SESSION_A],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stamp, None);
}

#[test]
fn settle_lost_refuses_present_transcripts_and_reports_the_named_target() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("present-count");
    assert_success(&harness.register(&summarizer, 10_000));
    let transcript = harness.write_transcript(SESSION_A, "PRESENT_REQUEST", "present answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));

    // A pre-#57 lumped park whose file still exists is a size-cap park, not a loss.
    let connection = Connection::open(harness.state.join("munshi.db")).unwrap();
    connection
        .execute(
            "UPDATE sessions SET lifecycle_state='failed', next_retry_at_ms=-1,
                last_error_category='source-failed'
             WHERE source_session_id=?1",
            params![SESSION_A],
        )
        .unwrap();
    drop(connection);

    let settle = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["settle-lost", SESSION_A, "--json", "--state-dir"])
        .arg(&harness.state)
        .output()
        .unwrap();
    // The named target was not settled: explicit non-zero exit, explicit reason — never a
    // silent zero-candidate success (the issue #54 lesson).
    assert_eq!(settle.status.code(), Some(2), "{settle:?}");
    let report: Value = serde_json::from_slice(&settle.stdout).unwrap();
    assert_eq!(
        report["items"][0]["result"], "transcript-present",
        "{report}"
    );
    assert_eq!(report["settled"], 0, "{report}");

    // An unregistered state directory degrades to an empty sweep, exit 0.
    let empty_dir = harness.root().join("never-registered");
    fs::create_dir_all(&empty_dir).unwrap();
    let sweep = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["settle-lost", "--all-missing", "--json", "--state-dir"])
        .arg(&empty_dir)
        .output()
        .unwrap();
    assert_success(&sweep);
    let report: Value = serde_json::from_slice(&sweep.stdout).unwrap();
    assert_eq!(report["candidates"], 0, "{report}");
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

    fn register_with_source_limit(
        &self,
        summarizer: &Path,
        timeout_ms: u64,
        max_source_bytes: usize,
    ) -> Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("register")
            .arg("--accept-transcript-processing")
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--output-dir")
            .arg(&self.output)
            .arg("--summarizer")
            .arg(summarizer)
            .arg("--timeout-ms")
            .arg(timeout_ms.to_string())
            .arg("--max-source-bytes")
            .arg(max_source_bytes.to_string())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn retry(&self, session_id: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("retry")
            .arg(session_id)
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--json")
            .output()
            .unwrap()
    }

    fn retry_force(&self, session_id: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("retry")
            .arg(session_id)
            .arg("--force")
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--json")
            .output()
            .unwrap()
    }

    fn status_text(&self) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("status")
            .arg("--state-dir")
            .arg(&self.state)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn status_json(&self) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_munshi"))
            .args(["status", "--json"])
            .arg("--state-dir")
            .arg(&self.state)
            .output()
            .unwrap();
        assert_success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn sessions_json(&self, state: Option<&str>) -> Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
        command
            .args(["sessions", "--json"])
            .arg("--state-dir")
            .arg(&self.state);
        if let Some(state) = state {
            command.args(["--state", state]);
        }
        let output = command.output().unwrap();
        assert_success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
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

    /// Stage the version-pinned `session-state/<id>/workspace.yaml` sibling record that
    /// carries a Copilot session's origin project directory.
    fn write_workspace(&self, session_id: &str) {
        self.write_workspace_with_cwd(session_id, &self.project.clone());
    }

    /// Like [`Self::write_workspace`], recording an arbitrary origin directory — used to
    /// stage recorded evidence for a directory that is then deleted (issue #40).
    fn write_workspace_with_cwd(&self, session_id: &str, cwd: &Path) {
        let path = self
            .copilot_home
            .join("session-state")
            .join(session_id)
            .join("workspace.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "id: {session_id}\ncwd: {}\nclient_name: github/cli\n",
                cwd.display()
            ),
        )
        .unwrap();
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
        self.queue_direct_with_origin(
            session_id,
            transcript,
            &self.project.clone(),
            stop_timestamp,
            end_timestamp,
        );
    }

    fn queue_direct_with_origin(
        &self,
        session_id: &str,
        transcript: &Path,
        origin: &Path,
        stop_timestamp: i64,
        end_timestamp: i64,
    ) {
        let mut state = StateStore::open(&self.state).unwrap();
        state
            .ingest_agent_stop(session_id, stop_timestamp, origin, transcript)
            .unwrap();
        state
            .ingest_session_end(
                session_id,
                end_timestamp,
                origin,
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

    /// A summarizer that records every invocation and then always exits nonzero: a
    /// deterministic `summary-failed` on every attempt.
    fn failing_summarizer(&self, count_name: &str) -> PathBuf {
        let script = self.root().join(format!("{count_name}.sh"));
        let count = self.root().join(count_name);
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\ncat >/dev/null\nprintf x >> '{}'\nexit 7\n",
                count.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script.canonicalize().unwrap()
    }

    /// A summarizer that fails deterministically for `CLOG_REQUEST` transcripts (counting each
    /// failing call) and succeeds for everything else.
    fn clogging_summarizer(&self, count_name: &str) -> PathBuf {
        let script = self.root().join(format!("{count_name}.sh"));
        let count = self.root().join(count_name);
        let body = format!(
            r#"#!/bin/sh
set -eu
input=$(cat)
case "$input" in
  *CLOG_REQUEST*)
    printf x >> '{}'
    exit 7
    ;;
esac
printf '%s' '{{"title":"Starved session archive","goal":"Archive the healthy session.","work_completed":["Archived the starved session."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["fairness"]}}'
"#,
            count.display()
        );
        fs::write(&script, body).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script.canonicalize().unwrap()
    }

    /// How many times a counting summarizer script has been invoked.
    fn summarizer_calls(&self, count_name: &str) -> usize {
        fs::read(self.root().join(count_name))
            .map(|marks| marks.len())
            .unwrap_or(0)
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

fn session_retry_park(harness: &Harness, session_id: &str) -> (Option<String>, Option<i64>) {
    Connection::open(harness.state.join("munshi.db"))
        .unwrap()
        .query_row(
            "SELECT last_error_category,next_retry_at_ms
             FROM sessions WHERE source_session_id=?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn session_failure_streak(harness: &Harness, session_id: &str) -> i64 {
    Connection::open(harness.state.join("munshi.db"))
        .unwrap()
        .query_row(
            "SELECT failure_streak FROM sessions WHERE source_session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// Simulates a session's scheduled backoff having elapsed without touching the failure streak,
/// so escalation across attempts can be exercised without waiting out real wall-clock delays.
fn make_retry_due(harness: &Harness, session_id: &str) {
    let changed = Connection::open(harness.state.join("munshi.db"))
        .unwrap()
        .execute(
            "UPDATE sessions SET next_retry_at_ms=1
             WHERE source_session_id=?1 AND next_retry_at_ms>=0",
            [session_id],
        )
        .unwrap();
    assert_eq!(changed, 1, "session {session_id} had no pending backoff");
}

fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
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
