use std::fs::{self, OpenOptions};
use std::io::Cursor;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use munshi::{
    DisclosureDecision, SourceKind, accept_disclosure, parse_archive_markdown, read_last_failure,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";

#[test]
fn disclosure_requires_explicit_noninteractive_acceptance_and_prompt_is_testable() {
    let mut output = Vec::new();
    let error = accept_disclosure(false, &mut Cursor::new(b""), false, &mut output).unwrap_err();
    assert!(error.to_string().contains("--accept-transcript-processing"));
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("summarization is ON by default"));
    assert!(text.contains("NO secret redaction"));
    assert!(text.contains("sent again to the configured summarizer"));
    assert!(text.contains("local Markdown"));
    assert!(text.contains("Remote delivery remains DISABLED"));

    let mut output = Vec::new();
    let decision =
        accept_disclosure(false, &mut Cursor::new(b"I ACCEPT\n"), true, &mut output).unwrap();
    assert_eq!(decision, DisclosureDecision::Prompt);
    assert!(String::from_utf8(output).unwrap().contains("Type I ACCEPT"));
}

/// Issue #31: re-running `register` (e.g. to raise policy budgets) must not reset the
/// archive-upload and summary-delivery sections their own commands configured — most critically
/// the persistent Patwari client UUID, which uploads are durably keyed under. Since issue #36 both
/// carried sections are self-contained (`enabled` lives inside them), so the carry-forward is a
/// straight section copy with no separate top-level flag to remember.
#[test]
fn reregistration_preserves_archive_upload_and_summary_delivery_configuration() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));

    // Configure and enable archive upload and summary delivery through their own commands (never
    // by hand-editing config.json). Neither endpoint is contacted by these commands.
    let munshi = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_munshi"))
            .args(args)
            .arg("--state-dir")
            .arg(&paths.state)
            .output()
            .unwrap();
        assert_success(&output);
    };
    munshi(&[
        "archive-upload",
        "configure",
        "--endpoint",
        "http://127.0.0.1:9",
    ]);
    munshi(&["archive-upload", "enable"]);
    munshi(&[
        "summary-delivery",
        "configure",
        "--endpoint",
        "http://127.0.0.1:10",
        "--vault",
        "Vault",
        "--folder",
        "Munshi",
    ]);
    munshi(&["summary-delivery", "enable"]);

    let config_path = paths.state.join("config.json");
    let before: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    let client_id = before["archive_upload"]["client_id"]
        .as_str()
        .expect("configure minted a persistent client UUID")
        .to_owned();

    // Re-register with a different unrelated flag: registration-owned settings still update
    // normally while the carried-forward sections survive verbatim.
    assert_success(&register_command(&paths, fake("success.sh"), 7_000, true));

    let after: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(after["version"], 2);
    assert_eq!(after["limits"]["timeout_ms"], 7_000);
    assert_eq!(after["archive_upload"]["enabled"], true);
    assert_eq!(after["archive_upload"]["endpoint"], "http://127.0.0.1:9");
    assert_eq!(after["archive_upload"]["client_id"], client_id.as_str());
    assert_eq!(after["summary_delivery"]["enabled"], true);
    assert_eq!(after["summary_delivery"]["endpoint"], "http://127.0.0.1:10");
    assert_eq!(after["summary_delivery"]["vault"], "Vault");
    assert_eq!(after["summary_delivery"]["folder"], "Munshi");
    assert!(after.get("remote_delivery").is_none());
    assert!(after.get("delivery").is_none());
}

