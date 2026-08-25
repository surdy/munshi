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

use crate::project::{ProjectIdentity, ProjectOrigin, project_label};
use crate::registration::{
    RegistrationError, durable_remove, ensure_directory, validate_regular_owned_file,
};
use crate::render::{ArchivedMarkdown, content_hash, parse_archive_markdown};
use crate::source::{PreviousSource, SourceHomes, SourceKind, derive_transcript_path};
use crate::summary::StructuredSummary;

const DATABASE_FILE: &str = "munshi.db";
const SCHEMA_VERSION: i64 = 10;
const WORKER_RESERVATION_STALE_MS: i64 = 5_000;

/// The `transcript_source` recorded for a path re-derived from a session's ID through its source's
/// own version-pinned discovery machinery (issue #53), as opposed to one a hook reported
/// (`hook`), the Copilot session-ID fallback supplied at ingest (`version-pinned-fallback`), or the
/// recovery sweep attached (`version-pinned-recovery`). It marks a path Munshi found for a row that
/// had none — a rebuilt row, or one whose recorded transcript moved.
const REDERIVED_TRANSCRIPT_SOURCE: &str = "version-pinned-rederived";

/// Consecutive failures with the same error category on the same `state_generation` after which
/// [`StateStore::fail_attempt`] parks the session permanently (`next_retry_at_ms = -1`) instead
/// of scheduling another retry (issue #38). A parked session keeps its real failure category and
/// is skipped by every plain sweep; only an explicit targeted `retry`, a `--force` retry, or new
/// session activity (which bumps the generation) makes it eligible again.
pub const RETRY_PARK_THRESHOLD: i64 = 5;

/// Escalating per-session retry backoff (issue #38), indexed by the consecutive-failure streak:
/// 10 minutes after the first failure, then 30 minutes, 90 minutes, 4 hours, and a 24-hour cap
/// for any longer streak. With [`RETRY_PARK_THRESHOLD`] at 5 the cap only applies if the
/// threshold is ever raised.
const RETRY_BACKOFF_SCHEDULE_MS: [i64; 5] = [
    10 * 60 * 1_000,
    30 * 60 * 1_000,
    90 * 60 * 1_000,
    4 * 60 * 60 * 1_000,
    24 * 60 * 60 * 1_000,
];

/// The scheduled delay before the next retry after `streak` consecutive failures (issue #38).
fn retry_backoff_ms(streak: i64) -> i64 {
    let index = usize::try_from(streak.saturating_sub(1))
        .unwrap_or(0)
        .min(RETRY_BACKOFF_SCHEDULE_MS.len() - 1);
    RETRY_BACKOFF_SCHEDULE_MS[index]
}

/// The consecutive-failure streak a failure with `category` would record now (issue #38): prior
/// failures extend the streak only when both the category and the `state_generation` match;
/// anything else restarts it at one. Shared by [`StateStore::fail_attempt`] (which records it) and
/// [`StateStore::projected_failure_streak`] (which lets the worker ask, before recording anything,
/// whether this failure would reach the park threshold — the issue #43 placeholder trigger).
fn next_failure_streak(
    prior_streak: i64,
    prior_category: Option<&str>,
    prior_generation: Option<i64>,
    category: &str,
    state_generation: i64,
) -> i64 {
    if prior_category == Some(category) && prior_generation == Some(state_generation) {
        prior_streak.saturating_add(1)
    } else {
        1
    }
}

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
    pub source: SourceKind,
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
    /// When an archive worker last recorded a not-archive-worthy verdict while settling the
    /// row back to `observed` (issue #50). A read-time display signal only: the stored
    /// lifecycle stays `observed` so the issue #49 rescue and the hook requeue paths keep
    /// treating the row as reactivatable when the transcript grows.
    pub not_archive_worthy_at_ms: Option<i64>,
    /// When the operator settled this session as `transcript-lost` (issue #58): its source
    /// transcript was destroyed and judged unrecoverable. A read-time display signal like
    /// `not_archive_worthy_at_ms` — the stored lifecycle stays `observed` so the row reactivates
    /// through the normal paths if the transcript ever reappears at its recorded path.
    pub transcript_lost_at_ms: Option<i64>,
    pub last_error_category: Option<String>,
    /// When the failed session becomes retry-eligible again: `None` is immediately eligible, a
    /// timestamp is a scheduled backoff, and a negative value is a permanent park (issues #38/#44).
    pub next_retry_at_ms: Option<i64>,
    /// Consecutive failed attempts with the same error category on the same `state_generation`
    /// (issue #38). Reset by any successful attempt, a `--force` retry, or a lifted park.
    pub failure_streak: i64,
    /// When Munshi first observed the session, which is not when the session itself began: the
    /// row is created by the first hook or sweep that names it, possibly long after the first
    /// turn. The transcript's own start time is `previous_source.started_at`.
    pub created_at_ms: i64,
    /// When any lifecycle, cursor, or bookkeeping write last touched this row.
    pub updated_at_ms: i64,
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

/// How a placeholder-archived session (issue #43) is left owing a real summary: the archive
/// columns advance exactly as for a real revision, but the session remains `failed` and
/// permanently parked (`next_retry_at_ms = -1`) under `category`, with the failure streak carried
/// so the #38 lift machinery (`munshi retry`, `--force`, new session activity) works unchanged.
/// `cause` distinguishes the deterministic input-capacity class in diagnostics (direction c):
/// `summarizer-rejected` (the summarizer process refused the input) versus `summary-input-limit`
/// (Munshi's own `max_input_bytes` cap), or a generic `summary-failed`.
#[derive(Debug, Clone)]
pub struct PlaceholderPark<'a> {
    pub category: &'a str,
    pub cause: &'a str,
    pub streak: i64,
}

/// One recorded diagnostic: the operation that failed or deferred, its stable category, and the
/// session it named, if any. Every field is a bounded Munshi-authored code or identifier — the
/// table has no free-form message column, so nothing here can carry transcript content.
///
/// `source` and `session_id` are both `None` when the diagnostic named no session, and both go
/// `None` again if that session row is later deleted (the join is left, the foreign key sets
/// `diagnostics.session_id` to NULL).
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub source: Option<SourceKind>,
    pub operation: String,
    pub category: String,
    pub cause_category: Option<String>,
    pub session_id: Option<String>,
    pub recorded_at_ms: i64,
}

/// One recorded processing attempt joined to the session it belongs to: the outcome bookkeeping
/// a read-only caller needs to see what the worker has been doing (issue #56). The attempt's
/// `planned_*` columns are deliberately absent — they describe archive content, and this view is
/// consumed over the CLI JSON boundary (ADR 0007) by callers that must never learn about content.
#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub source: SourceKind,
    pub session_id: String,
    /// The session's display project label, derived by [`project_label`]. `None` when the
    /// session recorded no origin evidence at all.
    pub project: Option<String>,
    pub outcome: String,
    pub error_category: Option<String>,
    pub started_at_ms: i64,
    /// `None` while the attempt still holds its lease; a recovery sweep settles abandoned
    /// leases, so a long-unfinished attempt is a stalled worker rather than a live one.
    pub finished_at_ms: Option<i64>,
}

/// The Munshi-owned remote delivery record for one logical session and one Notesmith sink.
///
/// This is rebuildable operational state (ADR 0002/0004): it holds the persisted remote note
/// identifier, the last successfully delivered summary revision, and bounded retry/dead-letter
/// bookkeeping. It never gates local archival, and a delivery failure never mutates a session's
/// archival lifecycle.
#[derive(Debug, Clone)]
pub struct DeliveryRecord {
    pub session_database_id: i64,
    pub source: SourceKind,
    pub session_id: String,
    pub endpoint: String,
    pub vault: String,
    pub note_path: Option<String>,
    pub delivered_revision: Option<u64>,
    pub delivered_summary_hash: Option<String>,
    pub remote_hash: Option<String>,
    /// The correlated Notesmith history commit (issue #9) that preserves this delivered revision,
    /// when versioned delivery is active. `None` for latest-only (non-versioned) deliveries.
    pub history_commit: Option<String>,
    pub delivery_state: String,
    pub attempts: u32,
    pub next_attempt_at_ms: Option<i64>,
    pub last_error_category: Option<String>,
    pub updated_at_ms: i64,
}

/// One mirrored harness-memory directory's sync state for one Notesmith sink (issue #59).
#[derive(Debug, Clone)]
pub struct MemorySyncRecord {
    pub slug: String,
    pub endpoint: String,
    pub vault: String,
    /// The canonical machine label the mirror was routed under (provenance; routing itself
    /// always derives from the configured label at run time).
    pub machine: String,
    /// sha256 of the per-file content manifest at the last successful sync, or `None` when the
    /// directory has never synced.
    pub manifest_hash: Option<String>,
    pub synced_revision: u64,
    pub file_count: u64,
    /// The correlated Notesmith history commit that preserves this synced revision.
    pub history_commit: Option<String>,
    pub sync_state: String,
    pub attempts: u32,
    pub next_attempt_at_ms: Option<i64>,
    pub last_error_category: Option<String>,
    pub updated_at_ms: i64,
}

/// A successful memory sync result to persist for one directory's sink row.
#[derive(Debug, Clone)]
pub struct MemorySyncSuccess {
    pub manifest_hash: String,
    pub file_count: u64,
    pub history_commit: Option<String>,
}

/// A successful delivery result to persist for one session's sink row.
#[derive(Debug, Clone)]
pub struct DeliverySuccess {
    pub note_path: String,
    pub delivered_revision: u64,
    pub delivered_summary_hash: String,
    pub remote_hash: Option<String>,
    /// The correlated remote history commit that preserved this revision, when versioned.
    pub history_commit: Option<String>,
}

/// The Munshi-owned Patwari archive-upload record for one logical session and one archive server.
///
/// This is rebuildable operational state (ADR 0004/0009) that mirrors [`DeliveryRecord`]: it holds
/// the last successfully uploaded summary revision and its snapshot id, the in-flight capture
/// identity used to resume an interrupted upload, and bounded retry/dead-letter bookkeeping. It
/// never gates local archival, and an upload failure never mutates a session's archival lifecycle.
#[derive(Debug, Clone)]
pub struct ArchiveUploadRecord {
    pub session_database_id: i64,
    pub source: SourceKind,
    pub session_id: String,
    pub endpoint: String,
    /// The capture id of the current in-flight snapshot attempt (a fresh UUID per distinct
    /// revision, reused verbatim on retry of the same one).
    pub capture_id: Option<String>,
    /// The summary revision the current `capture_id`/`captured_at`/`upload_id` were minted for.
    pub capture_revision: Option<u64>,
    /// The `captured_at` timestamp fixed when the current capture was minted, held stable across
    /// retries so the canonical manifest — and therefore the capture idempotency — does not drift.
    pub captured_at: Option<String>,
    /// The server upload id of the current attempt, persisted so a crashed run can resume it.
    pub upload_id: Option<String>,
    pub uploaded_revision: Option<u64>,
    pub uploaded_summary_hash: Option<String>,
    /// The content hash of the markdown the recorded snapshot uploaded. Keyed on alongside the
    /// revision and summary hash so a cursor-only re-render (same revision and summary, fresh
    /// markdown) re-uploads instead of being treated as an idempotent no-op. `None` on a row
    /// written before this was recorded: what markdown it uploaded is unknown, not known-current.
    pub uploaded_markdown_hash: Option<String>,
    pub snapshot_id: Option<String>,
    /// Patwari's own session id for the uploaded snapshot (the receipt's `session_id`, issue #76) —
    /// the identity `restore --session` filters on, distinct from `snapshot_id` and from the harness
    /// `source_session_id`. `None` on a row written before it was recorded, or one whose only uploads
    /// predate schema 10, until `archive-upload reconcile` backfills it.
    pub patwari_session_id: Option<String>,
    /// The artifact logical paths the recorded snapshot contained, in the canonical order they
    /// were uploaded in (issue #47). `None` on a row written before the ledger recorded them: what
    /// that snapshot contained is unknown, not known-complete.
    pub uploaded_artifact_paths: Option<Vec<String>>,
    pub upload_state: String,
    pub attempts: u32,
    pub next_attempt_at_ms: Option<i64>,
    pub last_error_category: Option<String>,
    pub updated_at_ms: i64,
    /// Lifetime bytes actually transferred to this endpoint for this session across every
    /// successful upload (issue #65) — the receipt's `upload_transfer_bytes`, accumulated. Blob
    /// dedup makes a fully deduplicated re-upload contribute 0. 0 on pre-#65 rows.
    pub transfer_bytes_total: u64,
    /// The latest uploaded snapshot's total stored (compressed) bytes; `None` before any
    /// measured upload.
    pub last_stored_bytes: Option<u64>,
    /// The latest uploaded snapshot's total original (uncompressed) bytes; `None` before any
    /// measured upload.
    pub last_original_bytes: Option<u64>,
}

/// The resolved capture identity for one upload attempt returned by
/// [`StateStore::prepare_archive_capture`].
#[derive(Debug, Clone)]
pub struct CapturePrep {
    pub capture_id: String,
    pub captured_at: String,
    /// A previously created server upload id to resume, when the same capture is being retried.
    pub resume_upload_id: Option<String>,
}

