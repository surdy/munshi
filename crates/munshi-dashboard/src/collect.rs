//! Snapshot collection: the `/api/data` payload and the cache in front of it.
//!
//! Every figure comes from invoking the `munshi` binary with `--json`. The dashboard never opens
//! the state directory or its SQLite database (ADR 0007), so it cannot contend with a running
//! Munshi, cannot corrupt anything, and cannot drift from the CLI's published contracts.
//!
//! A command that is missing, exits non-zero, times out, or emits unparseable JSON contributes one
//! entry to the payload's top-level `errors` array and leaves its section `null` or absent. The
//! page renders every section's empty state and banners the errors, so a partial snapshot is
//! always preferable to no snapshot.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::db;

/// How long a collected snapshot is served before the next request re-collects it. The page polls
/// every 45 seconds, so one browser tab costs roughly one round of `munshi` invocations per poll.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Wall-clock bound on one `munshi` invocation. A command still running at the bound is killed and
/// reported as a degraded source rather than allowed to wedge the snapshot.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);

/// How often the wall-clock bound is checked while a command runs.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Bound on one captured stream. `sessions --json --limit 1000` is the largest expected output by
/// far and stays well under this; a stream that exceeds it is truncated and fails its JSON parse,
/// which the caller reports as a degraded source.
const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024;

/// Bound on one `errors` entry's message. The page concatenates every message into a single banner
/// line, and a clap usage error alone runs to several lines.
const MAX_ERROR_CHARS: usize = 400;

/// Serves snapshots of the archiving backlog, shelling out to one `munshi` binary.
pub(crate) struct Collector {
    munshi: PathBuf,
    cache: Mutex<Option<Cached>>,
}

/// A serialized snapshot and the instant it was collected.
struct Cached {
    at: Instant,
    body: Arc<Vec<u8>>,
}

/// The parsed output of one collection round. A field is `None` exactly when its command
/// contributed an entry to the round's `errors` array instead.
struct Sources {
    status: Option<Value>,
    uploads: Option<Value>,
    deliveries: Option<Value>,
    sessions: Option<Value>,
    attempts: Option<Value>,
    diagnostics: Option<Value>,
}

impl Collector {
    /// Binds the collector to a `munshi` executable. A bare name is resolved against `PATH` on
    /// every invocation, so upgrading the installed binary needs no dashboard restart.
    pub(crate) fn new(munshi: PathBuf) -> Self {
        Self {
            munshi,
            cache: Mutex::new(None),
        }
    }

    /// The serialized snapshot body, re-collected when the cached one is older than [`CACHE_TTL`].
    /// Collection runs under the cache lock, so concurrent requests share one round of `munshi`
    /// invocations instead of racing several; a panicked previous holder is recovered from rather
    /// than propagated, since the cache holds no invariant a panic could break.
    pub(crate) fn snapshot(&self) -> Arc<Vec<u8>> {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = cache.as_ref()
            && cached.at.elapsed() < CACHE_TTL
        {
            return Arc::clone(&cached.body);
        }
        let payload = self.collect(unix_millis());
        let body = Arc::new(
            serde_json::to_vec(&payload).expect("a snapshot of JSON values re-serializes"),
        );
        *cache = Some(Cached {
            at: Instant::now(),
            body: Arc::clone(&body),
        });
        body
    }

    /// Runs one round of `munshi` commands and assembles the payload. `now_ms` stamps the payload
    /// and anchors the outcome bins, so both agree with each other rather than with the clock at
    /// two different points of a slow round.
    fn collect(&self, now_ms: i64) -> Value {
        let mut errors = Vec::new();
        let sources = Sources {
            status: self.munshi_json(&["status", "--json"], "munshi status", &mut errors),
            uploads: self.munshi_json(
                &["archive-upload", "status", "--json"],
                "munshi archive-upload status",
                &mut errors,
            ),
            deliveries: self.munshi_json(
                &["summary-delivery", "status", "--json"],
                "munshi summary-delivery status",
                &mut errors,
            ),
            sessions: self.munshi_json(
                &["sessions", "--json", "--limit", "1000"],
                "munshi sessions",
                &mut errors,
            ),
            attempts: self.munshi_json(
                &["attempts", "--json", "--limit", "200"],
                "munshi attempts",
                &mut errors,
            ),
            diagnostics: self.munshi_json(
                &["diagnostics", "--json", "--limit", "5"],
                "munshi diagnostics",
                &mut errors,
            ),
        };
        payload(now_ms, errors, sources)
    }

