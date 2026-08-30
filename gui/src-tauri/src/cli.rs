//! Bounded invocation of the `munshi` executable, and the only place this app talks to Munshi.
//!
//! Every figure the GUI displays comes from `munshi ... --json` (ADR 0007). This app never opens
//! Munshi's state directory, never touches `munshi.db`, and never invokes the hidden
//! `hook`/`hook-worker` subcommands. That keeps it unable to contend with a running Munshi,
//! unable to corrupt anything, and unable to drift from the published contracts.
//!
//! The invocation discipline is ported from `munshi-dashboard`'s collector, for the same reasons
//! it was written there: both pipes are drained on their own threads (an undrained pipe deadlocks
//! a `--limit 1000` listing, and the deadline would then misreport the deadlock as a hang), each
//! stream is capped, and a process still running at the deadline is killed rather than waited on.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

/// How long any one `munshi` invocation may run before it is killed. `sessions --limit 1000` on a
/// large archive is the slow case; a summarizing `retry` is bounded by Munshi's own timeout and
/// returns as soon as it has claimed the session, not when the model finishes.
const DEADLINE: Duration = Duration::from_secs(25);

/// How often the deadline is re-checked while the child runs.
const POLL: Duration = Duration::from_millis(20);

/// Per-stream ceiling. `sessions --json --limit 1000` is the largest documented payload and sits
/// far below this; the cap exists so a pathological binary cannot exhaust memory.
const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// How much of a failure's stderr is carried into the UI banner.
const MAX_ERROR_CHARS: usize = 400;

/// One failed invocation, surfaced to the page as a banner entry rather than failing the snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct CommandError {
    /// The logical section this invocation feeds (`status`, `sessions`, …).
    pub source: String,
    /// The argument vector, for a user who wants to reproduce the failure in a terminal.
    pub command: Vec<String>,
    /// A single-line, length-bounded explanation.
    pub message: String,
}

/// Runs `munshi` with `args`, enforcing the deadline and the stream caps.
///
/// On success the child's stdout is returned; stderr is only ever used to explain a failure.
///
/// Returns `Err(message)` for every failure mode a caller must degrade over: the binary is
/// missing, it exited non-zero, or it outlived the deadline.
pub fn run(program: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        // No stdin: nothing Munshi is asked here is interactive, and an inherited terminal would
        // let a prompt block the app forever.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run {}: {error}", program.display()))?;

    // Take both pipes before waiting. Draining them on their own threads is what keeps a large
    // listing from filling a pipe buffer and deadlocking against our own wait loop.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_reader = thread::spawn(move || read_capped(stdout_pipe.as_mut()));
    let stderr_reader = thread::spawn(move || read_capped(stderr_pipe.as_mut()));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= DEADLINE {
                    // Kill, then still join the readers: they end when the pipes close.
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return Err(format!(
                        "{} {} did not finish within {}s",
                        program.display(),
                        args.join(" "),
                        DEADLINE.as_secs()
                    ));
                }
                thread::sleep(POLL);
            }
            Err(error) => {
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("could not wait for {}: {error}", program.display()));
            }
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    if !status.success() {
        let detail = first_line(&stderr).unwrap_or_else(|| match status.code() {
            Some(code) => format!("exited with status {code}"),
            None => "terminated by a signal".to_string(),
        });
        return Err(truncate(&detail, MAX_ERROR_CHARS));
    }

    Ok(stdout)
}

/// Runs `munshi` and parses its stdout as one JSON contract.
///
/// Every read-only contract is valid but empty on a machine that has never run `munshi register`
/// (ADR 0007), so an unregistered machine reaches the page as empty panels, not as errors.
pub fn run_json(program: &Path, section: &str, args: &[&str]) -> Result<Value, CommandError> {
    let describe = || {
        let mut command = vec![program.display().to_string()];
        command.extend(args.iter().map(|argument| (*argument).to_string()));
        command
    };

    let stdout = run(program, args).map_err(|message| CommandError {
        source: section.to_string(),
        command: describe(),
        message,
    })?;

    serde_json::from_str::<Value>(&stdout).map_err(|error| CommandError {
        source: section.to_string(),
        command: describe(),
        message: truncate(&format!("output was not valid JSON: {error}"), MAX_ERROR_CHARS),
    })
}

/// Reads a pipe to EOF, stopping at the cap. Reading to the cap and then continuing to discard
/// keeps the child from blocking on a full pipe even when its output is being thrown away.
fn read_capped(pipe: Option<&mut impl Read>) -> String {
    let Some(pipe) = pipe else {
        return String::new();
    };
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if buffer.len() < MAX_STREAM_BYTES {
                    let room = MAX_STREAM_BYTES - buffer.len();
                    buffer.extend_from_slice(&chunk[..read.min(room)]);
                }
            }
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The first non-empty line of `text`, which is where Munshi puts its `error: …` line.
fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Truncates on a character boundary, marking that it happened.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `/bin/sh` stands in for `munshi` so these exercise the invocation discipline itself
    /// without needing a built CLI on the machine running the tests.
    fn shell() -> PathBuf {
        PathBuf::from("/bin/sh")
    }

    #[test]
    fn captures_stdout_of_a_successful_run() {
        let stdout = run(&shell(), &["-c", "printf '{\"ok\":true}'"]).expect("should succeed");
        assert_eq!(stdout, "{\"ok\":true}");
    }

    #[test]
    fn reports_the_error_line_of_a_failed_run() {
        let error = run(&shell(), &["-c", "echo 'error: nope' >&2; exit 3"])
            .expect_err("a non-zero exit is an error");
        assert_eq!(error, "error: nope");
    }

    #[test]
    fn falls_back_to_the_exit_status_when_stderr_is_silent() {
        let error = run(&shell(), &["-c", "exit 4"]).expect_err("a non-zero exit is an error");
        assert_eq!(error, "exited with status 4");
    }

    #[test]
    fn reports_a_missing_binary_rather_than_panicking() {
        let error = run(Path::new("/nonexistent/munshi"), &["status"])
            .expect_err("a missing binary is an error");
        assert!(error.starts_with("could not run /nonexistent/munshi"), "{error}");
    }

    #[test]
    fn drains_a_large_stream_without_deadlocking() {
        // Far more than a pipe buffer holds: this deadlocks if the reader is not on its own thread.
        let stdout = run(&shell(), &["-c", "yes abcdefghij | head -c 400000"])
            .expect("a large stream should still complete");
        assert_eq!(stdout.len(), 400_000);
    }

    #[test]
    fn parse_failure_is_reported_against_its_section() {
        let error = run_json(&shell(), "status", &["-c", "printf 'not json'"])
            .expect_err("unparseable output is an error");
        assert_eq!(error.source, "status");
        assert!(error.message.starts_with("output was not valid JSON"), "{}", error.message);
    }

    #[test]
    fn truncate_marks_that_it_shortened() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("abc", 3), "abc");
    }
}
