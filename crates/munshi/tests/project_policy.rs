use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use munshi::{BudgetOutcome, CompletionReason, StateStore, parse_archive_markdown};
use serde_json::{Value, json};
use tempfile::TempDir;

const SESSION_A: &str = "11111111-1111-4111-8111-111111111111";
const SESSION_B: &str = "22222222-2222-4222-8222-222222222222";

#[test]
fn explicit_disable_defers_processing_and_preserves_existing_archive() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("disable-count");
    assert_success(&harness.register(&summarizer));
    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    let archive_path = harness.archive_path(SESSION_A);
    let before = fs::read(&archive_path).unwrap();
    let before_markdown = parse_archive_markdown(std::str::from_utf8(&before).unwrap()).unwrap();
    assert_eq!(before_markdown.summary_revision, 1);

    assert_success(&harness.project_disable());
    let config: Value =
        serde_json::from_slice(&fs::read(harness.state.join("config.json")).unwrap()).unwrap();
    assert_eq!(
        config["policy"]["disabled_projects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    harness.append_turn(&transcript, "DELTA_REQUEST", "delta answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 20, 21);
    std::thread::sleep(Duration::from_millis(300));

    let after = fs::read(&archive_path).unwrap();
    assert_eq!(
        before, after,
        "disabled project must not rewrite its archive"
    );
    let session = harness.get_session(SESSION_A);
    assert_eq!(
        session.last_error_category.as_deref(),
        Some("project-disabled")
    );
    assert_ne!(session.lifecycle_state, "archived");

    assert_success(&harness.project_enable());
    assert_success(&harness.recover(0, false, false));
    assert_success(&harness.wait(SESSION_A, 5_000));
    let after_enable = fs::read(&archive_path).unwrap();
    let enabled_markdown =
        parse_archive_markdown(std::str::from_utf8(&after_enable).unwrap()).unwrap();
    assert_eq!(enabled_markdown.summary_revision, 2);
}

#[test]
fn project_status_reports_effective_policy_and_reregistration_preserves_disable() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("status-count");
    assert_success(&harness.register_with_budgets(&summarizer, 3, 9, 2));

    let status = harness.project_status_json();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["max_calls_per_hour"], 3);
    assert_eq!(status["max_calls_per_day"], 9);

    assert_success(&harness.project_disable());
    let status = harness.project_status_json();
    assert_eq!(status["enabled"], false);
    assert_eq!(status["reason"], "project-disabled");

    // Re-registering with the same arguments must not silently re-enable the project.
    assert_success(&harness.register_with_budgets(&summarizer, 3, 9, 2));
    let status = harness.project_status_json();
    assert_eq!(status["enabled"], false);
}

#[test]
fn nearest_parent_override_disables_without_cli_and_can_be_reenabled() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("override-count");
    assert_success(&harness.register(&summarizer));
    fs::write(
        harness.project.join(".munshi.toml"),
        "[project]\nenabled = false\n",
    )
    .unwrap();

    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    std::thread::sleep(Duration::from_millis(300));

    assert!(!harness.archive_exists(SESSION_A));
    let session = harness.get_session(SESSION_A);
    assert_eq!(
        session.last_error_category.as_deref(),
        Some("project-override-disabled")
    );

    fs::write(
        harness.project.join(".munshi.toml"),
        "[project]\nenabled = true\n",
    )
    .unwrap();
    assert_success(&harness.recover(0, false, false));
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert!(harness.archive_exists(SESSION_A));
}

#[test]
fn malformed_project_override_fails_closed() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("invalid-override-count");
    assert_success(&harness.register(&summarizer));
    fs::write(
        harness.project.join(".munshi.toml"),
        "this is not [valid toml",
    )
    .unwrap();

    let transcript = harness.write_transcript(SESSION_A, "INITIAL_REQUEST", "initial answer");
    harness.complete_lifecycle(SESSION_A, &transcript, 10, 11);
    std::thread::sleep(Duration::from_millis(300));

    assert!(!harness.archive_exists(SESSION_A));
    let session = harness.get_session(SESSION_A);
    assert_eq!(
        session.last_error_category.as_deref(),
        Some("project-override-invalid")
    );
}

#[test]
fn concurrency_budget_defers_second_session_until_capacity_frees() {
    let harness = Harness::new();
    let summarizer = harness.sleeping_summarizer("concurrency-count", 1);
    assert_success(&harness.register_with_budgets(&summarizer, 100, 1000, 1));
    let transcript_a = harness.write_transcript(SESSION_A, "A_REQUEST", "a answer");
    let transcript_b = harness.write_transcript(SESSION_B, "B_REQUEST", "b answer");
    harness.queue_direct(SESSION_A, &transcript_a, 10, 11);
    harness.queue_direct(SESSION_B, &transcript_b, 20, 21);

    let mut first = harness.spawn_worker(SESSION_A);
    std::thread::sleep(Duration::from_millis(250));
    let second = harness.worker(SESSION_B);
    assert!(second.status.success());
    let session_b = harness.get_session(SESSION_B);
    assert_eq!(
        session_b.last_error_category.as_deref(),
        Some("concurrency-deferred")
    );
    assert!(!harness.archive_exists(SESSION_B));

    assert!(first.wait().unwrap().success());
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert!(harness.archive_exists(SESSION_A));

    assert_success(&harness.recover(0, false, false));
    assert_success(&harness.wait(SESSION_B, 5_000));
    assert!(harness.archive_exists(SESSION_B));
}

