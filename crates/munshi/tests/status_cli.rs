use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

#[test]
fn configuration_check_json_distinguishes_disabled_and_delivery_states() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    let baseline = harness.configuration_check_json();
    assert_eq!(baseline["schema_version"], 1);
    assert_eq!(baseline["command"], "configuration-check");
    assert_eq!(
        baseline["configuration"]["capture_state"], "enabled",
        "baseline capture should be enabled"
    );
    assert_eq!(
        baseline["configuration"]["delivery_state"], "disabled",
        "baseline delivery should be disabled"
    );
    assert_eq!(baseline["configuration"]["archive_git_history"], false);
    assert_eq!(baseline["configuration"]["disabled_projects"], 0);
    assert_eq!(baseline["configuration"]["runtime_compatible"], true);

    assert_success(&harness.project_disable());
    let disabled = harness.configuration_check_json();
    assert_eq!(
        disabled["configuration"]["capture_state"],
        "disabled-project"
    );
    assert_eq!(disabled["configuration"]["disabled_projects"], 1);

    harness.mutate_config(|config| {
        config["summary_delivery"]["enabled"] = Value::Bool(true);
    });
    let delivery = harness.configuration_check_json();
    assert_eq!(
        delivery["configuration"]["delivery_state"],
        "delivery-related"
    );
    assert_eq!(
        delivery["configuration"]["capture_state"],
        "disabled-project"
    );
}

