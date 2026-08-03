//! Adapter conformance and shared-pipeline integration tests for the Claude Code and
//! Codex source adapters. Fixtures are synthetic/sanitized (see `docs/harness-adapters.md`);
//! no private transcript content is used.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use munshi::{
    ArchiveConfig, ArchiveOutcome, CompletionReason, HookResult, NormalizedEvent,
    NormalizedSession, ProjectIdentity, SUMMARIZER_EXHAUST_DIAGNOSTIC, SessionReference,
    SourceError, SourceHomes, SourceKind, StateStore, WorkerContext, archive_session,
    build_summary_input, inspect_project, load_session, parse_archive_markdown,
    recorded_project_identity, resolve_session_reference, run_archive_worker_for_source,
    validate_transcript_envelope,
};
use rusqlite::{Connection, params};
use serde_json::json;
use tempfile::TempDir;

const CLAUDE_NORMAL: &str = "0c1a0de0-0000-4000-8000-000000000001";
const CLAUDE_RESUMED: &str = "0c1a0de0-0000-4000-8000-000000000002";
const CLAUDE_INTERRUPTED: &str = "0c1a0de0-0000-4000-8000-000000000003";
const CLAUDE_MISSING: &str = "0c1a0de0-0000-4000-8000-000000000004";
const CLAUDE_TRUNCATED: &str = "0c1a0de0-0000-4000-8000-000000000005";
const CLAUDE_CONCURRENT_A: &str = "0c1a0de0-0000-4000-8000-000000000006";
const CLAUDE_BOOKKEEPING: &str = "0c1a0de0-0000-4000-8000-000000000205";

const CODEX_NORMAL: &str = "c0de0000-0000-4000-8000-000000000001";
const CODEX_RESUMED: &str = "c0de0000-0000-4000-8000-000000000002";
const CODEX_MISSING: &str = "c0de0000-0000-4000-8000-000000000003";
const CODEX_TRUNCATED: &str = "c0de0000-0000-4000-8000-000000000004";

// ---------------------------------------------------------------------------
// Normalizer conformance
// ---------------------------------------------------------------------------

#[test]
fn claude_normal_normalizes_to_the_shared_model() {
    let session = load_fixture(
        SourceKind::ClaudeCode,
        "claude-code-2.1.44",
        "normal",
        CLAUDE_NORMAL,
    );
    assert_eq!(session.source, SourceKind::ClaudeCode);
    assert_eq!(session.session_id, CLAUDE_NORMAL);
    assert_eq!(session.user_requests, 1);
    assert_eq!(session.assistant_messages, 2);
    assert_eq!(session.tool_activities, 2);
    assert!(session.is_archive_worthy());
    assert!(session.started_at.is_some() && session.updated_at.is_some());
    let kinds: Vec<_> = session.events.iter().map(|event| event.kind).collect();
    assert_eq!(kinds, ["user", "assistant", "tool", "tool", "assistant"]);
}

#[test]
fn claude_2_1_205_bookkeeping_records_stay_ignored_metadata() {
    // Claude Code 2.1.205 interleaves ai-title, attachment, last-prompt, mode,
    // and queue-operation records between messages. The pinned 2.1.44 envelope
    // is unchanged for user/assistant records; the new bookkeeping types must
    // degrade to ignored metadata (docs/phase-0-claude-code-findings.md).
    let session = load_fixture(
        SourceKind::ClaudeCode,
        "claude-code-2.1.205",
        "transcript",
        CLAUDE_BOOKKEEPING,
    );
    assert_eq!(session.user_requests, 1);
    assert_eq!(session.assistant_messages, 1);
    assert!(session.is_archive_worthy());
    assert_eq!(session.ignored_events, 6);
    let kinds: Vec<_> = session.events.iter().map(|event| event.kind).collect();
    assert_eq!(kinds, ["user", "assistant"]);
}

#[test]
fn claude_transcript_origin_skips_leading_bookkeeping_records() {
    // The 2.1.205 fixture opens with queue-operation records that carry no cwd; the origin
    // reader must scan past them to the first record with an absolute top-level cwd.
    let path = fixture(
        "claude-code-2.1.205",
        "transcript",
        &format!("{CLAUDE_BOOKKEEPING}.jsonl"),
    );
    assert_eq!(
        munshi::claude_transcript_origin(&path),
        Some(PathBuf::from("/work/demo"))
    );
    // A transcript with no cwd anywhere yields no origin rather than a guess.
    let directory = test_directory();
    let no_cwd = directory.path().join("no-cwd.jsonl");
    fs::write(&no_cwd, "{\"type\":\"ai-title\",\"aiTitle\":\"x\"}\n").unwrap();
    assert_eq!(munshi::claude_transcript_origin(&no_cwd), None);
}

#[test]
fn copilot_workspace_origin_reads_only_the_pinned_cwd_key() {
    let directory = test_directory();
    let session = directory.path().join("session-state/copilot-origin");
    fs::create_dir_all(&session).unwrap();
    let events = session.join("events.jsonl");
    fs::write(&events, b"").unwrap();

    // No workspace.yaml yields no origin rather than a guess.
    assert_eq!(munshi::copilot_workspace_origin(&events), None);

    let workspace = session.join("workspace.yaml");
    fs::write(
        &workspace,
        "id: copilot-origin\ncwd: /work/demo\nclient_name: github/cli\n",
    )
    .unwrap();
    assert_eq!(
        munshi::copilot_workspace_origin(&events),
        Some(PathBuf::from("/work/demo"))
    );

    // Quoted scalars are unwrapped; non-absolute values are rejected.
    fs::write(&workspace, "cwd: \"/work/quoted\"\n").unwrap();
    assert_eq!(
        munshi::copilot_workspace_origin(&events),
        Some(PathBuf::from("/work/quoted"))
    );
    fs::write(&workspace, "cwd: relative/path\n").unwrap();
    assert_eq!(munshi::copilot_workspace_origin(&events), None);
    // Nested or foreign keys never match the pinned top-level key.
    fs::write(&workspace, "meta:\n  cwd: /work/nested\n").unwrap();
    assert_eq!(munshi::copilot_workspace_origin(&events), None);
}

#[test]
fn codex_normal_normalizes_to_the_shared_model() {
    let session = load_fixture(
        SourceKind::Codex,
        "codex-rollout-0.x",
        "normal",
        CODEX_NORMAL,
    );
    assert_eq!(session.source, SourceKind::Codex);
    assert_eq!(session.user_requests, 1);
    assert_eq!(session.assistant_messages, 2);
    assert_eq!(session.tool_activities, 2);
    assert!(session.is_archive_worthy());
    // session_meta, turn_context and reasoning records are ignored metadata.
    assert!(session.ignored_events >= 3);
}

#[test]
fn adapters_tolerate_missing_optional_fields() {
    let claude = load_fixture(
        SourceKind::ClaudeCode,
        "claude-code-2.1.44",
        "missing-fields",
        CLAUDE_MISSING,
    );
    assert!(claude.is_archive_worthy());
    assert_eq!(claude.user_requests, 1);

    let codex = load_fixture(
        SourceKind::Codex,
        "codex-rollout-0.x",
        "missing-fields",
        CODEX_MISSING,
    );
    assert!(codex.is_archive_worthy());
    assert_eq!(codex.user_requests, 1);
    // The array-form function_call_output is still captured as tool activity.
    assert!(codex.tool_activities >= 1);
}

