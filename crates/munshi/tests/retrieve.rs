//! Integration coverage for `munshi retrieve` (issue #22).
//!
//! These tests drive the real `munshi` binary against an in-process fake Patwari daemon that serves
//! the hash-addressed artifact listing (`GET /api/v1/artifacts?original_sha256=…`) and the
//! stored-bytes content route with its `x-patwari-*` metadata headers, exactly as the issue #19
//! upload tests drive the real client against a fake daemon. They prove the byte-for-byte round
//! trip, newest-match selection, `--list`, client-side `--query`, and that tampered or mismatched
//! content exits non-zero without emitting any bytes — plus that a malformed hash is rejected
//! locally before any network access.
//!
//! Retrieval's server is addressed with the `--endpoint` override so a test needs no registered
//! configuration; the unconfigured-endpoint test exercises the configuration-backed path instead.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn round_trips_a_zstd_artifact_to_stdout() {
    let original = b"# Extracted transcript\n".repeat(64);
    let server = FakePatwari::start(vec![StoredArtifact::compressed(
        "art-1",
        "snap-1",
        "transcript.jsonl",
        &original,
        "2026-07-25T00:00:00Z",
    )]);
    let hash = sha256_hex(&original);

    let output = run(&server, &[&hash]);
    assert!(output.status.success(), "stderr: {}", output.stderr());
    assert_eq!(
        output.stdout, original,
        "stdout reproduces the original bytes"
    );
}

#[test]
fn round_trips_to_an_output_file() {
    let original = b"identity content that does not compress".to_vec();
    let server = FakePatwari::start(vec![StoredArtifact::identity(
        "art-1",
        "snap-1",
        "summary.md",
        &original,
        "2026-07-25T00:00:00Z",
    )]);
    let hash = sha256_hex(&original);
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("out.bin");
    let target = target.to_str().unwrap();

    let output = run(&server, &[&hash, "--output", target]);
    assert!(output.status.success(), "stderr: {}", output.stderr());
    assert!(
        output.stdout.is_empty(),
        "no bytes go to stdout with --output"
    );
    assert_eq!(std::fs::read(target).unwrap(), original);

    // Refuses to clobber the existing file without --force.
    let refused = run(&server, &[&hash, "--output", target]);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "stderr: {}",
        refused.stderr()
    );

    // --force replaces it.
    let forced = run(&server, &[&hash, "--output", target, "--force"]);
    assert!(forced.status.success(), "stderr: {}", forced.stderr());
    assert_eq!(std::fs::read(target).unwrap(), original);
}

#[test]
fn multiple_matches_pick_the_newest_and_list_shows_all() {
    let original = b"shared deduplicated content".to_vec();
    // Two snapshots dedup the same original bytes. The bytes are identical, so both reproduce the
    // original — but --list must show both and the default must choose the newest (by created_at).
    let older = StoredArtifact::identity(
        "art-old",
        "snap-old",
        "old/path.txt",
        &original,
        "2026-07-24T00:00:00Z",
    );
    let newer = StoredArtifact::identity(
        "art-new",
        "snap-new",
        "new/path.txt",
        &original,
        "2026-07-25T00:00:00Z",
    );
    let server = FakePatwari::start(vec![older, newer]);
    let hash = sha256_hex(&original);

    // --list --json shows both matches, newest first.
    let listed = run(&server, &[&hash, "--list", "--json"]);
    assert!(listed.status.success(), "stderr: {}", listed.stderr());
    let report: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(report["total"], 2);
    assert_eq!(report["items"][0]["artifact_id"], "art-new");
    assert_eq!(report["items"][1]["artifact_id"], "art-old");

    // Default retrieval downloads the newest artifact's content route.
    let retrieved = run(&server, &[&hash]);
    assert!(retrieved.status.success(), "stderr: {}", retrieved.stderr());
    assert_eq!(retrieved.stdout, original);
    assert_eq!(
        server.last_content_artifact().as_deref(),
        Some("art-new"),
        "the newest artifact's content was fetched"
    );
}

