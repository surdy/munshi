//! Integration coverage for `munshi verify-archive-parse` (issue #28).
//!
//! These tests drive the real `munshi` binary against an in-process fake Patwari daemon serving
//! the archive-walk read surface: the cursor-paginated snapshot listing, per-snapshot canonical
//! manifests, the snapshot-filtered artifact listing, and the stored-bytes content route with its
//! `x-patwari-*` metadata headers. Served transcript bodies are built from the committed adapter
//! fixtures under `fixtures/`, so the accounting the command reports is checked against a direct
//! `munshi-transcript` fold over the same bytes.
//!
//! Covered: a clean two-snapshot walk (exit 0, counts match the fixture fold), unknown record
//! kinds and malformed lines as findings (exit 4), a stored-hash mismatch as a verification
//! failure that does not stop the walk (exit 6), unsupported artifact-set versions / unknown
//! source agents / missing transcripts as skipped-not-fatal accounting, cursor pagination across
//! multiple listing pages, the `--session` filter, and the unconfigured-endpoint error path.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use munshi_transcript::{SessionSummary, Source, TranscriptStream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn clean_walk_parses_every_snapshot_and_matches_the_fixture_fold() {
    let claude = claude_fixture_bytes();
    let codex = codex_fixture_bytes();
    let server = FakeArchive::start(
        vec![
            FakeSnapshot::new(
                "11111111-1111-4111-8111-111111111111",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "claude-code",
                1,
                vec![
                    FakeArtifact::zstd("art-claude", "transcript.jsonl", &claude),
                    FakeArtifact::identity("art-claude-summary", "summary.md", b"# summary"),
                ],
            ),
            FakeSnapshot::new(
                "22222222-2222-4222-8222-222222222222",
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "codex-cli",
                1,
                vec![FakeArtifact::zstd("art-codex", "transcript.jsonl", &codex)],
            ),
        ],
        50,
    );

    let output = run(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["command"], "verify-archive-parse");
    assert_eq!(report["session_filter"], Value::Null);
    assert_eq!(report["totals"]["snapshots"], 2);
    assert_eq!(report["totals"]["parsed"], 2);
    assert_eq!(report["totals"]["skipped"], 0);
    assert_eq!(report["totals"]["unknown_records"], 0);
    assert_eq!(report["totals"]["record_errors"], 0);

    let (claude_records, claude_fold) = fixture_fold(Source::ClaudeCode, &claude);
    let (codex_records, codex_fold) = fixture_fold(Source::Codex, &codex);
    assert_accounting_matches(&report["snapshots"][0], claude_records, &claude_fold);
    assert_accounting_matches(&report["snapshots"][1], codex_records, &codex_fold);
    assert_eq!(report["snapshots"][0]["status"]["result"], "parsed");
    assert_eq!(report["snapshots"][1]["status"]["result"], "parsed");

    // The codex fixture's deliberately-unarchived kinds appear as named ignored counts.
    let codex_ignored = &report["snapshots"][1]["accounting"]["ignored_kinds"];
    assert_eq!(codex_ignored["session_meta"], 1);
    assert_eq!(codex_ignored["turn_context"], 1);
    assert_eq!(codex_ignored["reasoning"], 1);

    // One aggregate per (source_agent, artifact_set_version) pair.
    let aggregates = report["aggregates"].as_array().unwrap();
    assert_eq!(aggregates.len(), 2);
    assert!(
        aggregates
            .iter()
            .any(|group| group["source_agent"] == "claude-code"
                && group["artifact_set_version"] == 1
                && group["parsed"] == 1)
    );

    // The human rendering also succeeds and reports the clean verdict.
    let human = run(&server, &["--all"]);
    assert_eq!(human.status.code(), Some(0), "stderr: {}", human.stderr());
    let printed = String::from_utf8_lossy(&human.stdout);
    assert!(printed.contains("2 parsed"), "got: {printed}");
    assert!(printed.contains("no findings"), "got: {printed}");
}

