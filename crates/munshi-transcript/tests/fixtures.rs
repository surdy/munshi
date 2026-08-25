//! Fixture conformance walk for the lossless streaming parser (issue #26, ADR 0011).
//!
//! Every sanitized adapter fixture must stream-parse with every non-empty line accounted
//! for exactly once. Well-formed fixtures yield zero `Unknown` records and zero record
//! errors; the deliberately-negative fixtures (a malformed line, truncated trailing
//! records, a synthetic future event type) must surface as per-record `Unknown`/error
//! items without aborting the stream.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use munshi_transcript::{
    AssistantMeta, Classification, Compaction, CompactionPhase, Event, NON_OBJECT_JSON_KIND,
    Record, RecordError, SessionSummary, Source, TokenUsage, TranscriptStream,
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
    // Issue #77: the assistant key sets a model and token usage are read from, old and new,
    // including a message split across two records and figures that must not be read.
    well_formed(
        Source::ClaudeCode,
        "claude-code-assistant-usage/transcript/0c1a0de0-0000-4000-8000-000000077002.jsonl",
    ),
    // Issue #77: the compaction markers each harness writes, the near-miss `system`
    // subtypes that must not be read as one, and figures that must not be read at all.
    well_formed(
        Source::ClaudeCode,
        "claude-code-compaction/transcript/0c1a0de0-0000-4000-8000-000000077003.jsonl",
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
        "copilot-assistant-usage/cccccccc-cccc-4ccc-8ccc-cccccccccccc/events.jsonl",
    ),
    well_formed(
        Source::Copilot,
        "copilot-compaction/dddddddd-dddd-4ddd-8ddd-dddddddddddd/events.jsonl",
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

/// Every content event a fixture yields, in order.
fn content_events(source: Source, relative: &str) -> Vec<Event> {
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
        .collect()
}

/// Every tool event in a fixture, as `(name, promoted command)`.
fn tool_commands(source: Source, relative: &str) -> Vec<(Option<String>, Option<String>)> {
    content_events(source, relative)
        .into_iter()
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
                }
                // Exact, not substring: the rendering must equal the join over the
                // non-derived fields, so a derived key leaking in (or a legacy field
                // dropping out) fails even when a field's *value* contains `key=` text.
                let expected = tool
                    .fields
                    .iter()
                    .filter(|(key, _)| !tool.derived.contains(*key))
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                assert_eq!(
                    tool.rendered(),
                    expected,
                    "{}: derived fields leaked into the legacy rendering",
                    fixture.path
                );
                if tool.event() == Some("local_shell_call") && tool.command().is_some() {
                    assert!(tool.derived.is_empty(), "{}", fixture.path);
                    assert!(tool.rendered().contains("command="), "{}", fixture.path);
                }
            }
        }
    }
}

const CLAUDE_USAGE_FIXTURE: &str =
    "claude-code-assistant-usage/transcript/0c1a0de0-0000-4000-8000-000000077002.jsonl";
const COPILOT_USAGE_FIXTURE: &str =
    "copilot-assistant-usage/cccccccc-cccc-4ccc-8ccc-cccccccccccc/events.jsonl";

/// Every record of a fixture, as `(the kinds of the events it yielded, its promoted meta)`.
/// Records are the unit here because the meta is: a record's usage does not depend on what
/// its content classified as.
fn record_metas(source: Source, relative: &str) -> Vec<(Vec<&'static str>, Option<AssistantMeta>)> {
    stream_fixture(source, relative)
        .iter()
        .filter_map(|item| item.as_ref().ok())
        .map(|record| {
            let kinds = match &record.classification {
                Classification::Content { events } => events.iter().map(Event::kind).collect(),
                _ => Vec::new(),
            };
            (kinds, record.assistant_meta.as_deref().cloned())
        })
        .collect()
}

/// The expected meta of a record naming both a model and a message id.
fn meta(model: &str, message_id: &str, usage: Option<TokenUsage>) -> Option<AssistantMeta> {
    Some(AssistantMeta {
        model: Some(model.to_owned()),
        usage,
        message_id: Some(message_id.to_owned()),
    })
}