#[test]
fn query_prints_matching_lines_with_context() {
    let original = b"line one\nline two\nHERE is the needle\nline four\nline five\n".to_vec();
    let server = FakePatwari::start(vec![StoredArtifact::compressed(
        "art-1",
        "snap-1",
        "transcript.jsonl",
        &original,
        "2026-07-25T00:00:00Z",
    )]);
    let hash = sha256_hex(&original);

    let output = run(&server, &[&hash, "--query", "needle"]);
    assert!(output.status.success(), "stderr: {}", output.stderr());
    let printed = String::from_utf8_lossy(&output.stdout);
    // The matching line is present, marked with ':' and its 1-based line number.
    assert!(printed.contains("3:HERE is the needle"), "got: {printed}");
    // Context lines either side are present, marked with '-' (line 1 is within 2 lines of context).
    assert!(printed.contains("2-line two"), "got: {printed}");
    assert!(printed.contains("4-line four"), "got: {printed}");
    assert!(printed.contains("1-line one"), "got: {printed}");
    // Query mode prints grep-style annotated lines, never a raw content dump.
    assert_ne!(
        output.stdout, original,
        "query output is not the raw content"
    );
}

#[test]
fn tampered_stored_bytes_exit_nonzero_without_emitting() {
    let original = b"authentic content".to_vec();
    let mut artifact = StoredArtifact::compressed(
        "art-1",
        "snap-1",
        "transcript.jsonl",
        &original,
        "2026-07-25T00:00:00Z",
    );
    // Flip a stored byte so it no longer matches the declared stored sha256 header.
    artifact.served_stored_bytes[0] ^= 0xff;
    let server = FakePatwari::start(vec![artifact]);
    let hash = sha256_hex(&original);

    let output = run(&server, &[&hash]);
    assert_eq!(output.status.code(), Some(6), "stderr: {}", output.stderr());
    assert!(
        output.stdout.is_empty(),
        "no bytes are emitted on a mismatch"
    );
}

#[test]
fn wrong_original_hash_exits_nonzero_without_emitting() {
    // The listing matches the requested hash, but the content route serves a genuinely different
    // artifact (with its own consistent stored/original hashes). Stored verification passes; the
    // decompressed original digest then fails to match the requested hash.
    let requested = b"the content the ticket claims".to_vec();
    let served = b"entirely different content".to_vec();
    let mut artifact = StoredArtifact::compressed(
        "art-1",
        "snap-1",
        "transcript.jsonl",
        &served,
        "2026-07-25T00:00:00Z",
    );
    // The listing advertises the requested hash even though the content is different.
    artifact.listing_original_sha256 = sha256_hex(&requested);
    let server = FakePatwari::start(vec![artifact]);
    let hash = sha256_hex(&requested);

    let output = run(&server, &[&hash]);
    assert_eq!(output.status.code(), Some(6), "stderr: {}", output.stderr());
    assert!(
        output.stdout.is_empty(),
        "no bytes are emitted on a mismatch"
    );
}

#[test]
fn oversized_artifact_is_refused_with_a_distinct_code() {
    let original = b"content larger than a tiny download cap".to_vec();
    let server = FakePatwari::start(vec![StoredArtifact::identity(
        "art-1",
        "snap-1",
        "summary.md",
        &original,
        "2026-07-25T00:00:00Z",
    )]);
    let hash = sha256_hex(&original);

    // A cap below the artifact's stored size refuses it up front with the distinct TooLarge exit
    // code (7), emitting nothing — never a misleading truncated stored-size verification failure.
    let refused = run(&server, &[&hash, "--max-download-bytes", "4"]);
    assert_eq!(
        refused.status.code(),
        Some(7),
        "stderr: {}",
        refused.stderr()
    );
    assert!(
        refused.stdout.is_empty(),
        "no bytes are emitted when refused"
    );
    assert!(
        refused.stderr().contains("exceeds"),
        "stderr names the size: {}",
        refused.stderr()
    );

    // Raising the cap above the stored size retrieves it normally.
    let ok = run(&server, &[&hash, "--max-download-bytes", "10000000"]);
    assert!(ok.status.success(), "stderr: {}", ok.stderr());
    assert_eq!(ok.stdout, original);
}