#[test]
fn unknown_records_and_malformed_lines_are_findings() {
    // The clean claude fixture (5 records) plus an unknown record kind on line 6 and a malformed
    // line on line 7 — precisely the interpretation gaps the tool exists to reveal.
    let mut transcript = claude_fixture_bytes();
    transcript.extend_from_slice(
        br#"{"type":"wibble-2.2","timestamp":"2026-07-11T21:00:00.000Z","payload":{"secret":"handshake"}}"#,
    );
    transcript.push(b'\n');
    transcript.extend_from_slice(b"{\"type\":\"assistant\",\"message\":{\"content\":");
    transcript.push(b'\n');

    let server = FakeArchive::start(
        vec![FakeSnapshot::new(
            "11111111-1111-4111-8111-111111111111",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "claude-code",
            1,
            vec![FakeArtifact::zstd("art-1", "transcript.jsonl", &transcript)],
        )],
        50,
    );

    let output = run(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["totals"]["parsed"], 1,
        "findings do not abort the parse"
    );

    let accounting = &report["snapshots"][0]["accounting"];
    assert_eq!(accounting["records_seen"], 7);
    assert_eq!(accounting["unknown_records"], 1);
    assert_eq!(accounting["unknown_kinds"]["wibble-2.2"], 1);
    let sample = &accounting["unknown_samples"][0];
    assert_eq!(sample["line"], 6);
    assert!(
        sample["raw"].as_str().unwrap().contains("wibble-2.2"),
        "sample carries the raw record: {sample}"
    );
    assert_eq!(accounting["record_errors"], 1);
    assert_eq!(accounting["record_error_samples"][0]["line"], 7);
}