#[test]
fn source_specific_metadata_is_recorded_without_leaking_reasoning() {
    // Codex reasoning content must never appear in normalized events.
    let session = load_fixture(
        SourceKind::Codex,
        "codex-rollout-0.x",
        "normal",
        CODEX_NORMAL,
    );
    assert!(
        session
            .events
            .iter()
            .all(|event| !event.content.contains("reasoning") && event.kind != "reasoning")
    );
    // Claude gitBranch/summary metadata does not become an event, but timestamps are kept.
    let claude = load_fixture(
        SourceKind::ClaudeCode,
        "claude-code-2.1.44",
        "resumed",
        CLAUDE_RESUMED,
    );
    assert!(claude.started_at.is_some());
    assert!(claude.events.iter().all(|event| event.kind != "summary"));
}

#[test]
fn truncated_transcripts_are_rejected_for_every_source() {
    let claude = resolve_and_load(
        SourceKind::ClaudeCode,
        &fixture(
            "claude-code-2.1.44",
            "truncated",
            &format!("{CLAUDE_TRUNCATED}.jsonl"),
        ),
        CLAUDE_TRUNCATED,
    );
    assert!(matches!(claude, Err(SourceError::IncompleteTrailingRecord)));

    let codex = resolve_and_load(
        SourceKind::Codex,
        &fixture(
            "codex-rollout-0.x",
            "truncated",
            &format!("{CODEX_TRUNCATED}.jsonl"),
        ),
        CODEX_TRUNCATED,
    );
    assert!(matches!(codex, Err(SourceError::IncompleteTrailingRecord)));
}

#[test]
fn envelope_recognition_rejects_foreign_transcripts() {
    let claude_path = fixture(
        "claude-code-2.1.44",
        "normal",
        &format!("{CLAUDE_NORMAL}.jsonl"),
    );
    let codex_path = fixture(
        "codex-rollout-0.x",
        "normal",
        &format!("{CODEX_NORMAL}.jsonl"),
    );
    let copilot_path = manual_copilot_events();

    // Each adapter accepts its own transcript.
    assert!(validate_transcript_envelope(SourceKind::ClaudeCode, &claude_path, 1 << 20).is_ok());
    assert!(validate_transcript_envelope(SourceKind::Codex, &codex_path, 1 << 20).is_ok());
    assert!(validate_transcript_envelope(SourceKind::Copilot, &copilot_path, 1 << 20).is_ok());

    // And rejects the others.
    assert!(validate_transcript_envelope(SourceKind::Codex, &claude_path, 1 << 20).is_err());
    assert!(validate_transcript_envelope(SourceKind::ClaudeCode, &codex_path, 1 << 20).is_err());
    assert!(validate_transcript_envelope(SourceKind::Copilot, &claude_path, 1 << 20).is_err());
    assert!(validate_transcript_envelope(SourceKind::ClaudeCode, &copilot_path, 1 << 20).is_err());
}

// ---------------------------------------------------------------------------
// One-shot shared archive pipeline (source selection independent of summarizer)
// ---------------------------------------------------------------------------

#[test]
fn claude_archives_through_the_shared_one_shot_pipeline() {
    let (outcome, output, project, _dir) = archive_fixture(
        SourceKind::ClaudeCode,
        "claude-code-2.1.44",
        "normal",
        CLAUDE_NORMAL,
    );
    let ArchiveOutcome::Archived { id, relative_path } = outcome else {
        panic!("expected archived outcome");
    };
    assert_eq!(id, format!("claude-code:{CLAUDE_NORMAL}"));
    let markdown = fs::read_to_string(output.join(&relative_path)).unwrap();
    let parsed = parse_archive_markdown(&markdown).unwrap();
    assert_eq!(parsed.source, SourceKind::ClaudeCode);
    assert_eq!(parsed.session_id, CLAUDE_NORMAL);
    // Copilot's summarizer archived a Claude session: capture and summarization are decoupled.
    assert!(markdown.contains("agent: \"claude-code\""));
    assert!(!markdown.contains(project.to_string_lossy().as_ref()));
}

#[test]
fn codex_archives_through_the_shared_one_shot_pipeline() {
    let (outcome, output, _project, _dir) = archive_fixture(
        SourceKind::Codex,
        "codex-rollout-0.x",
        "normal",
        CODEX_NORMAL,
    );
    let ArchiveOutcome::Archived { id, relative_path } = outcome else {
        panic!("expected archived outcome");
    };
    assert_eq!(id, format!("codex:{CODEX_NORMAL}"));
    let parsed =
        parse_archive_markdown(&fs::read_to_string(output.join(relative_path)).unwrap()).unwrap();
    assert_eq!(parsed.source, SourceKind::Codex);
}

// ---------------------------------------------------------------------------
// Shared archive/state pipeline (worker + SQLite state machine)
// ---------------------------------------------------------------------------

#[test]
fn claude_resumed_session_revises_through_the_state_pipeline() {
    let harness = StateHarness::new();
    // Copy the normal transcript so a resumed turn can be appended without touching fixtures.
    let transcript = harness.copy_transcript(
        &fixture(
            "claude-code-2.1.44",
            "normal",
            &format!("{CLAUDE_NORMAL}.jsonl"),
        ),
        CLAUDE_NORMAL,
    );

    let first = harness.archive(
        SourceKind::ClaudeCode,
        CLAUDE_NORMAL,
        &transcript,
        "complete",
    );
    assert!(matches!(first, HookResult::Archived { .. }));
    let markdown = harness.read_archive(SourceKind::ClaudeCode, CLAUDE_NORMAL);
    assert_eq!(markdown.summary_revision, 1);
    assert_eq!(markdown.source, SourceKind::ClaudeCode);
    assert_eq!(markdown.completion_reason, "complete");

    // Resume: append a further Claude turn and archive again -> revision 2 (delta).
    harness.append_line(
        &transcript,
        &format!(
            "{{\"type\":\"user\",\"uuid\":\"u9\",\"parentUuid\":\"a3\",\"sessionId\":\"{CLAUDE_NORMAL}\",\"timestamp\":\"2026-07-11T20:06:00.000Z\",\"cwd\":\"/work/demo\",\"version\":\"2.1.44\",\"gitBranch\":\"main\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{{\"role\":\"user\",\"content\":\"Also add a goodbye function.\"}}}}"
        ),
    );
    harness.append_line(
        &transcript,
        &format!(
            "{{\"type\":\"assistant\",\"uuid\":\"a9\",\"parentUuid\":\"u9\",\"sessionId\":\"{CLAUDE_NORMAL}\",\"timestamp\":\"2026-07-11T20:07:00.000Z\",\"cwd\":\"/work/demo\",\"version\":\"2.1.44\",\"gitBranch\":\"main\",\"isSidechain\":false,\"userType\":\"external\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Added goodbye().\"}}]}}}}"
        ),
    );

    let second = harness.archive(
        SourceKind::ClaudeCode,
        CLAUDE_NORMAL,
        &transcript,
        "complete",
    );
    assert!(matches!(second, HookResult::Archived { .. }));
    let revised = harness.read_archive(SourceKind::ClaudeCode, CLAUDE_NORMAL);
    assert_eq!(revised.summary_revision, 2);
}