#[test]
fn missing_hash_reports_not_found() {
    let server = FakePatwari::start(vec![]);
    let hash = sha256_hex(b"never uploaded");

    let output = run(&server, &[&hash]);
    assert_eq!(output.status.code(), Some(4), "stderr: {}", output.stderr());
    assert!(
        output.stderr().contains("may not be uploaded yet"),
        "stderr: {}",
        output.stderr()
    );
}

#[test]
fn malformed_hash_is_rejected_locally_before_any_network() {
    // A dead endpoint proves the malformed hash fails before any connection is attempted.
    let state = TempDir::new().unwrap();
    let output = run_raw(
        state.path(),
        &["not-a-valid-hash", "--endpoint", "http://127.0.0.1:1"],
    );
    assert_eq!(output.status.code(), Some(2), "stderr: {}", output.stderr());
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr().contains("invalid content hash"),
        "stderr: {}",
        output.stderr()
    );
}

#[test]
fn unconfigured_endpoint_reports_not_configured() {
    // No --endpoint and an unregistered state directory: retrieval has no server to reach.
    let state = TempDir::new().unwrap();
    let hash = sha256_hex(b"anything");
    let output = run_raw(state.path(), &[&hash]);
    assert_eq!(output.status.code(), Some(3), "stderr: {}", output.stderr());
}

// ---------------------------------------------------------------------------
// Real Patwari end-to-end (opt-in, ignored by default)
// ---------------------------------------------------------------------------

