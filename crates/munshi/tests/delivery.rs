//! Integration coverage for opt-in Notesmith delivery (issue #8).
//!
//! These tests drive the real `munshi` binary and its real minimal HTTP client against an
//! in-process fake Notesmith daemon, exercising create, replace, outage + retry, duplicate
//! prevention, backfill dry run/confirmation, and — crucially — that local archival is fully
//! independent of delivery success.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};
use tempfile::TempDir;

const VAULT: &str = "work";

#[test]
fn backfill_dry_run_reports_candidates_without_contacting_the_sink() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");

    harness.configure(&server.endpoint());
    harness.enable();

    let dry = harness.delivery_backfill_json(false);
    assert_eq!(dry["command"], "delivery-backfill");
    assert_eq!(dry["confirmed"], false);
    assert_eq!(dry["candidates"], 1);
    assert_eq!(dry["created"], 0);
    assert_eq!(
        server.request_count(),
        0,
        "a dry run must never contact the sink"
    );
}

#[test]
fn backfill_confirm_creates_then_replaces_the_same_note() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    let transcript = harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");

    harness.configure(&server.endpoint());
    harness.enable();

    let run = harness.delivery_backfill_json(true);
    assert_eq!(run["confirmed"], true);
    assert_eq!(run["candidates"], 1);
    assert_eq!(run["created"], 1);
    assert_eq!(
        server.note_count(),
        1,
        "first delivery creates exactly one note"
    );

    let status = harness.delivery_status_json();
    assert_eq!(status["delivered"], 1);
    let item = &status["items"][0];
    assert_eq!(item["state"], "delivered");
    assert_eq!(item["delivered_revision"], 1);
    let note_path = item["note_path"].as_str().unwrap().to_owned();
    let first_body = server.note_body(&note_path).expect("note stored");
    assert!(first_body.contains("Contract summary title"));

    // A newer summary revision replaces the persisted note in place (worker auto-delivers).
    harness.revise_session(SESSION_A, &transcript, "GOAL_TWO", "answer two");

    assert_eq!(
        server.note_count(),
        1,
        "a later revision replaces the same note rather than creating a second"
    );
    let show = harness.show_json(SESSION_A);
    assert_eq!(show["session"]["delivery"]["state"], "delivered");
    assert_eq!(show["session"]["delivery"]["delivered_revision"], 2);
    assert_eq!(
        show["session"]["delivery"]["note_path"],
        Value::String(note_path)
    );
}

#[test]
fn replace_overwrites_remote_edits() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    let transcript = harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();
    assert_eq!(harness.delivery_backfill_json(true)["created"], 1);

    let note_path = harness.delivery_status_json()["items"][0]["note_path"]
        .as_str()
        .unwrap()
        .to_owned();
    // Simulate a remote edit of the Munshi-owned note.
    server.set_note_body(&note_path, "---\ntitle: hand edited\n---\nlocal edit");

    harness.revise_session(SESSION_A, &transcript, "GOAL_TWO", "answer two");

    let body = server.note_body(&note_path).expect("note present");
    assert!(
        !body.contains("hand edited"),
        "Munshi owns delivered notes and overwrites remote edits"
    );
    assert!(body.contains("Contract summary title"));
}

#[test]
fn outage_never_rolls_back_local_archive_and_retry_recovers() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.configure(&server.endpoint());
    harness.enable();

    // Deliver during a total outage: the archive must still succeed locally.
    server.set_outage(true);
    let _ = harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");

    let show = harness.show_json(SESSION_A);
    assert_eq!(
        show["session"]["state"], "archived",
        "a delivery outage never changes the archived lifecycle"
    );
    let archive_path = show["session"]["archive_path"].as_str().unwrap();
    assert!(
        harness.output.join(archive_path).exists(),
        "the local archive file exists regardless of delivery"
    );
    let delivery = harness.delivery_status_json();
    assert_eq!(delivery["delivered"], 0);
    assert_eq!(delivery["failed"], 1);
    assert_eq!(server.note_count(), 0);

    // Recover the sink and retry: delivery now succeeds without re-summarizing.
    server.set_outage(false);
    let retry = harness.delivery_retry_all_json(false);
    assert_eq!(retry["created"], 1);
    assert_eq!(server.note_count(), 1);
    assert_eq!(harness.delivery_status_json()["delivered"], 1);
}

