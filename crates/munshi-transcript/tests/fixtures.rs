//! Fixture conformance walk for the lossless streaming parser (issue #26, ADR 0011).
//!
//! Every sanitized adapter fixture must stream-parse with every non-empty line accounted
//! for exactly once. Well-formed fixtures yield zero `Unknown` records and zero record
//! errors; the deliberately-negative fixtures (a malformed line, truncated trailing
//! records, a synthetic future event type) must surface as per-record `Unknown`/error
//! items without aborting the stream.

use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use munshi_transcript::{
    Classification, Event, NON_OBJECT_JSON_KIND, Record, RecordError, SessionSummary, Source,
    TranscriptStream,
};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

struct Fixture {
    source: Source,
    path: &'static str,
    expected_unknown: usize,
    expected_errors: usize,
}

const fn well_formed(source: Source, path: &'static str) -> Fixture {
    Fixture {
        source,
        path,
        expected_unknown: 0,
        expected_errors: 0,
    }
}

const FIXTURES: &[Fixture] = &[
    well_formed(
        Source::ClaudeCode,
        "claude-code-2.1.44/normal/0c1a0de0-0000-4000-8000-000000000001.jsonl",
    ),
    well_formed(
        Source::ClaudeCode,
        "claude-code-2.1.44/resumed/0c1a0de0-0000-4000-8000-000000000002.jsonl",
    ),
    well_formed(
        Source::ClaudeCode,
        "claude-code-2.1.44/interrupted/0c1a0de0-0000-4000-8000-000000000003.jsonl",
    ),
    well_formed(
        Source::ClaudeCode,
        "claude-code-2.1.44/missing-fields/0c1a0de0-0000-4000-8000-000000000004.jsonl",
    ),
    // Truncated transcript: the incomplete trailing record is a per-record error item,
    // not a whole-parse abort.
    Fixture {
        source: Source::ClaudeCode,
        path: "claude-code-2.1.44/truncated/0c1a0de0-0000-4000-8000-000000000005.jsonl",
        expected_unknown: 0,
        expected_errors: 1,
    },
    well_formed(
        Source::ClaudeCode,
        "claude-code-2.1.44/concurrent/0c1a0de0-0000-4000-8000-000000000006.jsonl",
    ),
    well_formed(
        Source::ClaudeCode,
        "claude-code-2.1.44/concurrent/0c1a0de0-0000-4000-8000-000000000007.jsonl",
    ),
    well_formed(
        Source::ClaudeCode,
        "claude-code-2.1.205/transcript/0c1a0de0-0000-4000-8000-000000000205.jsonl",
    ),
    well_formed(
        Source::ClaudeCode,
        "claude-code-2.1.2xx-bookkeeping/transcript/0c1a0de0-0000-4000-8000-000000000230.jsonl",
    ),
    well_formed(
        Source::ClaudeCode,
        "claude-code-2.1.2xx-bookkeeping/transcript/0c1a0de0-0000-4000-8000-000000000231.jsonl",
    ),
    // Issue #77: the shell shapes each harness records a command in, including the ones
    // whose command is deliberately not promoted.
    well_formed(
        Source::ClaudeCode,
        "claude-code-shell-command/transcript/0c1a0de0-0000-4000-8000-000000000077.jsonl",
    ),
    well_formed(
        Source::Codex,
        "codex-rollout-0.x/normal/c0de0000-0000-4000-8000-000000000001.jsonl",
    ),
    well_formed(
        Source::Codex,
        "codex-rollout-0.x/shell-command/c0de0000-0000-4000-8000-000000000077.jsonl",
    ),
    well_formed(
        Source::Codex,
        "codex-rollout-0.x/resumed/c0de0000-0000-4000-8000-000000000002.jsonl",
    ),
    well_formed(
        Source::Codex,
        "codex-rollout-0.x/missing-fields/c0de0000-0000-4000-8000-000000000003.jsonl",
    ),
    Fixture {
        source: Source::Codex,
        path: "codex-rollout-0.x/truncated/c0de0000-0000-4000-8000-000000000004.jsonl",
        expected_unknown: 0,
        expected_errors: 1,
    },
    well_formed(
        Source::Codex,
        "codex-rollout-0.x/concurrent/c0de0000-0000-4000-8000-000000000005.jsonl",
    ),
    well_formed(
        Source::Codex,
        "codex-rollout-0.x/concurrent/c0de0000-0000-4000-8000-000000000006.jsonl",
    ),
    well_formed(
        Source::Copilot,
        "manual/copilot/11111111-1111-4111-8111-111111111111/events.jsonl",
    ),
    // This fixture deliberately carries a synthetic future event type
    // (`future.private_event`); a lossless reader must surface it as `Unknown`.
    Fixture {
        source: Source::Copilot,
        path: "manual/copilot/22222222-2222-4222-8222-222222222222/events.jsonl",
        expected_unknown: 1,
        expected_errors: 0,
    },
    // Line 2 is literally `this is not json`: exactly one record error, and the stream
    // continues past it.
    Fixture {
        source: Source::Copilot,
        path: "manual/copilot/33333333-3333-4333-8333-333333333333/events.jsonl",
        expected_unknown: 0,
        expected_errors: 1,
    },
    well_formed(
        Source::Copilot,
        "manual/copilot/44444444-4444-4444-8444-444444444444/events.jsonl",
    ),
    well_formed(
        Source::Copilot,
        "manual/copilot/77777777-7777-4777-8777-777777777777/events.jsonl",
    ),
    well_formed(
        Source::Copilot,
        "copilot-1.0.70/transcript/synthetic-envelope.jsonl",
    ),
    // The four archive-observed tool-activity kinds (issue #51): skill.invoked,
    // tool.user_requested, and external_tool.requested/completed must classify as
    // content, leaving zero unknowns.
    well_formed(
        Source::Copilot,
        "copilot-tool-activity/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/events.jsonl",
    ),
    well_formed(
        Source::Copilot,
        "copilot-shell-command/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb/events.jsonl",
    ),
    well_formed(
        Source::Copilot,
        "copilot-1.0.5x-bookkeeping/55555555-5555-4555-8555-555555555555/events.jsonl",
    ),
    well_formed(
        Source::Copilot,
        "copilot-1.0.5x-bookkeeping/66666666-6666-4666-8666-666666666666/events.jsonl",
    ),
];