/// Issue #77: a claude-code record carries the model and token figures it records — every
/// figure the pinned key set names, no figure it does not — whatever its content yields.
#[test]
fn claude_records_carry_the_model_and_usage_they_record() {
    // `msg_split`'s two records are one API message, each repeating that message's usage
    // verbatim; `msg_toolonly` and `msg_thinking` yield no assistant event at all. The
    // split message predates `cache_creation`, so its buckets are absent while its total
    // is present — the shape that makes the two independent readings.
    let split = Some(TokenUsage {
        input_tokens: Some(64),
        output_tokens: Some(16),
        cache_creation_input_tokens: Some(0),
        cache_read_input_tokens: Some(4096),
        thinking_tokens: Some(0),
        service_tier: Some("standard".to_owned()),
        speed: Some("standard".to_owned()),
        inference_geo: Some("not_available".to_owned()),
        ..TokenUsage::default()
    });
    assert_eq!(
        record_metas(Source::ClaudeCode, CLAUDE_USAGE_FIXTURE),
        [
            // A user record is not an assistant record, whatever it carries.
            (vec!["user"], None),
            // The modern key set, whole: both cache tiers beside their total, and the
            // `server_tool_use` and `iterations` figures left in the raw record. The write
            // went to the 5-minute tier here and to the 1-hour tier in the next record, so
            // a transposed bucket key cannot pass both rows.
            (
                vec!["assistant"],
                meta(
                    "claude-synthetic-opus",
                    "msg_full",
                    Some(TokenUsage {
                        input_tokens: Some(120),
                        output_tokens: Some(48),
                        cache_creation_input_tokens: Some(1024),
                        cache_5m_input_tokens: Some(1024),
                        cache_1h_input_tokens: Some(0),
                        cache_read_input_tokens: Some(8192),
                        thinking_tokens: Some(32),
                        service_tier: Some("standard".to_owned()),
                        speed: Some("fast".to_owned()),
                        inference_geo: Some("us".to_owned()),
                    }),
                ),
            ),
            // The shape the archive actually holds: every cache write on the 1-hour tier.
            (
                vec!["assistant"],
                meta(
                    "claude-synthetic-opus",
                    "msg_buckets",
                    Some(TokenUsage {
                        input_tokens: Some(80),
                        output_tokens: Some(10),
                        cache_creation_input_tokens: Some(4096),
                        cache_5m_input_tokens: Some(0),
                        cache_1h_input_tokens: Some(4096),
                        cache_read_input_tokens: Some(0),
                        service_tier: Some("standard".to_owned()),
                        speed: Some("standard".to_owned()),
                        inference_geo: Some("not_available".to_owned()),
                        ..TokenUsage::default()
                    }),
                ),
            ),
            // The one disagreement the archive holds, reproduced: a total of 0 against a
            // 1-hour bucket of 2,277. Both are promoted exactly as recorded — reconciling
            // them would be inventing a figure the source never wrote.
            (
                vec!["assistant"],
                meta(
                    "claude-synthetic-opus",
                    "msg_bucket_drift",
                    Some(TokenUsage {
                        input_tokens: Some(40),
                        output_tokens: Some(20),
                        cache_creation_input_tokens: Some(0),
                        cache_5m_input_tokens: Some(0),
                        cache_1h_input_tokens: Some(2277),
                        cache_read_input_tokens: Some(0),
                        service_tier: Some("standard".to_owned()),
                        ..TokenUsage::default()
                    }),
                ),
            ),
            (
                vec!["assistant"],
                meta("claude-synthetic-opus", "msg_split", split.clone()),
            ),
            (
                vec!["assistant", "tool", "assistant"],
                meta("claude-synthetic-opus", "msg_split", split),
            ),
            // A message that only called a tool: no assistant event, and 8 output tokens
            // that a fold over assistant events would never see.
            (
                vec!["tool"],
                meta(
                    "claude-synthetic-opus",
                    "msg_toolonly",
                    Some(TokenUsage {
                        input_tokens: Some(32),
                        output_tokens: Some(8),
                        service_tier: Some("standard".to_owned()),
                        ..TokenUsage::default()
                    }),
                ),
            ),
            // A thinking-only message: recognized, empty, and billed for 64 output tokens,
            // all of them thinking.
            (
                Vec::new(),
                meta(
                    "claude-synthetic-opus",
                    "msg_thinking",
                    Some(TokenUsage {
                        input_tokens: Some(16),
                        output_tokens: Some(64),
                        thinking_tokens: Some(64),
                        ..TokenUsage::default()
                    }),
                ),
            ),
            // The tool result: a user record, so no meta, though it yields a tool event.
            (vec!["tool"], None),
            // `<synthetic>` is a model id like any other: passed through verbatim, never
            // resolved. Its null tier/speed/geo/details keys read as absent, while its zero
            // counts are real counts.
            (
                vec!["assistant"],
                meta(
                    "<synthetic>",
                    "msg_synthetic",
                    Some(TokenUsage {
                        input_tokens: Some(0),
                        output_tokens: Some(5),
                        cache_creation_input_tokens: Some(0),
                        cache_read_input_tokens: Some(0),
                        ..TokenUsage::default()
                    }),
                ),
            ),
            // An older key set predating the cache, thinking, tier, speed, and geo keys.
            (
                vec!["assistant"],
                meta(
                    "claude-synthetic-legacy",
                    "msg_old",
                    Some(TokenUsage {
                        input_tokens: Some(7),
                        output_tokens: Some(3),
                        ..TokenUsage::default()
                    }),
                ),
            ),
            // A stringified count, a negative, a float, a null, and a `cache_creation`
            // whose buckets are a string and a null: none is read as a number, leaving the
            // usage carrying only the tier it did record. A present-but-unreadable bucket
            // reads exactly like an absent one, since neither is a figure.
            (
                vec!["assistant"],
                meta(
                    "claude-synthetic-legacy",
                    "msg_unreadable",
                    Some(TokenUsage {
                        service_tier: Some("standard".to_owned()),
                        ..TokenUsage::default()
                    }),
                ),
            ),
            // A record naming no model, no id, and no usage claims nothing at all.
            (vec!["assistant"], None),
        ]
    );

    // The pinned 2.1.44 fixtures predate the promotion and were not touched by it: the same
    // reading gives them the old two-key usage, inventing none of the rest — including for
    // `msg_a2`, whose content is a lone `tool_use`.
    let old_keyset = Some(TokenUsage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        ..TokenUsage::default()
    });
    assert_eq!(
        record_metas(
            Source::ClaudeCode,
            "claude-code-2.1.44/normal/0c1a0de0-0000-4000-8000-000000000001.jsonl",
        ),
        [
            (vec!["user"], None),
            (
                vec!["assistant"],
                meta("claude-synthetic", "msg_a1", old_keyset.clone()),
            ),
            (
                vec!["tool"],
                meta("claude-synthetic", "msg_a2", old_keyset.clone()),
            ),
            (vec!["tool"], None),
            (
                vec!["assistant"],
                meta("claude-synthetic", "msg_a3", old_keyset),
            ),
        ]
    );
}

