//! Equivalence gate for issue #26 (ADR 0011): for every fixture the legacy
//! `munshi::load_session` normalizer parses successfully, the `munshi-transcript`
//! streaming fold must reproduce the `NormalizedSession` counts, the `started_at` /
//! `updated_at` window, and — event by event, in order — the exact legacy
//! `(kind, content)` strings. This pins the extracted read-time interpreter to today's
//! observable capture behavior before issue #27 rewires the capture path onto it.
//!
//! Since issue #27 rebuilt `load_session` on the stream itself, the count and window
//! assertions are equivalences by construction. The test is kept as the guard for the
//! `legacy_content` contract: what capture persists as `NormalizedEvent` `(kind, content)`
//! must remain byte-identical to what [`Event::kind`]/[`Event::legacy_content`] promise
//! read-time consumers, and every committed fixture must keep streaming with zero
//! malformed records.

use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use munshi::{
    NormalizedSession, SessionReference, SourceKind, load_session, resolve_session_reference,
};
use munshi_transcript::{Classification, Record, SessionSummary, Source, TranscriptStream};

const MAX_SOURCE_BYTES: usize = 1 << 20;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Every committed fixture that `load_session` parses successfully. The malformed
/// (`33333333-*`) and truncated fixtures are excluded because the legacy whole-parse
/// normalizer rejects them outright; the streaming crate's own fixture walk covers those.
const CASES: &[(SourceKind, Source, &str, &str)] = &[
    (
        SourceKind::ClaudeCode,
        Source::ClaudeCode,
        "claude-code-2.1.44/normal/0c1a0de0-0000-4000-8000-000000000001.jsonl",
        "0c1a0de0-0000-4000-8000-000000000001",
    ),
    (
        SourceKind::ClaudeCode,
        Source::ClaudeCode,
        "claude-code-2.1.44/resumed/0c1a0de0-0000-4000-8000-000000000002.jsonl",
        "0c1a0de0-0000-4000-8000-000000000002",
    ),
    (
        SourceKind::ClaudeCode,
        Source::ClaudeCode,
        "claude-code-2.1.44/interrupted/0c1a0de0-0000-4000-8000-000000000003.jsonl",
        "0c1a0de0-0000-4000-8000-000000000003",
    ),
    (
        SourceKind::ClaudeCode,
        Source::ClaudeCode,
        "claude-code-2.1.44/missing-fields/0c1a0de0-0000-4000-8000-000000000004.jsonl",
        "0c1a0de0-0000-4000-8000-000000000004",
    ),
    (
        SourceKind::ClaudeCode,
        Source::ClaudeCode,
        "claude-code-2.1.44/concurrent/0c1a0de0-0000-4000-8000-000000000006.jsonl",
        "0c1a0de0-0000-4000-8000-000000000006",
    ),
    (
        SourceKind::ClaudeCode,
        Source::ClaudeCode,
        "claude-code-2.1.44/concurrent/0c1a0de0-0000-4000-8000-000000000007.jsonl",
        "0c1a0de0-0000-4000-8000-000000000007",
    ),
    (
        SourceKind::ClaudeCode,
        Source::ClaudeCode,
        "claude-code-2.1.205/transcript/0c1a0de0-0000-4000-8000-000000000205.jsonl",
        "0c1a0de0-0000-4000-8000-000000000205",
    ),
    (
        SourceKind::Codex,
        Source::Codex,
        "codex-rollout-0.x/normal/c0de0000-0000-4000-8000-000000000001.jsonl",
        "c0de0000-0000-4000-8000-000000000001",
    ),
    (
        SourceKind::Codex,
        Source::Codex,
        "codex-rollout-0.x/resumed/c0de0000-0000-4000-8000-000000000002.jsonl",
        "c0de0000-0000-4000-8000-000000000002",
    ),
    (
        SourceKind::Codex,
        Source::Codex,
        "codex-rollout-0.x/missing-fields/c0de0000-0000-4000-8000-000000000003.jsonl",
        "c0de0000-0000-4000-8000-000000000003",
    ),
    (
        SourceKind::Codex,
        Source::Codex,
        "codex-rollout-0.x/concurrent/c0de0000-0000-4000-8000-000000000005.jsonl",
        "c0de0000-0000-4000-8000-000000000005",
    ),
    (
        SourceKind::Codex,
        Source::Codex,
        "codex-rollout-0.x/concurrent/c0de0000-0000-4000-8000-000000000006.jsonl",
        "c0de0000-0000-4000-8000-000000000006",
    ),
    (
        SourceKind::Copilot,
        Source::Copilot,
        "manual/copilot/11111111-1111-4111-8111-111111111111/events.jsonl",
        "11111111-1111-4111-8111-111111111111",
    ),
    (
        SourceKind::Copilot,
        Source::Copilot,
        "manual/copilot/22222222-2222-4222-8222-222222222222/events.jsonl",
        "22222222-2222-4222-8222-222222222222",
    ),
    (
        SourceKind::Copilot,
        Source::Copilot,
        "manual/copilot/44444444-4444-4444-8444-444444444444/events.jsonl",
        "44444444-4444-4444-8444-444444444444",
    ),
    (
        SourceKind::Copilot,
        Source::Copilot,
        "manual/copilot/77777777-7777-4777-8777-777777777777/events.jsonl",
        "77777777-7777-4777-8777-777777777777",
    ),
];

