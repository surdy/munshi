//! Marathon sessions: chunked map-reduce summarization (issue #48, summarizer contract v2).
//!
//! Drives the real munshi binary against the phase-aware fake summarizer
//! (`fixtures/manual/fake-summarizer/phase-aware.sh`), which validates every request against the
//! v2 envelope (contract_version, phase, `MUNSHI_SUMMARIZER_PHASE`, per-phase fields) and logs
//! one line per invocation so tests can assert per-phase requests and counts.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

const SESSION: &str = "48484848-4848-4848-8448-484848484848";

// ---------------------------------------------------------------------------
// Chunk-boundary correctness (record boundaries, order, coverage)
// ---------------------------------------------------------------------------

#[test]
fn chunk_ranges_split_only_on_event_boundaries_and_cover_every_event() {
    use munshi::{NormalizedEvent, chunk_event_ranges};
    let event = |kind: &'static str, size: usize| NormalizedEvent {
        kind,
        content: "z".repeat(size),
    };
    let events = vec![
        event("user", 100),
        event("assistant", 300),
        event("tool", 900),
        event("assistant", 50),
        event("user", 2_000), // alone over the target: must get its own range, never split
        event("assistant", 100),
        event("tool", 100),
    ];
    let target = 1_000;
    let ranges = chunk_event_ranges(&events, target);
    // Contiguous, in order, non-empty, and covering every event exactly once.
    let mut expected_start = 0;
    for range in &ranges {
        assert_eq!(range.start, expected_start, "ranges must be contiguous");
        assert!(range.end > range.start, "ranges must be non-empty");
        expected_start = range.end;
    }
    assert_eq!(
        expected_start,
        events.len(),
        "ranges must cover every event"
    );
    // Each multi-event range fits the target; an oversized event is isolated, not split.
    for range in &ranges {
        let size: usize = events[range.clone()]
            .iter()
            .map(|event| serde_json::to_vec(event).unwrap().len() + 1)
            .sum();
        assert!(
            size <= target || range.len() == 1,
            "range {range:?} of {size} bytes exceeds the target without being a single event"
        );
    }
    let oversized_range = ranges
        .iter()
        .find(|range| range.contains(&4))
        .expect("the oversized event is covered");
    assert_eq!(oversized_range.len(), 1, "oversized events sit alone");
    // Deterministic.
    assert_eq!(ranges, chunk_event_ranges(&events, target));
}

// ---------------------------------------------------------------------------
// End-to-end lifecycle tests against the phase-aware fake
// ---------------------------------------------------------------------------

/// Below the chunk threshold nothing changes but the additive v2 envelope: exactly one
/// invocation, `phase: "complete"`, and the calibrated defaults persisted at registration.
#[test]
fn below_threshold_session_summarizes_one_shot_with_v2_envelope() {
    let harness = Harness::new();
    harness.register(&[]);
    let config: Value =
        serde_json::from_slice(&std::fs::read(harness.state.join("config.json")).unwrap()).unwrap();
    // The token-calibrated defaults (issue #48 live-calibration comment): the backend boundary is
    // ~922k tokens, so at byte/token ratios of ~3.2–4.5 the old 6 MiB / 2 MiB byte-calibrated
    // pair admitted one-shot rejections.
    assert_eq!(config["limits"]["chunk_threshold_bytes"], 2_621_440);
    assert_eq!(config["limits"]["chunk_size_bytes"], 1_572_864);
    // Field-calibrated size caps (issue #41): 64 MiB raw / 8 MiB normalized cover real agentic
    // sessions. `max_input_bytes` sits above the chunk threshold by design — chunking engages
    // first, the input cap is only a hard safety bound.
    assert_eq!(config["limits"]["max_source_bytes"], 67_108_864);
    assert_eq!(config["limits"]["max_input_bytes"], 8_388_608);

    harness.write_marathon_transcript(SESSION, 4, 40);
    harness.drive_hooks_and_wait(SESSION);
    assert_eq!(harness.log_lines(), vec!["complete 4"]);
    let markdown = harness.archive_markdown(SESSION);
    assert!(markdown.contains("Complete one-shot summary"), "{markdown}");
}