/// A successful archive-upload result to persist for one session's server row.
#[derive(Debug, Clone)]
pub struct ArchiveUploadSuccess {
    pub uploaded_revision: u64,
    pub uploaded_summary_hash: String,
    /// The content hash of the markdown this upload carried, recorded so a later cursor-only
    /// re-render (same revision and summary, fresh markdown) is not mistaken for an already-uploaded
    /// snapshot. `None` only when the session record carried no markdown hash.
    pub uploaded_markdown_hash: Option<String>,
    pub snapshot_id: String,
    /// Patwari's own session id from the upload receipt (issue #76) — the identity `restore --session`
    /// filters on, recorded so `sessions`/`archive-upload status` can surface the id restore needs.
    pub patwari_session_id: String,
    /// Every artifact logical path the uploaded snapshot contained (issue #47), so a later run can
    /// tell a self-contained snapshot from one that predates the full-snapshot guarantee.
    pub uploaded_artifact_paths: Vec<String>,
    /// The receipt's `upload_transfer_bytes` for this upload — bytes actually moved on the wire,
    /// 0 when every artifact deduplicated (issue #65). Accumulated into the row's lifetime total.
    pub transfer_bytes: u64,
    /// The receipt's snapshot-wide stored (compressed) byte total.
    pub total_stored_bytes: u64,
    /// The receipt's snapshot-wide original (uncompressed) byte total.
    pub total_original_bytes: u64,
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
    source_kind: String,
}

impl StateStore {
    pub fn open(state_directory: &Path) -> Result<Self, StateError> {
        Self::open_for_source(state_directory, SourceKind::Copilot)
    }