fn stream_fixture(source: Source, relative: &str) -> Vec<Result<Record, RecordError>> {
    let path = fixture_root().join(relative);
    let reader = BufReader::new(File::open(&path).unwrap());
    TranscriptStream::new(source, 1, reader)
        .unwrap()
        .collect_records()
}

#[test]
fn every_fixture_streams_losslessly_with_expected_unknowns_and_errors() {
    for fixture in FIXTURES {
        let items = stream_fixture(fixture.source, fixture.path);

        // Lossless accounting: one item per non-empty line, exactly once.
        let bytes = fs::read(fixture_root().join(fixture.path)).unwrap();
        let nonempty_lines = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count();
        assert_eq!(
            items.len(),
            nonempty_lines,
            "{}: every non-empty line appears exactly once",
            fixture.path
        );

        let unknown = items
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    Ok(Record {
                        classification: Classification::Unknown { .. },
                        ..
                    })
                )
            })
            .count();
        let errors = items.iter().filter(|item| item.is_err()).count();
        assert_eq!(
            unknown, fixture.expected_unknown,
            "{}: unknown records",
            fixture.path
        );
        assert_eq!(
            errors, fixture.expected_errors,
            "{}: record errors",
            fixture.path
        );
    }
}

#[test]
fn malformed_line_surfaces_as_an_error_item_and_the_stream_continues() {
    let items = stream_fixture(
        Source::Copilot,
        "manual/copilot/33333333-3333-4333-8333-333333333333/events.jsonl",
    );
    assert_eq!(items.len(), 3);
    let Err(RecordError::MalformedJson { line, record, raw }) = &items[1] else {
        panic!("line 2 must be a malformed-JSON record error");
    };
    assert_eq!((*line, *record), (2, 2));
    assert_eq!(raw, b"this is not json");
    // The stream continued past the error: the assistant message on line 3 is intact.
    let Ok(Record {
        classification: Classification::Content { events },
        ..
    }) = &items[2]
    else {
        panic!("line 3 must still classify as content");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind(), "assistant");
}

