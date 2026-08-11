//! Integration coverage for `munshi restore` (issue #70).
//!
//! These tests drive the real `munshi` binary against an in-process fake Patwari daemon serving the
//! archive read surface — the cursor-paginated snapshot listing, per-snapshot canonical manifests,
//! the snapshot-filtered artifact listing, and the stored-bytes content route with its
//! `x-patwari-*` metadata headers — and then assert against the files that land in the archive
//! output directory. Served `summary.md` bodies are rendered by Munshi's own renderer, so the
//! layout a restore reproduces is checked against the layout archival would have produced rather
//! than against a hand-written fixture.
//!
//! Covered: a whole-archive restore onto an empty machine, the registered-machine path where the
//! restored Markdown is imported into operational state and the row is linked to its restored
//! transcript, an idempotent rerun that transfers nothing, resumption after an interrupted run, the
//! differing-local-file refusal and its `--force` escape hatch, the unregistered refusal, cursor
//! pagination, newest-snapshot-per-session selection, a tampered artifact refused on verification,
//! a traversing sidecar logical path refused rather than written, a `--session` filter that matches
//! nothing, and `--dry-run`.
//!
//! Every invocation passes an explicit `--state-dir` and an isolated `HOME`/`MUNSHI_HOME`/harness
//! home environment, so no test can reach the developer's real `~/.munshi` or `~/.claude`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use munshi::{
    ArchiveMetadata, NormalizedSession, ProjectIdentity, ProjectOrigin, SnapshotArtifactIndex,
    SourceKind, StructuredSummary, content_hash, render_revision_markdown,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const SESSION_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const SESSION_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const SNAPSHOT_1: &str = "11111111-1111-4111-8111-111111111111";
const SNAPSHOT_2: &str = "22222222-2222-4222-8222-222222222222";
const SNAPSHOT_3: &str = "33333333-3333-4333-8333-333333333333";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn restores_the_whole_record_onto_an_empty_machine() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(
        vec![session.snapshot(SNAPSHOT_1, SESSION_A).with_sidecars()],
        50,
    );

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["command"], "restore");
    assert_eq!(report["totals"]["snapshots"], 1);
    assert_eq!(report["totals"]["restored"], 1);
    assert_eq!(report["totals"]["artifacts_written"], 5);
    assert_eq!(report["totals"]["artifacts_present"], 0);
    assert_eq!(report["snapshots"][0]["status"]["result"], "restored");
    assert_eq!(report["snapshots"][0]["session_id"], "sess-one");
    assert_eq!(
        report["snapshots"][0]["relative_path"],
        "munshi/sess-one.md"
    );

    // The whole artifact set landed in the archive layout: the summary where archival writes it,
    // sidecars in the staged sidecar directory archival re-reads, and the two artifacts archival
    // never writes locally in the restored-artifact directory beside them.
    let root = machine.output.path();
    assert_eq!(
        std::fs::read(root.join("munshi/sess-one.md")).unwrap(),
        session.summary_bytes()
    );
    assert_eq!(
        std::fs::read(root.join("munshi/sess-one.restored/transcript.jsonl")).unwrap(),
        session.transcript
    );
    assert_eq!(
        std::fs::read(root.join("munshi/sess-one.sidecar/plan.md")).unwrap(),
        b"# plan\n"
    );
    assert_eq!(
        std::fs::read(root.join("munshi/sess-one.sidecar/checkpoints/one.md")).unwrap(),
        b"# checkpoint\n"
    );
    let output_digest = sha256_hex(b"an elided tool output");
    assert_eq!(
        std::fs::read(root.join(format!("munshi/sess-one.restored/outputs/{output_digest}")))
            .unwrap(),
        b"an elided tool output"
    );

    // An unregistered machine cannot import state — the harness homes the importer derives from
    // live in a registration — so it says so and names the command to run afterwards.
    assert_eq!(report["state"]["result"], "skipped");
    assert_eq!(report["state"]["reason"], "unregistered");
    assert!(
        report["state"]["message"]
            .as_str()
            .unwrap()
            .contains("munshi register"),
        "state: {}",
        report["state"]
    );
}

#[test]
fn a_registered_machine_imports_the_restored_record_into_state() {
    let machine = Machine::new();
    machine.register();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"]["result"], "rebuilt");
    assert_eq!(report["state"]["sessions"], 1);
    // The row is pointed at the restored transcript: on a wiped machine the harness home holds
    // nothing to re-derive from, so without this the row could never upload a full snapshot again.
    assert_eq!(report["state"]["transcripts_linked"], 1);

    let sessions = machine.sessions_json();
    let item = &sessions["items"][0];
    assert_eq!(sessions["total"], 1);
    assert_eq!(item["session_id"], "sess-one");
    assert_eq!(item["source"], "copilot");
    // Imported rows land archived with no observation, so nothing can claim them and nothing can
    // park them as transcript-missing (issue #58) or deadlock them origin-less (issue #39).
    assert_eq!(item["lifecycle_state"], "archived");
    assert_eq!(item["revision"], 2);
    assert_eq!(item["archive_path"], "munshi/sess-one.md");
}

#[test]
fn rerunning_a_completed_restore_transfers_nothing() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    let first = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(first.status.code(), Some(0), "stderr: {}", first.stderr());
    server.clear_requests();

    let second = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(second.status.code(), Some(0), "stderr: {}", second.stderr());
    let report: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(report["totals"]["artifacts_written"], 0);
    assert_eq!(report["totals"]["artifacts_present"], 3);
    assert_eq!(report["totals"]["bytes_written"], 0);
    assert_eq!(report["snapshots"][0]["status"]["result"], "restored");

    // Verify-and-skip, not exists-and-skip: the local copies were proved identical from the
    // listing's declared digests, so the content route was never called at all.
    assert!(
        !server
            .requests()
            .iter()
            .any(|target| target.contains("/content")),
        "requests: {:?}",
        server.requests()
    );
}

#[test]
fn an_interrupted_restore_resumes_on_the_next_run() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    assert_eq!(
        machine.restore(&server, &["--all", "--json"]).status.code(),
        Some(0)
    );
    // Simulate a run that died after the summary but before the transcript.
    let transcript = machine
        .output
        .path()
        .join("munshi/sess-one.restored/transcript.jsonl");
    std::fs::remove_file(&transcript).unwrap();
    server.clear_requests();

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["totals"]["artifacts_written"], 1,
        "only the missing artifact is fetched again"
    );
    assert_eq!(report["totals"]["artifacts_present"], 2);
    assert_eq!(std::fs::read(&transcript).unwrap(), session.transcript);
    let downloads = server
        .requests()
        .iter()
        .filter(|target| target.contains("/content"))
        .count();
    assert_eq!(downloads, 1, "requests: {:?}", server.requests());
}

