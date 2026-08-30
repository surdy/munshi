//! The Munshi desktop addon: a native window over the same versioned CLI/JSON boundary the
//! backlog dashboard reads (ADR 0007, ADR 0014).
//!
//! There is no HTTP server here and no port to bind. `munshi-dashboard` had to publish
//! unauthenticated session metadata on loopback and defend that with a bind-address check; this
//! app passes the same payload straight from a Rust command to its own webview over Tauri's IPC,
//! so session titles and project names never leave the process. Everything else is unchanged:
//! contracts in, panels out, and no access to the state directory.

mod cli;
mod resolve;

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};
use tauri::{Manager, State};

use crate::cli::CommandError;
use crate::resolve::CliInfo;

/// Where the CLI sits inside the app bundle. Declared as a bundle resource in `tauri.conf.json`.
const BUNDLED_CLI_RESOURCE: &str = "resources/bin/munshi";

/// Resolved once at startup: the path of the CLI shipped inside this bundle, if there is one.
/// Absent under `tauri dev`, where the app runs from a target directory with no bundle around it.
struct AppState {
    bundled_cli: Option<PathBuf>,
}

/// One collection round: every read-only contract the UI draws from, plus whatever failed.
///
/// Sections are `null` when their invocation failed, and the failure is described in `errors`. A
/// partial snapshot is always better than no snapshot — one drifted or slow contract must degrade
/// its own panel, not blank the window.
#[derive(Debug, Serialize)]
struct Snapshot {
    /// Milliseconds since the epoch, so the page can show the age of what it is displaying.
    generated_at_ms: i64,
    /// Which binary this round was read from, and whether it needs installing or updating.
    cli: CliInfo,
    errors: Vec<CommandError>,
    status: Option<Value>,
    sessions: Option<Value>,
    attempts: Option<Value>,
    diagnostics: Option<Value>,
    uploads: Option<Value>,
    deliveries: Option<Value>,
}

/// Resolves the CLI or explains why it could not be found.
fn require_cli(state: &AppState) -> Result<PathBuf, String> {
    resolve::resolve(state.bundled_cli.as_deref())
        .map(|(path, _)| path)
        .ok_or_else(|| {
            "no munshi executable found. Install the bundled command, or put `munshi` on your PATH."
                .to_string()
        })
}

/// Drops the per-session `items` array from a sink status contract.
///
/// Both sink commands return one row per session — on a mature archive that is thousands of rows
/// and megabytes of JSON, none of which the window draws: the panels use the top-level counts
/// only. Stripping it here keeps each round's IPC payload proportional to what is displayed. The
/// backlog dashboard does the same thing for the same reason.
fn strip_items(mut report: Value) -> Value {
    if let Some(object) = report.as_object_mut() {
        object.remove("items");
    }
    report
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// Collects one snapshot: six invocations, each degrading independently.
///
/// The listing limits match the dashboard's so both surfaces show the same depth of history.
#[tauri::command]
async fn snapshot(state: State<'_, AppState>) -> Result<Snapshot, String> {
    let bundled = state.bundled_cli.clone();
    // The invocations are blocking and can take seconds; running them on the UI thread would
    // freeze the window while a large listing is collected.
    tauri::async_runtime::spawn_blocking(move || {
        let info = resolve::info(bundled.as_deref());
        let Some((program, _)) = resolve::resolve(bundled.as_deref()) else {
            return Snapshot {
                generated_at_ms: now_ms(),
                cli: info,
                errors: Vec::new(),
                status: None,
                sessions: None,
                attempts: None,
                diagnostics: None,
                uploads: None,
                deliveries: None,
            };
        };

        let mut errors = Vec::new();
        let mut collect = |section: &str, args: &[&str]| match cli::run_json(&program, section, args)
        {
            Ok(value) => Some(value),
            Err(error) => {
                errors.push(error);
                None
            }
        };

        let status = collect("status", &["status", "--json"]);
        let sessions = collect("sessions", &["sessions", "--json", "--limit", "1000"]);
        let attempts = collect("attempts", &["attempts", "--json", "--limit", "200"]);
        let diagnostics = collect("diagnostics", &["diagnostics", "--json", "--limit", "20"]);
        let uploads = collect("uploads", &["archive-upload", "status", "--json"])
            .map(strip_items);
        let deliveries = collect("deliveries", &["summary-delivery", "status", "--json"])
            .map(strip_items);

        Snapshot {
            generated_at_ms: now_ms(),
            cli: info,
            errors,
            status,
            sessions,
            attempts,
            diagnostics,
            uploads,
            deliveries,
        }
    })
    .await
    .map_err(|error| format!("snapshot collection panicked: {error}"))
}

/// `munshi show <session> --source <source> --json` — one session and its current summary.
#[tauri::command]
async fn show_session(
    state: State<'_, AppState>,
    source: String,
    session_id: String,
) -> Result<Value, String> {
    let program = require_cli(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        cli::run_json(&program, "show", &["show", &session_id, "--source", &source, "--json"])
            .map_err(|error| error.message)
    })
    .await
    .map_err(|error| format!("show panicked: {error}"))?
}