#[test]
fn truncated_trailing_records_surface_as_final_error_items() {
    for (source, path, line) in [
        (
            Source::ClaudeCode,
            "claude-code-2.1.44/truncated/0c1a0de0-0000-4000-8000-000000000005.jsonl",
            5,
        ),
        (
            Source::Codex,
            "codex-rollout-0.x/truncated/c0de0000-0000-4000-8000-000000000004.jsonl",
            8,
        ),
    ] {
        let items = stream_fixture(source, path);
        assert!(
            items[..items.len() - 1].iter().all(Result::is_ok),
            "{path}: only the trailing record is in error"
        );
        let Some(Err(RecordError::MalformedJson {
            line: error_line, ..
        })) = items.last()
        else {
            panic!("{path}: the incomplete trailing record must be an error item");
        };
        assert_eq!(*error_line, line, "{path}: trailing error line");
    }
}

#[test]
fn claude_bookkeeping_records_classify_as_typed_ignored_not_unknown() {
    let items = stream_fixture(
        Source::ClaudeCode,
        "claude-code-2.1.205/transcript/0c1a0de0-0000-4000-8000-000000000205.jsonl",
    );
    let ignored_kinds: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Ignored { kind },
                ..
            }) => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ignored_kinds,
        [
            "queue-operation",
            "queue-operation",
            "attachment",
            "ai-title",
            "last-prompt",
            "mode",
        ]
    );
    let content_kinds: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Content { events },
                ..
            }) => Some(events.iter().map(Event::kind).collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(content_kinds, ["user", "assistant"]);
}

#[test]
fn archive_observed_bookkeeping_kinds_classify_as_typed_ignored_not_unknown() {
    // Issue #30: newer Claude Code session bookkeeping the pinned 2.1.44/2.1.205
    // schema predates.
    let items = stream_fixture(
        Source::ClaudeCode,
        "claude-code-2.1.2xx-bookkeeping/transcript/0c1a0de0-0000-4000-8000-000000000230.jsonl",
    );
    let ignored_kinds: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Ignored { kind },
                ..
            }) => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ignored_kinds,
        [
            "permission-mode",
            "permission-mode",
            "pr-link",
            "file-history-delta",
            "frame-link",
        ]
    );
    let content_kinds: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Content { events },
                ..
            }) => Some(events.iter().map(Event::kind).collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(content_kinds, ["user", "assistant"]);

    // Issue #34: the Copilot `session.usage_checkpoint` bookkeeping kind absent from
    // the pinned 1.0.70 event tables.
    let items = stream_fixture(
        Source::Copilot,
        "copilot-1.0.5x-bookkeeping/55555555-5555-4555-8555-555555555555/events.jsonl",
    );
    let ignored_kinds: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Ignored { kind },
                ..
            }) => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ignored_kinds,
        [
            "session.start",
            "session.usage_checkpoint",
            "session.shutdown"
        ]
    );
    let summary = SessionSummary::summarize(&items);
    assert_eq!(summary.ignored_events, 3);
}

#[test]
fn census_typed_bookkeeping_kinds_classify_as_typed_ignored_not_unknown() {
    // Issue #45: historical Copilot bookkeeping kinds surfaced by the full-archive
    // census across pre-1.0.70 CLI versions.
    let items = stream_fixture(
        Source::Copilot,
        "copilot-1.0.5x-bookkeeping/66666666-6666-4666-8666-666666666666/events.jsonl",
    );
    let ignored_kinds: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Ignored { kind },
                ..
            }) => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ignored_kinds,
        [
            "session.start",
            "permission.requested",
            "permission.completed",
            "session.mode_changed",
            "session.permissions_changed",
            "subagent.started",
            "subagent.completed",
            "system.notification",
            "session.binary_asset",
            "session.compaction_start",
            "session.compaction_complete",
            "session.workspace_file_changed",
            "session.task_complete",
            "session.context_changed",
            "session.info",
            "session.plan_changed",
            "session.error",
            "abort",
            "session.shutdown",
        ]
    );
    let content_kinds: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Content { events },
                ..
            }) => Some(events.iter().map(Event::kind).collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(content_kinds, ["user", "assistant"]);
    let summary = SessionSummary::summarize(&items);
    assert_eq!(summary.ignored_events, 19);

    // Issue #46: the Claude Code `agent-name` bookkeeping kind, the census's only
    // remaining claude-code Unknown.
    let items = stream_fixture(
        Source::ClaudeCode,
        "claude-code-2.1.2xx-bookkeeping/transcript/0c1a0de0-0000-4000-8000-000000000231.jsonl",
    );
    let ignored_kinds: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Ignored { kind },
                ..
            }) => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ignored_kinds, ["agent-name"]);
    let content_kinds: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Content { events },
                ..
            }) => Some(events.iter().map(Event::kind).collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(content_kinds, ["user", "assistant"]);
    let summary = SessionSummary::summarize(&items);
    assert_eq!(summary.ignored_events, 1);
}

