use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::archive_git::{ArchiveGitError, commit_archive_revision};
use crate::policy::resolve_policy;
use crate::project::{ProjectIdentity, ProjectIdentityError, inspect_project};
use crate::registration::{RegistrationError, load_stored_config};
use crate::render::{
    ArchiveMetadata, RenderError, archive_path, atomic_replace, content_hash,
    parse_archive_markdown, render_revision_markdown,
};
use crate::source::{
    PreviousSource, SessionReference, SourceError, TranscriptLoadMode, load_session_update,
    resolve_session_reference, validate_transcript_envelope,
};
use crate::state::{
    BudgetOutcome, Claim, ClaimOutcome, CompletionReason, PersistedArchive, PlannedArchive,
    SessionRecord, StateError, StateStore, WaitState, migrate_legacy_state, now_ms,
    try_acquire_session_lock,
};
use crate::summary::{
    SummarizerConfig, SummaryError, build_revision_summary_input, build_summary_input, run_summary,
};

const MAX_HOOK_PAYLOAD_BYTES: u64 = 64 * 1024;
const DEFAULT_RECOVERY_STALE_MS: u64 = 30 * 60 * 1_000;
const RECOVERY_SCAN_LIMIT: usize = 32;

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
    #[serde(default)]
    pub cause_code: Option<String>,
    pub session_id: Option<String>,
    pub recorded_at_unix_ms: u128,
}