#[test]
fn a_differing_local_file_is_refused_until_force() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    // A local summary that is not the archived one: restore must report it, not overwrite it.
    let summary_path = machine.output.path().join("munshi/sess-one.md");
    std::fs::create_dir_all(summary_path.parent().unwrap()).unwrap();
    std::fs::write(&summary_path, b"locally edited, not the archive's copy").unwrap();

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["totals"]["refused"], 1);
    assert_eq!(report["totals"]["artifacts_refused"], 1);
    assert_eq!(report["snapshots"][0]["status"]["result"], "refused");
    let summary = artifact(&report["snapshots"][0], "summary.md");
    assert_eq!(summary["result"], "refused-differs");
    assert!(
        summary["reason"].as_str().unwrap().contains("--force"),
        "reason: {summary}"
    );
    assert_eq!(
        std::fs::read(&summary_path).unwrap(),
        b"locally edited, not the archive's copy",
        "a refusal never writes"
    );
    // A refused record is not imported: the database must not learn a revision the file on disk
    // does not hold.
    assert_eq!(report["state"]["result"], "skipped");

    let forced = machine.restore(&server, &["--all", "--force", "--json"]);
    assert_eq!(forced.status.code(), Some(0), "stderr: {}", forced.stderr());
    let report: Value = serde_json::from_slice(&forced.stdout).unwrap();
    assert_eq!(
        artifact(&report["snapshots"][0], "summary.md")["result"],
        "replaced"
    );
    assert_eq!(
        std::fs::read(&summary_path).unwrap(),
        session.summary_bytes()
    );
}

#[test]
fn an_unregistered_machine_without_an_output_directory_refuses() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    // No --output-dir and no registration: restore has nowhere to write and says so, naming both
    // flags rather than guessing at a default archive location.
    let output = machine.run(&["--all", "--endpoint", &server.endpoint(), "--json"]);
    assert_eq!(output.status.code(), Some(3), "stdout: {:?}", output.stdout);
    assert!(
        output.stderr().contains("--output-dir"),
        "stderr: {}",
        output.stderr()
    );
    assert!(
        !server
            .requests()
            .iter()
            .any(|target| target.contains("/snapshots")),
        "the refusal happens before any request: {:?}",
        server.requests()
    );
}

#[test]
fn an_unconfigured_endpoint_refuses() {
    let machine = Machine::new();
    let output = machine.run(&[
        "--all",
        "--output-dir",
        machine.output.path().to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(3), "stdout: {:?}", output.stdout);
    assert!(
        output.stderr().contains("no archive server"),
        "stderr: {}",
        output.stderr()
    );
}

#[test]
fn pagination_follows_cursors_across_listing_pages() {
    let machine = Machine::new();
    let sessions: Vec<ArchivedSession> = (1..=3)
        .map(|index| ArchivedSession::copilot(&format!("sess-{index}"), "munshi"))
        .collect();
    let snapshots: Vec<FakeSnapshot> = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| {
            session.snapshot(
                &format!(
                    "{0}{0}{0}{0}{0}{0}{0}{0}-1111-4111-8111-111111111111",
                    index + 1
                ),
                &format!(
                    "{0}{0}{0}{0}{0}{0}{0}{0}-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    index + 1
                ),
            )
        })
        .collect();
    // Two snapshots per page forces a second page behind a cursor.
    let server = FakeArchive::start(snapshots, 2);

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["totals"]["snapshots"], 3);
    assert_eq!(report["totals"]["restored"], 3);
    for session in &sessions {
        assert!(
            machine
                .output
                .path()
                .join(format!("munshi/{}.md", session.session_id))
                .is_file()
        );
    }

    let snapshot_requests: Vec<String> = server
        .requests()
        .iter()
        .filter(|target| target.starts_with("/api/v1/snapshots?"))
        .cloned()
        .collect();
    assert_eq!(
        snapshot_requests.len(),
        2,
        "requests: {snapshot_requests:?}"
    );
    assert!(
        snapshot_requests[1].contains("cursor="),
        "the second page uses the returned cursor: {snapshot_requests:?}"
    );
}

#[test]
fn only_the_newest_snapshot_of_a_session_is_restored() {
    let machine = Machine::new();
    let newest = ArchivedSession::copilot_revision("sess-one", "munshi", 9, b"newest transcript\n");
    let older = ArchivedSession::copilot_revision("sess-one", "munshi", 2, b"older transcript\n");
    // Patwari lists newest first; both snapshots belong to the same session.
    let server = FakeArchive::start(
        vec![
            newest.snapshot(SNAPSHOT_1, SESSION_A),
            older.snapshot(SNAPSHOT_2, SESSION_A),
        ],
        50,
    );

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["totals"]["snapshots"], 1);
    assert_eq!(report["totals"]["superseded"], 1);
    assert_eq!(report["snapshots"][0]["snapshot_id"], SNAPSHOT_1);
    assert_eq!(report["snapshots"][0]["superseded_snapshots"], 1);
    assert_eq!(
        std::fs::read(machine.output.path().join("munshi/sess-one.md")).unwrap(),
        newest.summary_bytes(),
        "the local record holds the newest revision"
    );
    assert_eq!(
        std::fs::read(
            machine
                .output
                .path()
                .join("munshi/sess-one.restored/transcript.jsonl")
        )
        .unwrap(),
        newest.transcript
    );
    // The superseded snapshot's artifacts were never fetched.
    assert!(
        !server
            .requests()
            .iter()
            .any(|target| target.contains(SNAPSHOT_2)),
        "requests: {:?}",
        server.requests()
    );
}

#[test]
fn a_tampered_artifact_fails_verification_and_writes_nothing() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let mut snapshot = session.snapshot(SNAPSHOT_1, SESSION_A);
    // Flip a stored byte of the transcript so it no longer matches its declared stored sha256.
    let transcript = snapshot
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.logical_path == "transcript.jsonl")
        .unwrap();
    transcript.served_stored_bytes[0] ^= 0xff;
    let server = FakeArchive::start(vec![snapshot], 50);

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(6), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["snapshots"][0]["status"]["result"], "failed");
    assert_eq!(report["snapshots"][0]["status"]["class"], "verification");
    assert_eq!(
        artifact(&report["snapshots"][0], "transcript.jsonl")["result"],
        "failed"
    );
    assert!(
        !machine
            .output
            .path()
            .join("munshi/sess-one.restored/transcript.jsonl")
            .exists(),
        "no unverified byte reaches the archive output directory"
    );
    // The summary verified and was written: one bad artifact does not discard the whole snapshot.
    assert!(machine.output.path().join("munshi/sess-one.md").is_file());
}