/// Over the threshold the session is split on record boundaries into ~chunk_size_bytes chunks,
/// each summarized with a per-segment request carrying the previous chunk's summary, then one
/// reduce request over the chunk summaries produces the archived session summary. Every
/// invocation is charged against the per-project budget.
#[test]
fn over_threshold_session_chunks_in_order_then_reduces() {
    let harness = Harness::new();
    harness.register(&[
        "--chunk-threshold-bytes",
        "4000",
        "--chunk-size-bytes",
        "1200",
        "--max-calls-per-hour",
        "100",
    ]);
    harness.write_marathon_transcript(SESSION, 20, 180);
    harness.drive_hooks_and_wait(SESSION);

    let lines = harness.log_lines();
    let chunks: Vec<ChunkLine> = lines
        .iter()
        .filter(|line| line.starts_with("chunk "))
        .map(|line| ChunkLine::parse(line))
        .collect();
    let reduces: Vec<&String> = lines
        .iter()
        .filter(|line| line.starts_with("reduce "))
        .collect();
    assert!(chunks.len() >= 2, "expected multiple chunks: {lines:?}");
    assert_eq!(reduces.len(), 1, "expected exactly one reduce: {lines:?}");
    assert_eq!(
        lines.last().map(String::as_str),
        Some(format!("reduce {} 0", chunks.len()).as_str()),
        "the reduce runs last, over every chunk summary, quoting no events"
    );
    // In-order 1..=count segments; the marker coverage is complete and contiguous across chunk
    // boundaries (no event lost, duplicated, or split); continuity context from chunk 2 on.
    let mut next_marker = 0;
    for (position, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.index, position + 1);
        assert_eq!(chunk.count, chunks.len());
        assert_eq!(chunk.first, next_marker, "coverage gap at {position}");
        assert_eq!(chunk.previous, position > 0, "previous-summary continuity");
        next_marker = chunk.last + 1;
    }
    assert_eq!(next_marker, 20, "all 20 events summarized exactly once");

    let markdown = harness.archive_markdown(SESSION);
    assert!(
        markdown.contains(&format!("Reduced {} segment summaries", chunks.len())),
        "{markdown}"
    );
    assert!(!markdown.contains("summary_placeholder"), "{markdown}");
    // Each invocation was individually charged against the per-project budget.
    assert_eq!(harness.budget_calls(), chunks.len() + 1);
}

/// When the reduce input itself exceeds the threshold, groups of segment summaries are condensed
/// into intermediate summaries first, and the final reduce runs over those (reduce recursion).
#[test]
fn oversized_reduce_input_recurses_through_intermediate_reduces() {
    let harness = Harness::new();
    harness.register(&[
        "--chunk-threshold-bytes",
        "4000",
        "--chunk-size-bytes",
        "600",
        "--max-calls-per-hour",
        "100",
    ]);
    // Pad every chunk summary so the combined reduce input cannot fit the threshold.
    std::fs::write(format!("{}.pad", harness.log.display()), b"800").unwrap();
    harness.write_marathon_transcript(SESSION, 12, 250);
    harness.drive_hooks_and_wait(SESSION);

    let lines = harness.log_lines();
    let chunk_count = lines
        .iter()
        .filter(|line| line.starts_with("chunk "))
        .count();
    let reduces: Vec<&String> = lines
        .iter()
        .filter(|line| line.starts_with("reduce "))
        .collect();
    assert!(chunk_count >= 4, "expected several chunks: {lines:?}");
    assert!(
        reduces.len() >= 2,
        "an oversized reduce input must recurse through intermediate reduces: {lines:?}"
    );
    let final_inputs: usize = reduces
        .last()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        final_inputs < chunk_count,
        "the final reduce runs over condensed intermediates, not all {chunk_count} chunks"
    );
    let markdown = harness.archive_markdown(SESSION);
    assert!(
        markdown.contains(&format!("Reduced {final_inputs} segment summaries")),
        "{markdown}"
    );
}

/// A failure mid-chunk aborts the whole attempt through the ordinary issue #38 backoff — no
/// partial summary and no archive — and the next attempt re-runs the chunk sequence from the
/// start before reducing.
#[test]
fn mid_chunk_failure_backs_off_without_partial_summaries() {
    let harness = Harness::new();
    harness.register(&[
        "--chunk-threshold-bytes",
        "4000",
        "--chunk-size-bytes",
        "1200",
        "--max-calls-per-hour",
        "100",
    ]);
    std::fs::write(format!("{}.fail-chunk-2", harness.log.display()), b"").unwrap();
    harness.write_marathon_transcript(SESSION, 20, 180);
    harness.drive_hooks(SESSION);
    harness.wait_for_backoff(SESSION);

    let lines = harness.log_lines();
    assert!(lines.iter().any(|line| line.starts_with("chunk 1 ")));
    assert!(lines.iter().any(|line| line.starts_with("chunk-failed 2 ")));
    assert!(
        !lines.iter().any(|line| line.starts_with("reduce ")),
        "no reduce may run after a failed chunk: {lines:?}"
    );
    assert!(
        !harness.archive_file(SESSION).exists(),
        "no partial summary may be archived"
    );
    let (category, next_retry, streak) = harness.session_park(SESSION);
    assert_eq!(category.as_deref(), Some("summary-failed"));
    assert!(next_retry.is_some_and(|at| at >= 0), "backoff, not a park");
    assert_eq!(streak, 1);

    // The next attempt restarts chunking from segment 1 and completes.
    std::fs::remove_file(format!("{}.fail-chunk-2", harness.log.display())).unwrap();
    harness.make_retry_due(SESSION);
    assert_cli_success(&harness.munshi(&["hook-worker", "--session-id", SESSION]));
    let lines = harness.log_lines();
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("chunk 1 "))
            .count(),
        2,
        "the retry re-runs the chunk sequence from the start: {lines:?}"
    );
    let markdown = harness.archive_markdown(SESSION);
    assert!(markdown.contains("Reduced"), "{markdown}");
    assert!(markdown.contains("summary_revision: 1"), "{markdown}");
}