/// `munshi retry <session> --json` — the "summarize now" and "retry" actions ADR 0007 blesses.
///
/// Munshi decides what retrying means for the session's current state; the GUI only chooses the
/// label. `force` bypasses the backoff schedule for a session that is parked.
#[tauri::command]
async fn retry_session(
    state: State<'_, AppState>,
    source: String,
    session_id: String,
    force: bool,
) -> Result<Value, String> {
    let program = require_cli(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut args = vec!["retry", session_id.as_str(), "--source", source.as_str(), "--json"];
        if force {
            args.push("--force");
        }
        cli::run_json(&program, "retry", &args).map_err(|error| error.message)
    })
    .await
    .map_err(|error| format!("retry panicked: {error}"))?
}

/// `munshi retry-all --json` — drain every eligible session.
#[tauri::command]
async fn retry_all(state: State<'_, AppState>, force: bool) -> Result<Value, String> {
    let program = require_cli(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut args = vec!["retry-all", "--json"];
        if force {
            args.push("--force");
        }
        cli::run_json(&program, "retry-all", &args).map_err(|error| error.message)
    })
    .await
    .map_err(|error| format!("retry-all panicked: {error}"))?
}

/// `munshi tick --json` — the same idempotent sweep the launchd job runs, on demand.
#[tauri::command]
async fn run_tick(state: State<'_, AppState>) -> Result<Value, String> {
    let program = require_cli(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        cli::run_json(&program, "tick", &["tick", "--json"]).map_err(|error| error.message)
    })
    .await
    .map_err(|error| format!("tick panicked: {error}"))?
}

/// `munshi doctor --json` — registration and dependency diagnosis for the setup panel.
#[tauri::command]
async fn doctor(state: State<'_, AppState>) -> Result<Value, String> {
    let program = require_cli(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        cli::run_json(&program, "doctor", &["doctor", "--json"]).map_err(|error| error.message)
    })
    .await
    .map_err(|error| format!("doctor panicked: {error}"))?
}

/// The CLI picture on its own, for refreshing the setup panel without a full collection round.
#[tauri::command]
async fn cli_info(state: State<'_, AppState>) -> Result<CliInfo, String> {
    let bundled = state.bundled_cli.clone();
    tauri::async_runtime::spawn_blocking(move || resolve::info(bundled.as_deref()))
        .await
        .map_err(|error| format!("cli_info panicked: {error}"))
}

/// Copies the bundled CLI to `~/.local/bin/munshi`.
#[tauri::command]
async fn install_cli(state: State<'_, AppState>) -> Result<String, String> {
    let bundled = state
        .bundled_cli
        .clone()
        .ok_or_else(|| "this build has no bundled munshi to install".to_string())?;
    tauri::async_runtime::spawn_blocking(move || resolve::install_cli(&bundled))
        .await
        .map_err(|error| format!("install panicked: {error}"))?
}

/// Reads an archive file's Markdown for the summary viewer.
///
/// The path comes from the `archive_path` field of the `sessions`/`show` contracts, i.e. from
/// Munshi itself rather than from the page, and it is read — never written. Munshi owns these
/// files and may atomically replace them; the GUI is a reader like any other.
#[tauri::command]
async fn read_archive(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = PathBuf::from(path);
        let metadata = std::fs::metadata(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        // Summaries are prose, a few KiB at most. The ceiling stops a mistaken path (a transcript,
        // a database) from being pulled into the webview wholesale.
        const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024;
        if metadata.len() > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "{} is {} bytes, larger than a summary should ever be",
                path.display(),
                metadata.len()
            ));
        }
        std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))
    })
    .await
    .map_err(|error| format!("read panicked: {error}"))?
}

/// Hands a path or URL to the desktop to open.
///
/// Opening is done here rather than through a general-purpose opener plugin so the set of things
/// the webview can launch stays closed: an existing local path, or one of the two URL schemes
/// Munshi actually emits. `notesmith://` is the deep link `show --json` computes for a delivered
/// note; `https://` covers the configured Patwari and Notesmith endpoints.
#[tauri::command]
async fn open_target(target: String) -> Result<(), String> {
    let allowed_scheme = target.starts_with("https://") || target.starts_with("notesmith://");
    if !allowed_scheme && !Path::new(&target).exists() {
        return Err(format!("refusing to open {target}: not an existing path or an allowed URL"));
    }
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "macos")]
        let mut command = std::process::Command::new("/usr/bin/open");
        #[cfg(not(target_os = "macos"))]
        let mut command = std::process::Command::new("xdg-open");

        command
            .arg(&target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("could not open {target}: {error}"))
    })
    .await
    .map_err(|error| format!("open panicked: {error}"))?
}

/// Everything the frontend needs to reproduce a failure in a terminal.
#[tauri::command]
fn about() -> Value {
    json!({
        "app_version": env!("CARGO_PKG_VERSION"),
        // The contract version this UI was written against (ADR 0007). Every read-only contract
        // is `schema_version: 1`, extended additively; the page warns if it ever sees another.
        "expected_schema_version": 1,
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // `resolve_resource` returns a path whether or not the file exists, so probe it: under
            // `tauri dev` there is no bundle and the app must fall back to an installed CLI.
            let bundled = app
                .path()
                .resolve(BUNDLED_CLI_RESOURCE, tauri::path::BaseDirectory::Resource)
                .ok()
                .filter(|path| path.exists());
            app.manage(AppState { bundled_cli: bundled });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            show_session,
            retry_session,
            retry_all,
            run_tick,
            doctor,
            cli_info,
            install_cli,
            read_archive,
            open_target,
            about,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Munshi desktop addon");
}