#[derive(Debug, Error)]
pub enum HookWorkerError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error("archive persisted but final state commit failed")]
    PostPersist(#[source] StateError),
    #[error("archive replacement reached its target but durability confirmation failed")]
    PostPersistWrite(#[source] RenderError),
    #[error(transparent)]
    Registration(#[from] RegistrationError),
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("rewritten transcript is no longer archive-worthy")]
    SourceNoLongerArchiveWorthy,
    #[error(transparent)]
    Project(#[from] ProjectIdentityError),
    #[error(transparent)]
    ArchiveGit(#[from] ArchiveGitError),
    #[error(transparent)]
    Summary(#[from] SummaryError),
    #[error(transparent)]
    Render(#[from] RenderError),
    #[error("hook worker I/O failed")]
    Io(#[from] io::Error),
    #[error("hook worker JSON failed")]
    Json(#[from] serde_json::Error),
    #[error("processing deferred by policy or budget ({0})")]
    Deferred(&'static str),
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

pub fn handle_hook(event: HookEvent, state_directory: &Path, input: impl Read) {
    let result = match event {
        HookEvent::AgentStop => handle_agent_stop(state_directory, input),
        HookEvent::SessionEnd => handle_session_end(state_directory, input),
    };
    if let Err(failure) = result {
        record_failure(state_directory, failure);
    }
    let _ = spawn_recovery_sweep(state_directory);
}

pub fn run_archive_worker(
    state_directory: &Path,
    session_id: &str,
) -> Result<HookResult, HookWorkerError> {
    let result = run_archive_worker_inner(state_directory, session_id);
    if let Err(error) = &result {
        if let Ok(mut state) = StateStore::open(state_directory) {
            let category = worker_error_code(error);
            let _ = state.clear_worker_reservation(session_id);
            let _ = state.record_diagnostic("archive-worker", category, None, Some(session_id));
        }
    }
    result
}

fn run_archive_worker_inner(
    state_directory: &Path,
    session_id: &str,
) -> Result<HookResult, HookWorkerError> {
    validate_session_id(session_id).map_err(|_| StateError::InvalidState)?;
    let Some(_lock) = try_acquire_session_lock(state_directory, session_id)? else {
        return Ok(HookResult::Failed {
            code: "worker-busy".to_owned(),
        });
    };

    let stored = load_stored_config(state_directory)?;
    let output_directory = PathBuf::from(&stored.output_directory);
    let mut state = StateStore::open(state_directory)?;
    let session = state.get_session(session_id)?;
    if session
        .as_ref()
        .is_none_or(|session| session.current_revision == 0)
    {
        let _ = state.hydrate_session_from_archives(&output_directory, session_id)?;
    }
    if let Some(result) = reconcile_persisted_attempt(&mut state, &output_directory, session_id)? {
        return Ok(result);
    }
    if state
        .get_session(session_id)?
        .is_some_and(|session| session.lifecycle_state == "processing")
    {
        state.abandon_processing(session_id, "worker-interrupted")?;
    }
    if let Some(result) = policy_gate(&mut state, &stored, session_id)? {
        return Ok(result);
    }

    let lease = Duration::from_millis(stored.limits.timeout_ms.saturating_add(60_000));
    let claim =
        match state.claim_session(session_id, lease, false, stored.policy.max_concurrency)? {
            ClaimOutcome::NotClaimable => return current_result(&state, session_id),
            ClaimOutcome::ConcurrencyExceeded => {
                state.record_deferred(session_id, "concurrency-deferred")?;
                state.clear_worker_reservation(session_id)?;
                return Ok(HookResult::Failed {
                    code: "concurrency-deferred".to_owned(),
                });
            }
            ClaimOutcome::Claimed(claim) => claim,
        };
    match process_claim(&mut state, state_directory, &stored, &claim) {
        Ok(result) => Ok(result),
        Err(error @ (HookWorkerError::PostPersist(_) | HookWorkerError::PostPersistWrite(_))) => {
            Err(error)
        }
        Err(HookWorkerError::Deferred(category)) => {
            state.abandon_processing(session_id, category)?;
            Ok(HookResult::Failed {
                code: category.to_owned(),
            })
        }
        Err(error) => {
            let category = worker_error_code(&error);
            let retryable = worker_error_retryable(&error);
            state.fail_attempt(&claim, category, retryable)?;
            Ok(HookResult::Failed {
                code: category.to_owned(),
            })
        }
    }
}

pub fn run_recovery(
    state_directory: &Path,
    stale_after: Duration,
    force_retry: bool,
    rebuild: bool,
) -> Result<(), HookWorkerError> {
    let recovery_deadline = Instant::now() + Duration::from_secs(1);
    let _recovery_lock = loop {
        if let Some(lock) = try_acquire_session_lock(state_directory, "_recovery")? {
            break lock;
        }
        if Instant::now() >= recovery_deadline {
            return Err(StateError::LockBusy.into());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stored = load_stored_config(state_directory)?;
    if rebuild {
        crate::state::rebuild_database(state_directory, Path::new(&stored.output_directory))?;
    }
    let mut state = StateStore::open(state_directory)?;
    migrate_legacy_state(
        &mut state,
        state_directory,
        Duration::from_millis(stored.limits.timeout_ms).saturating_add(Duration::from_secs(60)),
    )?;
    let cutoff = now_ms().saturating_sub(stale_after.as_millis().try_into().unwrap_or(i64::MAX));
    let mut reserved_sessions = Vec::new();
    for session in state.unresolved_sessions()? {
        let Ok(path) = resolve_fallback_transcript(state_directory, &session.session_id) else {
            continue;
        };
        let metadata = fs::metadata(&path)?;
        let evidence = format!(
            "{}:{}:{}",
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec()
        );
        if state.attach_recovered_transcript(&session.session_id, &path, &evidence)? {
            reserved_sessions.push(session.session_id);
        }
    }
    for session in state.stale_known_sessions(cutoff)? {
        let Some(path) = session.transcript_path.as_deref() else {
            continue;
        };
        let Some(origin) = session.origin_cwd.as_deref() else {
            continue;
        };
        if !source_is_stale_and_supported(path, cutoff, stored.limits.max_source_bytes) {
            continue;
        }
        let metadata = fs::metadata(path)?;
        let evidence = format!(
            "{}:{}:{}",
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec()
        );
        if state.mark_recovery_interrupted(&session.session_id, path, Some(origin), &evidence)? {
            reserved_sessions.push(session.session_id);
        }
    }
    discover_unknown_sessions(
        &mut state,
        state_directory,
        cutoff,
        stored.limits.max_source_bytes,
    )?;
    reserved_sessions.extend(state.reserve_eligible_workers(force_retry, RECOVERY_SCAN_LIMIT)?);
    reserved_sessions.sort();
    reserved_sessions.dedup();
    for session_id in reserved_sessions {
        if spawn_worker(state_directory, &session_id).is_err() {
            let _ = state.clear_worker_reservation(&session_id);
            let _ =
                state.record_diagnostic("recovery", "worker-spawn-failed", None, Some(&session_id));
        }
    }
    Ok(())
}

pub fn wait_for_hook_result(
    state_directory: &Path,
    session_id: &str,
    timeout: Duration,
) -> Result<HookResult, HookWorkerError> {
    validate_session_id(session_id).map_err(|_| StateError::InvalidState)?;
    let deadline = Instant::now() + timeout;
    let state = StateStore::open(state_directory)?;
    loop {
        let (status, relative_path, error) = state.wait_state(session_id)?;
        match status {
            WaitState::Archived => {
                return Ok(HookResult::Archived {
                    relative_path: relative_path
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                });
            }
            WaitState::NotArchiveWorthy => return Ok(HookResult::NotArchiveWorthy),
            WaitState::Failed => {
                return Ok(HookResult::Failed {
                    code: error.unwrap_or_else(|| "archive-failed".to_owned()),
                });
            }
            WaitState::Pending => {}
        }
        if Instant::now() >= deadline {
            return Err(HookWorkerError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for hook worker",
            )));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn read_last_failure(state_directory: &Path) -> Result<Option<HookFailure>, HookWorkerError> {
    let state = StateStore::open(state_directory)?;
    Ok(state.latest_diagnostic()?.map(|diagnostic| HookFailure {
        operation: diagnostic.operation,
        code: diagnostic.category,
        cause_code: diagnostic.cause_category,
        session_id: diagnostic.session_id,
        recorded_at_unix_ms: diagnostic.recorded_at_ms.try_into().unwrap_or_default(),
    }))
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
    let mut state = StateStore::open(state_directory).map_err(|_| {
        failure(
            "agent-stop",
            "state-open-failed",
            Some(payload.session_id.clone()),
        )
    })?;
    state
        .ingest_agent_stop(
            &payload.session_id,
            payload.timestamp.try_into().unwrap_or(i64::MAX),
            Path::new(&payload.cwd),
            Path::new(&payload.transcript_path),
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
    let completion = match payload.reason.as_str() {
        "complete" => CompletionReason::Complete,
        "user_exit" => CompletionReason::Interrupted,
        _ => CompletionReason::Unknown,
    };
    let mut state = StateStore::open(state_directory).map_err(|_| {
        failure(
            "session-end",
            "state-open-failed",
            Some(payload.session_id.clone()),
        )
    })?;
    let needs_fallback = state
        .get_session(&payload.session_id)
        .ok()
        .flatten()
        .is_none_or(|session| session.transcript_path.is_none());
    let fallback = needs_fallback
        .then(|| resolve_fallback_transcript(state_directory, &payload.session_id))
        .transpose()
        .ok()
        .flatten();
    let reserved = state
        .ingest_session_end(
            &payload.session_id,
            payload.timestamp.try_into().unwrap_or(i64::MAX),
            Path::new(&payload.cwd),
            &payload.reason,
            completion,
            fallback.as_deref(),
        )
        .map_err(|_| {
            failure(
                "session-end",
                "state-write-failed",
                Some(payload.session_id.clone()),
            )
        })?;
    if reserved && spawn_worker(state_directory, &payload.session_id).is_err() {
        let _ = state.clear_worker_reservation(&payload.session_id);
        return Err(failure(
            "session-end",
            "worker-spawn-failed",
            Some(payload.session_id),
        ));
    }
    Ok(())
}

fn process_claim(
    state: &mut StateStore,
    state_directory: &Path,
    stored: &crate::registration::StoredConfig,
    claim: &Claim,
) -> Result<HookResult, HookWorkerError> {
    let transcript_path = claim
        .session
        .transcript_path
        .clone()
        .or_else(|| resolve_fallback_transcript(state_directory, &claim.session.session_id).ok())
        .ok_or(SourceError::TranscriptNotFound)?;
    let resolved = resolve_session_reference(&SessionReference {
        session_id: Some(claim.session.session_id.clone()),
        events_path: Some(transcript_path),
        copilot_home: None,
    })?;
    let output_directory = PathBuf::from(&stored.output_directory);
    let prior = load_prior_archive(&output_directory, claim)?;
    let previous_source = prior.as_ref().map(|(_, archive)| {
        let cursor = archive.cursor.as_ref();
        PreviousSource {
            normalizer_version: cursor.map_or(0, |cursor| cursor.normalizer_version),
            record_count: cursor.map_or(archive.source_cursor, |cursor| cursor.record_count),
            byte_offset: cursor.map_or(0, |cursor| cursor.byte_offset),
            prefix_hash: cursor
                .map(|cursor| cursor.prefix_hash.clone())
                .unwrap_or_default(),
            source_hash: archive.source_hash.clone(),
            source_bytes: cursor.map_or(0, |cursor| cursor.source_bytes),
            started_at: archive.started_at.clone(),
            updated_at: archive.updated_at.clone(),
            user_requests: claim
                .session
                .previous_source
                .as_ref()
                .map_or(0, |source| source.user_requests),
            assistant_messages: claim
                .session
                .previous_source
                .as_ref()
                .map_or(0, |source| source.assistant_messages),
            tool_activities: claim
                .session
                .previous_source
                .as_ref()
                .map_or(0, |source| source.tool_activities),
        }
    });
    let update = load_session_update(
        &resolved,
        stored.limits.max_source_bytes,
        previous_source.as_ref(),
    )?;
    if update.mode == TranscriptLoadMode::Unchanged {
        state.complete_no_change(claim)?;
        return Ok(HookResult::Archived {
            relative_path: claim
                .session
                .markdown_relative_path
                .clone()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        });
    }
    if claim.session.current_revision == 0 && !update.session.is_archive_worthy() {
        state.complete_not_archive_worthy(claim)?;
        return Ok(HookResult::NotArchiveWorthy);
    }
    if claim.session.current_revision > 0
        && update.mode == TranscriptLoadMode::Full
        && !update.session.is_archive_worthy()
    {
        return Err(HookWorkerError::SourceNoLongerArchiveWorthy);
    }

    let project = match claim.session.project.clone() {
        Some(project) if !project.component.is_empty() => project,
        _ => inspect_project(
            claim
                .session
                .origin_cwd
                .as_deref()
                .ok_or(StateError::InvalidState)?,
        )?,
    };
    let prior_summary = prior.as_ref().map(|(_, archive)| &archive.summary);
    let cursor_only = claim.session.current_revision > 0
        && update.mode == TranscriptLoadMode::Delta
        && update.session.events.is_empty();
    let summary = if cursor_only {
        prior_summary.cloned().ok_or(StateError::InvalidState)?
    } else {
        let policy = resolve_policy(
            &stored.policy.as_global(),
            &stored.policy.disabled_projects,
            &project.identity,
            claim.session.origin_cwd.as_deref(),
        );
        if !policy.enabled {
            let category = policy
                .disabled_reason
                .map(|reason| reason.as_category())
                .unwrap_or("project-disabled");
            return Err(HookWorkerError::Deferred(category));
        }
        let max_input_bytes = policy
            .max_input_bytes
            .unwrap_or(stored.limits.max_input_bytes);
        let timeout_ms = policy.timeout_ms.unwrap_or(stored.limits.timeout_ms);
        let input = if update.mode == TranscriptLoadMode::Delta {
            build_revision_summary_input(
                &update.session,
                &project,
                prior_summary.ok_or(StateError::InvalidState)?,
                max_input_bytes,
            )?
        } else {
            build_summary_input(&update.session, &project, max_input_bytes)?
        };
        // Checking the budget and recording the call happen in one atomic transaction (see
        // `reserve_summarizer_call`), so two processes racing on the same project's budget
        // cannot both observe capacity and both proceed. This runs only once `input` has been
        // built successfully, so a call that will never reach the summarizer (for example
        // oversized input rejected above) is never charged against the budget.
        match state.reserve_summarizer_call(
            &project.identity,
            now_ms(),
            policy.max_calls_per_hour,
            policy.max_calls_per_day,
        )? {
            BudgetOutcome::HourlyExceeded => {
                return Err(HookWorkerError::Deferred("budget-hourly-exceeded"));
            }
            BudgetOutcome::DailyExceeded => {
                return Err(HookWorkerError::Deferred("budget-daily-exceeded"));
            }
            BudgetOutcome::Reserved => {}
        }
        run_summary(
            &SummarizerConfig {
                binary: PathBuf::from(&stored.summarizer.executable),
                args: stored.summarizer.args.iter().map(Into::into).collect(),
                timeout: Duration::from_millis(timeout_ms),
                stdout_limit: stored.limits.max_stdout_bytes,
                stderr_limit: stored.limits.max_stderr_bytes,
            },
            input,
        )?
    };
    update.snapshot.verify_unchanged()?;

    let revision = if cursor_only {
        claim.session.current_revision
    } else {
        claim.session.current_revision.saturating_add(1)
    };
    let completion = claim
        .session
        .completion_reason
        .as_deref()
        .unwrap_or("unknown");
    let fallback_reason = update.fallback_reason.map(|reason| reason.code());
    let metadata = ArchiveMetadata {
        session: &update.session,
        project: &project,
    };
    let markdown =
        render_revision_markdown(&metadata, &summary, revision, completion, fallback_reason);
    let output = if let Some(relative) = claim.session.markdown_relative_path.as_ref() {
        validate_relative_archive_path(relative)?;
        output_directory.join(relative)
    } else {
        archive_path(&output_directory, &metadata)
    };
    let previous_markdown = fs::read(&output).ok();
    let relative_path = output
        .strip_prefix(&output_directory)
        .map_err(|_| StateError::InvalidState)?
        .to_path_buf();
    let markdown_hash = content_hash(markdown.as_bytes());
    let plan = PlannedArchive {
        revision,
        record_count: update.session.source_cursor,
        byte_offset: update.session.source_byte_cursor,
        prefix_hash: update.session.source_prefix_hash.clone(),
        source_hash: update.session.source_hash.clone(),
        source_bytes: update.session.source_bytes,
        markdown_relative_path: relative_path.clone(),
        markdown_hash: markdown_hash.clone(),
        completion_reason: completion.to_owned(),
        fallback_reason: fallback_reason.map(ToOwned::to_owned),
    };
    state.store_plan(claim, &plan)?;
    if let Err(error) = atomic_replace(&output, markdown.as_bytes()) {
        if fs::read(&output)
            .ok()
            .is_some_and(|bytes| content_hash(&bytes) == markdown_hash)
        {
            return Err(HookWorkerError::PostPersistWrite(error));
        }
        return Err(error.into());
    }
    if stored.archive_git_history && !cursor_only {
        if let Err(error) = commit_archive_revision(
            state_directory,
            &output_directory,
            &relative_path,
            Some(project.identity.as_str()),
            &claim.session.session_id,
            revision,
        ) {
            if let Err(rollback) =
                rollback_markdown_after_failed_commit(&output, previous_markdown.as_deref())
            {
                return Err(HookWorkerError::PostPersistWrite(rollback));
            }
            return Err(error.into());
        }
    }
    let summary_json = serde_json::to_vec(&summary)?;
    state
        .complete_attempt(
            claim,
            &PersistedArchive {
                revision,
                summary,
                summary_hash: content_hash(&summary_json),
                markdown_relative_path: relative_path.clone(),
                markdown_hash,
                project,
                normalizer_version: crate::source::NORMALIZER_VERSION,
                record_count: update.session.source_cursor,
                byte_offset: update.session.source_byte_cursor,
                prefix_hash: update.session.source_prefix_hash,
                source_hash: update.session.source_hash,
                source_bytes: update.session.source_bytes,
                started_at: update.session.started_at,
                updated_at: update.session.updated_at,
                user_requests: update.session.user_requests,
                assistant_messages: update.session.assistant_messages,
                tool_activities: update.session.tool_activities,
                completion_reason: completion.to_owned(),
                fallback_reason: fallback_reason.map(ToOwned::to_owned),
            },
            false,
        )
        .map_err(HookWorkerError::PostPersist)?;
    Ok(HookResult::Archived {
        relative_path: relative_path.to_string_lossy().into_owned(),
    })
}

fn reconcile_persisted_attempt(
    state: &mut StateStore,
    output_directory: &Path,
    session_id: &str,
) -> Result<Option<HookResult>, HookWorkerError> {
    let Some(plan) = state.pending_plan(session_id)? else {
        return Ok(None);
    };
    validate_relative_archive_path(&plan.plan.markdown_relative_path)?;
    let output = output_directory.join(&plan.plan.markdown_relative_path);
    let bytes = match fs::read(&output) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    if content_hash(&bytes) != plan.plan.markdown_hash {
        return Ok(None);
    }
    fs::File::open(&output)?.sync_all()?;
    fs::File::open(output.parent().ok_or(StateError::InvalidState)?)?.sync_all()?;
    let markdown = std::str::from_utf8(&bytes)
        .map_err(|_| RenderError::InvalidArchive)
        .and_then(parse_archive_markdown)?;
    let Some(cursor) = markdown.cursor.as_ref() else {
        return Ok(None);
    };
    if markdown.session_id != session_id
        || markdown.summary_revision != plan.plan.revision
        || cursor.record_count != plan.plan.record_count
        || cursor.byte_offset != plan.plan.byte_offset
        || cursor.prefix_hash != plan.plan.prefix_hash
        || cursor.source_hash != plan.plan.source_hash
    {
        return Ok(None);
    }
    let session = state
        .get_session(session_id)?
        .ok_or(StateError::InvalidState)?;
    let claim = Claim {
        attempt_id: plan.attempt_id,
        token: plan.token,
        state_generation: plan.state_generation,
        retry_state: plan.retry_state,
        session,
    };
    let summary_json = serde_json::to_vec(&markdown.summary)?;
    state.complete_attempt(
        &claim,
        &PersistedArchive {
            revision: markdown.summary_revision,
            summary: markdown.summary,
            summary_hash: content_hash(&summary_json),
            markdown_relative_path: plan.plan.markdown_relative_path.clone(),
            markdown_hash: plan.plan.markdown_hash.clone(),
            project: markdown.project,
            normalizer_version: cursor.normalizer_version,
            record_count: cursor.record_count,
            byte_offset: cursor.byte_offset,
            prefix_hash: cursor.prefix_hash.clone(),
            source_hash: cursor.source_hash.clone(),
            source_bytes: cursor.source_bytes,
            started_at: markdown.started_at,
            updated_at: markdown.updated_at,
            user_requests: claim
                .session
                .previous_source
                .as_ref()
                .map_or(0, |source| source.user_requests),
            assistant_messages: claim
                .session
                .previous_source
                .as_ref()
                .map_or(0, |source| source.assistant_messages),
            tool_activities: claim
                .session
                .previous_source
                .as_ref()
                .map_or(0, |source| source.tool_activities),
            completion_reason: markdown.completion_reason,
            fallback_reason: markdown.cursor_fallback_reason,
        },
        true,
    )?;
    Ok(Some(HookResult::Archived {
        relative_path: plan
            .plan
            .markdown_relative_path
            .to_string_lossy()
            .into_owned(),
    }))
}

fn rollback_markdown_after_failed_commit(
    output: &Path,
    previous_markdown: Option<&[u8]>,
) -> Result<(), RenderError> {
    if let Some(previous_markdown) = previous_markdown {
        return atomic_replace(output, previous_markdown);
    }
    if output.exists() {
        fs::remove_file(output).map_err(RenderError::Io)?;
        if let Some(parent) = output.parent() {
            fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(RenderError::Io)?;
        }
    }
    Ok(())
}

fn load_prior_archive(
    output_directory: &Path,
    claim: &Claim,
) -> Result<Option<(PathBuf, crate::render::ArchivedMarkdown)>, HookWorkerError> {
    if claim.session.current_revision == 0 {
        return Ok(None);
    }
    let relative = claim
        .session
        .markdown_relative_path
        .clone()
        .ok_or(StateError::InvalidState)?;
    validate_relative_archive_path(&relative)?;
    let bytes = fs::read(output_directory.join(&relative))?;
    if claim
        .session
        .markdown_hash
        .as_deref()
        .is_some_and(|hash| hash != content_hash(&bytes))
    {
        return Err(StateError::InvalidState.into());
    }
    let markdown = std::str::from_utf8(&bytes)
        .map_err(|_| RenderError::InvalidArchive)
        .and_then(parse_archive_markdown)?;
    if markdown.session_id != claim.session.session_id
        || markdown.summary_revision != claim.session.current_revision
    {
        return Err(StateError::InvalidState.into());
    }
    Ok(Some((relative, markdown)))
}

/// Determines a session's project identity, preferring the cached identity from a prior
/// successful summary and falling back to inspecting the session's recorded origin directory.
fn resolve_session_project(session: &SessionRecord) -> Option<ProjectIdentity> {
    if let Some(project) = session.project.clone() {
        if !project.component.is_empty() {
            return Some(project);
        }
    }
    session
        .origin_cwd
        .as_deref()
        .and_then(|cwd| inspect_project(cwd).ok())
}

/// Enforces global concurrency and per-project enable/disable policy before a session is claimed.
/// Returns `Some` with a result the caller should return immediately when work must be deferred;
/// deferred work is left in its current pending lifecycle state so a later hook or user-invoked
/// command retries it opportunistically once concurrency frees up or the project is re-enabled.
fn policy_gate(
    state: &mut StateStore,
    stored: &crate::registration::StoredConfig,
    session_id: &str,
) -> Result<Option<HookResult>, HookWorkerError> {
    // Concurrency is intentionally not checked here: it is enforced atomically inside
    // `claim_session`'s own `BEGIN IMMEDIATE` transaction, so the count it decides on and the
    // claim it allows or refuses always come from the same transaction. A separate advisory
    // check at this point would race with other processes doing the same and could let more
    // than `max_concurrency` sessions be claimed.
    let Some(session) = state.get_session(session_id)? else {
        return Ok(None);
    };
    let Some(project) = resolve_session_project(&session) else {
        return Ok(None);
    };
    let policy = resolve_policy(
        &stored.policy.as_global(),
        &stored.policy.disabled_projects,
        &project.identity,
        session.origin_cwd.as_deref(),
    );
    if !policy.enabled {
        let category = policy
            .disabled_reason
            .map(|reason| reason.as_category())
            .unwrap_or("project-disabled");
        let _ = state.record_deferred(session_id, category);
        state.clear_worker_reservation(session_id)?;
        return Ok(Some(current_result(state, session_id)?));
    }
    Ok(None)
}

fn current_result(state: &StateStore, session_id: &str) -> Result<HookResult, HookWorkerError> {
    let (wait, path, error) = state.wait_state(session_id)?;
    Ok(match wait {
        WaitState::Archived => HookResult::Archived {
            relative_path: path.unwrap_or_default().to_string_lossy().into_owned(),
        },
        WaitState::NotArchiveWorthy => HookResult::NotArchiveWorthy,
        WaitState::Failed | WaitState::Pending => HookResult::Failed {
            code: error.unwrap_or_else(|| "work-not-claimable".to_owned()),
        },
    })
}

fn resolve_fallback_transcript(
    state_directory: &Path,
    session_id: &str,
) -> Result<PathBuf, SourceError> {
    let copilot_home = state_directory
        .parent()
        .ok_or(SourceError::MissingCopilotHome)?
        .to_path_buf();
    let resolved = resolve_session_reference(&SessionReference {
        session_id: Some(session_id.to_owned()),
        events_path: None,
        copilot_home: Some(copilot_home),
    })?;
    validate_transcript_envelope(&resolved.events_path, 8 * 1024 * 1024)?;
    Ok(resolved.events_path)
}

fn discover_unknown_sessions(
    state: &mut StateStore,
    state_directory: &Path,
    cutoff_ms: i64,
    max_source_bytes: usize,
) -> Result<(), HookWorkerError> {
    let Some(copilot_home) = state_directory.parent() else {
        return Ok(());
    };
    let session_state = copilot_home.join("session-state");
    if !session_state.is_dir() {
        return Ok(());
    }
    let mut inspected = 0;
    for entry in fs::read_dir(session_state)? {
        if inspected >= RECOVERY_SCAN_LIMIT {
            break;
        }
        let entry = entry?;
        if entry.file_type()?.is_symlink() || !entry.metadata()?.is_dir() {
            continue;
        }
        let Some(session_id) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        if validate_session_id(&session_id).is_err() || state.get_session(&session_id)?.is_some() {
            continue;
        }
        let resolved = match resolve_session_reference(&SessionReference {
            session_id: Some(session_id.clone()),
            events_path: None,
            copilot_home: Some(copilot_home.to_path_buf()),
        }) {
            Ok(resolved) => resolved,
            Err(_) => continue,
        };
        if !source_is_stale_and_supported(&resolved.events_path, cutoff_ms, max_source_bytes) {
            continue;
        }
        let metadata = fs::metadata(&resolved.events_path)?;
        let evidence = format!(
            "{}:{}:{}",
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec()
        );
        let _ =
            state.mark_recovery_interrupted(&session_id, &resolved.events_path, None, &evidence)?;
        inspected += 1;
    }
    Ok(())
}

fn source_is_stale_and_supported(path: &Path, cutoff_ms: i64, max_source_bytes: usize) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let modified_ms = metadata
        .mtime()
        .saturating_mul(1_000)
        .saturating_add(metadata.mtime_nsec() / 1_000_000);
    modified_ms <= cutoff_ms && validate_transcript_envelope(path, max_source_bytes).is_ok()
}

fn spawn_worker(state_directory: &Path, session_id: &str) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    spawn_detached(
        Command::new(executable)
            .arg("hook-worker")
            .arg("--state-dir")
            .arg(state_directory)
            .arg("--session-id")
            .arg(session_id),
    )
}

fn spawn_recovery_sweep(state_directory: &Path) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    spawn_detached(
        Command::new(executable)
            .arg("hook")
            .arg("recover")
            .arg("--state-dir")
            .arg(state_directory)
            .arg("--stale-after-ms")
            .arg(DEFAULT_RECOVERY_STALE_MS.to_string()),
    )
}

fn spawn_detached(command: &mut Command) -> io::Result<()> {
    command
        .current_dir("/")
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
    command.spawn().map(|_| ())
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

fn validate_relative_archive_path(path: &Path) -> Result<(), StateError> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path.extension().and_then(|extension| extension.to_str()) != Some("md")
    {
        Err(StateError::InvalidState)
    } else {
        Ok(())
    }
}

fn failure(operation: &str, code: &str, session_id: Option<String>) -> HookFailure {
    HookFailure {
        operation: operation.to_owned(),
        code: code.to_owned(),
        cause_code: None,
        session_id,
        recorded_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    }
}

fn record_failure(state_directory: &Path, failure: HookFailure) {
    if let Ok(mut state) = StateStore::open(state_directory) {
        let _ = state.record_diagnostic(
            &failure.operation,
            &failure.code,
            failure.cause_code.as_deref(),
            failure.session_id.as_deref(),
        );
    }
}

fn worker_error_code(error: &HookWorkerError) -> &'static str {
    match error {
        HookWorkerError::State(_) => "state-failed",
        HookWorkerError::PostPersist(_) => "state-finalize-failed",
        HookWorkerError::PostPersistWrite(_) => "archive-finalize-failed",
        HookWorkerError::Registration(_) => "config-invalid",
        HookWorkerError::Source(SourceError::ChangedDuringRead) => "source-changed",
        HookWorkerError::Source(SourceError::IncompleteTrailingRecord) => "source-incomplete",
        HookWorkerError::Source(SourceError::TranscriptNotFound) => "transcript-unresolved",
        HookWorkerError::Source(_) => "source-failed",
        HookWorkerError::SourceNoLongerArchiveWorthy => "source-not-archive-worthy",
        HookWorkerError::Project(_) => "project-failed",
        HookWorkerError::ArchiveGit(ArchiveGitError::LockBusy) => "archive-git-busy",
        HookWorkerError::ArchiveGit(ArchiveGitError::RepositoryNotDedicated) => {
            "archive-git-invalid-repo"
        }
        HookWorkerError::ArchiveGit(ArchiveGitError::SourceRepositoryForbidden) => {
            "archive-git-source-repo"
        }
        HookWorkerError::ArchiveGit(_) => "archive-git-failed",
        HookWorkerError::Summary(_) => "summary-failed",
        HookWorkerError::Render(_) => "archive-write-failed",
        HookWorkerError::Io(_) => "io-failed",
        HookWorkerError::Json(_) => "json-failed",
        HookWorkerError::Deferred(category) => category,
    }
}

fn worker_error_retryable(error: &HookWorkerError) -> bool {
    matches!(
        error,
        HookWorkerError::Source(
            SourceError::ChangedDuringRead
                | SourceError::IncompleteTrailingRecord
                | SourceError::TranscriptNotFound
        ) | HookWorkerError::Summary(_)
            | HookWorkerError::Io(_)
            | HookWorkerError::State(_)
            | HookWorkerError::PostPersist(_)
            | HookWorkerError::PostPersistWrite(_)
            | HookWorkerError::ArchiveGit(ArchiveGitError::LockBusy)
    )
}