/// Issue #36: a version-1 configuration (top-level `remote_delivery` bool + `delivery` section)
/// loads losslessly, is persisted as the unified version-2 `summary_delivery` shape on first load,
/// and round-trips bit-for-bit thereafter.
#[test]
fn v1_config_migrates_losslessly_to_v2_and_round_trips() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let config_path = paths.state.join("config.json");

    // Downgrade the freshly written v2 config to the exact v1 on-disk shape. Sections added after
    // version 2 was current (`summarizer_exhaust`, issue #60; `memory_sync`, issue #59) are
    // dropped: no real v1 file has them, and the v1 shape rejects keys it does not know.
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config.as_object_mut().unwrap().remove("summarizer_exhaust");
    config.as_object_mut().unwrap().remove("memory_sync");
    let mut delivery = config
        .as_object_mut()
        .unwrap()
        .remove("summary_delivery")
        .unwrap();
    delivery.as_object_mut().unwrap().remove("enabled");
    delivery["endpoint"] = json!("http://127.0.0.1:11");
    delivery["vault"] = json!("MigratedVault");
    delivery["folder"] = json!("Munshi");
    delivery["credential"] = json!({"source": "env", "var": "MUNSHI_TOKEN"});
    delivery["max_attempts"] = json!(7);
    delivery["provision_history"] = json!(true);
    config["version"] = json!(1);
    config["remote_delivery"] = json!(true);
    config["delivery"] = delivery;
    let mut bytes = serde_json::to_vec_pretty(&config).unwrap();
    bytes.push(b'\n');
    fs::write(&config_path, bytes).unwrap();

    // Any command that loads the configuration migrates it; the settings it reports come from the
    // migrated view.
    let status = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["summary-delivery", "status", "--json"])
        .arg("--state-dir")
        .arg(&paths.state)
        .output()
        .unwrap();
    assert_success(&status);
    let report: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(report["settings"]["enabled"], true);
    assert_eq!(report["settings"]["endpoint"], "http://127.0.0.1:11");
    assert_eq!(report["settings"]["vault"], "MigratedVault");
    assert_eq!(report["settings"]["max_attempts"], 7);
    assert_eq!(report["settings"]["provision_history"], true);

    // The migration was persisted: version 2, one self-contained section, legacy keys gone, every
    // v1 value carried losslessly.
    let migrated: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(migrated["version"], 2);
    assert!(migrated.get("remote_delivery").is_none());
    assert!(migrated.get("delivery").is_none());
    let section = &migrated["summary_delivery"];
    assert_eq!(section["enabled"], true);
    assert_eq!(section["endpoint"], "http://127.0.0.1:11");
    assert_eq!(section["vault"], "MigratedVault");
    assert_eq!(section["folder"], "Munshi");
    assert_eq!(section["credential"]["source"], "env");
    assert_eq!(section["credential"]["var"], "MUNSHI_TOKEN");
    assert_eq!(section["max_attempts"], 7);
    assert_eq!(section["provision_history"], true);
    // A migrated configuration never gains retention it did not ask for (issue #60).
    assert_eq!(migrated["summarizer_exhaust"]["home"], Value::Null);
    assert_eq!(migrated["summarizer_exhaust"]["retention_days"], 0);
    // Nor memory sync (issue #59): migrated configurations stay opted out.
    assert_eq!(migrated["memory_sync"]["enabled"], false);
    assert_eq!(migrated["memory_sync"]["machine_label"], Value::Null);

    // A v2 config round-trips: a second load rewrites nothing.
    let bytes_before = fs::read(&config_path).unwrap();
    let inode_before = fs::metadata(&config_path).unwrap().ino();
    let again = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["summary-delivery", "status", "--json"])
        .arg("--state-dir")
        .arg(&paths.state)
        .output()
        .unwrap();
    assert_success(&again);
    assert_eq!(fs::read(&config_path).unwrap(), bytes_before);
    assert_eq!(fs::metadata(&config_path).unwrap().ino(), inode_before);

    // Re-registering over a v1 config also carries the section forward into the v2 shape.
    fs::write(&config_path, {
        let mut value = migrated.clone();
        value.as_object_mut().unwrap().remove("summarizer_exhaust");
        value.as_object_mut().unwrap().remove("memory_sync");
        let mut delivery = value
            .as_object_mut()
            .unwrap()
            .remove("summary_delivery")
            .unwrap();
        delivery.as_object_mut().unwrap().remove("enabled");
        value["version"] = json!(1);
        value["remote_delivery"] = json!(true);
        value["delivery"] = delivery;
        let mut bytes = serde_json::to_vec_pretty(&value).unwrap();
        bytes.push(b'\n');
        bytes
    })
    .unwrap();
    assert_success(&register_command(&paths, fake("success.sh"), 9_000, true));
    let reregistered: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(reregistered["version"], 2);
    assert_eq!(reregistered["limits"]["timeout_ms"], 9_000);
    assert_eq!(reregistered["summary_delivery"]["enabled"], true);
    assert_eq!(reregistered["summary_delivery"]["vault"], "MigratedVault");
    assert!(reregistered.get("remote_delivery").is_none());
}

/// Issue #36: the `delivery` subcommand remains a visible deprecated alias for `summary-delivery`,
/// with identical behavior.
#[test]
fn delivery_alias_still_drives_summary_delivery_commands() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));

    let output = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["delivery", "status", "--json"])
        .arg("--state-dir")
        .arg(&paths.state)
        .output()
        .unwrap();
    assert_success(&output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["command"], "summary-delivery-status");
    assert_eq!(report["settings"]["enabled"], false);

    let help = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("--help")
        .output()
        .unwrap();
    assert_success(&help);
    let text = String::from_utf8_lossy(&help.stdout).into_owned();
    assert!(
        text.contains("summary-delivery"),
        "help must list the new subcommand: {text}"
    );
    assert!(
        text.contains("[aliases: delivery]") || text.contains("alias: delivery"),
        "help must advertise the deprecated alias: {text}"
    );
}

/// Issue #85: `hook recover --rebuild-state` is the only bridge from a manually archived session
/// into upload state, so it must be reachable from `--help` rather than only from prose. The hook
/// ingestion points stay hidden — a person never types them.
#[test]
fn help_lists_hook_and_hook_help_lists_recover_while_hiding_the_ingestion_points() {
    let top = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("--help")
        .output()
        .unwrap();
    assert_success(&top);
    let text = String::from_utf8_lossy(&top.stdout).into_owned();
    assert!(
        text.lines()
            .any(|line| line.trim_start().starts_with("hook ")),
        "top-level help must list `hook`: {text}"
    );

    let hook = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["hook", "--help"])
        .output()
        .unwrap();
    assert_success(&hook);
    let text = String::from_utf8_lossy(&hook.stdout).into_owned();
    assert!(
        text.contains("recover"),
        "`hook --help` must list `recover`: {text}"
    );
    assert!(
        !text.contains("agent-stop") && !text.contains("session-end"),
        "`hook --help` must keep the hook ingestion points hidden: {text}"
    );

    // The bridge is documented in one place, and `archive --help` points at it (issue #85).
    let archive = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["archive", "--help"])
        .output()
        .unwrap();
    assert_success(&archive);
    let text = String::from_utf8_lossy(&archive.stdout).into_owned();
    assert!(
        text.contains("shipping-to-patwari.md") && text.contains("hook recover"),
        "`archive --help` must point at the manual-archive bridge: {text}"
    );
}

