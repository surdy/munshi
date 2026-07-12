use std::fs;
use std::io::Cursor;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use munshi::{DisclosureDecision, accept_disclosure};
use serde_json::{Value, json};
use tempfile::TempDir;

const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";

#[test]
fn disclosure_requires_explicit_noninteractive_acceptance_and_prompt_is_testable() {
    let mut output = Vec::new();
    let error = accept_disclosure(false, &mut Cursor::new(b""), false, &mut output).unwrap_err();
    assert!(error.to_string().contains("--accept-disclosure"));
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("summarization is ON by default"));
    assert!(text.contains("NO secret redaction"));
    assert!(text.contains("sent to the summarizer executable"));
    assert!(text.contains("local Markdown"));
    assert!(text.contains("Remote delivery remains DISABLED"));

    let mut output = Vec::new();
    let decision =
        accept_disclosure(false, &mut Cursor::new(b"I ACCEPT\n"), true, &mut output).unwrap();
    assert_eq!(decision, DisclosureDecision::Prompt);
    assert!(String::from_utf8(output).unwrap().contains("Type I ACCEPT"));
}

#[test]
fn register_and_unregister_are_idempotent_and_preserve_unrelated_files() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    fs::create_dir_all(paths.copilot_home.join("hooks")).unwrap();
    fs::write(
        paths.copilot_home.join("hooks/other.json"),
        b"{\"version\":1}\n",
    )
    .unwrap();
    fs::write(
        paths.copilot_home.join("settings.json"),
        b"{\"theme\":\"dark\"}\n",
    )
    .unwrap();

    for _ in 0..2 {
        let output = register_command(&paths, fake("success.sh"), 2_000, true);
        assert_success(&output);
    }

    let hook_path = paths.copilot_home.join("hooks/munshi.json");
    let hook: Value = serde_json::from_slice(&fs::read(&hook_path).unwrap()).unwrap();
    let executable = Path::new(env!("CARGO_BIN_EXE_munshi"))
        .canonicalize()
        .unwrap();
    assert_eq!(hook["version"], 1);
    for (event, command) in [("agentStop", "agent-stop"), ("sessionEnd", "session-end")] {
        let entry = &hook["hooks"][event][0];
        assert_eq!(entry["type"], "command");
        assert_eq!(entry["exec"], executable.to_string_lossy().as_ref());
        assert_eq!(
            entry["args"],
            json!(["hook", command, "--state-dir", paths.state])
        );
        assert_eq!(entry["timeoutSec"], 5);
        assert!(entry.get("bash").is_none());
        assert!(entry.get("command").is_none());
    }
    let config: Value =
        serde_json::from_slice(&fs::read(paths.state.join("config.json")).unwrap()).unwrap();
    assert_eq!(config["remote_delivery"], false);
    assert_eq!(
        config["summarizer"]["executable"],
        fake("success.sh").to_string_lossy().as_ref()
    );
    assert_eq!(
        config["output_directory"],
        paths.output.to_string_lossy().as_ref()
    );
    assert!(paths.copilot_home.join("hooks/other.json").exists());
    assert!(paths.copilot_home.join("settings.json").exists());

    for _ in 0..2 {
        let output = unregister_command(&paths);
        assert_success(&output);
    }
    assert!(!hook_path.exists());
    assert!(!paths.state.join("config.json").exists());
    assert!(paths.copilot_home.join("hooks/other.json").exists());
    assert!(paths.copilot_home.join("settings.json").exists());
}