    /// Open the shared operational state store scoped to a specific capturing harness.
    ///
    /// The SQLite schema is source-neutral: sessions are keyed by
    /// `(source_kind, source_session_id)`, so multiple harnesses can share one database.
    /// Session-scoped queries are bound to this store's `source_kind`, keeping Copilot's
    /// hook-driven behavior byte-for-byte identical while letting other adapters drive the
    /// same lifecycle state machine.
    pub fn open_for_source(state_directory: &Path, source: SourceKind) -> Result<Self, StateError> {
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
        let mut store = Self {
            connection,
            source_kind: source.agent_label().to_owned(),
        };
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
        if current < 3 {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "CREATE TABLE deliveries (
                    id INTEGER PRIMARY KEY,
                    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    endpoint TEXT NOT NULL,
                    vault TEXT NOT NULL,
                    note_path TEXT,
                    delivered_revision INTEGER,
                    delivered_summary_hash TEXT,
                    remote_hash TEXT,
                    delivery_state TEXT NOT NULL CHECK (delivery_state IN (
                        'pending','delivered','failed','dead-letter'
                    )),
                    attempts INTEGER NOT NULL DEFAULT 0,
                    next_attempt_at_ms INTEGER,
                    last_error_category TEXT,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL,
                    UNIQUE(session_id, endpoint, vault)
                 );
                 CREATE INDEX deliveries_state_idx
                    ON deliveries(delivery_state, next_attempt_at_ms);",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![3, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 3)?;
            transaction.commit()?;
        }
        if current < 4 {
            // Issue #9: versioned Notesmith delivery. A delivery may be `blocked` when the remote
            // vault cannot preserve correlated revision history, and each delivered revision may
            // record the correlated remote history commit (`history_commit`). SQLite cannot alter a
            // CHECK constraint in place, so the table is rebuilt preserving every existing row.
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "CREATE TABLE deliveries_new (
                    id INTEGER PRIMARY KEY,
                    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    endpoint TEXT NOT NULL,
                    vault TEXT NOT NULL,
                    note_path TEXT,
                    delivered_revision INTEGER,
                    delivered_summary_hash TEXT,
                    remote_hash TEXT,
                    history_commit TEXT,
                    delivery_state TEXT NOT NULL CHECK (delivery_state IN (
                        'pending','delivered','failed','dead-letter','blocked'
                    )),
                    attempts INTEGER NOT NULL DEFAULT 0,
                    next_attempt_at_ms INTEGER,
                    last_error_category TEXT,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL,
                    UNIQUE(session_id, endpoint, vault)
                 );
                 INSERT INTO deliveries_new (
                    id,session_id,endpoint,vault,note_path,delivered_revision,
                    delivered_summary_hash,remote_hash,history_commit,delivery_state,attempts,
                    next_attempt_at_ms,last_error_category,created_at_ms,updated_at_ms
                 )
                 SELECT
                    id,session_id,endpoint,vault,note_path,delivered_revision,
                    delivered_summary_hash,remote_hash,NULL,delivery_state,attempts,
                    next_attempt_at_ms,last_error_category,created_at_ms,updated_at_ms
                 FROM deliveries;
                 DROP TABLE deliveries;
                 ALTER TABLE deliveries_new RENAME TO deliveries;
                 CREATE INDEX deliveries_state_idx
                    ON deliveries(delivery_state, next_attempt_at_ms);",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![4, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 4)?;
            transaction.commit()?;
        }
        if current < 5 {
            // Issue #19 (ADR 0009): rebuildable operational state tracking one Patwari archive
            // upload per (session, endpoint), mirroring `deliveries`. Upload runs strictly
            // downstream of local archival and never gates it. `capture_id`/`captured_at` are the
            // idempotency identity of the current snapshot attempt: a fresh pair is minted per
            // distinct revision and reused verbatim on retry so an interrupted upload resumes
            // rather than duplicates (Patwari keys idempotency on the client + capture id). The
            // persistent client UUID lives in durable `config.json`, not here, so it survives a
            // rebuild of this database.
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "CREATE TABLE archive_uploads (
                    id INTEGER PRIMARY KEY,
                    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    endpoint TEXT NOT NULL,
                    capture_id TEXT,
                    capture_revision INTEGER,
                    captured_at TEXT,
                    upload_id TEXT,
                    uploaded_revision INTEGER,
                    uploaded_summary_hash TEXT,
                    snapshot_id TEXT,
                    upload_state TEXT NOT NULL CHECK (upload_state IN (
                        'pending','uploaded','failed','dead-letter'
                    )),
                    attempts INTEGER NOT NULL DEFAULT 0,
                    next_attempt_at_ms INTEGER,
                    last_error_category TEXT,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL,
                    UNIQUE(session_id, endpoint)
                 );
                 CREATE INDEX archive_uploads_state_idx
                    ON archive_uploads(upload_state, next_attempt_at_ms);",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![5, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 5)?;
            transaction.commit()?;
        }
        if current < 6 {
            // Issue #47: record which artifact logical paths the uploaded snapshot actually
            // contained, so the client can tell a self-contained snapshot (ADR 0009 — `summary.md`
            // plus `transcript.jsonl` plus any extracted outputs) from the summary-only snapshots
            // an earlier client produced for sessions whose transcript it could not read. A row
            // written before this migration carries NULL: what it uploaded is unknown, which
            // `archive-upload backfill` treats as "not proven self-contained" and re-verifies once.
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "ALTER TABLE archive_uploads ADD COLUMN uploaded_artifact_paths TEXT;",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![6, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 6)?;
            transaction.commit()?;
        }
        if current < 7 {
            // Issue #59: rebuildable operational state for mirroring harness auto-memory
            // directories into a Notesmith vault, one row per (memory directory, endpoint,
            // vault). `manifest_hash` is the sha256 of the per-file content manifest at the last
            // successful sync — the change detector that lets an unchanged directory no-op
            // without ever contacting the sink. Rows are not session-scoped (memory belongs to a
            // project directory, not a session), so unlike `deliveries` there is no `sessions`
            // join and the table is shared across source scopes. The state machine mirrors
            // `deliveries` (issue #9 semantics for `blocked`).
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "CREATE TABLE memory_sync (
                    id INTEGER PRIMARY KEY,
                    slug TEXT NOT NULL,
                    endpoint TEXT NOT NULL,
                    vault TEXT NOT NULL,
                    machine TEXT NOT NULL,
                    manifest_hash TEXT,
                    synced_revision INTEGER NOT NULL DEFAULT 0,
                    file_count INTEGER NOT NULL DEFAULT 0,
                    history_commit TEXT,
                    sync_state TEXT NOT NULL CHECK (sync_state IN (
                        'pending','synced','failed','dead-letter','blocked'
                    )),
                    attempts INTEGER NOT NULL DEFAULT 0,
                    next_attempt_at_ms INTEGER,
                    last_error_category TEXT,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL,
                    UNIQUE(slug, endpoint, vault)
                 );
                 CREATE INDEX memory_sync_state_idx
                    ON memory_sync(sync_state, next_attempt_at_ms);",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![7, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 7)?;
            transaction.commit()?;
        }
        if current < 8 {
            // Issue #65: persist the transfer accounting Patwari's upload receipts already
            // report, so "real transfer-volume pain" (the deferral trigger of issue #24) is a
            // measured number instead of an estimate. `transfer_bytes_total` accumulates the
            // bytes actually moved on the wire across every successful upload of this row
            // (0 when blob dedup absorbed everything); `last_stored_bytes`/`last_original_bytes`
            // are the latest snapshot's compressed and original totals — latest, not summed,
            // because successive revisions of one session overlap almost entirely. Rows written
            // before this migration carry 0/NULL: their transfers were never measured.
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "ALTER TABLE archive_uploads
                    ADD COLUMN transfer_bytes_total INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE archive_uploads ADD COLUMN last_stored_bytes INTEGER;
                 ALTER TABLE archive_uploads ADD COLUMN last_original_bytes INTEGER;",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![8, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 8)?;
            transaction.commit()?;
        }
        if current < 9 {
            // Issue #73: a cursor-only re-render (a new source cursor at the same revision and
            // summary, hooks.rs `cursor_only`) rewrites the markdown but leaves `uploaded_revision`
            // and `uploaded_summary_hash` unchanged, so the upload idempotency check never re-fires
            // and the archive's newest snapshot permanently lags the local markdown — which makes
            // `restore` refuse the session. Record the uploaded markdown hash so the check can key
            // on it too. Rows written before this migration carry NULL: their uploaded markdown is
            // unknown, so they re-upload once (blob dedup makes that cheap) and record it after.
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(
                "ALTER TABLE archive_uploads ADD COLUMN uploaded_markdown_hash TEXT;",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![9, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 9)?;
            transaction.commit()?;
        }
        if current < 10 {
            // Issue #76: record Patwari's own session id (the receipt's `session_id`, distinct from
            // the snapshot id and from the harness `source_session_id`) alongside each uploaded
            // snapshot, because `munshi restore --session` filters on exactly that id and nothing a
            // user reaches — `munshi sessions`, `archive-upload status` — surfaced it, leaving the
            // harness id the listings *do* show a dead end against restore. Rows written before this
            // migration carry NULL until a later upload records it or `archive-upload reconcile`
            // backfills them from the server's snapshot listing.
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction
                .execute_batch("ALTER TABLE archive_uploads ADD COLUMN patwari_session_id TEXT;")?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![10, now_ms()],
            )?;
            transaction.pragma_update(None, "user_version", 10)?;
            transaction.commit()?;
        }
        let user_version: i64 =
            self.connection
                .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if user_version > SCHEMA_VERSION {
            return Err(StateError::NewerSchema);
        }
        ensure_processing_attempts_git_history_column(&self.connection)?;
        ensure_session_failure_streak_columns(&self.connection)?;
        ensure_session_project_origin_column(&self.connection)?;
        ensure_session_not_archive_worthy_column(&self.connection)?;
        ensure_session_transcript_lost_column(&self.connection)?;
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
            &self.source_kind,
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
            &self.source_kind,
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
            reserve_worker_in_transaction(&transaction, database_id, now, false)?
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
                    last_error_category,source_kind,next_retry_at_ms,failure_streak,
                    origin_project_origin,not_archive_worthy_at_ms,
                    created_at_ms,updated_at_ms,transcript_lost_at_ms
                 FROM sessions
                 WHERE source_kind=?2 AND source_session_id=?1",
                params![session_id, self.source_kind],
                session_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>, StateError> {
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
                last_error_category,source_kind,next_retry_at_ms,failure_streak,
                origin_project_origin,not_archive_worthy_at_ms,
                created_at_ms,updated_at_ms,transcript_lost_at_ms
             FROM sessions
             ORDER BY updated_at_ms DESC,id DESC",
        )?;
        statement
            .query_map([], session_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Resolves the internal session row id for this store's source scope.
    fn session_database_id(&self, session_id: &str) -> Result<Option<i64>, StateError> {
        validate_session_id(session_id)?;
        self.connection
            .query_row(
                "SELECT id FROM sessions WHERE source_kind=?2 AND source_session_id=?1",
                params![session_id, self.source_kind],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Ensures a `pending` delivery row exists for one session and sink, returning the current
    /// record. The row is created idempotently on the `(session, endpoint, vault)` key so repeated
    /// calls never duplicate a delivery target. Returns `None` when the session is unknown in this
    /// source scope.
    pub fn ensure_delivery_target(
        &mut self,
        session_id: &str,
        endpoint: &str,
        vault: &str,
    ) -> Result<Option<DeliveryRecord>, StateError> {
        let Some(database_id) = self.session_database_id(session_id)? else {
            return Ok(None);
        };
        let now = now_ms();
        self.connection.execute(
            "INSERT INTO deliveries(
                session_id,endpoint,vault,delivery_state,attempts,created_at_ms,updated_at_ms
             ) VALUES (?1,?2,?3,'pending',0,?4,?4)
             ON CONFLICT(session_id,endpoint,vault) DO NOTHING",
            params![database_id, endpoint, vault, now],
        )?;
        self.get_delivery(session_id, endpoint, vault)
    }

    /// Reads the delivery record for one session and sink in this source scope, if any.
    pub fn get_delivery(
        &self,
        session_id: &str,
        endpoint: &str,
        vault: &str,
    ) -> Result<Option<DeliveryRecord>, StateError> {
        let Some(database_id) = self.session_database_id(session_id)? else {
            return Ok(None);
        };
        self.connection
            .query_row(
                "SELECT
                    d.session_id,s.source_kind,s.source_session_id,d.endpoint,d.vault,
                    d.note_path,d.delivered_revision,d.delivered_summary_hash,d.remote_hash,
                    d.history_commit,d.delivery_state,d.attempts,d.next_attempt_at_ms,
                    d.last_error_category,d.updated_at_ms
                 FROM deliveries d JOIN sessions s ON s.id=d.session_id
                 WHERE d.session_id=?1 AND d.endpoint=?2 AND d.vault=?3",
                params![database_id, endpoint, vault],
                delivery_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Lists every recorded delivery across all sources, most recently updated first.
    pub fn list_deliveries(&self) -> Result<Vec<DeliveryRecord>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT
                d.session_id,s.source_kind,s.source_session_id,d.endpoint,d.vault,
                d.note_path,d.delivered_revision,d.delivered_summary_hash,d.remote_hash,
                d.history_commit,d.delivery_state,d.attempts,d.next_attempt_at_ms,
                d.last_error_category,d.updated_at_ms
             FROM deliveries d JOIN sessions s ON s.id=d.session_id
             ORDER BY d.updated_at_ms DESC,d.id DESC",
        )?;
        statement
            .query_map([], delivery_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Records a successful delivery: stores the persisted remote note path, the delivered summary
    /// revision and hash, clears retry bookkeeping, and marks the row `delivered`.
    pub fn record_delivery_success(
        &mut self,
        session_id: &str,
        endpoint: &str,
        vault: &str,
        success: &DeliverySuccess,
    ) -> Result<(), StateError> {
        let Some(database_id) = self.session_database_id(session_id)? else {
            return Err(StateError::InvalidState);
        };
        let now = now_ms();
        self.connection.execute(
            "UPDATE deliveries SET
                note_path=?4,delivered_revision=?5,delivered_summary_hash=?6,remote_hash=?7,
                history_commit=?8,delivery_state='delivered',attempts=0,next_attempt_at_ms=NULL,
                last_error_category=NULL,updated_at_ms=?9
             WHERE session_id=?1 AND endpoint=?2 AND vault=?3",
            params![
                database_id,
                endpoint,
                vault,
                success.note_path,
                i64::try_from(success.delivered_revision).unwrap_or(i64::MAX),
                success.delivered_summary_hash,
                success.remote_hash,
                success.history_commit,
                now,
            ],
        )?;
        Ok(())
    }

    /// Records a delivery blocked by a missing remote revision-history capability (issue #9).
    ///
    /// Blocking is a configuration gate, not a transient failure: the attempt count is left
    /// untouched (so a block never advances toward a dead letter), retry bookkeeping is cleared,
    /// and the row is marked `blocked` with an actionable `category`. Local archival is never
    /// affected. A blocked row becomes deliverable again once the capability is present and the
    /// delivery is retried or a new revision is archived.
    pub fn record_delivery_blocked(
        &mut self,
        session_id: &str,
        endpoint: &str,
        vault: &str,
        category: &str,
    ) -> Result<DeliveryRecord, StateError> {
        let Some(database_id) = self.session_database_id(session_id)? else {
            return Err(StateError::InvalidState);
        };
        let now = now_ms();
        self.connection.execute(
            "UPDATE deliveries SET
                delivery_state='blocked',next_attempt_at_ms=NULL,
                last_error_category=?4,updated_at_ms=?5
             WHERE session_id=?1 AND endpoint=?2 AND vault=?3",
            params![database_id, endpoint, vault, category, now],
        )?;
        self.get_delivery(session_id, endpoint, vault)?
            .ok_or(StateError::InvalidState)
    }

    /// Records a failed delivery attempt. Increments the attempt count, and either schedules the
    /// next attempt at `next_attempt_at_ms` (bounded backoff) while under `max_attempts`, or parks
    /// the row as a `dead-letter` once attempts are exhausted. Never touches archival state.
    pub fn record_delivery_failure(
        &mut self,
        session_id: &str,
        endpoint: &str,
        vault: &str,
        category: &str,
        max_attempts: u32,
        next_attempt_at_ms: i64,
    ) -> Result<DeliveryRecord, StateError> {
        let Some(_) = self.session_database_id(session_id)? else {
            return Err(StateError::InvalidState);
        };
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempts: u32 = transaction
            .query_row(
                "SELECT attempts FROM deliveries d
                 JOIN sessions s ON s.id=d.session_id
                 WHERE s.source_kind=?2 AND s.source_session_id=?1
                   AND d.endpoint=?3 AND d.vault=?4",
                params![session_id, self.source_kind, endpoint, vault],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            .try_into()
            .unwrap_or(u32::MAX);
        let attempts = attempts.saturating_add(1);
        let exhausted = attempts >= max_attempts;
        let (state, next_attempt) = if exhausted {
            ("dead-letter", None)
        } else {
            ("failed", Some(next_attempt_at_ms))
        };
        transaction.execute(
            "UPDATE deliveries SET
                delivery_state=?5,attempts=?6,next_attempt_at_ms=?7,
                last_error_category=?8,updated_at_ms=?9
             WHERE session_id=(
                    SELECT id FROM sessions WHERE source_kind=?2 AND source_session_id=?1
                 ) AND endpoint=?3 AND vault=?4",
            params![
                session_id,
                self.source_kind,
                endpoint,
                vault,
                state,
                attempts,
                next_attempt,
                category,
                now,
            ],
        )?;
        transaction.commit()?;
        self.get_delivery(session_id, endpoint, vault)?
            .ok_or(StateError::InvalidState)
    }

    /// Resets a delivery row to `pending` so a subsequent delivery attempt is eligible, clearing
    /// backoff. With `force`, a `dead-letter` row is revived and its attempt count reset.
    pub fn reset_delivery_for_retry(
        &mut self,
        session_id: &str,
        endpoint: &str,
        vault: &str,
        force: bool,
    ) -> Result<Option<DeliveryRecord>, StateError> {
        let Some(current) = self.get_delivery(session_id, endpoint, vault)? else {
            return Ok(None);
        };
        if current.delivery_state == "dead-letter" && !force {
            return Ok(Some(current));
        }
        let now = now_ms();
        let reset_attempts = force || current.delivery_state == "dead-letter";
        self.connection.execute(
            "UPDATE deliveries SET
                delivery_state='pending',next_attempt_at_ms=NULL,
                attempts=CASE WHEN ?5 THEN 0 ELSE attempts END,updated_at_ms=?6
             WHERE session_id=(
                    SELECT id FROM sessions WHERE source_kind=?2 AND source_session_id=?1
                 ) AND endpoint=?3 AND vault=?4",
            params![
                session_id,
                self.source_kind,
                endpoint,
                vault,
                reset_attempts,
                now
            ],
        )?;
        self.get_delivery(session_id, endpoint, vault)
    }

    /// Ensures a memory-sync row exists for one memory directory and sink, returning the current
    /// record. Created idempotently on the `(slug, endpoint, vault)` key; the recorded machine
    /// label follows the configured one so a relabeled machine's provenance stays truthful.
    pub fn ensure_memory_sync_target(
        &mut self,
        slug: &str,
        endpoint: &str,
        vault: &str,
        machine: &str,
    ) -> Result<MemorySyncRecord, StateError> {
        let now = now_ms();
        self.connection.execute(
            "INSERT INTO memory_sync(
                slug,endpoint,vault,machine,sync_state,attempts,created_at_ms,updated_at_ms
             ) VALUES (?1,?2,?3,?4,'pending',0,?5,?5)
             ON CONFLICT(slug,endpoint,vault) DO UPDATE SET machine=?4",
            params![slug, endpoint, vault, machine, now],
        )?;
        self.get_memory_sync(slug, endpoint, vault)?
            .ok_or(StateError::InvalidState)
    }

    /// Reads the memory-sync record for one directory and sink, if any.
    pub fn get_memory_sync(
        &self,
        slug: &str,
        endpoint: &str,
        vault: &str,
    ) -> Result<Option<MemorySyncRecord>, StateError> {
        self.connection
            .query_row(
                "SELECT slug,endpoint,vault,machine,manifest_hash,synced_revision,file_count,
                        history_commit,sync_state,attempts,next_attempt_at_ms,
                        last_error_category,updated_at_ms
                 FROM memory_sync WHERE slug=?1 AND endpoint=?2 AND vault=?3",
                params![slug, endpoint, vault],
                memory_sync_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Lists every recorded memory sync, most recently updated first.
    pub fn list_memory_sync(&self) -> Result<Vec<MemorySyncRecord>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT slug,endpoint,vault,machine,manifest_hash,synced_revision,file_count,
                    history_commit,sync_state,attempts,next_attempt_at_ms,
                    last_error_category,updated_at_ms
             FROM memory_sync ORDER BY updated_at_ms DESC,id DESC",
        )?;
        statement
            .query_map([], memory_sync_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Records a successful memory sync: stores the synced manifest, advances the revision
    /// counter, clears retry bookkeeping, and marks the row `synced`.
    pub fn record_memory_sync_success(
        &mut self,
        slug: &str,
        endpoint: &str,
        vault: &str,
        success: &MemorySyncSuccess,
    ) -> Result<MemorySyncRecord, StateError> {
        let now = now_ms();
        self.connection.execute(
            "UPDATE memory_sync SET
                manifest_hash=?4,file_count=?5,history_commit=?6,
                synced_revision=synced_revision+1,sync_state='synced',attempts=0,
                next_attempt_at_ms=NULL,last_error_category=NULL,updated_at_ms=?7
             WHERE slug=?1 AND endpoint=?2 AND vault=?3",
            params![
                slug,
                endpoint,
                vault,
                success.manifest_hash,
                i64::try_from(success.file_count).unwrap_or(i64::MAX),
                success.history_commit,
                now,
            ],
        )?;
        self.get_memory_sync(slug, endpoint, vault)?
            .ok_or(StateError::InvalidState)
    }

    /// Records a memory sync blocked by a missing remote revision-history capability. Same
    /// semantics as [`Self::record_delivery_blocked`]: a configuration gate, never an attempt.
    pub fn record_memory_sync_blocked(
        &mut self,
        slug: &str,
        endpoint: &str,
        vault: &str,
        category: &str,
    ) -> Result<MemorySyncRecord, StateError> {
        let now = now_ms();
        self.connection.execute(
            "UPDATE memory_sync SET
                sync_state='blocked',next_attempt_at_ms=NULL,
                last_error_category=?4,updated_at_ms=?5
             WHERE slug=?1 AND endpoint=?2 AND vault=?3",
            params![slug, endpoint, vault, category, now],
        )?;
        self.get_memory_sync(slug, endpoint, vault)?
            .ok_or(StateError::InvalidState)
    }

    /// Records a failed memory sync attempt with the same bounded-backoff/dead-letter semantics
    /// as [`Self::record_delivery_failure`].
    pub fn record_memory_sync_failure(
        &mut self,
        slug: &str,
        endpoint: &str,
        vault: &str,
        category: &str,
        max_attempts: u32,
        next_attempt_at_ms: i64,
    ) -> Result<MemorySyncRecord, StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempts: u32 = transaction
            .query_row(
                "SELECT attempts FROM memory_sync
                 WHERE slug=?1 AND endpoint=?2 AND vault=?3",
                params![slug, endpoint, vault],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            .try_into()
            .unwrap_or(u32::MAX);
        let attempts = attempts.saturating_add(1);
        let exhausted = attempts >= max_attempts;
        let (state, next_attempt) = if exhausted {
            ("dead-letter", None)
        } else {
            ("failed", Some(next_attempt_at_ms))
        };
        transaction.execute(
            "UPDATE memory_sync SET
                sync_state=?4,attempts=?5,next_attempt_at_ms=?6,
                last_error_category=?7,updated_at_ms=?8
             WHERE slug=?1 AND endpoint=?2 AND vault=?3",
            params![
                slug,
                endpoint,
                vault,
                state,
                attempts,
                next_attempt,
                category,
                now,
            ],
        )?;
        transaction.commit()?;
        self.get_memory_sync(slug, endpoint, vault)?
            .ok_or(StateError::InvalidState)
    }

    /// Resets a memory-sync row to `pending` so a subsequent sync attempt is eligible, clearing
    /// backoff. With `force`, a `dead-letter` row is revived and its attempt count reset.
    pub fn reset_memory_sync_for_retry(
        &mut self,
        slug: &str,
        endpoint: &str,
        vault: &str,
        force: bool,
    ) -> Result<Option<MemorySyncRecord>, StateError> {
        let Some(current) = self.get_memory_sync(slug, endpoint, vault)? else {
            return Ok(None);
        };
        if current.sync_state == "dead-letter" && !force {
            return Ok(Some(current));
        }
        let now = now_ms();
        let reset_attempts = force || current.sync_state == "dead-letter";
        self.connection.execute(
            "UPDATE memory_sync SET
                sync_state='pending',next_attempt_at_ms=NULL,
                attempts=CASE WHEN ?4 THEN 0 ELSE attempts END,updated_at_ms=?5
             WHERE slug=?1 AND endpoint=?2 AND vault=?3",
            params![slug, endpoint, vault, reset_attempts, now],
        )?;
        self.get_memory_sync(slug, endpoint, vault)
    }

    /// Ensures a `pending` archive-upload row exists for one session and server, returning the
    /// current record. Created idempotently on the `(session, endpoint)` key. Returns `None` when
    /// the session is unknown in this source scope.
    pub fn ensure_archive_upload_target(
        &mut self,
        session_id: &str,
        endpoint: &str,
    ) -> Result<Option<ArchiveUploadRecord>, StateError> {
        let Some(database_id) = self.session_database_id(session_id)? else {
            return Ok(None);
        };
        let now = now_ms();
        self.connection.execute(
            "INSERT INTO archive_uploads(
                session_id,endpoint,upload_state,attempts,created_at_ms,updated_at_ms
             ) VALUES (?1,?2,'pending',0,?3,?3)
             ON CONFLICT(session_id,endpoint) DO NOTHING",
            params![database_id, endpoint, now],
        )?;
        self.get_archive_upload(session_id, endpoint)
    }

    /// Reads the archive-upload record for one session and server in this source scope, if any.
    pub fn get_archive_upload(
        &self,
        session_id: &str,
        endpoint: &str,
    ) -> Result<Option<ArchiveUploadRecord>, StateError> {
        let Some(database_id) = self.session_database_id(session_id)? else {
            return Ok(None);
        };
        self.connection
            .query_row(
                "SELECT
                    au.session_id,s.source_kind,s.source_session_id,au.endpoint,
                    au.capture_id,au.capture_revision,au.captured_at,au.upload_id,
                    au.uploaded_revision,au.uploaded_summary_hash,au.snapshot_id,
                    au.uploaded_artifact_paths,
                    au.upload_state,au.attempts,au.next_attempt_at_ms,au.last_error_category,
                    au.updated_at_ms,
                    au.transfer_bytes_total,au.last_stored_bytes,au.last_original_bytes,
                    au.uploaded_markdown_hash,au.patwari_session_id
                 FROM archive_uploads au JOIN sessions s ON s.id=au.session_id
                 WHERE au.session_id=?1 AND au.endpoint=?2",
                params![database_id, endpoint],
                archive_upload_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Lists every recorded archive upload across all sources, most recently updated first.
    pub fn list_archive_uploads(&self) -> Result<Vec<ArchiveUploadRecord>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT
                au.session_id,s.source_kind,s.source_session_id,au.endpoint,
                au.capture_id,au.capture_revision,au.captured_at,au.upload_id,
                au.uploaded_revision,au.uploaded_summary_hash,au.snapshot_id,
                au.uploaded_artifact_paths,
                au.upload_state,au.attempts,au.next_attempt_at_ms,au.last_error_category,
                au.updated_at_ms,
                au.transfer_bytes_total,au.last_stored_bytes,au.last_original_bytes,
                au.uploaded_markdown_hash,au.patwari_session_id
             FROM archive_uploads au JOIN sessions s ON s.id=au.session_id
             ORDER BY au.updated_at_ms DESC,au.id DESC",
        )?;
        statement
            .query_map([], archive_upload_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Lists archive-upload rows eligible for a retry attempt across all sources, driven by
    /// `archive_uploads_state_idx`: rows in `pending` or `failed` (never `uploaded` or `dead-letter`)
    /// whose backoff has elapsed. A `failed` row is due once `next_attempt_at_ms <= now`; a `pending`
    /// row carries no schedule and is always due. Because a `failed` row that exhausts its bounded
    /// attempts becomes `dead-letter`, the state filter alone excludes exhausted uploads. Ordered
    /// oldest-scheduled first so a backlog drains fairly, capped at `limit`.
    pub fn eligible_archive_uploads(
        &self,
        now: i64,
        limit: usize,
    ) -> Result<Vec<ArchiveUploadRecord>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT
                au.session_id,s.source_kind,s.source_session_id,au.endpoint,
                au.capture_id,au.capture_revision,au.captured_at,au.upload_id,
                au.uploaded_revision,au.uploaded_summary_hash,au.snapshot_id,
                au.uploaded_artifact_paths,
                au.upload_state,au.attempts,au.next_attempt_at_ms,au.last_error_category,
                au.updated_at_ms,
                au.transfer_bytes_total,au.last_stored_bytes,au.last_original_bytes,
                au.uploaded_markdown_hash,au.patwari_session_id
             FROM archive_uploads au JOIN sessions s ON s.id=au.session_id
             WHERE au.upload_state IN ('pending','failed')
               AND (au.next_attempt_at_ms IS NULL OR au.next_attempt_at_ms <= ?1)
             ORDER BY COALESCE(au.next_attempt_at_ms, 0),au.id
             LIMIT ?2",
        )?;
        statement
            .query_map(params![now, limit as i64], archive_upload_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Resolves the capture identity for uploading `revision`, minting a fresh `capture_id` and
    /// `captured_at` for a distinct snapshot attempt or reusing the persisted pair (and any server
    /// upload id) when retrying the same one.
    ///
    /// A distinct attempt is any that targets a different `revision` than the persisted capture, or
    /// one whose prior attempt already terminally uploaded; those mint fresh identity so a changed
    /// snapshot can never collide with the previous capture id. An in-flight or failed attempt for
    /// the same revision reuses the exact stored `capture_id`/`captured_at`/`upload_id`, so an
    /// interrupted upload resumes rather than creating a duplicate capture.
    pub fn prepare_archive_capture(
        &mut self,
        session_id: &str,
        endpoint: &str,
        revision: u64,
        fresh_capture_id: &str,
        fresh_captured_at: &str,
    ) -> Result<CapturePrep, StateError> {
        self.ensure_archive_upload_target(session_id, endpoint)?;
        let Some(database_id) = self.session_database_id(session_id)? else {
            return Err(StateError::InvalidState);
        };
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<CaptureRow> = transaction
            .query_row(
                "SELECT capture_id,capture_revision,captured_at,upload_id,upload_state
                     FROM archive_uploads WHERE session_id=?1 AND endpoint=?2",
                params![database_id, endpoint],
                |row| {
                    Ok(CaptureRow {
                        capture_id: row.get(0)?,
                        capture_revision: row.get(1)?,
                        captured_at: row.get(2)?,
                        upload_id: row.get(3)?,
                        upload_state: row.get(4)?,
                    })
                },
            )
            .optional()?;
        let revision_i64 = i64::try_from(revision).unwrap_or(i64::MAX);
        let reuse = existing.and_then(|row| match row {
            CaptureRow {
                capture_id: Some(capture_id),
                capture_revision: Some(capture_revision),
                captured_at: Some(captured_at),
                upload_id,
                upload_state,
            } if capture_revision == revision_i64
                && (upload_state == "pending" || upload_state == "failed") =>
            {
                Some(CapturePrep {
                    capture_id,
                    captured_at,
                    resume_upload_id: upload_id,
                })
            }
            _ => None,
        });
        let prep = if let Some(prep) = reuse {
            prep
        } else {
            transaction.execute(
                "UPDATE archive_uploads SET
                    capture_id=?3,capture_revision=?4,captured_at=?5,upload_id=NULL,
                    upload_state='pending',updated_at_ms=?6
                 WHERE session_id=?1 AND endpoint=?2",
                params![
                    database_id,
                    endpoint,
                    fresh_capture_id,
                    revision_i64,
                    fresh_captured_at,
                    now,
                ],
            )?;
            CapturePrep {
                capture_id: fresh_capture_id.to_owned(),
                captured_at: fresh_captured_at.to_owned(),
                resume_upload_id: None,
            }
        };
        transaction.commit()?;
        Ok(prep)
    }

    /// Persists the server upload id for the current capture so a crashed run can resume it.
    pub fn record_archive_upload_id(
        &mut self,
        session_id: &str,
        endpoint: &str,
        upload_id: &str,
    ) -> Result<(), StateError> {
        let Some(database_id) = self.session_database_id(session_id)? else {
            return Err(StateError::InvalidState);
        };
        self.connection.execute(
            "UPDATE archive_uploads SET upload_id=?3,updated_at_ms=?4
             WHERE session_id=?1 AND endpoint=?2",
            params![database_id, endpoint, upload_id, now_ms()],
        )?;
        Ok(())
    }

    /// Records a successful archive upload: stores the uploaded revision, its summary hash, the
    /// server snapshot id, and the artifact set the snapshot contained (issue #47), clears retry
    /// bookkeeping, and marks the row `uploaded`.
    pub fn record_archive_upload_success(
        &mut self,
        session_id: &str,
        endpoint: &str,
        success: &ArchiveUploadSuccess,
    ) -> Result<(), StateError> {
        let Some(database_id) = self.session_database_id(session_id)? else {
            return Err(StateError::InvalidState);
        };
        self.connection.execute(
            "UPDATE archive_uploads SET
                uploaded_revision=?3,uploaded_summary_hash=?4,snapshot_id=?5,
                uploaded_artifact_paths=?6,
                transfer_bytes_total=transfer_bytes_total+?8,
                last_stored_bytes=?9,last_original_bytes=?10,
                uploaded_markdown_hash=?11,patwari_session_id=?12,
                upload_state='uploaded',attempts=0,next_attempt_at_ms=NULL,
                last_error_category=NULL,updated_at_ms=?7
             WHERE session_id=?1 AND endpoint=?2",
            params![
                database_id,
                endpoint,
                i64::try_from(success.uploaded_revision).unwrap_or(i64::MAX),
                success.uploaded_summary_hash,
                success.snapshot_id,
                join_artifact_paths(&success.uploaded_artifact_paths),
                now_ms(),
                i64::try_from(success.transfer_bytes).unwrap_or(i64::MAX),
                i64::try_from(success.total_stored_bytes).unwrap_or(i64::MAX),
                i64::try_from(success.total_original_bytes).unwrap_or(i64::MAX),
                success.uploaded_markdown_hash,
                success.patwari_session_id,
            ],
        )?;
        Ok(())
    }

    /// Backfills a row's Patwari session id from its snapshot id, only while it is still missing
    /// (issue #76 `archive-upload reconcile`). Scoped to one endpoint, because the snapshot→session
    /// mapping comes from one server's listing. Never overwrites a recorded id, and does not disturb
    /// `updated_at_ms` — filling metadata is not an upload event. Returns whether a row changed.
    pub fn backfill_patwari_session_id(
        &mut self,
        endpoint: &str,
        snapshot_id: &str,
        patwari_session_id: &str,
    ) -> Result<bool, StateError> {
        let updated = self.connection.execute(
            "UPDATE archive_uploads SET patwari_session_id=?3
             WHERE endpoint=?1 AND snapshot_id=?2 AND patwari_session_id IS NULL",
            params![endpoint, snapshot_id, patwari_session_id],
        )?;
        Ok(updated > 0)
    }

    /// Resets a row whose recorded snapshot has disappeared from Patwari. The row may since have
    /// moved from `uploaded` to `failed` or `dead-letter`; its snapshot still needs replacement.
    /// Stale server identities are cleared so the next backfill mints a fresh capture, while
    /// successful-upload hashes and accounting remain as historical ledger data.
    pub fn repair_missing_archive_upload(
        &mut self,
        session_id: &str,
        endpoint: &str,
        snapshot_id: &str,
    ) -> Result<bool, StateError> {
        let updated = self.connection.execute(
            "UPDATE archive_uploads SET
                capture_id=NULL,capture_revision=NULL,captured_at=NULL,upload_id=NULL,
                snapshot_id=NULL,patwari_session_id=NULL,upload_state='pending',
                attempts=0,next_attempt_at_ms=NULL,last_error_category=NULL,updated_at_ms=?5
             WHERE session_id=(
                    SELECT id FROM sessions WHERE source_kind=?2 AND source_session_id=?1
                 ) AND endpoint=?3 AND snapshot_id=?4",
            params![
                session_id,
                self.source_kind,
                endpoint,
                snapshot_id,
                now_ms()
            ],
        )?;
        Ok(updated > 0)
    }

    /// Records a failed archive-upload attempt. Increments the attempt count, then either schedules
    /// the next attempt (bounded backoff) while under `max_attempts` or parks the row as a
    /// `dead-letter` once attempts are exhausted. Never touches archival state.
    pub fn record_archive_upload_failure(
        &mut self,
        session_id: &str,
        endpoint: &str,
        category: &str,
        max_attempts: u32,
        next_attempt_at_ms: i64,
    ) -> Result<ArchiveUploadRecord, StateError> {
        if self.session_database_id(session_id)?.is_none() {
            return Err(StateError::InvalidState);
        }
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempts: u32 = transaction
            .query_row(
                "SELECT attempts FROM archive_uploads au
                 JOIN sessions s ON s.id=au.session_id
                 WHERE s.source_kind=?2 AND s.source_session_id=?1 AND au.endpoint=?3",
                params![session_id, self.source_kind, endpoint],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            .try_into()
            .unwrap_or(u32::MAX);
        let attempts = attempts.saturating_add(1);
        let exhausted = attempts >= max_attempts;
        let (state, next_attempt) = if exhausted {
            ("dead-letter", None)
        } else {
            ("failed", Some(next_attempt_at_ms))
        };
        transaction.execute(
            "UPDATE archive_uploads SET
                upload_state=?4,attempts=?5,next_attempt_at_ms=?6,
                last_error_category=?7,updated_at_ms=?8
             WHERE session_id=(
                    SELECT id FROM sessions WHERE source_kind=?2 AND source_session_id=?1
                 ) AND endpoint=?3",
            params![
                session_id,
                self.source_kind,
                endpoint,
                state,
                attempts,
                next_attempt,
                category,
                now,
            ],
        )?;
        transaction.commit()?;
        self.get_archive_upload(session_id, endpoint)?
            .ok_or(StateError::InvalidState)
    }

    /// Resets an archive-upload row to `pending` so a subsequent attempt is eligible, clearing
    /// backoff. With `force`, a `dead-letter` row is revived and its attempt count reset.
    pub fn reset_archive_upload_for_retry(
        &mut self,
        session_id: &str,
        endpoint: &str,
        force: bool,
    ) -> Result<Option<ArchiveUploadRecord>, StateError> {
        let Some(current) = self.get_archive_upload(session_id, endpoint)? else {
            return Ok(None);
        };
        if current.upload_state == "dead-letter" && !force {
            return Ok(Some(current));
        }
        let reset_attempts = force || current.upload_state == "dead-letter";
        self.connection.execute(
            "UPDATE archive_uploads SET
                upload_state='pending',next_attempt_at_ms=NULL,
                attempts=CASE WHEN ?4 THEN 0 ELSE attempts END,updated_at_ms=?5
             WHERE session_id=(
                    SELECT id FROM sessions WHERE source_kind=?2 AND source_session_id=?1
                 ) AND endpoint=?3",
            params![
                session_id,
                self.source_kind,
                endpoint,
                reset_attempts,
                now_ms()
            ],
        )?;
        self.get_archive_upload(session_id, endpoint)
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
                         WHERE source_kind=?2 AND source_session_id=?1",
                        params![session_id, self.source_kind],
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
             WHERE source_kind=?4 AND source_session_id=?1",
            params![session_id, category, now, self.source_kind],
        )?;
        self.record_diagnostic("archive-worker", category, None, Some(session_id))
    }

    pub fn latest_diagnostic(&self) -> Result<Option<Diagnostic>, StateError> {
        self.connection
            .query_row(
                &format!("{DIAGNOSTIC_COLUMNS} ORDER BY d.id DESC LIMIT 1"),
                [],
                diagnostic_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Lists recorded diagnostics newest first, at most `limit` of them. Spans every source kind
    /// — a diagnostic is an operator-facing record of what Munshi did, not session-scoped work —
    /// and reads nothing the `status` contract does not already expose.
    pub fn list_diagnostics(&self, limit: usize) -> Result<Vec<Diagnostic>, StateError> {
        let mut statement = self.connection.prepare(&format!(
            "{DIAGNOSTIC_COLUMNS} ORDER BY d.recorded_at_ms DESC,d.id DESC LIMIT ?1"
        ))?;
        statement
            .query_map([bounded_limit(limit)], diagnostic_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Counts every recorded diagnostic, so a caller can tell a truncated
    /// [`Self::list_diagnostics`] tail from the whole table.
    pub fn count_diagnostics(&self) -> Result<usize, StateError> {
        let total: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM diagnostics", [], |row| row.get(0))?;
        Ok(usize::try_from(total).unwrap_or_default())
    }

    /// Lists recorded processing attempts most recently finished first, with attempts still
    /// holding a lease last, at most `limit` of them. Spans every source kind, like the recovery
    /// work lists: the attempt log describes worker activity, not one harness's sessions.
    ///
    /// `since_ms` keeps only attempts whose activity — the finish instant, or the start instant
    /// while unfinished — is at or after that Unix millisecond, so a caller polling a window
    /// never has to page back through the whole history to find it.
    pub fn list_processing_attempts(
        &self,
        since_ms: Option<i64>,
        limit: usize,
    ) -> Result<Vec<AttemptRecord>, StateError> {
        let mut statement = self.connection.prepare(&format!(
            "{ATTEMPT_COLUMNS} {ATTEMPT_SINCE_FILTER}
             ORDER BY a.finished_at_ms IS NULL,a.finished_at_ms DESC,a.id DESC
             LIMIT ?2"
        ))?;
        statement
            .query_map(params![since_ms, bounded_limit(limit)], attempt_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Counts the attempts [`Self::list_processing_attempts`] matches before `limit` truncates
    /// them, under the same `since_ms` window.
    pub fn count_processing_attempts(&self, since_ms: Option<i64>) -> Result<usize, StateError> {
        let total: i64 = self.connection.query_row(
            &format!(
                "SELECT COUNT(*) FROM processing_attempts a
                 JOIN sessions s ON s.id=a.session_id {ATTEMPT_SINCE_FILTER}"
            ),
            params![since_ms],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(total).unwrap_or_default())
    }
}

/// The diagnostics tail projection shared by [`StateStore::latest_diagnostic`] and
/// [`StateStore::list_diagnostics`]; the left join keeps diagnostics that named no session (or
/// whose session row is gone) instead of dropping them.
const DIAGNOSTIC_COLUMNS: &str = "SELECT s.source_kind,d.operation,d.category,d.cause_category,
            s.source_session_id,d.recorded_at_ms
     FROM diagnostics d
     LEFT JOIN sessions s ON s.id=d.session_id";

/// The attempt projection shared by the attempt list and its count. The inner join is deliberate:
/// an attempt whose session row is gone has no identity to report.
const ATTEMPT_COLUMNS: &str = "SELECT s.source_kind,s.source_session_id,s.origin_project_name,
            s.origin_project_component,s.origin_cwd,a.outcome,a.error_category,
            a.started_at_ms,a.finished_at_ms
     FROM processing_attempts a
     JOIN sessions s ON s.id=a.session_id";

/// The `since_ms` window as `?1`: a NULL bound selects everything.
const ATTEMPT_SINCE_FILTER: &str =
    "WHERE ?1 IS NULL OR COALESCE(a.finished_at_ms,a.started_at_ms) >= ?1";

/// Clamps a caller-supplied row limit into SQLite's signed range; a limit too large to bind is
/// indistinguishable from no limit at all.
fn bounded_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

fn diagnostic_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Diagnostic> {
    Ok(Diagnostic {
        source: row
            .get::<_, Option<String>>(0)?
            .as_deref()
            .and_then(SourceKind::from_agent_label),
        operation: row.get(1)?,
        category: row.get(2)?,
        cause_category: row.get(3)?,
        session_id: row.get(4)?,
        recorded_at_ms: row.get(5)?,
    })
}

fn attempt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttemptRecord> {
    Ok(AttemptRecord {
        source: SourceKind::from_agent_label(&row.get::<_, String>(0)?).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other("unknown source kind")),
            )
        })?,
        session_id: row.get(1)?,
        project: project_label(
            row.get::<_, Option<String>>(2)?.as_deref(),
            row.get::<_, Option<String>>(3)?.as_deref(),
            row.get::<_, Option<String>>(4)?.as_deref().map(Path::new),
        ),
        outcome: row.get(5)?,
        error_category: row.get(6)?,
        started_at_ms: row.get(7)?,
        finished_at_ms: row.get(8)?,
    })
}

fn upsert_session(
    transaction: &Transaction<'_>,
    source_kind: &str,
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
         ) VALUES (?6,?1,?2,?3,?4,'observed',?5,?5)
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
            now,
            source_kind
        ],
    )?;
    transaction
        .query_row(
            "SELECT id FROM sessions
             WHERE source_kind=?2 AND source_session_id=?1",
            params![session_id, source_kind],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn reserve_worker_in_transaction(
    transaction: &Transaction<'_>,
    database_id: i64,
    now: i64,
    force: bool,
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
                OR ?4
                OR next_retry_at_ms IS NULL
                OR (next_retry_at_ms >= 0 AND next_retry_at_ms <= ?2))",
        params![database_id, now, now - WORKER_RESERVATION_STALE_MS, force],
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
            // NULL means live: rows written before issue #40 carry no marker.
            origin: match row.get::<_, Option<String>>(37)?.as_deref() {
                Some("recorded") => ProjectOrigin::Recorded,
                _ => ProjectOrigin::Live,
            },
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
        source: SourceKind::from_agent_label(&row.get::<_, String>(34)?).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                34,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other("unknown source kind")),
            )
        })?,
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
        not_archive_worthy_at_ms: row.get(38)?,
        transcript_lost_at_ms: row.get(41)?,
        last_error_category: row.get(33)?,
        next_retry_at_ms: row.get(35)?,
        failure_streak: row.get(36)?,
        created_at_ms: row.get(39)?,
        updated_at_ms: row.get(40)?,
    })
}

