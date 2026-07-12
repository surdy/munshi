use std::fs;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::archive::{ArchiveConfig, ArchiveError, ArchiveOutcome, archive_session};
use crate::registration::{
    RegistrationError, atomic_json_replace, create_owned_lock, durable_remove, load_stored_config,
    validate_regular_owned_file,
};
use crate::source::SessionReference;

const MAX_HOOK_PAYLOAD_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    AgentStop,
    SessionEnd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum HookResult {
    Archived { relative_path: String },
    NotArchiveWorthy,
    Failed { code: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookFailure {
    pub operation: String,
    pub code: String,
    pub session_id: Option<String>,
    pub recorded_at_unix_ms: u128,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentStopPayload {
    #[serde(rename = "sessionId")]
    session_id: String,
    timestamp: u64,
    cwd: String,
    #[serde(rename = "transcriptPath")]
    transcript_path: String,
    #[serde(rename = "stopReason")]
    stop_reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionEndPayload {
    #[serde(rename = "sessionId")]
    session_id: String,
    timestamp: u64,
    cwd: String,
    reason: String,
    #[serde(default)]
    error: Option<IgnoredAny>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingSession {
    version: u32,
    session_id: String,
    transcript_path: String,
    origin_cwd: String,
}

pub fn handle_hook(event: HookEvent, state_directory: &Path, input: impl Read) {
    let result = match event {
        HookEvent::AgentStop => handle_agent_stop(state_directory, input),
        HookEvent::SessionEnd => handle_session_end(state_directory, input),
    };
    if let Err(failure) = result {
        record_failure(state_directory, failure);
    }
}

pub fn run_archive_worker(
    state_directory: &Path,
    session_id: &str,
) -> Result<HookResult, RegistrationError> {
    validate_session_id(session_id)?;
    let lock_path = worker_lock_path(state_directory, session_id);
    let result = run_archive_worker_inner(state_directory, session_id);
    let finalized = (|| match result {
        Ok(result) => {
            if !matches!(result, HookResult::Failed { .. }) {
                durable_remove(&pending_path(state_directory, session_id))?;
            }
            atomic_json_replace(&result_path(state_directory, session_id), &result)?;
            Ok(result)
        }
        Err(failure) => {
            record_failure(state_directory, failure.clone());
            let result = HookResult::Failed { code: failure.code };
            atomic_json_replace(&result_path(state_directory, session_id), &result)?;
            Ok(result)
        }
    })();
    let _ = durable_remove(&lock_path);
    finalized
}

pub fn wait_for_hook_result(
    state_directory: &Path,
    session_id: &str,
    timeout: Duration,
) -> Result<HookResult, RegistrationError> {
    validate_session_id(session_id)?;
    let path = result_path(state_directory, session_id);
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            validate_regular_owned_file(&path)?;
            let bytes = fs::read(&path).map_err(RegistrationError::Io)?;
            return serde_json::from_slice(&bytes).map_err(RegistrationError::Json);
        }
        if Instant::now() >= deadline {
            return Err(RegistrationError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for hook worker",
            )));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn read_last_failure(state_directory: &Path) -> Result<Option<HookFailure>, RegistrationError> {
    let path = state_directory.join("failures/last.json");
    if !path.exists() {
        return Ok(None);
    }
    validate_regular_owned_file(&path)?;
    let bytes = fs::read(path).map_err(RegistrationError::Io)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(RegistrationError::Json)
}

fn handle_agent_stop(state_directory: &Path, input: impl Read) -> Result<(), HookFailure> {
    let payload: AgentStopPayload =
        read_one_json(input).map_err(|code| failure("agent-stop", code, None))?;
    validate_session_id(&payload.session_id)
        .map_err(|_| failure("agent-stop", "invalid-session-id", None))?;
    validate_timestamp(payload.timestamp)
        .map_err(|code| failure("agent-stop", code, Some(payload.session_id.clone())))?;
    validate_absolute_string(&payload.cwd)
        .map_err(|code| failure("agent-stop", code, Some(payload.session_id.clone())))?;
    validate_absolute_string(&payload.transcript_path)
        .map_err(|code| failure("agent-stop", code, Some(payload.session_id.clone())))?;
    if payload.stop_reason != "end_turn" {
        return Err(failure(
            "agent-stop",
            "unsupported-stop-reason",
            Some(payload.session_id),
        ));
    }
    let pending = PendingSession {
        version: 1,
        session_id: payload.session_id.clone(),
        transcript_path: payload.transcript_path,
        origin_cwd: payload.cwd,
    };
    durable_remove(&result_path(state_directory, &payload.session_id)).map_err(|_| {
        failure(
            "agent-stop",
            "state-remove-failed",
            Some(payload.session_id.clone()),
        )
    })?;
    atomic_json_replace(
        &pending_path(state_directory, &payload.session_id),
        &pending,
    )
    .map_err(|_| failure("agent-stop", "state-write-failed", Some(payload.session_id)))
}

fn handle_session_end(state_directory: &Path, input: impl Read) -> Result<(), HookFailure> {
    let payload: SessionEndPayload =
        read_one_json(input).map_err(|code| failure("session-end", code, None))?;
    validate_session_id(&payload.session_id)
        .map_err(|_| failure("session-end", "invalid-session-id", None))?;
    validate_timestamp(payload.timestamp)
        .map_err(|code| failure("session-end", code, Some(payload.session_id.clone())))?;
    validate_absolute_string(&payload.cwd)
        .map_err(|code| failure("session-end", code, Some(payload.session_id.clone())))?;
    if payload.reason.trim().is_empty() || payload.reason.len() > 128 {
        return Err(failure(
            "session-end",
            "invalid-reason",
            Some(payload.session_id),
        ));
    }
    let _ = payload.error;
    let pending = pending_path(state_directory, &payload.session_id);
    if !pending.exists() {
        return Ok(());
    }
    let lock_path = worker_lock_path(state_directory, &payload.session_id);
    let Some(lock) = create_owned_lock(&lock_path).map_err(|_| {
        failure(
            "session-end",
            "worker-lock-failed",
            Some(payload.session_id.clone()),
        )
    })?
    else {
        return Ok(());
    };
    drop(lock);

    let executable = std::env::current_exe().map_err(|_| {
        failure(
            "session-end",
            "current-executable-failed",
            Some(payload.session_id.clone()),
        )
    })?;
    let mut command = Command::new(executable);
    command
        .arg("hook")
        .arg("archive-worker")
        .arg("--state-dir")
        .arg(state_directory)
        .arg("--session-id")
        .arg(&payload.session_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    if command.spawn().is_err() {
        let _ = durable_remove(&lock_path);
        return Err(failure(
            "session-end",
            "worker-spawn-failed",
            Some(payload.session_id),
        ));
    }
    Ok(())
}

fn run_archive_worker_inner(
    state_directory: &Path,
    session_id: &str,
) -> Result<HookResult, HookFailure> {
    let pending_path = pending_path(state_directory, session_id);
    validate_regular_owned_file(&pending_path).map_err(|_| {
        failure(
            "archive-worker",
            "pending-state-invalid",
            Some(session_id.to_owned()),
        )
    })?;
    let pending_bytes = fs::read(&pending_path).map_err(|_| {
        failure(
            "archive-worker",
            "pending-state-read-failed",
            Some(session_id.to_owned()),
        )
    })?;
    let pending: PendingSession = serde_json::from_slice(&pending_bytes).map_err(|_| {
        failure(
            "archive-worker",
            "pending-state-malformed",
            Some(session_id.to_owned()),
        )
    })?;
    if pending.version != 1 || pending.session_id != session_id {
        return Err(failure(
            "archive-worker",
            "pending-state-mismatch",
            Some(session_id.to_owned()),
        ));
    }
    validate_absolute_string(&pending.transcript_path)
        .map_err(|code| failure("archive-worker", code, Some(session_id.to_owned())))?;
    validate_absolute_string(&pending.origin_cwd)
        .map_err(|code| failure("archive-worker", code, Some(session_id.to_owned())))?;
    let stored = load_stored_config(state_directory).map_err(|_| {
        failure(
            "archive-worker",
            "config-invalid",
            Some(session_id.to_owned()),
        )
    })?;
    let outcome = archive_session(&ArchiveConfig {
        reference: SessionReference {
            session_id: Some(session_id.to_owned()),
            events_path: Some(PathBuf::from(pending.transcript_path)),
            copilot_home: None,
        },
        project_directory: PathBuf::from(pending.origin_cwd),
        output_directory: PathBuf::from(stored.output_directory),
        summarizer_binary: PathBuf::from(stored.summarizer.executable),
        summarizer_args: stored.summarizer.args.into_iter().map(Into::into).collect(),
        timeout: Duration::from_millis(stored.limits.timeout_ms),
        max_source_bytes: stored.limits.max_source_bytes,
        max_input_bytes: stored.limits.max_input_bytes,
        max_stdout_bytes: stored.limits.max_stdout_bytes,
        max_stderr_bytes: stored.limits.max_stderr_bytes,
    })
    .map_err(|error| {
        failure(
            "archive-worker",
            archive_error_code(&error),
            Some(session_id.to_owned()),
        )
    })?;
    Ok(match outcome {
        ArchiveOutcome::Archived { relative_path, .. } => HookResult::Archived {
            relative_path: relative_path.to_string_lossy().into_owned(),
        },
        ArchiveOutcome::NotArchiveWorthy { .. } => HookResult::NotArchiveWorthy,
    })
}

fn read_one_json<T: for<'de> Deserialize<'de>>(input: impl Read) -> Result<T, &'static str> {
    let mut bytes = Vec::new();
    input
        .take(MAX_HOOK_PAYLOAD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "payload-read-failed")?;
    if bytes.len() as u64 > MAX_HOOK_PAYLOAD_BYTES {
        return Err("payload-too-large");
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let value = T::deserialize(&mut deserializer).map_err(|_| "payload-invalid")?;
    deserializer
        .end()
        .map_err(|_| "payload-not-single-object")?;
    Ok(value)
}

fn validate_timestamp(timestamp: u64) -> Result<(), &'static str> {
    if timestamp == 0 {
        Err("invalid-timestamp")
    } else {
        Ok(())
    }
}

fn validate_absolute_string(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.len() > 16 * 1024 || !Path::new(value).is_absolute() {
        Err("invalid-path")
    } else {
        Ok(())
    }
}

fn validate_session_id(value: &str) -> Result<(), RegistrationError> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Err(RegistrationError::MalformedOwnedFile)
    } else {
        Ok(())
    }
}

fn pending_path(state_directory: &Path, session_id: &str) -> PathBuf {
    state_directory
        .join("pending")
        .join(format!("{session_id}.json"))
}

fn worker_lock_path(state_directory: &Path, session_id: &str) -> PathBuf {
    state_directory
        .join("workers")
        .join(format!("{session_id}.lock"))
}

fn result_path(state_directory: &Path, session_id: &str) -> PathBuf {
    state_directory
        .join("results")
        .join(format!("{session_id}.json"))
}

fn failure(operation: &str, code: &str, session_id: Option<String>) -> HookFailure {
    HookFailure {
        operation: operation.to_owned(),
        code: code.to_owned(),
        session_id,
        recorded_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    }
}

fn record_failure(state_directory: &Path, failure: HookFailure) {
    let _ = atomic_json_replace(&state_directory.join("failures/last.json"), &failure);
}

fn archive_error_code(error: &ArchiveError) -> &'static str {
    match error {
        ArchiveError::Source(_) => "source-failed",
        ArchiveError::Project(_) => "project-failed",
        ArchiveError::Summary(_) => "summary-failed",
        ArchiveError::Render(_) => "archive-write-failed",
    }
}