#[test]
fn registration_rejects_symlinked_or_malformed_owned_paths() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    fs::create_dir_all(&paths.copilot_home).unwrap();
    let elsewhere = directory.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, paths.copilot_home.join("hooks")).unwrap();
    let output = register_command(&paths, fake("success.sh"), 2_000, true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsafe symlink"));
    assert!(!elsewhere.join("munshi.json").exists());

    fs::remove_file(paths.copilot_home.join("hooks")).unwrap();
    fs::create_dir_all(paths.copilot_home.join("hooks")).unwrap();
    fs::write(paths.copilot_home.join("hooks/munshi.json"), b"{not-json").unwrap();
    let output = register_command(&paths, fake("success.sh"), 2_000, true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed"));
    let output = unregister_command(&paths);
    assert!(!output.status.success());
    assert!(paths.copilot_home.join("hooks/munshi.json").exists());

    fs::remove_file(paths.copilot_home.join("hooks/munshi.json")).unwrap();
    fs::set_permissions(
        paths.copilot_home.join("hooks"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let output = register_command(&paths, fake("success.sh"), 2_000, true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ownership"));
}

#[test]
fn hook_payload_errors_fail_open_without_echoing_private_content() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let private = "PRIVATE-CWD-AND-TRANSCRIPT";
    let payload = format!(
        "{{\"sessionId\":\"{SESSION_ID}\",\"timestamp\":1,\"cwd\":\"/{private}\",\"transcriptPath\":\"/{private}/events.jsonl\",\"stopReason\":\"end_turn\"}}\n{{}}"
    );
    let output = hook_command(&paths, "agent-stop", payload.as_bytes());
    assert_success(&output);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let failure = fs::read_to_string(paths.state.join("failures/last.json")).unwrap();
    assert!(failure.contains("payload-not-single-object"));
    assert!(!failure.contains(private));
    assert!(
        !paths
            .state
            .join(format!("pending/{SESSION_ID}.json"))
            .exists()
    );
}

#[test]
fn agent_stop_uses_an_atomic_minimal_metadata_handoff() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let transcript = fixture_events();
    let project = git_project(directory.path());
    let payload = agent_stop_payload(&project, &transcript);
    assert_success(&hook_command(
        &paths,
        "agent-stop",
        payload.to_string().as_bytes(),
    ));

    let pending_path = paths.state.join(format!("pending/{SESSION_ID}.json"));
    let pending: Value = serde_json::from_slice(&fs::read(pending_path).unwrap()).unwrap();
    assert_eq!(
        pending,
        json!({
            "version": 1,
            "session_id": SESSION_ID,
            "transcript_path": transcript,
            "origin_cwd": project,
        })
    );
    assert!(
        fs::read_dir(paths.state.join("pending"))
            .unwrap()
            .all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".munshi-")
            })
    );
}

#[test]
fn session_end_returns_quickly_and_reports_detached_failure_deterministically() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("timeout.sh"), 400, true));
    let project = git_project(directory.path());
    assert_success(&hook_command(
        &paths,
        "agent-stop",
        agent_stop_payload(&project, &fixture_events())
            .to_string()
            .as_bytes(),
    ));

    let started = Instant::now();
    let output = hook_command(
        &paths,
        "session-end",
        session_end_payload(&project).to_string().as_bytes(),
    );
    assert_success(&output);
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "sessionEnd blocked for {:?}",
        started.elapsed()
    );
    let waited = wait_command(&paths, 5_000);
    assert!(!waited.status.success());
    assert!(String::from_utf8_lossy(&waited.stdout).contains("\"status\":\"failed\""));
    let failure = fs::read_to_string(paths.state.join("failures/last.json")).unwrap();
    assert!(failure.contains("summary-failed"));
    assert!(!failure.contains(project.to_string_lossy().as_ref()));
}

