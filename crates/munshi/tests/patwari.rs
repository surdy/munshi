//! Integration coverage for the Patwari archive-upload client (issue #19).
//!
//! These tests drive the real synchronous [`munshi::PatwariClient`] and its real HTTP client
//! against an in-process fake Patwari daemon, exercising client registration, the create-upload
//! 201 (new) and 200 (resume/duplicate) paths, resumable chunk PUTs (including `chunk_conflict` and
//! resume-after-partial), completion, and the `capture_id` idempotency lifecycle. A separate test
//! drives the state layer directly to prove a retry reuses its capture and a new revision mints a
//! fresh one.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use munshi::{
    ArtifactSource, CaptureContext, INITIAL_ARTIFACT_SET_VERSION, PatwariClient, PatwariError,
    SessionContext, SourceKind, StateStore, assemble_artifact_sources, build_manifest,
    extract_outputs, prepare_artifacts,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const CLIENT_ID: &str = "11111111-1111-4111-8111-111111111111";
const CHUNK_SIZE: u64 = 8;

// ---------------------------------------------------------------------------
// Client-level flow tests
// ---------------------------------------------------------------------------

#[test]
fn registers_and_uploads_a_snapshot_end_to_end() {
    let server = FakePatwari::start();
    let client = PatwariClient::connect(&server.endpoint(), CLIENT_ID).unwrap();

    client
        .register_client(Some("workstation"), Some("Munshi"), &metadata())
        .unwrap();
    assert_eq!(server.registered_client().as_deref(), Some(CLIENT_ID));

    let artifacts = prepare_artifacts(vec![artifact("summary.md", b"ABCDEFGHIJKLMNOP")]);
    let manifest = manifest_for(&artifacts, "sess-1");
    let mut seen_upload_id = None;
    let receipt = client
        .upload_snapshot("capture-1", &manifest, &artifacts, None, |upload_id| {
            seen_upload_id = Some(upload_id.to_owned());
        })
        .unwrap();

    // A 16-byte identity artifact over an 8-byte chunk layout transfers two chunks and completes.
    assert!(receipt.snapshot_id.starts_with("snap-"));
    assert_eq!(receipt.capture_id, "capture-1");
    assert!(seen_upload_id.is_some());
    assert_eq!(server.upload_count(), 1);
    assert_eq!(server.accepted_chunk_count(), 2);
    assert_eq!(server.completed_count(), 1);
    assert_eq!(server.create_status_codes(), vec![201]);
}

#[test]
fn duplicate_capture_id_reuses_the_same_upload_and_snapshot() {
    let server = FakePatwari::start();
    let client = PatwariClient::connect(&server.endpoint(), CLIENT_ID).unwrap();
    let artifacts = prepare_artifacts(vec![artifact("summary.md", b"ABCDEFGHIJKLMNOP")]);
    let manifest = manifest_for(&artifacts, "sess-1");

    let first = client
        .upload_snapshot("capture-1", &manifest, &artifacts, None, |_| {})
        .unwrap();
    let again = client
        .upload_snapshot("capture-1", &manifest, &artifacts, None, |_| {})
        .unwrap();

    // Reusing the capture id with an unchanged manifest resolves to the same snapshot without ever
    // creating a second server upload (201 then 200).
    assert_eq!(first.snapshot_id, again.snapshot_id);
    assert_eq!(server.upload_count(), 1, "no duplicate upload was created");
    assert_eq!(server.create_status_codes(), vec![201, 200]);
}

#[test]
fn reused_capture_id_with_a_changed_manifest_conflicts() {
    let server = FakePatwari::start();
    let client = PatwariClient::connect(&server.endpoint(), CLIENT_ID).unwrap();
    let first_artifacts = prepare_artifacts(vec![artifact("summary.md", b"ABCDEFGHIJKLMNOP")]);
    let first_manifest = manifest_for(&first_artifacts, "sess-1");
    client
        .upload_snapshot("capture-1", &first_manifest, &first_artifacts, None, |_| {})
        .unwrap();

    // The same capture id with a different manifest is a client-side idempotency violation.
    let changed_artifacts = prepare_artifacts(vec![artifact("summary.md", b"DIFFERENT_BYTES!")]);
    let changed_manifest = manifest_for(&changed_artifacts, "sess-1");
    let error = client
        .upload_snapshot(
            "capture-1",
            &changed_manifest,
            &changed_artifacts,
            None,
            |_| {},
        )
        .unwrap_err();
    assert!(
        matches!(error, PatwariError::CaptureConflict),
        "got {error:?}"
    );
    assert_eq!(error.category(), "capture-conflict");
}

#[test]
fn a_chunk_conflict_is_surfaced() {
    let server = FakePatwari::start();
    server.conflict_on_chunk(0, 0);
    let client = PatwariClient::connect(&server.endpoint(), CLIENT_ID).unwrap();
    let artifacts = prepare_artifacts(vec![artifact("summary.md", b"ABCDEFGHIJKLMNOP")]);
    let manifest = manifest_for(&artifacts, "sess-1");

    let error = client
        .upload_snapshot("capture-1", &manifest, &artifacts, None, |_| {})
        .unwrap_err();
    assert!(
        matches!(error, PatwariError::ChunkConflict),
        "got {error:?}"
    );
    assert_eq!(error.category(), "chunk-conflict");
    assert_eq!(
        server.completed_count(),
        0,
        "a conflicted upload never completes"
    );
}

#[test]
fn an_interrupted_upload_resumes_without_reuploading_accepted_chunks() {
    let server = FakePatwari::start();
    let client = PatwariClient::connect(&server.endpoint(), CLIENT_ID).unwrap();
    let artifacts = prepare_artifacts(vec![artifact("summary.md", b"ABCDEFGHIJKLMNOP")]);
    let manifest = manifest_for(&artifacts, "sess-1");

    // Interrupt after the server accepts the first chunk: the second chunk PUT fails transiently.
    server.accept_only(1);
    let mut persisted_upload_id = None;
    let interrupted =
        client.upload_snapshot("capture-1", &manifest, &artifacts, None, |upload_id| {
            persisted_upload_id = Some(upload_id.to_owned());
        });
    assert!(interrupted.is_err(), "the interrupted attempt fails");
    let upload_id = persisted_upload_id.expect("the upload id was persisted before interruption");
    assert_eq!(
        server.accepted_chunk_count(),
        1,
        "only the first chunk was accepted"
    );

    // Resume with the persisted upload id: a status GET reports chunk 0 accepted, so only the
    // missing second chunk is re-sent, and the upload completes.
    server.accept_unlimited();
    let chunk_puts_before = server.chunk_put_count();
    let receipt = client
        .upload_snapshot("capture-1", &manifest, &artifacts, Some(&upload_id), |_| {})
        .unwrap();
    assert!(receipt.snapshot_id.starts_with("snap-"));
    assert_eq!(server.accepted_chunk_count(), 2);
    assert_eq!(server.completed_count(), 1);
    assert_eq!(
        server.chunk_put_count() - chunk_puts_before,
        1,
        "resume re-sends only the one missing chunk, not the already-accepted one"
    );
    // Resume went through a status GET, not a second create.
    assert_eq!(server.upload_count(), 1);
}

// ---------------------------------------------------------------------------
// Snapshot artifact set v1: extracted outputs (issue #20)
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_output_becomes_a_content_addressed_artifact() {
    let threshold = 64;
    let transcript = format!(
        "{}\n{}\n",
        copilot_user("please run the build"),
        copilot_tool_complete("call-1", &"x".repeat(500)),
    )
    .into_bytes();

    let sources = assemble_artifact_sources(
        Some(b"# Summary\n".to_vec()),
        Some(transcript),
        SourceKind::Copilot,
        threshold,
    );
    // Set v1 with one extraction, in canonical (path-sorted) order: the outputs/<sha256> artifact
    // sorts before the fixed roles (issue #33).
    let paths: Vec<&str> = sources.iter().map(|s| s.logical_path.as_str()).collect();
    assert_eq!(sources.len(), 3);
    assert_eq!(paths[1], "summary.md");
    assert_eq!(paths[2], "transcript.jsonl");
    let stem = paths[0]
        .strip_prefix("outputs/")
        .expect("extracted output uses the outputs/ role path")
        .to_owned();
    // The logical path is the bare lowercase hex sha256 of the extracted content itself.
    assert_eq!(sha256_hex(&sources[0].bytes), stem);

    // The extraction flows into the manifest with a matching prefixed original digest.
    let artifacts = prepare_artifacts(sources);
    let manifest = manifest_for(&artifacts, "sess-1");
    let listed: Vec<&str> = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| artifact["logical_path"].as_str().unwrap())
        .collect();
    assert_eq!(
        listed,
        vec![&format!("outputs/{stem}"), "summary.md", "transcript.jsonl"]
    );
    assert_eq!(
        manifest["artifacts"][0]["original_sha256"]
            .as_str()
            .unwrap(),
        format!("sha256:{stem}")
    );
}