/// The gap that keeping the meta on the record closes, priced on the fixture: a fold over
/// assistant events cannot see a tool-calling or thinking-only message, and a fold that
/// forgets to deduplicate charges a split message twice.
#[test]
fn every_billed_record_is_reachable_and_summing_records_double_counts() {
    let rows = record_metas(Source::ClaudeCode, CLAUDE_USAGE_FIXTURE);
    let output = |meta: &Option<AssistantMeta>| {
        meta.as_ref()
            .and_then(|meta| meta.usage.as_ref())
            .and_then(|usage| usage.output_tokens)
            .unwrap_or(0)
    };

    // What a consumer must compute: one usage per message id.
    let mut per_message: BTreeMap<&str, u64> = BTreeMap::new();
    for (_, meta) in &rows {
        if let Some(id) = meta.as_ref().and_then(|meta| meta.message_id.as_deref()) {
            per_message.insert(id, output(meta));
        }
    }
    assert_eq!(per_message.values().sum::<u64>(), 174);

    // Summing records instead double-charges `msg_split`, which two records both carry.
    let per_record: u64 = rows.iter().map(|(_, meta)| output(meta)).sum();
    assert_eq!(per_record, 190);

    // And what the same fold would have reached had the meta hung off assistant events:
    // nothing of `msg_toolonly` (8) or `msg_thinking` (64), the shapes that dominate a real
    // agentic session.
    let reachable_from_assistant_events: u64 = rows
        .iter()
        .filter(|(kinds, _)| kinds.contains(&"assistant"))
        .filter_map(|(_, meta)| meta.as_ref().and_then(|meta| meta.message_id.as_deref()))
        .collect::<BTreeSet<_>>()
        .iter()
        .filter_map(|id| per_message.get(id))
        .sum();
    assert_eq!(reachable_from_assistant_events, 102);
}

