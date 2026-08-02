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
        // Additive on schema_version 1 (issue #56): a dashboard reads project and row
        // timestamps from this contract instead of opening munshi.db.
        let created = item["created_at_ms"].as_i64().expect("created_at_ms");
        let updated = item["updated_at_ms"].as_i64().expect("updated_at_ms");
        assert!(created > 0, "created_at_ms must be a real instant");
        assert!(
            updated >= created,
            "updated_at_ms {updated} predates created_at_ms {created}"
        );
    }

    let archived_item = items
        .iter()
        .find(|item| item["session_id"] == archived)
        .expect("archived session listed");
    assert_eq!(
        archived_item["project"], "munshi",
        "a resolved project identity must surface as its project name"
    );
    let summary_pending_item = items
        .iter()
        .find(|item| item["session_id"] == summary_pending)
        .expect("summary-pending session listed");
    assert_eq!(
        summary_pending_item["project"], "project",
        "a session whose project never resolved falls back to its origin basename"
    );

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

/// A caller that only knows a project directory (Madari, or the dashboard, probing before the
/// user has ever run `munshi register` there) must get a valid, empty `schema_version: 1`
/// contract from every read-only command — including the two added for issue #56 — and the probe
/// must not create the state database it is reporting on.
#[test]
fn attempts_and_diagnostics_json_on_an_unregistered_state_directory_degrade_to_empty_contracts() {
    let harness = Harness::new();

    let attempts = harness.attempts_json(None, None);
    assert_eq!(attempts["schema_version"], 1);
    assert_eq!(attempts["command"], "attempts");
    assert_eq!(attempts["since_ms"], Value::Null);
    assert_eq!(attempts["total"], 0);
    assert_eq!(attempts["returned"], 0);
    assert_eq!(attempts["items"], json!([]));

    let diagnostics = harness.diagnostics_json(None);
    assert_eq!(diagnostics["schema_version"], 1);
    assert_eq!(diagnostics["command"], "diagnostics");
    assert_eq!(diagnostics["total"], 0);
    assert_eq!(diagnostics["returned"], 0);
    assert_eq!(diagnostics["items"], json!([]));

    assert!(
        !harness.state.join("munshi.db").exists(),
        "a read-only probe must not create the state database"
    );
}

/// The `attempts` contract exposes one row per processing attempt — outcome, error category, and
/// timing joined to session identity — so a dashboard can bin outcomes and list recent failures
/// without opening the store (ADR 0007). Rows only: no rollup, no histogram.
#[test]
fn attempts_json_exposes_attempt_rows_ordered_by_finish_time() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    let archived = "aa000000-0000-4000-8000-000000000001";
    let failed = "aa000000-0000-4000-8000-000000000002";

    let archived_events = harness.write_transcript(archived, "ARCHIVE_REQUEST", "archive");
    harness.complete_lifecycle(archived, &archived_events, 50_000, 50_001);
    assert_success(&harness.wait(archived, 5_000));

    let failed_events = harness.write_transcript(failed, "FAIL_REQUEST", "fails");
    harness.complete_lifecycle(failed, &failed_events, 51_000, 51_001);
    assert!(!harness.wait(failed, 5_000).status.success());

    let report = harness.attempts_json(None, None);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["command"], "attempts");
    assert_eq!(report["since_ms"], Value::Null);
    let items = report["items"].as_array().unwrap();
    assert_eq!(report["total"], items.len());
    assert_eq!(report["returned"], items.len());
    assert!(items.len() >= 2, "both lifecycles recorded an attempt");

    for item in items {
        assert_eq!(
            item.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "source",
                "session_id",
                "project",
                "outcome",
                "error_category",
                "started_at_ms",
                "finished_at_ms",
            ]
            .iter()
            .collect::<Vec<_>>(),
            "the attempt row must stay exactly the documented field set"
        );
        assert_eq!(item["source"], "copilot");
        assert!(
            item["project"].is_string(),
            "every attempt inherits its session's project label"
        );
        assert!(item["started_at_ms"].as_i64().unwrap() > 0);
    }

    // Finished attempts come first, newest first; unfinished attempts sort last.
    let finished = items
        .iter()
        .map(|item| item["finished_at_ms"].as_i64())
        .collect::<Vec<_>>();
    assert!(
        finished.windows(2).all(|pair| match (pair[0], pair[1]) {
            (Some(newer), Some(older)) => newer >= older,
            (Some(_), None) => true,
            (None, None) => true,
            (None, Some(_)) => false,
        }),
        "attempts must be ordered by finish time with unfinished ones last: {finished:?}"
    );

    let succeeded = items
        .iter()
        .find(|item| item["session_id"] == archived)
        .expect("the archived session recorded an attempt");
    assert_eq!(succeeded["outcome"], "succeeded");
    assert_eq!(succeeded["error_category"], Value::Null);
    assert_eq!(
        succeeded["project"], "munshi",
        "an archived session has a resolved project identity to inherit"
    );

    let failure = items
        .iter()
        .find(|item| item["session_id"] == failed)
        .expect("the failed session recorded an attempt");
    assert_eq!(failure["outcome"], "failed");
    assert!(
        failure["error_category"].is_string(),
        "a failed attempt must name its error category"
    );
    assert!(failure["finished_at_ms"].as_i64().unwrap() > 0);
    assert_eq!(
        failure["project"], "project",
        "a session that never archived has only its origin basename to label with"
    );

    // `--limit` truncates the page but never the total, so a caller can tell it is paging.
    let limited = harness.attempts_json(Some(1), None);
    assert_eq!(limited["returned"], 1);
    assert_eq!(limited["items"].as_array().unwrap().len(), 1);
    assert_eq!(limited["total"], report["total"]);

    // `--since-ms` bounds the window on both the page and the total.
    let future = harness.attempts_json(None, Some(i64::MAX / 2));
    assert_eq!(future["since_ms"], i64::MAX / 2);
    assert_eq!(future["total"], 0);
    assert_eq!(future["items"], json!([]));
}