    /// Runs one `munshi` subcommand and parses its stdout as JSON. Every failure mode — spawn,
    /// non-zero exit, timeout, malformed JSON — becomes one `errors` entry and `None`.
    fn munshi_json(&self, args: &[&str], label: &str, errors: &mut Vec<Value>) -> Option<Value> {
        let output = match run_bounded(&self.munshi, args, COMMAND_TIMEOUT) {
            Ok(output) => output,
            Err(error) => {
                errors.push(error_entry(label, &error.to_string()));
                return None;
            }
        };
        if !output.succeeded {
            let detail = if output.stderr.trim().is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            errors.push(error_entry(label, detail.trim()));
            return None;
        }
        match serde_json::from_str(&output.stdout) {
            Ok(value) => Some(value),
            Err(error) => {
                errors.push(error_entry(label, &error.to_string()));
                None
            }
        }
    }
}

/// Assembles the `/api/data` payload from one round's parsed sources. The key set and the shape of
/// every value reproduce the Python spike this crate replaces, so the page renders unchanged.
fn payload(now_ms: i64, errors: Vec<Value>, sources: Sources) -> Value {
    let mut payload = Map::new();
    payload.insert("generated_at_ms".to_owned(), json!(now_ms));
    payload.insert("errors".to_owned(), Value::Array(errors));
    payload.insert("status".to_owned(), sources.status.unwrap_or(Value::Null));
    payload.insert(
        "uploads".to_owned(),
        sources.uploads.map_or(Value::Null, without_items),
    );
    payload.insert(
        "deliveries".to_owned(),
        sources.deliveries.map_or(Value::Null, without_items),
    );
    payload.insert("driver".to_owned(), absent_driver());
    payload.insert("rate".to_owned(), absent_rate());
    payload.insert(
        "db".to_owned(),
        db::assemble(
            now_ms,
            sources.sessions.as_ref(),
            sources.attempts.as_ref(),
            sources.diagnostics.as_ref(),
        ),
    );
    Value::Object(payload)
}

/// The upload or delivery status contract with its per-session `items` array removed. The page
/// reads only the totals, and those arrays are the bulk of both payloads.
fn without_items(mut contract: Value) -> Value {
    if let Some(object) = contract.as_object_mut() {
        object.remove("items");
    }
    contract
}

/// The spike's backlog-driver section, permanently empty. The driver and its log are gone, and the
/// page reads exactly these values to render its burndown chart, driver pill and round pill in
/// their absent states.
fn absent_driver() -> Value {
    json!({
        "alive": false,
        "pids": [],
        "log_mtime_ms": null,
        "epochs": [],
        "current_epoch_index": null,
    })
}

/// The spike's archiving-rate section, permanently empty. Rate and ETA were derived from the
/// driver log's round-over-round deltas, so with the log gone the page shows both tiles as
/// unavailable.
fn absent_rate() -> Value {
    json!({
        "sessions_per_hour": null,
        "eta_hours": null,
        "window_rounds": 0,
    })
}

/// One `errors` entry, its message bounded at [`MAX_ERROR_CHARS`] characters.
fn error_entry(source: &str, message: &str) -> Value {
    json!({
        "source": source,
        "message": message.chars().take(MAX_ERROR_CHARS).collect::<String>(),
    })
}

/// The current Unix time in milliseconds, clamped at the epoch if the system clock predates it.
fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// One finished command's captured output.
#[derive(Debug)]
struct CommandOutput {
    succeeded: bool,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Error)]
enum CommandError {
    #[error("could not run {0}: {1}")]
    Spawn(String, std::io::Error),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("could not collect command output: {0}")]
    Io(String),
}

/// Runs `program` with `args`, killing it once `timeout` elapses.
///
/// Both streams are drained on their own threads: a `--limit 1000` session listing far exceeds a
/// pipe buffer, and an undrained pipe would block the child forever, which the timeout would then
/// misreport as a hang. Each captured stream is bounded at [`MAX_OUTPUT_BYTES`].
fn run_bounded(
    program: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<CommandOutput, CommandError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| CommandError::Spawn(program.display().to_string(), error))?;
    let stdout = child.stdout.take().expect("piped stdout is available");
    let stderr = child.stderr.take().expect("piped stderr is available");
    let stdout_reader = thread::spawn(move || read_bounded(stdout));
    let stderr_reader = thread::spawn(move || read_bounded(stderr));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(CommandError::Io(error.to_string())),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CommandError::Timeout(timeout));
        }
        thread::sleep(POLL_INTERVAL);
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| CommandError::Io("the stdout reader panicked".to_owned()))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| CommandError::Io("the stderr reader panicked".to_owned()))?;
    Ok(CommandOutput {
        succeeded: status.success(),
        stdout,
        stderr,
    })
}