#[test]
fn repeated_delivery_of_the_same_revision_is_idempotent() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();

    assert_eq!(harness.delivery_backfill_json(true)["created"], 1);
    let after_first = server.request_count();

    // A second confirmed backfill of an unchanged revision creates nothing new.
    let again = harness.delivery_backfill_json(true);
    assert_eq!(again["candidates"], 0);
    assert_eq!(again["created"], 0);
    assert_eq!(server.note_count(), 1);
    assert_eq!(
        server.request_count(),
        after_first,
        "an already-delivered revision does not contact the sink again"
    );
}

#[test]
fn create_conflict_is_adopted_as_a_replace() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();

    // A note already exists at the deterministic path (as after an operational-state rebuild):
    // creation returns 409 and Munshi adopts the note via a replace instead of duplicating it.
    let component = harness.project_component();
    let path = format!("Munshi/{component}/copilot-{SESSION_A}.md");
    server.set_note_body(&path, "---\ntitle: preexisting\n---\nold");

    harness.configure_with_folder(&server.endpoint(), "Munshi");
    let run = harness.delivery_backfill_json(true);
    let delivered = run["created"].as_u64().unwrap() + run["replaced"].as_u64().unwrap();
    assert_eq!(delivered, 1);
    assert_eq!(
        server.note_count(),
        1,
        "the conflicting note is adopted, not duplicated"
    );
    let body = server.note_body(&path).unwrap();
    assert!(body.contains("Contract summary title"));
}

#[test]
fn disabled_project_stops_future_delivery_but_retains_history() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();
    assert_eq!(harness.delivery_backfill_json(true)["created"], 1);

    harness.project_disable();

    // A new revision under a disabled project is not delivered, but existing history remains.
    let status = harness.delivery_status_json();
    assert_eq!(
        status["delivered"], 1,
        "existing delivery history is retained"
    );

    let retry = harness.delivery_backfill_json(true);
    assert_eq!(
        retry["candidates"], 0,
        "a disabled project offers no delivery candidates"
    );
    assert_eq!(server.note_count(), 1);
}

// ---------------------------------------------------------------------------
// Fake Notesmith daemon
// ---------------------------------------------------------------------------

struct FakeState {
    notes: HashMap<String, String>,
    requests: usize,
}

struct FakeNotesmith {
    port: u16,
    state: Arc<Mutex<FakeState>>,
    outage: Arc<AtomicBool>,
}

impl FakeNotesmith {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(FakeState {
            notes: HashMap::new(),
            requests: 0,
        }));
        let outage = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_outage = Arc::clone(&outage);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                handle_connection(stream, &thread_state, &thread_outage);
            }
        });
        Self {
            port,
            state,
            outage,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn set_outage(&self, outage: bool) {
        self.outage.store(outage, Ordering::SeqCst);
    }

    fn note_count(&self) -> usize {
        self.state.lock().unwrap().notes.len()
    }

    fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests
    }

    fn note_body(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().notes.get(path).cloned()
    }

    fn set_note_body(&self, path: &str, body: &str) {
        self.state
            .lock()
            .unwrap()
            .notes
            .insert(path.to_owned(), body.to_owned());
    }
}

