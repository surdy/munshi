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
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["totals"]["artifacts_skipped"], 1);
    assert_eq!(report["totals"]["artifacts_written"], 2);
    assert!(
        !machine
            .output
            .path()
            .join("munshi/sess-one.restored/outputs")
            .exists()
    );
    // Skipping a re-derivable artifact still leaves the record restored, not failed.
    assert_eq!(report["snapshots"][0]["status"]["result"], "restored");
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
        let summarizer = self.root.path().join("summarizer.sh");
        std::fs::write(&summarizer, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&summarizer, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = self
            .command("register")
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
    session_id: String,
    summary: String,
    transcript: Vec<u8>,
    extracted_output: Vec<u8>,
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
        let hash = content_hash(transcript);
        let session = NormalizedSession {
            source: SourceKind::Copilot,
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
            source_agent: "copilot-cli".to_owned(),
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
