use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use munshi_probe::capture::{CaptureError, CaptureMode, capture_hook, capture_hook_in_directory};
use munshi_probe::inspect::inspect_transcript;
use munshi_probe::summary::{
    Phase0Summary, SummaryProbeConfig, SummaryProbeError, run_summary_probe,
};
use tempfile::TempDir;

#[test]
fn sanitized_capture_is_recursive_and_atomically_created() {
    let directory = test_directory();
    let output = directory.path().join("hook.json");
    let input = br#"{
        "event": "agentStop",
        "sessionId": "private-id",
        "items": ["private text", 7, false, null, {"kind": "tool"}]
    }"#;

    let report = capture_hook(
        input.as_slice(),
        &output,
        CaptureMode::Sanitized {
            replacement: "<removed>".to_owned(),
            preserved_values: BTreeSet::from(["agentStop".to_owned(), "tool".to_owned()]),
        },
    )
    .unwrap();

    assert!(report.sanitized);
    let captured: serde_json::Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(
        captured,
        serde_json::json!({
            "event": "agentStop",
            "sessionId": "<removed>",
            "items": ["<removed>", 7, false, null, {"kind": "tool"}]
        })
    );

    let error = capture_hook(
        br#"{"event":"different"}"#.as_slice(),
        &output,
        CaptureMode::Raw,
    )
    .unwrap_err();
    assert!(matches!(error, CaptureError::AlreadyExists(path) if path == output));
    assert_eq!(
        captured,
        serde_json::from_slice::<serde_json::Value>(&fs::read(&output).unwrap()).unwrap()
    );
}

#[test]
fn directory_capture_names_fixture_after_hook_event_and_never_collides() {
    let directory = test_directory();
    let input = br#"{"hook_event_name": "SessionEnd", "session_id": "private-id"}"#;

    let first = capture_hook_in_directory(
        input.as_slice(),
        directory.path(),
        CaptureMode::Sanitized {
            replacement: "<redacted>".to_owned(),
            preserved_values: BTreeSet::from(["SessionEnd".to_owned()]),
        },
    )
    .unwrap();
    let second =
        capture_hook_in_directory(input.as_slice(), directory.path(), CaptureMode::Raw).unwrap();

    assert_ne!(first.path, second.path);
    for report in [&first, &second] {
        let name = report.path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("SessionEnd-"), "unexpected name {name}");
        assert!(name.ends_with(".json"));
        assert!(report.path.exists());
    }
    let sanitized: serde_json::Value =
        serde_json::from_slice(&fs::read(&first.path).unwrap()).unwrap();
    assert_eq!(
        sanitized,
        serde_json::json!({"hook_event_name": "SessionEnd", "session_id": "<redacted>"})
    );
}

#[test]
fn directory_capture_degrades_missing_event_name_to_generic_stem() {
    let directory = test_directory();

    let report = capture_hook_in_directory(
        br#"{"session_id": "private-id"}"#.as_slice(),
        directory.path(),
        CaptureMode::Raw,
    )
    .unwrap();

    let name = report.path.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("hook-"), "unexpected name {name}");

    let hostile = capture_hook_in_directory(
        br#"{"hook_event_name": "../../escape"}"#.as_slice(),
        directory.path(),
        CaptureMode::Raw,
    )
    .unwrap();
    let hostile_name = hostile.path.file_name().unwrap().to_str().unwrap();
    assert!(
        hostile_name.starts_with("escape-"),
        "unexpected name {hostile_name}"
    );
    assert_eq!(hostile.path.parent().unwrap(), directory.path());
}

#[test]
fn sanitization_failure_never_creates_a_raw_fixture() {
    let directory = test_directory();
    let output = directory.path().join("hook.json");

    let error = capture_hook(
        b"not json".as_slice(),
        &output,
        CaptureMode::Sanitized {
            replacement: "<redacted>".to_owned(),
            preserved_values: BTreeSet::new(),
        },
    )
    .unwrap_err();

    assert!(matches!(error, CaptureError::InvalidJson(_)));
    assert!(!output.exists());
}

#[test]
fn transcript_inspection_reports_only_structural_metrics() {
    let directory = test_directory();
    let transcript = directory.path().join("events.jsonl");
    let contents = concat!(
        "{\"type\":\"user\",\"secret\":\"alpha\"}\n",
        "not-json\n",
        "{\"type\":\"tool\",\"secret\":\"beta\",\"ok\":true}\n",
        "[1,2,3]\n"
    );
    fs::write(&transcript, contents).unwrap();

    let report = inspect_transcript(&transcript, &BTreeSet::from(["type".to_owned()])).unwrap();

    assert_eq!(report.bytes, contents.len() as u64);
    assert_eq!(report.lines, 4);
    assert_eq!(report.json_valid_lines, 3);
    assert_eq!(
        report.top_level_key_frequency,
        BTreeMap::from([
            ("ok".to_owned(), 1),
            ("secret".to_owned(), 2),
            ("type".to_owned(), 2),
        ])
    );
    assert_eq!(
        report.discriminator_value_counts["type"],
        BTreeMap::from([("\"tool\"".to_owned(), 1), ("\"user\"".to_owned(), 1)])
    );
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(!serialized.contains("alpha"));
    assert!(!serialized.contains("beta"));
}