#[test]
fn status_sessions_and_show_json_contracts_cover_required_states() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    let archived = "11111111-1111-4111-8111-111111111111";
    let revision = "22222222-2222-4222-8222-222222222222";
    let summary_pending = "33333333-3333-4333-8333-333333333333";
    let interrupted = "44444444-4444-4444-8444-444444444444";
    let failed = "55555555-5555-4555-8555-555555555555";
    let disabled = "5f2a7c4a-5ddb-4768-b362-68e4d8a0ad6c";

    let archived_events = harness.write_transcript(archived, "ARCHIVE_REQUEST", "archive");
    harness.complete_lifecycle(archived, &archived_events, 10_000, 10_001);
    let archived_wait = harness.wait(archived, 5_000);
    assert_success(&archived_wait);

    let revision_events = harness.write_transcript(revision, "REVISION_INITIAL", "first");
    harness.complete_lifecycle(revision, &revision_events, 11_000, 11_001);
    let revision_wait = harness.wait(revision, 5_000);
    assert_success(&revision_wait);
    harness.append_turn(&revision_events, "REVISION_DELTA", "second");
    assert_success(&harness.hook(
        "agent-stop",
        &json!({
            "sessionId": revision,
            "timestamp": 11_010_u64,
            "cwd": harness.project,
            "transcriptPath": revision_events,
            "stopReason": "end_turn",
        }),
    ));

    assert_success(&harness.hook(
        "session-end",
        &json!({
            "sessionId": summary_pending,
            "timestamp": 12_000_u64,
            "cwd": harness.project,
            "reason": "complete",
        }),
    ));

    assert_success(&harness.hook(
        "session-end",
        &json!({
            "sessionId": interrupted,
            "timestamp": 13_000_u64,
            "cwd": harness.project,
            "reason": "user_exit",
        }),
    ));

    let failed_events = harness.write_transcript(failed, "FAIL_REQUEST", "fails");
    harness.complete_lifecycle(failed, &failed_events, 14_000, 14_001);
    let failed_wait = harness.wait(failed, 5_000);
    assert!(!failed_wait.status.success(), "failed wait should fail");

    assert_success(&harness.project_disable());
    let disabled_events = harness.write_transcript(disabled, "DISABLED_REQUEST", "blocked");
    harness.complete_lifecycle(disabled, &disabled_events, 15_000, 15_001);
    let disabled_wait = harness.wait(disabled, 5_000);
    assert!(
        !disabled_wait.status.success(),
        "disabled-project wait should fail"
    );

    let status = harness.status_json();
    assert_eq!(status["schema_version"], 1);
    assert_eq!(status["command"], "status");
    assert_eq!(status["sessions"]["archived"], 1);
    assert_eq!(status["sessions"]["revision_pending"], 1);
    assert_eq!(status["sessions"]["summary_pending"], 1);
    assert_eq!(status["sessions"]["interrupted"], 1);
    assert_eq!(status["sessions"]["failed"], 1);
    assert_eq!(status["sessions"]["delivery_related"], 0);
    assert_eq!(status["sessions"]["disabled_project"], 1);

    let sessions = harness.sessions_json(None);
    assert_eq!(sessions["schema_version"], 1);
    assert_eq!(sessions["command"], "sessions");
    let items = sessions["items"].as_array().unwrap();
    let states = items
        .iter()
        .map(|item| item["state"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    for required in [
        "archived",
        "revision-pending",
        "summary-pending",
        "interrupted",
        "failed",
        "disabled-project",
    ] {
        assert!(
            states.contains(required),
            "missing state {required} in sessions output"
        );
    }
    for item in items {
        assert!(item.get("transcript_path").is_none());
        assert_eq!(
            item["source"], "copilot",
            "session list items must expose their source kind"
        );
    }

    let failed_only = harness.sessions_json(Some("failed"));
    assert!(
        failed_only["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| { item["state"] == "failed" || item["state"] == "delivery-related" })
    );

    let show_output = harness.show_raw(archived);
    assert_success(&show_output);
    let show: Value = serde_json::from_slice(&show_output.stdout).unwrap();
    assert_eq!(show["schema_version"], 1);
    assert_eq!(show["command"], "show");
    assert_eq!(show["found"], true);
    assert_eq!(show["session"]["state"], "archived");
    assert_eq!(show["session"]["source_kind"], "copilot");
    assert_eq!(
        show["session"]["summary"]["title"],
        "Contract summary title"
    );
    assert!(!String::from_utf8_lossy(&show_output.stdout).contains("events.jsonl"));
}

#[test]
fn retry_and_retry_all_are_idempotent() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    let retry_one = "66666666-6666-4666-8666-666666666666";
    let retry_all = "77777777-7777-4777-8777-777777777777";

    let retry_one_events = harness.write_transcript(retry_one, "FAIL_REQUEST", "fails");
    harness.complete_lifecycle(retry_one, &retry_one_events, 20_000, 20_001);
    let failed_wait = harness.wait(retry_one, 5_000);
    assert!(!failed_wait.status.success());

    harness.replace_transcript(retry_one, "RECOVER_REQUEST", "works now");
    let retry_archived = harness.retry_json(retry_one, true);
    assert_eq!(retry_archived["result"], "archived");
    assert_eq!(retry_archived["force"], true);

    let retry_again = harness.retry_json(retry_one, false);
    assert_eq!(retry_again["result"], "not-eligible");
    assert_eq!(retry_again["state_before"], "archived");
    assert_eq!(retry_again["force"], false);

    let retry_all_events = harness.write_transcript(retry_all, "FAIL_REQUEST", "fails");
    harness.complete_lifecycle(retry_all, &retry_all_events, 21_000, 21_001);
    let failed_wait = harness.wait(retry_all, 5_000);
    assert!(!failed_wait.status.success());
    harness.replace_transcript(retry_all, "RECOVER_ALL_REQUEST", "works now");

    let retry_all_once = harness.retry_all_json(true, 32);
    assert_eq!(retry_all_once["attempted"], 1);
    assert_eq!(retry_all_once["archived"], 1);
    assert_eq!(retry_all_once["items"][0]["session_id"], retry_all);
    assert_eq!(retry_all_once["items"][0]["result"], "archived");
    assert_eq!(retry_all_once["force"], true);

    let retry_all_again = harness.retry_all_json(false, 32);
    assert_eq!(retry_all_again["attempted"], 0);
    assert_eq!(retry_all_again["force"], false);
}

#[test]
fn retry_all_limit_one_does_not_mutate_unselected_permanent_failure() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    let first = "88888888-8888-4888-8888-888888888888";
    let second = "99999999-9999-4999-8999-999999999999";

    let first_events = harness.write_transcript(first, "FAIL_REQUEST", "fails");
    harness.complete_lifecycle(first, &first_events, 30_000, 30_001);
    assert!(!harness.wait(first, 5_000).status.success());
    harness.replace_transcript(first, "RECOVER_FIRST_REQUEST", "works now");

    let second_events = harness.write_transcript(second, "FAIL_REQUEST", "fails");
    harness.complete_lifecycle(second, &second_events, 31_000, 31_001);
    assert!(!harness.wait(second, 5_000).status.success());
    harness.replace_transcript(second, "RECOVER_SECOND_REQUEST", "works now");

    harness.set_next_retry(first, None);
    harness.set_next_retry(second, Some(-1));

    let once = harness.retry_all_json(false, 1);
    assert_eq!(once["attempted"], 1);
    assert_eq!(once["archived"], 1);
    assert_eq!(once["items"][0]["session_id"], first);
    assert_eq!(once["items"][0]["result"], "archived");
    assert_eq!(once["force"], false);

    assert_eq!(harness.next_retry(second), Some(-1));

    let again = harness.retry_all_json(false, 1);
    assert_eq!(again["attempted"], 0);
    assert_eq!(again["archived"], 0);
    assert_eq!(again["force"], false);
}