#[test]
fn multiple_oversized_outputs_are_hash_sorted_with_correct_digests() {
    let threshold = 64;
    let transcript = format!(
        "{}\n{}\n{}\n",
        copilot_tool_complete("call-1", &"a".repeat(300)),
        copilot_tool_complete("call-2", &"b".repeat(300)),
        copilot_user("hi"),
    )
    .into_bytes();

    let outputs = extract_outputs(&transcript, SourceKind::Copilot, threshold);
    assert_eq!(outputs.len(), 2);
    // Deterministic ordering: ascending by content hash, each digest matching its bytes.
    assert!(outputs[0].sha256 < outputs[1].sha256);
    for output in &outputs {
        assert_eq!(sha256_hex(&output.content), output.sha256);
    }

    // The assembled set is in canonical path order: the hash-sorted outputs (which sort below
    // "summary.md") first, then the fixed roles (issue #33).
    let sources = assemble_artifact_sources(
        Some(b"# Summary\n".to_vec()),
        Some(transcript),
        SourceKind::Copilot,
        threshold,
    );
    let paths: Vec<String> = sources
        .iter()
        .map(|source| source.logical_path.clone())
        .collect();
    assert_eq!(
        paths,
        vec![
            format!("outputs/{}", outputs[0].sha256),
            format!("outputs/{}", outputs[1].sha256),
            "summary.md".to_owned(),
            "transcript.jsonl".to_owned(),
        ]
    );
}

#[test]
fn under_threshold_events_produce_exactly_transcript_and_summary() {
    let threshold = 128 * 1024;
    let transcript = format!(
        "{}\n{}\n",
        copilot_user("a small request"),
        copilot_tool_complete("call-1", "a tiny tool output"),
    )
    .into_bytes();

    assert!(extract_outputs(&transcript, SourceKind::Copilot, threshold).is_empty());
    let sources = assemble_artifact_sources(
        Some(b"# Summary\n".to_vec()),
        Some(transcript),
        SourceKind::Copilot,
        threshold,
    );
    let paths: Vec<&str> = sources.iter().map(|s| s.logical_path.as_str()).collect();
    assert_eq!(paths, vec!["summary.md", "transcript.jsonl"]);
}

#[test]
fn a_grown_revision_keeps_unchanged_extracted_outputs_stable() {
    let threshold = 64;
    let first = format!("{}\n", copilot_tool_complete("call-1", &"a".repeat(300))).into_bytes();
    let mut grown = first.clone();
    grown.extend_from_slice(
        format!("{}\n", copilot_tool_complete("call-2", &"b".repeat(300))).as_bytes(),
    );

    let first_sources = assemble_artifact_sources(
        Some(b"# rev 1\n".to_vec()),
        Some(first.clone()),
        SourceKind::Copilot,
        threshold,
    );
    let grown_sources = assemble_artifact_sources(
        Some(b"# rev 2\n".to_vec()),
        Some(grown),
        SourceKind::Copilot,
        threshold,
    );

    // The untouched first extracted output resolves to the same logical path and bytes in the
    // second revision — dedup-ready: Patwari collapses it to the existing blob.
    let first_output = first_sources
        .iter()
        .find(|s| s.logical_path.starts_with("outputs/"))
        .expect("first revision extracts one output");
    assert!(
        grown_sources
            .iter()
            .any(|s| s.logical_path == first_output.logical_path && s.bytes == first_output.bytes),
        "the unchanged extracted output is byte-identical across revisions"
    );

    // The grown revision re-prepares the larger transcript and gains the second extraction.
    let grown_transcript = grown_sources
        .iter()
        .find(|s| s.logical_path == "transcript.jsonl")
        .unwrap();
    assert_ne!(grown_transcript.bytes, first, "the transcript grew");
    assert_eq!(
        grown_sources
            .iter()
            .filter(|s| s.logical_path.starts_with("outputs/"))
            .count(),
        2
    );
}

#[test]
fn retry_reuses_capture_with_extracted_outputs_and_identical_manifest() {
    let threshold = 64;
    let build_transcript = || {
        format!(
            "{}\n{}\n",
            copilot_user("go"),
            copilot_tool_complete("call-1", &"z".repeat(400)),
        )
        .into_bytes()
    };
    let build_manifest_and_artifacts = || {
        let sources = assemble_artifact_sources(
            Some(b"# Summary\n".to_vec()),
            Some(build_transcript()),
            SourceKind::Copilot,
            threshold,
        );
        assert!(
            sources
                .iter()
                .any(|s| s.logical_path.starts_with("outputs/")),
            "the artifact set carries an extracted output"
        );
        let artifacts = prepare_artifacts(sources);
        let manifest = manifest_for(&artifacts, "sess-1");
        (manifest, artifacts)
    };

    let (manifest, artifacts) = build_manifest_and_artifacts();
    // Re-deriving the whole set from identical inputs yields a byte-identical canonical manifest,
    // so a reused capture id never trips Patwari's changed-manifest conflict.
    let (manifest_again, _) = build_manifest_and_artifacts();
    assert_eq!(manifest.to_string(), manifest_again.to_string());

    let server = FakePatwari::start();
    let client = PatwariClient::connect(&server.endpoint(), CLIENT_ID).unwrap();
    let first = client
        .upload_snapshot("capture-1", &manifest, &artifacts, None, |_| {})
        .unwrap();
    let again = client
        .upload_snapshot("capture-1", &manifest, &artifacts, None, |_| {})
        .unwrap();
    assert_eq!(first.snapshot_id, again.snapshot_id);
    assert_eq!(server.upload_count(), 1, "no duplicate upload was created");
    assert_eq!(server.create_status_codes(), vec![201, 200]);
}

// ---------------------------------------------------------------------------
// Chunk routing by logical path against a canonicalizing server (issue #33)
// ---------------------------------------------------------------------------

/// The live failure of issue #33: a snapshot with extracted outputs assembles a local artifact
/// list whose order once differed from Patwari's canonical (path-sorted) `artifact_index` space,
/// so a positional client PUT the wrong artifact's bytes and the server 422'd on the negotiated
/// chunk layout. The fake now canonicalizes exactly like the real server; this round-trip proves
/// every chunk of every artifact lands and the receipt completes.
#[test]
fn a_snapshot_with_extracted_outputs_uploads_every_chunk_and_completes() {
    let threshold = 64;
    let transcript = format!(
        "{}\n{}\n",
        copilot_user("run the build"),
        copilot_tool_complete("call-1", &"x".repeat(500)),
    )
    .into_bytes();
    let sources = assemble_artifact_sources(
        Some(b"# Summary\n\nA body that says enough to be worth archiving.\n".to_vec()),
        Some(transcript),
        SourceKind::Copilot,
        threshold,
    );
    assert!(
        sources
            .iter()
            .any(|s| s.logical_path.starts_with("outputs/")),
        "the snapshot carries an extracted output"
    );
    let artifacts = prepare_artifacts(sources);
    let manifest = manifest_for(&artifacts, "sess-outputs");
    let expected_chunks: u64 = artifacts
        .iter()
        .map(|artifact| (artifact.stored_bytes.len() as u64).div_ceil(CHUNK_SIZE))
        .sum();

    let server = FakePatwari::start();
    let client = PatwariClient::connect(&server.endpoint(), CLIENT_ID).unwrap();
    let receipt = client
        .upload_snapshot("capture-outputs", &manifest, &artifacts, None, |_| {})
        .expect("a snapshot with extracted outputs uploads cleanly");
    assert!(receipt.snapshot_id.starts_with("snap-"));
    assert_eq!(receipt.artifact_count, artifacts.len() as u32);
    assert_eq!(
        server.accepted_chunk_count() as u64,
        expected_chunks,
        "every chunk of every artifact landed"
    );
    assert_eq!(server.completed_count(), 1, "the receipt completed");
}

