use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::project::ProjectIdentity;
use crate::registration::{
    RegistrationError, durable_remove, ensure_directory, validate_regular_owned_file,
};
use crate::render::{ArchivedMarkdown, content_hash, parse_archive_markdown};
use crate::source::PreviousSource;
use crate::summary::StructuredSummary;

const DATABASE_FILE: &str = "munshi.db";
const SCHEMA_VERSION: i64 = 2;
const WORKER_RESERVATION_STALE_MS: i64 = 5_000;

#[derive(Debug, Error)]
pub enum StateError {
    #[error(transparent)]
    Registration(#[from] RegistrationError),
    #[error("SQLite state operation failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("state I/O failed")]
    Io(#[from] io::Error),
    #[error("state JSON failed")]
    Json(#[from] serde_json::Error),
    #[error("state schema is newer than this Munshi version")]
    NewerSchema,
    #[error("state contains an invalid value")]
    InvalidState,
    #[error("another process holds the requested state lock")]
    LockBusy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionReason {
    Complete,
    Interrupted,
    Unknown,
}

impl CompletionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Interrupted => "interrupted",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub database_id: i64,
    pub session_id: String,
    pub origin_cwd: Option<PathBuf>,
    pub project: Option<ProjectIdentity>,
    pub transcript_path: Option<PathBuf>,
    pub lifecycle_state: String,
    pub completion_reason: Option<String>,
    pub source_end_reason: Option<String>,
    pub current_revision: u64,
    pub current_summary: Option<StructuredSummary>,
    pub current_summary_hash: Option<String>,
    pub markdown_relative_path: Option<PathBuf>,
    pub markdown_hash: Option<String>,
    pub previous_source: Option<PreviousSource>,
    pub fallback_reason: Option<String>,
    pub state_generation: i64,
    pub active: bool,
    pub last_agent_stop_ms: Option<i64>,
    pub last_session_end_ms: Option<i64>,
    pub last_error_category: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Claim {
    pub attempt_id: i64,
    pub token: String,
    pub state_generation: i64,
    pub retry_state: String,
    pub session: SessionRecord,
}

#[derive(Debug, Clone)]
pub struct PlannedArchive {
    pub revision: u64,
    pub record_count: u64,
    pub byte_offset: u64,
    pub prefix_hash: String,
    pub source_hash: String,
    pub source_bytes: u64,
    pub markdown_relative_path: PathBuf,
    pub markdown_hash: String,
    pub archive_git_history: bool,
    pub completion_reason: String,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingPlan {
    pub attempt_id: i64,
    pub token: String,
    pub state_generation: i64,
    pub retry_state: String,
    pub plan: PlannedArchive,
}

#[derive(Debug, Clone)]
pub struct PersistedArchive {
    pub revision: u64,
    pub summary: StructuredSummary,
    pub summary_hash: String,
    pub markdown_relative_path: PathBuf,
    pub markdown_hash: String,
    pub project: ProjectIdentity,
    pub normalizer_version: u32,
    pub record_count: u64,
    pub byte_offset: u64,
    pub prefix_hash: String,
    pub source_hash: String,
    pub source_bytes: u64,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub user_requests: usize,
    pub assistant_messages: usize,
    pub tool_activities: usize,
    pub archive_git_history: bool,
    pub completion_reason: String,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub operation: String,
    pub category: String,
    pub cause_category: Option<String>,
    pub session_id: Option<String>,
    pub recorded_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitState {
    Pending,
    Archived,
    NotArchiveWorthy,
    Failed,
}

/// Result of [`StateStore::claim_session`]. `ConcurrencyExceeded` and a claimed session are
/// decided by the same atomic transaction as the count they are based on, so no other process can
/// observe stale capacity and also claim.
#[derive(Debug)]
pub enum ClaimOutcome {
    Claimed(Box<Claim>),
    ConcurrencyExceeded,
    NotClaimable,
}

/// Result of [`StateStore::reserve_summarizer_call`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetOutcome {
    Reserved,
    HourlyExceeded,
    DailyExceeded,
}

pub struct StateStore {
    connection: Connection,
}

impl StateStore {
    pub fn open(state_directory: &Path) -> Result<Self, StateError> {
        ensure_directory(state_directory)?;
        ensure_directory(&state_directory.join("locks"))?;
        let _migration_lock = acquire_named_lock_with_timeout(
            state_directory,
            "_migration",
            Duration::from_millis(250),
        )?;
        let database_path = state_directory.join(DATABASE_FILE);
        if database_path.exists() {
            validate_regular_owned_file(&database_path)?;
        } else if fs::symlink_metadata(&database_path).is_ok() {
            return Err(StateError::InvalidState);
        }
        let connection = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        fs::set_permissions(&database_path, fs::Permissions::from_mode(0o600))?;
        connection.busy_timeout(Duration::from_millis(250))?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;",
        )?;
        let mut store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn database_path(state_directory: &Path) -> PathBuf {
        state_directory.join(DATABASE_FILE)
    }

    fn migrate(&mut self) -> Result<(), StateError> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at_ms INTEGER NOT NULL
             );",
        )?;
        let current: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        if current > SCHEMA_VERSION {
            return Err(StateError::NewerSchema);
        }
        if current < 1 {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "CREATE TABLE sessions (
                    id INTEGER PRIMARY KEY,
                    source_kind TEXT NOT NULL,
                    source_session_id TEXT NOT NULL,
                    origin_cwd TEXT,
                    origin_project_identity TEXT,
                    origin_project_component TEXT,
                    origin_project_name TEXT,
                    origin_repository TEXT,
                    origin_branch TEXT,
                    transcript_path TEXT,
                    transcript_source TEXT,
                    last_agent_stop_ms INTEGER,
                    last_session_end_ms INTEGER,
                    source_end_reason TEXT,
                    completion_reason TEXT,
                    lifecycle_state TEXT NOT NULL CHECK (lifecycle_state IN (
                        'observed','summary-pending','archived','revision-pending',
                        'interrupted','processing','failed'
                    )),
                    retry_state TEXT CHECK (retry_state IN (
                        'summary-pending','revision-pending','interrupted'
                    )),
                    next_retry_at_ms INTEGER,
                    current_observation_id INTEGER REFERENCES source_observations(id),
                    current_summary_revision INTEGER NOT NULL DEFAULT 0,
                    current_summary_json TEXT,
                    current_summary_hash TEXT,
                    current_markdown_relative_path TEXT,
                    current_markdown_hash TEXT,
                    normalizer_version INTEGER,
                    source_cursor_records INTEGER,
                    source_cursor_bytes INTEGER,
                    source_prefix_hash TEXT,
                    source_hash TEXT,
                    source_bytes INTEGER,
                    source_started_at TEXT,
                    source_updated_at TEXT,
                    source_user_requests INTEGER NOT NULL DEFAULT 0,
                    source_assistant_messages INTEGER NOT NULL DEFAULT 0,
                    source_tool_activities INTEGER NOT NULL DEFAULT 0,
                    last_fallback_reason TEXT,
                    state_generation INTEGER NOT NULL DEFAULT 0,
                    active INTEGER NOT NULL DEFAULT 0,
                    claim_token TEXT,
                    claim_started_at_ms INTEGER,
                    worker_generation INTEGER,
                    worker_spawned_at_ms INTEGER,
                    last_error_category TEXT,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL,
                    UNIQUE(source_kind, source_session_id)
                 );
                 CREATE TABLE source_observations (
                    id INTEGER PRIMARY KEY,
                    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    event_kind TEXT NOT NULL CHECK (event_kind IN (
                        'agent-stop','session-end','recovery-scan','legacy'
                    )),
                    event_timestamp_ms INTEGER,
                    transcript_path TEXT,
                    completion_reason TEXT,
                    dedupe_key TEXT NOT NULL,
                    observed_at_ms INTEGER NOT NULL,
                    UNIQUE(session_id, dedupe_key)
                 );
                 CREATE TABLE processing_attempts (
                    id INTEGER PRIMARY KEY,
                    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    state_generation INTEGER NOT NULL,
                    retry_state TEXT NOT NULL,
                    lease_token TEXT NOT NULL UNIQUE,
                    owner_pid INTEGER,
                    started_at_ms INTEGER NOT NULL,
                    lease_expires_at_ms INTEGER NOT NULL,
                    finished_at_ms INTEGER,
                    outcome TEXT NOT NULL CHECK (outcome IN (
                        'processing','succeeded','failed','superseded','recovered'
                    )),
                    error_category TEXT,
                    recovery_reason TEXT,
                    planned_revision INTEGER,
                    planned_record_count INTEGER,
                    planned_byte_offset INTEGER,
                    planned_prefix_hash TEXT,
                    planned_source_hash TEXT,
                    planned_source_bytes INTEGER,
                    planned_markdown_relative_path TEXT,
                    planned_markdown_hash TEXT,
                    planned_archive_git_history INTEGER NOT NULL DEFAULT 0,
                    planned_completion_reason TEXT,
                    planned_fallback_reason TEXT
                 );
                 CREATE TABLE diagnostics (
                    id INTEGER PRIMARY KEY,
                    session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
                    operation TEXT NOT NULL,
                    category TEXT NOT NULL,
                    cause_category TEXT,
                    recorded_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE legacy_imports (
                    legacy_path TEXT PRIMARY KEY,
                    content_hash TEXT NOT NULL,
                    imported_at_ms INTEGER NOT NULL
                 );
                 CREATE INDEX source_observations_session_idx
                    ON source_observations(session_id, id);
                 CREATE INDEX processing_attempts_recovery_idx
                    ON processing_attempts(outcome, lease_expires_at_ms);
                 CREATE INDEX sessions_work_idx
                    ON sessions(lifecycle_state, active, next_retry_at_ms);",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![1, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 1)?;
            transaction.commit()?;
        }
        if current < 2 {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "CREATE TABLE summarizer_calls (
                    id INTEGER PRIMARY KEY,
                    project_identity TEXT NOT NULL,
                    called_at_ms INTEGER NOT NULL
                 );
                 CREATE INDEX summarizer_calls_project_idx
                    ON summarizer_calls(project_identity, called_at_ms);",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![2, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 2)?;
            transaction.commit()?;
        }
        let user_version: i64 =
            self.connection
                .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if user_version > SCHEMA_VERSION {
            return Err(StateError::NewerSchema);
        }
        ensure_processing_attempts_git_history_column(&self.connection)?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64, StateError> {
        self.connection
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn ingest_agent_stop(
        &mut self,
        session_id: &str,
        timestamp_ms: i64,
        origin_cwd: &Path,
        transcript_path: &Path,
    ) -> Result<(), StateError> {
        validate_session_id(session_id)?;
        let now = now_ms();
        let dedupe = dedupe_key(&[
            "agent-stop",
            session_id,
            &timestamp_ms.to_string(),
            path_text(origin_cwd)?,
            path_text(transcript_path)?,
        ]);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = upsert_session(
            &transaction,
            session_id,
            Some(origin_cwd),
            None,
            "hook",
            now,
        )?;
        transaction.execute(
            "UPDATE sessions SET
                transcript_path=CASE
                    WHEN last_agent_stop_ms IS NULL OR last_agent_stop_ms <= ?3 THEN ?2
                    ELSE transcript_path END,
                transcript_source=CASE
                    WHEN last_agent_stop_ms IS NULL OR last_agent_stop_ms <= ?3 THEN 'hook'
                    ELSE transcript_source END,
                last_agent_stop_ms=CASE
                    WHEN last_agent_stop_ms IS NULL OR last_agent_stop_ms < ?3 THEN ?3
                    ELSE last_agent_stop_ms END,
                updated_at_ms=?4
             WHERE id=?1",
            params![database_id, path_text(transcript_path)?, timestamp_ms, now],
        )?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO source_observations(
                session_id,event_kind,event_timestamp_ms,transcript_path,
                completion_reason,dedupe_key,observed_at_ms
             ) VALUES (?1,'agent-stop',?2,?3,NULL,?4,?5)",
            params![
                database_id,
                timestamp_ms,
                path_text(transcript_path)?,
                dedupe,
                now
            ],
        )?;
        if inserted == 1 {
            let observation_id = transaction.last_insert_rowid();
            transaction.execute(
                "UPDATE sessions SET
                    current_observation_id=?2,
                    state_generation=state_generation+1,
                    active=1,
                    lifecycle_state=CASE
                        WHEN lifecycle_state='processing' THEN lifecycle_state
                        WHEN current_summary_revision > 0 THEN 'revision-pending'
                        ELSE 'observed' END,
                    updated_at_ms=?3
                 WHERE id=?1",
                params![database_id, observation_id, now],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn ingest_session_end(
        &mut self,
        session_id: &str,
        timestamp_ms: i64,
        origin_cwd: &Path,
        source_reason: &str,
        completion_reason: CompletionReason,
        fallback_transcript_path: Option<&Path>,
    ) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let now = now_ms();
        let safe_source_reason = safe_source_reason(source_reason);
        let dedupe = dedupe_key(&[
            "session-end",
            session_id,
            &timestamp_ms.to_string(),
            path_text(origin_cwd)?,
            safe_source_reason,
        ]);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = upsert_session(
            &transaction,
            session_id,
            Some(origin_cwd),
            fallback_transcript_path,
            if fallback_transcript_path.is_some() {
                "version-pinned-fallback"
            } else {
                "hook"
            },
            now,
        )?;
        if let Some(path) = fallback_transcript_path {
            transaction.execute(
                "UPDATE sessions SET
                    transcript_path=COALESCE(transcript_path, ?2),
                    transcript_source=CASE WHEN transcript_path IS NULL
                        THEN 'version-pinned-fallback' ELSE transcript_source END
                 WHERE id=?1",
                params![database_id, path_text(path)?],
            )?;
        }
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO source_observations(
                session_id,event_kind,event_timestamp_ms,transcript_path,
                completion_reason,dedupe_key,observed_at_ms
             ) VALUES (?1,'session-end',?2,?3,?4,?5,?6)",
            params![
                database_id,
                timestamp_ms,
                fallback_transcript_path.map(path_text).transpose()?,
                completion_reason.as_str(),
                dedupe,
                now
            ],
        )?;
        if inserted == 0 {
            transaction.commit()?;
            return Ok(false);
        }
        transaction.execute(
            "UPDATE sessions SET
                last_session_end_ms=CASE
                    WHEN last_session_end_ms IS NULL OR last_session_end_ms < ?2 THEN ?2
                    ELSE last_session_end_ms END,
                source_end_reason=?3,
                completion_reason=?4,
                active=0,
                updated_at_ms=?5
             WHERE id=?1",
            params![
                database_id,
                timestamp_ms,
                safe_source_reason,
                completion_reason.as_str(),
                now
            ],
        )?;
        let observation_id = transaction.last_insert_rowid();
        transaction.execute(
            "UPDATE sessions SET
                current_observation_id=?2,
                state_generation=state_generation+1,
                lifecycle_state=CASE
                    WHEN lifecycle_state='processing' THEN lifecycle_state
                    WHEN current_summary_revision > 0 THEN 'revision-pending'
                    WHEN ?3='interrupted' OR ?3='unknown' THEN 'interrupted'
                    ELSE 'summary-pending' END,
                retry_state=NULL,
                next_retry_at_ms=NULL,
                last_error_category=CASE
                    WHEN transcript_path IS NULL THEN 'transcript-unresolved'
                    ELSE NULL END,
                updated_at_ms=?4
             WHERE id=?1",
            params![database_id, observation_id, completion_reason.as_str(), now],
        )?;
        let can_spawn: bool = transaction.query_row(
            "SELECT transcript_path IS NOT NULL AND origin_cwd IS NOT NULL
                AND lifecycle_state <> 'processing'
             FROM sessions WHERE id=?1",
            [database_id],
            |row| row.get(0),
        )?;
        let reserved = if can_spawn {
            reserve_worker_in_transaction(&transaction, database_id, now)?
        } else {
            false
        };
        transaction.commit()?;
        Ok(reserved)
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionRecord>, StateError> {
        validate_session_id(session_id)?;
        self.connection
            .query_row(
                "SELECT
                    id,source_session_id,origin_cwd,
                    origin_project_identity,origin_project_component,origin_project_name,
                    origin_repository,origin_branch,transcript_path,lifecycle_state,
                    completion_reason,source_end_reason,current_summary_revision,
                    current_summary_json,current_summary_hash,current_markdown_relative_path,
                    current_markdown_hash,normalizer_version,source_cursor_records,
                    source_cursor_bytes,source_prefix_hash,source_hash,source_bytes,
                    source_started_at,source_updated_at,source_user_requests,
                    source_assistant_messages,source_tool_activities,last_fallback_reason,
                    state_generation,active,last_agent_stop_ms,last_session_end_ms,
                    last_error_category
                 FROM sessions
                 WHERE source_kind='copilot-cli' AND source_session_id=?1",
                [session_id],
                session_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn record_diagnostic(
        &mut self,
        operation: &str,
        category: &str,
        cause_category: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<(), StateError> {
        let database_id = session_id
            .map(|session_id| {
                self.connection
                    .query_row(
                        "SELECT id FROM sessions
                         WHERE source_kind='copilot-cli' AND source_session_id=?1",
                        [session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
            })
            .transpose()?
            .flatten();
        self.connection.execute(
            "INSERT INTO diagnostics(
                session_id,operation,category,cause_category,recorded_at_ms
             ) VALUES (?1,?2,?3,?4,?5)",
            params![database_id, operation, category, cause_category, now_ms()],
        )?;
        Ok(())
    }

    /// Records that a session was left pending by policy or a budget before any attempt was
    /// claimed (a disabled project, exhausted concurrency, or an exhausted call budget). The
    /// session's lifecycle state and retry schedule are left untouched so a later hook or
    /// user-invoked command retries it opportunistically; only the visible diagnostic category is
    /// updated.
    pub fn record_deferred(&mut self, session_id: &str, category: &str) -> Result<(), StateError> {
        let now = now_ms();
        self.connection.execute(
            "UPDATE sessions SET last_error_category=?2,updated_at_ms=?3
             WHERE source_kind='copilot-cli' AND source_session_id=?1",
            params![session_id, category, now],
        )?;
        self.record_diagnostic("archive-worker", category, None, Some(session_id))
    }

    pub fn latest_diagnostic(&self) -> Result<Option<Diagnostic>, StateError> {
        self.connection
            .query_row(
                "SELECT d.operation,d.category,d.cause_category,s.source_session_id,
                        d.recorded_at_ms
                 FROM diagnostics d
                 LEFT JOIN sessions s ON s.id=d.session_id
                 ORDER BY d.id DESC LIMIT 1",
                [],
                |row| {
                    Ok(Diagnostic {
                        operation: row.get(0)?,
                        category: row.get(1)?,
                        cause_category: row.get(2)?,
                        session_id: row.get(3)?,
                        recorded_at_ms: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }
}

fn upsert_session(
    transaction: &Transaction<'_>,
    session_id: &str,
    origin_cwd: Option<&Path>,
    transcript_path: Option<&Path>,
    transcript_source: &str,
    now: i64,
) -> Result<i64, StateError> {
    transaction.execute(
        "INSERT INTO sessions(
            source_kind,source_session_id,origin_cwd,transcript_path,transcript_source,
            lifecycle_state,created_at_ms,updated_at_ms
         ) VALUES ('copilot-cli',?1,?2,?3,?4,'observed',?5,?5)
         ON CONFLICT(source_kind,source_session_id) DO UPDATE SET
            origin_cwd=COALESCE(sessions.origin_cwd,excluded.origin_cwd),
            transcript_path=COALESCE(excluded.transcript_path,sessions.transcript_path),
            transcript_source=CASE WHEN excluded.transcript_path IS NOT NULL
                THEN excluded.transcript_source ELSE sessions.transcript_source END,
            updated_at_ms=excluded.updated_at_ms",
        params![
            session_id,
            origin_cwd.map(path_text).transpose()?,
            transcript_path.map(path_text).transpose()?,
            transcript_source,
            now
        ],
    )?;
    transaction
        .query_row(
            "SELECT id FROM sessions
             WHERE source_kind='copilot-cli' AND source_session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn reserve_worker_in_transaction(
    transaction: &Transaction<'_>,
    database_id: i64,
    now: i64,
) -> Result<bool, StateError> {
    let changed = transaction.execute(
        "UPDATE sessions SET
            worker_generation=state_generation,
            worker_spawned_at_ms=?2,
            updated_at_ms=?2
         WHERE id=?1
           AND active=0
           AND lifecycle_state IN (
                'summary-pending','revision-pending','interrupted','failed','processing'
           )
           AND transcript_path IS NOT NULL
           AND origin_cwd IS NOT NULL
           AND (worker_spawned_at_ms IS NULL OR worker_spawned_at_ms < ?3)
           AND (lifecycle_state <> 'processing' OR EXISTS (
                SELECT 1 FROM processing_attempts a
                WHERE a.session_id=sessions.id AND a.outcome='processing'
                  AND a.lease_expires_at_ms <= ?2
           ))
           AND (lifecycle_state <> 'failed'
                OR next_retry_at_ms IS NULL
                OR (next_retry_at_ms >= 0 AND next_retry_at_ms <= ?2))",
        params![database_id, now, now - WORKER_RESERVATION_STALE_MS],
    )?;
    Ok(changed == 1)
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let summary_json: Option<String> = row.get(13)?;
    let current_summary = summary_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                13,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    let project_identity: Option<String> = row.get(3)?;
    let project = match project_identity {
        Some(identity) => Some(ProjectIdentity {
            identity,
            component: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            project: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            repository: row.get(6)?,
            branch: row.get(7)?,
        }),
        None => None,
    };
    let normalizer_version: Option<u32> = row.get(17)?;
    let previous_source = match normalizer_version {
        Some(normalizer_version) => Some(PreviousSource {
            normalizer_version,
            record_count: row.get::<_, Option<u64>>(18)?.ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(
                    18,
                    "source_cursor_records".to_owned(),
                    rusqlite::types::Type::Null,
                )
            })?,
            byte_offset: row.get::<_, Option<u64>>(19)?.ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(
                    19,
                    "source_cursor_bytes".to_owned(),
                    rusqlite::types::Type::Null,
                )
            })?,
            prefix_hash: row.get::<_, Option<String>>(20)?.ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(
                    20,
                    "source_prefix_hash".to_owned(),
                    rusqlite::types::Type::Null,
                )
            })?,
            source_hash: row.get::<_, Option<String>>(21)?.ok_or_else(|| {
                rusqlite::Error::InvalidColumnType(
                    21,
                    "source_hash".to_owned(),
                    rusqlite::types::Type::Null,
                )
            })?,
            source_bytes: row.get::<_, Option<u64>>(22)?.unwrap_or_default(),
            started_at: row.get(23)?,
            updated_at: row.get(24)?,
            user_requests: row.get::<_, i64>(25)?.try_into().unwrap_or_default(),
            assistant_messages: row.get::<_, i64>(26)?.try_into().unwrap_or_default(),
            tool_activities: row.get::<_, i64>(27)?.try_into().unwrap_or_default(),
        }),
        None => None,
    };
    Ok(SessionRecord {
        database_id: row.get(0)?,
        session_id: row.get(1)?,
        origin_cwd: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
        project,
        transcript_path: row.get::<_, Option<String>>(8)?.map(PathBuf::from),
        lifecycle_state: row.get(9)?,
        completion_reason: row.get(10)?,
        source_end_reason: row.get(11)?,
        current_revision: row.get::<_, i64>(12)?.try_into().unwrap_or_default(),
        current_summary,
        current_summary_hash: row.get(14)?,
        markdown_relative_path: row.get::<_, Option<String>>(15)?.map(PathBuf::from),
        markdown_hash: row.get(16)?,
        previous_source,
        fallback_reason: row.get(28)?,
        state_generation: row.get(29)?,
        active: row.get(30)?,
        last_agent_stop_ms: row.get(31)?,
        last_session_end_ms: row.get(32)?,
        last_error_category: row.get(33)?,
    })
}

fn safe_source_reason(reason: &str) -> &'static str {
    match reason {
        "complete" => "complete",
        "user_exit" => "user_exit",
        _ => "unknown",
    }
}

fn path_text(path: &Path) -> Result<&str, StateError> {
    path.to_str().ok_or(StateError::InvalidState)
}

fn ensure_processing_attempts_git_history_column(
    connection: &Connection,
) -> Result<(), StateError> {
    let has_column = {
        let mut statement = connection.prepare("PRAGMA table_info(processing_attempts)")?;
        let mut rows = statement.query([])?;
        let mut found = false;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == "planned_archive_git_history" {
                found = true;
                break;
            }
        }
        found
    };
    if !has_column {
        connection.execute(
            "ALTER TABLE processing_attempts
             ADD COLUMN planned_archive_git_history INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(())
}

fn dedupe_key(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("sha256:{:x}", digest.finalize())
}

fn validate_session_id(value: &str) -> Result<(), StateError> {
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
        Err(StateError::InvalidState)
    } else {
        Ok(())
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

impl StateStore {
    pub fn reserve_worker(&mut self, session_id: &str, force: bool) -> Result<bool, StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if force {
            transaction.execute(
                "UPDATE sessions SET next_retry_at_ms=NULL
                 WHERE source_kind='copilot-cli' AND source_session_id=?1
                   AND lifecycle_state='failed'",
                [session_id],
            )?;
        }
        let database_id = transaction
            .query_row(
                "SELECT id FROM sessions
                 WHERE source_kind='copilot-cli' AND source_session_id=?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let reserved = match database_id {
            Some(database_id) => reserve_worker_in_transaction(&transaction, database_id, now)?,
            None => false,
        };
        transaction.commit()?;
        Ok(reserved)
    }

    pub fn reserve_eligible_workers(
        &mut self,
        force: bool,
        limit: usize,
    ) -> Result<Vec<String>, StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if force {
            transaction.execute(
                "UPDATE sessions SET next_retry_at_ms=NULL
                 WHERE lifecycle_state='failed'",
                [],
            )?;
        }
        let mut statement = transaction.prepare(
            "SELECT id,source_session_id
             FROM sessions
             WHERE active=0
               AND lifecycle_state IN (
                    'summary-pending','revision-pending','interrupted','failed','processing'
               )
               AND transcript_path IS NOT NULL
               AND origin_cwd IS NOT NULL
               AND (worker_spawned_at_ms IS NULL OR worker_spawned_at_ms < ?1)
               AND (lifecycle_state <> 'processing' OR EXISTS (
                    SELECT 1 FROM processing_attempts a
                    WHERE a.session_id=sessions.id AND a.outcome='processing'
                      AND a.lease_expires_at_ms <= ?2
               ))
               AND (lifecycle_state <> 'failed'
                    OR next_retry_at_ms IS NULL
                    OR (next_retry_at_ms >= 0 AND next_retry_at_ms <= ?2))
             ORDER BY updated_at_ms,id
             LIMIT ?3",
        )?;
        let rows = statement
            .query_map(
                params![now - WORKER_RESERVATION_STALE_MS, now, limit as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut reserved = Vec::new();
        for (database_id, session_id) in rows {
            if reserve_worker_in_transaction(&transaction, database_id, now)? {
                reserved.push(session_id);
            }
        }
        transaction.commit()?;
        Ok(reserved)
    }

    /// Counts sessions with a live (non-expired) processing lease. This is a non-authoritative
    /// snapshot for reporting only; enforcement uses the atomic check inside [`claim_session`]
    /// so a claim decision and the count it is based on always come from the same transaction.
    ///
    /// [`claim_session`]: StateStore::claim_session
    pub fn count_active_processing(&self) -> Result<i64, StateError> {
        let now = now_ms();
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM processing_attempts
                 WHERE outcome='processing' AND lease_expires_at_ms > ?1",
                [now],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Atomically checks a project's rolling hourly and daily summarizer-call budget and, only if
    /// neither limit is currently met, records one call and returns [`BudgetOutcome::Reserved`].
    /// The check and the reservation happen inside one `BEGIN IMMEDIATE` transaction, so two
    /// processes racing for the same project's budget cannot both observe capacity and both
    /// insert: SQLite serializes their write transactions, and the second to run always sees the
    /// first's committed row. Callers should call this immediately before invoking the
    /// summarizer, once they are certain a call will actually be made, so a call that never
    /// reaches the summarizer (for example an unchanged or cursor-only revision, or oversized
    /// input rejected before spawning it) is never charged.
    pub fn reserve_summarizer_call(
        &mut self,
        project_identity: &str,
        now_ms: i64,
        max_calls_per_hour: u32,
        max_calls_per_day: u32,
    ) -> Result<BudgetOutcome, StateError> {
        const PRUNE_WINDOW_MS: i64 = 25 * 60 * 60 * 1_000;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM summarizer_calls WHERE called_at_ms < ?1",
            [now_ms.saturating_sub(PRUNE_WINDOW_MS)],
        )?;
        let hour_ago = now_ms.saturating_sub(60 * 60 * 1_000);
        let hourly_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM summarizer_calls
             WHERE project_identity=?1 AND called_at_ms >= ?2",
            params![project_identity, hour_ago],
            |row| row.get(0),
        )?;
        if hourly_count >= max_calls_per_hour as i64 {
            transaction.commit()?;
            return Ok(BudgetOutcome::HourlyExceeded);
        }
        let day_ago = now_ms.saturating_sub(24 * 60 * 60 * 1_000);
        let daily_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM summarizer_calls
             WHERE project_identity=?1 AND called_at_ms >= ?2",
            params![project_identity, day_ago],
            |row| row.get(0),
        )?;
        if daily_count >= max_calls_per_day as i64 {
            transaction.commit()?;
            return Ok(BudgetOutcome::DailyExceeded);
        }
        transaction.execute(
            "INSERT INTO summarizer_calls(project_identity, called_at_ms) VALUES (?1,?2)",
            params![project_identity, now_ms],
        )?;
        transaction.commit()?;
        Ok(BudgetOutcome::Reserved)
    }

    /// Counts a project's summarizer invocations since `since_ms`. This is a read-only reporting
    /// helper; enforcement uses [`reserve_summarizer_call`] so the check and reservation are
    /// atomic.
    ///
    /// [`reserve_summarizer_call`]: StateStore::reserve_summarizer_call
    pub fn summarizer_calls_since(
        &self,
        project_identity: &str,
        since_ms: i64,
    ) -> Result<i64, StateError> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM summarizer_calls
                 WHERE project_identity=?1 AND called_at_ms >= ?2",
                params![project_identity, since_ms],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn clear_worker_reservation(&mut self, session_id: &str) -> Result<(), StateError> {
        self.connection.execute(
            "UPDATE sessions SET worker_generation=NULL,worker_spawned_at_ms=NULL
             WHERE source_kind='copilot-cli' AND source_session_id=?1",
            [session_id],
        )?;
        Ok(())
    }

    pub fn pending_plan(&self, session_id: &str) -> Result<Option<PendingPlan>, StateError> {
        self.connection
            .query_row(
                "SELECT a.id,a.lease_token,a.state_generation,a.retry_state,
                        a.planned_revision,a.planned_record_count,a.planned_byte_offset,
                        a.planned_prefix_hash,a.planned_source_hash,a.planned_source_bytes,
                        a.planned_markdown_relative_path,a.planned_markdown_hash,
                        a.planned_archive_git_history,
                        a.planned_completion_reason,a.planned_fallback_reason
                 FROM processing_attempts a
                 JOIN sessions s ON s.id=a.session_id
                 WHERE s.source_kind='copilot-cli' AND s.source_session_id=?1
                   AND a.outcome='processing'
                   AND a.planned_revision IS NOT NULL
                 ORDER BY a.id DESC LIMIT 1",
                [session_id],
                |row| {
                    Ok(PendingPlan {
                        attempt_id: row.get(0)?,
                        token: row.get(1)?,
                        state_generation: row.get(2)?,
                        retry_state: row.get(3)?,
                        plan: PlannedArchive {
                            revision: row.get::<_, i64>(4)?.try_into().unwrap_or_default(),
                            record_count: row.get::<_, i64>(5)?.try_into().unwrap_or_default(),
                            byte_offset: row.get::<_, i64>(6)?.try_into().unwrap_or_default(),
                            prefix_hash: row.get(7)?,
                            source_hash: row.get(8)?,
                            source_bytes: row.get::<_, i64>(9)?.try_into().unwrap_or_default(),
                            markdown_relative_path: PathBuf::from(row.get::<_, String>(10)?),
                            markdown_hash: row.get(11)?,
                            archive_git_history: row.get::<_, i64>(12)? != 0,
                            completion_reason: row.get(13)?,
                            fallback_reason: row.get(14)?,
                        },
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn abandon_processing(
        &mut self,
        session_id: &str,
        category: &str,
    ) -> Result<(), StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = transaction
            .query_row(
                "SELECT s.id,a.id,a.retry_state
                 FROM sessions s
                 JOIN processing_attempts a ON a.session_id=s.id
                 WHERE s.source_kind='copilot-cli' AND s.source_session_id=?1
                   AND a.outcome='processing'
                 ORDER BY a.id DESC LIMIT 1",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((database_id, attempt_id, retry_state)) = row {
            transaction.execute(
                "UPDATE processing_attempts SET
                    outcome='failed',error_category=?2,finished_at_ms=?3
                 WHERE id=?1 AND outcome='processing'",
                params![attempt_id, category, now],
            )?;
            transaction.execute(
                "UPDATE sessions SET
                    lifecycle_state=?2,retry_state=NULL,next_retry_at_ms=NULL,
                    claim_token=NULL,claim_started_at_ms=NULL,
                    worker_generation=NULL,worker_spawned_at_ms=NULL,
                    last_error_category=?3,updated_at_ms=?4
                 WHERE id=?1",
                params![database_id, retry_state, category, now],
            )?;
            transaction.execute(
                "INSERT INTO diagnostics(
                    session_id,operation,category,cause_category,recorded_at_ms
                 ) VALUES (?1,'archive-worker',?2,NULL,?3)",
                params![database_id, category, now],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn claim_session(
        &mut self,
        session_id: &str,
        lease_duration: Duration,
        force: bool,
        max_concurrency: usize,
    ) -> Result<ClaimOutcome, StateError> {
        validate_session_id(session_id)?;
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session = transaction
            .query_row(
                "SELECT
                    id,source_session_id,origin_cwd,
                    origin_project_identity,origin_project_component,origin_project_name,
                    origin_repository,origin_branch,transcript_path,lifecycle_state,
                    completion_reason,source_end_reason,current_summary_revision,
                    current_summary_json,current_summary_hash,current_markdown_relative_path,
                    current_markdown_hash,normalizer_version,source_cursor_records,
                    source_cursor_bytes,source_prefix_hash,source_hash,source_bytes,
                    source_started_at,source_updated_at,source_user_requests,
                    source_assistant_messages,source_tool_activities,last_fallback_reason,
                    state_generation,active,last_agent_stop_ms,last_session_end_ms,
                    last_error_category
                 FROM sessions
                 WHERE source_kind='copilot-cli' AND source_session_id=?1",
                [session_id],
                session_from_row,
            )
            .optional()?;
        let Some(ref mut session) = session else {
            transaction.commit()?;
            return Ok(ClaimOutcome::NotClaimable);
        };
        if session.active
            || !matches!(
                session.lifecycle_state.as_str(),
                "summary-pending" | "revision-pending" | "interrupted" | "failed"
            )
        {
            transaction.commit()?;
            return Ok(ClaimOutcome::NotClaimable);
        }
        if session.lifecycle_state == "failed" && !force {
            let ready: bool = transaction.query_row(
                "SELECT next_retry_at_ms IS NULL
                        OR (next_retry_at_ms >= 0 AND next_retry_at_ms <= ?2)
                 FROM sessions WHERE id=?1",
                params![session.database_id, now],
                |row| row.get(0),
            )?;
            if !ready {
                transaction.commit()?;
                return Ok(ClaimOutcome::NotClaimable);
            }
        }
        // Concurrency is checked inside this same BEGIN IMMEDIATE transaction as the claim
        // itself: SQLite serializes writers, so a second process's claim_session call blocks
        // here until this transaction commits or rolls back, and then re-reads a count that
        // already reflects this decision. That makes "count active leases, then claim" atomic
        // across processes rather than two separate races.
        let active_processing: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM processing_attempts
             WHERE outcome='processing' AND lease_expires_at_ms > ?1",
            [now],
            |row| row.get(0),
        )?;
        if active_processing >= max_concurrency as i64 {
            transaction.commit()?;
            return Ok(ClaimOutcome::ConcurrencyExceeded);
        }
        let retry_state = if session.lifecycle_state == "failed" {
            transaction
                .query_row(
                    "SELECT retry_state FROM sessions WHERE id=?1",
                    [session.database_id],
                    |row| row.get::<_, Option<String>>(0),
                )?
                .unwrap_or_else(|| {
                    if session.current_revision > 0 {
                        "revision-pending".to_owned()
                    } else {
                        "summary-pending".to_owned()
                    }
                })
        } else {
            session.lifecycle_state.clone()
        };
        let token = lease_token(session_id, now);
        let lease_ms: i64 = lease_duration
            .as_millis()
            .try_into()
            .unwrap_or(i64::MAX - now);
        transaction.execute(
            "INSERT INTO processing_attempts(
                session_id,state_generation,retry_state,lease_token,owner_pid,
                started_at_ms,lease_expires_at_ms,outcome
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,'processing')",
            params![
                session.database_id,
                session.state_generation,
                retry_state,
                token,
                std::process::id(),
                now,
                now.saturating_add(lease_ms)
            ],
        )?;
        let attempt_id = transaction.last_insert_rowid();
        transaction.execute(
            "UPDATE sessions SET
                lifecycle_state='processing',retry_state=?2,
                claim_token=?3,claim_started_at_ms=?4,
                worker_generation=NULL,worker_spawned_at_ms=NULL,
                updated_at_ms=?4
             WHERE id=?1",
            params![session.database_id, retry_state, token, now],
        )?;
        transaction.commit()?;
        Ok(ClaimOutcome::Claimed(Box::new(Claim {
            attempt_id,
            token,
            state_generation: session.state_generation,
            retry_state,
            session: session.clone(),
        })))
    }

    pub fn store_plan(&mut self, claim: &Claim, plan: &PlannedArchive) -> Result<(), StateError> {
        let changed = self.connection.execute(
            "UPDATE processing_attempts SET
                planned_revision=?2,planned_record_count=?3,planned_byte_offset=?4,
                planned_prefix_hash=?5,planned_source_hash=?6,planned_source_bytes=?7,
                planned_markdown_relative_path=?8,planned_markdown_hash=?9,
                planned_archive_git_history=?10,
                planned_completion_reason=?11,planned_fallback_reason=?12
             WHERE id=?1 AND lease_token=?13 AND outcome='processing'",
            params![
                claim.attempt_id,
                plan.revision as i64,
                plan.record_count as i64,
                plan.byte_offset as i64,
                plan.prefix_hash,
                plan.source_hash,
                plan.source_bytes as i64,
                path_text(&plan.markdown_relative_path)?,
                plan.markdown_hash,
                if plan.archive_git_history {
                    1_i64
                } else {
                    0_i64
                },
                plan.completion_reason,
                plan.fallback_reason,
                claim.token
            ],
        )?;
        if changed != 1 {
            return Err(StateError::InvalidState);
        }
        Ok(())
    }

    pub fn complete_attempt(
        &mut self,
        claim: &Claim,
        persisted: &PersistedArchive,
        recovered: bool,
    ) -> Result<(), StateError> {
        let summary_json = serde_json::to_string(&persisted.summary)?;
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_token: Option<String> = transaction.query_row(
            "SELECT claim_token FROM sessions WHERE id=?1",
            [claim.session.database_id],
            |row| row.get(0),
        )?;
        if current_token.as_deref() != Some(&claim.token) {
            return Err(StateError::InvalidState);
        }
        let planned = transaction
            .query_row(
                "SELECT planned_revision,planned_record_count,planned_byte_offset,
                        planned_prefix_hash,planned_source_hash,planned_source_bytes,
                        planned_markdown_relative_path,planned_markdown_hash,
                        planned_archive_git_history,
                        planned_completion_reason,planned_fallback_reason
                 FROM processing_attempts
                 WHERE id=?1 AND lease_token=?2 AND outcome='processing'",
                params![claim.attempt_id, claim.token],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<i64>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                    ))
                },
            )
            .optional()?;
        let Some(planned) = planned else {
            return Err(StateError::InvalidState);
        };
        if planned.0 != Some(persisted.revision as i64)
            || planned.1 != Some(persisted.record_count as i64)
            || planned.2 != Some(persisted.byte_offset as i64)
            || planned.3.as_deref() != Some(&persisted.prefix_hash)
            || planned.4.as_deref() != Some(&persisted.source_hash)
            || planned.5 != Some(persisted.source_bytes as i64)
            || planned.6.as_deref() != Some(path_text(&persisted.markdown_relative_path)?)
            || planned.7.as_deref() != Some(&persisted.markdown_hash)
            || planned.8 != Some(if persisted.archive_git_history { 1 } else { 0 })
            || planned.9.as_deref() != Some(&persisted.completion_reason)
            || planned.10.as_deref() != persisted.fallback_reason.as_deref()
        {
            return Err(StateError::InvalidState);
        }
        transaction.execute(
            "UPDATE sessions SET
                origin_project_identity=COALESCE(origin_project_identity,?2),
                origin_project_component=COALESCE(origin_project_component,?3),
                origin_project_name=COALESCE(origin_project_name,?4),
                origin_repository=COALESCE(origin_repository,?5),
                origin_branch=COALESCE(origin_branch,?6),
                current_summary_revision=?7,
                current_summary_json=?8,current_summary_hash=?9,
                current_markdown_relative_path=?10,current_markdown_hash=?11,
                normalizer_version=?12,source_cursor_records=?13,
                source_cursor_bytes=?14,source_prefix_hash=?15,source_hash=?16,
                source_bytes=?17,source_started_at=?18,source_updated_at=?19,
                source_user_requests=?20,source_assistant_messages=?21,
                source_tool_activities=?22,completion_reason=?23,
                last_fallback_reason=?24,last_error_category=NULL,
                lifecycle_state=CASE WHEN state_generation=?25
                    THEN 'archived' ELSE 'revision-pending' END,
                retry_state=NULL,next_retry_at_ms=NULL,
                claim_token=NULL,claim_started_at_ms=NULL,
                worker_generation=NULL,worker_spawned_at_ms=NULL,
                updated_at_ms=?26
             WHERE id=?1",
            params![
                claim.session.database_id,
                persisted.project.identity,
                persisted.project.component,
                persisted.project.project,
                persisted.project.repository,
                persisted.project.branch,
                persisted.revision as i64,
                summary_json,
                persisted.summary_hash,
                path_text(&persisted.markdown_relative_path)?,
                persisted.markdown_hash,
                persisted.normalizer_version,
                persisted.record_count as i64,
                persisted.byte_offset as i64,
                persisted.prefix_hash,
                persisted.source_hash,
                persisted.source_bytes as i64,
                persisted.started_at,
                persisted.updated_at,
                persisted.user_requests as i64,
                persisted.assistant_messages as i64,
                persisted.tool_activities as i64,
                persisted.completion_reason,
                persisted.fallback_reason,
                claim.state_generation,
                now
            ],
        )?;
        transaction.execute(
            "UPDATE processing_attempts SET
                outcome=?2,recovery_reason=CASE WHEN ?3 THEN 'post-persist-recovery' ELSE NULL END,
                finished_at_ms=?4
             WHERE id=?1 AND lease_token=?5 AND outcome='processing'",
            params![
                claim.attempt_id,
                if recovered { "recovered" } else { "succeeded" },
                recovered,
                now,
                claim.token
            ],
        )?;
        if let Some(reason) = persisted.fallback_reason.as_deref() {
            transaction.execute(
                "INSERT INTO diagnostics(
                    session_id,operation,category,cause_category,recorded_at_ms
                 ) VALUES (?1,'cursor-recovery',?2,NULL,?3)",
                params![claim.session.database_id, reason, now],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn complete_no_change(&mut self, claim: &Claim) -> Result<(), StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE processing_attempts SET outcome='superseded',finished_at_ms=?2
             WHERE id=?1 AND lease_token=?3 AND outcome='processing'",
            params![claim.attempt_id, now, claim.token],
        )?;
        transaction.execute(
            "UPDATE sessions SET
                lifecycle_state=CASE WHEN state_generation=?2
                    THEN CASE WHEN current_summary_revision > 0
                        THEN 'archived' ELSE 'observed' END
                    ELSE CASE WHEN current_summary_revision > 0
                        THEN 'revision-pending' ELSE 'summary-pending' END END,
                retry_state=NULL,next_retry_at_ms=NULL,
                claim_token=NULL,claim_started_at_ms=NULL,
                worker_generation=NULL,worker_spawned_at_ms=NULL,
                last_error_category=NULL,updated_at_ms=?3
             WHERE id=?1 AND claim_token=?4",
            params![
                claim.session.database_id,
                claim.state_generation,
                now,
                claim.token
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn complete_not_archive_worthy(&mut self, claim: &Claim) -> Result<(), StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE processing_attempts SET outcome='succeeded',finished_at_ms=?2
             WHERE id=?1 AND lease_token=?3 AND outcome='processing'",
            params![claim.attempt_id, now, claim.token],
        )?;
        transaction.execute(
            "UPDATE sessions SET lifecycle_state='observed',
                retry_state=NULL,next_retry_at_ms=NULL,
                claim_token=NULL,claim_started_at_ms=NULL,
                worker_generation=NULL,worker_spawned_at_ms=NULL,
                last_error_category=NULL,updated_at_ms=?2
             WHERE id=?1 AND claim_token=?3",
            params![claim.session.database_id, now, claim.token],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn fail_attempt(
        &mut self,
        claim: &Claim,
        category: &str,
        retryable: bool,
    ) -> Result<(), StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM processing_attempts WHERE session_id=?1",
            [claim.session.database_id],
            |row| row.get(0),
        )?;
        let exponent: u32 = attempt_count.clamp(0, 6).try_into().unwrap_or(6);
        let delay = 1_000_i64.saturating_mul(2_i64.saturating_pow(exponent));
        transaction.execute(
            "UPDATE processing_attempts SET
                outcome='failed',error_category=?2,finished_at_ms=?3
             WHERE id=?1 AND lease_token=?4 AND outcome='processing'",
            params![claim.attempt_id, category, now, claim.token],
        )?;
        transaction.execute(
            "UPDATE sessions SET
                lifecycle_state='failed',retry_state=?2,
                next_retry_at_ms=?3,last_error_category=?4,
                claim_token=NULL,claim_started_at_ms=NULL,
                worker_generation=NULL,worker_spawned_at_ms=NULL,
                updated_at_ms=?5
             WHERE id=?1 AND claim_token=?6",
            params![
                claim.session.database_id,
                claim.retry_state,
                if retryable {
                    now.saturating_add(delay)
                } else {
                    -1
                },
                category,
                now,
                claim.token
            ],
        )?;
        transaction.execute(
            "INSERT INTO diagnostics(
                session_id,operation,category,cause_category,recorded_at_ms
             ) VALUES (?1,'archive-worker',?2,NULL,?3)",
            params![claim.session.database_id, category, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn wait_state(
        &self,
        session_id: &str,
    ) -> Result<(WaitState, Option<PathBuf>, Option<String>), StateError> {
        let record = self
            .get_session(session_id)?
            .ok_or(StateError::InvalidState)?;
        let state = match record.lifecycle_state.as_str() {
            "archived" => WaitState::Archived,
            "failed" => WaitState::Failed,
            "observed" if record.current_revision == 0 && record.last_session_end_ms.is_some() => {
                WaitState::NotArchiveWorthy
            }
            _ => WaitState::Pending,
        };
        Ok((
            state,
            record.markdown_relative_path,
            record.last_error_category,
        ))
    }

    pub fn hydrate_session_from_archives(
        &mut self,
        output_directory: &Path,
        session_id: &str,
    ) -> Result<bool, StateError> {
        let records = scan_archives(output_directory, Some(session_id))?;
        let Some(record) = records
            .into_iter()
            .max_by_key(|record| record.archive.summary_revision)
        else {
            return Ok(false);
        };
        self.import_archive_record(&record)?;
        Ok(true)
    }

    pub fn rebuild_from_archives(&mut self, output_directory: &Path) -> Result<usize, StateError> {
        let records = scan_archives(output_directory, None)?;
        let mut latest = std::collections::BTreeMap::<String, OwnedArchive>::new();
        for record in records {
            let session_id = record.archive.session_id.clone();
            if latest
                .get(&session_id)
                .is_none_or(|old| old.archive.summary_revision < record.archive.summary_revision)
            {
                latest.insert(session_id, record);
            }
        }
        for record in latest.values() {
            self.import_archive_record(record)?;
        }
        Ok(latest.len())
    }

    fn import_archive_record(&mut self, record: &OwnedArchive) -> Result<(), StateError> {
        let now = now_ms();
        let summary_json = serde_json::to_string(&record.archive.summary)?;
        let summary_hash = content_hash(summary_json.as_bytes());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = upsert_session(
            &transaction,
            &record.archive.session_id,
            None,
            None,
            "archive-rebuild",
            now,
        )?;
        let current_revision: i64 = transaction.query_row(
            "SELECT current_summary_revision FROM sessions WHERE id=?1",
            [database_id],
            |row| row.get(0),
        )?;
        if current_revision >= record.archive.summary_revision as i64 {
            transaction.commit()?;
            return Ok(());
        }
        let component = if record.archive.project.component.is_empty() {
            record
                .relative_path
                .parent()
                .and_then(Path::file_name)
                .and_then(OsStr::to_str)
                .unwrap_or("project")
                .to_owned()
        } else {
            record.archive.project.component.clone()
        };
        let cursor = record.archive.cursor.as_ref();
        transaction.execute(
            "UPDATE sessions SET
                origin_project_identity=?2,origin_project_component=?3,
                origin_project_name=?4,origin_repository=?5,origin_branch=?6,
                current_summary_revision=?7,current_summary_json=?8,
                current_summary_hash=?9,current_markdown_relative_path=?10,
                current_markdown_hash=?11,normalizer_version=?12,
                source_cursor_records=?13,source_cursor_bytes=?14,
                source_prefix_hash=?15,source_hash=?16,source_bytes=?17,
                source_started_at=?18,source_updated_at=?19,
                completion_reason=?20,last_fallback_reason=?21,
                lifecycle_state=CASE
                    WHEN lifecycle_state='processing' THEN lifecycle_state
                    WHEN current_observation_id IS NOT NULL
                         AND lifecycle_state IN (
                            'summary-pending','revision-pending','interrupted','failed'
                         ) THEN 'revision-pending'
                    ELSE 'archived' END,
                updated_at_ms=?22
             WHERE id=?1",
            params![
                database_id,
                record.archive.project.identity,
                component,
                record.archive.project.project,
                record.archive.project.repository,
                record.archive.project.branch,
                record.archive.summary_revision as i64,
                summary_json,
                summary_hash,
                path_text(&record.relative_path)?,
                record.markdown_hash,
                cursor.map(|cursor| cursor.normalizer_version),
                cursor.map(|cursor| cursor.record_count as i64),
                cursor.map(|cursor| cursor.byte_offset as i64),
                cursor.map(|cursor| cursor.prefix_hash.as_str()),
                record.archive.source_hash,
                cursor.map(|cursor| cursor.source_bytes as i64),
                record.archive.started_at,
                record.archive.updated_at,
                record.archive.completion_reason,
                record
                    .archive
                    .cursor_fallback_reason
                    .as_deref()
                    .or_else(|| (record.archive.schema_version == 1).then_some("legacy-cursor")),
                now
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn stale_known_sessions(&self, cutoff_ms: i64) -> Result<Vec<SessionRecord>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT
                id,source_session_id,origin_cwd,
                origin_project_identity,origin_project_component,origin_project_name,
                origin_repository,origin_branch,transcript_path,lifecycle_state,
                completion_reason,source_end_reason,current_summary_revision,
                current_summary_json,current_summary_hash,current_markdown_relative_path,
                current_markdown_hash,normalizer_version,source_cursor_records,
                source_cursor_bytes,source_prefix_hash,source_hash,source_bytes,
                source_started_at,source_updated_at,source_user_requests,
                source_assistant_messages,source_tool_activities,last_fallback_reason,
                state_generation,active,last_agent_stop_ms,last_session_end_ms,
                last_error_category
             FROM sessions
             WHERE active=1
               AND last_agent_stop_ms IS NOT NULL
               AND updated_at_ms <= ?1
               AND transcript_path IS NOT NULL
               AND origin_cwd IS NOT NULL
               AND (last_session_end_ms IS NULL OR last_session_end_ms < last_agent_stop_ms)",
        )?;
        statement
            .query_map([cutoff_ms], session_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn unresolved_sessions(&self) -> Result<Vec<SessionRecord>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT
                id,source_session_id,origin_cwd,
                origin_project_identity,origin_project_component,origin_project_name,
                origin_repository,origin_branch,transcript_path,lifecycle_state,
                completion_reason,source_end_reason,current_summary_revision,
                current_summary_json,current_summary_hash,current_markdown_relative_path,
                current_markdown_hash,normalizer_version,source_cursor_records,
                source_cursor_bytes,source_prefix_hash,source_hash,source_bytes,
                source_started_at,source_updated_at,source_user_requests,
                source_assistant_messages,source_tool_activities,last_fallback_reason,
                state_generation,active,last_agent_stop_ms,last_session_end_ms,
                last_error_category
             FROM sessions
             WHERE transcript_path IS NULL
               AND lifecycle_state IN (
                    'summary-pending','revision-pending','interrupted','failed'
               )",
        )?;
        statement
            .query_map([], session_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn attach_recovered_transcript(
        &mut self,
        session_id: &str,
        transcript_path: &Path,
        evidence_key: &str,
    ) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = transaction
            .query_row(
                "SELECT id,completion_reason,current_summary_revision
                 FROM sessions
                 WHERE source_kind='copilot-cli' AND source_session_id=?1",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((database_id, completion_reason, revision)) = row else {
            transaction.commit()?;
            return Ok(false);
        };
        let dedupe = dedupe_key(&["recovery-path", session_id, evidence_key]);
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO source_observations(
                session_id,event_kind,event_timestamp_ms,transcript_path,
                completion_reason,dedupe_key,observed_at_ms
             ) VALUES (?1,'recovery-scan',NULL,?2,?3,?4,?5)",
            params![
                database_id,
                path_text(transcript_path)?,
                completion_reason,
                dedupe,
                now
            ],
        )?;
        if inserted == 1 {
            let observation_id = transaction.last_insert_rowid();
            transaction.execute(
                "UPDATE sessions SET
                    current_observation_id=?2,state_generation=state_generation+1,
                    transcript_path=?3,transcript_source='version-pinned-recovery',
                    lifecycle_state=CASE
                        WHEN lifecycle_state='processing' THEN lifecycle_state
                        WHEN ?4 > 0 THEN 'revision-pending'
                        WHEN completion_reason='complete' THEN 'summary-pending'
                        ELSE 'interrupted' END,
                    last_error_category=NULL,updated_at_ms=?5
                 WHERE id=?1",
                params![
                    database_id,
                    observation_id,
                    path_text(transcript_path)?,
                    revision,
                    now
                ],
            )?;
        }
        let reserved = if inserted == 1 {
            reserve_worker_in_transaction(&transaction, database_id, now)?
        } else {
            false
        };
        transaction.commit()?;
        Ok(reserved)
    }

    pub fn mark_recovery_interrupted(
        &mut self,
        session_id: &str,
        transcript_path: &Path,
        origin_cwd: Option<&Path>,
        evidence_key: &str,
    ) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let now = now_ms();
        let dedupe = dedupe_key(&["recovery-scan", session_id, evidence_key]);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = upsert_session(
            &transaction,
            session_id,
            origin_cwd,
            Some(transcript_path),
            "version-pinned-recovery",
            now,
        )?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO source_observations(
                session_id,event_kind,event_timestamp_ms,transcript_path,
                completion_reason,dedupe_key,observed_at_ms
             ) VALUES (?1,'recovery-scan',NULL,?2,'unknown',?3,?4)",
            params![database_id, path_text(transcript_path)?, dedupe, now],
        )?;
        if inserted == 1 {
            let observation_id = transaction.last_insert_rowid();
            transaction.execute(
                "UPDATE sessions SET
                    current_observation_id=?2,state_generation=state_generation+1,
                    transcript_path=?3,transcript_source='version-pinned-recovery',
                    completion_reason='unknown',source_end_reason='unknown',active=0,
                    lifecycle_state=CASE WHEN lifecycle_state='processing'
                        THEN lifecycle_state
                        WHEN current_summary_revision > 0 THEN 'revision-pending'
                        ELSE 'interrupted' END,
                    last_error_category=CASE WHEN origin_cwd IS NULL
                        THEN 'origin-unresolved' ELSE NULL END,
                    updated_at_ms=?4
                 WHERE id=?1",
                params![
                    database_id,
                    observation_id,
                    path_text(transcript_path)?,
                    now
                ],
            )?;
        }
        let reserved = if inserted == 1 {
            reserve_worker_in_transaction(&transaction, database_id, now)?
        } else {
            false
        };
        transaction.commit()?;
        Ok(reserved)
    }
}

#[derive(Debug, Clone)]
struct OwnedArchive {
    relative_path: PathBuf,
    markdown_hash: String,
    archive: ArchivedMarkdown,
}

fn scan_archives(
    output_directory: &Path,
    wanted_session_id: Option<&str>,
) -> Result<Vec<OwnedArchive>, StateError> {
    if !output_directory.exists() {
        return Ok(Vec::new());
    }
    let root = output_directory.canonicalize()?;
    if !root.is_dir() {
        return Err(StateError::InvalidState);
    }
    let mut records = Vec::new();
    for project_entry in fs::read_dir(&root)? {
        let project_entry = project_entry?;
        let project_type = project_entry.file_type()?;
        if project_type.is_symlink() || !project_type.is_dir() {
            continue;
        }
        for entry in fs::read_dir(project_entry.path())? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() || !entry.metadata()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some("md") {
                continue;
            }
            if validate_regular_owned_file(&path).is_err() {
                continue;
            }
            if let Some(wanted) = wanted_session_id {
                if path.file_stem().and_then(OsStr::to_str) != Some(wanted) {
                    continue;
                }
            }
            let bytes = fs::read(&path)?;
            let text = match std::str::from_utf8(&bytes) {
                Ok(text) => text,
                Err(_) => continue,
            };
            let mut archive = match parse_archive_markdown(text) {
                Ok(archive) => archive,
                Err(_) => continue,
            };
            if path.file_stem().and_then(OsStr::to_str) != Some(&archive.session_id) {
                continue;
            }
            let relative_path = path
                .strip_prefix(&root)
                .map_err(|_| StateError::InvalidState)?
                .to_path_buf();
            if archive.project.component.is_empty() {
                archive.project.component = relative_path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(OsStr::to_str)
                    .unwrap_or("project")
                    .to_owned();
            }
            if relative_path
                .parent()
                .and_then(Path::file_name)
                .and_then(OsStr::to_str)
                != Some(&archive.project.component)
            {
                continue;
            }
            records.push(OwnedArchive {
                relative_path,
                markdown_hash: content_hash(&bytes),
                archive,
            });
        }
    }
    Ok(records)
}

pub struct SessionLock {
    _file: File,
}

pub fn try_acquire_session_lock(
    state_directory: &Path,
    session_id: &str,
) -> Result<Option<SessionLock>, StateError> {
    validate_session_id(session_id)?;
    let locks = ensure_directory(&state_directory.join("locks"))?;
    let path = locks.join(format!("{session_id}.lock"));
    let (file, created) = loop {
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                validate_lock_metadata(&path, &metadata)?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)?;
                break (file, false);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&path)
                {
                    Ok(file) => break (file, true),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(StateError::Io(error)),
                }
            }
            Err(error) => return Err(StateError::Io(error)),
        }
    };
    if created {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.sync_all()?;
        File::open(&locks)?.sync_all()?;
    }
    let metadata = file.metadata()?;
    validate_lock_metadata(&path, &metadata)?;
    let lock_result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if lock_result == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(StateError::Io(error));
    }
    let current = fs::symlink_metadata(&path)?;
    validate_lock_metadata(&path, &current)?;
    if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return Err(StateError::InvalidState);
    }
    Ok(Some(SessionLock { _file: file }))
}

fn validate_lock_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), StateError> {
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        let _ = path;
        Err(StateError::InvalidState)
    } else {
        Ok(())
    }
}

fn lease_token(session_id: &str, now: i64) -> String {
    dedupe_key(&[
        "lease",
        session_id,
        &now.to_string(),
        &std::process::id().to_string(),
        &format!("{:?}", SystemTime::now()),
    ])
}

fn acquire_named_lock_with_timeout(
    state_directory: &Path,
    name: &str,
    timeout: Duration,
) -> Result<SessionLock, StateError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(lock) = try_acquire_session_lock(state_directory, name)? {
            return Ok(lock);
        }
        if std::time::Instant::now() >= deadline {
            return Err(StateError::LockBusy);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySessionMetadata {
    version: u32,
    session_id: String,
    transcript_path: String,
    origin_cwd: String,
    agent_stop_timestamp: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyArchiveJob {
    version: u32,
    session_id: String,
    transcript_path: String,
    origin_cwd: String,
    agent_stop_timestamp: u64,
    session_end_timestamp: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
enum LegacyHookResult {
    Archived { relative_path: String },
    NotArchiveWorthy,
    Failed { code: String },
}

#[derive(Debug, Deserialize)]
struct LegacyFailure {
    operation: String,
    code: String,
    #[serde(default)]
    cause_code: Option<String>,
    session_id: Option<String>,
}

pub(crate) fn migrate_legacy_state(
    state: &mut StateStore,
    state_directory: &Path,
    fresh_window: Duration,
) -> Result<(), StateError> {
    let Some(_lock) = try_acquire_session_lock(state_directory, "_legacy")? else {
        return Ok(());
    };
    migrate_legacy_sessions(state, state_directory)?;
    migrate_legacy_pending(state, state_directory, fresh_window)?;
    migrate_legacy_results(state, state_directory)?;
    migrate_legacy_failure(state, state_directory)?;
    Ok(())
}

fn migrate_legacy_sessions(
    state: &mut StateStore,
    state_directory: &Path,
) -> Result<(), StateError> {
    let root = state_directory.join("sessions");
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() || !entry.metadata()?.is_dir() {
            continue;
        }
        let path = entry.path().join("latest.json");
        if !path.exists() {
            continue;
        }
        let bytes = read_owned_legacy(&path)?;
        let metadata: LegacySessionMetadata = match serde_json::from_slice(&bytes) {
            Ok(metadata) => metadata,
            Err(_) => {
                state.record_diagnostic(
                    "legacy-migration",
                    "legacy-state-malformed",
                    None,
                    None,
                )?;
                continue;
            }
        };
        if metadata.version != 1
            || metadata.session_id != entry.file_name().to_string_lossy()
            || validate_session_id(&metadata.session_id).is_err()
            || !Path::new(&metadata.transcript_path).is_absolute()
            || !Path::new(&metadata.origin_cwd).is_absolute()
        {
            state.record_diagnostic(
                "legacy-migration",
                "legacy-state-mismatch",
                None,
                Some(&metadata.session_id),
            )?;
            continue;
        }
        let hash = content_hash(&bytes);
        if !state.legacy_imported(&path, &hash)? {
            state.import_legacy_session(&metadata, &path, &hash)?;
        }
        durable_remove(&path)?;
        let _ = fs::remove_dir(entry.path());
    }
    let _ = fs::remove_dir(root);
    Ok(())
}

fn migrate_legacy_pending(
    state: &mut StateStore,
    state_directory: &Path,
    fresh_window: Duration,
) -> Result<(), StateError> {
    let pending_root = state_directory.join("pending");
    if !pending_root.is_dir() {
        return Ok(());
    }
    let worker_root = state_directory.join("workers");
    for entry in fs::read_dir(&pending_root)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() || !entry.metadata()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(OsStr::to_str) != Some("json") {
            continue;
        }
        let session_id = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
        let worker_path = worker_root.join(format!("{session_id}.lock"));
        if is_fresh(&path, fresh_window)
            || (worker_path.exists() && is_fresh(&worker_path, fresh_window))
        {
            continue;
        }
        let bytes = read_owned_legacy(&path)?;
        let job: LegacyArchiveJob = match serde_json::from_slice(&bytes) {
            Ok(job) => job,
            Err(_) => {
                state.record_diagnostic(
                    "legacy-migration",
                    "legacy-job-malformed",
                    None,
                    Some(session_id),
                )?;
                continue;
            }
        };
        if job.version != 1
            || job.session_id != session_id
            || validate_session_id(&job.session_id).is_err()
            || !Path::new(&job.transcript_path).is_absolute()
            || !Path::new(&job.origin_cwd).is_absolute()
        {
            state.record_diagnostic(
                "legacy-migration",
                "legacy-job-mismatch",
                None,
                Some(session_id),
            )?;
            continue;
        }
        let hash = content_hash(&bytes);
        if !state.legacy_imported(&path, &hash)? {
            state.import_legacy_job(&job, &path, &hash)?;
        }
        durable_remove(&path)?;
        if worker_path.exists() {
            durable_remove(&worker_path)?;
        }
    }
    let _ = fs::remove_dir(pending_root);
    let _ = fs::remove_dir(worker_root);
    Ok(())
}

fn migrate_legacy_results(
    state: &mut StateStore,
    state_directory: &Path,
) -> Result<(), StateError> {
    let root = state_directory.join("results");
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_type()?.is_symlink() || !entry.metadata()?.is_file() {
            continue;
        }
        let path = entry.path();
        let bytes = read_owned_legacy(&path)?;
        let result: LegacyHookResult = match serde_json::from_slice(&bytes) {
            Ok(result) => result,
            Err(_) => {
                state.record_diagnostic(
                    "legacy-migration",
                    "legacy-result-malformed",
                    None,
                    None,
                )?;
                continue;
            }
        };
        let session_id = path.file_stem().and_then(OsStr::to_str);
        match result {
            LegacyHookResult::Archived { relative_path } => {
                let _ = relative_path;
                state.record_diagnostic(
                    "legacy-migration",
                    "legacy-result-requires-markdown",
                    None,
                    session_id,
                )?;
            }
            LegacyHookResult::NotArchiveWorthy => {
                state.record_diagnostic(
                    "legacy-migration",
                    "legacy-not-archive-worthy",
                    None,
                    session_id,
                )?;
            }
            LegacyHookResult::Failed { code } => {
                state.record_diagnostic(
                    "legacy-migration",
                    "legacy-worker-failed",
                    Some(safe_legacy_code(&code)),
                    session_id,
                )?;
            }
        }
        durable_remove(&path)?;
    }
    let _ = fs::remove_dir(root);
    Ok(())
}

fn migrate_legacy_failure(
    state: &mut StateStore,
    state_directory: &Path,
) -> Result<(), StateError> {
    let root = state_directory.join("failures");
    let path = root.join("last.json");
    if !path.exists() {
        return Ok(());
    }
    let bytes = read_owned_legacy(&path)?;
    if let Ok(failure) = serde_json::from_slice::<LegacyFailure>(&bytes) {
        state.record_diagnostic(
            "legacy-migration",
            safe_legacy_code(&failure.code),
            failure.cause_code.as_deref().map(safe_legacy_code),
            failure.session_id.as_deref(),
        )?;
        let _ = failure.operation;
        durable_remove(&path)?;
        let _ = fs::remove_dir(root);
    } else {
        state.record_diagnostic("legacy-migration", "legacy-failure-malformed", None, None)?;
    }
    Ok(())
}

impl StateStore {
    fn legacy_imported(&self, path: &Path, hash: &str) -> Result<bool, StateError> {
        self.connection
            .query_row(
                "SELECT content_hash=?2 FROM legacy_imports WHERE legacy_path=?1",
                params![path_text(path)?, hash],
                |row| row.get(0),
            )
            .optional()
            .map(|value| value.unwrap_or(false))
            .map_err(Into::into)
    }