fn delivery_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeliveryRecord> {
    Ok(DeliveryRecord {
        session_database_id: row.get(0)?,
        source: SourceKind::from_agent_label(&row.get::<_, String>(1)?).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other("unknown source kind")),
            )
        })?,
        session_id: row.get(2)?,
        endpoint: row.get(3)?,
        vault: row.get(4)?,
        note_path: row.get(5)?,
        delivered_revision: row
            .get::<_, Option<i64>>(6)?
            .map(|value| u64::try_from(value).unwrap_or_default()),
        delivered_summary_hash: row.get(7)?,
        remote_hash: row.get(8)?,
        history_commit: row.get(9)?,
        delivery_state: row.get(10)?,
        attempts: row.get::<_, i64>(11)?.try_into().unwrap_or_default(),
        next_attempt_at_ms: row.get(12)?,
        last_error_category: row.get(13)?,
        updated_at_ms: row.get(14)?,
    })
}

fn memory_sync_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemorySyncRecord> {
    Ok(MemorySyncRecord {
        slug: row.get(0)?,
        endpoint: row.get(1)?,
        vault: row.get(2)?,
        machine: row.get(3)?,
        manifest_hash: row.get(4)?,
        synced_revision: row.get::<_, i64>(5)?.try_into().unwrap_or_default(),
        file_count: row.get::<_, i64>(6)?.try_into().unwrap_or_default(),
        history_commit: row.get(7)?,
        sync_state: row.get(8)?,
        attempts: row.get::<_, i64>(9)?.try_into().unwrap_or_default(),
        next_attempt_at_ms: row.get(10)?,
        last_error_category: row.get(11)?,
        updated_at_ms: row.get(12)?,
    })
}

