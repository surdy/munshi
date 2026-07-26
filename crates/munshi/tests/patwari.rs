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
use std::path::Path;
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
    // Set v1 with one extraction: summary.md, transcript.jsonl, then one outputs/<sha256>.
    let paths: Vec<&str> = sources.iter().map(|s| s.logical_path.as_str()).collect();
    assert_eq!(sources.len(), 3);
    assert_eq!(paths[0], "summary.md");
    assert_eq!(paths[1], "transcript.jsonl");
    let stem = paths[2]
        .strip_prefix("outputs/")
        .expect("extracted output uses the outputs/ role path")
        .to_owned();
    // The logical path is the bare lowercase hex sha256 of the extracted content itself.
    assert_eq!(sha256_hex(&sources[2].bytes), stem);

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
        vec!["summary.md", "transcript.jsonl", &format!("outputs/{stem}")]
    );
    assert_eq!(
        manifest["artifacts"][2]["original_sha256"]
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

    // The assembled set keeps the fixed roles first, then the same hash-sorted outputs.
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
            "summary.md".to_owned(),
            "transcript.jsonl".to_owned(),
            format!("outputs/{}", outputs[0].sha256),
            format!("outputs/{}", outputs[1].sha256),
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
            },
        )
        .unwrap();
    let after_success = store
        .prepare_archive_capture(session, endpoint, 2, "capture-C", "2026-07-25T02:00:00Z")
        .unwrap();
    assert_eq!(after_success.capture_id, "capture-C");
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
    session_id: String,
    /// Accepted chunk indexes per artifact index.
    artifacts: Vec<AcceptedArtifact>,
    completed: bool,
}

struct AcceptedArtifact {
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

    let artifacts = manifest["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|artifact| {
            let stored = artifact["stored_size_bytes"].as_u64().unwrap_or(0);
            AcceptedArtifact {
                chunk_count: stored.div_ceil(CHUNK_SIZE),
                accepted: Vec::new(),
            }
        })
        .collect();
    let index = state.uploads.len();
    state.uploads.push(Upload {
        capture_id: capture_id.clone(),
        manifest_sig,
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
    let already = state.uploads[index]
        .artifacts
        .get(artifact_index as usize)
        .map(|artifact| artifact.accepted.contains(&chunk_index))
        .unwrap_or(false);
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
