use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const TITLE_LIMIT: usize = 200;
const SUMMARY_LIMIT: usize = 2_000;

#[derive(Debug, Clone)]
pub struct SummaryProbeConfig {
    pub binary: PathBuf,
    pub args: Vec<OsString>,
    pub timeout: Duration,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Phase0Summary {
    pub title: String,
    pub summary: String,
}

#[derive(Debug, Error)]
pub enum SummaryProbeError {
    #[error("failed to spawn summary executable: {0}")]
    Spawn(#[source] io::Error),
    #[error("summary process I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("summary process timed out after {0:?}")]
    Timeout(Duration),
    #[error("summary process {stream} exceeded its {limit}-byte limit")]
    OutputLimit { stream: &'static str, limit: usize },
    #[error("summary process exited unsuccessfully (code {code:?}, {stderr_bytes} stderr bytes)")]
    NonZeroExit {
        code: Option<i32>,
        stderr_bytes: usize,
    },
    #[error("summary stdout was not valid JSON: {0}")]
    MalformedJson(#[source] serde_json::Error),
    #[error("summary field {field} must contain between 1 and {max} characters")]
    InvalidShape { field: &'static str, max: usize },
    #[error("summary worker thread terminated unexpectedly")]
    WorkerPanic,
}

pub fn run_summary_probe(
    config: &SummaryProbeConfig,
    input: Vec<u8>,
) -> Result<Phase0Summary, SummaryProbeError> {
    let mut command = Command::new(&config.binary);
    command
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    let mut child = command.spawn().map_err(SummaryProbeError::Spawn)?;
    let pid = child.id();
    let mut stdin = child.stdin.take().expect("piped stdin is available");
    let stdout = child.stdout.take().expect("piped stdout is available");
    let stderr = child.stderr.take().expect("piped stderr is available");

    let stdin_worker = thread::spawn(move || {
        let result = stdin.write_all(&input);
        drop(stdin);
        result
    });
    let stdout_exceeded = Arc::new(AtomicBool::new(false));
    let stderr_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_worker = spawn_bounded_reader(stdout, config.stdout_limit, stdout_exceeded.clone());
    let stderr_worker = spawn_bounded_reader(stderr, config.stderr_limit, stderr_exceeded.clone());

    let started = Instant::now();
    let status = loop {
        if stdout_exceeded.load(Ordering::Relaxed) {
            terminate_process_group(&mut child, pid);
            let (_stdin, _stdout, _stderr) =
                join_workers(stdin_worker, stdout_worker, stderr_worker)?;
            return Err(SummaryProbeError::OutputLimit {
                stream: "stdout",
                limit: config.stdout_limit,
            });
        }
        if stderr_exceeded.load(Ordering::Relaxed) {
            terminate_process_group(&mut child, pid);
            let (_stdin, _stdout, _stderr) =
                join_workers(stdin_worker, stdout_worker, stderr_worker)?;
            return Err(SummaryProbeError::OutputLimit {
                stream: "stderr",
                limit: config.stderr_limit,
            });
        }
        if started.elapsed() >= config.timeout {
            terminate_process_group(&mut child, pid);
            let (_stdin, _stdout, _stderr) =
                join_workers(stdin_worker, stdout_worker, stderr_worker)?;
            return Err(SummaryProbeError::Timeout(config.timeout));
        }

        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                terminate_process_group(&mut child, pid);
                let (_stdin, _stdout, _stderr) =
                    join_workers(stdin_worker, stdout_worker, stderr_worker)?;
                return Err(SummaryProbeError::Io(error));
            }
        }
    };

    kill_remaining_group(pid);
    let (stdin_result, stdout, stderr) = join_workers(stdin_worker, stdout_worker, stderr_worker)?;
    if let Err(error) = stdin_result {
        if error.kind() != io::ErrorKind::BrokenPipe {
            return Err(SummaryProbeError::Io(error));
        }
    }
    if stdout.exceeded {
        return Err(SummaryProbeError::OutputLimit {
            stream: "stdout",
            limit: config.stdout_limit,
        });
    }
    if stderr.exceeded {
        return Err(SummaryProbeError::OutputLimit {
            stream: "stderr",
            limit: config.stderr_limit,
        });
    }
    validate_exit(status, stderr.bytes.len())?;

    let result: Phase0Summary =
        serde_json::from_slice(&stdout.bytes).map_err(SummaryProbeError::MalformedJson)?;
    validate_field("title", &result.title, TITLE_LIMIT)?;
    validate_field("summary", &result.summary, SUMMARY_LIMIT)?;
    Ok(result)
}

struct BoundedRead {
    bytes: Vec<u8>,
    exceeded: bool,
}

type ReaderWorker = thread::JoinHandle<io::Result<BoundedRead>>;
type StdinWorker = thread::JoinHandle<io::Result<()>>;

fn spawn_bounded_reader(
    mut reader: impl Read + Send + 'static,
    limit: usize,
    exceeded: Arc<AtomicBool>,
) -> ReaderWorker {
    thread::spawn(move || {
        let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
        let mut buffer = [0_u8; 8 * 1024];
        let mut hit_limit = false;
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let remaining = limit.saturating_sub(bytes.len());
            bytes.extend_from_slice(&buffer[..count.min(remaining)]);
            if count > remaining {
                hit_limit = true;
                exceeded.store(true, Ordering::Relaxed);
            }
        }
        Ok(BoundedRead {
            bytes,
            exceeded: hit_limit,
        })
    })
}

fn join_workers(
    stdin: StdinWorker,
    stdout: ReaderWorker,
    stderr: ReaderWorker,
) -> Result<(io::Result<()>, BoundedRead, BoundedRead), SummaryProbeError> {
    let stdin = stdin.join().map_err(|_| SummaryProbeError::WorkerPanic)?;
    let stdout = stdout
        .join()
        .map_err(|_| SummaryProbeError::WorkerPanic)??;
    let stderr = stderr
        .join()
        .map_err(|_| SummaryProbeError::WorkerPanic)??;
    Ok((stdin, stdout, stderr))
}

fn validate_exit(status: ExitStatus, stderr_bytes: usize) -> Result<(), SummaryProbeError> {
    if status.success() {
        Ok(())
    } else {
        Err(SummaryProbeError::NonZeroExit {
            code: status.code(),
            stderr_bytes,
        })
    }
}

fn validate_field(field: &'static str, value: &str, max: usize) -> Result<(), SummaryProbeError> {
    let length = value.trim().chars().count();
    if length == 0 || length > max {
        Err(SummaryProbeError::InvalidShape { field, max })
    } else {
        Ok(())
    }
}

fn terminate_process_group(child: &mut Child, pid: u32) {
    kill_remaining_group(pid);
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_remaining_group(pid: u32) {
    // The child starts a fresh process group, so a negative PID targets it and its descendants.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}