#[test]
fn claude_interrupted_session_records_the_interrupted_reason() {
    let harness = StateHarness::new();
    let transcript = harness.copy_transcript(
        &fixture(
            "claude-code-2.1.44",
            "interrupted",
            &format!("{CLAUDE_INTERRUPTED}.jsonl"),
        ),
        CLAUDE_INTERRUPTED,
    );
    let result = harness.archive(
        SourceKind::ClaudeCode,
        CLAUDE_INTERRUPTED,
        &transcript,
        "user_exit",
    );
    assert!(matches!(result, HookResult::Archived { .. }));
    let markdown = harness.read_archive(SourceKind::ClaudeCode, CLAUDE_INTERRUPTED);
    assert_eq!(markdown.completion_reason, "interrupted");
}

#[test]
fn codex_resumed_session_archives_through_the_state_pipeline() {
    let harness = StateHarness::new();
    let transcript = harness.copy_transcript(
        &fixture(
            "codex-rollout-0.x",
            "resumed",
            &format!("{CODEX_RESUMED}.jsonl"),
        ),
        CODEX_RESUMED,
    );
    let result = harness.archive(SourceKind::Codex, CODEX_RESUMED, &transcript, "complete");
    assert!(matches!(result, HookResult::Archived { .. }));
    let markdown = harness.read_archive(SourceKind::Codex, CODEX_RESUMED);
    assert_eq!(markdown.source, SourceKind::Codex);
    assert_eq!(markdown.summary_revision, 1);
}