/// The cache tiers are a second reading of the cache write, not a re-derivation of the
/// total: across the fixture the two disagree by exactly the drift the archive holds, and
/// the crate reports both rather than choosing between them.
#[test]
fn cache_tiers_are_promoted_beside_their_total_and_never_reconciled() {
    let mut totals = 0;
    let mut tiers = 0;
    let mut with_tiers = 0;
    let mut total_without_tiers = 0;
    for (_, meta) in record_metas(Source::ClaudeCode, CLAUDE_USAGE_FIXTURE) {
        let Some(usage) = meta.and_then(|meta| meta.usage) else {
            continue;
        };
        totals += usage.cache_creation_input_tokens.unwrap_or(0);
        match (usage.cache_5m_input_tokens, usage.cache_1h_input_tokens) {
            (None, None) => {
                total_without_tiers += usize::from(usage.cache_creation_input_tokens.is_some());
            }
            (five_minute, one_hour) => {
                with_tiers += 1;
                tiers += five_minute.unwrap_or(0) + one_hour.unwrap_or(0);
            }
        }
    }
    // Three records split their write by tier; three state a total with no split at all —
    // what the key set looked like before `cache_creation` existed, and what an unreadable
    // `cache_creation` degrades to.
    assert_eq!((with_tiers, total_without_tiers), (3, 3));
    // 1024 + 4096 + 0 against 1024 + 4096 + 2277: `msg_bucket_drift`'s total reads 0 while
    // its 1-hour bucket reads 2,277, exactly as one archived message does.
    assert_eq!((totals, tiers), (5120, 7397));
    assert_eq!(tiers, totals + 2277);
}

/// Copilot records one output-token count per message and no input, cache, thinking, tier,
/// speed, or geo figure, so its usage carries that count alone. Its tool records carry no
/// usage of their own and are not scraped for one.
#[test]
fn copilot_records_carry_the_model_and_output_token_count() {
    assert_eq!(
        record_metas(Source::Copilot, COPILOT_USAGE_FIXTURE),
        [
            (Vec::new(), None), // session.start
            (vec!["user"], None),
            (Vec::new(), None), // session.model_change: a session fact, not a message's
            (
                vec!["assistant"],
                meta(
                    "claude-synthetic-opus",
                    "cccccccc-0000-4000-8000-0000000000a1",
                    Some(TokenUsage {
                        output_tokens: Some(128),
                        ..TokenUsage::default()
                    }),
                ),
            ),
            // The shape that record-level meta exists for: a turn that only called tools,
            // which the CLI records as an `assistant.message` with blank `content` and a
            // separate `tool.execution_*` record for the call itself. It classifies
            // `Empty` — the `ignored_kinds` assertion below pins that it is not `Ignored` —
            // so it yields no event of any kind, and its 96 output tokens are reachable
            // only because the meta hangs off the record.
            (
                Vec::new(),
                meta(
                    "claude-synthetic-opus",
                    "cccccccc-0000-4000-8000-0000000000a5",
                    Some(TokenUsage {
                        output_tokens: Some(96),
                        ..TokenUsage::default()
                    }),
                ),
            ),
            // No `outputTokens` key: the model and id are still promoted, the usage is not
            // invented as zero.
            (
                vec!["assistant"],
                meta(
                    "claude-synthetic-opus",
                    "cccccccc-0000-4000-8000-0000000000a2",
                    None,
                ),
            ),
            (
                vec!["assistant"],
                Some(AssistantMeta {
                    model: None,
                    usage: None,
                    message_id: Some("cccccccc-0000-4000-8000-0000000000a3".to_owned()),
                }),
            ),
            // A stringified count is not read as a number.
            (
                vec!["assistant"],
                meta(
                    "claude-synthetic-opus",
                    "cccccccc-0000-4000-8000-0000000000a4",
                    None,
                ),
            ),
            // session.shutdown, whose `modelMetrics` rolls the whole session's tokens up:
            // bookkeeping, because it attributes to no message.
            (Vec::new(), None),
        ]
    );

    let ignored_kinds: Vec<_> = stream_fixture(Source::Copilot, COPILOT_USAGE_FIXTURE)
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Ignored { kind },
                ..
            }) => Some(kind.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ignored_kinds,
        ["session.start", "session.model_change", "session.shutdown"]
    );
}