#[test]
fn hourly_budget_defers_second_summarization_in_the_same_project() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("hourly-count");
    assert_success(&harness.register_with_budgets(&summarizer, 1, 1000, 2));
    let transcript_a = harness.write_transcript(SESSION_A, "A_REQUEST", "a answer");
    harness.complete_lifecycle(SESSION_A, &transcript_a, 10, 11);
    assert_success(&harness.wait(SESSION_A, 5_000));
    assert!(harness.archive_exists(SESSION_A));

    let transcript_b = harness.write_transcript(SESSION_B, "B_REQUEST", "b answer");
    harness.complete_lifecycle(SESSION_B, &transcript_b, 20, 21);
    std::thread::sleep(Duration::from_millis(300));

    assert!(!harness.archive_exists(SESSION_B));
    let session_b = harness.get_session(SESSION_B);
    assert_eq!(
        session_b.last_error_category.as_deref(),
        Some("budget-hourly-exceeded")
    );

    // Forcing a retry cannot manufacture budget: the session must remain deferred.
    assert_success(&harness.recover(0, true, false));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!harness.archive_exists(SESSION_B));
}

#[test]
fn summarizer_call_budget_state_tracks_rolling_windows_and_enforces_the_limit() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("state-budget-count");
    assert_success(&harness.register(&summarizer));
    let mut state = StateStore::open(&harness.state).unwrap();
    let now = 1_000_000_000_i64;
    assert_eq!(
        state
            .reserve_summarizer_call("example/project", now, 2, 1_000)
            .unwrap(),
        BudgetOutcome::Reserved
    );
    assert_eq!(
        state
            .reserve_summarizer_call("example/project", now + 1_000, 2, 1_000)
            .unwrap(),
        BudgetOutcome::Reserved
    );
    // A third call within the same rolling hour exceeds the limit of 2 and must be refused
    // rather than silently recorded.
    assert_eq!(
        state
            .reserve_summarizer_call("example/project", now + 2_000, 2, 1_000)
            .unwrap(),
        BudgetOutcome::HourlyExceeded
    );
    assert_eq!(
        state
            .reserve_summarizer_call("other/project", now + 1_000, 2, 1_000)
            .unwrap(),
        BudgetOutcome::Reserved
    );
    assert_eq!(
        state
            .summarizer_calls_since("example/project", now - 1)
            .unwrap(),
        2
    );
    assert_eq!(
        state
            .summarizer_calls_since("other/project", now - 1)
            .unwrap(),
        1
    );
    assert_eq!(state.count_active_processing().unwrap(), 0);
}