#[test]
fn synthetic_envelope_records_all_classify_as_typed_ignored() {
    // Every record carries `data: {"synthetic": true}` — recognized event types whose
    // data misses the pinned content/validation keys degrade to `Ignored`, exactly as
    // the legacy normalizer counted them as ignored (never `Unknown`).
    let items = stream_fixture(
        Source::Copilot,
        "copilot-1.0.70/transcript/synthetic-envelope.jsonl",
    );
    let kinds: Vec<_> = items
        .iter()
        .map(|item| match item {
            Ok(Record {
                classification: Classification::Ignored { kind },
                ..
            }) => kind.as_str(),
            other => panic!("expected only ignored records, found {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "session.start",
            "session.model_change",
            "system.message",
            "user.message",
            "assistant.turn_start",
            "assistant.message",
            "assistant.turn_end",
            "hook.start",
            "hook.end",
            "session.resume",
            "session.shutdown",
        ]
    );
    // Numeric timestamps are preserved raw but never parsed as RFC 3339.
    let summary = SessionSummary::summarize(&items);
    assert_eq!(summary.started_at(), None);
    assert_eq!(summary.ignored_events, 11);
}

#[test]
fn unknown_records_carry_the_raw_record() {
    let items = stream_fixture(
        Source::Copilot,
        "manual/copilot/22222222-2222-4222-8222-222222222222/events.jsonl",
    );
    let raws: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Unknown { raw },
                ..
            }) => Some(raw.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(raws.len(), 1);
    assert!(raws[0].contains("\"future.private_event\""));
}

/// Every tool event in a fixture, as `(name, promoted command)`.
fn tool_commands(source: Source, relative: &str) -> Vec<(Option<String>, Option<String>)> {
    stream_fixture(source, relative)
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Content { events },
                ..
            }) => Some(events.clone()),
            _ => None,
        })
        .flatten()
        .filter_map(|event| match event {
            Event::Tool(tool) => Some((
                tool.name().map(ToOwned::to_owned),
                tool.command().map(ToOwned::to_owned),
            )),
            _ => None,
        })
        .collect()
}