/// The in-flight capture columns read by [`StateStore::prepare_archive_capture`].
struct CaptureRow {
    capture_id: Option<String>,
    capture_revision: Option<i64>,
    captured_at: Option<String>,
    upload_id: Option<String>,
    upload_state: String,
}

fn archive_upload_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArchiveUploadRecord> {
    let revision = |index: usize| -> rusqlite::Result<Option<u64>> {
        Ok(row
            .get::<_, Option<i64>>(index)?
            .map(|value| u64::try_from(value).unwrap_or_default()))
    };
    Ok(ArchiveUploadRecord {
        session_database_id: row.get(0)?,
        source: SourceKind::from_agent_label(&row.get::<_, String>(1)?).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other("unknown source kind")),
            )
        })?,
        session_id: row.get(2)?,
        endpoint: row.get(3)?,
        capture_id: row.get(4)?,
        capture_revision: revision(5)?,
        captured_at: row.get(6)?,
        upload_id: row.get(7)?,
        uploaded_revision: revision(8)?,
        uploaded_summary_hash: row.get(9)?,
        uploaded_markdown_hash: row.get(20)?,
        patwari_session_id: row.get(21)?,
        snapshot_id: row.get(10)?,
        uploaded_artifact_paths: row
            .get::<_, Option<String>>(11)?
            .map(|joined| split_artifact_paths(&joined)),
        upload_state: row.get(12)?,
        attempts: row.get::<_, i64>(13)?.try_into().unwrap_or_default(),
        next_attempt_at_ms: row.get(14)?,
        last_error_category: row.get(15)?,
        updated_at_ms: row.get(16)?,
        transfer_bytes_total: row.get::<_, i64>(17)?.try_into().unwrap_or_default(),
        last_stored_bytes: revision(18)?,
        last_original_bytes: revision(19)?,
    })
}

/// The stored artifact-path list separator. Logical paths are reserved names, content-addressed
/// `outputs/<sha256>` paths, or allowlist-derived `sidecar/<relative>` paths (ADR 0009, issue
/// #23), so none can contain a newline.
const ARTIFACT_PATH_SEPARATOR: &str = "\n";

/// Joins an uploaded snapshot's artifact logical paths into the single ledger column.
fn join_artifact_paths(paths: &[String]) -> String {
    paths.join(ARTIFACT_PATH_SEPARATOR)
}

/// Splits a stored artifact-path list back into logical paths. An empty stored value yields an
/// empty list — a recorded snapshot with no artifacts, which is distinct from an unrecorded one.
fn split_artifact_paths(joined: &str) -> Vec<String> {
    joined
        .split(ARTIFACT_PATH_SEPARATOR)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect()
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

/// Additive columns tracking the per-session consecutive-failure streak that drives the
/// escalating retry backoff and the repeat-failure park (issue #38), added the same way as the
/// git-history column above so existing databases upgrade in place without a schema rebuild.
fn ensure_session_failure_streak_columns(connection: &Connection) -> Result<(), StateError> {
    let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
    let existing: std::collections::BTreeSet<String> = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    drop(statement);
    for (name, definition) in [
        (
            "failure_streak",
            "failure_streak INTEGER NOT NULL DEFAULT 0",
        ),
        ("failure_streak_category", "failure_streak_category TEXT"),
        (
            "failure_streak_generation",
            "failure_streak_generation INTEGER",
        ),
    ] {
        if !existing.contains(name) {
            connection.execute(&format!("ALTER TABLE sessions ADD COLUMN {definition}"), [])?;
        }
    }
    Ok(())
}

/// Additive column recording how a session's project identity was derived (issue #40):
/// `'recorded'` when it came from transcript-recorded origin evidence after the origin
/// directory disappeared, NULL for a live-resolved identity (which keeps every pre-#40 row
/// meaning "live" without a rewrite). Added the same way as the failure-streak columns so
/// existing databases upgrade in place without a schema rebuild.
fn ensure_session_project_origin_column(connection: &Connection) -> Result<(), StateError> {
    let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
    let existing: std::collections::BTreeSet<String> = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    drop(statement);
    if !existing.contains("origin_project_origin") {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN origin_project_origin TEXT",
            [],
        )?;
    }
    Ok(())
}