#[test]
fn retry_all_force_limit_one_only_resets_selected_failed_session() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    let first = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let second = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    let first_events = harness.write_transcript(first, "FAIL_REQUEST", "fails");
    harness.complete_lifecycle(first, &first_events, 32_000, 32_001);
    assert!(!harness.wait(first, 5_000).status.success());
    harness.replace_transcript(first, "RECOVER_FIRST_FORCE_REQUEST", "works now");

    let second_events = harness.write_transcript(second, "FAIL_REQUEST", "fails");
    harness.complete_lifecycle(second, &second_events, 33_000, 33_001);
    assert!(!harness.wait(second, 5_000).status.success());
    harness.replace_transcript(second, "RECOVER_SECOND_FORCE_REQUEST", "works now");

    harness.set_next_retry(first, Some(-1));
    harness.set_next_retry(second, Some(-1));

    let forced = harness.retry_all_json(true, 1);
    assert_eq!(forced["attempted"], 1);
    assert_eq!(forced["archived"], 1);
    assert_eq!(forced["items"][0]["session_id"], first);
    assert_eq!(forced["force"], true);

    assert_eq!(harness.next_retry(second), Some(-1));

    let without_force = harness.retry_all_json(false, 1);
    assert_eq!(without_force["attempted"], 0);
    assert_eq!(without_force["force"], false);
}