/// Uploads a snapshot to a locally built Patwari server, then retrieves each artifact by its
/// original hash and proves the bytes round-trip through the real listing and content routes.
///
/// Opt in by pointing `PATWARI_SERVER_BIN` at a built `patwari-server` binary:
/// `cargo build -p patwari-server` in the patwari repo, then
/// `PATWARI_SERVER_BIN=/path/to/target/debug/patwari-server \
///   cargo test -p munshi --test retrieve -- --ignored real_patwari`.
#[test]
#[ignore = "requires a locally built patwari-server via PATWARI_SERVER_BIN"]
fn real_patwari_round_trips_uploaded_artifacts() {
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
    let mut metadata = BTreeMap::new();
    metadata.insert("munshi_version".to_owned(), "0.1.0".to_owned());
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

    let summary = b"# Real retrieval end-to-end\n\nBody content that compresses well well well.";
    let transcript = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";
    let artifacts = prepare_artifacts(vec![
        munshi::ArtifactSource {
            logical_path: "summary.md".to_owned(),
            media_type: Some("text/markdown".to_owned()),
            bytes: summary.to_vec(),
        },
        munshi::ArtifactSource {
            logical_path: "transcript.jsonl".to_owned(),
            media_type: Some("application/jsonl".to_owned()),
            bytes: transcript.to_vec(),
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

    // The binary retrieves each artifact from the real server by its original hash.
    let state = TempDir::new().unwrap();
    for (bytes, needle) in [
        (summary.to_vec(), "compresses"),
        (transcript.to_vec(), "assistant"),
    ] {
        let hash = sha256_hex(&bytes);
        let output = run_raw(state.path(), &[&hash, "--endpoint", &endpoint]);
        assert!(output.status.success(), "stderr: {}", output.stderr());
        assert_eq!(output.stdout, bytes, "byte-for-byte round trip");

        let query = run_raw(
            state.path(),
            &[&hash, "--endpoint", &endpoint, "--query", needle],
        );
        assert!(query.status.success(), "stderr: {}", query.stderr());
        assert!(String::from_utf8_lossy(&query.stdout).contains(needle));
    }

    drop(guard);
}

// ---------------------------------------------------------------------------
// Local redemption (`--local`, issue #25 groundwork)
// ---------------------------------------------------------------------------

/// Writes a Copilot transcript containing one oversized tool output, registers the session in a
/// fresh state directory the way an agent-stop hook would, and returns the state dir plus the
/// ticket hash and original content of the oversized event.
fn seed_local_session(session_id: &str) -> (TempDir, TempDir, String, Vec<u8>) {
    let state = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let session_dir = home.path().join("session-state").join(session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let transcript_path = session_dir.join("events.jsonl");
    let oversized = "x".repeat(500);
    let transcript = format!(
        "{}\n{}\n",
        json!({
            "id": "user-record",
            "timestamp": "2026-07-25T00:00:00Z",
            "parentId": "root",
            "type": "user.message",
            "data": { "content": "run the build" },
        }),
        json!({
            "id": "call-1",
            "timestamp": "2026-07-25T00:00:00Z",
            "parentId": "root",
            "type": "tool.execution_complete",
            "data": { "toolCallId": "call-1", "success": true, "result": { "content": oversized } },
        }),
    )
    .into_bytes();
    std::fs::write(&transcript_path, &transcript).unwrap();

    let mut store =
        munshi::StateStore::open_for_source(state.path(), munshi::SourceKind::Copilot).unwrap();
    store
        .ingest_agent_stop(session_id, 1_753_400_000_000, home.path(), &transcript_path)
        .unwrap();

    // The ticket hash is whatever extraction addresses the oversized event as, derived through the
    // same pure function the elision marker and the artifact index use.
    let outputs = munshi::extract_outputs(&transcript, munshi::SourceKind::Copilot, 64);
    assert_eq!(outputs.len(), 1, "exactly one event exceeds the threshold");
    let ticket = outputs[0].sha256.clone();
    let content = outputs[0].content.clone();
    (state, home, ticket, content)
}

#[test]
fn local_redeems_a_ticket_from_the_transcript_without_a_server() {
    let (state, _home, ticket, content) = seed_local_session("sess-local");

    // Bare session ID, no endpoint, no network: stdout reproduces the elided bytes exactly.
    let output = run_raw(
        state.path(),
        &[&ticket, "--local", "--session", "sess-local"],
    );
    assert!(output.status.success(), "stderr: {}", output.stderr());
    assert_eq!(output.stdout, content, "byte-for-byte local redemption");

    // The prefixed identity summarizer input carries resolves identically.
    let prefixed = run_raw(
        state.path(),
        &[&ticket, "--local", "--session", "copilot:sess-local"],
    );
    assert!(prefixed.status.success(), "stderr: {}", prefixed.stderr());
    assert_eq!(prefixed.stdout, content);

    // Client-side --query works over locally redeemed content too.
    let query = run_raw(
        state.path(),
        &[&ticket, "--local", "--session", "sess-local", "--query", "xxx"],
    );
    assert!(query.status.success(), "stderr: {}", query.stderr());
    assert!(String::from_utf8_lossy(&query.stdout).contains("xxx"));
}

#[test]
fn local_misses_exit_distinctly_without_emitting_bytes() {
    let (state, _home, ticket, _content) = seed_local_session("sess-local");

    // A hash no event carries: exit 4 (no matching artifact), nothing on stdout.
    let absent = "0".repeat(64);
    let miss = run_raw(state.path(), &[&absent, "--local", "--session", "sess-local"]);
    assert_eq!(miss.status.code(), Some(4), "stderr: {}", miss.stderr());
    assert!(miss.stdout.is_empty());

    // An unknown session: exit 4 as well — a script-visible miss, not CLI misuse.
    let unknown = run_raw(state.path(), &[&ticket, "--local", "--session", "sess-other"]);
    assert_eq!(unknown.status.code(), Some(4), "stderr: {}", unknown.stderr());
    assert!(unknown.stdout.is_empty());

    // A session prefix contradicting --source is CLI misuse and fails hard.
    let contradicted = run_raw(
        state.path(),
        &[
            &ticket,
            "--local",
            "--session",
            "copilot:sess-local",
            "--source",
            "claude-code",
        ],
    );
    assert!(!contradicted.status.success());
    assert!(contradicted.stdout.is_empty());
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

/// Runs `munshi retrieve <args> --endpoint <server> --state-dir <fresh>` against a fake server.
fn run(server: &FakePatwari, args: &[&str]) -> RunOutput {
    let state = TempDir::new().unwrap();
    let endpoint = server.endpoint();
    let mut full: Vec<&str> = args.to_vec();
    full.push("--endpoint");
    full.push(&endpoint);
    run_raw(state.path(), &full)
}

fn run_raw(state_dir: &Path, args: &[&str]) -> RunOutput {
    let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
    command.arg("retrieve");
    command.args(args);
    command.arg("--state-dir").arg(state_dir);
    let output = command.output().expect("run munshi retrieve");
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
// Fake Patwari retrieval daemon
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StoredArtifact {
    artifact_id: String,
    snapshot_id: String,
    logical_path: String,
    media_type: Option<String>,
    compression: String,
    original_size_bytes: u64,
    stored_sha256: String,
    created_at: String,
    /// The original hash advertised in the listing (defaults to the true original hash).
    listing_original_sha256: String,
    /// The bytes the content route actually serves (may be tampered by a test).
    served_stored_bytes: Vec<u8>,
    /// The stored/original hashes the content headers declare (consistent with `served_stored_bytes`
    /// unless a test deliberately corrupts them).
    header_stored_sha256: String,
    header_original_sha256: String,
    header_original_size: u64,
}

impl StoredArtifact {
    fn compressed(
        artifact_id: &str,
        snapshot_id: &str,
        logical_path: &str,
        original: &[u8],
        created_at: &str,
    ) -> Self {
        let stored = zstd::encode_all(original, 3).expect("zstd compress");
        Self::build(
            artifact_id,
            snapshot_id,
            logical_path,
            original,
            &stored,
            "zstd",
            created_at,
        )
    }

    fn identity(
        artifact_id: &str,
        snapshot_id: &str,
        logical_path: &str,
        original: &[u8],
        created_at: &str,
    ) -> Self {
        Self::build(
            artifact_id,
            snapshot_id,
            logical_path,
            original,
            original,
            "identity",
            created_at,
        )
    }

    fn build(
        artifact_id: &str,
        snapshot_id: &str,
        logical_path: &str,
        original: &[u8],
        stored: &[u8],
        compression: &str,
        created_at: &str,
    ) -> Self {
        let original_sha256 = sha256_hex(original);
        let stored_sha256 = sha256_hex(stored);
        Self {
            artifact_id: artifact_id.to_owned(),
            snapshot_id: snapshot_id.to_owned(),
            logical_path: logical_path.to_owned(),
            media_type: Some("text/plain".to_owned()),
            compression: compression.to_owned(),
            original_size_bytes: original.len() as u64,
            stored_sha256: stored_sha256.clone(),
            created_at: created_at.to_owned(),
            listing_original_sha256: original_sha256.clone(),
            served_stored_bytes: stored.to_vec(),
            header_stored_sha256: stored_sha256,
            header_original_sha256: original_sha256,
            header_original_size: original.len() as u64,
        }
    }
}

struct FakeState {
    artifacts: Vec<StoredArtifact>,
    last_content_artifact: Option<String>,
}

struct FakePatwari {
    port: u16,
    state: Arc<Mutex<FakeState>>,
}

impl FakePatwari {
    fn start(artifacts: Vec<StoredArtifact>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(FakeState {
            artifacts,
            last_content_artifact: None,
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

    fn last_content_artifact(&self) -> Option<String> {
        self.state.lock().unwrap().last_content_artifact.clone()
    }
}

fn handle_connection(mut stream: TcpStream, state: &Arc<Mutex<FakeState>>) {
    let Some((method, target)) = read_request(&mut stream) else {
        return;
    };
    let mut guard = state.lock().unwrap();
    let response = route(&method, &target, &mut guard);
    drop(guard);
    let _ = stream.write_all(&response);
}

fn route(method: &str, target: &str, state: &mut FakeState) -> Vec<u8> {
    let path = target.split('?').next().unwrap_or(target);
    if method == "GET" && path == "/api/v1/artifacts" {
        return list_artifacts(target, state);
    }
    if method == "GET" && path.starts_with("/api/v1/artifacts/") && path.ends_with("/content") {
        let artifact_id = path
            .trim_start_matches("/api/v1/artifacts/")
            .trim_end_matches("/content");
        return content(artifact_id, state);
    }
    json_response(
        404,
        &json!({ "error": { "code": "not_found", "message": "unhandled" } }),
    )
}

fn list_artifacts(target: &str, state: &mut FakeState) -> Vec<u8> {
    let query = target.split('?').nth(1).unwrap_or("");
    let requested = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("original_sha256="))
        .map(percent_decode)
        .unwrap_or_default();
    let items: Vec<Value> = state
        .artifacts
        .iter()
        .enumerate()
        .filter(|(_, artifact)| artifact.listing_original_sha256 == requested)
        .map(|(index, artifact)| {
            json!({
                "artifact_id": artifact.artifact_id,
                "snapshot_id": artifact.snapshot_id,
                "artifact_index": index,
                "logical_path": artifact.logical_path,
                "media_type": artifact.media_type,
                "original_size_bytes": artifact.original_size_bytes,
                "original_sha256": format!("sha256:{}", artifact.listing_original_sha256),
                "stored_size_bytes": artifact.served_stored_bytes.len(),
                "stored_sha256": format!("sha256:{}", artifact.stored_sha256),
                "compression": artifact.compression,
                "created_at": artifact.created_at,
                "metadata_url": format!("/api/v1/artifacts/{}", artifact.artifact_id),
                "content_url": format!("/api/v1/artifacts/{}/content", artifact.artifact_id),
            })
        })
        .collect();
    json_response(
        200,
        &json!({ "items": items, "next_cursor": Value::Null, "high_watermark": Value::Null }),
    )
}

fn content(artifact_id: &str, state: &mut FakeState) -> Vec<u8> {
    let Some(artifact) = state
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_id == artifact_id)
        .cloned()
    else {
        return json_response(
            404,
            &json!({ "error": { "code": "artifact_not_found", "message": "x" } }),
        );
    };
    state.last_content_artifact = Some(artifact_id.to_owned());

    let body = artifact.served_stored_bytes;
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
        artifact.header_original_size
    ));
    head.push_str(&format!(
        "x-patwari-original-sha256: sha256:{}\r\n",
        artifact.header_original_sha256
    ));
    head.push_str(&format!("x-patwari-stored-size-bytes: {}\r\n", body.len()));
    head.push_str(&format!(
        "x-patwari-stored-sha256: sha256:{}\r\n",
        artifact.header_stored_sha256
    ));
    if let Some(media_type) = &artifact.media_type {
        head.push_str(&format!("x-patwari-media-type: {media_type}\r\n"));
    }
    head.push_str("\r\n");
    let mut response = head.into_bytes();
    response.extend_from_slice(&body);
    response
}

fn json_response(status: u16, value: &Value) -> Vec<u8> {
    let body = value.to_string();
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        422 => "Unprocessable Entity",
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