/// Additive column recording when an archive worker last judged a session's content not
/// archive-worthy while settling it back to `observed` (issue #50), added the same way as
/// the columns above so existing databases upgrade in place without a schema rebuild.
///
/// The stored lifecycle deliberately stays `observed` — `not-archive-worthy` is a read-time
/// label, exactly like the hook-path verdict derived from `last_session_end_ms` — so every
/// reactivation path keeps working: hook ingestion requeues the row on new evidence, and
/// the issue #49 rescue (which keys on `observed` with no session-end) rescues it again
/// whenever the transcript has since changed. Only the display moves.
///
/// When the column is first added, rows already settled by the issue #49 rescue are
/// backfilled: `observed`, inactive, revision 0, no session-end verdict, with a recorded
/// succeeded attempt — the only shape a worker's not-archive-worthy verdict leaves behind
/// (an archive verdict always raises the revision above 0). The verdict time is the row's
/// `updated_at_ms`, written by that verdict.
fn ensure_session_not_archive_worthy_column(connection: &Connection) -> Result<(), StateError> {
    let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
    let existing: std::collections::BTreeSet<String> = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    drop(statement);
    if !existing.contains("not_archive_worthy_at_ms") {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN not_archive_worthy_at_ms INTEGER",
            [],
        )?;
        connection.execute(
            "UPDATE sessions SET not_archive_worthy_at_ms=updated_at_ms
             WHERE lifecycle_state='observed'
               AND active=0
               AND current_summary_revision=0
               AND last_session_end_ms IS NULL
               AND EXISTS (
                    SELECT 1 FROM processing_attempts
                    WHERE session_id=sessions.id AND outcome='succeeded'
               )",
            [],
        )?;
    }
    Ok(())
}