/// The negative half: a source that records no per-message model or usage anywhere.
#[test]
fn codex_records_carry_no_meta() {
    // Rollout `message` payloads name neither model nor usage — the model they run under
    // sits on the turn-level `turn_context` record, which describes a turn and not a
    // message — so nothing in a Codex transcript is promoted.
    for fixture in FIXTURES {
        if fixture.source != Source::Codex {
            continue;
        }
        let rows = record_metas(fixture.source, fixture.path);
        assert!(!rows.is_empty(), "{}", fixture.path);
        assert!(
            rows.iter().all(|(_, meta)| meta.is_none()),
            "{}",
            fixture.path
        );
    }
}

const CLAUDE_COMPACTION_FIXTURE: &str =
    "claude-code-compaction/transcript/0c1a0de0-0000-4000-8000-000000077003.jsonl";
const COPILOT_COMPACTION_FIXTURE: &str =
    "copilot-compaction/dddddddd-dddd-4ddd-8ddd-dddddddddddd/events.jsonl";

/// Every record of a fixture as `(classification tag, compaction)`, so a test can pin what a
/// record promotes *and* that promoting it left the record in the census it was already in.
fn record_compactions(source: Source, relative: &str) -> Vec<(String, Option<Compaction>)> {
    stream_fixture(source, relative)
        .into_iter()
        .map(|item| {
            // A malformed line promotes nothing, and the deliberately-truncated fixtures
            // carry one: an error item is a row like any other, never an abort.
            let Ok(record) = item else {
                return ("error".to_owned(), None);
            };
            let tag = match &record.classification {
                Classification::Content { events } => format!("content:{}", events.len()),
                Classification::Empty => "empty".to_owned(),
                Classification::Ignored { kind } => format!("ignored:{kind}"),
                Classification::Unknown { .. } => "unknown".to_owned(),
            };
            (tag, record.compaction.map(|compaction| *compaction))
        })
        .collect()
}

/// A compaction with nothing but a phase: what a marker whose figures are unreadable — or
/// were never written — still states, because the record's existence is the claim.
fn marker(phase: CompactionPhase) -> Compaction {
    Compaction {
        phase,
        trigger: None,
        succeeded: None,
        pre_tokens: None,
        post_tokens: None,
        system_tokens: None,
        conversation_tokens: None,
        tool_definition_tokens: None,
        token_limit: None,
    }
}

#[test]
fn claude_compact_boundaries_are_read_and_neighbouring_system_records_are_not() {
    let rows = record_compactions(Source::ClaudeCode, CLAUDE_COMPACTION_FIXTURE);
    let compactions: Vec<_> = rows
        .iter()
        .map(|(tag, compaction)| (tag.as_str(), compaction.clone()))
        .collect();

    assert_eq!(
        compactions,
        vec![
            ("content:1", None),
            ("content:1", None),
            // A boundary states its trigger and both sizes. `cumulativeDroppedTokens`,
            // `durationMs` and the preserved-message uuids beside them stay in the record.
            (
                "ignored:system",
                Some(Compaction {
                    trigger: Some("manual".to_owned()),
                    pre_tokens: Some(214864),
                    post_tokens: Some(8156),
                    ..marker(CompactionPhase::Complete)
                })
            ),
            // The post-compaction summary is a user event, exactly as it always was, and
            // carries no compaction of its own: one compaction, one marker.
            ("content:1", None),
            ("content:1", None),
            // The trigger passes through verbatim, so an `auto` boundary is not folded into
            // the `manual` the archive happens to hold only examples of.
            (
                "ignored:system",
                Some(Compaction {
                    trigger: Some("auto".to_owned()),
                    pre_tokens: Some(339462),
                    post_tokens: Some(10705),
                    ..marker(CompactionPhase::Complete)
                })
            ),
            // A different `system` subtype carrying a `compactMetadata`-shaped key is not a
            // compaction: the subtype certifies the meaning, never the key name.
            ("ignored:system", None),
            // A `system` record with no subtype at all.
            ("ignored:system", None),
            // A real boundary whose figures are a string, a negative and a float, and whose
            // trigger is blank: each is left absent rather than guessed, and the boundary
            // still reports that a compaction happened.
            ("ignored:system", Some(marker(CompactionPhase::Complete))),
            // `compactMetadata` that is not an object, and a boundary with none at all.
            ("ignored:system", Some(marker(CompactionPhase::Complete))),
            ("ignored:system", Some(marker(CompactionPhase::Complete))),
            ("content:1", None),
        ]
    );
}