#[test]
fn committed_transcript_fixture_has_expected_envelope_and_discriminators() {
    let transcript = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/copilot-1.0.70/transcript/synthetic-envelope.jsonl");

    let report = inspect_transcript(&transcript, &BTreeSet::from(["type".to_owned()])).unwrap();

    assert_eq!(report.bytes, 1_210);
    assert_eq!(report.bytes, fs::metadata(&transcript).unwrap().len());
    assert_eq!(report.lines, 11);
    assert_eq!(report.json_valid_lines, 11);
    assert_eq!(
        report.top_level_key_frequency,
        BTreeMap::from([
            ("agentId".to_owned(), 1),
            ("data".to_owned(), 11),
            ("id".to_owned(), 11),
            ("parentId".to_owned(), 11),
            ("timestamp".to_owned(), 11),
            ("type".to_owned(), 11),
        ])
    );
    assert_eq!(
        report.discriminator_value_counts["type"],
        BTreeMap::from([
            ("\"assistant.message\"".to_owned(), 1),
            ("\"assistant.turn_end\"".to_owned(), 1),
            ("\"assistant.turn_start\"".to_owned(), 1),
            ("\"hook.end\"".to_owned(), 1),
            ("\"hook.start\"".to_owned(), 1),
            ("\"session.model_change\"".to_owned(), 1),
            ("\"session.resume\"".to_owned(), 1),
            ("\"session.shutdown\"".to_owned(), 1),
            ("\"session.start\"".to_owned(), 1),
            ("\"system.message\"".to_owned(), 1),
            ("\"user.message\"".to_owned(), 1),
        ])
    );
}

#[test]
fn fake_summary_executable_receives_stdin_and_returns_valid_json() {
    let directory = test_directory();
    let script = executable_script(
        directory.path(),
        "success.sh",
        r#"received=$(cat)
[ "$received" = "transcript" ] || exit 9
printf '{"title":"Compatibility probe","summary":"Validated JSON invocation."}'
"#,
    );

    let result = run_summary_probe(&config(script), b"transcript".to_vec()).unwrap();

    assert_eq!(
        result,
        Phase0Summary {
            title: "Compatibility probe".to_owned(),
            summary: "Validated JSON invocation.".to_owned(),
        }
    );
}

#[test]
fn malformed_summary_json_is_explicit() {
    let directory = test_directory();
    let script = executable_script(directory.path(), "malformed.sh", "printf 'not-json'\n");

    let error = run_summary_probe(&config(script), Vec::new()).unwrap_err();

    assert!(matches!(error, SummaryProbeError::MalformedJson(_)));
}

#[test]
fn invalid_summary_shape_is_explicit() {
    let directory = test_directory();
    let script = executable_script(
        directory.path(),
        "empty-summary.sh",
        r#"printf '{"title":"Probe","summary":"  "}'"#,
    );

    let error = run_summary_probe(&config(script), Vec::new()).unwrap_err();

    assert!(matches!(
        error,
        SummaryProbeError::InvalidShape {
            field: "summary",
            ..
        }
    ));
}

#[test]
fn summary_fields_are_trimmed_before_returned_limits_are_enforced() {
    let directory = test_directory();
    let padding = " ".repeat(500);
    let output = serde_json::json!({
        "title": format!("{padding}Probe{padding}"),
        "summary": format!("{padding}Concise result.{padding}"),
    });
    let script = executable_script(
        directory.path(),
        "padded-summary.sh",
        &format!("printf '%s' '{}'\n", output),
    );

    let result = run_summary_probe(&config(script), Vec::new()).unwrap();

    assert_eq!(result.title, "Probe");
    assert_eq!(result.summary, "Concise result.");
    assert!(result.title.chars().count() <= 200);
    assert!(result.summary.chars().count() <= 2_000);
}

#[test]
fn non_zero_exit_is_explicit_without_echoing_stderr() {
    let directory = test_directory();
    let script = executable_script(
        directory.path(),
        "failure.sh",
        "printf 'private failure detail' >&2\nexit 7\n",
    );

    let error = run_summary_probe(&config(script), Vec::new()).unwrap_err();

    assert!(matches!(
        error,
        SummaryProbeError::NonZeroExit { code: Some(7), .. }
    ));
    assert!(!error.to_string().contains("private failure detail"));
}

#[test]
fn stdout_and_stderr_are_bounded() {
    let directory = test_directory();
    for (name, body, expected_stream) in [
        (
            "large-stdout.sh",
            "printf '01234567890123456789'\n",
            "stdout",
        ),
        (
            "large-stderr.sh",
            "printf '01234567890123456789' >&2\nsleep 1\n",
            "stderr",
        ),
    ] {
        let script = executable_script(directory.path(), name, body);
        let mut probe_config = config(script);
        probe_config.stdout_limit = 8;
        probe_config.stderr_limit = 8;

        let error = run_summary_probe(&probe_config, Vec::new()).unwrap_err();

        assert!(matches!(
            error,
            SummaryProbeError::OutputLimit { stream, limit: 8 }
                if stream == expected_stream
        ));
    }
}