#[test]
fn budget_reservation_never_oversubscribes_under_concurrent_racers() {
    let harness = Harness::new();
    let summarizer = harness.revision_summarizer("budget-race-count");
    assert_success(&harness.register(&summarizer));

    const MAX_PER_HOUR: u32 = 5;
    const RACERS: usize = 16;
    let now = 2_000_000_000_i64;
    let handles: Vec<_> = (0..RACERS)
        .map(|_| {
            let state_dir = harness.state.clone();
            std::thread::spawn(move || {
                // Each thread opens its own connection to the same database file, the same way
                // each independent `munshi hook-worker` process would.
                let mut state = StateStore::open(&state_dir).unwrap();
                state
                    .reserve_summarizer_call("race/project", now, MAX_PER_HOUR, 1_000)
                    .unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let reserved = outcomes
        .iter()
        .filter(|outcome| **outcome == BudgetOutcome::Reserved)
        .count();
    assert_eq!(reserved, MAX_PER_HOUR as usize);
    assert_eq!(outcomes.len() - reserved, RACERS - MAX_PER_HOUR as usize);
    let recorded = StateStore::open(&harness.state)
        .unwrap()
        .summarizer_calls_since("race/project", now - 1)
        .unwrap();
    assert_eq!(recorded, MAX_PER_HOUR as i64);
}

#[test]
fn concurrency_limit_of_one_never_overlaps_summarizer_across_real_processes() {
    let harness = Harness::new();
    let (summarizer, lock_dir, violations) = harness.exclusive_sleeping_summarizer("excl-count", 1);
    assert_success(&harness.register_with_budgets(&summarizer, 1_000, 1_000, 1));

    let sessions = [
        "33333333-3333-4333-8333-333333333333",
        "44444444-4444-4444-8444-444444444444",
        "55555555-5555-4555-8555-555555555555",
    ];
    for (index, session_id) in sessions.iter().enumerate() {
        let transcript = harness.write_transcript(
            session_id,
            &format!("REQUEST_{index}"),
            &format!("ANSWER_{index}"),
        );
        harness.queue_direct(
            session_id,
            &transcript,
            10 + index as i64,
            11 + index as i64,
        );
    }

    // Spawn all three worker processes back to back with no synchronization between them: this
    // is the real, deterministic cross-process race the atomic concurrency check must survive.
    let mut children: Vec<_> = sessions.iter().map(|id| harness.spawn_worker(id)).collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }

    // With max_concurrency=1 at most one of the three could have claimed and run immediately;
    // the others deferred. Recover repeatedly so each deferred session gets a turn once the
    // single slot frees up.
    for _ in 0..(sessions.len() * 5) {
        if sessions.iter().all(|id| harness.archive_exists(id)) {
            break;
        }
        let _ = harness.recover(0, false, false);
        std::thread::sleep(Duration::from_millis(300));
    }

    for session_id in sessions {
        assert!(
            harness.archive_exists(session_id),
            "session {session_id} was never archived"
        );
    }
    assert!(
        !violations.exists(),
        "summarizer ran concurrently with itself: {}",
        fs::read_to_string(&violations).unwrap_or_default()
    );
    assert!(!lock_dir.exists(), "exclusive lock directory was left held");
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
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-policy-test-artifacts");
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
                .args([
                    "remote",
                    "add",
                    "origin",
                    "git@github.com:surdy/munshi-policy.git"
                ])
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

    fn register(&self, summarizer: &Path) -> Output {
        self.register_with_budgets(summarizer, 10, 50, 2)
    }

    fn register_with_budgets(
        &self,
        summarizer: &Path,
        max_calls_per_hour: u32,
        max_calls_per_day: u32,
        max_concurrency: usize,
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
            .arg("10000")
            .arg("--max-calls-per-hour")
            .arg(max_calls_per_hour.to_string())
            .arg("--max-calls-per-day")
            .arg(max_calls_per_day.to_string())
            .arg("--max-concurrency")
            .arg(max_concurrency.to_string())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn project_disable(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("project")
            .arg("disable")
            .arg(&self.project)
            .arg("--state-dir")
            .arg(&self.state)
            .output()
            .unwrap()
    }

    fn project_enable(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("project")
            .arg("enable")
            .arg(&self.project)
            .arg("--state-dir")
            .arg(&self.state)
            .output()
            .unwrap()
    }

    fn project_status_json(&self) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("project")
            .arg("status")
            .arg(&self.project)
            .arg("--state-dir")
            .arg(&self.state)
            .output()
            .unwrap();
        assert_success(&output);
        let text = String::from_utf8(output.stdout).unwrap();
        let mut fields = serde_json::Map::new();
        for part in text.split_whitespace() {
            if let Some((key, value)) = part.split_once('=') {
                let parsed = match value {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "none" => Value::Null,
                    other => other
                        .parse::<i64>()
                        .map(Value::from)
                        .unwrap_or_else(|_| Value::String(other.to_owned())),
                };
                fields.insert(key.to_owned(), parsed);
            }
        }
        Value::Object(fields)
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

    fn archive_exists(&self, session_id: &str) -> bool {
        let Ok(mut entries) = fs::read_dir(&self.output) else {
            return false;
        };
        let Some(Ok(entry)) = entries.next() else {
            return false;
        };
        entry.path().join(format!("{session_id}.md")).exists()
    }

    fn get_session(&self, session_id: &str) -> munshi::SessionRecord {
        StateStore::open(&self.state)
            .unwrap()
            .get_session(session_id)
            .unwrap()
            .unwrap()
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
  *\"previous_summary\"*)
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
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\ncat >/dev/null\nprintf x >> '{}'\nsleep {}\nprintf '%s' '{}'\n",
                count.display(),
                seconds,
                r#"{"title":"Concurrent archive","goal":"Test concurrency budgets.","work_completed":["Processed one session."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["concurrency"]}"#
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script.canonicalize().unwrap()
    }

    /// A summarizer that proves mutual exclusion using an atomic `mkdir` as a lock: if a second
    /// invocation ever runs while an earlier one still holds the directory, it records an
    /// overlap violation instead of silently succeeding. This avoids relying on OS timestamp
    /// resolution to detect a same-instant overlap.
    fn exclusive_sleeping_summarizer(
        &self,
        count_name: &str,
        seconds: u64,
    ) -> (PathBuf, PathBuf, PathBuf) {
        let script = self.root().join(format!("{count_name}.sh"));
        let lock_dir = self.root().join(format!("{count_name}.lock"));
        let violations = self.root().join(format!("{count_name}.violations"));
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nset -eu\ncat >/dev/null\nif ! mkdir '{lock}' 2>/dev/null; then\n  printf 'overlap\\n' >> '{violations}'\n  exit 1\nfi\nsleep {seconds}\nrmdir '{lock}'\nprintf '%s' '{json}'\n",
                lock = lock_dir.display(),
                violations = violations.display(),
                seconds = seconds,
                json = r#"{"title":"Exclusive run","goal":"Prove mutual exclusion.","work_completed":["Ran alone."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["concurrency"]}"#
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        (script.canonicalize().unwrap(), lock_dir, violations)
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
    .iter()
    .map(|value| value.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