/// A request no split can bring under the chunk threshold — here a single event larger than the
/// threshold itself — is genuinely unchunkable: the deterministic `summary-input-limit` verdict
/// engages the issue #43 placeholder floor immediately, without any summarizer invocation.
#[test]
fn unchunkable_single_event_floors_immediately_under_the_input_limit_category() {
    let harness = Harness::new();
    harness.register(&[
        "--chunk-threshold-bytes",
        "2000",
        "--chunk-size-bytes",
        "1200",
    ]);
    // One 4000-byte event (far under max_event_text_bytes, so it is not elided) that cannot fit
    // any chunk request under the 2000-byte threshold.
    harness.write_marathon_transcript(SESSION, 2, 4_000);
    harness.drive_hooks(SESSION);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !harness.archive_file(SESSION).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "unchunkable session never placeholder-archived"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let markdown = harness.archive_markdown(SESSION);
    assert!(markdown.contains("summary_placeholder: true"), "{markdown}");
    assert!(
        markdown.contains(
            "Summary unavailable: normalized input exceeds the configured summarizer input limit"
        ),
        "{markdown}"
    );
    assert_eq!(
        harness.log_lines(),
        Vec::<String>::new(),
        "no summarizer invocation may be attempted"
    );
    let (category, next_retry, _) = harness.session_park(SESSION);
    assert_eq!(category.as_deref(), Some("summary-input-limit"));
    assert_eq!(next_retry, Some(-1));
}

/// The issue #48 backfill path end-to-end: a marathon session parked under the placeholder floor
/// (repeated chunk-phase rejections reaching the issue #38 park threshold) is replaced by a real
/// chunked summary through the existing issue #43 supersession machinery on `munshi retry`.
#[test]
fn placeholder_is_replaced_by_a_real_chunked_summary_on_retry() {
    let harness = Harness::new();
    harness.register(&[
        "--chunk-threshold-bytes",
        "4000",
        "--chunk-size-bytes",
        "1200",
        "--max-calls-per-hour",
        "100",
    ]);
    let fail_all = format!("{}.fail-all", harness.log.display());
    std::fs::write(&fail_all, b"").unwrap();
    harness.write_marathon_transcript(SESSION, 20, 180);
    harness.drive_hooks(SESSION);
    harness.wait_for_backoff(SESSION);
    // Attempts 2-5 reach the park threshold; the placeholder floor archives at attempt 5.
    for _ in 0..4 {
        harness.make_retry_due(SESSION);
        let _ = harness.munshi(&["hook-worker", "--session-id", SESSION]);
    }
    let markdown = harness.archive_markdown(SESSION);
    assert!(markdown.contains("summary_placeholder: true"), "{markdown}");
    assert!(
        markdown.contains("munshi-placeholder-summary"),
        "{markdown}"
    );
    let (category, next_retry, streak) = harness.session_park(SESSION);
    assert_eq!(category.as_deref(), Some("summary-failed"));
    assert_eq!(next_retry, Some(-1));
    assert_eq!(streak, 5);

    // Targeted retry with a now-working summarizer: the chunked path replaces the placeholder.
    std::fs::remove_file(&fail_all).unwrap();
    let retried = harness.json(&["retry", SESSION, "--json"]);
    assert_eq!(retried["result"], "archived", "retry report: {retried}");
    let replaced = harness.archive_markdown(SESSION);
    assert!(replaced.contains("summary_revision: 2"), "{replaced}");
    assert!(!replaced.contains("summary_placeholder"), "{replaced}");
    assert!(!replaced.contains("munshi-placeholder-summary"));
    assert!(replaced.contains("Reduced"), "{replaced}");
    let lines = harness.log_lines();
    assert!(lines.iter().any(|line| line.starts_with("chunk 1 ")));
    assert!(lines.iter().any(|line| line.starts_with("reduce ")));
}