#[test]
fn registration_is_idempotent_preserves_files_and_guards_the_1_0_70_hook_schema() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    fs::create_dir_all(paths.copilot_home.join("hooks")).unwrap();
    fs::write(
        paths.copilot_home.join("hooks/other.json"),
        b"{\"version\":1}\n",
    )
    .unwrap();
    fs::write(paths.copilot_home.join("hooks/broken.json"), b"{broken").unwrap();
    fs::write(
        paths.copilot_home.join("settings.json"),
        b"{\"theme\":\"dark\"}\n",
    )
    .unwrap();

    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let hook_path = paths.copilot_home.join("hooks/munshi.json");
    let original_inode = fs::metadata(&hook_path).unwrap().ino();
    let config_path = paths.state.join("config.json");
    let original_config_inode = fs::metadata(&config_path).unwrap().ino();
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    assert_eq!(fs::metadata(&hook_path).unwrap().ino(), original_inode);
    assert_eq!(
        fs::metadata(&config_path).unwrap().ino(),
        original_config_inode
    );
    let hook: Value = serde_json::from_slice(&fs::read(&hook_path).unwrap()).unwrap();
    let executable = Path::new(env!("CARGO_BIN_EXE_munshi"))
        .canonicalize()
        .unwrap();
    assert_eq!(hook["version"], 1);
    for (event, command) in [("agentStop", "agent-stop"), ("sessionEnd", "session-end")] {
        let entry = &hook["hooks"][event][0];
        assert_eq!(entry["type"], "command");
        assert_eq!(entry["exec"], executable.to_string_lossy().as_ref());
        assert_eq!(entry["args"], json!(["hook", command]));
        assert_eq!(entry["timeoutSec"], 2);
        assert!(entry.get("bash").is_none());
        assert!(entry.get("command").is_none());
    }
    let config: Value =
        serde_json::from_slice(&fs::read(paths.state.join("config.json")).unwrap()).unwrap();
    assert_eq!(config["version"], 2);
    assert_eq!(config["summary_delivery"]["enabled"], false);
    assert_eq!(config["archive_git_history"], false);
    assert_eq!(config["local_archival_enabled"], true);
    assert_eq!(config["transcript_processing_accepted"], true);
    assert_eq!(config["project_origin"], "agent_stop_cwd");
    assert_eq!(
        config["summarizer"]["executable"],
        fake("success.sh").to_string_lossy().as_ref()
    );
    assert_eq!(
        config["output_directory"],
        paths.output.to_string_lossy().as_ref()
    );
    assert!(paths.copilot_home.join("hooks/other.json").exists());
    assert_eq!(
        fs::read(paths.copilot_home.join("hooks/broken.json")).unwrap(),
        b"{broken"
    );
    assert!(paths.copilot_home.join("settings.json").exists());

    for _ in 0..2 {
        let output = unregister_command(&paths);
        assert_success(&output);
    }
    let registration_lock = paths.state.join("locks/.munshi-registration.lock");
    assert!(registration_lock.is_file());
    assert_eq!(
        fs::metadata(&registration_lock).unwrap().mode() & 0o777,
        0o600
    );
    assert!(!hook_path.exists());
    assert!(!paths.state.join("config.json").exists());
    assert!(paths.copilot_home.join("hooks/other.json").exists());
    assert_eq!(
        fs::read(paths.copilot_home.join("hooks/broken.json")).unwrap(),
        b"{broken"
    );
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
    fs::create_dir_all(paths.state.join("locks")).unwrap();
    // The failed register above legitimately left a lock file behind; replace it with the
    // hostile symlink under test.
    let _ = fs::remove_file(paths.state.join("locks/.munshi-registration.lock"));
    let lock_target = elsewhere.join("lock-target");
    fs::write(&lock_target, b"lock-target-bytes").unwrap();
    symlink(
        &lock_target,
        paths.state.join("locks/.munshi-registration.lock"),
    )
    .unwrap();
    let output = register_command(&paths, fake("success.sh"), 2_000, true);
    assert!(!output.status.success());
    assert_eq!(fs::read(&lock_target).unwrap(), b"lock-target-bytes");
    fs::remove_file(paths.state.join("locks/.munshi-registration.lock")).unwrap();

    let unsafe_lock = paths.state.join("locks/.munshi-registration.lock");
    fs::write(&unsafe_lock, b"").unwrap();
    fs::set_permissions(&unsafe_lock, fs::Permissions::from_mode(0o666)).unwrap();
    let output = register_command(&paths, fake("success.sh"), 2_000, true);
    assert!(!output.status.success());
    fs::remove_file(&unsafe_lock).unwrap();

    let target = elsewhere.join("target.json");
    fs::write(&target, b"target-bytes").unwrap();
    symlink(&target, paths.copilot_home.join("hooks/munshi.json")).unwrap();
    let output = register_command(&paths, fake("success.sh"), 2_000, true);
    assert!(!output.status.success());
    assert_eq!(fs::read(&target).unwrap(), b"target-bytes");
    assert!(
        fs::symlink_metadata(paths.copilot_home.join("hooks/munshi.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_file(paths.copilot_home.join("hooks/munshi.json")).unwrap();

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
fn fresh_unregister_creates_no_hooks_or_lock() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    fs::create_dir_all(&paths.copilot_home).unwrap();
    let settings = paths.copilot_home.join("settings.json");
    fs::write(&settings, b"{\"unchanged\":true}\n").unwrap();

    assert_success(&unregister_command(&paths));

    assert!(!paths.copilot_home.join("hooks").exists());
    assert_eq!(fs::read(settings).unwrap(), b"{\"unchanged\":true}\n");
}

#[test]
fn absent_hooks_cleanup_removes_only_recognized_config_without_creating_hooks() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    fs::remove_file(paths.copilot_home.join("hooks/munshi.json")).unwrap();
    fs::remove_dir(paths.copilot_home.join("hooks")).unwrap();
    fs::set_permissions(&paths.copilot_home, fs::Permissions::from_mode(0o500)).unwrap();

    let output = unregister_command(&paths);
    fs::set_permissions(&paths.copilot_home, fs::Permissions::from_mode(0o700)).unwrap();
    assert_success(&output);
    assert!(!paths.state.join("config.json").exists());
    assert!(!paths.copilot_home.join("hooks").exists());
}

#[test]
fn stale_lock_is_reusable_and_existing_hook_unregister_honors_contention() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    fs::create_dir_all(paths.state.join("locks")).unwrap();
    let lock = paths.state.join("locks/.munshi-registration.lock");
    fs::write(&lock, b"").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();

    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    assert!(lock.is_file());
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let output = unregister_command(&paths);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("another Munshi registration operation is active")
    );
    assert!(paths.copilot_home.join("hooks/munshi.json").exists());
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
    drop(held);
    assert_success(&unregister_command(&paths));
    assert!(lock.is_file());
}

