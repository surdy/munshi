//! Integration tests for opt-in harness-memory mirroring (issue #59): configuration with a
//! canonical machine label, verbatim mirroring with manifest + correlated history commit,
//! hash-compare idempotence, the history-required block, the clean-tree preflight, and the
//! bounded-retry dead letter with `--force` revival.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};
use tempfile::TempDir;

const SLUG: &str = "-Users-test-repos-demo";

// ---------------------------------------------------------------------------
// Fake Notesmith
// ---------------------------------------------------------------------------

struct FakeCommit {
    message: String,
    sha: String,
    files_changed: usize,
}

struct FakeState {
    notes: HashMap<String, String>,
    requests: usize,
    git_enabled: bool,
    commits: Vec<FakeCommit>,
    /// Paths written since the last commit — the dirty working tree.
    dirty: Vec<String>,
    /// Injected unrelated dirty paths (files Munshi does not own).
    extra_dirty: Vec<String>,
}

struct FakeNotesmith {
    port: u16,
    state: Arc<Mutex<FakeState>>,
    outage: Arc<AtomicBool>,
}

impl FakeNotesmith {
    fn start(git_enabled: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(FakeState {
            notes: HashMap::new(),
            requests: 0,
            git_enabled,
            commits: Vec::new(),
            dirty: Vec::new(),
            extra_dirty: Vec::new(),
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

    fn set_git_enabled(&self, enabled: bool) {
        self.state.lock().unwrap().git_enabled = enabled;
    }

    fn add_unrelated_dirty(&self, path: &str) {
        self.state.lock().unwrap().extra_dirty.push(path.to_owned());
    }

    fn clear_unrelated_dirty(&self) {
        self.state.lock().unwrap().extra_dirty.clear();
    }

    fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests
    }

    fn note_body(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().notes.get(path).cloned()
    }

    fn note_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.state.lock().unwrap().notes.keys().cloned().collect();
        paths.sort();
        paths
    }

    fn commits(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .commits
            .iter()
            .map(|commit| commit.message.clone())
            .collect()
    }
}

struct FakeRequest {
    method: String,
    target: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<FakeRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .map(str::trim)
            .and_then(|value| value.parse::<usize>().ok())
        {
            content_length = value;
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(FakeRequest {
        method,
        target,
        body,
    })
}

fn respond(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn handle_connection(
    mut stream: TcpStream,
    state: &Arc<Mutex<FakeState>>,
    outage: &Arc<AtomicBool>,
) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    if outage.load(Ordering::SeqCst) {
        let _ = stream.write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    let mut guard = state.lock().unwrap();
    guard.requests += 1;
    let response = route(&request, &mut guard);
    drop(guard);
    let _ = stream.write_all(response.as_bytes());
}

fn route(request: &FakeRequest, state: &mut FakeState) -> String {
    let target = request.target.replace("%2F", "/");
    let path = target.split('?').next().unwrap_or_default();
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    // All routes are /api/v/<vault>/...
    if segments.len() < 4 || segments[0] != "api" || segments[1] != "v" {
        return respond("404 Not Found", "{}");
    }
    match (request.method.as_str(), segments[3]) {
        ("PUT", "notes") => {
            let note_path = segments[4..].join("/");
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let Some(content) = body.get("content").and_then(Value::as_str) else {
                return respond("400 Bad Request", "{}");
            };
            state.notes.insert(note_path.clone(), content.to_owned());
            if !state.dirty.contains(&note_path) {
                state.dirty.push(note_path.clone());
            }
            respond(
                "200 OK",
                &json!({ "path": note_path, "hash": "fakehash" }).to_string(),
            )
        }
        ("GET", "config") => respond(
            "200 OK",
            &json!({
                "config": { "git": { "enabled": state.git_enabled } },
                "hash": "confighash",
            })
            .to_string(),
        ),
        ("GET", "git") if segments.get(4) == Some(&"status") => {
            if !state.git_enabled {
                return respond("400 Bad Request", "{}");
            }
            let mut changed = state.dirty.clone();
            changed.extend(state.extra_dirty.iter().cloned());
            respond(
                "200 OK",
                &json!({
                    "clean": changed.is_empty(),
                    "changed": changed,
                    "staged": [],
                    "untracked": [],
                })
                .to_string(),
            )
        }
        ("POST", "git") if segments.get(4) == Some(&"commit") => {
            if !state.git_enabled {
                return respond("400 Bad Request", "{}");
            }
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let message = body
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let files_changed = state.dirty.len() + state.extra_dirty.len();
            if files_changed == 0 {
                return respond("200 OK", &json!({ "committed": false }).to_string());
            }
            let sha = format!("sha{:04}", state.commits.len() + 1);
            state.commits.push(FakeCommit {
                message,
                sha: sha.clone(),
                files_changed,
            });
            state.dirty.clear();
            state.extra_dirty.clear();
            respond(
                "200 OK",
                &json!({ "committed": true, "sha": sha }).to_string(),
            )
        }
        ("GET", "git") if segments.get(4) == Some(&"log") => {
            if !state.git_enabled {
                return respond("400 Bad Request", "{}");
            }
            let entries: Vec<Value> = state
                .commits
                .iter()
                .rev()
                .map(|commit| {
                    json!({
                        "subject": commit.message,
                        "sha": commit.sha,
                        "filesChanged": commit.files_changed,
                    })
                })
                .collect();
            respond("200 OK", &Value::Array(entries).to_string())
        }
        _ => respond("404 Not Found", "{}"),
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    #[allow(dead_code)]
    directory: TempDir,
    copilot_home: PathBuf,
    claude_home: PathBuf,
    state: PathBuf,
    output: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/munshi-memory-sync-test-artifacts");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("memory-sync-case-")
            .tempdir_in(root)
            .unwrap();
        let copilot_home = directory.path().join("copilot-home");
        let claude_home = directory.path().join("claude-home");
        std::fs::create_dir_all(&copilot_home).unwrap();
        std::fs::create_dir_all(&claude_home).unwrap();
        Self {
            state: directory.path().join("munshi-home"),
            output: directory.path().join("archives"),
            copilot_home,
            claude_home,
            directory,
        }
    }

    fn register(&self) {
        let output = Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("register")
            .arg("--accept-transcript-processing")
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .arg("--claude-home")
            .arg(&self.claude_home)
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--output-dir")
            .arg(&self.output)
            .arg("--summarizer")
            .arg(fake_summarizer())
            .arg("--timeout-ms")
            .arg("5000")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn memory_dir(&self) -> PathBuf {
        self.claude_home.join("projects").join(SLUG).join("memory")
    }

    fn write_memory_file(&self, relative: &str, content: &str) {
        let path = self.memory_dir().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn memory_sync(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_munshi"))
            .arg("memory-sync")
            .args(args)
            .arg("--state-dir")
            .arg(&self.state)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn configure(&self, endpoint: &str, machine: Option<&str>, max_attempts: Option<&str>) {
        let mut args = vec![
            "configure",
            "--endpoint",
            endpoint,
            "--vault",
            "memvault",
            "--folder",
            "memory",
        ];
        if let Some(machine) = machine {
            args.extend_from_slice(&["--machine", machine]);
        }
        if let Some(max_attempts) = max_attempts {
            args.extend_from_slice(&["--max-attempts", max_attempts]);
        }
        assert_success(&self.memory_sync(&args));
        assert_success(&self.memory_sync(&["enable"]));
    }

    fn run_json(&self, force: bool) -> (Output, Option<Value>) {
        let mut args = vec!["run", "--json"];
        if force {
            args.push("--force");
        }
        let output = self.memory_sync(&args);
        let report = serde_json::from_slice(&output.stdout).ok();
        (output, report)
    }

    fn status_json(&self) -> Value {
        let output = self.memory_sync(&["status", "--json"]);
        assert_success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn fake_summarizer() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/manual/fake-summarizer")
        .join("success.sh")
        .canonicalize()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The machine label is chosen once at configure time — explicit flag wins, sanitized to a
/// routing-safe slug — and syncing is refused before configuration (opt-in discipline).
#[test]
fn configure_persists_one_canonical_sanitized_machine_label() {
    let harness = Harness::new();
    harness.register();

    // Enabling before configuring is refused (the sink is not addressable).
    let premature = harness.memory_sync(&["enable"]);
    assert!(!premature.status.success());

    harness.configure("http://127.0.0.1:19", Some("Test's MacBook Pro"), None);
    let status = harness.status_json();
    assert_eq!(status["settings"]["enabled"], true);
    assert_eq!(status["settings"]["machine_label"], "test-s-macbook-pro");
    assert_eq!(status["settings"]["vault"], "memvault");
    // No archive upload configured on this harness: no machine id is invented.
    assert_eq!(status["settings"]["machine_id"], Value::Null);
}

/// The full mirror loop: files land verbatim under `<folder>/<machine>/<slug>/`, the sibling
/// manifest note carries identity and the file table, the revision is preserved as a correlated
/// history commit, and an unchanged tree is a no-op that never contacts the sink.
#[test]
fn run_mirrors_files_with_manifest_and_correlated_commit_and_is_idempotent() {
    let harness = Harness::new();
    harness.register();
    harness.write_memory_file("MEMORY.md", "- [Fact](fact.md) — hook\n");
    harness.write_memory_file(
        "fact.md",
        "---\nname: fact\n---\n\nThe project uses Rust.\n",
    );
    let sink = FakeNotesmith::start(true);
    harness.configure(&sink.endpoint(), Some("testbox"), None);

    let (output, report) = harness.run_json(false);
    assert_success(&output);
    let report = report.unwrap();
    assert_eq!(report["synced"], 1);
    assert_eq!(report["failed"], 0);
    assert_eq!(
        sink.note_paths(),
        vec![
            format!("memory/testbox/{SLUG}.manifest.md"),
            format!("memory/testbox/{SLUG}/MEMORY.md"),
            format!("memory/testbox/{SLUG}/fact.md"),
        ]
    );
    // Mirrored file content is verbatim — the file's own frontmatter included, nothing injected.
    assert_eq!(
        sink.note_body(&format!("memory/testbox/{SLUG}/fact.md")),
        Some("---\nname: fact\n---\n\nThe project uses Rust.\n".to_owned())
    );
    let manifest = sink
        .note_body(&format!("memory/testbox/{SLUG}.manifest.md"))
        .unwrap();
    assert!(manifest.contains("munshi_machine: testbox"));
    assert!(manifest.contains("munshi_revision: 1"));
    assert!(manifest.contains("| MEMORY.md |"));
    assert_eq!(
        sink.commits(),
        vec![format!("munshi memory testbox:{SLUG} revision 1")]
    );

    // Unchanged tree: the second run is a hash-compare no-op with zero sink contact.
    let before = sink.request_count();
    let (output, report) = harness.run_json(false);
    assert_success(&output);
    let report = report.unwrap();
    assert_eq!(report["synced"], 0);
    assert_eq!(report["unchanged"], 1);
    assert_eq!(sink.request_count(), before);

    // A content change syncs revision 2 with its own correlated commit.
    harness.write_memory_file("fact.md", "---\nname: fact\n---\n\nNow with more facts.\n");
    let (output, report) = harness.run_json(false);
    assert_success(&output);
    assert_eq!(report.unwrap()["synced"], 1);
    assert_eq!(sink.commits().len(), 2);
    assert_eq!(
        sink.commits()[1],
        format!("munshi memory testbox:{SLUG} revision 2")
    );
    let status = harness.status_json();
    assert_eq!(status["items"][0]["synced_revision"], 2);
    assert_eq!(status["items"][0]["state"], "synced");
}

/// History is half the feature: a vault that cannot preserve correlated history blocks the sync
/// (attempt-neutral, actionable) instead of degrading to latest-only; enabling the capability
/// unblocks the next run.
#[test]
fn missing_history_capability_blocks_instead_of_degrading() {
    let harness = Harness::new();
    harness.register();
    harness.write_memory_file("MEMORY.md", "remember this\n");
    let sink = FakeNotesmith::start(false);
    harness.configure(&sink.endpoint(), Some("testbox"), None);

    let (output, report) = harness.run_json(false);
    assert!(!output.status.success());
    let report = report.unwrap();
    assert_eq!(report["blocked"], 1);
    assert_eq!(report["synced"], 0);
    // Blocked never wrote a note and never committed.
    assert!(sink.note_paths().is_empty());
    let status = harness.status_json();
    assert_eq!(status["items"][0]["state"], "blocked");
    assert_eq!(
        status["items"][0]["last_error_category"],
        "remote-history-unavailable"
    );

    sink.set_git_enabled(true);
    let (output, report) = harness.run_json(false);
    assert_success(&output);
    assert_eq!(report.unwrap()["synced"], 1);
    assert_eq!(sink.commits().len(), 1);
}

/// Notesmith commits stage the whole tree, so an unrelated dirty file refuses the correlated
/// commit rather than bundling a stranger's changes into it.
#[test]
fn unrelated_dirty_tree_refuses_the_commit() {
    let harness = Harness::new();
    harness.register();
    harness.write_memory_file("MEMORY.md", "remember this\n");
    let sink = FakeNotesmith::start(true);
    sink.add_unrelated_dirty("journal/today.md");
    harness.configure(&sink.endpoint(), Some("testbox"), None);

    let (output, report) = harness.run_json(false);
    assert!(!output.status.success());
    let report = report.unwrap();
    assert_eq!(report["failed"], 1);
    assert_eq!(report["items"][0]["code"], "remote-history-dirty");

    // The operator resolves the dirty tree; a forced run (bypassing backoff) syncs.
    sink.clear_unrelated_dirty();
    let (output, report) = harness.run_json(true);
    assert_success(&output);
    assert_eq!(report.unwrap()["synced"], 1);
}

/// A persistent outage burns its bounded attempts into a dead letter that ordinary runs leave
/// parked; `--force` revives it once the sink is healthy again.
#[test]
fn outage_parks_as_dead_letter_and_force_revives() {
    let harness = Harness::new();
    harness.register();
    harness.write_memory_file("MEMORY.md", "remember this\n");
    let sink = FakeNotesmith::start(true);
    harness.configure(&sink.endpoint(), Some("testbox"), Some("1"));

    sink.set_outage(true);
    let (output, report) = harness.run_json(false);
    assert!(!output.status.success());
    assert_eq!(report.unwrap()["failed"], 1);
    let status = harness.status_json();
    assert_eq!(status["items"][0]["state"], "dead-letter");

    // A plain run leaves the dead letter parked without contacting the sink.
    sink.set_outage(false);
    let before = sink.request_count();
    let (output, report) = harness.run_json(false);
    assert_success(&output);
    assert_eq!(report.unwrap()["synced"], 0);
    assert_eq!(sink.request_count(), before);

    let (output, report) = harness.run_json(true);
    assert_success(&output);
    assert_eq!(report.unwrap()["synced"], 1);
    let status = harness.status_json();
    assert_eq!(status["items"][0]["state"], "synced");
    assert_eq!(status["items"][0]["attempts"], 0);
}