/// Path matching is the load-bearing fix, not order agreement: the client hands its artifacts over
/// in a deliberately non-canonical order with distinct sizes, and the fake (like the real Patwari)
/// indexes them path-sorted. Positional routing would trip the fake's negotiated chunk-length
/// check — the live 422 of issue #33 — while path-based routing uploads each artifact's own bytes.
#[test]
fn chunk_routing_matches_server_artifacts_by_path_not_position() {
    let artifacts = prepare_artifacts(vec![
        artifact(
            "transcript.jsonl",
            b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n{\"type\":\"tool\"}\n",
        ),
        artifact("summary.md", b"# Short\n"),
    ]);
    let manifest = manifest_for(&artifacts, "sess-unordered");
    let expected_chunks: u64 = artifacts
        .iter()
        .map(|artifact| (artifact.stored_bytes.len() as u64).div_ceil(CHUNK_SIZE))
        .sum();

    let server = FakePatwari::start();
    let client = PatwariClient::connect(&server.endpoint(), CLIENT_ID).unwrap();
    let receipt = client
        .upload_snapshot("capture-unordered", &manifest, &artifacts, None, |_| {})
        .expect("a non-canonical local order still routes every chunk to the right artifact");
    assert!(receipt.snapshot_id.starts_with("snap-"));
    assert_eq!(server.accepted_chunk_count() as u64, expected_chunks);
    assert_eq!(server.completed_count(), 1);
}

// ---------------------------------------------------------------------------
// State-layer capture lifecycle
// ---------------------------------------------------------------------------

#[test]
fn state_reuses_capture_on_retry_and_mints_fresh_for_a_new_revision() {
    let directory = TempDir::new().unwrap();
    let state_dir = directory.path().join("munshi-home");
    let session = "22222222-2222-4222-8222-222222222222";
    let endpoint = "http://127.0.0.1:1";

    let mut store = StateStore::open(&state_dir).unwrap();
    store
        .ingest_agent_stop(
            session,
            10_000,
            Path::new("/tmp/project"),
            Path::new("/tmp/t.jsonl"),
        )
        .unwrap();

    // First attempt for revision 1 mints a fresh capture and captured_at.
    let first = store
        .prepare_archive_capture(session, endpoint, 1, "capture-A", "2026-07-25T00:00:00Z")
        .unwrap();
    assert_eq!(first.capture_id, "capture-A");
    assert_eq!(first.captured_at, "2026-07-25T00:00:00Z");
    assert!(first.resume_upload_id.is_none());

    // A recorded upload id and a retry of the SAME revision reuse the exact capture, captured_at,
    // and resumable upload id — a fresh id passed in is ignored.
    store
        .record_archive_upload_id(session, endpoint, "upl-xyz")
        .unwrap();
    let retry = store
        .prepare_archive_capture(
            session,
            endpoint,
            1,
            "capture-IGNORED",
            "2026-07-25T09:99:99Z",
        )
        .unwrap();
    assert_eq!(retry.capture_id, "capture-A");
    assert_eq!(retry.captured_at, "2026-07-25T00:00:00Z");
    assert_eq!(retry.resume_upload_id.as_deref(), Some("upl-xyz"));

    // A DISTINCT snapshot attempt (a new revision) mints a fresh capture and clears the resume id.
    let next = store
        .prepare_archive_capture(session, endpoint, 2, "capture-B", "2026-07-25T01:00:00Z")
        .unwrap();
    assert_eq!(next.capture_id, "capture-B");
    assert_eq!(next.captured_at, "2026-07-25T01:00:00Z");
    assert!(next.resume_upload_id.is_none());

    // A completed revision is not re-attempted with the same capture; a later same-revision prepare
    // after success mints fresh (terminal state is not reusable).
    store
        .record_archive_upload_success(
            session,
            endpoint,
            &munshi::ArchiveUploadSuccess {
                uploaded_revision: 2,
                uploaded_summary_hash: "hash2".to_owned(),
                snapshot_id: "snap-2".to_owned(),
                uploaded_artifact_paths: vec![
                    "summary.md".to_owned(),
                    "transcript.jsonl".to_owned(),
                ],
            },
        )
        .unwrap();
    let after_success = store
        .prepare_archive_capture(session, endpoint, 2, "capture-C", "2026-07-25T02:00:00Z")
        .unwrap();
    assert_eq!(after_success.capture_id, "capture-C");
}

// ---------------------------------------------------------------------------
// CLI backfill of sessions archived while upload was disabled (issue #32)
// ---------------------------------------------------------------------------

const BACKFILL_SESSION: &str = "33333333-3333-4333-8333-333333333333";

/// A session archived while upload was disabled has no `archive_uploads` row, so neither the
/// post-archive worker nor `archive-upload retry` ever uploads it. `archive-upload backfill` finds
/// it, runs the normal upload path against the (fake) server, and records the row; a second run
/// finds no candidates and never contacts the server again.
#[test]
fn backfill_uploads_archived_sessions_without_rows_and_is_idempotent() {
    let harness = CliHarness::new();
    harness.register();
    harness.archive_session(BACKFILL_SESSION);

    // Archived with upload disabled: no upload row exists anywhere.
    let status = harness.archive_upload_status();
    assert_eq!(status["total"], 0);

    let server = FakePatwari::start();
    harness.configure_and_enable(&server.endpoint());

    let (report, success) = harness.backfill();
    assert!(success, "backfill exits zero when nothing fails");
    assert_eq!(report["command"], "archive-upload-backfill");
    assert_eq!(report["candidates"], 1);
    assert_eq!(report["uploaded"], 1);
    assert_eq!(report["already_uploaded"], 0);
    assert_eq!(report["skipped"], 0);
    assert_eq!(report["failed"], 0);
    assert_eq!(report["items"][0]["session_id"], BACKFILL_SESSION);
    assert_eq!(report["items"][0]["outcome"]["result"], "uploaded");
    assert_eq!(server.completed_count(), 1);
    // The upload registered the durable client UUID minted at configure time.
    let client_id = harness
        .configured_client_id()
        .expect("configure minted a persistent client UUID");
    assert_eq!(
        server.registered_client().as_deref(),
        Some(client_id.as_str()),
        "backfill uploads under the persistent configured client UUID"
    );

    // The row is now recorded as uploaded for the configured endpoint.
    let status = harness.archive_upload_status();
    assert_eq!(status["total"], 1);
    assert_eq!(status["uploaded"], 1);
    assert_eq!(status["items"][0]["session_id"], BACKFILL_SESSION);
    assert_eq!(status["items"][0]["state"], "uploaded");
    assert!(status["items"][0]["snapshot_id"].is_string());

    // The uploaded snapshot is self-contained (ADR 0009, issue #47): the summary and the verbatim
    // transcript are both in the manifest, and the ledger records the set that was uploaded.
    assert_eq!(
        manifest_paths(&server.manifest(0)),
        vec!["summary.md", "transcript.jsonl"],
    );
    assert_eq!(
        harness.recorded_artifact_paths(BACKFILL_SESSION).as_deref(),
        Some("summary.md\ntranscript.jsonl"),
    );

    // Idempotent: the recorded row is a self-contained snapshot of the current revision, so a
    // second run finds no candidates and performs no further server work.
    let (again, success) = harness.backfill();
    assert!(success);
    assert_eq!(again["candidates"], 0);
    assert_eq!(again["uploaded"], 0);
    assert_eq!(server.completed_count(), 1, "no second upload happened");
    assert_eq!(server.upload_count(), 1);
}

// ---------------------------------------------------------------------------
// Full-snapshot self-containment (issue #47)
// ---------------------------------------------------------------------------

const NO_TRANSCRIPT_SESSION: &str = "47474747-4747-4747-8747-474747474741";
const SUMMARY_ONLY_SESSION: &str = "47474747-4747-4747-8747-474747474742";
const LEGACY_LEDGER_SESSION: &str = "47474747-4747-4747-8747-474747474743";

/// ADR 0009 archives *full* snapshots, so a session whose transcript munshi cannot read must not
/// upload a summary-only one. `rebuild-state` reconstructs a session from its archive Markdown
/// alone and never learns a transcript path; the upload path used to read the transcript
/// best-effort and silently uploaded `artifacts: ['summary.md']` for exactly those sessions
/// (issue #47). Now the incomplete snapshot is skipped, no server work happens, and the session
/// uploads the complete set as soon as its transcript is readable again.
#[test]
fn a_session_without_a_readable_transcript_never_uploads_a_summary_only_snapshot() {
    let harness = CliHarness::new();
    harness.register();
    harness.archive_session(NO_TRANSCRIPT_SESSION);
    // Exactly what a `rebuild-state` row looks like: archived, summarized, no transcript path.
    let transcript = harness.forget_transcript_path(NO_TRANSCRIPT_SESSION);

    let server = FakePatwari::start();
    harness.configure_and_enable(&server.endpoint());

    let (report, success) = harness.backfill();
    assert!(success, "an unassemblable snapshot is not a failure");
    assert_eq!(report["candidates"], 1);
    assert_eq!(report["uploaded"], 0);
    assert_eq!(report["skipped"], 1);
    assert_eq!(report["failed"], 0);
    assert_eq!(report["items"][0]["outcome"]["result"], "skipped");
    assert_eq!(
        report["items"][0]["outcome"]["reason"], "missing-transcript.jsonl",
        "report: {report}"
    );
    assert_eq!(
        server.upload_count(),
        0,
        "no summary-only snapshot may reach the archive"
    );

    // The skip costs no bounded attempt and is not terminal: the row is pending, and once the
    // transcript is readable the ordinary retry path uploads the complete set.
    let status = harness.archive_upload_status();
    assert_eq!(status["items"][0]["state"], "pending");
    assert_eq!(status["items"][0]["attempts"], 0);

    harness.restore_transcript_path(NO_TRANSCRIPT_SESSION, &transcript);
    let retried = harness.json(&["archive-upload", "retry", NO_TRANSCRIPT_SESSION, "--json"]);
    assert_eq!(retried["items"][0]["outcome"]["result"], "uploaded");
    assert_eq!(
        manifest_paths(&server.manifest(0)),
        vec!["summary.md", "transcript.jsonl"],
        "the recovered upload carries the full artifact set"
    );
}

/// The backfill mechanism for snapshots already in the archive (issue #47): a session whose latest
/// uploaded snapshot lacks `transcript.jsonl` is a backfill candidate even though its ledger row is
/// `uploaded` at the current revision. The re-upload mints a fresh capture carrying the complete
/// set; the old summary-only snapshot stays in the archive as historical provenance (Patwari
/// snapshots are immutable). A third run has nothing left to converge.
#[test]
fn backfill_reuploads_a_recorded_summary_only_snapshot_as_a_full_one() {
    let harness = CliHarness::new();
    harness.register();
    harness.archive_session(SUMMARY_ONLY_SESSION);
    let server = FakePatwari::start();
    harness.configure_and_enable(&server.endpoint());

    let (report, success) = harness.backfill();
    assert!(success);
    assert_eq!(report["uploaded"], 1);

    // Rewrite the ledger to what an older client left behind for a session whose transcript it
    // could not read: an `uploaded` row for this exact revision whose snapshot was summary-only.
    harness.record_artifact_paths(SUMMARY_ONLY_SESSION, Some("summary.md"));

    let (report, success) = harness.backfill();
    assert!(success);
    assert_eq!(report["candidates"], 1, "report: {report}");
    assert_eq!(report["uploaded"], 1);
    assert_eq!(report["already_uploaded"], 0);
    assert_eq!(report["items"][0]["session_id"], SUMMARY_ONLY_SESSION);
    assert_eq!(server.completed_count(), 2, "the full snapshot re-uploaded");
    assert_eq!(
        manifest_paths(&server.manifest(1)),
        vec!["summary.md", "transcript.jsonl"],
    );
    // A distinct capture: reusing the summary-only snapshot's capture id with a changed manifest
    // would be an idempotency violation the server rejects.
    assert_ne!(
        server.manifest(0)["capture"]["captured_at"],
        Value::Null,
        "both captures are recorded"
    );

    let (again, success) = harness.backfill();
    assert!(success);
    assert_eq!(again["candidates"], 0);
    assert_eq!(server.completed_count(), 2, "convergence is a fixed point");
}

/// A row written before the ledger recorded artifact sets carries no set at all, so what it
/// uploaded is unknown — not known-complete. Backfill re-verifies it exactly once; the re-upload
/// is cheap (Patwari deduplicates blobs by content hash and coalesces an identical snapshot
/// fingerprint) and afterwards the row records its set and is never a candidate again.
#[test]
fn backfill_reverifies_a_row_that_predates_the_recorded_artifact_set_exactly_once() {
    let harness = CliHarness::new();
    harness.register();
    harness.archive_session(LEGACY_LEDGER_SESSION);
    let server = FakePatwari::start();
    harness.configure_and_enable(&server.endpoint());
    let (report, _) = harness.backfill();
    assert_eq!(report["uploaded"], 1);

    harness.record_artifact_paths(LEGACY_LEDGER_SESSION, None);
    let (report, success) = harness.backfill();
    assert!(success);
    assert_eq!(report["candidates"], 1, "report: {report}");
    assert_eq!(report["uploaded"], 1);
    assert_eq!(
        manifest_paths(&server.manifest(1)),
        vec!["summary.md", "transcript.jsonl"],
    );

    let (again, _) = harness.backfill();
    assert_eq!(again["candidates"], 0);
    assert_eq!(server.completed_count(), 2);
}

// ---------------------------------------------------------------------------
// Placeholder-summary durability floor (issue #43)
// ---------------------------------------------------------------------------

const FLOOR_SESSION: &str = "43434343-4343-4343-8343-434343434343";
const CAP_SESSION: &str = "43434343-4343-4343-8343-434343434344";
const PLACEHOLDER_TAG: &str = "munshi-placeholder-summary";

/// The durability floor (issue #43): a session whose summarizer fails deterministically must not
/// stay unarchived forever. Below the park threshold nothing placeholders; at the threshold the
/// session archives with an explicit machine-generated placeholder summary, stays parked (a real
/// summary is still owed), and the full transcript uploads byte-identically to Patwari alongside
/// the flagged `summary.md`. A later successful `munshi retry` replaces the placeholder with a
/// real summary through the normal revision machinery and re-uploads a new snapshot.
#[test]
fn placeholder_floor_archives_and_uploads_at_the_park_threshold_and_retry_replaces_it() {
    let harness = CliHarness::new();
    let summarizer = harness.toggleable_summarizer("floor-count");
    harness.register_with_summarizer(&summarizer, &[]);
    let server = FakePatwari::start();
    harness.configure_and_enable(&server.endpoint());

    let transcript = harness.write_transcript(FLOOR_SESSION);
    harness.hook(
        "agent-stop",
        &json!({
            "sessionId": FLOOR_SESSION,
            "timestamp": 10_000,
            "cwd": harness.project,
            "transcriptPath": transcript,
            "stopReason": "end_turn",
        }),
    );
    harness.hook(
        "session-end",
        &json!({
            "sessionId": FLOOR_SESSION,
            "timestamp": 10_001,
            "cwd": harness.project,
            "reason": "complete",
        }),
    );
    // The hook-spawned worker makes attempt 1 and fails; wait for its verdict to land.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while harness.session_park(FLOOR_SESSION).1.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "first failing attempt never recorded a backoff"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Attempts 2-4: still below the park threshold, so no placeholder engages — no archive
    // Markdown exists and Patwari never sees an upload.
    for _ in 0..3 {
        harness.make_retry_due(FLOOR_SESSION);
        let _ = harness.run_worker(FLOOR_SESSION);
    }
    assert_eq!(harness.summarizer_calls("floor-count"), 4);
    assert!(
        !harness.archive_file(FLOOR_SESSION).exists(),
        "below the park threshold nothing may placeholder-archive"
    );
    assert_eq!(server.upload_count(), 0);
    assert!(harness.status_text().contains("placeholder=0"));

    // Attempt 5 reaches the park threshold: the placeholder floor archives and uploads.
    harness.make_retry_due(FLOOR_SESSION);
    let _ = harness.run_worker(FLOOR_SESSION);
    assert_eq!(harness.summarizer_calls("floor-count"), 5);
    let markdown = std::fs::read_to_string(harness.archive_file(FLOOR_SESSION))
        .expect("placeholder floor wrote the archive Markdown");
    assert!(
        markdown.contains("summary_placeholder: true"),
        "archive must be visibly flagged: {markdown}"
    );
    assert!(markdown.contains(PLACEHOLDER_TAG), "markdown: {markdown}");
    assert!(
        markdown.contains("Summary unavailable: summarizer rejected oversized input (munshi#43)."),
        "markdown: {markdown}"
    );
    assert!(markdown.contains("summary_revision: 1"));

    // The session still owes a real summary: parked under its real category, streak intact.
    let (category, next_retry, streak) = harness.session_park(FLOOR_SESSION);
    assert_eq!(category.as_deref(), Some("summary-failed"));
    assert_eq!(next_retry, Some(-1));
    assert_eq!(streak, 5);
    let status = harness.status_text();
    assert!(status.contains("placeholder=1"), "status: {status}");
    assert!(status.contains("parked=1"), "status: {status}");

    // The snapshot uploaded: the transcript byte-identical to the source, the summary the exact
    // flagged Markdown. Manifest digests are the client's own sha256 declarations, which the fake
    // (like the real server) verified chunk-by-chunk during the upload.
    assert_eq!(server.completed_count(), 1);
    let manifest = server.manifest(0);
    let digest_of = |path: &str| {
        manifest["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|artifact| artifact["logical_path"] == path)
            .unwrap_or_else(|| panic!("manifest missing {path}"))["original_sha256"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let transcript_bytes = std::fs::read(&transcript).unwrap();
    assert_eq!(
        digest_of("transcript.jsonl"),
        format!("sha256:{}", sha256_hex(&transcript_bytes)),
        "uploaded transcript must be byte-identical to the source"
    );
    assert_eq!(
        digest_of("summary.md"),
        format!("sha256:{}", sha256_hex(markdown.as_bytes())),
        "uploaded summary.md must be the flagged placeholder document"
    );
    let uploads = harness.archive_upload_status();
    assert_eq!(uploads["items"][0]["state"], "uploaded");

    // A later successful retry replaces the placeholder with a real summary (a new revision
    // through the existing revision machinery) and re-uploads a new snapshot.
    std::fs::write(harness.success_flag(), b"").unwrap();
    let retried = harness.json(&["retry", FLOOR_SESSION, "--json"]);
    assert_eq!(retried["result"], "archived", "retry report: {retried}");
    assert_eq!(harness.summarizer_calls("floor-count"), 6);
    let replaced = std::fs::read_to_string(harness.archive_file(FLOOR_SESSION)).unwrap();
    assert!(replaced.contains("summary_revision: 2"), "{replaced}");
    assert!(
        !replaced.contains("summary_placeholder"),
        "real summary must drop the placeholder flag: {replaced}"
    );
    assert!(!replaced.contains(PLACEHOLDER_TAG));
    assert!(replaced.contains("Recovered real summary"));
    assert_eq!(server.completed_count(), 2);
    let second = server.manifest(1);
    assert_eq!(second["capture"]["source_cursor"], "2");
    // The replacement revision is a full snapshot too, not a summary-only re-upload (issue #47).
    assert_eq!(
        manifest_paths(&second),
        vec!["summary.md", "transcript.jsonl"],
    );
    let status = harness.status_text();
    assert!(status.contains("placeholder=0"), "status: {status}");
    assert!(status.contains("archived=1"), "status: {status}");
}

/// An input-capacity verdict is deterministic on the first attempt, so the floor engages
/// immediately with a distinct category — no summarizer invocation is ever attempted or billed.
/// Since issue #52 the input cap must stay at or above the chunk threshold, so the reachable
/// deterministic verdict is the genuinely unchunkable one: a threshold no split can bring a
/// request under, which fails before the first chunk invocation.
#[test]
fn input_cap_violation_placeholders_immediately_with_a_distinct_category() {
    let harness = CliHarness::new();
    let summarizer = harness.toggleable_summarizer("cap-count");
    harness.register_with_summarizer(&summarizer, &["--chunk-threshold-bytes", "128"]);
    let server = FakePatwari::start();
    harness.configure_and_enable(&server.endpoint());

    let transcript = harness.write_transcript(CAP_SESSION);
    harness.hook(
        "agent-stop",
        &json!({
            "sessionId": CAP_SESSION,
            "timestamp": 10_000,
            "cwd": harness.project,
            "transcriptPath": transcript,
            "stopReason": "end_turn",
        }),
    );
    harness.hook(
        "session-end",
        &json!({
            "sessionId": CAP_SESSION,
            "timestamp": 10_001,
            "cwd": harness.project,
            "reason": "complete",
        }),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !harness.archive_file(CAP_SESSION).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "input-cap violation never placeholder-archived"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let markdown = std::fs::read_to_string(harness.archive_file(CAP_SESSION))
        .expect("input-cap placeholder floor wrote the archive Markdown");
    assert!(markdown.contains("summary_placeholder: true"), "{markdown}");
    assert!(markdown.contains(PLACEHOLDER_TAG));
    assert!(
        markdown.contains(
            "Summary unavailable: normalized input exceeds the configured summarizer input limit (munshi#43)."
        ),
        "markdown: {markdown}"
    );
    assert_eq!(
        harness.summarizer_calls("cap-count"),
        0,
        "the cap violation is detected before the summarizer runs"
    );

    // Distinct category (issue #43 direction c): the park records the input-cap class, not the
    // generic summarizer verdict, and the diagnostic distinguishes the cause.
    let (category, next_retry, _) = harness.session_park(CAP_SESSION);
    assert_eq!(category.as_deref(), Some("summary-input-limit"));
    assert_eq!(next_retry, Some(-1));
    let diagnostic: (String, Option<String>) =
        rusqlite::Connection::open(harness.state.join("munshi.db"))
            .unwrap()
            .query_row(
                "SELECT category,cause_category FROM diagnostics
                 WHERE category='placeholder-archived' ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("placeholder diagnostic recorded");
    assert_eq!(diagnostic.1.as_deref(), Some("summary-input-limit"));

    // The transcript still made it into the durable archive (the detached worker uploads
    // downstream of the placeholder archive, so poll briefly).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while server.completed_count() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "placeholder snapshot was never uploaded"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(harness.status_text().contains("placeholder=1"));
}

/// With upload disabled (or never configured) backfill refuses up front with the same clear
/// guard message the retry path uses, uploading nothing.
#[test]
fn backfill_is_a_guarded_no_op_when_upload_is_disabled() {
    let harness = CliHarness::new();
    harness.register();
    harness.archive_session(BACKFILL_SESSION);

    let output = harness.munshi(&["archive-upload", "backfill"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("archive upload is not enabled"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Nothing was scanned into an upload row.
    assert_eq!(harness.archive_upload_status()["total"], 0);
}

/// Drives the real munshi binary: registration, one hook-driven archive lifecycle, and the
/// `archive-upload` CLI. Mirrors the delivery-test harness, narrowed to what backfill needs.
struct CliHarness {
    #[allow(dead_code)]
    directory: TempDir,
    copilot_home: PathBuf,
    state: PathBuf,
    output: PathBuf,
    project: PathBuf,
}

impl CliHarness {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/munshi-patwari-test-artifacts");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("backfill-case-")
            .tempdir_in(root)
            .unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        Self {
            copilot_home: directory.path().join("copilot-home"),
            state: directory.path().join("munshi-home"),
            output: directory.path().join("archives"),
            project,
            directory,
        }
    }

    /// Runs the binary with `args` plus `--state-dir`.
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

    fn register(&self) {
        let summarizer = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/manual/fake-summarizer/status-contract.sh");
        std::fs::set_permissions(&summarizer, std::fs::Permissions::from_mode(0o755)).unwrap();
        self.register_with_summarizer(&summarizer.canonicalize().unwrap(), &[]);
    }

    fn register_with_summarizer(&self, summarizer: &Path, extra_args: &[&str]) {
        let output = Command::new(env!("CARGO_BIN_EXE_munshi"))
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
            .arg("5000")
            .args(extra_args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_cli_success(&output);
    }

    /// A summarizer that counts every invocation and fails (exit 7) until the harness's
    /// `allow-success` control file exists, after which it emits one valid real summary.
    fn toggleable_summarizer(&self, count_name: &str) -> PathBuf {
        let script = self.directory.path().join(format!("{count_name}.sh"));
        let count = self.directory.path().join(count_name);
        let flag = self.success_flag();
        let body = format!(
            "#!/bin/sh\nset -eu\ncat >/dev/null\nprintf x >> '{}'\n[ -e '{}' ] || exit 7\nprintf '%s' '{}'\n",
            count.display(),
            flag.display(),
            r#"{"title":"Recovered real summary","goal":"Summarize after the placeholder floor.","work_completed":["Produced the real summary on retry."],"decisions":[],"files_changed":[],"commands_and_validation":[],"open_items":[],"tags":["recovered"]}"#,
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.canonicalize().unwrap()
    }

    fn success_flag(&self) -> PathBuf {
        self.directory.path().join("allow-success")
    }

    fn summarizer_calls(&self, count_name: &str) -> usize {
        std::fs::read(self.directory.path().join(count_name))
            .map(|marks| marks.len())
            .unwrap_or(0)
    }

    /// Simulates a session's scheduled backoff having elapsed (issue #38 test technique) so
    /// escalation can be driven without waiting out real delays.
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

    fn run_worker(&self, session_id: &str) -> Output {
        self.munshi(&["hook-worker", "--session-id", session_id])
    }

    fn status_text(&self) -> String {
        String::from_utf8_lossy(&self.munshi(&["status"]).stdout).into_owned()
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

    fn configure_and_enable(&self, endpoint: &str) {
        assert_cli_success(&self.munshi(&["archive-upload", "configure", "--endpoint", endpoint]));
        assert_cli_success(&self.munshi(&["archive-upload", "enable"]));
    }

    fn configured_client_id(&self) -> Option<String> {
        let config: Value =
            serde_json::from_slice(&std::fs::read(self.state.join("config.json")).unwrap())
                .unwrap();
        config["archive_upload"]["client_id"]
            .as_str()
            .map(ToOwned::to_owned)
    }

    fn archive_upload_status(&self) -> Value {
        self.json(&["archive-upload", "status", "--json"])
    }

    fn database(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.state.join("munshi.db")).unwrap()
    }

    /// Drops a session's recorded transcript path, reproducing a `rebuild-state` row (issue #47):
    /// archived and summarized from its Markdown alone, with no transcript munshi can read.
    /// Returns the path it forgot.
    fn forget_transcript_path(&self, session_id: &str) -> String {
        let connection = self.database();
        let path: String = connection
            .query_row(
                "SELECT transcript_path FROM sessions WHERE source_session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        let changed = connection
            .execute(
                "UPDATE sessions SET transcript_path=NULL WHERE source_session_id=?1",
                [session_id],
            )
            .unwrap();
        assert_eq!(changed, 1);
        path
    }

    fn restore_transcript_path(&self, session_id: &str, path: &str) {
        let changed = self
            .database()
            .execute(
                "UPDATE sessions SET transcript_path=?2 WHERE source_session_id=?1",
                rusqlite::params![session_id, path],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }

    /// Rewrites the artifact set an upload row records: `Some` for a snapshot known to have
    /// carried exactly those logical paths, `None` for a row written before the ledger recorded
    /// them at all.
    fn record_artifact_paths(&self, session_id: &str, paths: Option<&str>) {
        let changed = self
            .database()
            .execute(
                "UPDATE archive_uploads SET uploaded_artifact_paths=?2
                 WHERE session_id=(SELECT id FROM sessions WHERE source_session_id=?1)",
                rusqlite::params![session_id, paths],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }

    fn recorded_artifact_paths(&self, session_id: &str) -> Option<String> {
        self.database()
            .query_row(
                "SELECT uploaded_artifact_paths FROM archive_uploads
                 WHERE session_id=(SELECT id FROM sessions WHERE source_session_id=?1)",
                [session_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn backfill(&self) -> (Value, bool) {
        let output = self.munshi(&["archive-upload", "backfill", "--json"]);
        assert!(
            !output.stdout.is_empty(),
            "empty stdout; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = serde_json::from_slice(&output.stdout).expect("valid JSON");
        (report, output.status.success())
    }

    /// Writes a transcript and drives one full hook-driven archive lifecycle.
    fn archive_session(&self, session_id: &str) {
        let transcript = self.write_transcript(session_id);
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
        assert_cli_success(&self.munshi(&[
            "hook",
            "wait",
            "--session-id",
            session_id,
            "--timeout-ms",
            "10000",
        ]));
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

    fn write_transcript(&self, session_id: &str) -> PathBuf {
        let path = self
            .copilot_home
            .join("session-state")
            .join(session_id)
            .join("events.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = [
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
                "data": {"content": "please summarize the build"},
            }),
            json!({
                "id": "initial-assistant",
                "timestamp": "2026-07-12T00:00:02.000Z",
                "parentId": "initial-user",
                "type": "assistant.message",
                "data": {"content": "done", "messageId": "initial-message"},
            }),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        std::fs::write(&path, content).unwrap();
        path.canonicalize().unwrap()
    }
}

/// The logical paths a recorded manifest lists, in the manifest's own (canonical) order.
fn manifest_paths(manifest: &Value) -> Vec<String> {
    manifest["artifacts"]
        .as_array()
        .expect("manifest lists artifacts")
        .iter()
        .map(|artifact| artifact["logical_path"].as_str().unwrap().to_owned())
        .collect()
}

fn assert_cli_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// Real Patwari end-to-end (opt-in, ignored by default)
// ---------------------------------------------------------------------------

/// Drives the real client against a locally built Patwari server, proving the manifest shape,
/// digest prefixes, chunk headers, and completion contract match a real peer — not just the fake.
///
/// Opt in by pointing `PATWARI_SERVER_BIN` at a built `patwari-server` binary, for example:
/// `cargo build -p patwari-server` in the patwari repo, then
/// `PATWARI_SERVER_BIN=/path/to/target/debug/patwari-server \
///   cargo test -p munshi --test patwari -- --ignored real_patwari`.
#[test]
#[ignore = "requires a locally built patwari-server via PATWARI_SERVER_BIN"]
fn real_patwari_accepts_registration_upload_and_resume() {
    let Ok(binary) = std::env::var("PATWARI_SERVER_BIN") else {
        eprintln!("PATWARI_SERVER_BIN not set; skipping real Patwari end-to-end test");
        return;
    };
    let data_dir = TempDir::new().unwrap();
    // Reserve a port, then let the server bind it.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut server = std::process::Command::new(&binary)
        .arg("serve")
        .env("PATWARI_DATA_DIR", data_dir.path())
        .env("PATWARI_BIND_ADDR", format!("127.0.0.1:{port}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn patwari-server");
    let endpoint = format!("http://127.0.0.1:{port}");
    let guard = ChildGuard(&mut server);

    // Wait for the server to accept connections by retrying a registration.
    let client = PatwariClient::connect(&endpoint, CLIENT_ID).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if client
            .register_client(Some("test-host"), None, &metadata())
            .is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server never became ready"
        );
        thread::sleep(std::time::Duration::from_millis(100));
    }

    let artifacts = prepare_artifacts(vec![
        artifact(
            "summary.md",
            b"# Real Patwari end-to-end\n\nBody content that compresses well well well.",
        ),
        artifact(
            "transcript.jsonl",
            b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n",
        ),
    ]);
    let manifest = manifest_for(&artifacts, "real-sess-1");
    let receipt = client
        .upload_snapshot("real-capture-1", &manifest, &artifacts, None, |_| {})
        .expect("real upload succeeds");
    assert!(!receipt.snapshot_id.is_empty());
    assert_eq!(receipt.artifact_count, 2);

    // Reusing the same capture id and manifest is idempotent: the same snapshot, no error.
    let again = client
        .upload_snapshot("real-capture-1", &manifest, &artifacts, None, |_| {})
        .expect("duplicate capture is idempotent");
    assert_eq!(again.snapshot_id, receipt.snapshot_id);

    drop(guard);
}

/// Kills the spawned server when the test scope ends.
struct ChildGuard<'a>(&'a mut std::process::Child);
impl Drop for ChildGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn metadata() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    map.insert("munshi_version".to_owned(), "0.1.0".to_owned());
    map
}

fn artifact(logical_path: &str, bytes: &[u8]) -> ArtifactSource {
    ArtifactSource {
        logical_path: logical_path.to_owned(),
        media_type: Some("text/markdown".to_owned()),
        bytes: bytes.to_vec(),
    }
}

/// A single version-pinned Copilot `user.message` transcript record.
fn copilot_user(text: &str) -> String {
    json!({
        "id": "user-record",
        "timestamp": "2026-07-25T00:00:00Z",
        "parentId": "root",
        "type": "user.message",
        "data": { "content": text },
    })
    .to_string()
}

/// A single Copilot `tool.execution_complete` record whose textual result is `output`. Its
/// normalized event content is what the extractor content-addresses when it exceeds the threshold.
fn copilot_tool_complete(call_id: &str, output: &str) -> String {
    json!({
        "id": call_id,
        "timestamp": "2026-07-25T00:00:00Z",
        "parentId": "root",
        "type": "tool.execution_complete",
        "data": {
            "toolCallId": call_id,
            "success": true,
            "result": { "content": output },
        },
    })
    .to_string()
}

fn manifest_for(artifacts: &[munshi::PreparedArtifact], session_id: &str) -> Value {
    build_manifest(
        &SessionContext {
            source_agent: SourceKind::Copilot.agent_label().to_owned(),
            source_session_id: session_id.to_owned(),
        },
        &CaptureContext {
            captured_at: "2026-07-25T00:00:00Z".to_owned(),
            source_cursor: Some("1".to_owned()),
            source_state_hash: None,
            source_metadata: BTreeMap::new(),
            project: Some("github.com/o/r".to_owned()),
            repository: None,
            branch: None,
            source_agent_version: None,
            artifact_set_version: INITIAL_ARTIFACT_SET_VERSION,
            munshi_version: Some("0.1.0".to_owned()),
        },
        artifacts,
    )
}

// ---------------------------------------------------------------------------
// Fake Patwari daemon
// ---------------------------------------------------------------------------

struct Upload {
    capture_id: String,
    manifest_sig: u64,
    /// The full manifest as received, so tests can assert the exact per-artifact digests the
    /// client declared (e.g. byte-identity of an uploaded transcript, issue #43).
    manifest: Value,
    session_id: String,
    /// Accepted chunk indexes per artifact index, in the server's canonical (path-sorted) order —
    /// like the real Patwari, NOT the manifest's order (issue #33).
    artifacts: Vec<AcceptedArtifact>,
    completed: bool,
}

struct AcceptedArtifact {
    logical_path: String,
    stored_size: u64,
    chunk_count: u64,
    accepted: Vec<u64>,
}

struct FakeState {
    registered_client: Option<String>,
    uploads: Vec<Upload>,
    capture_to_upload: HashMap<String, usize>,
    create_status_codes: Vec<u16>,
    chunk_puts: usize,
    completed: usize,
    /// When `Some`, accept at most this many new chunks, then fail further new-chunk PUTs (503).
    accept_budget: Option<u32>,
    /// A `(artifact_index, chunk_index)` that should always answer `chunk_conflict`.
    conflict_chunk: Option<(u32, u64)>,
}

struct FakePatwari {
    port: u16,
    state: Arc<Mutex<FakeState>>,
}

impl FakePatwari {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(FakeState {
            registered_client: None,
            uploads: Vec::new(),
            capture_to_upload: HashMap::new(),
            create_status_codes: Vec::new(),
            chunk_puts: 0,
            completed: 0,
            accept_budget: None,
            conflict_chunk: None,
        }));
        let thread_state = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                handle_connection(stream, &thread_state);
            }
        });
        Self { port, state }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn registered_client(&self) -> Option<String> {
        self.state.lock().unwrap().registered_client.clone()
    }

    fn upload_count(&self) -> usize {
        self.state.lock().unwrap().uploads.len()
    }

    /// The manifest the client declared for upload `index`, as received at create time.
    fn manifest(&self, index: usize) -> Value {
        self.state.lock().unwrap().uploads[index].manifest.clone()
    }

    fn create_status_codes(&self) -> Vec<u16> {
        self.state.lock().unwrap().create_status_codes.clone()
    }

    fn chunk_put_count(&self) -> usize {
        self.state.lock().unwrap().chunk_puts
    }

    fn completed_count(&self) -> usize {
        self.state.lock().unwrap().completed
    }

    fn accepted_chunk_count(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .uploads
            .iter()
            .flat_map(|upload| upload.artifacts.iter())
            .map(|artifact| artifact.accepted.len())
            .sum()
    }

    fn accept_only(&self, budget: u32) {
        self.state.lock().unwrap().accept_budget = Some(budget);
    }

    fn accept_unlimited(&self) {
        self.state.lock().unwrap().accept_budget = None;
    }

    fn conflict_on_chunk(&self, artifact_index: u32, chunk_index: u64) {
        self.state.lock().unwrap().conflict_chunk = Some((artifact_index, chunk_index));
    }
}

fn handle_connection(mut stream: TcpStream, state: &Arc<Mutex<FakeState>>) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let mut guard = state.lock().unwrap();
    let response = route(&request, &mut guard);
    drop(guard);
    let _ = stream.write_all(&response);
}

struct FakeRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn route(request: &FakeRequest, state: &mut FakeState) -> Vec<u8> {
    let segments: Vec<&str> = request.target.trim_start_matches('/').split('/').collect();
    // /api/v1/clients/{id}
    if request.method == "PUT" && segments.len() == 4 && segments[2] == "clients" {
        state.registered_client = Some(decode(segments[3]));
        return json_response(200, &json!({ "client_id": decode(segments[3]) }));
    }
    // /api/v1/uploads ...
    if segments.len() >= 3 && segments[2] == "uploads" {
        if request.method == "POST" && segments.len() == 3 {
            return create_upload(request, state);
        }
        if segments.len() >= 4 {
            let upload_id = decode(segments[3]);
            if request.method == "GET" && segments.len() == 4 {
                return upload_status(&upload_id, state);
            }
            if request.method == "POST" && segments.len() == 5 && segments[4] == "complete" {
                return complete_upload(&upload_id, state);
            }
            if request.method == "PUT" && segments.len() == 8 && segments[4] == "artifacts" {
                let artifact_index: u32 = segments[5].parse().unwrap_or(0);
                let chunk_index: u64 = segments[7].parse().unwrap_or(0);
                return put_chunk(request, &upload_id, artifact_index, chunk_index, state);
            }
        }
    }
    json_response(
        404,
        &json!({ "error": { "code": "not_found", "message": "unhandled" } }),
    )
}

fn create_upload(request: &FakeRequest, state: &mut FakeState) -> Vec<u8> {
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let capture_id = body["capture_id"].as_str().unwrap_or_default().to_owned();
    let manifest = &body["manifest"];
    let manifest_sig = simple_hash(&manifest.to_string());
    let session_id = manifest["session"]["source_session_id"]
        .as_str()
        .unwrap_or("session")
        .to_owned();

    if let Some(&index) = state.capture_to_upload.get(&capture_id) {
        // Duplicate capture id: identical manifest resumes (200), a changed one conflicts.
        if state.uploads[index].manifest_sig != manifest_sig {
            return json_response(
                409,
                &json!({ "error": { "code": "capture_id_conflict", "message": "changed manifest" } }),
            );
        }
        state.create_status_codes.push(200);
        let response = upload_status_value(index, state);
        return json_response(200, &response);
    }

    let mut artifacts: Vec<AcceptedArtifact> = manifest["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|artifact| {
            let stored = artifact["stored_size_bytes"].as_u64().unwrap_or(0);
            AcceptedArtifact {
                logical_path: artifact["logical_path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                stored_size: stored,
                chunk_count: stored.div_ceil(CHUNK_SIZE),
                accepted: Vec::new(),
            }
        })
        .collect();
    // Canonicalize like the real Patwari: `artifact_index` is assigned over the artifacts sorted by
    // logical path, whatever order the manifest listed them in (issue #33).
    artifacts.sort_by(|a, b| a.logical_path.cmp(&b.logical_path));
    let index = state.uploads.len();
    state.uploads.push(Upload {
        capture_id: capture_id.clone(),
        manifest_sig,
        manifest: manifest.clone(),
        session_id,
        artifacts,
        completed: false,
    });
    state.capture_to_upload.insert(capture_id, index);
    state.create_status_codes.push(201);
    let response = upload_status_value(index, state);
    json_response(201, &response)
}

fn upload_status(upload_id: &str, state: &mut FakeState) -> Vec<u8> {
    let Some(index) = upload_index(upload_id) else {
        return json_response(
            404,
            &json!({ "error": { "code": "not_found", "message": "x" } }),
        );
    };
    if index >= state.uploads.len() {
        return json_response(
            404,
            &json!({ "error": { "code": "not_found", "message": "x" } }),
        );
    }
    let response = upload_status_value(index, state);
    json_response(200, &response)
}

fn put_chunk(
    request: &FakeRequest,
    upload_id: &str,
    artifact_index: u32,
    chunk_index: u64,
    state: &mut FakeState,
) -> Vec<u8> {
    state.chunk_puts += 1;
    if state.conflict_chunk == Some((artifact_index, chunk_index)) {
        return json_response(
            409,
            &json!({ "error": { "code": "chunk_conflict", "message": "different bytes" } }),
        );
    }
    // Require the octet-stream content type and matching length/sha headers, like the real server.
    if request.headers.get("content-type").map(String::as_str) != Some("application/octet-stream") {
        return json_response(
            422,
            &json!({ "error": { "code": "invalid", "message": "content type" } }),
        );
    }
    let declared_len: usize = request
        .headers
        .get("x-patwari-chunk-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(usize::MAX);
    if declared_len != request.body.len() {
        return json_response(
            422,
            &json!({ "error": { "code": "invalid", "message": "length" } }),
        );
    }
    let expected_sha = format!("sha256:{}", sha256_hex(&request.body));
    if request.headers.get("x-patwari-chunk-sha256") != Some(&expected_sha) {
        return json_response(
            422,
            &json!({ "error": { "code": "invalid", "message": "sha" } }),
        );
    }
    let Some(index) = upload_index(upload_id).filter(|index| *index < state.uploads.len()) else {
        return json_response(
            404,
            &json!({ "error": { "code": "not_found", "message": "x" } }),
        );
    };
    let Some(artifact) = state.uploads[index].artifacts.get(artifact_index as usize) else {
        return json_response(
            404,
            &json!({ "error": { "code": "not_found", "message": "unknown artifact index" } }),
        );
    };
    // Enforce the negotiated chunk layout like the real server: every chunk of this (canonically
    // indexed) artifact is CHUNK_SIZE bytes except a smaller final chunk. A client that routes
    // another artifact's bytes here — the positional-mapping bug of issue #33 — is rejected.
    let expected_len = if chunk_index + 1 == artifact.chunk_count {
        artifact.stored_size - (artifact.chunk_count - 1) * CHUNK_SIZE
    } else {
        CHUNK_SIZE
    };
    if chunk_index >= artifact.chunk_count || request.body.len() as u64 != expected_len {
        return json_response(
            422,
            &json!({ "error": {
                "code": "chunk_length_mismatch",
                "message": "chunk length does not match the negotiated chunk layout",
            } }),
        );
    }
    let already = artifact.accepted.contains(&chunk_index);
    if !already {
        // A budget of accepted new chunks models an interruption after N chunks.
        if let Some(budget) = state.accept_budget {
            if budget == 0 {
                return json_response(
                    503,
                    &json!({ "error": { "code": "unavailable", "message": "interrupted" } }),
                );
            }
            state.accept_budget = Some(budget - 1);
        }
        if let Some(artifact) = state.uploads[index]
            .artifacts
            .get_mut(artifact_index as usize)
        {
            artifact.accepted.push(chunk_index);
        }
    }
    // 204 No Content, like the real server.
    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
}

fn complete_upload(upload_id: &str, state: &mut FakeState) -> Vec<u8> {
    let Some(index) = upload_index(upload_id).filter(|index| *index < state.uploads.len()) else {
        return json_response(
            404,
            &json!({ "error": { "code": "not_found", "message": "x" } }),
        );
    };
    // Every chunk of every artifact must be accepted before completion.
    let complete = state.uploads[index]
        .artifacts
        .iter()
        .all(|artifact| artifact.accepted.len() as u64 == artifact.chunk_count);
    if !complete {
        return json_response(
            422,
            &json!({ "error": { "code": "incomplete_upload", "message": "missing chunks" } }),
        );
    }
    if !state.uploads[index].completed {
        state.uploads[index].completed = true;
        state.completed += 1;
    }
    let upload = &state.uploads[index];
    let snapshot_id = format!("snap-{:016x}", upload.manifest_sig);
    json_response(
        200,
        &json!({
            "receipt": {
                "snapshot_id": snapshot_id,
                "session_id": upload.session_id,
                "snapshot_fingerprint": format!("fp-{:016x}", upload.manifest_sig),
                "manifest_sha256": format!("{:064x}", upload.manifest_sig),
                "artifact_count": upload.artifacts.len(),
                "total_original_bytes": 0,
                "total_stored_bytes": 0,
            },
            "transfer": {
                "upload_id": upload_id,
                "capture_id": upload.capture_id,
                "upload_transfer_bytes": 0,
                "newly_persisted_physical_bytes": 0,
            },
            "capture": { "capture_id": upload.capture_id },
        }),
    )
}

/// A deterministic upload id derived from the upload's index (the fake never expires uploads).
fn upload_id_for(index: usize) -> String {
    format!("upl-{index:08}")
}

fn upload_index(upload_id: &str) -> Option<usize> {
    upload_id.strip_prefix("upl-")?.parse().ok()
}

fn upload_status_value(index: usize, state: &FakeState) -> Value {
    let upload = &state.uploads[index];
    let artifacts: Vec<Value> = upload
        .artifacts
        .iter()
        .enumerate()
        .map(|(artifact_index, artifact)| {
            let missing: Vec<u64> = (0..artifact.chunk_count)
                .filter(|chunk| !artifact.accepted.contains(chunk))
                .collect();
            json!({
                "logical_path": artifact.logical_path,
                "artifact_index": artifact_index,
                "chunk_count": artifact.chunk_count,
                "missing_chunk_indexes": missing,
                "accepted_chunk_bitmap": "",
            })
        })
        .collect();
    let all_present = upload
        .artifacts
        .iter()
        .all(|artifact| artifact.accepted.len() as u64 == artifact.chunk_count);
    let status = if upload.completed {
        "completed"
    } else if all_present {
        "artifact_uploaded"
    } else {
        "created"
    };
    json!({
        "upload_id": upload_id_for(index),
        "capture_id": upload.capture_id,
        "session_id": upload.session_id,
        "status": status,
        "chunk_size_bytes": CHUNK_SIZE,
        "artifacts": artifacts,
    })
}

fn json_response(status: u16, value: &Value) -> Vec<u8> {
    let body = value.to_string();
    let reason = match status {
        200 => "OK",
        201 => "Created",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        503 => "Service Unavailable",
        _ => "Status",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn read_request(stream: &mut TcpStream) -> Option<FakeRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_subsequence(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > 8 * 1024 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_owned();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Some(FakeRequest {
        method,
        target,
        headers,
        body,
    })
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode(value: &str) -> String {
    let mut output = String::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                output.push(byte as char);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index] as char);
        index += 1;
    }
    output
}

fn simple_hash(content: &str) -> u64 {
    let mut hash: u64 = 1469598103934665603;
    for byte in content.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