#[test]
fn active_registration_lock_reports_distinct_contention() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    fs::create_dir_all(paths.state.join("locks")).unwrap();
    let lock_path = paths.state.join("locks/.munshi-registration.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let output = register_command(&paths, fake("success.sh"), 2_000, true);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("another Munshi registration operation is active")
    );
    assert!(!paths.copilot_home.join("hooks/munshi.json").exists());
    assert!(!paths.state.join("config.json").exists());

    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    drop(lock);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
}

#[test]
fn malformed_config_blocks_unregister_without_partial_removal() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let config = paths.state.join("config.json");
    fs::write(&config, b"{not-json").unwrap();

    let output = unregister_command(&paths);
    assert!(!output.status.success());
    assert!(paths.copilot_home.join("hooks/munshi.json").exists());
    assert_eq!(fs::read(config).unwrap(), b"{not-json");
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
    let failure = read_last_failure(&paths.state).unwrap().unwrap();
    assert_eq!(failure.code, "payload-not-single-object");
    assert!(
        !paths
            .state
            .join(format!("pending/{SESSION_ID}.json"))
            .exists()
    );
}

/// Issue #49: Copilot 1.0.76 added `stop_hook_active` to the `agentStop` payload and the
/// previous closed-schema parse rejected every stop as `payload-invalid`, so sessions never
/// accumulated agent-stop evidence. The current payload shape must ingest cleanly.
#[test]
fn copilot_1_0_76_agent_stop_with_stop_hook_active_records_evidence() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let project = git_project(directory.path());
    let mut payload = agent_stop_payload(&project, &fixture_events());
    payload["stop_hook_active"] = Value::Bool(false);
    assert_success(&hook_command(
        &paths,
        "agent-stop",
        payload.to_string().as_bytes(),
    ));

    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let row: (String, bool, Option<i64>, Option<String>) = connection
        .query_row(
            "SELECT lifecycle_state,active,last_agent_stop_ms,transcript_path
             FROM sessions WHERE source_session_id=?1",
            [SESSION_ID],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.0, "observed");
    assert!(row.1, "the session must be live, not an inactive husk");
    assert_eq!(row.2, Some(1_783_817_107_011));
    assert_eq!(row.3.as_deref(), fixture_events().to_str());
    let invalid: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM diagnostics WHERE category='payload-invalid'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(invalid, 0);
}

/// Issue #82: Copilot fires `agentStop` once per subagent, passing the subagent's tool-call id
/// as `sessionId` alongside the *parent* session's `transcriptPath`. Ingesting those created a
/// session per subagent that could never archive — the read-time identity check rejected it on
/// every attempt, forever. The stop must be refused at ingest and leave no session behind.
#[test]
fn copilot_subagent_agent_stop_creates_no_session() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let project = git_project(directory.path());

    // The parent session's own stop is ingested normally.
    let parent = agent_stop_payload(&project, &fixture_events());
    assert_success(&hook_command(
        &paths,
        "agent-stop",
        parent.to_string().as_bytes(),
    ));

    // The subagent's stop carries a tool-call id and the same, parent transcript path.
    let mut subagent = agent_stop_payload(&project, &fixture_events());
    subagent["sessionId"] = Value::String("call_RTDYl8D6VfcBoav2PS2id192".to_string());
    let output = hook_command(&paths, "agent-stop", subagent.to_string().as_bytes());

    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let phantom: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE source_session_id LIKE 'call!_%' ESCAPE '!'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phantom, 0, "a subagent stop must not become a session");

    // The parent is untouched, and the refusal is visible rather than silent.
    let parent_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE source_session_id=?1",
            [SESSION_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(parent_rows, 1);
    let recorded: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM diagnostics WHERE category='session-id-mismatch'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1, "the declined stop must leave one diagnostic");
    // The hook still exits zero: declining a payload is Munshi's business, and a non-zero exit
    // would surface inside the user's Copilot session as a failing hook.
    assert_success(&output);
}

