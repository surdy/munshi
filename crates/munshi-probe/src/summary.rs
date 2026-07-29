use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use munshi_runner::{RunnerConfig, RunnerError, run_bounded};
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
    let stdout = run_bounded(
        &RunnerConfig {
            binary: config.binary.clone(),
            args: config.args.clone(),
            envs: Vec::new(),
            timeout: config.timeout,
            stdout_limit: config.stdout_limit,
            stderr_limit: config.stderr_limit,
        },
        input,
    )
    .map_err(map_runner_error)?;
    let result: Phase0Summary =
        serde_json::from_slice(&stdout).map_err(SummaryProbeError::MalformedJson)?;
    let normalized = Phase0Summary {
        title: result.title.trim().to_owned(),
        summary: result.summary.trim().to_owned(),
    };
    validate_field("title", &normalized.title, TITLE_LIMIT)?;
    validate_field("summary", &normalized.summary, SUMMARY_LIMIT)?;
    Ok(normalized)
}

fn map_runner_error(error: RunnerError) -> SummaryProbeError {
    match error {
        RunnerError::Spawn(error) => SummaryProbeError::Spawn(error),
        RunnerError::Io(error) => SummaryProbeError::Io(error),
        RunnerError::Timeout(timeout) => SummaryProbeError::Timeout(timeout),
        RunnerError::OutputLimit { stream, limit } => {
            SummaryProbeError::OutputLimit { stream, limit }
        }
        RunnerError::NonZeroExit { code, stderr_bytes } => {
            SummaryProbeError::NonZeroExit { code, stderr_bytes }
        }
        RunnerError::WorkerPanic => SummaryProbeError::WorkerPanic,
    }
}

fn validate_field(field: &'static str, value: &str, max: usize) -> Result<(), SummaryProbeError> {
    let length = value.chars().count();
    if length == 0 || length > max {
        Err(SummaryProbeError::InvalidShape { field, max })
    } else {
        Ok(())
    }
}