/// Additive column recording when the operator settled a session as `transcript-lost`
/// (issue #58): its transcript was destroyed and judged unrecoverable. Added the same way as
/// the verdict column above so existing databases upgrade in place. No backfill: declaring
/// data lost is an explicit operator action (`munshi settle-lost`), never inferred.
fn ensure_session_transcript_lost_column(connection: &Connection) -> Result<(), StateError> {
    let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
    let existing: std::collections::BTreeSet<String> = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    drop(statement);
    if !existing.contains("transcript_lost_at_ms") {
        connection.execute(
            "ALTER TABLE sessions ADD COLUMN transcript_lost_at_ms INTEGER",
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
        let database_id = transaction
            .query_row(
                "SELECT id FROM sessions
                 WHERE source_kind=?2 AND source_session_id=?1",
                params![session_id, self.source_kind],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let reserved_database_id = match database_id {
            Some(database_id)
                if reserve_worker_in_transaction(&transaction, database_id, now, force)? =>
            {
                Some(database_id)
            }
            _ => None,
        };
        if force {
            if let Some(database_id) = reserved_database_id {
                // A forced retry clears any park or scheduled backoff and restarts the
                // consecutive-failure escalation from scratch (issue #38).
                transaction.execute(
                    "UPDATE sessions SET next_retry_at_ms=NULL,failure_streak=0,
                        failure_streak_category=NULL,failure_streak_generation=NULL
                     WHERE id=?1 AND lifecycle_state='failed'",
                    [database_id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(reserved_database_id.is_some())
    }

    /// Failed sessions parked permanently (`next_retry_at_ms < 0`) under the `source-oversized`
    /// verdict — or the pre-#57 lumped `source-failed` code older rows still carry — across
    /// every source scope. That verdict is config-dependent — it records that the transcript
    /// exceeded the source limit configured at failure time — so callers re-check the listed
    /// transcripts against the currently configured limit and lift stale parks (issue #44).
    /// Sessions without a recorded transcript path are omitted: there is nothing to re-measure.
    pub fn parked_source_limit_sessions(
        &self,
    ) -> Result<Vec<(SourceKind, String, PathBuf)>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT source_kind,source_session_id,transcript_path
             FROM sessions
             WHERE lifecycle_state='failed'
               AND next_retry_at_ms<0
               AND last_error_category IN ('source-oversized','source-failed')
               AND transcript_path IS NOT NULL
             ORDER BY updated_at_ms,id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(source_kind, session_id, path)| {
                // Skip any row with an unrecognized source label rather than mis-routing it.
                SourceKind::from_agent_label(&source_kind)
                    .map(|source| (source, session_id, PathBuf::from(path)))
            })
            .collect())
    }

    /// Lifts a permanent `source-oversized` park (or a pre-#57 `source-failed` one) so the
    /// normal claim gates re-evaluate the session. The caller must first verify the transcript
    /// fits the currently configured source limit; this only clears the frozen verdict
    /// (`next_retry_at_ms < 0`) recorded under a superseded configuration (issue #44) and never
    /// touches sessions failed for other reasons. The failure streak resets with the park
    /// (issue #38) so a lifted session gets a fresh escalation window rather than inheriting
    /// the failed-era streak.
    pub fn lift_source_limit_park(&mut self, session_id: &str) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let changed = self.connection.execute(
            "UPDATE sessions SET next_retry_at_ms=NULL,failure_streak=0,
                failure_streak_category=NULL,failure_streak_generation=NULL,updated_at_ms=?3
             WHERE source_kind=?2 AND source_session_id=?1
               AND lifecycle_state='failed'
               AND next_retry_at_ms<0
               AND last_error_category IN ('source-oversized','source-failed')",
            params![session_id, self.source_kind, now_ms()],
        )?;
        Ok(changed == 1)
    }

    /// Settles a parked session whose transcript was destroyed as `transcript-lost`
    /// (issue #58). Only an operator declares data lost, and only over a session that is
    /// unclaimed, permanently parked, failed under a missing-source category
    /// (`source-missing`, or the pre-#57 lumped `source-failed`), and with **read history**
    /// — a recorded source cursor or an archived revision — proving content existed before
    /// the file vanished. A missing-source row with no history is a phantom invocation, not
    /// a loss; the worker settles those not-archive-worthy on its own (issue #58). The
    /// caller must first verify the recorded transcript path does not currently exist. The
    /// row settles to `observed` with the verdict stamped in `transcript_lost_at_ms` — the
    /// same read-time labeling shape as the issue #50 worthiness verdict — and every
    /// recorded evidence column survives, so nothing munshi ever knew about the session is
    /// destroyed with it.
    pub fn settle_transcript_lost(&mut self, session_id: &str) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let now = now_ms();
        let changed = self.connection.execute(
            "UPDATE sessions SET lifecycle_state='observed',
                transcript_lost_at_ms=?3,
                retry_state=NULL,next_retry_at_ms=NULL,
                failure_streak=0,failure_streak_category=NULL,failure_streak_generation=NULL,
                last_error_category=NULL,updated_at_ms=?3
             WHERE source_kind=?2 AND source_session_id=?1
               AND lifecycle_state='failed'
               AND next_retry_at_ms<0
               AND claim_token IS NULL
               AND last_error_category IN ('source-missing','source-failed')
               AND (current_summary_revision>0 OR normalizer_version IS NOT NULL)",
            params![session_id, self.source_kind, now],
        )?;
        Ok(changed == 1)
    }

    /// Settled `transcript-lost` rows that recorded a transcript path, across every source
    /// scope, for the reactivation sweep: the caller re-checks each path and lifts the
    /// verdict of any transcript that has reappeared ([`lift_transcript_lost`]).
    ///
    /// [`lift_transcript_lost`]: StateStore::lift_transcript_lost
    pub fn settled_lost_transcripts(
        &self,
    ) -> Result<Vec<(SourceKind, String, PathBuf)>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT source_kind,source_session_id,transcript_path
             FROM sessions
             WHERE transcript_lost_at_ms IS NOT NULL
               AND transcript_path IS NOT NULL
             ORDER BY updated_at_ms,id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(source_kind, session_id, path)| {
                SourceKind::from_agent_label(&source_kind)
                    .map(|source| (source, session_id, PathBuf::from(path)))
            })
            .collect())
    }

    /// Lifts a `transcript-lost` verdict because the transcript reappeared at its recorded
    /// path (issue #58). Settled rows carry session-end evidence, so neither hook ingestion
    /// nor the issue #49 observed rescue would ever requeue them; the lift therefore returns
    /// the row to `failed` and immediately retry-eligible, which the normal retry sweeps and
    /// claim gates already understand — the next attempt re-reads the restored transcript.
    pub fn lift_transcript_lost(&mut self, session_id: &str) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let changed = self.connection.execute(
            "UPDATE sessions SET lifecycle_state='failed',
                transcript_lost_at_ms=NULL,
                retry_state=NULL,next_retry_at_ms=NULL,
                failure_streak=0,failure_streak_category=NULL,failure_streak_generation=NULL,
                last_error_category=NULL,updated_at_ms=?3
             WHERE source_kind=?2 AND source_session_id=?1
               AND transcript_lost_at_ms IS NOT NULL
               AND claim_token IS NULL",
            params![session_id, self.source_kind, now_ms()],
        )?;
        Ok(changed == 1)
    }

    /// Lifts a repeat-failure park (issue #38): [`fail_attempt`] parks a session permanently
    /// once [`RETRY_PARK_THRESHOLD`] consecutive same-category failures prove the failure
    /// deterministic. A plain sweep never retries such a park; this lift is reserved for an
    /// explicit, targeted operator action (`munshi retry <id>`, with `--force` covered by
    /// [`reserve_worker`]). Lifting resets the streak so the session gets a fresh escalation
    /// window, and leaves every other kind of park (for example `source-failed`, which
    /// [`lift_source_limit_park`] re-measures) untouched.
    ///
    /// [`fail_attempt`]: StateStore::fail_attempt
    /// [`reserve_worker`]: StateStore::reserve_worker
    /// [`lift_source_limit_park`]: StateStore::lift_source_limit_park
    pub fn lift_failure_park(&mut self, session_id: &str) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let changed = self.connection.execute(
            "UPDATE sessions SET next_retry_at_ms=NULL,failure_streak=0,
                failure_streak_category=NULL,failure_streak_generation=NULL,updated_at_ms=?3
             WHERE source_kind=?2 AND source_session_id=?1
               AND lifecycle_state='failed'
               AND next_retry_at_ms<0
               AND failure_streak>=?4",
            params![session_id, self.source_kind, now_ms(), RETRY_PARK_THRESHOLD],
        )?;
        Ok(changed == 1)
    }

    /// Reserves up to `limit` eligible sessions for worker spawns, least-recently-attempted
    /// first: never-attempted work leads, then sessions whose scheduled retry came due longest
    /// ago, breaking ties by the last attempt start. The previous insertion-order scan let the
    /// same head-of-line failures win the bounded concurrency slots every sweep and starve the
    /// rest of the queue (issue #38).
    pub fn reserve_eligible_workers(
        &mut self,
        force: bool,
        limit: usize,
    ) -> Result<Vec<(SourceKind, String)>, StateError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT id,source_session_id,source_kind
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
                    OR ?3
                    OR next_retry_at_ms IS NULL
                    OR (next_retry_at_ms >= 0 AND next_retry_at_ms <= ?2))
             ORDER BY COALESCE(next_retry_at_ms,0),
                      COALESCE((SELECT MAX(a.started_at_ms) FROM processing_attempts a
                                WHERE a.session_id=sessions.id),0),
                      id
             LIMIT ?4",
        )?;
        let rows = statement
            .query_map(
                params![now - WORKER_RESERVATION_STALE_MS, now, force, limit as i64],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut reserved = Vec::new();
        for (database_id, session_id, source_kind) in rows {
            // Skip any row with an unrecognized source label rather than mis-routing it.
            let Some(source) = SourceKind::from_agent_label(&source_kind) else {
                continue;
            };
            if reserve_worker_in_transaction(&transaction, database_id, now, force)? {
                if force {
                    // Forced retries clear parks and scheduled backoff and restart the
                    // consecutive-failure escalation from scratch (issue #38).
                    transaction.execute(
                        "UPDATE sessions SET next_retry_at_ms=NULL,failure_streak=0,
                            failure_streak_category=NULL,failure_streak_generation=NULL
                         WHERE id=?1 AND lifecycle_state='failed'",
                        [database_id],
                    )?;
                }
                reserved.push((source, session_id));
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
             WHERE source_kind=?2 AND source_session_id=?1",
            params![session_id, self.source_kind],
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
                 WHERE s.source_kind=?2 AND s.source_session_id=?1
                   AND a.outcome='processing'
                   AND a.planned_revision IS NOT NULL
                 ORDER BY a.id DESC LIMIT 1",
                params![session_id, self.source_kind],
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
                 WHERE s.source_kind=?2 AND s.source_session_id=?1
                   AND a.outcome='processing'
                 ORDER BY a.id DESC LIMIT 1",
                params![session_id, self.source_kind],
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
                    last_error_category,source_kind,next_retry_at_ms,failure_streak,
                    origin_project_origin,not_archive_worthy_at_ms,
                    created_at_ms,updated_at_ms,transcript_lost_at_ms
                 FROM sessions
                 WHERE source_kind=?2 AND source_session_id=?1",
                params![session_id, self.source_kind],
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
        self.complete_attempt_inner(claim, persisted, recovered, None)
    }

    /// Persists a placeholder archival (issue #43): the archive columns advance exactly like
    /// [`complete_attempt`] — the placeholder revision is real, uploaded, and delivered — but the
    /// session stays `failed` and permanently parked under `park.category`, recording that a real
    /// summary is still owed. The park composes with the issue #38 machinery: plain sweeps skip
    /// it, `munshi retry`/`--force` lift it, and the successful re-summary that follows replaces
    /// the placeholder through the ordinary revision path. The attempt itself is recorded as
    /// `failed` (the summarizer did fail) and a `placeholder-archived` diagnostic distinguishes
    /// the deterministic cause. When the worker already published the verdict through
    /// [`record_placeholder_verdict`] before making the archive file visible, `verdict_recorded`
    /// keeps the diagnostic single; the park columns are still re-asserted here, atomically with
    /// the archive columns.
    ///
    /// [`record_placeholder_verdict`]: StateStore::record_placeholder_verdict
    pub fn complete_placeholder_attempt(
        &mut self,
        claim: &Claim,
        persisted: &PersistedArchive,
        recovered: bool,
        park: &PlaceholderPark<'_>,
        verdict_recorded: bool,
    ) -> Result<(), StateError> {
        self.complete_attempt_inner(claim, persisted, recovered, Some((park, verdict_recorded)))
    }

    /// Publishes a decided placeholder park verdict (issue #43) *before* the placeholder archive
    /// file becomes visible: the park columns and the `placeholder-archived` diagnostic commit
    /// first, so no observer can ever see a placeholder archive on disk whose session does not
    /// yet carry its park. [`complete_placeholder_attempt`] then re-asserts the same park
    /// atomically with the archive columns (with `verdict_recorded` keeping the diagnostic
    /// single), and every failure path after this point overwrites the verdict with its own
    /// (`fail_attempt`, or lease-expiry reconciliation).
    ///
    /// [`complete_placeholder_attempt`]: StateStore::complete_placeholder_attempt
    pub fn record_placeholder_verdict(
        &mut self,
        claim: &Claim,
        park: &PlaceholderPark<'_>,
    ) -> Result<(), StateError> {
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
        transaction.execute(
            "UPDATE sessions SET
                lifecycle_state='failed',retry_state='revision-pending',
                next_retry_at_ms=-1,last_error_category=?2,
                failure_streak=?3,failure_streak_category=?2,failure_streak_generation=?4,
                updated_at_ms=?5
             WHERE id=?1",
            params![
                claim.session.database_id,
                park.category,
                park.streak.max(RETRY_PARK_THRESHOLD),
                claim.state_generation,
                now
            ],
        )?;
        transaction.execute(
            "INSERT INTO diagnostics(
                session_id,operation,category,cause_category,recorded_at_ms
             ) VALUES (?1,'archive-worker','placeholder-archived',?2,?3)",
            params![claim.session.database_id, park.cause, now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The worker's projection of [`fail_attempt`]'s streak bookkeeping: the streak a failure
    /// with `category` would record for this claim right now, without recording anything.
    ///
    /// [`fail_attempt`]: StateStore::fail_attempt
    pub fn projected_failure_streak(
        &self,
        claim: &Claim,
        category: &str,
    ) -> Result<i64, StateError> {
        let (prior_streak, prior_category, prior_generation): (i64, Option<String>, Option<i64>) =
            self.connection.query_row(
                "SELECT failure_streak,failure_streak_category,failure_streak_generation
                 FROM sessions WHERE id=?1",
                [claim.session.database_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        Ok(next_failure_streak(
            prior_streak,
            prior_category.as_deref(),
            prior_generation,
            category,
            claim.state_generation,
        ))
    }

    fn complete_attempt_inner(
        &mut self,
        claim: &Claim,
        persisted: &PersistedArchive,
        recovered: bool,
        placeholder: Option<(&PlaceholderPark<'_>, bool)>,
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
                origin_project_origin=COALESCE(origin_project_origin,?27),
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
                failure_streak=0,failure_streak_category=NULL,failure_streak_generation=NULL,
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
                now,
                persisted.project.origin.recorded_marker()
            ],
        )?;
        let outcome = match placeholder {
            // The archive advanced, but the summarizer attempt itself failed: record it honestly.
            Some(_) => "failed",
            None if recovered => "recovered",
            None => "succeeded",
        };
        transaction.execute(
            "UPDATE processing_attempts SET
                outcome=?2,error_category=?3,
                recovery_reason=CASE WHEN ?4 THEN 'post-persist-recovery' ELSE NULL END,
                finished_at_ms=?5
             WHERE id=?1 AND lease_token=?6 AND outcome='processing'",
            params![
                claim.attempt_id,
                outcome,
                placeholder.map(|(park, _)| park.category),
                recovered,
                now,
                claim.token
            ],
        )?;
        if let Some((park, verdict_recorded)) = placeholder {
            // Re-park in the same transaction the archive columns advanced in: the session owes a
            // real summary, so it must never surface as cleanly `archived`. The park mirrors
            // `fail_attempt`'s bookkeeping (category, streak, generation) so the issue #38 lift
            // paths treat it identically to any other deterministic-failure park.
            transaction.execute(
                "UPDATE sessions SET
                    lifecycle_state='failed',retry_state='revision-pending',
                    next_retry_at_ms=-1,last_error_category=?2,
                    failure_streak=?3,failure_streak_category=?2,failure_streak_generation=?4,
                    updated_at_ms=?5
                 WHERE id=?1",
                params![
                    claim.session.database_id,
                    park.category,
                    // At least the park threshold, so `lift_failure_park` recognizes it even when
                    // the trigger was deterministic on the first attempt (the input cap).
                    park.streak.max(RETRY_PARK_THRESHOLD),
                    claim.state_generation,
                    now
                ],
            )?;
            if !verdict_recorded {
                transaction.execute(
                    "INSERT INTO diagnostics(
                        session_id,operation,category,cause_category,recorded_at_ms
                     ) VALUES (?1,'archive-worker','placeholder-archived',?2,?3)",
                    params![claim.session.database_id, park.cause, now],
                )?;
            }
        }
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
                failure_streak=0,failure_streak_category=NULL,failure_streak_generation=NULL,
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

    /// Records the worker's not-archive-worthy verdict. The row settles back to `observed`
    /// — deliberately, not as a distinct lifecycle value: `observed` is the reactivatable
    /// shape every requeue path understands (hook ingestion on new evidence, the issue #49
    /// rescue on a changed transcript), so a stub that later grows a real reply re-enters
    /// the normal pipeline. The verdict itself is stamped in `not_archive_worthy_at_ms`
    /// (issue #50) so read-time surfaces (`operational_state`, [`wait_state`]) label the
    /// session `not-archive-worthy` even when no session-end hook was ever ingested — the
    /// sweep-discovered case that previously displayed as a phantom `observed` session.
    ///
    /// [`wait_state`]: StateStore::wait_state
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
                not_archive_worthy_at_ms=?2,
                retry_state=NULL,next_retry_at_ms=NULL,
                failure_streak=0,failure_streak_category=NULL,failure_streak_generation=NULL,
                claim_token=NULL,claim_started_at_ms=NULL,
                worker_generation=NULL,worker_spawned_at_ms=NULL,
                last_error_category=NULL,updated_at_ms=?2
             WHERE id=?1 AND claim_token=?3",
            params![claim.session.database_id, now, claim.token],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Records a failed attempt. A retryable failure schedules the next retry with the
    /// escalating per-session backoff of [`RETRY_BACKOFF_SCHEDULE_MS`], derived from the
    /// session's consecutive-failure streak — failures of the same category against the same
    /// `state_generation` extend the streak, anything else restarts it at one. Once the streak
    /// reaches [`RETRY_PARK_THRESHOLD`] the failure is treated as deterministic and the session
    /// is parked permanently (`next_retry_at_ms = -1`) with its real category retained, so it
    /// can no longer monopolize the concurrency slots every sweep (issue #38). Non-retryable
    /// failures park immediately, exactly as before.
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
        let (prior_streak, prior_category, prior_generation): (i64, Option<String>, Option<i64>) =
            transaction.query_row(
                "SELECT failure_streak,failure_streak_category,failure_streak_generation
                 FROM sessions WHERE id=?1",
                [claim.session.database_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let streak = next_failure_streak(
            prior_streak,
            prior_category.as_deref(),
            prior_generation,
            category,
            claim.state_generation,
        );
        let next_retry_at_ms = if !retryable || streak >= RETRY_PARK_THRESHOLD {
            -1
        } else {
            now.saturating_add(retry_backoff_ms(streak))
        };
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
                failure_streak=?5,failure_streak_category=?4,failure_streak_generation=?6,
                claim_token=NULL,claim_started_at_ms=NULL,
                worker_generation=NULL,worker_spawned_at_ms=NULL,
                updated_at_ms=?7
             WHERE id=?1 AND claim_token=?8",
            params![
                claim.session.database_id,
                claim.retry_state,
                next_retry_at_ms,
                category,
                streak,
                claim.state_generation,
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
            // A recorded verdict on unarchived content: either the hook path (a session-end
            // was ingested before the worker judged it) or the sweep path (the worker
            // stamped the verdict while settling the row, issue #50).
            "observed"
                if record.current_revision == 0
                    && (record.last_session_end_ms.is_some()
                        || record.not_archive_worthy_at_ms.is_some()) =>
            {
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
        homes: &SourceHomes,
    ) -> Result<bool, StateError> {
        let records = scan_archives(output_directory, Some((&self.source_kind, session_id)))?;
        let Some(record) = records
            .into_iter()
            .max_by_key(|record| record.archive.summary_revision)
        else {
            return Ok(false);
        };
        self.import_archive_record(&record, homes)?;
        Ok(true)
    }

    pub fn rebuild_from_archives(
        &mut self,
        output_directory: &Path,
        homes: &SourceHomes,
    ) -> Result<usize, StateError> {
        let records = scan_archives(output_directory, None)?;
        // Group by the full (source, session_id) identity so cross-source sessions
        // that share a session ID are both retained and never overwrite each other.
        let mut latest = std::collections::BTreeMap::<(String, String), OwnedArchive>::new();
        for record in records {
            let key = (
                record.archive.source.agent_label().to_owned(),
                record.archive.session_id.clone(),
            );
            if latest
                .get(&key)
                .is_none_or(|old| old.archive.summary_revision < record.archive.summary_revision)
            {
                latest.insert(key, record);
            }
        }
        for record in latest.values() {
            self.import_archive_record(record, homes)?;
        }
        Ok(latest.len())
    }

    /// Imports one archived revision into operational state.
    ///
    /// An archive Markdown file records everything about a session except where its transcript
    /// lives, so a rebuilt row used to be born with `transcript_path` NULL — unreadable, and (since
    /// issue #47) unable to upload a self-contained snapshot even though the transcript was still on
    /// disk. The import now re-derives the path from the session's ID through its source's own
    /// version-pinned discovery (issue #53), so a rebuilt row is born with a path wherever one is
    /// resolvable. Derivation is opportunistic by construction: a session whose transcript is gone,
    /// whose harness home is not registered, or whose source has no safe lookup keeps the NULL it
    /// had, and no derivation failure can fail a rebuild.
    fn import_archive_record(
        &mut self,
        record: &OwnedArchive,
        homes: &SourceHomes,
    ) -> Result<(), StateError> {
        let now = now_ms();
        let summary_json = serde_json::to_string(&record.archive.summary)?;
        let summary_hash = content_hash(summary_json.as_bytes());
        let transcript_path =
            derive_transcript_path(record.archive.source, &record.archive.session_id, homes);
        let transcript_source = if transcript_path.is_some() {
            REDERIVED_TRANSCRIPT_SOURCE
        } else {
            "archive-rebuild"
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = upsert_session(
            &transaction,
            record.archive.source.agent_label(),
            &record.archive.session_id,
            None,
            transcript_path.as_deref(),
            transcript_source,
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
                origin_project_origin=?23,
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
                now,
                record.archive.project.origin.recorded_marker()
            ],
        )?;
        if record.archive.summary_placeholder {
            // A placeholder archive (issue #43) records that a real summary is still owed. A
            // database rebuild must not launder it into a clean `archived` verdict, so restore the
            // park (the frontmatter does not retain the original failure category; the generic
            // summarizer verdict keeps `munshi retry` working). A session with newer observations
            // (`revision-pending`) is left alone — its next real attempt replaces the placeholder
            // anyway.
            transaction.execute(
                "UPDATE sessions SET
                    lifecycle_state='failed',retry_state='revision-pending',
                    next_retry_at_ms=-1,last_error_category='summary-failed',
                    failure_streak=?2,failure_streak_category='summary-failed',
                    failure_streak_generation=state_generation
                 WHERE id=?1 AND lifecycle_state='archived'",
                params![database_id, RETRY_PARK_THRESHOLD],
            )?;
        }
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
                last_error_category,source_kind,next_retry_at_ms,failure_streak,
                origin_project_origin,not_archive_worthy_at_ms,
                created_at_ms,updated_at_ms,transcript_lost_at_ms
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
                last_error_category,source_kind,next_retry_at_ms,failure_streak,
                origin_project_origin,not_archive_worthy_at_ms,
                created_at_ms,updated_at_ms,transcript_lost_at_ms
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

    /// Records a transcript path re-derived for a session that had none — or held one that no
    /// longer reads (issue #53). Returns whether a row in this store's source scope was updated.
    ///
    /// Deliberately smaller than [`Self::attach_recovered_transcript`]: that one records a *new
    /// observation* of unarchived work and re-enters the session into the archival lifecycle
    /// (bumping the state generation and reserving a worker), which is exactly what a read path
    /// such as archive upload must not do. This writes the located path and nothing else, so the
    /// session's archival lifecycle is untouched and the next read simply finds its transcript.
    pub fn record_derived_transcript_path(
        &mut self,
        session_id: &str,
        transcript_path: &Path,
    ) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let updated = self.connection.execute(
            "UPDATE sessions SET
                transcript_path=?3,transcript_source=?4,updated_at_ms=?5
             WHERE source_kind=?2 AND source_session_id=?1
               AND (transcript_path IS NULL OR transcript_path<>?3)",
            params![
                session_id,
                self.source_kind,
                path_text(transcript_path)?,
                REDERIVED_TRANSCRIPT_SOURCE,
                now_ms()
            ],
        )?;
        Ok(updated == 1)
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
                 WHERE source_kind=?2 AND source_session_id=?1",
                params![session_id, self.source_kind],
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
            reserve_worker_in_transaction(&transaction, database_id, now, false)?
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
            &self.source_kind,
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
            reserve_worker_in_transaction(&transaction, database_id, now, false)?
        } else {
            false
        };
        transaction.commit()?;
        Ok(reserved)
    }

    /// Known sessions the recovery queue holds but no worker path can ever claim: rows a
    /// database rebuild (or an origin-less sweep) left in `interrupted` with a transcript but
    /// no `origin_cwd` (issue #39). `stale_known_sessions` needs `active=1` plus agent-stop
    /// evidence and `reserve_eligible_workers` needs an origin, so without hydration these
    /// rows deadlock silently. Spans every source kind, like the other recovery work lists.
    pub fn unhydrated_recovery_sessions(&self) -> Result<Vec<SessionRecord>, StateError> {
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
                last_error_category,source_kind,next_retry_at_ms,failure_streak,
                origin_project_origin,not_archive_worthy_at_ms,
                created_at_ms,updated_at_ms,transcript_lost_at_ms
             FROM sessions
             WHERE active=0
               AND lifecycle_state='interrupted'
               AND transcript_path IS NOT NULL
               AND origin_cwd IS NULL
             ORDER BY updated_at_ms,id",
        )?;
        statement
            .query_map([], session_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Hydrates one unhydrated recovery row (see [`unhydrated_recovery_sessions`]) with its
    /// derived origin and transcript-mtime activity evidence, and reserves the normal archive
    /// worker for it inside the same transaction — the caller must spawn a worker whenever
    /// this reports a reservation, exactly as with [`mark_recovery_interrupted`]. The row
    /// stays `active=0` so the worker's `claim_session` can take it. Returns `false` without
    /// changes when the session no longer matches the unhydrated shape (a concurrent hook or
    /// sweep already hydrated or advanced it).
    ///
    /// [`unhydrated_recovery_sessions`]: StateStore::unhydrated_recovery_sessions
    /// [`mark_recovery_interrupted`]: StateStore::mark_recovery_interrupted
    pub fn hydrate_recovery_origin(
        &mut self,
        session_id: &str,
        origin_cwd: &Path,
        activity_ms: i64,
    ) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = transaction
            .query_row(
                "SELECT id FROM sessions
                 WHERE source_kind=?2 AND source_session_id=?1
                   AND active=0 AND lifecycle_state='interrupted'
                   AND transcript_path IS NOT NULL AND origin_cwd IS NULL",
                params![session_id, self.source_kind],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(database_id) = database_id else {
            transaction.commit()?;
            return Ok(false);
        };
        transaction.execute(
            "UPDATE sessions SET
                origin_cwd=?2,
                last_agent_stop_ms=COALESCE(last_agent_stop_ms,?3),
                state_generation=state_generation+1,
                last_error_category=NULL,
                updated_at_ms=?4
             WHERE id=?1",
            params![database_id, path_text(origin_cwd)?, activity_ms, now],
        )?;
        let reserved = reserve_worker_in_transaction(&transaction, database_id, now, false)?;
        transaction.commit()?;
        Ok(reserved)
    }

    /// Known sessions stranded in the second recovery-invisible shape (issue #49, sibling of
    /// [`unhydrated_recovery_sessions`]): `lifecycle_state='observed'` with `active=0` and no
    /// session-end verdict. The shape arises when a session's stop/end hooks never ingested
    /// (e.g. a rejected payload) and a swept `interrupted` row was then judged by a worker
    /// that returned it to `observed` — `stale_known_sessions` needs `active=1`,
    /// `reserve_eligible_workers` excludes `observed`, and the #39 hydration is scoped to
    /// `interrupted`, so nothing can ever claim the row again. Rows with
    /// `last_session_end_ms` set are excluded: for those, `observed` is a recorded
    /// not-archive-worthy verdict reachable through `wait_state`, not a stuck shape. Rows
    /// the sweep itself already settled stay in this list on purpose: their verdict is
    /// visible through `not_archive_worthy_at_ms` (issue #50), and the rescue's
    /// evidence-keyed dedupe keeps an unchanged transcript from churning while a transcript
    /// that has since grown is requeued through the normal pipeline. Spans every source
    /// kind, like the other recovery work lists.
    ///
    /// [`unhydrated_recovery_sessions`]: StateStore::unhydrated_recovery_sessions
    pub fn stuck_observed_sessions(&self) -> Result<Vec<SessionRecord>, StateError> {
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
                last_error_category,source_kind,next_retry_at_ms,failure_streak,
                origin_project_origin,not_archive_worthy_at_ms,
                created_at_ms,updated_at_ms,transcript_lost_at_ms
             FROM sessions
             WHERE active=0
               AND lifecycle_state='observed'
               AND last_session_end_ms IS NULL
               AND transcript_lost_at_ms IS NULL
             ORDER BY updated_at_ms,id",
        )?;
        statement
            .query_map([], session_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Requeues one stuck observed row (see [`stuck_observed_sessions`]) through the normal
    /// interrupted-recovery pipeline: fills the missing origin and transcript-mtime activity
    /// evidence, moves the row to `interrupted` (or `revision-pending` when a summary
    /// already exists), and reserves the archive worker inside the same transaction — the
    /// caller must spawn a worker whenever this reports a reservation, exactly as with
    /// [`mark_recovery_interrupted`]. The rescue observation is deduplicated on the
    /// transcript's byte/mtime evidence, so a session the worker again judges
    /// not-archive-worthy settles instead of churning through every later sweep; only a
    /// transcript that has since changed is rescued again. Returns `false` without changes
    /// when the session no longer matches the stuck shape or was already rescued at this
    /// transcript state.
    ///
    /// [`stuck_observed_sessions`]: StateStore::stuck_observed_sessions
    /// [`mark_recovery_interrupted`]: StateStore::mark_recovery_interrupted
    pub fn rescue_observed_session(
        &mut self,
        session_id: &str,
        transcript_path: &Path,
        origin_cwd: Option<&Path>,
        evidence_key: &str,
        activity_ms: i64,
    ) -> Result<bool, StateError> {
        validate_session_id(session_id)?;
        let now = now_ms();
        let dedupe = dedupe_key(&["observed-recovery", session_id, evidence_key]);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = transaction
            .query_row(
                "SELECT id FROM sessions
                 WHERE source_kind=?2 AND source_session_id=?1
                   AND active=0 AND lifecycle_state='observed'
                   AND last_session_end_ms IS NULL
                   AND transcript_path IS NOT NULL",
                params![session_id, self.source_kind],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(database_id) = database_id else {
            transaction.commit()?;
            return Ok(false);
        };
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO source_observations(
                session_id,event_kind,event_timestamp_ms,transcript_path,
                completion_reason,dedupe_key,observed_at_ms
             ) VALUES (?1,'recovery-scan',NULL,?2,'unknown',?3,?4)",
            params![database_id, path_text(transcript_path)?, dedupe, now],
        )?;
        if inserted == 0 {
            transaction.commit()?;
            return Ok(false);
        }
        let observation_id = transaction.last_insert_rowid();
        transaction.execute(
            "UPDATE sessions SET
                current_observation_id=?2,state_generation=state_generation+1,
                origin_cwd=COALESCE(origin_cwd,?3),
                last_agent_stop_ms=COALESCE(last_agent_stop_ms,?4),
                completion_reason=COALESCE(completion_reason,'unknown'),
                source_end_reason=COALESCE(source_end_reason,'unknown'),
                lifecycle_state=CASE WHEN current_summary_revision > 0
                    THEN 'revision-pending' ELSE 'interrupted' END,
                last_error_category=NULL,
                updated_at_ms=?5
             WHERE id=?1",
            params![
                database_id,
                observation_id,
                origin_cwd.map(path_text).transpose()?,
                activity_ms,
                now
            ],
        )?;
        let reserved = reserve_worker_in_transaction(&transaction, database_id, now, false)?;
        transaction.commit()?;
        Ok(reserved)
    }

    /// Marks a recovery-held session that cannot be hydrated safely (never-drop: it stays
    /// queued, nothing on disk is touched). The visible category is written to the row and,
    /// only on the transition, one diagnostics entry is recorded — repeat sweeps over the
    /// same parked session stay quiet.
    ///
    /// The row must still be recovery-held inside the transaction, the same in-transaction
    /// re-check [`hydrate_recovery_origin`] makes: the sweep derives its verdict outside any
    /// transaction, so a session a concurrent hook has since claimed (`active=1`) or archived
    /// would otherwise be labelled `origin-unresolved` on the way past. The guard spans both
    /// recovery-held shapes the sweep parks from — the unhydrated `interrupted` rows and the
    /// stuck `observed` ones — and a row that has moved on is left alone. Cosmetic only:
    /// scheduling never consulted the label.
    ///
    /// [`hydrate_recovery_origin`]: StateStore::hydrate_recovery_origin
    pub fn park_recovery_session(
        &mut self,
        session_id: &str,
        category: &str,
    ) -> Result<(), StateError> {
        validate_session_id(session_id)?;
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let database_id = transaction
            .query_row(
                "SELECT id FROM sessions
                 WHERE source_kind=?2 AND source_session_id=?1
                   AND active=0 AND lifecycle_state IN ('interrupted','observed')
                   AND (last_error_category IS NULL OR last_error_category<>?3)",
                params![session_id, self.source_kind, category],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(database_id) = database_id else {
            transaction.commit()?;
            return Ok(());
        };
        transaction.execute(
            "UPDATE sessions SET last_error_category=?2,updated_at_ms=?3 WHERE id=?1",
            params![database_id, category, now],
        )?;
        transaction.execute(
            "INSERT INTO diagnostics(
                session_id,operation,category,cause_category,recorded_at_ms
             ) VALUES (?1,'recovery',?2,NULL,?3)",
            params![database_id, category, now],
        )?;
        transaction.commit()?;
        Ok(())
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
    wanted: Option<(&str, &str)>,
) -> Result<Vec<OwnedArchive>, StateError> {
    if !output_directory.exists() {
        return Ok(Vec::new());
    }
    let root = output_directory.canonicalize()?;
    if !root.is_dir() {
        return Err(StateError::InvalidState);
    }
    let mut files = Vec::new();
    collect_archive_files(&root, 0, &mut files)?;
    let mut records = Vec::new();
    for path in files {
        if validate_regular_owned_file(&path).is_err() {
            continue;
        }
        if path.file_stem().and_then(OsStr::to_str).is_none() {
            continue;
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
        if let Some((wanted_source, wanted_session)) = wanted {
            if archive.source.agent_label() != wanted_source || archive.session_id != wanted_session
            {
                continue;
            }
        }
        let relative_path = path
            .strip_prefix(&root)
            .map_err(|_| StateError::InvalidState)?
            .to_path_buf();
        // Schema-1 records omit the project component; recover it from the
        // component directory, which sits one level up for Copilot's flat layout
        // and two levels up for the source-nested layout.
        if archive.project.component.is_empty() {
            let component_dir = match archive.source {
                SourceKind::Copilot => relative_path.parent(),
                _ => relative_path.parent().and_then(Path::parent),
            };
            archive.project.component = component_dir
                .and_then(Path::file_name)
                .and_then(OsStr::to_str)
                .unwrap_or("project")
                .to_owned();
        }
        // Accept a record only when it sits at the exact source-scoped path Munshi
        // would write it to. This ties the file location to (source, component,
        // session_id) and rejects misplaced or spoofed archives.
        let expected = crate::render::archive_relative_path(
            archive.source,
            &archive.project.component,
            &archive.session_id,
        );
        if relative_path != expected {
            continue;
        }
        records.push(OwnedArchive {
            relative_path,
            markdown_hash: content_hash(&bytes),
            archive,
        });
    }
    Ok(records)
}

/// Depth-bounded, symlink-safe walk collecting `.md` files under the archive root.
/// The depth bound covers both Copilot's `<component>/<file>` layout and the
/// source-nested `<component>/<source>/<file>` layout without following symlinks.
fn collect_archive_files(
    directory: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Result<(), StateError> {
    if depth > 3 {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_archive_files(&path, depth + 1, out)?;
        } else if file_type.is_file() && path.extension().and_then(OsStr::to_str) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
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
            &self.source_kind,
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
            &self.source_kind,
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
    homes: &SourceHomes,
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
    state.rebuild_from_archives(output_directory, homes)
}