#[test]
fn duplicate_clean_hooks_start_one_worker_and_full_lifecycle_matches_manual_archive() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    let project = git_project(directory.path());
    let count = directory.path().join("summary-count");
    let summarizer = directory.path().join("counting-summarizer.sh");
    fs::write(
        &summarizer,
        format!(
            "#!/bin/sh\ncat >/dev/null\nprintf x >> '{}'\nprintf '%s' '{}'\n",
            count.display(),
            r#"{"title":"Implement manual archival","goal":"Archive one synthetic Copilot session safely.","work_completed":["Added defensive transcript normalization.","Rendered one deterministic Markdown record."],"decisions":["Use stable source identity instead of the title."],"files_changed":["crates/munshi/src/archive.rs"],"commands_and_validation":["cargo test --workspace"],"open_items":["Add resumed revisions in issue #3."],"tags":["rust","copilot-cli"]}"#
        ),
    )
    .unwrap();
    fs::set_permissions(&summarizer, fs::Permissions::from_mode(0o755)).unwrap();
    assert_success(&register_command(&paths, summarizer, 2_000, true));
    assert_success(&hook_command(
        &paths,
        "agent-stop",
        agent_stop_payload(&project, &fixture_events())
            .to_string()
            .as_bytes(),
    ));
    let end = session_end_payload(&project).to_string();
    assert_success(&hook_command(&paths, "session-end", end.as_bytes()));
    assert_success(&hook_command(&paths, "session-end", end.as_bytes()));
    let waited = wait_command(&paths, 5_000);
    assert_success(&waited);
    assert_eq!(fs::read_to_string(count).unwrap(), "x");

    let archive = find_archive(&paths.output);
    assert_eq!(
        fs::read_to_string(archive).unwrap(),
        include_str!("../../../fixtures/manual/expected/normal.md")
    );
    assert!(
        !paths
            .state
            .join(format!("pending/{SESSION_ID}.json"))
            .exists()
    );

    assert_success(&hook_command(&paths, "session-end", end.as_bytes()));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::read_to_string(directory.path().join("summary-count")).unwrap(),
        "x"
    );
}

#[test]
fn cli_noninteractive_registration_refuses_without_acceptance() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    let output = register_command(&paths, fake("success.sh"), 2_000, false);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("summarization is ON by default"));
    assert!(stderr.contains("--accept-disclosure"));
    assert!(!paths.copilot_home.join("hooks/munshi.json").exists());
}

struct Paths {
    copilot_home: PathBuf,
    state: PathBuf,
    output: PathBuf,
}

impl Paths {
    fn new(directory: &TempDir) -> Self {
        Self {
            copilot_home: directory.path().join("copilot-home"),
            state: directory.path().join("state"),
            output: directory.path().join("archives"),
        }
    }
}

fn register_command(paths: &Paths, summarizer: PathBuf, timeout_ms: u64, accepted: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
    command
        .arg("register")
        .arg("--copilot-home")
        .arg(&paths.copilot_home)
        .arg("--state-dir")
        .arg(&paths.state)
        .arg("--output-dir")
        .arg(&paths.output)
        .arg("--summarizer")
        .arg(summarizer)
        .arg("--timeout-ms")
        .arg(timeout_ms.to_string())
        .stdin(Stdio::null());
    if accepted {
        command.arg("--accept-disclosure");
    }
    command.output().unwrap()
}

fn unregister_command(paths: &Paths) -> Output {
    Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("unregister")
        .arg("--copilot-home")
        .arg(&paths.copilot_home)
        .arg("--state-dir")
        .arg(&paths.state)
        .output()
        .unwrap()
}

fn hook_command(paths: &Paths, event: &str, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("hook")
        .arg(event)
        .arg("--state-dir")
        .arg(&paths.state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn wait_command(paths: &Paths, timeout_ms: u64) -> Output {
    Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("hook")
        .arg("wait")
        .arg("--state-dir")
        .arg(&paths.state)
        .arg("--session-id")
        .arg(SESSION_ID)
        .arg("--timeout-ms")
        .arg(timeout_ms.to_string())
        .output()
        .unwrap()
}

fn agent_stop_payload(project: &Path, transcript: &Path) -> Value {
    json!({
        "sessionId": SESSION_ID,
        "timestamp": 1783817107011_u64,
        "cwd": project,
        "transcriptPath": transcript,
        "stopReason": "end_turn",
    })
}

fn session_end_payload(project: &Path) -> Value {
    json!({
        "sessionId": SESSION_ID,
        "timestamp": 1783817107057_u64,
        "cwd": project,
        "reason": "complete",
    })
}

fn fixture_events() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/manual/copilot")
        .join(SESSION_ID)
        .join("events.jsonl")
        .canonicalize()
        .unwrap()
}

fn fake(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/manual/fake-summarizer")
        .join(name)
        .canonicalize()
        .unwrap()
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

fn find_archive(output: &Path) -> PathBuf {
    let project = fs::read_dir(output)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    project.join(format!("{SESSION_ID}.md"))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn test_directory() -> TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-test-artifacts");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("hook-case-")
        .tempdir_in(root)
        .unwrap()
}