fn handle_connection(
    mut stream: TcpStream,
    state: &Arc<Mutex<FakeState>>,
    outage: &Arc<AtomicBool>,
) {
    let Some((method, target, body)) = read_request(&mut stream) else {
        return;
    };
    if outage.load(Ordering::SeqCst) {
        // Simulate an unavailable daemon.
        let _ = stream.write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    let mut guard = state.lock().unwrap();
    guard.requests += 1;
    let response = route(&method, &target, &body, &mut guard);
    drop(guard);
    let _ = stream.write_all(response.as_bytes());
}

fn route(method: &str, target: &str, body: &str, state: &mut FakeState) -> String {
    // target: /api/v/{vault}/notes  or  /api/v/{vault}/notes/{path...}
    let prefix = format!("/api/v/{VAULT}/notes");
    if method == "POST" && target == prefix {
        let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let folder = payload["folder"].as_str().unwrap_or("");
        let title = payload["title"].as_str().unwrap_or("note");
        let content = payload["content"].as_str().unwrap_or("").to_owned();
        let path = if folder.is_empty() {
            format!("{title}.md")
        } else {
            format!("{}/{title}.md", folder.trim_matches('/'))
        };
        if state.notes.contains_key(&path) {
            return json_response(409, &json!({ "error": "exists" }));
        }
        let hash = simple_hash(&content);
        state.notes.insert(path.clone(), content);
        return json_response(201, &json!({ "path": path, "hash": hash }));
    }
    if method == "PUT" && target.starts_with(&format!("{prefix}/")) {
        let path = decode(&target[prefix.len() + 1..]);
        let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let content = payload["content"].as_str().unwrap_or("").to_owned();
        if !state.notes.contains_key(&path) {
            return json_response(404, &json!({ "error": "not found" }));
        }
        let hash = simple_hash(&content);
        state.notes.insert(path.clone(), content);
        return json_response(200, &json!({ "path": path, "hash": hash }));
    }
    json_response(404, &json!({ "error": "unhandled" }))
}

fn json_response(status: u16, value: &Value) -> String {
    let body = value.to_string();
    let reason = match status {
        200 => "OK",
        201 => "Created",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Status",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
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
    let mut content_length = 0usize;
    for line in lines {
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
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
    Some((method, target, String::from_utf8_lossy(&body).into_owned()))
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

fn simple_hash(content: &str) -> String {
    let mut hash: u64 = 1469598103934665603;
    for byte in content.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Test harness (drives the real munshi binary)
// ---------------------------------------------------------------------------

const SESSION_A: &str = "11111111-1111-4111-8111-111111111111";

struct Harness {
    #[allow(dead_code)]
    directory: TempDir,
    copilot_home: PathBuf,
    state: PathBuf,
    output: PathBuf,
    project: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/munshi-delivery-test-artifacts");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("delivery-case-")
            .tempdir_in(root)
            .unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
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
        let copilot_home = directory.path().join("copilot-home");
        Self {
            state: copilot_home.join("munshi"),
            output: directory.path().join("archives"),
            copilot_home,
            project,
            directory,
        }
    }

    fn munshi(&self) -> Command {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
    }

    fn register(&self) {
        let output = self
            .munshi()
            .arg("register")
            .arg("--accept-transcript-processing")
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .arg("--output-dir")
            .arg(&self.output)
            .arg("--summarizer")
            .arg(fake("status-contract.sh"))
            .arg("--timeout-ms")
            .arg("5000")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn configure(&self, endpoint: &str) {
        self.configure_with_folder(endpoint, "Munshi");
    }

    fn configure_with_folder(&self, endpoint: &str, folder: &str) {
        let output = self
            .munshi()
            .args(["delivery", "configure", "--endpoint"])
            .arg(endpoint)
            .args(["--vault", VAULT, "--folder", folder])
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn enable(&self) {
        let output = self
            .munshi()
            .args(["delivery", "enable"])
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn project_disable(&self) {
        let output = self
            .munshi()
            .args(["project", "disable"])
            .arg(&self.project)
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn project_component(&self) -> String {
        let show = self.show_json(SESSION_A);
        show["session"]["project"]["component"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn delivery_status_json(&self) -> Value {
        self.json([
            "delivery",
            "status",
            "--state-dir",
            self.state_str(),
            "--json",
        ])
    }

    fn delivery_backfill_json(&self, confirm: bool) -> Value {
        let mut args = vec![
            "delivery".to_owned(),
            "backfill".to_owned(),
            "--state-dir".to_owned(),
            self.state.display().to_string(),
            "--json".to_owned(),
        ];
        if confirm {
            args.push("--confirm".to_owned());
        }
        self.json(args)
    }

    fn delivery_retry_all_json(&self, force: bool) -> Value {
        let mut args = vec![
            "delivery".to_owned(),
            "retry".to_owned(),
            "--all".to_owned(),
            "--state-dir".to_owned(),
            self.state.display().to_string(),
            "--json".to_owned(),
        ];
        if force {
            args.push("--force".to_owned());
        }
        self.json(args)
    }

    fn show_json(&self, session_id: &str) -> Value {
        self.json([
            "show",
            session_id,
            "--state-dir",
            self.state_str(),
            "--json",
        ])
    }

    fn state_str(&self) -> &str {
        self.state.to_str().unwrap()
    }

    fn json<I, S>(&self, args: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = self.munshi().args(args).output().unwrap();
        assert!(
            !output.stdout.is_empty(),
            "empty stdout; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("valid JSON")
    }

    /// Registers, writes a transcript, and drives one full archive lifecycle. Returns the
    /// transcript path so callers can append further turns.
    fn archive_session(&self, session_id: &str, request: &str, answer: &str) -> PathBuf {
        let transcript = self.write_transcript(session_id, request, answer);
        self.agent_stop(session_id, &transcript, 10_000);
        assert_success(&self.hook(
            "session-end",
            &json!({
                "sessionId": session_id,
                "timestamp": 10_001,
                "cwd": self.project,
                "reason": "complete",
            }),
        ));
        assert_success(&self.wait(session_id));
        transcript
    }

    fn agent_stop(&self, session_id: &str, transcript: &Path, timestamp: u64) {
        assert_success(&self.hook(
            "agent-stop",
            &json!({
                "sessionId": session_id,
                "timestamp": timestamp,
                "cwd": self.project,
                "transcriptPath": transcript,
                "stopReason": "end_turn",
            }),
        ));
    }

    fn session_end(&self, session_id: &str, timestamp: u64) {
        assert_success(&self.hook(
            "session-end",
            &json!({
                "sessionId": session_id,
                "timestamp": timestamp,
                "cwd": self.project,
                "reason": "complete",
            }),
        ));
    }

    /// Appends a turn and drives a second full archive lifecycle for an existing session.
    fn revise_session(&self, session_id: &str, transcript: &Path, request: &str, answer: &str) {
        self.append_turn(transcript, request, answer);
        self.agent_stop(session_id, transcript, 20_000);
        self.session_end(session_id, 20_001);
        assert_success(&self.wait(session_id));
    }

    fn hook(&self, event: &str, payload: &Value) -> Output {
        let mut child = self
            .munshi()
            .arg("hook")
            .arg(event)
            .env("COPILOT_HOME", &self.copilot_home)
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
        child.wait_with_output().unwrap()
    }

    fn wait(&self, session_id: &str) -> Output {
        self.munshi()
            .arg("hook")
            .arg("wait")
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--session-id")
            .arg(session_id)
            .arg("--timeout-ms")
            .arg("10000")
            .output()
            .unwrap()
    }

    fn write_transcript(&self, session_id: &str, request: &str, answer: &str) -> PathBuf {
        let path = self
            .copilot_home
            .join("session-state")
            .join(session_id)
            .join("events.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, transcript(session_id, request, answer)).unwrap();
        path.canonicalize().unwrap()
    }

    fn append_turn(&self, transcript: &Path, request: &str, answer: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(transcript)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "id": format!("{request}-user"),
                "timestamp": "2026-07-12T00:01:00.000Z",
                "parentId": "initial-assistant",
                "type": "user.message",
                "data": {"content": request},
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "id": format!("{request}-assistant"),
                "timestamp": "2026-07-12T00:01:01.000Z",
                "parentId": format!("{request}-user"),
                "type": "assistant.message",
                "data": {"content": answer, "messageId": format!("{request}-message")},
            })
        )
        .unwrap();
    }
}

fn transcript(session_id: &str, request: &str, answer: &str) -> String {
    [
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
            "data": {"content": request},
        }),
        json!({
            "id": "initial-assistant",
            "timestamp": "2026-07-12T00:00:02.000Z",
            "parentId": "initial-user",
            "type": "assistant.message",
            "data": {"content": answer, "messageId": "initial-message"},
        }),
    ]
    .into_iter()
    .map(|value| value.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

fn fake(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/manual/fake-summarizer")
        .join(name);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.canonicalize().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