#[test]
fn a_traversing_sidecar_path_is_refused_rather_than_written() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let mut snapshot = session.snapshot(SNAPSHOT_1, SESSION_A);
    snapshot.artifacts.push(FakeArtifact::identity(
        "art-evil",
        "sidecar/../../../escaped.md",
        b"# escaped\n",
    ));
    let server = FakeArchive::start(vec![snapshot], 50);

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let escaped = artifact(&report["snapshots"][0], "sidecar/../../../escaped.md");
    assert_eq!(escaped["result"], "skipped");
    assert_eq!(escaped["skip_cause"], "unplaceable");
    assert!(
        escaped["reason"].as_str().unwrap().contains("unsafe"),
        "reason: {escaped}"
    );
    assert!(
        !machine.output.path().join("../../../escaped.md").exists()
            && !machine.root.path().join("escaped.md").exists(),
        "an archive server cannot write outside the output directory"
    );
}

#[test]
fn a_session_filter_that_matches_nothing_is_a_failure() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    let output = machine.restore(&server, &["--session", SESSION_B, "--json"]);
    assert_eq!(output.status.code(), Some(4), "stdout: {:?}", output.stdout);
    assert!(
        output.stderr().contains(SESSION_B),
        "stderr: {}",
        output.stderr()
    );
}

#[test]
fn the_session_filter_restores_only_that_session() {
    let machine = Machine::new();
    let wanted = ArchivedSession::copilot("sess-one", "munshi");
    let other = ArchivedSession::copilot("sess-two", "munshi");
    let server = FakeArchive::start(
        vec![
            wanted.snapshot(SNAPSHOT_1, SESSION_A),
            other.snapshot(SNAPSHOT_3, SESSION_B),
        ],
        50,
    );

    let output = machine.restore(&server, &["--session", SESSION_A, "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["session_filter"], SESSION_A);
    assert_eq!(report["totals"]["snapshots"], 1);
    assert!(machine.output.path().join("munshi/sess-one.md").is_file());
    assert!(!machine.output.path().join("munshi/sess-two.md").exists());
}

#[test]
fn a_dry_run_reports_the_writes_without_making_them() {
    let machine = Machine::new();
    machine.register();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    let output = machine.restore(&server, &["--all", "--dry-run", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["totals"]["artifacts_written"], 3);
    assert_eq!(
        artifact(&report["snapshots"][0], "transcript.jsonl")["result"],
        "would-write"
    );
    assert!(!machine.output.path().join("munshi/sess-one.md").exists());
    assert_eq!(report["state"]["result"], "skipped");
    assert_eq!(report["state"]["reason"], "dry-run");

    // Nothing but the summary — the artifact that says where the record belongs — is transferred by
    // a dry run, and nothing at all is written.
    let downloads: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|target| target.contains("/content"))
        .collect();
    assert_eq!(downloads.len(), 1, "requests: {downloads:?}");
    assert!(
        downloads[0].contains("art-summary"),
        "requests: {downloads:?}"
    );
}

#[test]
fn skip_outputs_leaves_the_re_derivable_artifacts_in_the_archive() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    let output = machine.restore(&server, &["--all", "--skip-outputs", "--json"]);
    // A run that skipped exactly what it was told to skip has done its whole job: exit 0, so a
    // scripted `--skip-outputs` restore never reads as failed to an exit-code gate.
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["totals"]["artifacts_skipped_by_request"], 1);
    assert_eq!(
        report["totals"]["artifacts_skipped"], 0,
        "a requested skip is not a finding"
    );
    assert_eq!(report["totals"]["artifacts_written"], 2);
    assert!(
        !machine
            .output
            .path()
            .join("munshi/sess-one.restored/outputs")
            .exists()
    );
    // The JSON still tells the two causes apart artifact by artifact.
    let skipped = report["snapshots"][0]["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["result"] == "skipped")
        .expect("the extracted output is reported, not silently dropped");
    assert_eq!(skipped["skip_cause"], "requested");
    assert!(
        skipped["logical_path"]
            .as_str()
            .unwrap()
            .starts_with("outputs/")
    );
    // Skipping a re-derivable artifact still leaves the record restored, not failed.
    assert_eq!(report["snapshots"][0]["status"]["result"], "restored");
}

#[test]
fn an_artifact_past_the_download_cap_is_a_finding_not_a_quiet_skip() {
    let machine = Machine::new();
    // A transcript far larger than its summary, so one cap can admit the summary and refuse it.
    let transcript = b"{\"type\":\"user\",\"padding\":\"xxxxxxxxxxxxxxxx\"}\n".repeat(400);
    let session = ArchivedSession::copilot_revision("sess-one", "munshi", 2, &transcript);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    // Nobody asked for this skip, so unlike `--skip-outputs` it is a finding and exits 4. The
    // transcript compresses well, so what refuses it is the declared *original* size — the
    // amplification half of the gate — before a byte is transferred.
    let output = machine.restore(
        &server,
        &["--all", "--max-download-bytes", "4096", "--json"],
    );
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["totals"]["artifacts_skipped"].as_u64().unwrap() > 0);
    assert_eq!(report["totals"]["artifacts_skipped_by_request"], 0);
    let transcript = artifact(&report["snapshots"][0], "transcript.jsonl");
    assert_eq!(transcript["result"], "skipped");
    assert_eq!(transcript["skip_cause"], "too-large");
    assert!(
        transcript["reason"]
            .as_str()
            .unwrap()
            .contains("--max-download-bytes"),
        "reason: {transcript}"
    );
}

#[test]
fn an_unsupported_artifact_set_version_is_skipped_not_fatal() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let mut snapshot = session.snapshot(SNAPSHOT_1, SESSION_A);
    snapshot.artifact_set_version = 99;
    let server = FakeArchive::start(vec![snapshot], 50);

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["snapshots"][0]["status"]["result"], "skipped");
    assert_eq!(
        report["snapshots"][0]["status"]["reason"],
        "unsupported-artifact-set-version"
    );
    // Nothing was downloaded for a snapshot this build cannot interpret.
    assert!(
        !server
            .requests()
            .iter()
            .any(|target| target.contains("/content")),
        "requests: {:?}",
        server.requests()
    );
}

#[test]
fn a_summary_that_is_not_a_munshi_archive_is_skipped() {
    let machine = Machine::new();
    let server = FakeArchive::start(
        vec![FakeSnapshot {
            snapshot_id: SNAPSHOT_1.to_owned(),
            session_id: SESSION_A.to_owned(),
            source_agent: "copilot-cli".to_owned(),
            artifact_set_version: 2,
            artifacts: vec![FakeArtifact::identity(
                "art-summary",
                "summary.md",
                b"# not frontmatter\n",
            )],
        }],
        50,
    );

    let output = machine.restore(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["snapshots"][0]["status"]["result"], "skipped");
    assert_eq!(
        report["snapshots"][0]["status"]["reason"],
        "unusable-summary"
    );
}