#[test]
fn copilot_compaction_markers_are_read_per_phase() {
    let rows = record_compactions(Source::Copilot, COPILOT_COMPACTION_FIXTURE);
    let compactions: Vec<_> = rows
        .iter()
        .map(|(tag, compaction)| (tag.as_str(), compaction.clone()))
        .collect();

    assert_eq!(
        compactions,
        vec![
            ("ignored:session.start", None),
            ("content:1", None),
            // The older start shape: the three-way breakdown and nothing else. Its total is
            // absent rather than summed, because the sum is not a figure the source wrote.
            (
                "ignored:session.compaction_start",
                Some(Compaction {
                    system_tokens: Some(19138),
                    conversation_tokens: Some(129018),
                    tool_definition_tokens: Some(12783),
                    ..marker(CompactionPhase::Start)
                })
            ),
            // The completion states the total and the outcome. `summaryContent`,
            // `compactionTokensUsed`, the checkpoint and the request ids all stay in the
            // record.
            (
                "ignored:session.compaction_complete",
                Some(Compaction {
                    succeeded: Some(true),
                    pre_tokens: Some(160939),
                    ..marker(CompactionPhase::Complete)
                })
            ),
            // The newer start shape names the total itself, plus the window and the trigger.
            (
                "ignored:session.compaction_start",
                Some(Compaction {
                    trigger: Some("threshold".to_owned()),
                    pre_tokens: Some(218150),
                    system_tokens: Some(11422),
                    conversation_tokens: Some(188577),
                    tool_definition_tokens: Some(18151),
                    token_limit: Some(272000),
                    ..marker(CompactionPhase::Start)
                })
            ),
            (
                "ignored:session.compaction_complete",
                Some(Compaction {
                    trigger: Some("threshold".to_owned()),
                    succeeded: Some(true),
                    pre_tokens: Some(218150),
                    post_tokens: Some(34994),
                    token_limit: Some(272000),
                    ..marker(CompactionPhase::Complete)
                })
            ),
            (
                "ignored:session.compaction_start",
                Some(Compaction {
                    trigger: Some("threshold".to_owned()),
                    pre_tokens: Some(160271),
                    system_tokens: Some(9835),
                    conversation_tokens: Some(132262),
                    tool_definition_tokens: Some(18174),
                    token_limit: Some(200000),
                    ..marker(CompactionPhase::Start)
                })
            ),
            // A failed compaction: the attempt happened and states its window, and no size
            // figure is invented for a compaction that never ran.
            (
                "ignored:session.compaction_complete",
                Some(Compaction {
                    trigger: Some("threshold".to_owned()),
                    succeeded: Some(false),
                    token_limit: Some(200000),
                    ..marker(CompactionPhase::Complete)
                })
            ),
            (
                "ignored:session.compaction_start",
                Some(Compaction {
                    system_tokens: Some(10271),
                    conversation_tokens: Some(403967),
                    tool_definition_tokens: Some(16516),
                    ..marker(CompactionPhase::Start)
                })
            ),
            // The trap this promotion is keyed per phase to avoid: a completion spelling
            // `systemTokens` / `conversationTokens` / `toolDefinitionsTokens` for the context
            // it was *left* with. Reading them here would file 11,189 post-compaction
            // conversation tokens as the 403,971 that went in.
            (
                "ignored:session.compaction_complete",
                Some(Compaction {
                    succeeded: Some(true),
                    pre_tokens: Some(403971),
                    post_tokens: Some(11193),
                    ..marker(CompactionPhase::Complete)
                })
            ),
            // Figures that are not counts, and a blank trigger.
            (
                "ignored:session.compaction_start",
                Some(marker(CompactionPhase::Start))
            ),
            // `data` that is not an object at all: the marker still marks a compaction.
            (
                "ignored:session.compaction_complete",
                Some(marker(CompactionPhase::Complete))
            ),
            // A neighbouring bookkeeping kind that happens to carry compaction-shaped keys
            // is not a compaction marker.
            ("ignored:session.context_changed", None),
            ("content:1", None),
            ("ignored:session.shutdown", None),
        ]
    );
}