/// Issue #49: a payload the pinned struct cannot parse at all (here: the timestamp field
/// gone) must still record agent-stop evidence — with the timestamp taken from receipt
/// time — instead of degrading into an evidence-less husk only the sweep can reconstruct.
/// The drift stays visible as one `payload-schema-drift` diagnostic.
#[test]
fn unparseable_agent_stop_still_records_receipt_time_evidence() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let project = git_project(directory.path());
    let mut payload = agent_stop_payload(&project, &fixture_events());
    payload.as_object_mut().unwrap().remove("timestamp");
    payload["stop_hook_active"] = Value::Bool(false);
    let before_ms: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap();
    assert_success(&hook_command(
        &paths,
        "agent-stop",
        payload.to_string().as_bytes(),
    ));

    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let row: (String, bool, Option<i64>) = connection
        .query_row(
            "SELECT lifecycle_state,active,last_agent_stop_ms
             FROM sessions WHERE source_session_id=?1",
            [SESSION_ID],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row.0, "observed");
    assert!(row.1, "the session must be live, not an inactive husk");
    assert!(
        row.2.is_some_and(|timestamp| timestamp >= before_ms),
        "receipt-time evidence expected, got {:?}",
        row.2
    );
    let failure = read_last_failure(&paths.state).unwrap().unwrap();
    assert_eq!(failure.code, "payload-schema-drift");
    assert_eq!(failure.session_id.as_deref(), Some(SESSION_ID));
}

#[test]
fn unresolved_session_end_stays_pending_and_later_interruption_archives() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let project = git_project(directory.path());
    assert_success(&hook_command(
        &paths,
        "session-end",
        session_end_payload(&project).to_string().as_bytes(),
    ));
    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let pending: (String, Option<String>, String) = connection
        .query_row(
            "SELECT lifecycle_state,transcript_path,last_error_category
             FROM sessions WHERE source_session_id=?1",
            [SESSION_ID],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        pending,
        (
            "summary-pending".to_owned(),
            None,
            "transcript-unresolved".to_owned()
        )
    );
    drop(connection);

    assert_success(&hook_command(
        &paths,
        "agent-stop",
        agent_stop_payload(&project, &fixture_events())
            .to_string()
            .as_bytes(),
    ));
    let mut interrupted = session_end_payload(&project);
    interrupted["reason"] = json!("user_exit");
    assert_success(&hook_command(
        &paths,
        "session-end",
        interrupted.to_string().as_bytes(),
    ));
    assert_success(&wait_command(&paths, 5_000));
    let archived =
        parse_archive_markdown(&fs::read_to_string(find_archive(&paths.output)).unwrap()).unwrap();
    assert_eq!(archived.completion_reason, "interrupted");
}

#[test]
fn agent_stop_uses_an_atomic_minimal_metadata_handoff() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    assert_success(&register_command(&paths, fake("success.sh"), 2_000, true));
    let transcript = fixture_events();
    let project = git_project(directory.path());
    let payload = agent_stop_payload(&project, &transcript);
    let output = hook_command(&paths, "agent-stop", payload.to_string().as_bytes());
    assert_success(&output);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let later_project = directory.path().join("later-project");
    fs::create_dir_all(&later_project).unwrap();
    assert_success(&hook_command(
        &paths,
        "agent-stop",
        agent_stop_payload(&later_project, &transcript)
            .to_string()
            .as_bytes(),
    ));

    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let row: (String, String, i64, String, i64) = connection
        .query_row(
            "SELECT transcript_path,origin_cwd,last_agent_stop_ms,lifecycle_state,
                    current_summary_revision
             FROM sessions WHERE source_session_id=?1",
            [SESSION_ID],
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
    assert_eq!(PathBuf::from(row.0), transcript);
    assert_eq!(PathBuf::from(row.1), project);
    assert_eq!(row.2, 1_783_817_107_011);
    assert_eq!(row.3, "observed");
    assert_eq!(row.4, 0);
    assert!(!paths.state.join("sessions").exists());
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
    let failure = read_last_failure(&paths.state).unwrap().unwrap();
    assert_eq!(failure.code, "summary-failed");
    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let row: (String, i64, String) = connection
        .query_row(
            "SELECT lifecycle_state,current_summary_revision,last_error_category
             FROM sessions WHERE source_session_id=?1",
            [SESSION_ID],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("failed".to_owned(), 0, "summary-failed".to_owned()));
    for legacy in ["pending", "workers", "results", "failures"] {
        assert!(!paths.state.join(legacy).exists());
    }
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
            "#!/bin/sh\n[ \"$1\" = \"--configured\" ] || exit 12\ncat >/dev/null\nprintf x >> '{}'\nprintf '%s' '{}'\n",
            count.display(),
            r#"{"title":"Implement manual archival","goal":"Archive one synthetic Copilot session safely.","work_completed":["Added defensive transcript normalization.","Rendered one deterministic Markdown record."],"decisions":["Use stable source identity instead of the title."],"files_changed":["crates/munshi/src/archive.rs"],"commands_and_validation":["cargo test --workspace"],"open_items":["Add resumed revisions in issue #3."],"tags":["rust","copilot-cli"]}"#
        ),
    )
    .unwrap();
    fs::set_permissions(&summarizer, fs::Permissions::from_mode(0o755)).unwrap();
    assert_success(&register_command_args(
        &paths,
        summarizer,
        2_000,
        true,
        &["--configured"],
    ));
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
    let markdown = fs::read_to_string(archive).unwrap();
    let parsed = parse_archive_markdown(&markdown).unwrap();
    assert_eq!(parsed.schema_version, 2);
    assert_eq!(parsed.summary_revision, 1);
    assert_eq!(parsed.session_id, SESSION_ID);
    assert_eq!(parsed.summary.title, "Implement manual archival");
    assert_eq!(parsed.completion_reason, "complete");
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
    assert!(stderr.contains("--accept-transcript-processing"));
    assert!(!paths.copilot_home.join("hooks/munshi.json").exists());
}