#[test]
fn stored_hash_mismatch_is_a_verification_failure_and_the_walk_continues() {
    let claude = claude_fixture_bytes();
    let codex = codex_fixture_bytes();
    let mut tampered = FakeArtifact::zstd("art-bad", "transcript.jsonl", &claude);
    // Flip a stored byte so it no longer matches the declared stored sha256.
    tampered.served_stored_bytes[0] ^= 0xff;
    let server = FakeArchive::start(
        vec![
            FakeSnapshot::new(
                "11111111-1111-4111-8111-111111111111",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "claude-code",
                1,
                vec![tampered],
            ),
            FakeSnapshot::new(
                "22222222-2222-4222-8222-222222222222",
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "codex-cli",
                1,
                vec![FakeArtifact::zstd("art-good", "transcript.jsonl", &codex)],
            ),
        ],
        50,
    );

    let output = run(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(6), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(report["snapshots"][0]["status"]["result"], "failed");
    assert_eq!(report["snapshots"][0]["status"]["class"], "verification");
    // The failure did not stop the walk: the second snapshot still parsed cleanly.
    assert_eq!(report["snapshots"][1]["status"]["result"], "parsed");
    assert_eq!(report["totals"]["failed_verification"], 1);
    assert_eq!(report["totals"]["parsed"], 1);
}

#[test]
fn unsupported_versions_and_unknown_agents_are_skipped_not_fatal() {
    let claude = claude_fixture_bytes();
    let server = FakeArchive::start(
        vec![
            FakeSnapshot::new(
                "11111111-1111-4111-8111-111111111111",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "mystery-agent-9000",
                1,
                vec![FakeArtifact::zstd("art-1", "transcript.jsonl", &claude)],
            ),
            FakeSnapshot::new(
                "22222222-2222-4222-8222-222222222222",
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "claude-code",
                2,
                vec![FakeArtifact::zstd("art-2", "transcript.jsonl", &claude)],
            ),
            FakeSnapshot::new(
                "33333333-3333-4333-8333-333333333333",
                "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                "claude-code",
                1,
                vec![FakeArtifact::identity(
                    "art-3",
                    "summary.md",
                    b"# no transcript",
                )],
            ),
        ],
        50,
    );

    let output = run(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(report["totals"]["skipped"], 3);
    assert_eq!(report["totals"]["parsed"], 0);
    let statuses: Vec<(&str, &str)> = report["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|snapshot| {
            (
                snapshot["status"]["result"].as_str().unwrap(),
                snapshot["status"]["reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("skipped", "unknown-source-agent"),
            ("skipped", "unsupported-artifact-set-version"),
            ("skipped", "no-transcript-artifact"),
        ]
    );
    // Skipped snapshots are never downloaded: no content route was ever requested.
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
fn pagination_follows_cursors_across_listing_pages() {
    let claude = claude_fixture_bytes();
    let snapshots: Vec<FakeSnapshot> = (1..=3)
        .map(|index| {
            FakeSnapshot::new(
                &format!("{index}{index}{index}{index}{index}{index}{index}{index}-1111-4111-8111-111111111111"),
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "claude-code",
                1,
                vec![FakeArtifact::zstd(
                    &format!("art-{index}"),
                    "transcript.jsonl",
                    &claude,
                )],
            )
        })
        .collect();
    // Two snapshots per page forces a second page behind a cursor.
    let server = FakeArchive::start(snapshots, 2);

    let output = run(&server, &["--all", "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["totals"]["snapshots"], 3);
    assert_eq!(report["totals"]["parsed"], 3);

    // The traversal continued only with the returned cursor.
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
        "second page uses the returned cursor: {snapshot_requests:?}"
    );
}

#[test]
fn session_filter_walks_only_that_session() {
    let claude = claude_fixture_bytes();
    let codex = codex_fixture_bytes();
    let wanted_session = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let server = FakeArchive::start(
        vec![
            FakeSnapshot::new(
                "11111111-1111-4111-8111-111111111111",
                wanted_session,
                "claude-code",
                1,
                vec![FakeArtifact::zstd("art-1", "transcript.jsonl", &claude)],
            ),
            FakeSnapshot::new(
                "22222222-2222-4222-8222-222222222222",
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "codex-cli",
                1,
                vec![FakeArtifact::zstd("art-2", "transcript.jsonl", &codex)],
            ),
        ],
        50,
    );

    let output = run(&server, &["--session", wanted_session, "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["session_filter"], wanted_session);
    assert_eq!(report["totals"]["snapshots"], 1);
    assert_eq!(report["snapshots"][0]["session_id"], wanted_session);
}

#[test]
fn unconfigured_endpoint_reports_not_configured() {
    // No --endpoint and an unregistered state directory: the walk has no server to reach.
    let state = test_directory();
    let output = run_raw(state.path(), &["--all"]);
    assert_eq!(output.status.code(), Some(3), "stderr: {}", output.stderr());
}

// ---------------------------------------------------------------------------
// Real Patwari end-to-end (opt-in, ignored by default)
// ---------------------------------------------------------------------------

/// Uploads one snapshot to a locally built Patwari server, then walks the archive with
/// `verify-archive-parse` and proves the transcript downloads, verifies, and parses cleanly.
///
/// Opt in by pointing `PATWARI_SERVER_BIN` at a built `patwari-server` binary:
/// `cargo build -p patwari-server` in the patwari repo, then
/// `PATWARI_SERVER_BIN=/path/to/target/debug/patwari-server \
///   cargo test -p munshi --test verify_archive -- --ignored real_patwari`.
#[test]
#[ignore = "requires a locally built patwari-server via PATWARI_SERVER_BIN"]
fn real_patwari_walk_parses_an_uploaded_snapshot() {
    use munshi::{
        CaptureContext, INITIAL_ARTIFACT_SET_VERSION, PatwariClient, SessionContext,
        build_manifest, prepare_artifacts,
    };
    use std::collections::BTreeMap;

    let Ok(binary) = std::env::var("PATWARI_SERVER_BIN") else {
        eprintln!("PATWARI_SERVER_BIN not set; skipping real Patwari end-to-end test");
        return;
    };
    let data_dir = TempDir::new().unwrap();
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut server = Command::new(&binary)
        .arg("serve")
        .env("PATWARI_DATA_DIR", data_dir.path())
        .env("PATWARI_BIND_ADDR", format!("127.0.0.1:{port}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn patwari-server");
    let endpoint = format!("http://127.0.0.1:{port}");
    struct ChildGuard<'a>(&'a mut std::process::Child);
    impl Drop for ChildGuard<'_> {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let guard = ChildGuard(&mut server);

    const CLIENT_ID: &str = "11111111-1111-4111-8111-111111111111";
    let client = PatwariClient::connect(&endpoint, CLIENT_ID).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let metadata = BTreeMap::new();
    loop {
        if client
            .register_client(Some("test-host"), None, &metadata)
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

    let transcript = std::fs::read(fixture_path(
        "copilot-1.0.70/transcript/synthetic-envelope.jsonl",
    ))
    .unwrap();
    let artifacts = prepare_artifacts(vec![
        munshi::ArtifactSource {
            logical_path: "summary.md".to_owned(),
            media_type: Some("text/markdown".to_owned()),
            bytes: b"# Verify-archive-parse end-to-end\n".to_vec(),
        },
        munshi::ArtifactSource {
            logical_path: "transcript.jsonl".to_owned(),
            media_type: Some("application/jsonl".to_owned()),
            bytes: transcript.clone(),
        },
    ]);
    let manifest = build_manifest(
        &SessionContext {
            source_agent: "copilot-cli".to_owned(),
            source_session_id: "real-sess-1".to_owned(),
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
        &artifacts,
    );
    client
        .upload_snapshot("real-capture-1", &manifest, &artifacts, None, |_| {})
        .expect("real upload succeeds");

    let state = TempDir::new().unwrap();
    let output = run_raw(state.path(), &["--all", "--endpoint", &endpoint, "--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", output.stderr());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["totals"]["parsed"], 1);
    assert_eq!(report["totals"]["unknown_records"], 0);
    assert_eq!(report["totals"]["record_errors"], 0);
    let (records, fold) = fixture_fold(Source::Copilot, &transcript);
    assert_accounting_matches(&report["snapshots"][0], records, &fold);

    drop(guard);
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../fixtures/{relative}"))
}

fn claude_fixture_bytes() -> Vec<u8> {
    std::fs::read(fixture_path(
        "claude-code-2.1.44/normal/0c1a0de0-0000-4000-8000-000000000001.jsonl",
    ))
    .unwrap()
}

fn codex_fixture_bytes() -> Vec<u8> {
    std::fs::read(fixture_path(
        "codex-rollout-0.x/normal/c0de0000-0000-4000-8000-000000000001.jsonl",
    ))
    .unwrap()
}

/// Streams a fixture directly through `munshi-transcript`, returning the total stream item count
/// and the legacy counting fold — the reference the CLI's reported accounting must match.
fn fixture_fold(source: Source, bytes: &[u8]) -> (u64, SessionSummary) {
    let items: Vec<_> = TranscriptStream::new(source, 1, bytes).unwrap().collect();
    let summary = SessionSummary::summarize(&items);
    (items.len() as u64, summary)
}

/// Asserts one reported snapshot's accounting equals the direct fixture fold.
fn assert_accounting_matches(snapshot: &Value, records: u64, fold: &SessionSummary) {
    let accounting = &snapshot["accounting"];
    assert_eq!(accounting["records_seen"], records, "in {snapshot}");
    assert_eq!(accounting["user_events"], fold.user_requests as u64);
    assert_eq!(
        accounting["assistant_events"],
        fold.assistant_messages as u64
    );
    assert_eq!(accounting["tool_events"], fold.tool_activities as u64);
    assert_eq!(accounting["record_errors"], fold.malformed_records as u64);
    // The fixtures carry no unknown records, so the fold's lumped ignored count is exact.
    assert_eq!(accounting["unknown_records"], 0);
    assert_eq!(accounting["ignored_records"], fold.ignored_events as u64);
}

fn test_directory() -> TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/munshi-verify-archive");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(root)
        .unwrap()
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

/// Runs `munshi verify-archive-parse <args> --endpoint <server> --state-dir <fresh>`.
fn run(server: &FakeArchive, args: &[&str]) -> RunOutput {
    let state = test_directory();
    let endpoint = server.endpoint();
    let mut full: Vec<&str> = args.to_vec();
    full.push("--endpoint");
    full.push(&endpoint);
    run_raw(state.path(), &full)
}

fn run_raw(state_dir: &Path, args: &[&str]) -> RunOutput {
    let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
    command.arg("verify-archive-parse");
    command.args(args);
    command.arg("--state-dir").arg(state_dir);
    let output = command.output().expect("run munshi verify-archive-parse");
    RunOutput {
        status: output.status,
        stdout: output.stdout,
        stderr_bytes: output.stderr,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Fake Patwari archive daemon
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
            "created_at": "2026-07-25T00:00:00Z",
            "metadata_url": format!("/api/v1/artifacts/{}", self.artifact_id),
            "content_url": format!("/api/v1/artifacts/{}/content", self.artifact_id),
        })
    }

    fn manifest_entry(&self) -> Value {
        json!({
            "logical_path": self.logical_path,
            "media_type": "application/octet-stream",
            "original_size_bytes": self.original_size_bytes,
            "original_sha256": format!("sha256:{}", self.original_sha256),
            "stored_size_bytes": self.served_stored_bytes.len(),
            "stored_sha256": format!("sha256:{}", self.stored_sha256),
            "compression": self.compression,
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

impl FakeSnapshot {
    fn new(
        snapshot_id: &str,
        session_id: &str,
        source_agent: &str,
        artifact_set_version: u64,
        artifacts: Vec<FakeArtifact>,
    ) -> Self {
        Self {
            snapshot_id: snapshot_id.to_owned(),
            session_id: session_id.to_owned(),
            source_agent: source_agent.to_owned(),
            artifact_set_version,
            artifacts,
        }
    }
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
                "completed_at": "2026-07-25T00:00:00Z",
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
    let artifacts: Vec<Value> = snapshot
        .artifacts
        .iter()
        .map(FakeArtifact::manifest_entry)
        .collect();
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
                    "captured_at": "2026-07-25T00:00:00Z",
                    "artifact_set_version": snapshot.artifact_set_version,
                },
                "artifacts": artifacts,
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