#[test]
fn state_store_isolates_sessions_by_source() {
    let harness = StateHarness::new();
    let claude = harness.copy_transcript(
        &fixture(
            "claude-code-2.1.44",
            "concurrent",
            &format!("{CLAUDE_CONCURRENT_A}.jsonl"),
        ),
        CLAUDE_CONCURRENT_A,
    );
    harness.archive(
        SourceKind::ClaudeCode,
        CLAUDE_CONCURRENT_A,
        &claude,
        "complete",
    );

    // The Claude session is visible under Claude scope but not Copilot scope, even though
    // they share one SQLite database.
    let claude_store = StateStore::open_for_source(&harness.state, SourceKind::ClaudeCode).unwrap();
    assert!(
        claude_store
            .get_session(CLAUDE_CONCURRENT_A)
            .unwrap()
            .is_some()
    );
    drop(claude_store);
    let copilot_store = StateStore::open(&harness.state).unwrap();
    assert!(
        copilot_store
            .get_session(CLAUDE_CONCURRENT_A)
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Cross-source identity: shared path collisions and rebuild/hydrate scoping
// ---------------------------------------------------------------------------

const SHARED_ID: &str = "5ha4ed00-0000-4000-8000-000000000001";

/// Two different harnesses that share one project and one session ID must archive
/// to distinct Markdown files instead of overwriting each other.
#[test]
fn two_sources_sharing_project_and_session_id_do_not_collide() {
    let shared = SharedArchives::new();
    let (claude_rel, claude_md) = shared.archive(SourceKind::ClaudeCode);
    let (codex_rel, codex_md) = shared.archive(SourceKind::Codex);

    assert_ne!(claude_rel, codex_rel);
    assert!(shared.output.join(&claude_rel).is_file());
    assert!(shared.output.join(&codex_rel).is_file());
    // Both files survive: neither archive was overwritten by the other source.
    assert_eq!(claude_md.source, SourceKind::ClaudeCode);
    assert_eq!(codex_md.source, SourceKind::Codex);
    assert_eq!(claude_md.session_id, SHARED_ID);
    assert_eq!(codex_md.session_id, SHARED_ID);
    // Layout: <component>/<source-prefix>/<session_id>.md, sharing the project
    // component but separated by a per-source segment.
    let claude_parts = path_parts(&claude_rel);
    let codex_parts = path_parts(&codex_rel);
    assert_eq!(claude_parts.len(), 3);
    assert_eq!(codex_parts.len(), 3);
    assert_eq!(claude_parts[0], codex_parts[0], "same project component");
    assert_eq!(claude_parts[1], "claude-code");
    assert_eq!(codex_parts[1], "codex");
    assert_eq!(claude_parts[2], format!("{SHARED_ID}.md"));
    assert_eq!(codex_parts[2], format!("{SHARED_ID}.md"));
}

/// A rebuild must retain both same-ID sessions and import each under its own source,
/// never cross-importing one source's archive into another.
#[test]
fn rebuild_retains_both_sources_and_never_cross_imports() {
    let shared = SharedArchives::new();
    let (claude_rel, _) = shared.archive(SourceKind::ClaudeCode);
    let (codex_rel, _) = shared.archive(SourceKind::Codex);

    let state = shared.dir.path().join("state");
    let mut store = StateStore::open(&state).unwrap();
    let count = store
        .rebuild_from_archives(&shared.output, &SourceHomes::default())
        .unwrap();
    assert_eq!(
        count, 2,
        "both same-ID cross-source archives must be retained"
    );
    drop(store);

    let claude_store = StateStore::open_for_source(&state, SourceKind::ClaudeCode).unwrap();
    let claude = claude_store.get_session(SHARED_ID).unwrap().unwrap();
    assert_eq!(
        claude.markdown_relative_path.as_deref(),
        Some(claude_rel.as_path())
    );

    let codex_store = StateStore::open_for_source(&state, SourceKind::Codex).unwrap();
    let codex = codex_store.get_session(SHARED_ID).unwrap().unwrap();
    assert_eq!(
        codex.markdown_relative_path.as_deref(),
        Some(codex_rel.as_path())
    );
}

/// A rebuilt row is born knowing where its transcript lives, wherever that is derivable (issue
/// #53). An archive Markdown file records everything about a session except its transcript path, so
/// rebuilt rows used to be unreadable — and, since issue #47, unable to upload a self-contained
/// snapshot — even with the transcript still on disk. The rebuild now searches the *registered*
/// harness home the same way the recovery sweep does. Codex has no safe session-ID-only lookup, so
/// its row keeps the NULL it had rather than guessing, and the rebuild itself never fails either
/// way.
#[test]
fn rebuild_rederives_transcript_paths_from_the_registered_claude_home() {
    let shared = SharedArchives::new();
    shared.archive(SourceKind::ClaudeCode);
    shared.archive(SourceKind::Codex);

    // A Claude home holding this session's transcript exactly where the harness keeps it, plus a
    // decoy project directory that must not be mistaken for it.
    let claude_home = shared.dir.path().join("claude-home");
    let project_directory = claude_home.join("projects/-work-demo");
    fs::create_dir_all(&project_directory).unwrap();
    fs::create_dir_all(claude_home.join("projects/-work-other")).unwrap();
    let transcript = project_directory.join(format!("{SHARED_ID}.jsonl"));
    fs::copy(
        fixture(
            "claude-code-2.1.44",
            "normal",
            &format!("{CLAUDE_NORMAL}.jsonl"),
        ),
        &transcript,
    )
    .unwrap();
    let homes = SourceHomes {
        copilot_home: None,
        claude_home: Some(claude_home),
    };

    let state = shared.dir.path().join("state");
    let mut store = StateStore::open(&state).unwrap();
    assert_eq!(
        store.rebuild_from_archives(&shared.output, &homes).unwrap(),
        2
    );
    drop(store);

    let claude_store = StateStore::open_for_source(&state, SourceKind::ClaudeCode).unwrap();
    let claude = claude_store.get_session(SHARED_ID).unwrap().unwrap();
    assert_eq!(
        claude.transcript_path,
        Some(transcript.canonicalize().unwrap()),
        "the rebuilt Claude row learned its transcript from the registered home"
    );
    let codex_store = StateStore::open_for_source(&state, SourceKind::Codex).unwrap();
    let codex = codex_store.get_session(SHARED_ID).unwrap().unwrap();
    assert_eq!(
        codex.transcript_path, None,
        "Codex has no safe session-ID lookup, so its rebuilt row stays honest about knowing nothing"
    );

    // Without a registered home there is nowhere derivation is permitted to look, and the rebuild
    // still succeeds — a derivation that finds nothing leaves the NULL exactly as before.
    let bare = shared.dir.path().join("bare-state");
    let mut bare_store = StateStore::open(&bare).unwrap();
    assert_eq!(
        bare_store
            .rebuild_from_archives(&shared.output, &SourceHomes::default())
            .unwrap(),
        2
    );
    drop(bare_store);
    let bare_claude = StateStore::open_for_source(&bare, SourceKind::ClaudeCode).unwrap();
    assert_eq!(
        bare_claude
            .get_session(SHARED_ID)
            .unwrap()
            .unwrap()
            .transcript_path,
        None
    );
}

/// Hydrating a single session scopes to the store's source, so it never pulls in a
/// different source's archive that happens to share the session ID.
#[test]
fn hydrate_scopes_to_the_store_source_only() {
    let shared = SharedArchives::new();
    let (claude_rel, _) = shared.archive(SourceKind::ClaudeCode);
    shared.archive(SourceKind::Codex);

    let state = shared.dir.path().join("state");
    let mut claude_store = StateStore::open_for_source(&state, SourceKind::ClaudeCode).unwrap();
    assert!(
        claude_store
            .hydrate_session_from_archives(&shared.output, SHARED_ID, &SourceHomes::default())
            .unwrap()
    );
    let claude = claude_store.get_session(SHARED_ID).unwrap().unwrap();
    assert_eq!(
        claude.markdown_relative_path.as_deref(),
        Some(claude_rel.as_path())
    );
    drop(claude_store);

    // The Claude-scoped hydrate must not have imported the Codex archive.
    let codex_store = StateStore::open_for_source(&state, SourceKind::Codex).unwrap();
    assert!(codex_store.get_session(SHARED_ID).unwrap().is_none());
}

/// The `retry` CLI must refuse to act on an ambiguous session ID shared by two sources
/// unless a `--source` selector is given, and must target exactly the selected source.
#[test]
fn retry_cli_requires_a_source_selector_when_ambiguous() {
    let harness = StateHarness::new();
    let claude = harness.copy_transcript_for(
        SourceKind::ClaudeCode,
        &fixture(
            "claude-code-2.1.44",
            "normal",
            &format!("{CLAUDE_NORMAL}.jsonl"),
        ),
        SHARED_ID,
    );
    let codex = harness.copy_transcript_for(
        SourceKind::Codex,
        &fixture(
            "codex-rollout-0.x",
            "normal",
            &format!("{CODEX_NORMAL}.jsonl"),
        ),
        SHARED_ID,
    );
    harness.archive(SourceKind::ClaudeCode, SHARED_ID, &claude, "complete");
    harness.archive(SourceKind::Codex, SHARED_ID, &codex, "complete");

    // Without --source the command must fail rather than pick a source arbitrarily.
    let ambiguous = harness.retry_cli(SHARED_ID, None);
    assert!(!ambiguous.status.success());
    let stderr = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(stderr.contains("multiple sources"), "stderr: {stderr}");
    assert!(stderr.contains("claude-code") && stderr.contains("codex"));

    // With an explicit selector the retry targets exactly that source.
    let targeted = harness.retry_cli(SHARED_ID, Some("codex"));
    assert!(
        targeted.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&targeted.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&targeted.stdout).unwrap();
    assert_eq!(report["command"], "retry");
    assert_eq!(report["source"], "codex");
    assert_eq!(report["session_id"], SHARED_ID);
}

/// A stale/interrupted Claude session recovered by `run_recovery` must be marked and
/// worked through the Claude adapter — never mis-attributed to Copilot and never given a
/// duplicate Copilot row.
#[test]
fn claude_stale_session_recovers_through_the_claude_adapter() {
    let harness = StateHarness::new();
    let transcript = harness.copy_transcript(
        &fixture(
            "claude-code-2.1.44",
            "normal",
            &format!("{CLAUDE_NORMAL}.jsonl"),
        ),
        CLAUDE_NORMAL,
    );
    // agent-stop with no session-end models an interrupted/crashed Claude session.
    harness.ingest_agent_stop_only(SourceKind::ClaudeCode, CLAUDE_NORMAL, &transcript);

    let recover = harness.recover_cli(0);
    assert!(
        recover.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&recover.stderr)
    );

    let record = harness
        .wait_for_archived(
            SourceKind::ClaudeCode,
            CLAUDE_NORMAL,
            Duration::from_secs(15),
        )
        .expect("Claude session must archive through the Claude adapter");
    assert_eq!(record.source, SourceKind::ClaudeCode);
    let markdown = harness.read_archive(SourceKind::ClaudeCode, CLAUDE_NORMAL);
    assert_eq!(markdown.source, SourceKind::ClaudeCode);
    // Recovery records the interrupted (unknown end) completion reason.
    assert_eq!(markdown.completion_reason, "unknown");

    // No duplicate Copilot row was created for the same session ID.
    let copilot = StateStore::open(&harness.state).unwrap();
    assert!(copilot.get_session(CLAUDE_NORMAL).unwrap().is_none());
}

/// Issue #40: a Claude session whose origin directory was deleted must archive from the
/// transcript's own recorded evidence — the per-record `cwd` decides the identity, the
/// per-record `gitBranch` is carried into provenance, and the frontmatter flags the
/// recorded origin.
#[test]
fn claude_gone_origin_archives_from_recorded_cwd_and_branch() {
    let harness = StateHarness::new();
    let transcript = harness.copy_transcript(
        &fixture(
            "claude-code-2.1.44",
            "normal",
            &format!("{CLAUDE_NORMAL}.jsonl"),
        ),
        CLAUDE_NORMAL,
    );
    let gone = harness.directory.path().join("gone-project");
    fs::create_dir_all(&gone).unwrap();
    let gone = gone.canonicalize().unwrap();
    {
        let mut store =
            StateStore::open_for_source(&harness.state, SourceKind::ClaudeCode).unwrap();
        store
            .ingest_agent_stop(CLAUDE_NORMAL, 100, &gone, &transcript)
            .unwrap();
        store
            .ingest_session_end(
                CLAUDE_NORMAL,
                101,
                &gone,
                "other",
                CompletionReason::Complete,
                None,
            )
            .unwrap();
    }
    fs::remove_dir_all(&gone).unwrap();

    let result = run_archive_worker_for_source(
        &harness.state,
        SourceKind::ClaudeCode,
        CLAUDE_NORMAL,
        WorkerContext::Interactive,
    )
    .unwrap();
    assert!(matches!(result, HookResult::Archived { .. }), "{result:?}");
    let markdown = harness.read_archive(SourceKind::ClaudeCode, CLAUDE_NORMAL);
    // Identity comes from the RECORDED cwd (/work/demo), not the deleted hook-provided origin.
    let expected = recorded_project_identity(Path::new("/work/demo"), Some("main".to_owned()));
    assert_eq!(markdown.project.identity, expected.identity);
    assert_eq!(markdown.project.component, expected.component);
    assert_eq!(markdown.project.project, "demo");
    // The recorded gitBranch reaches provenance where the live path would have supplied it.
    assert_eq!(markdown.project.branch.as_deref(), Some("main"));
    assert_eq!(markdown.project.repository, None);
}

/// Issue #61: a scheduler-descended (background) worker must not inspect a session's origin
/// directory — even one that exists — because on macOS that access is attributed to munshi
/// itself and raises a TCC prompt. An identity-less session is deferred untouched, and the
/// next interactive worker archives it with the full live identity, not the recorded-evidence
/// fallback.
#[test]
fn background_worker_defers_identity_less_session_instead_of_inspecting_origin() {
    let harness = StateHarness::new();
    let transcript = harness.copy_transcript(
        &fixture(
            "claude-code-2.1.44",
            "normal",
            &format!("{CLAUDE_NORMAL}.jsonl"),
        ),
        CLAUDE_NORMAL,
    );
    let origin = harness.directory.path().join("live-project");
    fs::create_dir_all(&origin).unwrap();
    let origin = origin.canonicalize().unwrap();
    // Make the origin its own remote-less git root so live inspection resolves it (and not
    // an enclosing repository the temp directory happens to sit inside).
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&origin)
            .status()
            .unwrap()
            .success()
    );
    {
        let mut store =
            StateStore::open_for_source(&harness.state, SourceKind::ClaudeCode).unwrap();
        store
            .ingest_agent_stop(CLAUDE_NORMAL, 100, &origin, &transcript)
            .unwrap();
        store
            .ingest_session_end(
                CLAUDE_NORMAL,
                101,
                &origin,
                "other",
                CompletionReason::Complete,
                None,
            )
            .unwrap();
    }

    let before = {
        let store = StateStore::open_for_source(&harness.state, SourceKind::ClaudeCode).unwrap();
        store.get_session(CLAUDE_NORMAL).unwrap().unwrap()
    };
    let result = run_archive_worker_for_source(
        &harness.state,
        SourceKind::ClaudeCode,
        CLAUDE_NORMAL,
        WorkerContext::Background,
    )
    .unwrap();
    assert!(
        matches!(&result, HookResult::Failed { code } if code == "project-inspection-deferred"),
        "{result:?}"
    );
    // The deferral leaves the session exactly where it was: same lifecycle, still no
    // project identity, ready for the next interactive sweep.
    let after = {
        let store = StateStore::open_for_source(&harness.state, SourceKind::ClaudeCode).unwrap();
        store.get_session(CLAUDE_NORMAL).unwrap().unwrap()
    };
    assert_eq!(after.lifecycle_state, before.lifecycle_state);
    assert!(after.project.is_none());

    let result = run_archive_worker_for_source(
        &harness.state,
        SourceKind::ClaudeCode,
        CLAUDE_NORMAL,
        WorkerContext::Interactive,
    )
    .unwrap();
    assert!(matches!(result, HookResult::Archived { .. }), "{result:?}");
    let markdown = harness.read_archive(SourceKind::ClaudeCode, CLAUDE_NORMAL);
    // Identity comes from the LIVE origin directory (the deferral preserved that fidelity),
    // not from the transcript's recorded cwd (/work/demo) the background worker would have
    // had to settle for.
    let expected = recorded_project_identity(&origin, None);
    assert_eq!(markdown.project.identity, expected.identity);
    assert_eq!(markdown.project.component, expected.component);
}