    fn import_legacy_session(
        &mut self,
        metadata: &LegacySessionMetadata,
        path: &Path,
        hash: &str,
    ) -> Result<(), StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = upsert_session(
            &transaction,
            &metadata.session_id,
            Some(Path::new(&metadata.origin_cwd)),
            Some(Path::new(&metadata.transcript_path)),
            "legacy",
            now,
        )?;
        let dedupe = dedupe_key(&["legacy-agent-stop", hash]);
        transaction.execute(
            "INSERT OR IGNORE INTO source_observations(
                session_id,event_kind,event_timestamp_ms,transcript_path,
                completion_reason,dedupe_key,observed_at_ms
             ) VALUES (?1,'legacy',?2,?3,NULL,?4,?5)",
            params![
                database_id,
                metadata.agent_stop_timestamp as i64,
                metadata.transcript_path,
                dedupe,
                now
            ],
        )?;
        transaction.execute(
            "UPDATE sessions SET
                last_agent_stop_ms=MAX(COALESCE(last_agent_stop_ms,0),?2),
                active=1,updated_at_ms=?3
             WHERE id=?1",
            params![database_id, metadata.agent_stop_timestamp as i64, now],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO legacy_imports(
                legacy_path,content_hash,imported_at_ms
             ) VALUES (?1,?2,?3)",
            params![path_text(path)?, hash, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn import_legacy_job(
        &mut self,
        job: &LegacyArchiveJob,
        path: &Path,
        hash: &str,
    ) -> Result<(), StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = upsert_session(
            &transaction,
            &job.session_id,
            Some(Path::new(&job.origin_cwd)),
            Some(Path::new(&job.transcript_path)),
            "legacy",
            now,
        )?;
        let dedupe = dedupe_key(&["legacy-session-end", hash]);
        transaction.execute(
            "INSERT OR IGNORE INTO source_observations(
                session_id,event_kind,event_timestamp_ms,transcript_path,
                completion_reason,dedupe_key,observed_at_ms
             ) VALUES (?1,'legacy',?2,?3,'complete',?4,?5)",
            params![
                database_id,
                job.session_end_timestamp as i64,
                job.transcript_path,
                dedupe,
                now
            ],
        )?;
        transaction.execute(
            "UPDATE sessions SET
                last_agent_stop_ms=MAX(COALESCE(last_agent_stop_ms,0),?2),
                last_session_end_ms=MAX(COALESCE(last_session_end_ms,0),?3),
                completion_reason='complete',source_end_reason='complete',active=0,
                lifecycle_state=CASE WHEN current_summary_revision > 0
                    THEN 'revision-pending' ELSE 'summary-pending' END,
                state_generation=state_generation+1,
                last_error_category='legacy-worker-interrupted',
                updated_at_ms=?4
             WHERE id=?1",
            params![
                database_id,
                job.agent_stop_timestamp as i64,
                job.session_end_timestamp as i64,
                now
            ],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO legacy_imports(
                legacy_path,content_hash,imported_at_ms
             ) VALUES (?1,?2,?3)",
            params![path_text(path)?, hash, now],
        )?;
        transaction.execute(
            "INSERT INTO diagnostics(
                session_id,operation,category,cause_category,recorded_at_ms
             ) VALUES (?1,'legacy-migration','legacy-worker-interrupted',NULL,?2)",
            params![database_id, now],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn read_owned_legacy(path: &Path) -> Result<Vec<u8>, StateError> {
    validate_regular_owned_file(path)?;
    fs::read(path).map_err(Into::into)
}

fn is_fresh(path: &Path, window: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < window)
}

fn safe_legacy_code(code: &str) -> &str {
    match code {
        "source-failed"
        | "project-failed"
        | "summary-failed"
        | "archive-write-failed"
        | "state-write-failed"
        | "worker-spawn-failed"
        | "job-remove-failed"
        | "worker-lock-remove-failed"
        | "result-write-failed" => code,
        _ => "legacy-failure",
    }
}

pub fn rebuild_database(
    state_directory: &Path,
    output_directory: &Path,
) -> Result<usize, StateError> {
    let Some(_lock) = try_acquire_session_lock(state_directory, "_rebuild")? else {
        return Err(StateError::LockBusy);
    };
    let database = StateStore::database_path(state_directory);
    if database.exists() {
        validate_regular_owned_file(&database)?;
        let backup = state_directory.join(format!(
            "munshi.db.backup-{}-{}",
            now_ms(),
            std::process::id()
        ));
        fs::rename(&database, backup)?;
    }
    for suffix in ["-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", database.to_string_lossy()));
        if path.exists() {
            validate_regular_owned_file(&path)?;
            fs::remove_file(path)?;
        }
    }
    File::open(state_directory)?.sync_all()?;
    let mut state = StateStore::open(state_directory)?;
    state.rebuild_from_archives(output_directory)
}