#[test]
fn timeout_kills_the_process_group() {
    let directory = test_directory();
    let marker = directory.path().join("descendant-survived");
    let script = executable_script(
        directory.path(),
        "timeout.sh",
        r#"(sleep 1; printf survived > "$1") &
sleep 5
"#,
    );
    let mut probe_config = config(script);
    probe_config.args = vec![OsString::from(marker.as_os_str())];
    probe_config.timeout = Duration::from_millis(100);
    let started = Instant::now();

    let error = run_summary_probe(&probe_config, Vec::new()).unwrap_err();

    assert!(matches!(error, SummaryProbeError::Timeout(_)));
    assert!(started.elapsed() < Duration::from_secs(2));
    thread::sleep(Duration::from_millis(1_100));
    assert!(!marker.exists());
}

#[test]
fn committed_live_fixtures_contain_only_allowlisted_sanitized_values() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/copilot-1.0.70");
    let mut files = Vec::new();
    collect_files(&root, &mut files);
    files.sort();

    let expected = [
        "interactive/agent-stop.json",
        "interactive/session-end.json",
        "interrupted/session-end.json",
        "noninteractive/agent-stop.json",
        "noninteractive/session-end.json",
        "resumed/agent-stop.json",
        "resumed/session-end.json",
        "transcript/synthetic-envelope.jsonl",
    ];
    let relative: Vec<_> = files
        .iter()
        .map(|path| {
            path.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    assert_eq!(relative, expected);

    for path in files {
        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("/Users/"));
        assert!(!contents.contains("/home/"));
        assert!(!contents.contains("surdy"));
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            for line in contents.lines() {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                assert_sanitized_strings(&value);
            }
        } else {
            let value: serde_json::Value = serde_json::from_str(&contents).unwrap();
            assert_sanitized_strings(&value);
        }
    }
}

#[test]
fn committed_claude_hook_fixtures_contain_only_allowlisted_sanitized_values() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/claude-code-2.1.205");
    let mut files = Vec::new();
    collect_files(&root, &mut files);
    files.sort();

    let expected = [
        "hooks/interrupted/session-end.json",
        "hooks/noninteractive/session-end.json",
        "hooks/noninteractive/stop.json",
        "hooks/resumed/session-end.json",
        "hooks/resumed/stop.json",
        "transcript/0c1a0de0-0000-4000-8000-000000000205.jsonl",
    ];
    let relative: Vec<_> = files
        .iter()
        .map(|path| {
            path.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    assert_eq!(relative, expected);

    for path in files {
        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("/Users/"));
        assert!(!contents.contains("/home/"));
        assert!(!contents.contains("surdy"));
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let value: serde_json::Value = serde_json::from_str(&contents).unwrap();
            assert_claude_hook_sanitized_strings(&value);
        }
    }
}

fn assert_claude_hook_sanitized_strings(value: &serde_json::Value) {
    match value {
        serde_json::Value::String(value) => assert!(
            matches!(
                value.as_str(),
                "<redacted>" | "Stop" | "SessionEnd" | "other" | "default"
            ),
            "unexpected fixture string value"
        ),
        serde_json::Value::Array(values) => {
            for value in values {
                assert_claude_hook_sanitized_strings(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                assert_claude_hook_sanitized_strings(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn config(binary: PathBuf) -> SummaryProbeConfig {
    SummaryProbeConfig {
        binary,
        args: Vec::new(),
        timeout: Duration::from_secs(2),
        stdout_limit: 4 * 1024,
        stderr_limit: 4 * 1024,
    }
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

fn assert_sanitized_strings(value: &serde_json::Value) {
    match value {
        serde_json::Value::String(value) => assert!(
            matches!(
                value.as_str(),
                "<redacted>"
                    | "<agent>"
                    | "<event-01>"
                    | "<event-02>"
                    | "<event-03>"
                    | "<event-04>"
                    | "<event-05>"
                    | "<event-06>"
                    | "<event-07>"
                    | "<event-08>"
                    | "<event-09>"
                    | "<event-10>"
                    | "<event-11>"
                    | "assistant.message"
                    | "assistant.turn_end"
                    | "assistant.turn_start"
                    | "complete"
                    | "end_turn"
                    | "hook.end"
                    | "hook.start"
                    | "session.model_change"
                    | "session.resume"
                    | "session.shutdown"
                    | "session.start"
                    | "system.message"
                    | "user.message"
                    | "user_exit"
            ),
            "unexpected fixture string value"
        ),
        serde_json::Value::Array(values) => {
            for value in values {
                assert_sanitized_strings(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                assert_sanitized_strings(value);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn executable_script(directory: &Path, name: &str, body: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn test_directory() -> TempDir {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-probe-test-artifacts");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(root)
        .unwrap()
}