/// The same for Codex: recovery must route the stale session to the Codex adapter and
/// leave no Copilot duplicate.
#[test]
fn codex_stale_session_recovers_through_the_codex_adapter() {
    let harness = StateHarness::new();
    let transcript = harness.copy_transcript(
        &fixture(
            "codex-rollout-0.x",
            "normal",
            &format!("{CODEX_NORMAL}.jsonl"),
        ),
        CODEX_NORMAL,
    );
    harness.ingest_agent_stop_only(SourceKind::Codex, CODEX_NORMAL, &transcript);

    let recover = harness.recover_cli(0);
    assert!(
        recover.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&recover.stderr)
    );

    let record = harness
        .wait_for_archived(SourceKind::Codex, CODEX_NORMAL, Duration::from_secs(15))
        .expect("Codex session must archive through the Codex adapter");
    assert_eq!(record.source, SourceKind::Codex);
    let markdown = harness.read_archive(SourceKind::Codex, CODEX_NORMAL);
    assert_eq!(markdown.source, SourceKind::Codex);

    let copilot = StateStore::open(&harness.state).unwrap();
    assert!(copilot.get_session(CODEX_NORMAL).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Summarizer-exhaust guard (issue #37)
// ---------------------------------------------------------------------------

const EXHAUST_COPILOT: &str = "e0000000-0000-4000-8000-000000000037";
const EXHAUST_CLAUDE: &str = "e0000000-0000-4000-8000-000000000038";
const EXHAUST_LATE: &str = "e0000000-0000-4000-8000-000000000039";

/// A summarizer that is itself a session-recording harness writes a fresh session for every
/// summary Munshi requests, and its first user message is Munshi's own request envelope. Such a
/// session must settle as not-archive-worthy with the `summarizer-exhaust` diagnostic and must
/// never reach the summarizer — otherwise archiving N sessions creates N more.
///
/// The Copilot case deliberately uses an envelope larger than the default 128 KiB per-event cap,
/// the size a real backlog session produces: recognition has to happen on the original content,
/// before elision replaces the event with a claim-ticket marker that carries none of its text.
#[test]
fn copilot_summarizer_exhaust_settles_without_invoking_the_summarizer() {
    let harness = StateHarness::new();
    let envelope = harness.summary_request_envelope(400, 512);
    assert!(
        envelope.len() > munshi::DEFAULT_MAX_EVENT_TEXT_BYTES,
        "the oversize case needs an envelope past the elision threshold: {} bytes",
        envelope.len()
    );
    let transcript = harness.write_copilot_session(EXHAUST_COPILOT, &[&envelope]);

    let result = harness.archive(
        SourceKind::Copilot,
        EXHAUST_COPILOT,
        &transcript,
        "complete",
    );

    assert_eq!(result, HookResult::NotArchiveWorthy, "{result:?}");
    assert_eq!(
        harness.summarizer_calls(),
        0,
        "exhaust must cost no summarizer call"
    );
    assert!(
        harness
            .diagnostic_categories(SourceKind::Copilot, EXHAUST_COPILOT)
            .contains(&SUMMARIZER_EXHAUST_DIAGNOSTIC.to_owned()),
        "the verdict must name its real reason"
    );
    harness.assert_settled_unarchived(SourceKind::Copilot, EXHAUST_COPILOT);
}

/// The same for Claude Code: the guard reads the normalized model, so it holds across every
/// transcript shape, not just the one whose wrapper first hit the loop.
#[test]
fn claude_summarizer_exhaust_settles_without_invoking_the_summarizer() {
    let harness = StateHarness::new();
    let envelope = harness.summary_request_envelope(2, 32);
    let transcript = harness.write_claude_session(EXHAUST_CLAUDE, &[&envelope]);

    let result = harness.archive(
        SourceKind::ClaudeCode,
        EXHAUST_CLAUDE,
        &transcript,
        "complete",
    );

    assert_eq!(result, HookResult::NotArchiveWorthy, "{result:?}");
    assert_eq!(harness.summarizer_calls(), 0);
    assert!(
        harness
            .diagnostic_categories(SourceKind::ClaudeCode, EXHAUST_CLAUDE)
            .contains(&SUMMARIZER_EXHAUST_DIAGNOSTIC.to_owned())
    );
    harness.assert_settled_unarchived(SourceKind::ClaudeCode, EXHAUST_CLAUDE);
}

/// The guard keys on the session's *first* user message. A genuine working session that merely
/// quotes a request envelope later — debugging a summarizer wrapper is exactly that session —
/// archives normally, and so does an ordinary session with no envelope anywhere.
#[test]
fn only_a_sessions_first_message_can_make_it_summarizer_exhaust() {
    let harness = StateHarness::new();
    let envelope = harness.summary_request_envelope(2, 32);
    let transcript = harness.write_claude_session(
        EXHAUST_LATE,
        &["Why does the summarizer wrapper reject this?", &envelope],
    );

    let result = harness.archive(
        SourceKind::ClaudeCode,
        EXHAUST_LATE,
        &transcript,
        "complete",
    );

    assert!(matches!(result, HookResult::Archived { .. }), "{result:?}");
    assert_eq!(
        harness.summarizer_calls(),
        1,
        "an ordinary session still gets summarized exactly once"
    );
    assert!(
        harness
            .diagnostic_categories(SourceKind::ClaudeCode, EXHAUST_LATE)
            .is_empty()
    );
    assert_eq!(
        harness
            .read_archive(SourceKind::ClaudeCode, EXHAUST_LATE)
            .session_id,
        EXHAUST_LATE
    );
}

/// Archive Git commit subjects must carry each source's durable identity (matching the
/// Markdown frontmatter `id`), and same-ID cross-source archives must produce distinct,
/// idempotent commits.
#[test]
fn archive_git_subjects_match_identity_and_stay_distinct_per_source() {
    let harness = StateHarness::new_with_git_history();
    let claude = harness.copy_transcript_for(
        SourceKind::ClaudeCode,
        &fixture(
            "claude-code-2.1.44",
            "normal",
            &format!("{CLAUDE_NORMAL}.jsonl"),
        ),
        SHARED_ID,
    );
    let codex = harness.copy_transcript_for(
        SourceKind::Codex,
        &fixture(
            "codex-rollout-0.x",
            "normal",
            &format!("{CODEX_NORMAL}.jsonl"),
        ),
        SHARED_ID,
    );

    let HookResult::Archived {
        relative_path: claude_rel,
    } = harness.archive(SourceKind::ClaudeCode, SHARED_ID, &claude, "complete")
    else {
        panic!("claude archive expected");
    };
    let HookResult::Archived {
        relative_path: codex_rel,
    } = harness.archive(SourceKind::Codex, SHARED_ID, &codex, "complete")
    else {
        panic!("codex archive expected");
    };

    // Subjects match the Markdown identity `<source-prefix>:<session_id>`.
    let claude_md = harness.read_archive(SourceKind::ClaudeCode, SHARED_ID);
    let codex_md = harness.read_archive(SourceKind::Codex, SHARED_ID);
    assert_eq!(claude_md.source, SourceKind::ClaudeCode);
    assert_eq!(codex_md.source, SourceKind::Codex);
    let subjects = harness.archive_commit_subjects();
    assert!(
        subjects.contains(&format!("archive: claude-code:{SHARED_ID} revision 1")),
        "subjects: {subjects:?}"
    );
    assert!(
        subjects.contains(&format!("archive: codex:{SHARED_ID} revision 1")),
        "subjects: {subjects:?}"
    );
    assert!(!subjects.iter().any(|subject| subject.contains("copilot:")));

    // Two distinct commits, each scoped to its own source-nested file.
    assert_eq!(harness.archive_commit_count(), 2);
    assert_ne!(claude_rel, codex_rel);
    assert_eq!(harness.archive_commits_touching(&claude_rel), 1);
    assert_eq!(harness.archive_commits_touching(&codex_rel), 1);

    // Idempotent: re-archiving the same unchanged source creates no new commit.
    harness.archive(SourceKind::ClaudeCode, SHARED_ID, &claude, "complete");
    assert_eq!(harness.archive_commit_count(), 2);
    assert_eq!(harness.archive_commits_touching(&claude_rel), 1);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn path_parts(path: &Path) -> Vec<String> {
    path.iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect()
}

/// Shared output directory and project used to archive several sources under the
/// same project component and session ID, to exercise cross-source collisions.
struct SharedArchives {
    dir: TempDir,
    output: PathBuf,
    project: PathBuf,
    summarizer: PathBuf,
}

impl SharedArchives {
    fn new() -> Self {
        let dir = test_directory();
        let project = git_project(dir.path());
        let output = dir.path().join("archives");
        let summarizer = adapter_summarizer(dir.path());
        Self {
            dir,
            output,
            project,
            summarizer,
        }
    }

    fn archive(&self, source: SourceKind) -> (PathBuf, munshi::ArchivedMarkdown) {
        let (family, scenario, id) = match source {
            SourceKind::ClaudeCode => ("claude-code-2.1.44", "normal", CLAUDE_NORMAL),
            SourceKind::Codex => ("codex-rollout-0.x", "normal", CODEX_NORMAL),
            SourceKind::Copilot => panic!("copilot resolution requires an events.jsonl parent dir"),
        };
        // Copy the source fixture to a transcript whose stem is the shared session ID.
        let transcripts = self.dir.path().join(format!("t-{}", source.as_selector()));
        fs::create_dir_all(&transcripts).unwrap();
        let transcript = transcripts.join(format!("{SHARED_ID}.jsonl"));
        fs::copy(
            fixture(family, scenario, &format!("{id}.jsonl")),
            &transcript,
        )
        .unwrap();

        let outcome = archive_session(&ArchiveConfig {
            reference: SessionReference {
                source,
                session_id: Some(SHARED_ID.to_owned()),
                events_path: Some(transcript),
                copilot_home: None,
            },
            project_directory: self.project.clone(),
            output_directory: self.output.clone(),
            summarizer_binary: self.summarizer.clone(),
            summarizer_args: Vec::new(),
            summarizer_env: Vec::new(),
            timeout: Duration::from_secs(5),
            max_source_bytes: 1 << 20,
            max_input_bytes: 1 << 20,
            max_stdout_bytes: 16 * 1024,
            max_stderr_bytes: 4 * 1024,
            max_event_text_bytes: munshi::DEFAULT_MAX_EVENT_TEXT_BYTES,
        })
        .unwrap();
        let ArchiveOutcome::Archived { relative_path, .. } = outcome else {
            panic!("expected archived outcome for {source:?}");
        };
        let markdown =
            parse_archive_markdown(&fs::read_to_string(self.output.join(&relative_path)).unwrap())
                .unwrap();
        (relative_path, markdown)
    }
}

fn fixture(family: &str, scenario: &str, file: &str) -> PathBuf {
    fixture_root().join(family).join(scenario).join(file)
}

fn manual_copilot_events() -> PathBuf {
    fixture_root().join("manual/copilot/11111111-1111-4111-8111-111111111111/events.jsonl")
}

fn resolve_and_load(
    source: SourceKind,
    path: &Path,
    session_id: &str,
) -> Result<NormalizedSession, SourceError> {
    let resolved = resolve_session_reference(&SessionReference {
        source,
        session_id: Some(session_id.to_owned()),
        events_path: Some(path.to_path_buf()),
        copilot_home: None,
    })?;
    load_session(&resolved, 1 << 20)
}

fn load_fixture(
    source: SourceKind,
    family: &str,
    scenario: &str,
    session_id: &str,
) -> NormalizedSession {
    let path = fixture(family, scenario, &format!("{session_id}.jsonl"));
    resolve_and_load(source, &path, session_id).unwrap()
}

fn archive_fixture(
    source: SourceKind,
    family: &str,
    scenario: &str,
    session_id: &str,
) -> (ArchiveOutcome, PathBuf, PathBuf, TempDir) {
    let directory = test_directory();
    let project = git_project(directory.path());
    let output = directory.path().join("archives");
    let outcome = archive_session(&ArchiveConfig {
        reference: SessionReference {
            source,
            session_id: Some(session_id.to_owned()),
            events_path: Some(fixture(family, scenario, &format!("{session_id}.jsonl"))),
            copilot_home: None,
        },
        project_directory: project.clone(),
        output_directory: output.clone(),
        summarizer_binary: adapter_summarizer(directory.path()),
        summarizer_args: Vec::new(),
        summarizer_env: Vec::new(),
        timeout: Duration::from_secs(5),
        max_source_bytes: 1 << 20,
        max_input_bytes: 1 << 20,
        max_stdout_bytes: 16 * 1024,
        max_stderr_bytes: 4 * 1024,
        max_event_text_bytes: munshi::DEFAULT_MAX_EVENT_TEXT_BYTES,
    })
    .unwrap();
    (outcome, output, project, directory)
}

struct StateHarness {
    directory: TempDir,
    state: PathBuf,
    output: PathBuf,
    project: PathBuf,
    clock: std::cell::Cell<i64>,
}

impl StateHarness {
    fn new() -> Self {
        Self::build(false)
    }

    fn new_with_git_history() -> Self {
        Self::build(true)
    }

    fn build(git_history: bool) -> Self {
        let directory = test_directory();
        let project = git_project(directory.path());
        let copilot_home = directory.path().join("copilot-home");
        let state = directory.path().join("munshi-home");
        let output = directory.path().join("archives");
        let summarizer = adapter_summarizer(directory.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
        command
            .arg("register")
            .arg("--accept-transcript-processing")
            .arg("--copilot-home")
            .arg(&copilot_home)
            .arg("--state-dir")
            .arg(&state)
            .arg("--output-dir")
            .arg(&output)
            .arg("--summarizer")
            .arg(&summarizer)
            .arg("--timeout-ms")
            .arg("10000")
            .stdin(Stdio::null());
        if git_history {
            command.arg("--archive-git-history");
        }
        let register = command.output().unwrap();
        assert!(register.status.success(), "register failed: {register:?}");
        Self {
            state,
            output,
            project,
            directory,
            clock: std::cell::Cell::new(100),
        }
    }

    fn copy_transcript(&self, source_path: &Path, session_id: &str) -> PathBuf {
        let dir = self.directory.path().join("transcripts");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join(format!("{session_id}.jsonl"));
        fs::copy(source_path, &target).unwrap();
        target
    }

    /// Copy a transcript into a per-source subdirectory so the same session ID can be
    /// staged for two different sources without the files colliding.
    fn copy_transcript_for(
        &self,
        source: SourceKind,
        source_path: &Path,
        session_id: &str,
    ) -> PathBuf {
        let dir = self
            .directory
            .path()
            .join("transcripts")
            .join(source.as_selector());
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join(format!("{session_id}.jsonl"));
        fs::copy(source_path, &target).unwrap();
        target
    }

    fn retry_cli(&self, session_id: &str, source: Option<&str>) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
        command
            .arg("retry")
            .arg(session_id)
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--json");
        if let Some(source) = source {
            command.arg("--source").arg(source);
        }
        command.output().unwrap()
    }

    /// Ingest a single agent-stop observation (with no session-end) to model an
    /// interrupted/crashed session for the given source.
    fn ingest_agent_stop_only(&self, source: SourceKind, session_id: &str, transcript: &Path) {
        let mut store = StateStore::open_for_source(&self.state, source).unwrap();
        let ts = self.clock.get();
        self.clock.set(ts + 10);
        store
            .ingest_agent_stop(session_id, ts, &self.project, transcript)
            .unwrap();
    }

    fn recover_cli(&self, stale_after_ms: u64) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("hook")
            .arg("recover")
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--stale-after-ms")
            .arg(stale_after_ms.to_string())
            .output()
            .unwrap()
    }

    fn wait_for_archived(
        &self,
        source: SourceKind,
        session_id: &str,
        timeout: Duration,
    ) -> Option<munshi::SessionRecord> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let store = StateStore::open_for_source(&self.state, source).unwrap();
            if let Some(record) = store.get_session(session_id).unwrap() {
                if record.lifecycle_state == "archived" {
                    return Some(record);
                }
            }
            drop(store);
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn append_line(&self, transcript: &Path, line: &str) {
        let mut contents = fs::read_to_string(transcript).unwrap();
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(line);
        contents.push('\n');
        fs::write(transcript, contents).unwrap();
    }

    fn archive(
        &self,
        source: SourceKind,
        session_id: &str,
        transcript: &Path,
        reason: &str,
    ) -> HookResult {
        let (completion, reason) = match reason {
            "complete" => (CompletionReason::Complete, "complete"),
            "user_exit" => (CompletionReason::Interrupted, "user_exit"),
            other => (CompletionReason::Unknown, other),
        };
        {
            let mut store = StateStore::open_for_source(&self.state, source).unwrap();
            let stop_ts = self.clock.get();
            let end_ts = stop_ts + 1;
            self.clock.set(stop_ts + 10);
            store
                .ingest_agent_stop(session_id, stop_ts, &self.project, transcript)
                .unwrap();
            store
                .ingest_session_end(session_id, end_ts, &self.project, reason, completion, None)
                .unwrap();
        }
        run_archive_worker_for_source(&self.state, source, session_id, WorkerContext::Interactive)
            .unwrap()
    }

    /// Builds one of Munshi's real summary-request envelopes — byte for byte what a summarizer
    /// reads on stdin — over a synthetic session of `event_count` events of `event_bytes` each.
    /// Produced through [`build_summary_input`] rather than hand-written JSON so the fixture can
    /// never drift from the envelope Munshi actually emits, which is the whole basis of the guard.
    fn summary_request_envelope(&self, event_count: usize, event_bytes: usize) -> String {
        let session = NormalizedSession {
            source: SourceKind::Copilot,
            session_id: "5ea0f0ed-0000-4000-8000-000000000001".to_owned(),
            events: (0..event_count)
                .map(|index| NormalizedEvent {
                    kind: if index % 2 == 0 { "user" } else { "assistant" },
                    content: "summarized event ".repeat(event_bytes.div_ceil(17)),
                })
                .collect(),
            user_requests: event_count.div_ceil(2),
            assistant_messages: event_count / 2,
            tool_activities: 0,
            ignored_events: 0,
            source_cursor: event_count as u64,
            source_byte_cursor: 0,
            source_prefix_hash: String::new(),
            source_hash: String::new(),
            source_bytes: 0,
            started_at: None,
            updated_at: None,
            artifact_index: Default::default(),
            opening_summary_request: false,
        };
        let project: ProjectIdentity = inspect_project(&self.project).unwrap();
        let bytes = build_summary_input(&session, &project, 16 * 1024 * 1024).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    /// Writes a Copilot transcript with one user/assistant turn per entry in `requests` — the
    /// shape Copilot CLI records when it answers a Munshi summary request.
    fn write_copilot_session(&self, session_id: &str, requests: &[&str]) -> PathBuf {
        let mut records = vec![json!({
            "id": "r0",
            "timestamp": "2026-07-29T00:00:00.000Z",
            "parentId": null,
            "type": "session.start",
            "data": {"sessionId": session_id},
        })];
        for (index, request) in requests.iter().enumerate() {
            records.push(json!({
                "id": format!("r{}u", index + 1),
                "timestamp": "2026-07-29T00:00:01.000Z",
                "parentId": format!("r{index}"),
                "type": "user.message",
                "data": {"content": request},
            }));
            records.push(json!({
                "id": format!("r{}", index + 1),
                "timestamp": "2026-07-29T00:00:02.000Z",
                "parentId": format!("r{}u", index + 1),
                "type": "assistant.message",
                "data": {
                    "content": "{\"title\":\"A summary the summarizer produced\"}",
                    "messageId": format!("m{index}"),
                },
            }));
        }
        // Copilot resolution keys on the `<session-id>/events.jsonl` layout of its session state.
        self.write_transcript_records(&Path::new(session_id).join("events.jsonl"), &records)
    }

    /// The same session shape in Claude Code's transcript format.
    fn write_claude_session(&self, session_id: &str, requests: &[&str]) -> PathBuf {
        let mut records = Vec::new();
        for (index, request) in requests.iter().enumerate() {
            records.push(json!({
                "type": "user",
                "uuid": format!("u{index}"),
                "parentUuid": if index == 0 { None } else { Some(format!("a{}", index - 1)) },
                "sessionId": session_id,
                "timestamp": "2026-07-29T00:00:01.000Z",
                "cwd": self.project.to_string_lossy(),
                "version": "2.1.44",
                "gitBranch": "main",
                "isSidechain": false,
                "userType": "external",
                "message": {"role": "user", "content": request},
            }));
            records.push(json!({
                "type": "assistant",
                "uuid": format!("a{index}"),
                "parentUuid": format!("u{index}"),
                "sessionId": session_id,
                "timestamp": "2026-07-29T00:00:02.000Z",
                "cwd": self.project.to_string_lossy(),
                "version": "2.1.44",
                "gitBranch": "main",
                "isSidechain": false,
                "userType": "external",
                "message": {
                    "role": "assistant",
                    "id": format!("msg_a{index}"),
                    "model": "claude-synthetic",
                    "content": [{"type": "text", "text": "{\"title\":\"A summary the summarizer produced\"}"}],
                    "stop_reason": "end_turn",
                },
            }));
        }
        self.write_transcript_records(Path::new(&format!("{session_id}.jsonl")), &records)
    }

    fn write_transcript_records(&self, relative: &Path, records: &[serde_json::Value]) -> PathBuf {
        let target = self.directory.path().join("transcripts").join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        let mut body = String::new();
        for record in records {
            body.push_str(&record.to_string());
            body.push('\n');
        }
        fs::write(&target, body).unwrap();
        target
    }

    /// Asserts the issue #50 settled shape a refused session leaves behind: back to `observed`,
    /// stamped with the verdict, and with no archive on disk.
    fn assert_settled_unarchived(&self, source: SourceKind, session_id: &str) {
        let store = StateStore::open_for_source(&self.state, source).unwrap();
        let record = store.get_session(session_id).unwrap().unwrap();
        assert_eq!(record.lifecycle_state, "observed");
        assert!(record.not_archive_worthy_at_ms.is_some());
        assert!(record.markdown_relative_path.is_none());
        assert_eq!(record.current_revision, 0);
    }

    /// How many times the fake summarizer has been invoked in this harness. The exhaust guard's
    /// whole point is that a refused session costs no summarizer call at all, so the tests assert
    /// on this rather than only on the verdict.
    fn summarizer_calls(&self) -> usize {
        fs::read(self.directory.path().join(SUMMARIZER_CALL_TALLY))
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    }

    /// Diagnostic categories recorded against one session, newest last.
    fn diagnostic_categories(&self, source: SourceKind, session_id: &str) -> Vec<String> {
        let connection = Connection::open(self.state.join("munshi.db")).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT d.category FROM diagnostics d
                 JOIN sessions s ON s.id = d.session_id
                 WHERE s.source_session_id = ?1 AND s.source_kind = ?2
                 ORDER BY d.id",
            )
            .unwrap();
        let rows = statement
            .query_map(params![session_id, source.agent_label()], |row| {
                row.get::<_, String>(0)
            })
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    fn read_archive(&self, source: SourceKind, session_id: &str) -> munshi::ArchivedMarkdown {
        let store = StateStore::open_for_source(&self.state, source).unwrap();
        let record = store.get_session(session_id).unwrap().unwrap();
        let relative = record.markdown_relative_path.unwrap();
        parse_archive_markdown(&fs::read_to_string(self.output.join(relative)).unwrap()).unwrap()
    }

    fn archive_commit_count(&self) -> usize {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.output)
            .args(["rev-list", "--count", "--all"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }

    fn archive_commit_subjects(&self) -> Vec<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.output)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    fn archive_commits_touching(&self, relative_path: &str) -> usize {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.output)
            .args(["log", "--format=%H", "--"])
            .arg(relative_path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }
}

/// Name of the tally file [`adapter_summarizer`] appends one byte to per invocation, read back by
/// [`StateHarness::summarizer_calls`]. Each harness owns a temporary directory, so the count is
/// per-test.
const SUMMARIZER_CALL_TALLY: &str = "summarizer-calls";

fn adapter_summarizer(root: &Path) -> PathBuf {
    let script = root.join("adapter-summarizer.sh");
    let body = format!(
        r#"#!/bin/sh
set -eu
printf 'x' >> "{tally}"
input=$(cat)
case "$input" in{rest}"#,
        tally = root.join(SUMMARIZER_CALL_TALLY).display(),
        rest = r#"
  *'"previous_summary"'*)
    printf '%s' '{"title":"Revised adapter session","goal":"Preserve prior work and add the resumed delta.","work_completed":["Preserved prior work.","Added resumed work."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["resume"]}'
    ;;
  *)
    printf '%s' '{"title":"Adapter session","goal":"Archive a vendor-neutral session.","work_completed":["Normalized the harness transcript.","Rendered a durable record."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["adapter"]}'
    ;;
esac
"#,
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script.canonicalize().unwrap()
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

fn test_directory() -> TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-adapter-artifacts");
    fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(root)
        .unwrap()
}