/// Copilot writes two records per compaction and Claude Code writes one, so the only fold
/// that means the same thing in both harnesses counts completions. Over these two fixtures a
/// record fold says 15 compactions and a completion fold says 10 — and the true figure is 10.
#[test]
fn counting_compaction_records_double_counts_copilot_and_counting_completions_does_not() {
    let counts = |source, relative| {
        let rows = record_compactions(source, relative);
        let markers: Vec<_> = rows
            .into_iter()
            .filter_map(|(_, compaction)| compaction)
            .collect();
        let completions = markers
            .iter()
            .filter(|compaction| compaction.phase == CompactionPhase::Complete)
            .count();
        (markers.len(), completions)
    };

    // Claude Code: one record per compaction, so the two folds agree.
    assert_eq!(
        counts(Source::ClaudeCode, CLAUDE_COMPACTION_FIXTURE),
        (5, 5)
    );
    // Copilot: strictly alternating starts and completions, so a record fold doubles.
    assert_eq!(counts(Source::Copilot, COPILOT_COMPACTION_FIXTURE), (10, 5));
}

#[test]
fn every_compaction_marker_stays_in_the_census_it_was_already_in() {
    // The whole byte-identity argument for this promotion, stated as a test: nothing it
    // reads is content, so no record it touches can gain, lose or change an event. A future
    // change that typed a marker as an event would fail here before it reached the archive.
    //
    // Walked over every fixture rather than the two compaction ones, because the record this
    // design most plausibly grows to read is Claude Code's `isCompactSummary` user message —
    // which is content, appears in fixtures of its own, and is declined on purpose. Reading
    // it would have to fail somewhere, and this is where.
    let mut markers = 0;
    for fixture in FIXTURES {
        for (tag, compaction) in record_compactions(fixture.source, fixture.path) {
            if compaction.is_some() {
                markers += 1;
                assert!(
                    tag.starts_with("ignored:"),
                    "{}: a compaction marker left its census as {tag}",
                    fixture.path
                );
            }
        }
    }
    assert!(markers > 0, "the walk must find a marker to prove anything");
}

#[test]
fn codex_records_carry_no_compaction() {
    // The rollout schema names a `compacted` metadata record, and the archive holds zero
    // Codex sessions, so this crate has never seen one of its payloads. Nothing is read for
    // it — including from the `compacted` record the resumed fixture carries, whose payload
    // is a message and no figure at all.
    let mut compacted_records = 0;
    for fixture in FIXTURES {
        if fixture.source != Source::Codex {
            continue;
        }
        let rows = record_compactions(fixture.source, fixture.path);
        assert!(!rows.is_empty(), "{}", fixture.path);
        compacted_records += rows
            .iter()
            .filter(|(tag, _)| tag == "ignored:compacted")
            .count();
        assert!(
            rows.iter().all(|(_, compaction)| compaction.is_none()),
            "{}",
            fixture.path
        );
    }
    assert!(
        compacted_records > 0,
        "a fixture must carry a `compacted` record for this to prove anything"
    );
}