/// Issue #77: shell-tool events carry a typed `command`, per source, for exactly the shapes
/// whose command location is certain — and nothing else gains one.
#[test]
fn shell_tool_events_carry_a_typed_command_per_source() {
    // Claude Code: `Bash`, from `tool_use.input.command`. `Read` is not a shell tool; the
    // last two `Bash` calls carry no readable command (no `command` key, non-object input).
    assert_eq!(
        tool_commands(
            Source::ClaudeCode,
            "claude-code-shell-command/transcript/0c1a0de0-0000-4000-8000-000000000077.jsonl",
        ),
        [
            (Some("Bash".to_owned()), Some("cargo test --all".to_owned())),
            (None, None), // the tool_result correlating the first call
            (Some("Bash".to_owned()), Some("cargo test --all".to_owned())),
            (Some("Read".to_owned()), None),
            (Some("Bash".to_owned()), None),
            (Some("Bash".to_owned()), None),
        ]
    );

    // Copilot: `bash` and `local_shell`, from `arguments.command`. `str_replace_editor`'s
    // `command` is an editor operation and `external_tool.requested` names an extension
    // tool, so neither is promoted.
    assert_eq!(
        tool_commands(
            Source::Copilot,
            "copilot-shell-command/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb/events.jsonl",
        ),
        [
            (
                Some("bash".to_owned()),
                Some("cargo fmt --check".to_owned())
            ),
            (None, None), // tool.execution_complete
            (
                Some("local_shell".to_owned()),
                Some("git status --short".to_owned())
            ),
            (Some("str_replace_editor".to_owned()), None),
            (Some("view".to_owned()), None),
            (Some("bash".to_owned()), None), // non-object arguments
            (Some("bash".to_owned()), None), // external_tool.requested
            (None, None),                    // external_tool.completed
        ]
    );

    // Codex: `function_call` named `shell`, from the JSON-string `arguments`, as argv —
    // the same rendering `local_shell_call` has always given `action.command`.
    assert_eq!(
        tool_commands(
            Source::Codex,
            "codex-rollout-0.x/shell-command/c0de0000-0000-4000-8000-000000000077.jsonl",
        ),
        [
            (
                Some("shell".to_owned()),
                Some(r#"["bash","-lc","ls -la"]"#.to_owned())
            ),
            (None, None), // function_call_output
            (Some("update_plan".to_owned()), None),
            (Some("shell".to_owned()), None), // unparseable arguments
            (None, Some(r#"["bash","-lc","echo hi"]"#.to_owned())), // local_shell_call
        ]
    );
}

/// The promotion is additive: it never moves the legacy `key=value` rendering, which is
/// capture's `NormalizedEvent.content` — what summaries are written from and what claim
/// tickets are content-addressed by. Codex `local_shell_call`'s `command` predates the
/// derived-field split and is the one `command` that does render.
#[test]
fn promoted_commands_stay_out_of_the_legacy_rendering() {
    for fixture in FIXTURES {
        for item in stream_fixture(fixture.source, fixture.path) {
            let Ok(Record {
                classification: Classification::Content { events },
                ..
            }) = item
            else {
                continue;
            };
            for event in events {
                let Event::Tool(tool) = &event else { continue };
                for key in &tool.derived {
                    assert!(tool.fields.contains_key(key), "{}: {key}", fixture.path);
                    assert!(
                        !tool.rendered().contains(&format!("{key}=")),
                        "{}: derived {key} leaked into the legacy rendering",
                        fixture.path
                    );
                }
                if tool.event() == Some("local_shell_call") && tool.command().is_some() {
                    assert!(tool.derived.is_empty(), "{}", fixture.path);
                    assert!(tool.rendered().contains("command="), "{}", fixture.path);
                }
            }
        }
    }
}

#[test]
fn unsupported_artifact_set_versions_are_rejected_up_front() {
    for version in [0, 3, u16::MAX] {
        let result = TranscriptStream::new(Source::Codex, version, "".as_bytes());
        assert!(result.is_err(), "version {version} must be rejected");
    }
    // Versions 1 and 2 share one interpreter per source: v2 (munshi issue #23) added optional
    // sidecar artifacts without changing transcript interpretation.
    for version in [1, 2] {
        assert!(TranscriptStream::new(Source::Codex, version, "".as_bytes()).is_ok());
    }
}

#[test]
fn non_object_json_lines_are_typed_ignored_and_blank_lines_are_skipped() {
    let transcript = "5\n\n[]\n{\"type\":\"session_meta\",\"payload\":{}}\n";
    let items = TranscriptStream::new(Source::Codex, 1, transcript.as_bytes())
        .unwrap()
        .collect_records();
    assert_eq!(items.len(), 3);
    for item in &items[..2] {
        let Ok(Record {
            classification: Classification::Ignored { kind },
            ..
        }) = item
        else {
            panic!("non-object JSON must be typed ignored");
        };
        assert_eq!(kind, NON_OBJECT_JSON_KIND);
    }
    // Physical line numbers count blank lines; record ordinals do not.
    let lines: Vec<_> = items
        .iter()
        .map(|item| item.as_ref().unwrap())
        .map(|record| (record.line, record.record))
        .collect();
    assert_eq!(lines, [(1, 1), (3, 2), (4, 3)]);
    let summary = SessionSummary::summarize(&items);
    assert_eq!(summary.ignored_events, 3);
}