/// Reads a child stream to end of file, bounded at [`MAX_OUTPUT_BYTES`] and decoded lossily. A read
/// error yields whatever arrived before it; the caller's JSON parse is what judges the result.
fn read_bounded<R: Read>(stream: R) -> String {
    let mut buffer = Vec::new();
    let _ = stream.take(MAX_OUTPUT_BYTES).read_to_end(&mut buffer);
    String::from_utf8_lossy(&buffer).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_sources() -> Sources {
        Sources {
            status: None,
            uploads: None,
            deliveries: None,
            sessions: None,
            attempts: None,
            diagnostics: None,
        }
    }

    #[test]
    fn payload_carries_the_spikes_top_level_keys_in_order() {
        let assembled = payload(1_700_000_000_000, Vec::new(), empty_sources());
        let keys: Vec<&str> = assembled
            .as_object()
            .expect("the payload is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "generated_at_ms",
                "errors",
                "status",
                "uploads",
                "deliveries",
                "driver",
                "rate",
                "db",
            ]
        );
        assert_eq!(assembled["generated_at_ms"], json!(1_700_000_000_000_i64));
    }

    #[test]
    fn every_missing_source_leaves_its_section_null() {
        let assembled = payload(0, Vec::new(), empty_sources());
        for section in ["status", "uploads", "deliveries", "db"] {
            assert_eq!(assembled[section], Value::Null, "{section} should be null");
        }
        assert_eq!(assembled["errors"], json!([]));
    }

    #[test]
    fn driver_and_rate_are_always_the_absent_placeholders() {
        let assembled = payload(0, Vec::new(), empty_sources());
        assert_eq!(
            assembled["driver"],
            json!({"alive": false, "pids": [], "log_mtime_ms": null, "epochs": [],
                   "current_epoch_index": null})
        );
        assert_eq!(
            assembled["rate"],
            json!({"sessions_per_hour": null, "eta_hours": null, "window_rounds": 0})
        );
    }

    #[test]
    fn upload_and_delivery_items_are_dropped_but_totals_kept() {
        let sources = Sources {
            uploads: Some(
                json!({"total": 863, "uploaded": 856, "failed": 0, "dead_letter": 7,
                                 "items": [{"session_id": "a1"}]}),
            ),
            deliveries: Some(json!({"total": 432, "delivered": 432, "failed": 0,
                                    "dead_letter": 0, "items": []})),
            ..empty_sources()
        };
        let assembled = payload(0, Vec::new(), sources);
        assert_eq!(assembled["uploads"].get("items"), None);
        assert_eq!(assembled["deliveries"].get("items"), None);
        assert_eq!(assembled["uploads"]["uploaded"], json!(856));
        assert_eq!(assembled["deliveries"]["total"], json!(432));
    }

    #[test]
    fn status_is_forwarded_verbatim() {
        let status = json!({"schema_version": 1, "sessions": {"total": 462, "archived": 431}});
        let sources = Sources {
            status: Some(status.clone()),
            ..empty_sources()
        };
        assert_eq!(payload(0, Vec::new(), sources)["status"], status);
    }

    #[test]
    fn error_messages_are_truncated_to_the_banner_bound() {
        let entry = error_entry("munshi attempts", &"e".repeat(MAX_ERROR_CHARS + 50));
        assert_eq!(entry["source"], json!("munshi attempts"));
        assert_eq!(
            entry["message"]
                .as_str()
                .expect("message is a string")
                .len(),
            MAX_ERROR_CHARS
        );
    }

    #[test]
    fn a_missing_binary_becomes_one_error_entry_and_no_section() {
        let collector = Collector::new(PathBuf::from("/nonexistent/munshi-for-tests"));
        let mut errors = Vec::new();
        let parsed = collector.munshi_json(&["status", "--json"], "munshi status", &mut errors);
        assert!(parsed.is_none());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["source"], json!("munshi status"));
        assert!(
            errors[0]["message"]
                .as_str()
                .expect("message is a string")
                .contains("/nonexistent/munshi-for-tests")
        );
    }

    #[test]
    fn a_failing_command_reports_its_stderr() {
        let collector = Collector::new(PathBuf::from("/bin/sh"));
        let mut errors = Vec::new();
        let parsed = collector.munshi_json(
            &["-c", "echo 'unrecognized subcommand' >&2; exit 2"],
            "munshi attempts",
            &mut errors,
        );
        assert!(parsed.is_none());
        assert_eq!(
            errors[0]["message"],
            json!("unrecognized subcommand"),
            "stderr should be preferred over empty stdout"
        );
    }

    #[test]
    fn unparseable_output_becomes_an_error_entry() {
        let collector = Collector::new(PathBuf::from("/bin/sh"));
        let mut errors = Vec::new();
        let parsed = collector.munshi_json(&["-c", "echo not-json"], "munshi status", &mut errors);
        assert!(parsed.is_none());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["source"], json!("munshi status"));
    }

    #[test]
    fn a_command_that_outruns_its_bound_is_killed() {
        let error = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "sleep 30"],
            Duration::from_millis(150),
        )
        .expect_err("a sleeping command exceeds a 150ms bound");
        assert!(matches!(error, CommandError::Timeout(_)), "{error}");
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_is_captured_whole() {
        let output = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "head -c 500000 /dev/zero | tr '\\0' 'x'"],
            Duration::from_secs(10),
        )
        .expect("the command runs");
        assert!(output.succeeded);
        assert_eq!(output.stdout.len(), 500_000);
    }
}