/// The new `limits` fields are additive with serde defaults: an existing v2 configuration
/// written before issue #48 (no `chunk_threshold_bytes`/`chunk_size_bytes`) still loads and
/// archives without any config version bump.
#[test]
fn v2_config_without_chunk_fields_loads_and_archives_with_defaults() {
    let harness = Harness::new();
    harness.register(&[]);
    let config_path = harness.state.join("config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(config["version"], 2, "no config version bump");
    let limits = config["limits"].as_object_mut().unwrap();
    limits
        .remove("chunk_threshold_bytes")
        .expect("field was persisted");
    limits
        .remove("chunk_size_bytes")
        .expect("field was persisted");
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    harness.write_marathon_transcript(SESSION, 4, 40);
    harness.drive_hooks_and_wait(SESSION);
    assert_eq!(harness.log_lines(), vec!["complete 4"]);
}

// ---------------------------------------------------------------------------
// The input cap / chunk threshold relation (issue #52)
// ---------------------------------------------------------------------------

/// `max_input_bytes` is a never-exceed backstop *above* the chunk threshold, never below it: a cap
/// under the threshold recreates the pre-issue-#48 band in which requests between the two values
/// floor to placeholder summaries instead of being chunked or summarized. `register` rejects the
/// inverted relation before writing any configuration; an equal (or larger) cap is accepted.
#[test]
fn register_rejects_an_input_cap_below_the_chunk_threshold() {
    let harness = Harness::new();
    // Against the default threshold (2.5 MiB).
    let inverted = harness.try_register(&["--max-input-bytes", "1000"]);
    assert!(!inverted.status.success());
    let stderr = String::from_utf8_lossy(&inverted.stderr).into_owned();
    assert!(stderr.contains("--max-input-bytes"), "stderr: {stderr}");
    assert!(
        stderr.contains("--chunk-threshold-bytes"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("placeholder"), "stderr: {stderr}");
    assert!(
        !harness.state.join("config.json").exists(),
        "a rejected registration must write no configuration"
    );

    // Also inverted against an explicit threshold.
    let explicit = harness.try_register(&[
        "--chunk-threshold-bytes",
        "8000",
        "--max-input-bytes",
        "4000",
    ]);
    assert!(!explicit.status.success());
    assert!(!harness.state.join("config.json").exists());

    // Equal is the tightest valid relation, and it registers.
    harness.register(&[
        "--chunk-threshold-bytes",
        "4000",
        "--max-input-bytes",
        "4000",
    ]);
    assert_eq!(harness.config()["limits"]["max_input_bytes"], 4000);
    assert_eq!(harness.config()["limits"]["chunk_threshold_bytes"], 4000);
}

/// Manual `munshi archive` validates its `--max-input-bytes` against the registered threshold —
/// the same relation the hook path is registered under — so a one-shot manual run cannot be given
/// a cap that would fail the session on size instead of summarizing it.
#[test]
fn manual_archive_rejects_an_input_cap_below_the_chunk_threshold() {
    let harness = Harness::new();
    harness.register(&["--chunk-threshold-bytes", "500000"]);
    let events = harness
        .write_marathon_transcript(SESSION, 4, 40)
        .canonicalize()
        .unwrap();
    let summarizer = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/manual/fake-summarizer/phase-aware.sh")
        .canonicalize()
        .unwrap();
    std::fs::set_permissions(&summarizer, std::fs::Permissions::from_mode(0o755)).unwrap();
    let archive = |max_input_bytes: &str| {
        harness.munshi(&[
            "archive",
            SESSION,
            "--events",
            events.to_str().unwrap(),
            "--project-dir",
            harness.project.to_str().unwrap(),
            "--output-dir",
            harness.output.to_str().unwrap(),
            "--summarizer",
            summarizer.to_str().unwrap(),
            "--summarizer-arg",
            harness.log.to_str().unwrap(),
            "--max-input-bytes",
            max_input_bytes,
        ])
    };

    let rejected = archive("1000");
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr).into_owned();
    assert!(stderr.contains("--max-input-bytes"), "stderr: {stderr}");
    assert!(
        stderr.contains("--chunk-threshold-bytes"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("500000"), "stderr: {stderr}");
    assert!(
        harness.log_lines().is_empty(),
        "a rejected manual archive must never invoke the summarizer"
    );

    // A cap at or above the registered threshold archives normally.
    assert_cli_success(&archive("500000"));
    assert_eq!(harness.log_lines(), vec!["complete 4"]);
}

/// The relation is only enforceable at the CLI, so a hand-edited `config.json` can still violate
/// it. `munshi doctor` names the violation rather than leaving it to be discovered as unexplained
/// `summary-input-limit` parks.
#[test]
fn doctor_warns_on_a_hand_edited_inverted_input_cap() {
    let harness = Harness::new();
    harness.register(&[]);
    let healthy = harness.json(&["doctor", "--json"]);
    assert!(
        !doctor_check_codes(&healthy).contains(&"input-cap-relation".to_owned()),
        "a valid relation must not warn: {healthy}"
    );

    let config_path = harness.state.join("config.json");
    let mut config = harness.config();
    config["limits"]["max_input_bytes"] = json!(1000);
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    let report = harness.json(&["doctor", "--json"]);
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["code"] == "input-cap-relation")
        .unwrap_or_else(|| panic!("doctor reports the inverted relation: {report}"));
    assert_eq!(check["status"], "warning", "check: {check}");
    let message = check["message"].as_str().unwrap();
    assert!(message.contains("1000"), "message: {message}");
    assert!(message.contains("2621440"), "message: {message}");
    assert!(message.contains("max_input_bytes"), "message: {message}");
    assert!(
        message.contains("chunk_threshold_bytes"),
        "message: {message}"
    );
}

fn doctor_check_codes(report: &Value) -> Vec<String> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["code"].as_str().unwrap().to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Contrib wrapper phase/model env plumbing
// ---------------------------------------------------------------------------

const WRAPPER_RESPONSE: &str = r#"{"title":"t","goal":"g","work_completed":["w"],"decisions":["d"],"files_changed":["f"],"commands_and_validation":["c"],"open_items":["o"],"tags":["x"]}"#;

/// Runs a contrib wrapper with the backing CLI replaced by a stub that records its argv, and
/// returns the recorded argv line.
fn run_wrapper(wrapper: &str, bin_env: &str, envs: &[(&str, &str)]) -> String {
    let directory = TempDir::new().unwrap();
    let argv_log = directory.path().join("argv");
    let stub = directory.path().join("stub.sh");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' \"$*\" > '{}'\nprintf '%s' '{}'\n",
            argv_log.display(),
            WRAPPER_RESPONSE,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wrapper_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contrib")
        .join(wrapper)
        .canonicalize()
        .unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .arg(&wrapper_path)
        // Isolate HOME so the Copilot wrapper's isolated-home setup stays inside the tempdir.
        .env("HOME", directory.path())
        .env(bin_env, &stub)
        .env_remove("MUNSHI_SUMMARIZER_PHASE")
        .env_remove("MUNSHI_CHUNK_MODEL")
        .env_remove("MUNSHI_REDUCE_MODEL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"contract_version\":2}")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "wrapper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(&argv_log)
        .unwrap()
        .trim()
        .to_owned()
}

#[test]
fn claude_wrapper_selects_the_model_from_the_phase_env() {
    let base = run_wrapper(
        "claude-summarizer.sh",
        "CLAUDE_BIN",
        &[
            ("CLAUDE_MODEL", "base-model"),
            ("MUNSHI_CHUNK_MODEL", "chunk-model"),
        ],
    );
    assert!(
        base.contains("--model base-model"),
        "no phase => base model: {base}"
    );

    let chunk = run_wrapper(
        "claude-summarizer.sh",
        "CLAUDE_BIN",
        &[
            ("CLAUDE_MODEL", "base-model"),
            ("MUNSHI_SUMMARIZER_PHASE", "chunk"),
            ("MUNSHI_CHUNK_MODEL", "chunk-model"),
        ],
    );
    assert!(chunk.contains("--model chunk-model"), "{chunk}");

    let reduce = run_wrapper(
        "claude-summarizer.sh",
        "CLAUDE_BIN",
        &[
            ("CLAUDE_MODEL", "base-model"),
            ("MUNSHI_SUMMARIZER_PHASE", "reduce"),
            ("MUNSHI_REDUCE_MODEL", "reduce-model"),
        ],
    );
    assert!(reduce.contains("--model reduce-model"), "{reduce}");

    // A phase without a matching override keeps the base model.
    let fallback = run_wrapper(
        "claude-summarizer.sh",
        "CLAUDE_BIN",
        &[
            ("CLAUDE_MODEL", "base-model"),
            ("MUNSHI_SUMMARIZER_PHASE", "chunk"),
        ],
    );
    assert!(fallback.contains("--model base-model"), "{fallback}");
}

#[test]
fn copilot_wrapper_passes_a_model_flag_only_when_the_phase_override_is_set() {
    let base = run_wrapper("copilot-summarizer.sh", "COPILOT_BIN", &[]);
    assert!(
        !base.contains("--model"),
        "without overrides no --model flag is passed (current behavior): {base}"
    );

    let chunk = run_wrapper(
        "copilot-summarizer.sh",
        "COPILOT_BIN",
        &[
            ("MUNSHI_SUMMARIZER_PHASE", "chunk"),
            ("MUNSHI_CHUNK_MODEL", "chunk-model"),
        ],
    );
    assert!(chunk.contains("--model chunk-model"), "{chunk}");

    let reduce = run_wrapper(
        "copilot-summarizer.sh",
        "COPILOT_BIN",
        &[
            ("MUNSHI_SUMMARIZER_PHASE", "reduce"),
            ("MUNSHI_REDUCE_MODEL", "reduce-model"),
        ],
    );
    assert!(reduce.contains("--model reduce-model"), "{reduce}");

    // The override is phase-scoped: a chunk override alone never leaks into other phases.
    let complete = run_wrapper(
        "copilot-summarizer.sh",
        "COPILOT_BIN",
        &[
            ("MUNSHI_SUMMARIZER_PHASE", "complete"),
            ("MUNSHI_CHUNK_MODEL", "chunk-model"),
        ],
    );
    assert!(!complete.contains("--model"), "{complete}");
}

// ---------------------------------------------------------------------------
// Configured summarizer environment (`summarizer.env` / --summarizer-env)
// ---------------------------------------------------------------------------

/// The `--summarizer-env` map round-trips through `config.json` and is registration-owned like
/// the other summarizer fields: each re-register rewrites it from its own flags, and a register
/// without the flag clears it. Values may contain `=`; only the key cannot.
#[test]
fn summarizer_env_round_trips_config_and_is_registration_owned() {
    let harness = Harness::new();
    harness.register(&[
        "--summarizer-env",
        "MUNSHI_TEST_SUMMARIZER_ENV=first",
        "--summarizer-env",
        "OTHER_KEY=value=with=equals",
    ]);
    let config = harness.config();
    assert_eq!(
        config["summarizer"]["env"],
        json!({
            "MUNSHI_TEST_SUMMARIZER_ENV": "first",
            "OTHER_KEY": "value=with=equals",
        })
    );

    harness.register(&["--summarizer-env", "MUNSHI_TEST_SUMMARIZER_ENV=second"]);
    assert_eq!(
        harness.config()["summarizer"]["env"],
        json!({"MUNSHI_TEST_SUMMARIZER_ENV": "second"})
    );

    harness.register(&[]);
    assert_eq!(harness.config()["summarizer"]["env"], json!({}));
}

/// Keys Munshi itself owns (the reserved `MUNSHI_SUMMARIZER_*` namespace, carrying
/// `MUNSHI_SUMMARIZER_PHASE`) and malformed assignments are rejected at register, before any
/// configuration is written.
#[test]
fn register_rejects_reserved_and_malformed_summarizer_env_keys() {
    let harness = Harness::new();
    let reserved = harness.try_register(&["--summarizer-env", "MUNSHI_SUMMARIZER_PHASE=chunk"]);
    assert!(!reserved.status.success());
    assert!(
        String::from_utf8_lossy(&reserved.stderr).contains("reserved"),
        "stderr: {}",
        String::from_utf8_lossy(&reserved.stderr)
    );

    let no_equals = harness.try_register(&["--summarizer-env", "NO_EQUALS_SIGN"]);
    assert!(!no_equals.status.success());
    assert!(String::from_utf8_lossy(&no_equals.stderr).contains("KEY=VALUE"));

    let empty_key = harness.try_register(&["--summarizer-env", "=value"]);
    assert!(!empty_key.status.success());
    assert!(String::from_utf8_lossy(&empty_key.stderr).contains("non-empty"));

    assert!(
        !harness.state.join("config.json").exists(),
        "a rejected registration must write no configuration"
    );
}

/// The configured environment reaches every child invocation of a chunked summary — each chunk
/// and the reduce alike — while Munshi's own phase variable still arrives beside it (the fake
/// fails any invocation whose MUNSHI_SUMMARIZER_PHASE does not match the request's phase).
#[test]
fn configured_env_reaches_every_phase_of_a_chunked_summary() {
    let harness = Harness::new();
    harness.register(&[
        "--summarizer-env",
        "MUNSHI_TEST_SUMMARIZER_ENV=from-config",
        "--chunk-threshold-bytes",
        "4000",
        "--chunk-size-bytes",
        "1200",
        "--max-calls-per-hour",
        "100",
    ]);
    harness.write_marathon_transcript(SESSION, 20, 180);
    harness.drive_hooks_and_wait(SESSION);

    let lines = harness.log_lines();
    let invocations = lines
        .iter()
        .filter(|line| {
            line.starts_with("chunk ")
                || line.starts_with("reduce ")
                || line.starts_with("complete ")
        })
        .count();
    let env_lines = lines
        .iter()
        .filter(|line| *line == "env from-config")
        .count();
    assert!(invocations >= 3, "expected a chunked run: {lines:?}");
    assert_eq!(
        env_lines, invocations,
        "every invocation must receive the configured environment: {lines:?}"
    );
}

/// Merge order: the configured map is exported before Munshi's own per-invocation variables, so
/// Munshi's win on conflict. `register` refuses reserved keys, so the conflict is planted by
/// hand-editing `config.json`; the fake summarizer fails any invocation whose
/// MUNSHI_SUMMARIZER_PHASE does not match the request's phase, so a successful archive proves
/// Munshi's value overrode the configured one.
#[test]
fn munshi_owned_phase_variable_wins_over_a_conflicting_configured_key() {
    let harness = Harness::new();
    harness.register(&["--summarizer-env", "MUNSHI_TEST_SUMMARIZER_ENV=alongside"]);
    let config_path = harness.state.join("config.json");
    let mut config = harness.config();
    config["summarizer"]["env"]["MUNSHI_SUMMARIZER_PHASE"] = Value::String("bogus".to_owned());
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    harness.write_marathon_transcript(SESSION, 4, 40);
    harness.drive_hooks_and_wait(SESSION);
    assert_eq!(harness.log_lines(), vec!["env alongside", "complete 4"]);
}

/// `munshi archive` takes the same repeatable `--summarizer-env` flag and passes the configured
/// environment to its one-shot summarizer invocation.
#[test]
fn manual_archive_passes_summarizer_env_to_the_summarizer() {
    let harness = Harness::new();
    let events = harness
        .write_marathon_transcript(SESSION, 4, 40)
        .canonicalize()
        .unwrap();
    let summarizer = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/manual/fake-summarizer/phase-aware.sh")
        .canonicalize()
        .unwrap();
    std::fs::set_permissions(&summarizer, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = harness.munshi(&[
        "archive",
        SESSION,
        "--events",
        events.to_str().unwrap(),
        "--project-dir",
        harness.project.to_str().unwrap(),
        "--output-dir",
        harness.output.to_str().unwrap(),
        "--summarizer",
        summarizer.to_str().unwrap(),
        "--summarizer-arg",
        harness.log.to_str().unwrap(),
        "--summarizer-env",
        "MUNSHI_TEST_SUMMARIZER_ENV=manual-env",
    ]);
    assert_cli_success(&output);
    assert_eq!(harness.log_lines(), vec!["env manual-env", "complete 4"]);
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct ChunkLine {
    index: usize,
    count: usize,
    first: usize,
    last: usize,
    previous: bool,
}

impl ChunkLine {
    /// Parses `chunk <index> <count> <events> <first-marker> <last-marker> <prev>`.
    fn parse(line: &str) -> Self {
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 7, "malformed chunk log line: {line}");
        Self {
            index: fields[1].parse().unwrap(),
            count: fields[2].parse().unwrap(),
            first: fields[4].parse().unwrap(),
            last: fields[5].parse().unwrap(),
            previous: fields[6] == "1",
        }
    }
}

struct Harness {
    #[allow(dead_code)]
    directory: TempDir,
    copilot_home: PathBuf,
    state: PathBuf,
    output: PathBuf,
    project: PathBuf,
    log: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/munshi-marathon-test-artifacts");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("marathon-case-")
            .tempdir_in(root)
            .unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        Self {
            copilot_home: directory.path().join("copilot-home"),
            state: directory.path().join("munshi-home"),
            output: directory.path().join("archives"),
            log: directory.path().join("phase-log"),
            project,
            directory,
        }
    }

    /// Registers with the phase-aware fake summarizer, passing the invocation-log path as the
    /// summarizer's own argument.
    fn register(&self, extra_args: &[&str]) {
        assert_cli_success(&self.try_register(extra_args));
    }

    /// Like [`Harness::register`], but returns the raw output so tests can assert rejections.
    fn try_register(&self, extra_args: &[&str]) -> Output {
        let summarizer = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/manual/fake-summarizer/phase-aware.sh");
        std::fs::set_permissions(&summarizer, std::fs::Permissions::from_mode(0o755)).unwrap();
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
            .arg(summarizer.canonicalize().unwrap())
            .arg("--summarizer-arg")
            .arg(&self.log)
            .arg("--timeout-ms")
            .arg("15000")
            .args(extra_args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// The current parsed `config.json`.
    fn config(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.state.join("config.json")).unwrap()).unwrap()
    }

    fn munshi(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .args(args)
            .arg("--state-dir")
            .arg(&self.state)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.munshi(args);
        assert!(
            !output.stdout.is_empty(),
            "empty stdout; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("valid JSON")
    }

    fn hook(&self, event: &str, payload: &Value) {
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
        assert_cli_success(&child.wait_with_output().unwrap());
    }

    fn drive_hooks(&self, session_id: &str) {
        let transcript = self
            .copilot_home
            .join("session-state")
            .join(session_id)
            .join("events.jsonl")
            .canonicalize()
            .unwrap();
        self.hook(
            "agent-stop",
            &json!({
                "sessionId": session_id,
                "timestamp": 10_000,
                "cwd": self.project,
                "transcriptPath": transcript,
                "stopReason": "end_turn",
            }),
        );
        self.hook(
            "session-end",
            &json!({
                "sessionId": session_id,
                "timestamp": 10_001,
                "cwd": self.project,
                "reason": "complete",
            }),
        );
    }

    fn drive_hooks_and_wait(&self, session_id: &str) {
        self.drive_hooks(session_id);
        assert_cli_success(&self.munshi(&[
            "hook",
            "wait",
            "--session-id",
            session_id,
            "--timeout-ms",
            "30000",
        ]));
    }

    /// Waits until the hook-spawned worker records its failing attempt's backoff.
    fn wait_for_backoff(&self, session_id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while self.session_park(session_id).1.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "failing attempt never recorded a backoff"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Writes a Copilot transcript of `events` alternating user/assistant messages, each
    /// carrying an `event-NNNN` sequence marker plus `content_bytes` of filler.
    fn write_marathon_transcript(
        &self,
        session_id: &str,
        events: usize,
        content_bytes: usize,
    ) -> PathBuf {
        let path = self
            .copilot_home
            .join("session-state")
            .join(session_id)
            .join("events.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut records = vec![
            json!({
                "id": "start",
                "timestamp": "2026-07-12T00:00:00.000Z",
                "parentId": null,
                "type": "session.start",
                "data": {"sessionId": session_id},
            })
            .to_string(),
        ];
        let mut parent = "start".to_owned();
        for position in 0..events {
            let id = format!("record-{position}");
            let content = format!("event-{position:04} {}", "y".repeat(content_bytes));
            let timestamp = format!(
                "2026-07-12T00:{:02}:{:02}.000Z",
                (position + 1) / 60,
                (position + 1) % 60
            );
            let record = if position % 2 == 0 {
                json!({
                    "id": id,
                    "timestamp": timestamp,
                    "parentId": parent,
                    "type": "user.message",
                    "data": {"content": content},
                })
            } else {
                json!({
                    "id": id,
                    "timestamp": timestamp,
                    "parentId": parent,
                    "type": "assistant.message",
                    "data": {"content": content, "messageId": format!("message-{position}")},
                })
            };
            records.push(record.to_string());
            parent = format!("record-{position}");
        }
        std::fs::write(&path, records.join("\n") + "\n").unwrap();
        path
    }

    fn log_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .map(|content| content.lines().map(ToOwned::to_owned).collect())
            .unwrap_or_default()
    }

    fn archive_file(&self, session_id: &str) -> PathBuf {
        let component = std::fs::read_dir(&self.output)
            .ok()
            .and_then(|mut entries| entries.next())
            .and_then(Result::ok)
            .map(|entry| entry.path())
            .unwrap_or_else(|| self.output.join("project"));
        component.join(format!("{session_id}.md"))
    }

    fn archive_markdown(&self, session_id: &str) -> String {
        std::fs::read_to_string(self.archive_file(session_id))
            .expect("archive Markdown was written")
    }

    fn session_park(&self, session_id: &str) -> (Option<String>, Option<i64>, i64) {
        rusqlite::Connection::open(self.state.join("munshi.db"))
            .unwrap()
            .query_row(
                "SELECT last_error_category,next_retry_at_ms,failure_streak
                 FROM sessions WHERE source_session_id=?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    fn make_retry_due(&self, session_id: &str) {
        let changed = rusqlite::Connection::open(self.state.join("munshi.db"))
            .unwrap()
            .execute(
                "UPDATE sessions SET next_retry_at_ms=1
                 WHERE source_session_id=?1 AND next_retry_at_ms>=0",
                [session_id],
            )
            .unwrap();
        assert_eq!(changed, 1, "session {session_id} had no pending backoff");
    }

    fn budget_calls(&self) -> usize {
        rusqlite::Connection::open(self.state.join("munshi.db"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM summarizer_calls", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap() as usize
    }
}

fn assert_cli_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