fn load_legacy(source: SourceKind, path: &Path, session_id: &str) -> NormalizedSession {
    let resolved = resolve_session_reference(&SessionReference {
        source,
        session_id: Some(session_id.to_owned()),
        events_path: Some(path.to_path_buf()),
        copilot_home: None,
    })
    .unwrap();
    load_session(&resolved, MAX_SOURCE_BYTES).unwrap()
}

fn assert_stream_matches_legacy(source: Source, path: &Path, session: &NormalizedSession) {
    let context = path.display();
    let reader = BufReader::new(File::open(path).unwrap());
    let items = TranscriptStream::new(source, 1, reader)
        .unwrap()
        .collect_records();
    let summary = SessionSummary::summarize(&items);

    assert_eq!(summary.malformed_records, 0, "{context}: record errors");
    assert_eq!(
        summary.user_requests, session.user_requests,
        "{context}: user_requests"
    );
    assert_eq!(
        summary.assistant_messages, session.assistant_messages,
        "{context}: assistant_messages"
    );
    assert_eq!(
        summary.tool_activities, session.tool_activities,
        "{context}: tool_activities"
    );
    assert_eq!(
        summary.ignored_events, session.ignored_events,
        "{context}: ignored_events"
    );
    assert_eq!(
        summary.started_at(),
        session.started_at,
        "{context}: started_at"
    );
    assert_eq!(
        summary.updated_at(),
        session.updated_at,
        "{context}: updated_at"
    );

    let streamed: Vec<(String, String)> = items
        .iter()
        .filter_map(|item| match item {
            Ok(Record {
                classification: Classification::Content { events },
                ..
            }) => Some(events),
            _ => None,
        })
        .flatten()
        .map(|event| (event.kind().to_owned(), event.legacy_content()))
        .collect();
    let legacy: Vec<(String, String)> = session
        .events
        .iter()
        .map(|event| (event.kind.to_owned(), event.content.clone()))
        .collect();
    assert_eq!(streamed, legacy, "{context}: ordered (kind, content) pairs");
}

#[test]
fn stream_fold_matches_legacy_normalization_for_every_parseable_fixture() {
    for (source_kind, source, relative, session_id) in CASES {
        let path = fixture_root().join(relative);
        let session = load_legacy(*source_kind, &path, session_id);
        assert_stream_matches_legacy(*source, &path, &session);
    }
}

#[test]
fn stream_fold_matches_legacy_normalization_for_the_synthetic_envelope() {
    // The synthetic-envelope fixture is not named `events.jsonl`, which Copilot session
    // resolution requires, so stage a copy under target/ in the expected layout.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-adapter-artifacts");
    fs::create_dir_all(&root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("envelope-")
        .tempdir_in(root)
        .unwrap();
    let source_path = fixture_root().join("copilot-1.0.70/transcript/synthetic-envelope.jsonl");
    let session_dir = directory.path().join("synthetic-envelope");
    fs::create_dir_all(&session_dir).unwrap();
    let staged = session_dir.join("events.jsonl");
    fs::copy(&source_path, &staged).unwrap();

    let session = load_legacy(SourceKind::Copilot, &staged, "synthetic-envelope");
    assert_eq!(session.ignored_events, 11);
    assert_stream_matches_legacy(Source::Copilot, &staged, &session);
}