#[test]
fn doctor_json_reports_runtime_failures() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    let healthy = harness.doctor_json();
    assert_eq!(healthy["schema_version"], 1);
    assert_eq!(healthy["command"], "doctor");
    assert_ne!(healthy["status"], "error");
    let git_check = healthy["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["code"] == "archive-git-repository")
        .cloned()
        .expect("archive-git-repository check exists");
    assert_eq!(git_check["status"], "ok");

    harness.mutate_config(|config| {
        config["summarizer"]["executable"] = Value::String(
            harness
                .root()
                .join("missing-summarizer")
                .to_string_lossy()
                .into_owned(),
        );
    });

    let unhealthy = harness.doctor_json();
    assert_eq!(unhealthy["status"], "error");
    let summarizer_check = unhealthy["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["code"] == "summarizer-executable")
        .cloned()
        .expect("summarizer-executable check exists");
    assert_eq!(summarizer_check["status"], "error");
}

#[test]
fn doctor_checks_archive_git_repository_when_enabled() {
    let harness = Harness::new();
    assert_success(&harness.register_with_options(fake("status-contract.sh"), 2_000, true));

    let healthy = harness.doctor_json();
    let git_check = healthy["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["code"] == "archive-git-repository")
        .cloned()
        .expect("archive-git-repository check exists");
    assert_eq!(git_check["status"], "ok");

    fs::remove_dir_all(harness.output.join(".git")).unwrap();
    let broken = harness.doctor_json();
    let git_check = broken["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["code"] == "archive-git-repository")
        .cloned()
        .expect("archive-git-repository check exists");
    assert_eq!(git_check["status"], "error");
}

/// Sessions parked permanently on a size cap (issue #41) surface a dedicated `size-cap-parked`
/// doctor warning naming the limit flag whose raise lifts them, instead of hiding inside the
/// generic failed/parked counters.
#[test]
fn doctor_hints_sessions_parked_on_a_size_cap() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    let healthy = harness.doctor_json();
    assert!(
        healthy["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|check| check["code"] != "size-cap-parked"),
        "a healthy report carries no size-cap hint"
    );

    let source_parked = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let input_parked = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    for (session_id, stop) in [(source_parked, 40_000), (input_parked, 41_000)] {
        let events = harness.write_transcript(session_id, "FAIL_REQUEST", "fails");
        harness.complete_lifecycle(session_id, &events, stop, stop + 1);
        assert!(!harness.wait(session_id, 5_000).status.success());
    }
    // Parked after both lifecycles: a later hook run sweeps stale source-limit parks (issue #44)
    // whose transcripts fit the configured limit, and these fabricated ones would qualify.
    harness.park_on_size_cap(source_parked, "source-failed");
    harness.park_on_size_cap(input_parked, "summary-input-limit");

    let report = harness.doctor_json();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["code"] == "size-cap-parked")
        .cloned()
        .expect("size-cap-parked check exists");
    assert_eq!(check["status"], "warning");
    let message = check["message"].as_str().unwrap();
    assert!(
        message.contains("2 session(s) parked on a size cap"),
        "{message}"
    );
    assert!(
        message.contains("1 source-failed (raise --max-source-bytes)"),
        "{message}"
    );
    assert!(
        message.contains("1 summary-input-limit (raise --max-input-bytes)"),
        "{message}"
    );
    assert_eq!(report["sessions"]["parked"], 2);
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
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-status-test-artifacts");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("status-case-")
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

    fn register(&self, summarizer: PathBuf, timeout_ms: u64) -> Output {
        self.register_with_options(summarizer, timeout_ms, false)
    }

    fn register_with_options(
        &self,
        summarizer: PathBuf,
        timeout_ms: u64,
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
            .arg(&self.output)
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

    fn configuration_check_json(&self) -> Value {
        self.json_output([
            "configuration-check",
            "--state-dir",
            self.state.to_str().unwrap(),
            "--json",
        ])
    }

    fn status_json(&self) -> Value {
        self.json_output([
            "status",
            "--state-dir",
            self.state.to_str().unwrap(),
            "--json",
        ])
    }

    fn sessions_json(&self, state: Option<&str>) -> Value {
        let mut args = vec![
            "sessions".to_owned(),
            "--state-dir".to_owned(),
            self.state.display().to_string(),
            "--json".to_owned(),
        ];
        if let Some(state) = state {
            args.push("--state".to_owned());
            args.push(state.to_owned());
        }
        self.json_output(args)
    }

    fn show_raw(&self, session_id: &str) -> Output {
        self.output([
            "show",
            session_id,
            "--state-dir",
            self.state.to_str().unwrap(),
            "--json",
        ])
    }

    fn retry_json(&self, session_id: &str, force: bool) -> Value {
        let mut args = vec![
            "retry".to_owned(),
            session_id.to_owned(),
            "--state-dir".to_owned(),
            self.state.display().to_string(),
            "--json".to_owned(),
        ];
        if force {
            args.push("--force".to_owned());
        }
        self.json_output(args)
    }

    fn retry_all_json(&self, force: bool, limit: usize) -> Value {
        let mut args = vec![
            "retry-all".to_owned(),
            "--state-dir".to_owned(),
            self.state.display().to_string(),
            "--json".to_owned(),
            "--limit".to_owned(),
            limit.to_string(),
        ];
        if force {
            args.push("--force".to_owned());
        }
        self.json_output(args)
    }

    fn doctor_json(&self) -> Value {
        self.json_output([
            "doctor",
            "--state-dir",
            self.state.to_str().unwrap(),
            "--json",
        ])
    }

    fn output<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .args(args)
            .output()
            .unwrap()
    }

    fn json_output<I, S>(&self, args: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.output(args);
        assert!(
            !output.stdout.is_empty(),
            "stdout unexpectedly empty; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("valid JSON output")
    }

    fn mutate_config(&self, update: impl FnOnce(&mut Value)) {
        let config_path = self.state.join("config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        update(&mut config);
        let mut bytes = serde_json::to_vec_pretty(&config).unwrap();
        bytes.push(b'\n');
        fs::write(config_path, bytes).unwrap();
    }

    fn set_next_retry(&self, session_id: &str, next_retry: Option<i64>) {
        let connection = Connection::open(self.state.join("munshi.db")).unwrap();
        connection
            .execute(
                "UPDATE sessions SET next_retry_at_ms=?2
                 WHERE source_kind='copilot-cli' AND source_session_id=?1",
                rusqlite::params![session_id, next_retry],
            )
            .unwrap();
    }

    /// Fabricates the permanent size-cap park of issues #38/#44: a deterministic
    /// `source-failed`/`summary-input-limit` verdict with a negative retry marker.
    fn park_on_size_cap(&self, session_id: &str, category: &str) {
        let connection = Connection::open(self.state.join("munshi.db")).unwrap();
        connection
            .execute(
                "UPDATE sessions SET next_retry_at_ms=-1, last_error_category=?2
                 WHERE source_kind='copilot-cli' AND source_session_id=?1",
                rusqlite::params![session_id, category],
            )
            .unwrap();
    }

    fn next_retry(&self, session_id: &str) -> Option<i64> {
        let connection = Connection::open(self.state.join("munshi.db")).unwrap();
        connection
            .query_row(
                "SELECT next_retry_at_ms FROM sessions
                 WHERE source_kind='copilot-cli' AND source_session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap()
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

    fn complete_lifecycle(
        &self,
        session_id: &str,
        transcript: &Path,
        stop_timestamp: u64,
        end_timestamp: u64,
    ) {
        assert_success(&self.hook(
            "agent-stop",
            &json!({
                "sessionId": session_id,
                "timestamp": stop_timestamp,
                "cwd": self.project,
                "transcriptPath": transcript,
                "stopReason": "end_turn",
            }),
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

fn fake(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/manual/fake-summarizer")
        .join(name);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.canonicalize().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