#[test]
fn dry_run_writes_nothing_and_direct_exec_preserves_spaces() {
    let directory = test_directory();
    let paths = Paths::new(&directory);
    let output = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("register")
        .arg("--dry-run")
        .arg("--accept-transcript-processing")
        .arg("--copilot-home")
        .arg(&paths.copilot_home)
        .arg("--state-dir")
        .arg(&paths.state)
        .arg("--output-dir")
        .arg(&paths.output)
        .arg("--summarizer")
        .arg(fake("success.sh"))
        .output()
        .unwrap();
    assert_success(&output);
    assert!(!paths.copilot_home.exists());

    let binary_directory = directory.path().join("bin with spaces");
    fs::create_dir_all(&binary_directory).unwrap();
    let copied_binary = binary_directory.join("munshi executable");
    fs::copy(env!("CARGO_BIN_EXE_munshi"), &copied_binary).unwrap();
    fs::set_permissions(&copied_binary, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(&copied_binary)
        .arg("register")
        .arg("--accept-transcript-processing")
        .arg("--copilot-home")
        .arg(&paths.copilot_home)
        .arg("--state-dir")
        .arg(&paths.state)
        .arg("--output-dir")
        .arg(&paths.output)
        .arg("--summarizer")
        .arg(fake("success.sh"))
        .output()
        .unwrap();
    assert_success(&output);
    let hook: Value =
        serde_json::from_slice(&fs::read(paths.copilot_home.join("hooks/munshi.json")).unwrap())
            .unwrap();
    assert_eq!(
        hook["hooks"]["agentStop"][0]["exec"],
        copied_binary
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
}

// ---------------------------------------------------------------------------
// Claude Code registration and hook ingestion
// ---------------------------------------------------------------------------

const CLAUDE_SESSION_ID: &str = "0c1a0de0-0000-4000-8000-000000000001";

fn claude_paths(directory: &TempDir) -> (Paths, PathBuf) {
    let paths = Paths::new(directory);
    (paths, directory.path().join("claude-home"))
}

fn claude_register_command(paths: &Paths, claude_home: &Path, summarizer: PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("register")
        .arg("--accept-transcript-processing")
        .arg("--harness")
        .arg("claude-code")
        .arg("--claude-home")
        .arg(claude_home)
        .arg("--state-dir")
        .arg(&paths.state)
        .arg("--output-dir")
        .arg(&paths.output)
        .arg("--summarizer")
        .arg(summarizer)
        .arg("--timeout-ms")
        .arg("10000")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn claude_hook_command(paths: &Paths, event: &str, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("hook")
        .arg(event)
        .arg("--source")
        .arg("claude-code")
        .env("MUNSHI_HOME", &paths.state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

/// A realistic 2.1.205 `Stop` payload including the undocumented extra fields the phase-0 probe
/// observed, proving tolerant parsing.
fn claude_stop_payload(project: &Path, transcript: &Path) -> Value {
    json!({
        "session_id": CLAUDE_SESSION_ID,
        "transcript_path": transcript,
        "cwd": project,
        "prompt_id": "840061a3-a65c-40ba-bb39-44fa3524b129",
        "permission_mode": "default",
        "hook_event_name": "Stop",
        "stop_hook_active": false,
        "last_assistant_message": "Done. Added hello().",
        "background_tasks": [],
        "session_crons": [],
    })
}

fn claude_session_end_payload(project: &Path, transcript: &Path, reason: &str) -> Value {
    json!({
        "session_id": CLAUDE_SESSION_ID,
        "transcript_path": transcript,
        "cwd": project,
        "prompt_id": "840061a3-a65c-40ba-bb39-44fa3524b129",
        "hook_event_name": "SessionEnd",
        "reason": reason,
    })
}

fn claude_fixture_transcript() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/claude-code-2.1.44/normal")
        .join(format!("{CLAUDE_SESSION_ID}.jsonl"))
        .canonicalize()
        .unwrap()
}

#[test]
fn claude_registration_merges_settings_and_unregister_removes_only_ours() {
    let directory = test_directory();
    let (paths, claude_home) = claude_paths(&directory);
    fs::create_dir_all(&claude_home).unwrap();
    let settings_path = claude_home.join("settings.json");
    let foreign_stop = json!({
        "hooks": [{"type": "command", "command": "/usr/local/bin/other-tool --notify"}]
    });
    fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&json!({
            "theme": "dark",
            "model": "opus",
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "/usr/bin/audit"}]}],
                "Stop": [foreign_stop],
            },
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(&settings_path, fs::Permissions::from_mode(0o644)).unwrap();

    assert_success(&claude_register_command(
        &paths,
        &claude_home,
        fake("success.sh"),
    ));

    let text = fs::read_to_string(&settings_path).unwrap();
    let settings: Value = serde_json::from_str(&text).unwrap();
    // User keys and their order survive the merge.
    assert_eq!(settings["theme"], "dark");
    assert_eq!(settings["model"], "opus");
    assert!(text.find("\"theme\"").unwrap() < text.find("\"model\"").unwrap());
    assert!(text.find("\"model\"").unwrap() < text.find("\"hooks\"").unwrap());
    // Foreign hooks are untouched; Munshi appended one entry per managed event.
    assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "Bash");
    let stop = settings["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2);
    assert_eq!(stop[0], foreign_stop);
    let installed = stop[1]["hooks"][0]["command"].as_str().unwrap();
    assert!(installed.ends_with(" hook agent-stop --source claude-code"));
    let session_end = settings["hooks"]["SessionEnd"].as_array().unwrap();
    assert_eq!(session_end.len(), 1);
    assert_eq!(session_end[0]["hooks"][0]["timeout"], 2);
    // Mode preserved; no Copilot installation was created.
    assert_eq!(fs::metadata(&settings_path).unwrap().mode() & 0o777, 0o644);
    assert!(!paths.copilot_home.exists());
    let config: Value =
        serde_json::from_slice(&fs::read(paths.state.join("config.json")).unwrap()).unwrap();
    assert_eq!(
        config["harnesses"]["claude_home"],
        claude_home.to_string_lossy().as_ref()
    );
    assert_eq!(config["harnesses"]["copilot_home"], Value::Null);

    // Re-registration is idempotent: managed entries are replaced, not duplicated.
    assert_success(&claude_register_command(
        &paths,
        &claude_home,
        fake("success.sh"),
    ));
    let settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
    assert_eq!(settings["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);

    // Unregister removes only Munshi's entries and keeps the file.
    assert_success(&unregister_command(&paths));
    let settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(settings["theme"], "dark");
    assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "Bash");
    assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert_eq!(settings["hooks"]["Stop"][0], foreign_stop);
    assert!(settings["hooks"].get("SessionEnd").is_none());
    assert!(!paths.state.join("config.json").exists());
}

#[test]
fn claude_registration_creates_minimal_settings_and_prunes_on_unregister() {
    let directory = test_directory();
    let (paths, claude_home) = claude_paths(&directory);

    assert_success(&claude_register_command(
        &paths,
        &claude_home,
        fake("success.sh"),
    ));
    let settings_path = claude_home.join("settings.json");
    let settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert_eq!(settings["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);

    assert_success(&unregister_command(&paths));
    // Only Munshi entries existed, so the hooks object is pruned entirely; the file remains.
    let settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(settings, json!({}));
}

#[test]
fn claude_registration_refuses_a_foreign_non_object_settings_file() {
    let directory = test_directory();
    let (paths, claude_home) = claude_paths(&directory);
    fs::create_dir_all(&claude_home).unwrap();
    fs::write(claude_home.join("settings.json"), b"[1,2,3]\n").unwrap();

    let output = claude_register_command(&paths, &claude_home, fake("success.sh"));
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not a JSON settings object"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(claude_home.join("settings.json")).unwrap(),
        b"[1,2,3]\n"
    );
    assert!(!paths.state.join("config.json").exists());
}

#[test]
fn claude_hooks_drive_full_lifecycle_to_archive() {
    let directory = test_directory();
    let (paths, claude_home) = claude_paths(&directory);
    let project = git_project(directory.path());
    assert_success(&claude_register_command(
        &paths,
        &claude_home,
        fake("success.sh"),
    ));
    let transcript = claude_fixture_transcript();

    // Stop fires once per assistant turn; a duplicate must not break ingestion.
    for _ in 0..2 {
        let output = claude_hook_command(
            &paths,
            "agent-stop",
            claude_stop_payload(&project, &transcript)
                .to_string()
                .as_bytes(),
        );
        assert_success(&output);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
    assert_success(&claude_hook_command(
        &paths,
        "session-end",
        claude_session_end_payload(&project, &transcript, "other")
            .to_string()
            .as_bytes(),
    ));

    let waited = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("hook")
        .arg("wait")
        .arg("--state-dir")
        .arg(&paths.state)
        .arg("--source")
        .arg("claude-code")
        .arg("--session-id")
        .arg(CLAUDE_SESSION_ID)
        .arg("--timeout-ms")
        .arg("10000")
        .output()
        .unwrap();
    assert_success(&waited);

    let relative = serde_json::from_slice::<Value>(&waited.stdout).unwrap()["relative_path"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        relative.contains("/claude-code/"),
        "unexpected archive path {relative}"
    );
    assert!(relative.ends_with(&format!("{CLAUDE_SESSION_ID}.md")));
    let archived =
        parse_archive_markdown(&fs::read_to_string(paths.output.join(&relative)).unwrap()).unwrap();
    assert_eq!(archived.source, SourceKind::ClaudeCode);
    // Phase-0 finding: reason "other" is ambiguous, so completion degrades to unknown while the
    // session is still archived.
    assert_eq!(archived.completion_reason, "unknown");

    // The Copilot scope must not see the Claude session.
    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let copilot_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE source_session_id=?1 AND source_kind='copilot'",
            [CLAUDE_SESSION_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(copilot_rows, 0);
}

#[test]
fn claude_hook_rejects_misrouted_and_malformed_payloads_without_state_rows() {
    let directory = test_directory();
    let (paths, claude_home) = claude_paths(&directory);
    let project = git_project(directory.path());
    assert_success(&claude_register_command(
        &paths,
        &claude_home,
        fake("success.sh"),
    ));
    let transcript = claude_fixture_transcript();

    // A SessionEnd payload delivered to the agent-stop hook is rejected by hook_event_name.
    let output = claude_hook_command(
        &paths,
        "agent-stop",
        claude_session_end_payload(&project, &transcript, "other")
            .to_string()
            .as_bytes(),
    );
    assert_success(&output);
    let failure = read_last_failure(&paths.state).unwrap().unwrap();
    assert!(
        failure.code == "unexpected-hook-event" || failure.code == "payload-invalid-json",
        "unexpected failure code {}",
        failure.code
    );
    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn claude_recovery_sweep_archives_hookless_sessions_and_skips_non_sessions() {
    let directory = test_directory();
    let (paths, claude_home) = claude_paths(&directory);
    let project = git_project(directory.path());
    assert_success(&claude_register_command(
        &paths,
        &claude_home,
        fake("success.sh"),
    ));

    // A force-killed session leaves only its transcript behind — no hooks ever fired.
    let project_dir = claude_home.join("projects/-work-demo");
    fs::create_dir_all(&project_dir).unwrap();
    let fixture = fs::read_to_string(claude_fixture_transcript()).unwrap();
    let transcript = project_dir.join(format!("{CLAUDE_SESSION_ID}.jsonl"));
    // Point the transcript's origin cwd at a real project so identity resolution succeeds.
    fs::write(
        &transcript,
        fixture.replace("/work/demo", project.to_str().unwrap()),
    )
    .unwrap();
    // Non-session entries the sweep must ignore: a sibling `<uuid>/` directory, a `memory/`
    // directory, and a foreign-envelope `.jsonl` with a plausible session-id stem.
    fs::create_dir_all(project_dir.join(CLAUDE_SESSION_ID)).unwrap();
    fs::create_dir_all(project_dir.join("memory")).unwrap();
    let foreign_id = "0c1a0de0-0000-4000-8000-00000000fefe";
    fs::write(
        project_dir.join(format!("{foreign_id}.jsonl")),
        b"{\"type\":\"session_meta\",\"payload\":{}}\n",
    )
    .unwrap();

    let recover = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("hook")
        .arg("recover")
        .arg("--state-dir")
        .arg(&paths.state)
        .arg("--stale-after-ms")
        .arg("0")
        .output()
        .unwrap();
    assert_success(&recover);

    let waited = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("hook")
        .arg("wait")
        .arg("--state-dir")
        .arg(&paths.state)
        .arg("--source")
        .arg("claude-code")
        .arg("--session-id")
        .arg(CLAUDE_SESSION_ID)
        .arg("--timeout-ms")
        .arg("15000")
        .output()
        .unwrap();
    assert_success(&waited);
    let relative = serde_json::from_slice::<Value>(&waited.stdout).unwrap()["relative_path"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(relative.contains("/claude-code/"));
    let archived =
        parse_archive_markdown(&fs::read_to_string(paths.output.join(&relative)).unwrap()).unwrap();
    assert_eq!(archived.source, SourceKind::ClaudeCode);
    // Swept sessions carry the interrupted (unknown end) completion reason.
    assert_eq!(archived.completion_reason, "unknown");

    // Only the real session entered the store: the sibling directory and the foreign-envelope
    // file were skipped, and nothing landed in the Copilot scope.
    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let rows: Vec<(String, String)> = connection
        .prepare("SELECT source_kind, source_session_id FROM sessions")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        vec![("claude-code".to_owned(), CLAUDE_SESSION_ID.to_owned())]
    );
}

#[test]
fn claude_recovery_sweep_leaves_fresh_transcripts_alone() {
    let directory = test_directory();
    let (paths, claude_home) = claude_paths(&directory);
    assert_success(&claude_register_command(
        &paths,
        &claude_home,
        fake("success.sh"),
    ));
    let project_dir = claude_home.join("projects/-work-demo");
    fs::create_dir_all(&project_dir).unwrap();
    // fs::write (not fs::copy, which preserves the fixture's old mtime on macOS) so the
    // transcript's modification time is genuinely fresh.
    fs::write(
        project_dir.join(format!("{CLAUDE_SESSION_ID}.jsonl")),
        fs::read(claude_fixture_transcript()).unwrap(),
    )
    .unwrap();

    // A generous staleness window means the just-written transcript is not yet stale.
    let recover = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .arg("hook")
        .arg("recover")
        .arg("--state-dir")
        .arg(&paths.state)
        .arg("--stale-after-ms")
        .arg("3600000")
        .output()
        .unwrap();
    assert_success(&recover);

    let connection = Connection::open(paths.state.join("munshi.db")).unwrap();
    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
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
            state: directory.path().join("munshi-home"),
            output: directory.path().join("archives"),
        }
    }
}

fn register_command(paths: &Paths, summarizer: PathBuf, timeout_ms: u64, accepted: bool) -> Output {
    register_command_args(paths, summarizer, timeout_ms, accepted, &[])
}

fn register_command_args(
    paths: &Paths,
    summarizer: PathBuf,
    timeout_ms: u64,
    accepted: bool,
    summarizer_args: &[&str],
) -> Output {
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
    for argument in summarizer_args {
        command.arg(format!("--summarizer-arg={argument}"));
    }
    if accepted {
        command.arg("--accept-transcript-processing");
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
        .env("MUNSHI_HOME", &paths.state)
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