/// The promotion is additive: what it reads may not change what an event renders as. Strip
/// the promoted keys out of every record of every fixture and the whole stream — record
/// accounting and the ordered `(kind, legacy content)` pairs alike — must come back
/// identical, because that rendering is capture's `NormalizedEvent.content`: what summaries
/// are written from and what an oversized event's claim ticket is content-addressed by.
#[test]
fn nothing_the_promotion_reads_reaches_the_legacy_rendering() {
    // `serde_json` is the crate's own dependency, so a fixture record can be edited here
    // and re-streamed rather than only read.
    let mut stripped_records = 0;
    let mut stripped_compactions = 0;
    for fixture in FIXTURES {
        let bytes = fs::read(fixture_root().join(fixture.path)).unwrap();
        let mut stripped = String::new();
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(line) else {
                // Malformed lines are left exactly as they are, errors and all.
                stripped.push_str(&String::from_utf8_lossy(line));
                stripped.push('\n');
                continue;
            };
            // Every key `AssistantMeta` is read from, on the record types it reads them
            // from — `message.id` included, which the classification never touches, so
            // removing it must not move a single byte of the rendering. Nested keys need
            // no entry of their own: deleting `message.usage` takes `cache_creation`'s
            // per-tier buckets and `output_tokens_details` with it.
            //
            // Two deliberate exceptions, both keys this promotion does not read: Copilot's
            // `skill.invoked` renders a `model` card field of its own, and Copilot's
            // `messageId` is what makes an `assistant.message` content at all. Stripping
            // either would change the rendering for reasons that have nothing to do with
            // the promotion, proving nothing.
            //
            // The compaction promotion (issue #77) strips alongside it, and needs no
            // exception at all: `compactMetadata` is nested, so deleting it takes every
            // figure with it, and the Copilot markers lose their whole `data` object, since
            // the promotion reads keys straight off it and the classification reads nothing
            // there. `subtype` goes too, and that is the sharpest case in the walk — it is
            // the key that tells a `compact_boundary` from every other `system` record, yet
            // a `system` record classifies `Ignored { kind: "system" }` whatever its subtype
            // says, so its removal must not move a byte either.
            let stripped_keys = match value.get("type").and_then(serde_json::Value::as_str) {
                Some("assistant") => Some(("message", ["id", "model", "usage"].as_slice())),
                Some("assistant.message") => Some(("data", ["model", "outputTokens"].as_slice())),
                _ => None,
            };
            if let Some((parent, keys)) = stripped_keys
                && let Some(object) = value
                    .get_mut(parent)
                    .and_then(|value| value.as_object_mut())
            {
                for key in keys {
                    stripped_records += usize::from(object.remove(*key).is_some());
                }
            }
            if let Some(object) = value.as_object_mut() {
                // Counted only where the record really is a marker, so the deliberate decoys
                // — a `system` record of another subtype carrying a `compactMetadata`-shaped
                // key — cannot keep the counter above zero while every real marker has
                // quietly stopped being stripped.
                let boundary = object.get("subtype").and_then(serde_json::Value::as_str)
                    == Some("compact_boundary");
                let (keys, marker): (&[&str], bool) =
                    match object.get("type").and_then(serde_json::Value::as_str) {
                        Some("system") => (["compactMetadata", "subtype"].as_slice(), boundary),
                        Some("session.compaction_start" | "session.compaction_complete") => {
                            (["data"].as_slice(), true)
                        }
                        _ => (&[], false),
                    };
                let mut removed = 0;
                for key in keys {
                    removed += usize::from(object.remove(*key).is_some());
                }
                if marker && removed > 0 {
                    stripped_compactions += 1;
                }
            }
            stripped.push_str(&value.to_string());
            stripped.push('\n');
        }

        let before = legacy_shape(stream_fixture(fixture.source, fixture.path));
        let after = legacy_shape(
            TranscriptStream::new(fixture.source, 1, stripped.as_bytes())
                .unwrap()
                .collect_records(),
        );
        assert_eq!(before, after, "{}", fixture.path);
    }
    // Counted per promotion, so a walk that quietly stopped stripping one of them cannot
    // keep passing on the strength of the other.
    assert!(
        stripped_records > 0,
        "the walk must strip something to prove anything"
    );
    assert!(
        stripped_compactions > 0,
        "the walk must strip a compaction marker to prove anything about that promotion"
    );
}

/// A stream reduced to what the legacy contract promises: per record, its classification
/// and the ordered `(kind, content)` pairs of the events it yielded.
fn legacy_shape(items: Vec<Result<Record, RecordError>>) -> Vec<(String, Vec<(String, String)>)> {
    items
        .iter()
        .map(|item| match item {
            Err(error) => (format!("error:{}", error.line()), Vec::new()),
            Ok(record) => {
                let class = match &record.classification {
                    Classification::Content { .. } => "content".to_owned(),
                    Classification::Empty => "empty".to_owned(),
                    Classification::Ignored { kind } => format!("ignored:{kind}"),
                    Classification::Unknown { raw } => format!("unknown:{raw}"),
                };
                let events = match &record.classification {
                    Classification::Content { events } => events
                        .iter()
                        .map(|event| (event.kind().to_owned(), event.legacy_content()))
                        .collect(),
                    _ => Vec::new(),
                };
                (class, events)
            }
        })
        .collect()
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