/// The `diagnostics` contract exposes the tail of the same operator-facing records `status`
/// already surfaces one of as `last_failure`: bounded Munshi-authored codes and the session they
/// named, newest first, and nothing that could carry transcript content.
#[test]
fn diagnostics_json_exposes_the_recorded_tail_newest_first() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    let first = "bb000000-0000-4000-8000-000000000001";
    let second = "bb000000-0000-4000-8000-000000000002";

    assert_success(&harness.project_disable());
    for (session_id, stop) in [(first, 60_000), (second, 61_000)] {
        let events = harness.write_transcript(session_id, "DISABLED_REQUEST", "blocked");
        harness.complete_lifecycle(session_id, &events, stop, stop + 1);
        assert!(!harness.wait(session_id, 5_000).status.success());
    }

    let report = harness.diagnostics_json(None);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["command"], "diagnostics");
    let items = report["items"].as_array().unwrap();
    assert_eq!(report["total"], items.len());
    assert_eq!(report["returned"], items.len());
    assert!(items.len() >= 2, "both disabled lifecycles were recorded");

    for item in items {
        assert_eq!(
            item.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "source",
                "session_id",
                "operation",
                "category",
                "cause_category",
                "recorded_at_ms",
            ]
            .iter()
            .collect::<Vec<_>>(),
            "the diagnostic row must stay exactly the documented field set"
        );
        assert!(item["operation"].is_string());
        assert!(item["category"].is_string());
        assert!(item["recorded_at_ms"].as_i64().unwrap() > 0);
    }

    let recorded = items
        .iter()
        .map(|item| item["recorded_at_ms"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert!(
        recorded.windows(2).all(|pair| pair[0] >= pair[1]),
        "diagnostics must be newest first: {recorded:?}"
    );

    let disabled = items
        .iter()
        .find(|item| item["session_id"] == second)
        .expect("the disabled session was named by a diagnostic");
    assert_eq!(disabled["source"], "copilot");
    assert_eq!(disabled["category"], "project-disabled");

    let limited = harness.diagnostics_json(Some(1));
    assert_eq!(limited["returned"], 1);
    assert_eq!(limited["items"].as_array().unwrap().len(), 1);
    assert_eq!(limited["total"], report["total"]);
    assert_eq!(limited["items"][0], items[0]);
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
        // A pre-#57 lumped park whose transcript still exists on disk classifies as the
        // size-cap case and is reported under its true name.
        message.contains("1 source-oversized (raise --max-source-bytes)"),
        "{message}"
    );
    assert!(
        message.contains("1 summary-input-limit (raise --chunk-threshold-bytes)"),
        "{message}"
    );
    assert_eq!(report["sessions"]["parked"], 2);
}

#[test]
fn phantom_parks_drain_to_not_archive_worthy_via_forced_retry() {
    let harness = Harness::new();
    assert_success(&harness.register(fake("status-contract.sh"), 2_000));

    // A failed rev-0 session with no cursor is the phantom shape (issue #58) once its
    // transcript is gone: a non-interactive `claude` invocation that fired hooks without
    // ever writing one.
    let phantom = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let events = harness.write_transcript(phantom, "FAIL_REQUEST", "fails");
    harness.complete_lifecycle(phantom, &events, 50_000, 50_001);
    assert!(!harness.wait(phantom, 5_000).status.success());
    harness.park_on_size_cap(phantom, "source-missing");
    fs::remove_file(&events).unwrap();

    // Doctor calls it what it is: a phantom, not a size cap, not a genuine loss.
    let report = harness.doctor_json();
    let codes: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"phantom-invocations-parked"), "{codes:?}");
    assert!(!codes.contains(&"size-cap-parked"), "{codes:?}");
    assert!(!codes.contains(&"transcript-missing-parked"), "{codes:?}");

    // The documented drain: a forced retry settles the phantom not-archive-worthy.
    let retry = Command::new(env!("CARGO_BIN_EXE_munshi"))
        .args(["retry-all", "--force", "--json", "--state-dir"])
        .arg(&harness.state)
        .output()
        .unwrap();
    assert_success(&retry);
    let report: Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(report["not_archive_worthy"], 1, "{report}");

    let sessions = harness.sessions_json(Some("not-archive-worthy"));
    let ids: Vec<&str> = sessions["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["session_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&phantom), "{ids:?}");
    assert_eq!(harness.status_json()["sessions"]["parked"], 0);
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

    fn attempts_json(&self, limit: Option<usize>, since_ms: Option<i64>) -> Value {
        let mut args = vec![
            "attempts".to_owned(),
            "--state-dir".to_owned(),
            self.state.display().to_string(),
            "--json".to_owned(),
        ];
        if let Some(limit) = limit {
            args.push("--limit".to_owned());
            args.push(limit.to_string());
        }
        if let Some(since_ms) = since_ms {
            args.push("--since-ms".to_owned());
            args.push(since_ms.to_string());
        }
        self.json_output(args)
    }

    fn diagnostics_json(&self, limit: Option<usize>) -> Value {
        let mut args = vec![
            "diagnostics".to_owned(),
            "--state-dir".to_owned(),
            self.state.display().to_string(),
            "--json".to_owned(),
        ];
        if let Some(limit) = limit {
            args.push("--limit".to_owned());
            args.push(limit.to_string());
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