// ---------------------------------------------------------------------------
// Resume restore (issue #71)
// ---------------------------------------------------------------------------

/// The cwd every archived Claude Code fixture records, and the directory name Claude Code derives
/// from it. Written out rather than computed so a change to the slug rule fails a test that states
/// the expected directory literally.
const ARCHIVED_CWD: &str = "/machine-a/repos/thing";
const ARCHIVED_SLUG: &str = "-machine-a-repos-thing";

#[test]
fn resume_places_a_claude_code_session_into_a_fresh_harness_home() {
    let machine = Machine::new();
    let session = ArchivedSession::claude_code("sess-one", "munshi", ARCHIVED_CWD);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    let output = machine.resume(&server, SESSION_A, &["--yes", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let resume = &report["resume"];

    assert_eq!(resume["status"]["result"], "placed");
    assert_eq!(resume["harness"], "claude-code");
    assert_eq!(resume["session_id"], "sess-one");
    assert_eq!(resume["project_directory"], ARCHIVED_CWD);
    assert_eq!(resume["project_slug"], ARCHIVED_SLUG);
    // The archived transcript's own records say which harness version wrote it; the archive
    // manifest does not (Munshi records no `source_agent_version` today).
    assert_eq!(resume["archived_harness_version"], "2.1.205");
    // The exact command to continue the conversation, and only because a transcript is in place.
    // It is scoped to a directory: `project_directory` is where it has to be run, because the
    // harness looks the session up in the projects directory of the current cwd.
    assert_eq!(resume["resume_command"], "claude --resume sess-one");
    assert_eq!(resume["project_directory"], ARCHIVED_CWD);

    // The transcript is verbatim, at the slugged path the harness reads.
    let target = machine.harness_transcript(ARCHIVED_SLUG, "sess-one");
    assert_eq!(resume["target_path"], target.display().to_string());
    assert_eq!(std::fs::read(&target).unwrap(), session.transcript);

    // The human line never states the command without the directory it only works from.
    let human = machine.resume(&server, SESSION_A, &["--yes"]);
    assert_eq!(human.status.code(), Some(0), "stderr: {}", human.stderr());
    let text = String::from_utf8_lossy(&human.stdout);
    let guidance = text
        .lines()
        .find(|line| line.contains("claude --resume sess-one"))
        .unwrap_or_else(|| panic!("no resume guidance in {text}"));
    assert!(guidance.contains(ARCHIVED_CWD), "guidance: {guidance}");
    // The record restore still happened in full: resume extends it, it does not replace it.
    assert_eq!(report["totals"]["restored"], 1);
    assert!(
        machine
            .output
            .path()
            .join("munshi/claude-code/sess-one.md")
            .is_file()
    );
}

#[test]
fn a_working_directory_missing_on_this_machine_is_a_warning_not_a_refusal() {
    let machine = Machine::new();
    let session = ArchivedSession::claude_code("sess-one", "munshi", ARCHIVED_CWD);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    // `/machine-a/repos/thing` does not exist here — the whole point of restoring onto a new
    // machine — and Claude Code still lists and resumes the session from its transcript.
    let output = machine.resume(&server, SESSION_A, &["--yes", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let warnings = report["resume"]["warnings"].as_array().unwrap();
    let missing = warnings
        .iter()
        .find_map(|warning| {
            let warning = warning.as_str().unwrap();
            warning.contains(ARCHIVED_CWD).then_some(warning)
        })
        .unwrap_or_else(|| panic!("warnings: {warnings:?}"));
    // The resume command is run *from* that directory, so the warning has to say to make it exist
    // — otherwise it reads as cosmetic next to the guidance line that names the same path.
    assert!(missing.contains("create or clone"), "warning: {missing}");
    assert_eq!(report["resume"]["status"]["result"], "placed");

    // A session whose working directory *does* exist says nothing about it.
    let present = Machine::new();
    let existing = present.root.path().join("live-project");
    std::fs::create_dir_all(&existing).unwrap();
    let cwd = existing.to_str().unwrap();
    let session = ArchivedSession::claude_code("sess-two", "munshi", cwd);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_2, SESSION_B)], 50);
    let output = present.resume(&server, SESSION_B, &["--yes", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let warnings = report["resume"]["warnings"].as_array().unwrap();
    assert!(
        !warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("does not exist")),
        "warnings: {warnings:?}"
    );
}

#[test]
fn rerunning_a_resume_is_a_no_op() {
    let machine = Machine::new();
    let session = ArchivedSession::claude_code("sess-one", "munshi", ARCHIVED_CWD);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    assert_eq!(
        machine
            .resume(&server, SESSION_A, &["--yes", "--json"])
            .status
            .code(),
        Some(0)
    );
    let output = machine.resume(&server, SESSION_A, &["--yes", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["resume"]["status"]["result"], "already-present");
    assert_eq!(
        report["resume"]["resume_command"],
        "claude --resume sess-one"
    );
    // A no-op rerun still says where the command works, because it is still the answer to "how do
    // I continue this session".
    assert_eq!(report["resume"]["project_directory"], ARCHIVED_CWD);
    assert_eq!(
        std::fs::read(machine.harness_transcript(ARCHIVED_SLUG, "sess-one")).unwrap(),
        session.transcript
    );
}

#[test]
fn a_differing_transcript_in_the_harness_home_is_never_replaced() {
    let machine = Machine::new();
    let session = ArchivedSession::claude_code("sess-one", "munshi", ARCHIVED_CWD);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    // A live session the harness has continued past the snapshot.
    let target = machine.harness_transcript(ARCHIVED_SLUG, "sess-one");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let live =
        b"{\"type\":\"user\",\"sessionId\":\"sess-one\",\"note\":\"newer than the archive\"}\n";
    std::fs::write(&target, live).unwrap();

    let output = machine.resume(&server, SESSION_A, &["--yes", "--json"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["resume"]["status"]["result"], "refused");
    assert_eq!(report["resume"]["status"]["reason"], "target-differs");
    assert!(
        report["resume"]["status"]["message"]
            .as_str()
            .unwrap()
            .contains("--force does not apply"),
        "message: {}",
        report["resume"]["status"]["message"]
    );
    assert!(report["resume"]["resume_command"].is_null());
    assert_eq!(std::fs::read(&target).unwrap(), live);

    // `--force` replaces stale *archive* files; a harness home is not Munshi's to overwrite.
    let forced = machine.resume(&server, SESSION_A, &["--yes", "--force", "--json"]);
    assert_eq!(forced.status.code(), Some(4), "stderr: {}", forced.stderr());
    let report: Value = serde_json::from_slice(&forced.stdout).unwrap();
    assert_eq!(report["resume"]["status"]["reason"], "target-differs");
    assert_eq!(std::fs::read(&target).unwrap(), live);
}

#[test]
fn resume_without_yes_reports_the_plan_and_writes_nothing() {
    let machine = Machine::new();
    let session = ArchivedSession::claude_code("sess-one", "munshi", ARCHIVED_CWD);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    let output = machine.resume(&server, SESSION_A, &["--json"]);
    // The operator asked for a resumable session and did not get one: a finding, not a success.
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["resume"]["status"]["result"], "planned");
    assert_eq!(report["resume"]["confirmed"], false);
    // The plan names the exact write, in `--json` as well as on the terminal.
    let target = machine.harness_transcript(ARCHIVED_SLUG, "sess-one");
    assert_eq!(
        report["resume"]["target_path"],
        target.display().to_string()
    );
    assert!(report["resume"]["resume_command"].is_null());
    assert!(!target.exists(), "an unconfirmed resume writes nothing");
    assert!(!machine.claude_home.join("projects").exists());
    // The record restore itself still completed.
    assert_eq!(report["totals"]["restored"], 1);

    let human = machine.resume(&server, SESSION_A, &[]);
    assert_eq!(human.status.code(), Some(4));
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("would write"), "stdout: {text}");
    assert!(text.contains("--yes"), "stdout: {text}");
}

#[test]
fn resume_refuses_a_harness_it_has_not_learned_to_place() {
    let machine = Machine::new();
    let session = ArchivedSession::copilot("sess-one", "munshi");
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    let output = machine.resume(&server, SESSION_A, &["--yes", "--json"]);
    // An explicitly named session that cannot be resumed is a finding, never a quiet success.
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["resume"]["status"]["result"], "refused");
    assert_eq!(report["resume"]["status"]["reason"], "unsupported-harness");
    let message = report["resume"]["status"]["message"].as_str().unwrap();
    assert!(message.contains("copilot-cli"), "message: {message}");
    assert!(
        message.contains("not supported for this harness yet"),
        "message: {message}"
    );
    // Nothing was written into the harness home, and the record was still restored.
    assert!(!machine.claude_home.join("projects").exists());
    assert!(machine.output.path().join("munshi/sess-one.md").is_file());
    assert_eq!(report["totals"]["restored"], 1);
}

#[test]
fn a_registered_machine_points_the_session_row_at_the_harness_home_transcript() {
    let machine = Machine::new();
    machine.register_with_claude();
    let session = ArchivedSession::claude_code("sess-one", "munshi", ARCHIVED_CWD);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    let output = machine.resume(&server, SESSION_A, &["--yes", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["resume"]["status"]["result"], "placed");
    assert_eq!(report["state"]["result"], "rebuilt");

    // The row reads the copy the harness itself reads and keeps appending to, not the archive-side
    // copy beside the restored Markdown.
    let recorded = machine
        .recorded_transcript_path("sess-one")
        .expect("the restored row records a transcript");
    let target = machine.harness_transcript(ARCHIVED_SLUG, "sess-one");
    assert_eq!(
        std::fs::canonicalize(&recorded).unwrap(),
        std::fs::canonicalize(&target).unwrap(),
        "recorded {recorded}, placed {}",
        target.display()
    );
    assert!(
        !recorded.contains(".restored"),
        "the archive-side copy must not win: {recorded}"
    );
}

#[test]
fn resuming_after_a_plain_restore_moves_the_row_off_the_archive_side_copy() {
    let machine = Machine::new();
    machine.register_with_claude();
    let session = ArchivedSession::claude_code("sess-one", "munshi", ARCHIVED_CWD);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    // Step one: the record only. With an empty harness home the row can only point at the restored
    // copy beside the archive Markdown.
    let first = machine.restore(&server, &["--session", SESSION_A, "--json"]);
    assert_eq!(first.status.code(), Some(0), "stderr: {}", first.stderr());
    let recorded = machine.recorded_transcript_path("sess-one").unwrap();
    assert!(recorded.contains(".restored"), "recorded {recorded}");

    // Step two: resume. The harness home now holds the session, so the row follows it there — the
    // restored copy is Munshi's own artifact and goes stale as soon as the conversation continues.
    let second = machine.resume(&server, SESSION_A, &["--yes", "--json"]);
    assert_eq!(second.status.code(), Some(0), "stderr: {}", second.stderr());
    let recorded = machine.recorded_transcript_path("sess-one").unwrap();
    assert_eq!(
        std::fs::canonicalize(&recorded).unwrap(),
        std::fs::canonicalize(machine.harness_transcript(ARCHIVED_SLUG, "sess-one")).unwrap(),
        "recorded {recorded}"
    );
}

#[test]
fn a_resume_without_a_registered_claude_home_refuses_rather_than_guessing() {
    let machine = Machine::new();
    let session = ArchivedSession::claude_code("sess-one", "munshi", ARCHIVED_CWD);
    let server = FakeArchive::start(vec![session.snapshot(SNAPSHOT_1, SESSION_A)], 50);

    // No `--claude-home`, and a registration that manages Copilot only: there is no harness home
    // to place into, and `$HOME/.claude` is deliberately not inferred.
    machine.register();
    let output = machine.restore(
        &server,
        &["--session", SESSION_A, "--resume", "--yes", "--json"],
    );
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["resume"]["status"]["reason"], "harness-home-unknown");
    assert!(
        report["resume"]["status"]["message"]
            .as_str()
            .unwrap()
            .contains("--claude-home"),
        "message: {}",
        report["resume"]["status"]["message"]
    );
    assert!(!machine.claude_home.join("projects").exists());
}

#[test]
fn the_cli_refuses_a_whole_archive_resume() {
    let machine = Machine::new();
    let claude_home = machine.claude_home.to_str().unwrap().to_owned();

    // Resuming is a deliberate, single-session act: `--all` is rejected by the parser itself.
    let all = machine.run(&["--all", "--resume", "--yes", "--claude-home", &claude_home]);
    assert_eq!(all.status.code(), Some(2), "stdout: {:?}", all.stdout);
    assert!(
        all.stderr().contains("--resume") || all.stderr().contains("--all"),
        "stderr: {}",
        all.stderr()
    );

    // `--yes` and `--claude-home` mean nothing without it, and `--dry-run` would be a second,
    // divergent spelling of the unconfirmed plan.
    let stray_yes = machine.run(&["--all", "--yes"]);
    assert_eq!(stray_yes.status.code(), Some(2));
    let dry = machine.run(&["--session", SESSION_A, "--resume", "--dry-run"]);
    assert_eq!(dry.status.code(), Some(2), "stdout: {:?}", dry.stdout);
}

// ---------------------------------------------------------------------------
// Harness: an isolated machine
// ---------------------------------------------------------------------------

/// A throwaway machine: its own `HOME`, Munshi state directory, archive output directory and
/// harness homes. Every `munshi` invocation runs against these, so nothing a test does can reach
/// the developer's real `~/.munshi` or `~/.claude`.
struct Machine {
    root: TempDir,
    state: PathBuf,
    output: OutputDirectory,
    copilot_home: PathBuf,
    claude_home: PathBuf,
}

/// The archive output directory, kept alive by the machine it belongs to.
struct OutputDirectory(PathBuf);

impl OutputDirectory {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Machine {
    fn new() -> Self {
        let root = test_directory();
        let state = root.path().join("munshi-state");
        let output = root.path().join("munshi-summaries");
        let copilot_home = root.path().join("copilot-home");
        let claude_home = root.path().join("claude-home");
        for directory in [&state, &output, &copilot_home] {
            std::fs::create_dir_all(directory).unwrap();
        }
        Self {
            root,
            state,
            output: OutputDirectory(output),
            copilot_home,
            claude_home,
        }
    }

    /// Registers the machine so the state-import half of a restore has a configuration to read.
    fn register(&self) {
        self.register_harnesses(false);
    }

    /// Registers the machine with its Claude Code home too, which is what makes the registered
    /// harness home a resume can place into — and what lets state import re-derive the placed
    /// transcript by itself.
    fn register_with_claude(&self) {
        self.register_harnesses(true);
    }

    fn register_harnesses(&self, claude: bool) {
        let summarizer = self.root.path().join("summarizer.sh");
        std::fs::write(&summarizer, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&summarizer, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = self.command("register");
        if claude {
            command.arg("--claude-home").arg(&self.claude_home);
        }
        let output = command
            .arg("--accept-transcript-processing")
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--output-dir")
            .arg(self.output.path())
            .arg("--summarizer")
            .arg(&summarizer)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "register failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Runs `munshi restore <args> --endpoint <server> --output-dir <output>`.
    fn restore(&self, server: &FakeArchive, args: &[&str]) -> RunOutput {
        let endpoint = server.endpoint();
        let output = self.output.path().to_str().unwrap().to_owned();
        let mut full: Vec<&str> = args.to_vec();
        full.extend_from_slice(&["--endpoint", &endpoint, "--output-dir", &output]);
        self.run(&full)
    }

    fn run(&self, args: &[&str]) -> RunOutput {
        let output = self
            .command("restore")
            .args(args)
            .arg("--state-dir")
            .arg(&self.state)
            .output()
            .expect("run munshi restore");
        RunOutput {
            status: output.status,
            stdout: output.stdout,
            stderr_bytes: output.stderr,
        }
    }

    /// Runs a resume restore for one session, always through an explicit `--claude-home` so a test
    /// can never write into a real harness home.
    fn resume(&self, server: &FakeArchive, session: &str, args: &[&str]) -> RunOutput {
        let claude_home = self.claude_home.to_str().unwrap().to_owned();
        let mut full: Vec<&str> = vec![
            "--session",
            session,
            "--resume",
            "--claude-home",
            &claude_home,
        ];
        full.extend_from_slice(args);
        self.restore(server, &full)
    }

    /// Where a Claude Code home keeps one session's transcript.
    fn harness_transcript(&self, slug: &str, session_id: &str) -> PathBuf {
        self.claude_home
            .join("projects")
            .join(slug)
            .join(format!("{session_id}.jsonl"))
    }

    /// The transcript path operational state recorded for a session. Read from the database
    /// because the `sessions` contract deliberately does not expose it, and *which* copy the row
    /// points at is exactly what a resume changes.
    fn recorded_transcript_path(&self, session_id: &str) -> Option<String> {
        let connection = rusqlite::Connection::open(self.state.join("munshi.db")).unwrap();
        connection
            .query_row(
                "SELECT transcript_path FROM sessions WHERE source_session_id = ?1",
                [session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
    }

    fn sessions_json(&self) -> Value {
        let output = self
            .command("sessions")
            .arg("--json")
            .arg("--state-dir")
            .arg(&self.state)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "sessions failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// A `munshi` invocation whose whole environment is confined to this machine.
    fn command(&self, subcommand: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
        command
            .arg(subcommand)
            .env("HOME", self.root.path())
            .env("MUNSHI_HOME", &self.state)
            .env("COPILOT_HOME", &self.copilot_home)
            .env("CLAUDE_CONFIG_DIR", &self.claude_home);
        command
    }
}

struct RunOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr_bytes: Vec<u8>,
}

impl RunOutput {
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr_bytes).into_owned()
    }
}

fn test_directory() -> TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-restore");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(root)
        .unwrap()
}

/// One artifact's entry in a snapshot report, by logical path.
fn artifact<'a>(snapshot: &'a Value, logical_path: &str) -> &'a Value {
    snapshot["artifacts"]
        .as_array()
        .expect("artifacts array")
        .iter()
        .find(|artifact| artifact["logical_path"] == logical_path)
        .unwrap_or_else(|| panic!("no {logical_path} artifact in {snapshot}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Harness: an archived session
// ---------------------------------------------------------------------------

/// One archived session as the archive holds it: the rendered `summary.md` Munshi's own renderer
/// produces, plus the transcript and the extracted output a snapshot of it would carry.
struct ArchivedSession {
    source: SourceKind,
    session_id: String,
    summary: String,
    transcript: Vec<u8>,
    extracted_output: Vec<u8>,
}

/// A Claude Code transcript shaped like the ones the harness writes: bookkeeping records that carry
/// no `cwd` or `version` come first (real `queue-operation` records do exactly this), so the scan
/// that reads the origin has to look past them.
fn claude_transcript(cwd: &str, version: &str) -> Vec<u8> {
    let session = "01234567-89ab-4cde-8f01-234567890abc";
    let records = [
        json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "timestamp": "2026-08-01T00:00:00Z",
            "sessionId": session,
            "content": "hello",
        }),
        json!({
            "type": "user",
            "uuid": "11111111-1111-4111-8111-111111111111",
            "parentUuid": Value::Null,
            "sessionId": session,
            "timestamp": "2026-08-01T00:00:01Z",
            "cwd": cwd,
            "version": version,
            "gitBranch": "main",
            "isSidechain": false,
            "userType": "external",
            "message": { "role": "user", "content": "hello" },
        }),
        json!({
            "type": "assistant",
            "uuid": "22222222-2222-4222-8222-222222222222",
            "parentUuid": "11111111-1111-4111-8111-111111111111",
            "sessionId": session,
            "timestamp": "2026-08-01T00:00:02Z",
            "cwd": cwd,
            "version": version,
            "gitBranch": "main",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "hi" }] },
        }),
    ];
    let mut bytes = Vec::new();
    for record in records {
        bytes.extend_from_slice(record.to_string().as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

impl ArchivedSession {
    fn copilot(session_id: &str, component: &str) -> Self {
        Self::copilot_revision(session_id, component, 2, b"{\"type\":\"user\"}\n")
    }

    fn copilot_revision(
        session_id: &str,
        component: &str,
        revision: u64,
        transcript: &[u8],
    ) -> Self {
        Self::revision(
            SourceKind::Copilot,
            session_id,
            component,
            revision,
            transcript,
        )
    }

    /// An archived Claude Code session whose transcript records `cwd` — the value the harness's
    /// `projects/<slug>` directory name encodes, and the only place a wiped machine can learn it.
    fn claude_code(session_id: &str, component: &str, cwd: &str) -> Self {
        Self::revision(
            SourceKind::ClaudeCode,
            session_id,
            component,
            2,
            &claude_transcript(cwd, "2.1.205"),
        )
    }

    fn revision(
        source: SourceKind,
        session_id: &str,
        component: &str,
        revision: u64,
        transcript: &[u8],
    ) -> Self {
        let hash = content_hash(transcript);
        let session = NormalizedSession {
            source,
            session_id: session_id.to_owned(),
            events: Vec::new(),
            user_requests: 1,
            assistant_messages: 1,
            tool_activities: 1,
            ignored_events: 0,
            source_cursor: 3,
            source_byte_cursor: transcript.len() as u64,
            source_prefix_hash: hash.clone(),
            source_hash: hash,
            source_bytes: transcript.len() as u64,
            started_at: None,
            updated_at: None,
            artifact_index: SnapshotArtifactIndex {
                extracted_outputs: Vec::new(),
            },
            opening_summary_request: false,
        };
        let project = ProjectIdentity {
            identity: "github.com/surdy/munshi".to_owned(),
            component: component.to_owned(),
            project: component.to_owned(),
            repository: Some("surdy/munshi".to_owned()),
            branch: None,
            origin: ProjectOrigin::Live,
        };
        let summary = StructuredSummary {
            title: format!("Archived {session_id}"),
            goal: "Restore the record.".to_owned(),
            work_completed: vec!["Did the work.".to_owned()],
            decisions: Vec::new(),
            files_changed: Vec::new(),
            commands_and_validation: Vec::new(),
            open_items: Vec::new(),
            tags: vec!["restore".to_owned()],
        };
        let markdown = render_revision_markdown(
            &ArchiveMetadata {
                session: &session,
                project: &project,
            },
            &summary,
            revision,
            "complete",
            None,
        );
        Self {
            source,
            session_id: session_id.to_owned(),
            summary: markdown,
            transcript: transcript.to_vec(),
            extracted_output: b"an elided tool output".to_vec(),
        }
    }

    fn summary_bytes(&self) -> Vec<u8> {
        self.summary.as_bytes().to_vec()
    }

    /// The snapshot the archive serves for this session: summary, transcript, and one extracted
    /// output, in artifact-set v2.
    fn snapshot(&self, snapshot_id: &str, patwari_session_id: &str) -> FakeSnapshot {
        let prefix = &snapshot_id[..4];
        FakeSnapshot {
            snapshot_id: snapshot_id.to_owned(),
            session_id: patwari_session_id.to_owned(),
            source_agent: self.source.agent_label().to_owned(),
            artifact_set_version: 2,
            artifacts: vec![
                FakeArtifact::identity(
                    &format!("art-summary-{prefix}"),
                    "summary.md",
                    self.summary.as_bytes(),
                ),
                FakeArtifact::zstd(
                    &format!("art-transcript-{prefix}"),
                    "transcript.jsonl",
                    &self.transcript,
                ),
                FakeArtifact::identity(
                    &format!("art-output-{prefix}"),
                    &format!("outputs/{}", sha256_hex(&self.extracted_output)),
                    &self.extracted_output,
                ),
            ],
        }
    }
}

impl FakeSnapshot {
    /// Adds the staged sidecar set an artifact-set-v2 Copilot snapshot carries.
    fn with_sidecars(mut self) -> Self {
        let prefix = self.snapshot_id[..4].to_owned();
        self.artifacts.push(FakeArtifact::identity(
            &format!("art-sidecar-plan-{prefix}"),
            "sidecar/plan.md",
            b"# plan\n",
        ));
        self.artifacts.push(FakeArtifact::identity(
            &format!("art-sidecar-checkpoint-{prefix}"),
            "sidecar/checkpoints/one.md",
            b"# checkpoint\n",
        ));
        self
    }
}

// ---------------------------------------------------------------------------
// Harness: fake Patwari archive daemon
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FakeArtifact {
    artifact_id: String,
    logical_path: String,
    compression: String,
    original_size_bytes: u64,
    original_sha256: String,
    /// The bytes the content route actually serves (may be tampered by a test).
    served_stored_bytes: Vec<u8>,
    /// The stored hash both the listing and the content headers declare (consistent with the
    /// untampered stored bytes).
    stored_sha256: String,
}

impl FakeArtifact {
    fn zstd(artifact_id: &str, logical_path: &str, original: &[u8]) -> Self {
        let stored = zstd::encode_all(original, 3).expect("zstd compress");
        Self::build(artifact_id, logical_path, original, &stored, "zstd")
    }

    fn identity(artifact_id: &str, logical_path: &str, original: &[u8]) -> Self {
        Self::build(artifact_id, logical_path, original, original, "identity")
    }

    fn build(
        artifact_id: &str,
        logical_path: &str,
        original: &[u8],
        stored: &[u8],
        compression: &str,
    ) -> Self {
        Self {
            artifact_id: artifact_id.to_owned(),
            logical_path: logical_path.to_owned(),
            compression: compression.to_owned(),
            original_size_bytes: original.len() as u64,
            original_sha256: sha256_hex(original),
            served_stored_bytes: stored.to_vec(),
            stored_sha256: sha256_hex(stored),
        }
    }

    fn listing_item(&self, snapshot_id: &str) -> Value {
        json!({
            "artifact_id": self.artifact_id,
            "snapshot_id": snapshot_id,
            "logical_path": self.logical_path,
            "media_type": "application/octet-stream",
            "original_size_bytes": self.original_size_bytes,
            "original_sha256": format!("sha256:{}", self.original_sha256),
            "stored_size_bytes": self.served_stored_bytes.len(),
            "stored_sha256": format!("sha256:{}", self.stored_sha256),
            "compression": self.compression,
            "created_at": "2026-08-01T00:00:00Z",
            "metadata_url": format!("/api/v1/artifacts/{}", self.artifact_id),
            "content_url": format!("/api/v1/artifacts/{}/content", self.artifact_id),
        })
    }
}

#[derive(Clone)]
struct FakeSnapshot {
    snapshot_id: String,
    session_id: String,
    source_agent: String,
    artifact_set_version: u64,
    artifacts: Vec<FakeArtifact>,
}

struct ArchiveState {
    snapshots: Vec<FakeSnapshot>,
    page_size: usize,
    requests: Vec<String>,
}

struct FakeArchive {
    port: u16,
    state: Arc<Mutex<ArchiveState>>,
}

impl FakeArchive {
    fn start(snapshots: Vec<FakeSnapshot>, page_size: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(ArchiveState {
            snapshots,
            page_size,
            requests: Vec::new(),
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

    fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }
}

fn handle_connection(mut stream: TcpStream, state: &Arc<Mutex<ArchiveState>>) {
    let Some((method, target)) = read_request(&mut stream) else {
        return;
    };
    let mut guard = state.lock().unwrap();
    guard.requests.push(target.clone());
    let response = route(&method, &target, &guard);
    drop(guard);
    let _ = stream.write_all(&response);
}

fn route(method: &str, target: &str, state: &ArchiveState) -> Vec<u8> {
    let path = target.split('?').next().unwrap_or(target);
    if method != "GET" {
        return not_found();
    }
    if path == "/api/v1/snapshots" {
        return list_snapshots(target, state);
    }
    if let Some(snapshot_id) = path
        .strip_prefix("/api/v1/snapshots/")
        .and_then(|rest| rest.strip_suffix("/manifest"))
    {
        return snapshot_manifest(snapshot_id, state);
    }
    if path == "/api/v1/artifacts" {
        return list_artifacts(target, state);
    }
    if let Some(artifact_id) = path
        .strip_prefix("/api/v1/artifacts/")
        .and_then(|rest| rest.strip_suffix("/content"))
    {
        return content(artifact_id, state);
    }
    not_found()
}

fn query_param(target: &str, name: &str) -> Option<String> {
    let query = target.split('?').nth(1)?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .map(percent_decode)
}

fn list_snapshots(target: &str, state: &ArchiveState) -> Vec<u8> {
    let session_filter = query_param(target, "session_id");
    let start: usize = query_param(target, "cursor")
        .and_then(|cursor| cursor.strip_prefix("cursor-").map(ToOwned::to_owned))
        .and_then(|index| index.parse().ok())
        .unwrap_or(0);
    let matching: Vec<&FakeSnapshot> = state
        .snapshots
        .iter()
        .filter(|snapshot| {
            session_filter
                .as_deref()
                .is_none_or(|session| snapshot.session_id == session)
        })
        .collect();
    let page: Vec<Value> = matching
        .iter()
        .skip(start)
        .take(state.page_size)
        .map(|snapshot| {
            json!({
                "snapshot_id": snapshot.snapshot_id,
                "session_id": snapshot.session_id,
                "completed_at": "2026-08-01T00:00:00Z",
                "artifact_count": snapshot.artifacts.len(),
                "snapshot_url": format!("/api/v1/snapshots/{}", snapshot.snapshot_id),
                "manifest_url": format!("/api/v1/snapshots/{}/manifest", snapshot.snapshot_id),
            })
        })
        .collect();
    let next = start + page.len();
    let next_cursor = if next < matching.len() {
        Value::String(format!("cursor-{next}"))
    } else {
        Value::Null
    };
    json_response(
        200,
        &json!({ "items": page, "next_cursor": next_cursor, "high_watermark": Value::Null }),
    )
}

fn snapshot_manifest(snapshot_id: &str, state: &ArchiveState) -> Vec<u8> {
    let Some(snapshot) = state
        .snapshots
        .iter()
        .find(|snapshot| snapshot.snapshot_id == snapshot_id)
    else {
        return not_found();
    };
    json_response(
        200,
        &json!({
            "manifest_id": format!("manifest-{snapshot_id}"),
            "snapshot_id": snapshot.snapshot_id,
            "session_id": snapshot.session_id,
            "manifest": {
                "schema_version": 1,
                "session": {
                    "source_agent": snapshot.source_agent,
                    "source_session_id": format!("src-{}", snapshot.session_id),
                },
                "capture": {
                    "captured_at": "2026-08-01T00:00:00Z",
                    "artifact_set_version": snapshot.artifact_set_version,
                },
                "artifacts": [],
            },
        }),
    )
}

fn list_artifacts(target: &str, state: &ArchiveState) -> Vec<u8> {
    let snapshot_id = query_param(target, "snapshot_id").unwrap_or_default();
    let items: Vec<Value> = state
        .snapshots
        .iter()
        .filter(|snapshot| snapshot.snapshot_id == snapshot_id)
        .flat_map(|snapshot| {
            snapshot
                .artifacts
                .iter()
                .map(|artifact| artifact.listing_item(&snapshot.snapshot_id))
        })
        .collect();
    json_response(
        200,
        &json!({ "items": items, "next_cursor": Value::Null, "high_watermark": Value::Null }),
    )
}

fn content(artifact_id: &str, state: &ArchiveState) -> Vec<u8> {
    let Some(artifact) = state
        .snapshots
        .iter()
        .flat_map(|snapshot| snapshot.artifacts.iter())
        .find(|artifact| artifact.artifact_id == artifact_id)
    else {
        return not_found();
    };
    let body = artifact.served_stored_bytes.clone();
    let mut head = String::new();
    head.push_str("HTTP/1.1 200 OK\r\n");
    head.push_str("Connection: close\r\n");
    head.push_str("Content-Type: application/octet-stream\r\n");
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str(&format!(
        "x-patwari-compression: {}\r\n",
        artifact.compression
    ));
    head.push_str(&format!(
        "x-patwari-original-size-bytes: {}\r\n",
        artifact.original_size_bytes
    ));
    head.push_str(&format!(
        "x-patwari-original-sha256: sha256:{}\r\n",
        artifact.original_sha256
    ));
    head.push_str(&format!("x-patwari-stored-size-bytes: {}\r\n", body.len()));
    head.push_str(&format!(
        "x-patwari-stored-sha256: sha256:{}\r\n",
        artifact.stored_sha256
    ));
    head.push_str("\r\n");
    let mut response = head.into_bytes();
    response.extend_from_slice(&body);
    response
}

fn not_found() -> Vec<u8> {
    json_response(
        404,
        &json!({ "error": { "code": "not_found", "message": "unhandled" } }),
    )
}

fn json_response(status: u16, value: &Value) -> Vec<u8> {
    let body = value.to_string();
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Status",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Reads one request, returning its method and target. GET requests carry no body here.
fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let request_line = head.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    Some((method, target))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                output.push(byte);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}
