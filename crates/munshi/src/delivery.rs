//! Opt-in delivery of current Munshi-owned summaries to a Notesmith sink.
//!
//! Delivery is disabled by default and is strictly downstream of local archival: a Notesmith
//! outage or an exhausted delivery attempt never rolls back, invalidates, or blocks a local
//! Markdown archive (ADR 0002, khata-handoff.md). Notesmith notes are Munshi-owned copies, so a
//! later summary revision replaces the matching note even if it was edited remotely.
//!
//! This module intentionally does **not** implement issue #9's mandatory remote revision-history
//! capability. It is structured for it: the [`NotesmithSink`] trait isolates the wire protocol,
//! and delivered notes carry stable session/revision frontmatter a future versioned sink can use.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use crate::http::{self, Header, HttpError, HttpResponse, encode_path, parse_http_endpoint};
use crate::registration::{
    DEFAULT_MAX_DELIVERY_ATTEMPTS, RegistrationError, StoredConfig, StoredCredential,
    StoredSummaryDelivery, load_stored_config, stored_config_exists, update_stored_config,
};
use crate::state::{
    DeliveryRecord, DeliverySuccess, SessionRecord, StateError, StateStore, now_ms,
};

/// Base backoff between failed delivery attempts; doubles per attempt up to [`MAX_BACKOFF_MS`].
const BASE_BACKOFF_MS: i64 = 60_000;
/// Upper bound on delivery backoff so a long outage still retries roughly hourly.
const MAX_BACKOFF_MS: i64 = 3_600_000;
/// Network timeout for a single delivery request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Error)]
pub enum DeliveryError {
    #[error(transparent)]
    Registration(#[from] RegistrationError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("delivery I/O failed")]
    Io(#[source] std::io::Error),
    #[error("Notesmith delivery is not enabled; run `munshi summary-delivery enable`")]
    NotEnabled,
    #[error("Notesmith sink is not configured; run `munshi summary-delivery configure`")]
    NotConfigured,
    #[error("delivery credential could not be resolved: {0}")]
    Credential(String),
    #[error("delivery endpoint {0} is not a supported http URL")]
    UnsupportedEndpoint(String),
}

impl From<HttpError> for DeliveryError {
    fn from(error: HttpError) -> Self {
        match error {
            HttpError::UnsupportedEndpoint(endpoint) => Self::UnsupportedEndpoint(endpoint),
            other => Self::Io(std::io::Error::other(other.to_string())),
        }
    }
}

/// Where the Notesmith bearer credential is read from. The secret itself is never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryCredentialSource {
    Env { var: String },
    Keychain { service: String, account: String },
}

impl DeliveryCredentialSource {
    fn to_stored(&self) -> StoredCredential {
        match self {
            Self::Env { var } => StoredCredential::Env { var: var.clone() },
            Self::Keychain { service, account } => StoredCredential::Keychain {
                service: service.clone(),
                account: account.clone(),
            },
        }
    }

    fn describe(stored: &StoredCredential) -> String {
        match stored {
            StoredCredential::Env { var } => format!("env:{var}"),
            StoredCredential::Keychain { service, account } => {
                format!("keychain:{service}/{account}")
            }
        }
    }
}

/// Sink details supplied by `munshi summary-delivery configure`.
#[derive(Debug, Clone)]
pub struct DeliverySinkConfig {
    pub endpoint: String,
    pub vault: String,
    pub folder: Option<String>,
    pub credential: Option<DeliveryCredentialSource>,
    pub max_attempts: Option<u32>,
    /// When `Some`, updates whether Munshi explicitly configures the remote history capability
    /// (versus verify-only) for versioned delivery. `None` leaves the current setting unchanged.
    pub provision_history: Option<bool>,
}

/// A safe, secret-free view of the resolved delivery configuration for reporting.
#[derive(Debug, Clone, Serialize)]
pub struct DeliverySettings {
    pub enabled: bool,
    pub addressable: bool,
    pub endpoint: Option<String>,
    pub vault: Option<String>,
    pub folder: Option<String>,
    pub credential_source: Option<String>,
    pub max_attempts: u32,
    /// `true` when versioned delivery is required: local archive Git history is enabled alongside
    /// delivery, so the remote must preserve correlated revision history (issue #9).
    pub versioned: bool,
    /// `true` when Munshi will explicitly configure the remote history capability if it is absent,
    /// rather than only verifying it.
    pub provision_history: bool,
}

impl DeliverySettings {
    fn from_config(config: &StoredConfig) -> Self {
        Self {
            enabled: config.summary_delivery.enabled,
            addressable: config.summary_delivery.is_addressable(),
            endpoint: config.summary_delivery.endpoint.clone(),
            vault: config.summary_delivery.vault.clone(),
            folder: config.summary_delivery.folder.clone(),
            credential_source: config
                .summary_delivery
                .credential
                .as_ref()
                .map(DeliveryCredentialSource::describe),
            max_attempts: config.summary_delivery.max_attempts,
            versioned: config.archive_git_history && config.summary_delivery.enabled,
            provision_history: config.summary_delivery.provision_history,
        }
    }

    /// The view for a state directory that has never been registered: delivery is off and
    /// unaddressable, with no recorded settings.
    fn unregistered() -> Self {
        Self {
            enabled: false,
            addressable: false,
            endpoint: None,
            vault: None,
            folder: None,
            credential_source: None,
            max_attempts: DEFAULT_MAX_DELIVERY_ATTEMPTS,
            versioned: false,
            provision_history: false,
        }
    }
}

/// One delivery record projected for the CLI, without secrets.
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryItem {
    pub source: String,
    pub session_id: String,
    pub state: String,
    pub note_path: Option<String>,
    pub note_link: Option<String>,
    pub delivered_revision: Option<u64>,
    /// The correlated remote history commit that preserved this revision (issue #9), when known.
    pub history_commit: Option<String>,
    pub attempts: u32,
    pub next_attempt_at_ms: Option<i64>,
    pub last_error_category: Option<String>,
}

/// The `munshi summary-delivery status` contract.
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryStatusReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub settings: DeliverySettings,
    pub total: usize,
    pub delivered: usize,
    pub pending: usize,
    pub failed: usize,
    pub dead_letter: usize,
    pub blocked: usize,
    pub items: Vec<DeliveryItem>,
}

/// The outcome of one session's delivery attempt.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case", tag = "result")]
pub enum DeliveryOutcome {
    Created {
        note_path: String,
        revision: u64,
    },
    Replaced {
        note_path: String,
        revision: u64,
    },
    AlreadyDelivered {
        note_path: Option<String>,
        revision: u64,
    },
    Skipped {
        reason: String,
    },
    Blocked {
        reason: String,
        detail: Option<String>,
    },
    Failed {
        category: String,
        dead_letter: bool,
    },
}

impl DeliveryOutcome {
    fn as_kind(&self) -> &'static str {
        match self {
            Self::Created { .. } => "created",
            Self::Replaced { .. } => "replaced",
            Self::AlreadyDelivered { .. } => "already-delivered",
            Self::Skipped { .. } => "skipped",
            Self::Blocked { .. } => "blocked",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Whether versioned (revision-history-preserving) delivery is required for this run, and the
/// resolved capability of the sink's vault. Versioned delivery is mandatory when local archive Git
/// history is enabled alongside Notesmith delivery (issue #9); it must never silently degrade to
/// latest-only storage when the remote cannot preserve correlated history.
pub(crate) enum HistoryGate {
    /// Local Git history is off, so latest-only delivery is the intended behavior (issue #8).
    NotRequired,
    /// Versioned delivery is required and the sink can preserve correlated history.
    Available,
    /// Versioned delivery is required but the sink cannot preserve history: block, don't degrade.
    Blocked {
        reason: String,
        detail: Option<String>,
    },
}

/// The `munshi summary-delivery backfill` / `munshi summary-delivery retry` contract.
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryRunReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub confirmed: bool,
    pub settings: DeliverySettings,
    pub candidates: usize,
    pub created: usize,
    pub replaced: usize,
    pub already_delivered: usize,
    pub skipped: usize,
    pub blocked: usize,
    pub failed: usize,
    pub items: Vec<DeliveryRunItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeliveryRunItem {
    pub source: String,
    pub session_id: String,
    pub outcome: DeliveryOutcome,
}

// ---------------------------------------------------------------------------
// Sink abstraction
// ---------------------------------------------------------------------------

/// A note reference returned by the sink: its persisted path and last known content hash.
#[derive(Debug, Clone)]
pub struct NoteRef {
    pub path: String,
    pub hash: Option<String>,
}

/// A create request for a Munshi-owned note. `body` is the durable summary body only: Notesmith's
/// create route assembles the note from a separate `frontmatter` map plus this body, so passing a
/// document that already carried a frontmatter block would stack two blocks.
#[derive(Debug, Clone)]
pub struct CreateNote<'a> {
    pub title: &'a str,
    pub folder: &'a str,
    pub body: &'a str,
    pub session_id: &'a str,
    pub source: &'a str,
    pub project_identity: &'a str,
    pub revision: u64,
}

#[derive(Debug, Error)]
pub enum SinkError {
    /// A create raced an existing note; the deterministic path is included when known.
    #[error("note already exists")]
    AlreadyExists { path: Option<String> },
    /// A replace targeted a note that no longer exists remotely.
    #[error("note not found")]
    NotFound,
    /// The sink could not be reached (a Notesmith outage): retryable.
    #[error("delivery transport failed: {0}")]
    Transport(String),
    /// The sink returned an unexpected status: retryable.
    #[error("delivery sink returned status {status}")]
    Server { status: u16, body: String },
    /// The sink response could not be understood.
    #[error("delivery sink protocol error: {0}")]
    Protocol(String),
    /// The sink's vault cannot preserve correlated revision history (issue #9): the versioned
    /// delivery contract requires it, so delivery is blocked rather than degraded to latest-only.
    #[error("remote revision history is unavailable: {0}")]
    HistoryUnavailable(String),
}

impl SinkError {
    fn category(&self) -> &'static str {
        match self {
            Self::AlreadyExists { .. } => "delivery-conflict",
            Self::NotFound => "delivery-not-found",
            Self::Transport(_) => "delivery-transport",
            Self::Server { .. } => "delivery-server",
            Self::Protocol(_) => "delivery-protocol",
            Self::HistoryUnavailable(_) => "remote-history-unavailable",
        }
    }
}

/// The revision-history capability of a Notesmith vault (issue #9).
///
/// Notesmith preserves per-note revision history through per-vault Git: the vault's
/// `git.enabled` config gates commits, and enabling it auto-initializes the repository
/// (notes-method `routes/git.rs`, `routes/config.rs`). `available` is `true` only when a delivered
/// revision can be preserved as a correlated commit.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryCapability {
    pub available: bool,
    /// The mechanism that preserves history; currently always `"git"`.
    pub mechanism: &'static str,
    /// A safe, secret-free human-readable detail for diagnostics.
    pub detail: Option<String>,
    /// `true` when Munshi configured the capability during this check (versus already present).
    pub configured: bool,
}

impl HistoryCapability {
    fn available(configured: bool) -> Self {
        Self {
            available: true,
            mechanism: "git",
            detail: Some(if configured {
                "vault git history enabled".to_owned()
            } else {
                "vault git history already enabled".to_owned()
            }),
            configured,
        }
    }

    fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            available: false,
            mechanism: "git",
            detail: Some(detail.into()),
            configured: false,
        }
    }
}

/// The Notesmith vault's Git working-tree state (issue #9). Because Notesmith's commit endpoint
/// stages the *entire* working tree (`notesmith-git::ops::commit_all`), Munshi requires the tree to
/// be clean of unrelated changes before it writes and commits a delivered revision — otherwise an
/// unrelated dirty file would be bundled into the Munshi-correlated commit.
#[derive(Debug, Clone)]
pub struct HistoryStatus {
    pub clean: bool,
    /// Every changed, staged, or untracked path reported by the vault.
    pub dirty_paths: Vec<String>,
}

/// The outcome of a `git/commit` call: whether a commit was created and its id.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub committed: bool,
    pub sha: Option<String>,
}

/// A commit located in the vault's history by its exact Munshi correlation message (issue #9).
#[derive(Debug, Clone)]
pub struct HistoryCommit {
    pub sha: String,
    /// Number of files the commit changed; `1` confirms only the delivered note was committed.
    pub files_changed: usize,
}

/// The Notesmith wire protocol Munshi depends on. Isolating it keeps the delivery orchestration
/// testable and leaves room for issue #9's versioned, revision-history-preserving sink.
pub trait NotesmithSink {
    fn create_note(&self, request: &CreateNote<'_>) -> Result<NoteRef, SinkError>;
    /// Replaces a note's content, overwriting any remote edits (Munshi owns delivered notes).
    fn replace_note(&self, path: &str, content: &str) -> Result<NoteRef, SinkError>;
    /// Reports whether the sink's vault can preserve correlated revision history (issue #9),
    /// without mutating the vault.
    fn history_capability(&self) -> Result<HistoryCapability, SinkError>;
    /// Verifies the revision-history capability and explicitly configures it when absent, so
    /// versioned delivery has a place to preserve correlated history.
    fn ensure_history_capability(&self) -> Result<HistoryCapability, SinkError>;
    /// Reports the vault's Git working-tree state so Munshi can refuse to commit a revision while
    /// unrelated changes are present (Notesmith commits stage the whole tree).
    fn history_status(&self) -> Result<HistoryStatus, SinkError>;
    /// Commits the just-delivered note revision into the vault's history with a message that
    /// correlates it to the local archive commit. Returns whether a commit was created and its id
    /// (a no-op `committed: false` when the tree already matched, e.g. after a crash-recovery
    /// idempotent replace).
    fn commit_revision(&self, message: &str) -> Result<CommitOutcome, SinkError>;
    /// Finds a commit in the vault's history whose subject is *exactly* `message` (no prefix or
    /// substring matching), returning its id and file count. Used to recover the correlated commit
    /// after a lost commit response or a rebuilt operational database.
    fn find_commit_by_message(&self, message: &str) -> Result<Option<HistoryCommit>, SinkError>;
}

// ---------------------------------------------------------------------------
// Note routing
// ---------------------------------------------------------------------------

/// Routes a session's delivered note under a stable, project-identity-derived folder using a
/// stable session-identity filename, so the note identifier is idempotent across deliveries.
fn note_route(
    folder: Option<&str>,
    component: &str,
    source: &str,
    session_id: &str,
) -> (String, String) {
    let mut dir = String::new();
    if let Some(folder) = folder {
        let trimmed = folder.trim_matches('/');
        if !trimmed.is_empty() {
            dir.push_str(trimmed);
            dir.push('/');
        }
    }
    dir.push_str(component);
    let title = format!("{source}-{session_id}");
    (dir, title)
}

fn note_path(folder: Option<&str>, component: &str, source: &str, session_id: &str) -> String {
    let (dir, title) = note_route(folder, component, source, session_id);
    format!("{dir}/{title}.md")
}

/// Splits a Munshi-owned archive Markdown document into its durable body, discarding the archive's
/// own YAML frontmatter block. Delivered notes carry Munshi *identity* frontmatter instead, so the
/// archive frontmatter must not be forwarded (it would otherwise stack a second block on create).
fn archive_body(markdown: &str) -> &str {
    if let Some(rest) = markdown.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        return rest[end + "\n---\n".len()..].trim_start_matches('\n');
    }
    markdown
}

/// The Munshi-owned identity frontmatter carried by every delivered note. These fields let a future
/// versioned sink (issue #9) correlate revisions and let Munshi recognize notes it owns.
fn identity_frontmatter(
    source: &str,
    session_id: &str,
    project_identity: &str,
    revision: u64,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("munshi_session".to_owned(), serde_json::json!(session_id));
    map.insert("munshi_source".to_owned(), serde_json::json!(source));
    map.insert(
        "munshi_project".to_owned(),
        serde_json::json!(project_identity),
    );
    map.insert("munshi_revision".to_owned(), serde_json::json!(revision));
    map
}

/// Serializes a flat identity-frontmatter map to YAML with sorted keys. Values are limited to
/// strings and integers, matching [`identity_frontmatter`].
fn frontmatter_yaml(map: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let mut yaml = String::new();
    for key in keys {
        match &map[key] {
            serde_json::Value::Number(number) => {
                yaml.push_str(&format!("{key}: {number}\n"));
            }
            value => {
                let text = value.as_str().unwrap_or_default();
                yaml.push_str(&format!("{key}: {}\n", yaml_quote(text)));
            }
        }
    }
    yaml
}

/// Double-quotes and escapes a YAML scalar string.
fn yaml_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// Builds a complete, valid Munshi-owned note document (one frontmatter block plus body) for a
/// replace, which — unlike create — takes the full document rather than a separate frontmatter map.
fn delivery_document(map: &serde_json::Map<String, serde_json::Value>, body: &str) -> String {
    format!("---\n{}---\n{}\n", frontmatter_yaml(map), body.trim_end())
}

fn backoff_ms(attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(16);
    BASE_BACKOFF_MS
        .saturating_mul(1_i64 << shift)
        .min(MAX_BACKOFF_MS)
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Delivers one session's current summary to the sink, recording the result in operational state.
///
/// This never mutates the session's archival lifecycle. On failure it schedules a bounded retry or
/// parks the delivery as a dead letter; on success it persists the remote note identifier and the
/// delivered revision so retries are idempotent.
pub(crate) fn deliver_one(
    state: &mut StateStore,
    sink: &dyn NotesmithSink,
    delivery: &StoredSummaryDelivery,
    disabled_projects: &[String],
    output_directory: &Path,
    record: &SessionRecord,
    gate: &HistoryGate,
) -> Result<DeliveryOutcome, DeliveryError> {
    let (Some(endpoint), Some(vault)) = (delivery.endpoint.as_deref(), delivery.vault.as_deref())
    else {
        return Ok(DeliveryOutcome::Skipped {
            reason: "not-configured".to_owned(),
        });
    };

    if record.current_revision == 0 {
        return Ok(DeliveryOutcome::Skipped {
            reason: "not-archived".to_owned(),
        });
    }
    let Some(relative) = record.markdown_relative_path.as_ref() else {
        return Ok(DeliveryOutcome::Skipped {
            reason: "no-archive-file".to_owned(),
        });
    };
    let Some(project) = record.project.as_ref() else {
        return Ok(DeliveryOutcome::Skipped {
            reason: "no-project-identity".to_owned(),
        });
    };
    // A disabled project stops future delivery while its existing history is retained.
    if disabled_projects.iter().any(|id| id == &project.identity) {
        return Ok(DeliveryOutcome::Skipped {
            reason: "project-disabled".to_owned(),
        });
    }

    let existing = state.ensure_delivery_target(&record.session_id, endpoint, vault)?;
    let existing = match existing {
        Some(existing) => existing,
        None => {
            return Ok(DeliveryOutcome::Skipped {
                reason: "session-unknown".to_owned(),
            });
        }
    };
    if existing.delivery_state == "dead-letter" {
        return Ok(DeliveryOutcome::Skipped {
            reason: "dead-letter".to_owned(),
        });
    }

    let summary_hash = record.current_summary_hash.clone();
    if existing.delivery_state == "delivered"
        && existing.delivered_revision == Some(record.current_revision)
        && existing.delivered_summary_hash == summary_hash
        && summary_hash.is_some()
    {
        return Ok(DeliveryOutcome::AlreadyDelivered {
            note_path: existing.note_path,
            revision: record.current_revision,
        });
    }

    // Issue #9: when versioned delivery is required but the sink cannot preserve correlated
    // revision history, block instead of silently degrading to latest-only. The block never
    // contacts the create/replace routes and never advances retry bookkeeping, and it leaves the
    // already-successful local archive untouched.
    if let HistoryGate::Blocked { reason, detail } = gate {
        state.record_delivery_blocked(&record.session_id, endpoint, vault, reason)?;
        return Ok(DeliveryOutcome::Blocked {
            reason: reason.clone(),
            detail: detail.clone(),
        });
    }

    let archive_markdown =
        fs::read_to_string(output_directory.join(relative)).map_err(DeliveryError::Io)?;
    let body = archive_body(&archive_markdown);
    let source_selector = record.source.as_selector();
    let identity = identity_frontmatter(
        source_selector,
        &record.session_id,
        &project.identity,
        record.current_revision,
    );
    // A complete, single-frontmatter-block document for replace (PUT takes the whole document).
    let document = delivery_document(&identity, body);
    let deterministic_path = note_path(
        delivery.folder.as_deref(),
        &project.component,
        source_selector,
        &record.session_id,
    );
    let (dir, title) = note_route(
        delivery.folder.as_deref(),
        &project.component,
        source_selector,
        &record.session_id,
    );
    let create = CreateNote {
        title: &title,
        folder: &dir,
        body,
        session_id: &record.session_id,
        source: source_selector,
        project_identity: &project.identity,
        revision: record.current_revision,
    };

    // Issue #9 preflight: Notesmith's commit endpoint stages the *entire* working tree
    // (`notesmith-git::ops::commit_all`), so before writing and committing a revision Munshi
    // requires the vault tree to be clean of unrelated changes. The session's own note is allowed
    // to be dirty (a prior attempt may have written it without committing). If any *other* path is
    // dirty, block instead of bundling unrelated work into the correlated commit or overwriting
    // the note. This clean-tree guard is the enforceable guarantee; see the post-commit check for
    // the residual concurrency window.
    if matches!(gate, HistoryGate::Available) {
        match sink.history_status() {
            Ok(status) if !status.clean => {
                let owned: [Option<&str>; 2] = [
                    Some(deterministic_path.as_str()),
                    existing.note_path.as_deref(),
                ];
                let unrelated: Vec<String> = status
                    .dirty_paths
                    .iter()
                    .filter(|path| {
                        !owned
                            .iter()
                            .flatten()
                            .any(|owned| normalize_note_path(owned) == normalize_note_path(path))
                    })
                    .cloned()
                    .collect();
                if !unrelated.is_empty() {
                    state.record_delivery_blocked(
                        &record.session_id,
                        endpoint,
                        vault,
                        "remote-history-dirty",
                    )?;
                    return Ok(DeliveryOutcome::Blocked {
                        reason: "remote-history-dirty".to_owned(),
                        detail: Some(format!(
                            "Notesmith vault has {} unrelated uncommitted change(s); commit or discard them before versioned delivery",
                            unrelated.len()
                        )),
                    });
                }
            }
            Ok(_) => {}
            Err(error) => {
                // The working-tree state could not be verified: fail (bounded retry) rather than
                // risk committing while the tree is dirty.
                let category = error.category();
                let updated = state.record_delivery_failure(
                    &record.session_id,
                    endpoint,
                    vault,
                    category,
                    delivery.max_attempts.max(1),
                    now_ms().saturating_add(backoff_ms(existing.attempts.saturating_add(1))),
                )?;
                return Ok(DeliveryOutcome::Failed {
                    category: category.to_owned(),
                    dead_letter: updated.delivery_state == "dead-letter",
                });
            }
        }
    }

    let result = if let Some(path) = existing.note_path.as_deref() {
        replace_or_create(sink, path, &document, &create, &deterministic_path)
    } else {
        create_or_adopt(sink, &create, &document, &deterministic_path)
    };
    match result {
        Ok((note, created)) => {
            // In versioned mode, preserve this revision as a correlated commit in the vault's own
            // history before recording success. The commit message carries the same source-scoped
            // session identity and revision as the local archive commit, so the two histories
            // correlate. Recovery is idempotent: a lost commit response or a rebuilt operational
            // database resolves to the existing commit by its exact message rather than losing the
            // correlation or degrading to a latest-only success.
            let history_commit = if matches!(gate, HistoryGate::Available) {
                let message = format!(
                    "munshi: {source_selector}:{} revision {}",
                    record.session_id, record.current_revision
                );
                match commit_and_correlate(sink, &message) {
                    CommitCorrelation::Committed(commit) => {
                        if commit.files_changed > 1 {
                            // A concurrent write landed in the narrow window between the clean-tree
                            // preflight and the whole-tree commit, bundling other files. Notesmith
                            // cannot split a commit, so this is recorded for visibility; Munshi does
                            // not claim exclusive one-file commits (documented in ADR 0006).
                            let _ = state.record_diagnostic(
                                "delivery",
                                "remote-history-conflated",
                                None,
                                Some(&record.session_id),
                            );
                        }
                        Some(commit.sha)
                    }
                    CommitCorrelation::Failed { category } => {
                        let updated = state.record_delivery_failure(
                            &record.session_id,
                            endpoint,
                            vault,
                            category,
                            delivery.max_attempts.max(1),
                            now_ms()
                                .saturating_add(backoff_ms(existing.attempts.saturating_add(1))),
                        )?;
                        return Ok(DeliveryOutcome::Failed {
                            category: category.to_owned(),
                            dead_letter: updated.delivery_state == "dead-letter",
                        });
                    }
                }
            } else {
                None
            };
            state.record_delivery_success(
                &record.session_id,
                endpoint,
                vault,
                &DeliverySuccess {
                    note_path: note.path.clone(),
                    delivered_revision: record.current_revision,
                    delivered_summary_hash: summary_hash.unwrap_or_default(),
                    remote_hash: note.hash,
                    history_commit,
                },
            )?;
            if created {
                Ok(DeliveryOutcome::Created {
                    note_path: note.path,
                    revision: record.current_revision,
                })
            } else {
                Ok(DeliveryOutcome::Replaced {
                    note_path: note.path,
                    revision: record.current_revision,
                })
            }
        }
        Err(error) => {
            let category = error.category();
            let updated = state.record_delivery_failure(
                &record.session_id,
                endpoint,
                vault,
                category,
                delivery.max_attempts.max(1),
                now_ms().saturating_add(backoff_ms(existing.attempts.saturating_add(1))),
            )?;
            Ok(DeliveryOutcome::Failed {
                category: category.to_owned(),
                dead_letter: updated.delivery_state == "dead-letter",
            })
        }
    }
}

/// The result of committing (or recovering the commit of) a delivered revision.
enum CommitCorrelation {
    Committed(HistoryCommit),
    Failed { category: &'static str },
}

/// Commits the delivered revision and correlates it to a durable commit id, recovering idempotently
/// from a lost commit response or a rebuilt operational database (issue #9).
///
/// The correlation uses the deterministic, source-qualified session+revision `message` and matches
/// it *exactly* against the vault history, so a commit is never confused with an unrelated one:
/// - a fresh commit is verified by looking its message up to capture the authoritative id and file
///   count;
/// - a `committed: false` no-op (the tree already matched — e.g. after a crash between the remote
///   commit and the local database write, or an idempotent replace) is recovered by finding the
///   existing commit; a missing commit is a failure, never a delivered-without-history success;
/// - a transport error (the commit may have landed before its response was lost) triggers a lookup
///   before the attempt is recorded as a failure.
fn commit_and_correlate(sink: &dyn NotesmithSink, message: &str) -> CommitCorrelation {
    match sink.commit_revision(message) {
        Ok(outcome) if outcome.committed => match sink.find_commit_by_message(message) {
            Ok(Some(commit)) => CommitCorrelation::Committed(commit),
            Ok(None) => match outcome.sha {
                // Committed, but the message could not be located (history rewritten between calls).
                // Fall back to the returned id; the revision is still preserved in history.
                Some(sha) => CommitCorrelation::Committed(HistoryCommit {
                    sha,
                    files_changed: 0,
                }),
                None => CommitCorrelation::Failed {
                    category: "remote-history-missing",
                },
            },
            Err(error) => CommitCorrelation::Failed {
                category: error.category(),
            },
        },
        Ok(_) => match sink.find_commit_by_message(message) {
            Ok(Some(commit)) => CommitCorrelation::Committed(commit),
            Ok(None) => CommitCorrelation::Failed {
                category: "remote-history-missing",
            },
            Err(error) => CommitCorrelation::Failed {
                category: error.category(),
            },
        },
        Err(error) => match sink.find_commit_by_message(message) {
            Ok(Some(commit)) => CommitCorrelation::Committed(commit),
            _ => CommitCorrelation::Failed {
                category: error.category(),
            },
        },
    }
}

/// Normalizes a vault-relative note path for comparison against `git/status` paths (which never
/// carry a leading slash).
fn normalize_note_path(path: &str) -> &str {
    path.trim_start_matches('/')
}

/// Replaces an existing note with the complete `document`, falling back to a create when the remote
/// note has been deleted.
fn replace_or_create(
    sink: &dyn NotesmithSink,
    path: &str,
    document: &str,
    create: &CreateNote<'_>,
    deterministic_path: &str,
) -> Result<(NoteRef, bool), SinkError> {
    match sink.replace_note(path, document) {
        Ok(note) => Ok((note, false)),
        Err(SinkError::NotFound) => create_or_adopt(sink, create, document, deterministic_path),
        Err(other) => Err(other),
    }
}

/// Creates a note from its body and identity frontmatter, adopting the deterministic path when the
/// note already exists (for example after a rebuilt operational database) by replacing it with the
/// complete `document`.
fn create_or_adopt(
    sink: &dyn NotesmithSink,
    create: &CreateNote<'_>,
    document: &str,
    deterministic_path: &str,
) -> Result<(NoteRef, bool), SinkError> {
    match sink.create_note(create) {
        Ok(note) => Ok((note, true)),
        Err(SinkError::AlreadyExists { path }) => {
            let adopt = path.unwrap_or_else(|| deterministic_path.to_owned());
            let note = sink.replace_note(&adopt, document)?;
            Ok((note, false))
        }
        Err(other) => Err(other),
    }
}

// ---------------------------------------------------------------------------
// Credential resolution
// ---------------------------------------------------------------------------

/// Resolves the bearer credential from the environment or the OS credential store. The credential
/// is never read from committed configuration.
fn resolve_credential(credential: &StoredCredential) -> Result<String, DeliveryError> {
    match credential {
        StoredCredential::Env { var } => std::env::var(var).map_err(|_| {
            DeliveryError::Credential(format!("environment variable {var} is not set"))
        }),
        StoredCredential::Keychain { service, account } => {
            let output = Command::new("security")
                .args(["find-generic-password", "-s", service, "-a", account, "-w"])
                .output()
                .map_err(|error| {
                    DeliveryError::Credential(format!("credential store lookup failed: {error}"))
                })?;
            if !output.status.success() {
                return Err(DeliveryError::Credential(format!(
                    "credential store has no entry for {service}/{account}"
                )));
            }
            let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if token.is_empty() {
                return Err(DeliveryError::Credential(
                    "credential store returned an empty token".to_owned(),
                ));
            }
            Ok(token)
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP Notesmith sink
// ---------------------------------------------------------------------------

/// A minimal blocking HTTP/1.1 Notesmith sink. Notesmith runs on localhost, so this deliberately
/// speaks plain HTTP over `std::net` and adds no async or TLS dependency.
pub struct HttpNotesmithSink {
    host: String,
    port: u16,
    vault: String,
    token: Option<String>,
    timeout: Duration,
}

impl HttpNotesmithSink {
    pub fn connect(
        endpoint: &str,
        vault: &str,
        token: Option<String>,
    ) -> Result<Self, DeliveryError> {
        let (host, port) = parse_http_endpoint(endpoint)?;
        Ok(Self {
            host,
            port,
            vault: vault.to_owned(),
            token,
            timeout: REQUEST_TIMEOUT,
        })
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, SinkError> {
        self.send_json(method, path, body, None)
    }

    /// Sends a JSON request, adding the `Accept`/`Content-Type`, optional bearer, and optional
    /// `If-Match` headers. The bearer token is passed as opaque header data and never logged.
    fn send_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        if_match: Option<&str>,
    ) -> Result<HttpResponse, SinkError> {
        let mut headers = vec![Header {
            name: "Accept",
            value: "application/json",
        }];
        let bearer;
        if let Some(token) = self.token.as_deref() {
            bearer = format!("Bearer {token}");
            headers.push(Header {
                name: "Authorization",
                value: &bearer,
            });
        }
        let etag;
        if let Some(if_match) = if_match {
            etag = format!("\"{if_match}\"");
            headers.push(Header {
                name: "If-Match",
                value: &etag,
            });
        }
        if body.is_some() {
            headers.push(Header {
                name: "Content-Type",
                value: "application/json",
            });
        }
        http::send(
            &self.host,
            self.port,
            self.timeout,
            &http::HttpRequest {
                method,
                path,
                headers: &headers,
                body,
            },
        )
        .map_err(sink_http_error)
    }
}

impl NotesmithSink for HttpNotesmithSink {
    fn create_note(&self, request: &CreateNote<'_>) -> Result<NoteRef, SinkError> {
        // Notesmith assembles the note from `content` (body only) plus a separate `frontmatter`
        // map, so the body must not carry its own frontmatter block.
        let frontmatter = identity_frontmatter(
            request.source,
            request.session_id,
            request.project_identity,
            request.revision,
        );
        let body = serde_json::json!({
            "title": request.title,
            "folder": request.folder,
            "content": request.body,
            "frontmatter": frontmatter,
        });
        let payload =
            serde_json::to_vec(&body).map_err(|error| SinkError::Protocol(error.to_string()))?;
        let response = self.request(
            "POST",
            &format!("/api/v/{}/notes", encode_path(&self.vault)),
            Some(&payload),
        )?;
        match response.status {
            200 | 201 => parse_note_ref(&response.body),
            409 => Err(SinkError::AlreadyExists { path: None }),
            status => Err(SinkError::Server {
                status,
                body: response.body_text(),
            }),
        }
    }

    fn replace_note(&self, path: &str, content: &str) -> Result<NoteRef, SinkError> {
        // No `expected_hash`: Munshi owns delivered notes and overwrites remote edits.
        let body = serde_json::json!({ "content": content });
        let payload =
            serde_json::to_vec(&body).map_err(|error| SinkError::Protocol(error.to_string()))?;
        let response = self.request(
            "PUT",
            &format!(
                "/api/v/{}/notes/{}",
                encode_path(&self.vault),
                encode_path(path)
            ),
            Some(&payload),
        )?;
        match response.status {
            200 => parse_note_ref(&response.body),
            404 => Err(SinkError::NotFound),
            status => Err(SinkError::Server {
                status,
                body: response.body_text(),
            }),
        }
    }

    fn history_capability(&self) -> Result<HistoryCapability, SinkError> {
        let enabled = self.vault_git_enabled()?;
        Ok(if enabled {
            HistoryCapability::available(false)
        } else {
            HistoryCapability::unavailable(
                "vault git history is disabled; enable [git] in the Notesmith vault config",
            )
        })
    }

    fn ensure_history_capability(&self) -> Result<HistoryCapability, SinkError> {
        let (config, hash) = self.vault_config()?;
        if git_enabled_in_config(&config) {
            return Ok(HistoryCapability::available(false));
        }
        // Explicitly configure the capability: flip `git.enabled` in the vault config and PUT it
        // back with the ETag Notesmith returned. Enabling git auto-initializes the repository
        // (notes-method routes/config.rs), so commits become possible without further setup.
        let mut updated = config;
        set_git_enabled(&mut updated, true)?;
        let payload =
            serde_json::to_vec(&updated).map_err(|error| SinkError::Protocol(error.to_string()))?;
        let response = self.send_json(
            "PUT",
            &format!("/api/v/{}/config", encode_path(&self.vault)),
            Some(&payload),
            Some(&hash),
        )?;
        match response.status {
            200 => {
                let value: serde_json::Value = serde_json::from_slice(&response.body)
                    .map_err(|error| SinkError::Protocol(error.to_string()))?;
                if value.get("config").is_some_and(git_enabled_in_config) {
                    Ok(HistoryCapability::available(true))
                } else {
                    Err(SinkError::HistoryUnavailable(
                        "vault config was saved but git history is still disabled".to_owned(),
                    ))
                }
            }
            status => Err(SinkError::HistoryUnavailable(format!(
                "could not enable vault git history (status {status})"
            ))),
        }
    }

    fn history_status(&self) -> Result<HistoryStatus, SinkError> {
        let response = self.request(
            "GET",
            &format!("/api/v/{}/git/status", encode_path(&self.vault)),
            None,
        )?;
        match response.status {
            200 => {
                let value: serde_json::Value = serde_json::from_slice(&response.body)
                    .map_err(|error| SinkError::Protocol(error.to_string()))?;
                let mut dirty_paths = Vec::new();
                for key in ["changed", "staged", "untracked"] {
                    if let Some(array) = value.get(key).and_then(serde_json::Value::as_array) {
                        for path in array.iter().filter_map(serde_json::Value::as_str) {
                            dirty_paths.push(path.to_owned());
                        }
                    }
                }
                let clean = value
                    .get("clean")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(dirty_paths.is_empty());
                Ok(HistoryStatus { clean, dirty_paths })
            }
            // 400 means the vault is not a git repository — treat as no history capability.
            400 => Err(SinkError::HistoryUnavailable(
                "vault is not a git repository".to_owned(),
            )),
            status => Err(SinkError::Server {
                status,
                body: response.body_text(),
            }),
        }
    }

    fn commit_revision(&self, message: &str) -> Result<CommitOutcome, SinkError> {
        let body = serde_json::json!({ "message": message });
        let payload =
            serde_json::to_vec(&body).map_err(|error| SinkError::Protocol(error.to_string()))?;
        let response = self.request(
            "POST",
            &format!("/api/v/{}/git/commit", encode_path(&self.vault)),
            Some(&payload),
        )?;
        match response.status {
            200 => {
                let value: serde_json::Value = serde_json::from_slice(&response.body)
                    .map_err(|error| SinkError::Protocol(error.to_string()))?;
                let committed = value
                    .get("committed")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let sha = value
                    .get("sha")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned);
                Ok(CommitOutcome { committed, sha })
            }
            // Notesmith returns 400 when git is not enabled or the vault is not a repository.
            400 => Err(SinkError::HistoryUnavailable(
                "vault git history is not enabled".to_owned(),
            )),
            status => Err(SinkError::Server {
                status,
                body: response.body_text(),
            }),
        }
    }

    fn find_commit_by_message(&self, message: &str) -> Result<Option<HistoryCommit>, SinkError> {
        let response = self.request(
            "GET",
            &format!("/api/v/{}/git/log?limit=500", encode_path(&self.vault)),
            None,
        )?;
        match response.status {
            200 => {
                let value: serde_json::Value = serde_json::from_slice(&response.body)
                    .map_err(|error| SinkError::Protocol(error.to_string()))?;
                let entries = value
                    .as_array()
                    .ok_or_else(|| SinkError::Protocol("git log is not an array".to_owned()))?;
                for entry in entries {
                    // Exact subject match only — never a prefix or substring — so a correlation
                    // message is never confused with another commit that merely contains it.
                    let subject = entry.get("subject").and_then(serde_json::Value::as_str);
                    if subject == Some(message) {
                        let sha = entry
                            .get("sha")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| {
                                SinkError::Protocol("git log entry missing sha".to_owned())
                            })?
                            .to_owned();
                        let files_changed = entry
                            .get("filesChanged")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0) as usize;
                        return Ok(Some(HistoryCommit { sha, files_changed }));
                    }
                }
                Ok(None)
            }
            400 => Err(SinkError::HistoryUnavailable(
                "vault is not a git repository".to_owned(),
            )),
            status => Err(SinkError::Server {
                status,
                body: response.body_text(),
            }),
        }
    }
}

impl HttpNotesmithSink {
    /// Reads the vault's `git.enabled` flag from its Notesmith config.
    fn vault_git_enabled(&self) -> Result<bool, SinkError> {
        let (config, _hash) = self.vault_config()?;
        Ok(git_enabled_in_config(&config))
    }

    /// Fetches the vault config document and its ETag hash for conflict-safe updates.
    fn vault_config(&self) -> Result<(serde_json::Value, String), SinkError> {
        let response = self.request(
            "GET",
            &format!("/api/v/{}/config", encode_path(&self.vault)),
            None,
        )?;
        match response.status {
            200 => {
                let value: serde_json::Value = serde_json::from_slice(&response.body)
                    .map_err(|error| SinkError::Protocol(error.to_string()))?;
                let config = value.get("config").cloned().ok_or_else(|| {
                    SinkError::Protocol("config response missing config".to_owned())
                })?;
                let hash = value
                    .get("hash")
                    .and_then(|value| value.as_str())
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| {
                        SinkError::Protocol("config response missing hash".to_owned())
                    })?;
                Ok((config, hash))
            }
            status => Err(SinkError::Server {
                status,
                body: response.body_text(),
            }),
        }
    }
}

/// Reads `config.git.enabled` from a Notesmith vault config document.
fn git_enabled_in_config(config: &serde_json::Value) -> bool {
    config
        .get("git")
        .and_then(|git| git.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Sets `config.git.enabled`, creating the `git` object when the config omits it.
fn set_git_enabled(config: &mut serde_json::Value, enabled: bool) -> Result<(), SinkError> {
    let object = config
        .as_object_mut()
        .ok_or_else(|| SinkError::Protocol("vault config is not a JSON object".to_owned()))?;
    let git = object.entry("git").or_insert_with(|| serde_json::json!({}));
    let git = git.as_object_mut().ok_or_else(|| {
        SinkError::Protocol("vault config git section is not an object".to_owned())
    })?;
    git.insert("enabled".to_owned(), serde_json::json!(enabled));
    Ok(())
}

fn parse_note_ref(body: &[u8]) -> Result<NoteRef, SinkError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|error| SinkError::Protocol(error.to_string()))?;
    let path = value
        .get("path")
        .and_then(|value| value.as_str())
        .ok_or_else(|| SinkError::Protocol("response is missing note path".to_owned()))?
        .to_owned();
    let hash = value
        .get("hash")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    Ok(NoteRef { path, hash })
}

/// Maps a shared-transport [`HttpError`] onto a [`SinkError`] retry category.
fn sink_http_error(error: HttpError) -> SinkError {
    match error {
        HttpError::Protocol(message) => SinkError::Protocol(message),
        HttpError::Transport(message) | HttpError::UnsupportedEndpoint(message) => {
            SinkError::Transport(message)
        }
    }
}

// ---------------------------------------------------------------------------
// Public configuration + run entry points
// ---------------------------------------------------------------------------

/// Builds an HTTP sink from configuration, resolving the credential from the environment or OS
/// credential store. Returns an error if the sink is not addressable or the credential is missing.
fn sink_from_config(config: &StoredConfig) -> Result<HttpNotesmithSink, DeliveryError> {
    let endpoint = config
        .summary_delivery
        .endpoint
        .as_deref()
        .ok_or(DeliveryError::NotConfigured)?;
    let vault = config
        .summary_delivery
        .vault
        .as_deref()
        .ok_or(DeliveryError::NotConfigured)?;
    let token = match config.summary_delivery.credential.as_ref() {
        Some(credential) => Some(resolve_credential(credential)?),
        None => None,
    };
    HttpNotesmithSink::connect(endpoint, vault, token)
}

/// Best-effort delivery of a freshly archived summary, invoked by the archive worker.
///
/// Delivery is strictly downstream of a successful local archive (ADR 0002): this returns
/// `Ok(None)` when delivery is disabled or unconfigured, records network/sink failures as a bounded
/// retry inside operational state, and never mutates the session's archival lifecycle. Credential
/// or connection errors surface to the caller so it can record a safe diagnostic.
/// Resolves the versioned-delivery [`HistoryGate`] for a sink.
///
/// When local archive Git history is enabled alongside delivery, versioned delivery is mandatory
/// (issue #9). This verifies the sink vault's revision-history capability, or explicitly configures
/// it when `provision` is set. If the capability is absent or cannot be verified, the gate blocks
/// delivery with an actionable reason rather than degrading to latest-only storage.
pub(crate) fn resolve_history_gate(
    sink: &dyn NotesmithSink,
    required: bool,
    provision: bool,
) -> HistoryGate {
    if !required {
        return HistoryGate::NotRequired;
    }
    let capability = if provision {
        sink.ensure_history_capability()
    } else {
        sink.history_capability()
    };
    match capability {
        Ok(capability) if capability.available => HistoryGate::Available,
        Ok(capability) => HistoryGate::Blocked {
            reason: "remote-history-unavailable".to_owned(),
            detail: capability.detail,
        },
        Err(error) => HistoryGate::Blocked {
            reason: error.category().to_owned(),
            detail: Some(error.to_string()),
        },
    }
}

/// Whether versioned delivery is required: local archive Git history is enabled *and* delivery is
/// enabled. In that mode the remote must preserve correlated revision history (issue #9).
fn history_required(config: &StoredConfig) -> bool {
    config.archive_git_history && config.summary_delivery.enabled
}

pub(crate) fn deliver_after_archive(
    state: &mut StateStore,
    config: &StoredConfig,
    session_id: &str,
) -> Result<Option<DeliveryOutcome>, DeliveryError> {
    if !config.summary_delivery.enabled || !config.summary_delivery.is_addressable() {
        return Ok(None);
    }
    let Some(record) = state.get_session(session_id)? else {
        return Ok(None);
    };
    let sink = sink_from_config(config)?;
    let gate = resolve_history_gate(
        &sink,
        history_required(config),
        config.summary_delivery.provision_history,
    );
    let output_directory = PathBuf::from(&config.output_directory);
    let outcome = deliver_one(
        state,
        &sink,
        &config.summary_delivery,
        &config.policy.disabled_projects,
        &output_directory,
        &record,
        &gate,
    )?;
    Ok(Some(outcome))
}

/// Reads the safe delivery settings view from the Munshi-owned configuration.
pub fn load_settings(state_directory: &Path) -> Result<DeliverySettings, DeliveryError> {
    let config = load_stored_config(state_directory)?;
    Ok(DeliverySettings::from_config(&config))
}

/// The `munshi summary-delivery history` contract: reports the remote revision-history capability and,
/// with `configure`, explicitly enables it when absent.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub settings: DeliverySettings,
    /// Whether versioned delivery is currently required (local Git history + delivery enabled).
    pub required: bool,
    /// Whether this invocation asked Munshi to explicitly configure the capability.
    pub configure_requested: bool,
    /// The probed capability, when the sink was reachable.
    pub capability: Option<HistoryCapability>,
    /// A stable status token: `available`, `configured`, `unavailable`, or `unreachable`.
    pub status: String,
    /// A safe, secret-free human-readable summary.
    pub message: String,
}

impl HistoryReport {
    /// `true` when versioned delivery would proceed (capability present); `false` when it would be
    /// blocked. Callers use this to choose an exit code.
    pub fn ok(&self) -> bool {
        matches!(self.status.as_str(), "available" | "configured")
    }

    pub fn print_human(&self) {
        print_settings(&self.settings);
        println!(
            "remote history: {} (required={}, configure={})",
            self.status, self.required, self.configure_requested
        );
        println!("{}", self.message);
    }
}

/// Verifies (or, with `configure`, explicitly configures) the Notesmith vault's revision-history
/// capability that versioned delivery depends on (issue #9). Never mutates local archival state.
pub fn verify_history(
    state_directory: &Path,
    configure: bool,
) -> Result<HistoryReport, DeliveryError> {
    let config = load_stored_config(state_directory)?;
    let settings = DeliverySettings::from_config(&config);
    if !config.summary_delivery.is_addressable() {
        return Err(DeliveryError::NotConfigured);
    }
    let required = history_required(&config);
    let provision = configure || config.summary_delivery.provision_history;
    let sink = sink_from_config(&config)?;
    let probe = if provision {
        sink.ensure_history_capability()
    } else {
        sink.history_capability()
    };
    let (capability, status, message) = match probe {
        Ok(capability) if capability.available => {
            let status = if capability.configured {
                "configured"
            } else {
                "available"
            };
            let message = format!(
                "remote revision history is {status} via {} — versioned delivery can preserve correlated history",
                capability.mechanism
            );
            (Some(capability), status.to_owned(), message)
        }
        Ok(capability) => {
            let message = capability.detail.clone().unwrap_or_else(|| {
                "remote revision history is unavailable; versioned delivery will be blocked"
                    .to_owned()
            });
            (Some(capability), "unavailable".to_owned(), message)
        }
        Err(error) => (None, "unreachable".to_owned(), error.to_string()),
    };
    Ok(HistoryReport {
        schema_version: 1,
        command: "summary-delivery-history",
        settings,
        required,
        configure_requested: configure,
        capability,
        status,
        message,
    })
}

/// Configures the Notesmith sink without enabling delivery. The credential source is recorded, but
/// never the secret itself.
pub fn configure_sink(
    state_directory: &Path,
    sink: DeliverySinkConfig,
) -> Result<DeliverySettings, DeliveryError> {
    // Validate the endpoint eagerly so a bad URL is rejected at configure time.
    parse_http_endpoint(&sink.endpoint)?;
    let (config, ()) = update_stored_config(state_directory, |config| {
        config.summary_delivery.endpoint = Some(sink.endpoint.clone());
        config.summary_delivery.vault = Some(sink.vault.clone());
        config.summary_delivery.folder = sink.folder.clone().filter(|value| !value.is_empty());
        config.summary_delivery.credential = sink
            .credential
            .as_ref()
            .map(DeliveryCredentialSource::to_stored);
        config.summary_delivery.max_attempts = sink
            .max_attempts
            .filter(|value| *value >= 1)
            .unwrap_or(DEFAULT_MAX_DELIVERY_ATTEMPTS);
        if let Some(provision) = sink.provision_history {
            config.summary_delivery.provision_history = provision;
        }
        Ok(())
    })?;
    Ok(DeliverySettings::from_config(&config))
}

/// Enables or disables delivery. Disabling stops future delivery while retaining delivery history.
pub fn set_enabled(
    state_directory: &Path,
    enabled: bool,
) -> Result<DeliverySettings, DeliveryError> {
    let result = update_stored_config(state_directory, |config| {
        if enabled && !config.summary_delivery.is_addressable() {
            return Err(RegistrationError::MalformedOwnedFile);
        }
        config.summary_delivery.enabled = enabled;
        Ok(())
    });
    let (config, ()) = match result {
        Ok(value) => value,
        Err(RegistrationError::MalformedOwnedFile) if enabled => {
            return Err(DeliveryError::NotConfigured);
        }
        Err(error) => return Err(DeliveryError::Registration(error)),
    };
    Ok(DeliverySettings::from_config(&config))
}

/// Builds the status contract: current settings plus every recorded delivery.
///
/// A state directory that has never been registered (no `config.json` yet — the common case for
/// an external caller such as Madari probing a project it hasn't set up Munshi in) degrades to an
/// empty, disabled report instead of an I/O error, matching `sessions`/`status`/`show`/`retry`,
/// which already return a valid `schema_version: 1` contract with no registration present.
pub fn status(state_directory: &Path) -> Result<DeliveryStatusReport, DeliveryError> {
    if !stored_config_exists(state_directory) {
        return Ok(DeliveryStatusReport {
            schema_version: 1,
            command: "summary-delivery-status",
            settings: DeliverySettings::unregistered(),
            total: 0,
            delivered: 0,
            pending: 0,
            failed: 0,
            dead_letter: 0,
            blocked: 0,
            items: Vec::new(),
        });
    }
    let config = load_stored_config(state_directory)?;
    let settings = DeliverySettings::from_config(&config);
    let deliveries = if StateStore::database_path(state_directory).exists() {
        StateStore::open(state_directory)?.list_deliveries()?
    } else {
        Vec::new()
    };

    let mut delivered = 0;
    let mut pending = 0;
    let mut failed = 0;
    let mut dead_letter = 0;
    let mut blocked = 0;
    let items = deliveries
        .iter()
        .map(|record| {
            match record.delivery_state.as_str() {
                "delivered" => delivered += 1,
                "pending" => pending += 1,
                "failed" => failed += 1,
                "dead-letter" => dead_letter += 1,
                "blocked" => blocked += 1,
                _ => {}
            }
            delivery_item(record)
        })
        .collect::<Vec<_>>();

    Ok(DeliveryStatusReport {
        schema_version: 1,
        command: "summary-delivery-status",
        settings,
        total: items.len(),
        delivered,
        pending,
        failed,
        dead_letter,
        blocked,
        items,
    })
}

fn delivery_item(record: &DeliveryRecord) -> DeliveryItem {
    let note_link = record.note_path.as_ref().map(|path| {
        format!(
            "notesmith://app/v/{}/{}",
            record.vault,
            path.trim_start_matches('/')
        )
    });
    DeliveryItem {
        source: record.source.as_selector().to_owned(),
        session_id: record.session_id.clone(),
        state: record.delivery_state.clone(),
        note_path: record.note_path.clone(),
        note_link,
        delivered_revision: record.delivered_revision,
        history_commit: record.history_commit.clone(),
        attempts: record.attempts,
        next_attempt_at_ms: record.next_attempt_at_ms,
        last_error_category: record.last_error_category.clone(),
    }
}

/// Which sessions a backfill or retry run should consider.
enum Selection {
    /// Every archived session with no successful current-revision delivery (backfill).
    Backfill,
    /// One session, addressed by optional source and session id (retry).
    One {
        source: Option<crate::source::SourceKind>,
        session_id: String,
    },
    /// Every failed/dead-letter delivery (retry --all).
    Failed,
}

/// Confirms and runs a delivery backfill over existing current archives. When `confirm` is false
/// the run is a dry run that only reports the candidate count and never contacts the sink.
pub fn backfill(
    state_directory: &Path,
    confirm: bool,
    limit: usize,
) -> Result<DeliveryRunReport, DeliveryError> {
    run(
        state_directory,
        "summary-delivery-backfill",
        Selection::Backfill,
        confirm,
        false,
        limit,
    )
}

/// Retries failed or dead-letter deliveries. `force` revives dead letters and resets their bounded
/// attempt count.
pub fn retry(
    state_directory: &Path,
    source: Option<crate::source::SourceKind>,
    session_id: Option<String>,
    all: bool,
    force: bool,
    limit: usize,
) -> Result<DeliveryRunReport, DeliveryError> {
    let selection = match session_id {
        Some(session_id) => Selection::One { source, session_id },
        None if all => Selection::Failed,
        None => Selection::Failed,
    };
    run(
        state_directory,
        "summary-delivery-retry",
        selection,
        true,
        force,
        limit,
    )
}

fn run(
    state_directory: &Path,
    command: &'static str,
    selection: Selection,
    confirm: bool,
    force: bool,
    limit: usize,
) -> Result<DeliveryRunReport, DeliveryError> {
    let config = load_stored_config(state_directory)?;
    let settings = DeliverySettings::from_config(&config);
    if !config.summary_delivery.enabled {
        return Err(DeliveryError::NotEnabled);
    }
    if !config.summary_delivery.is_addressable() {
        return Err(DeliveryError::NotConfigured);
    }
    let endpoint = config.summary_delivery.endpoint.clone().unwrap();
    let vault = config.summary_delivery.vault.clone().unwrap();
    let output_directory = PathBuf::from(&config.output_directory);

    let candidates = if StateStore::database_path(state_directory).exists() {
        select_candidates(
            state_directory,
            &config,
            &endpoint,
            &vault,
            &selection,
            force,
        )?
    } else {
        Vec::new()
    };
    let mut candidates = candidates;
    candidates.truncate(limit);

    let mut report = DeliveryRunReport {
        schema_version: 1,
        command,
        confirmed: confirm,
        settings,
        candidates: candidates.len(),
        created: 0,
        replaced: 0,
        already_delivered: 0,
        skipped: 0,
        blocked: 0,
        failed: 0,
        items: Vec::new(),
    };

    if !confirm {
        // Dry run: report the count without contacting the sink or resolving credentials.
        return Ok(report);
    }

    let token = match config.summary_delivery.credential.as_ref() {
        Some(credential) => Some(resolve_credential(credential)?),
        None => None,
    };
    let sink = HttpNotesmithSink::connect(&endpoint, &vault, token)?;
    // Resolve the versioned-delivery capability once for the whole run so every candidate is
    // gated consistently (issue #9): either all versioned deliveries are preserved as correlated
    // history, or they are all blocked with an actionable status — never silently latest-only.
    let gate = resolve_history_gate(
        &sink,
        history_required(&config),
        config.summary_delivery.provision_history,
    );

    for record in candidates {
        let mut state = StateStore::open_for_source(state_directory, record.source)?;
        if force {
            state.reset_delivery_for_retry(&record.session_id, &endpoint, &vault, true)?;
        }
        let outcome = deliver_one(
            &mut state,
            &sink,
            &config.summary_delivery,
            &config.policy.disabled_projects,
            &output_directory,
            &record,
            &gate,
        )?;
        match &outcome {
            DeliveryOutcome::Created { .. } => report.created += 1,
            DeliveryOutcome::Replaced { .. } => report.replaced += 1,
            DeliveryOutcome::AlreadyDelivered { .. } => report.already_delivered += 1,
            DeliveryOutcome::Skipped { .. } => report.skipped += 1,
            DeliveryOutcome::Blocked { .. } => report.blocked += 1,
            DeliveryOutcome::Failed { .. } => report.failed += 1,
        }
        let _ = outcome.as_kind();
        report.items.push(DeliveryRunItem {
            source: record.source.as_selector().to_owned(),
            session_id: record.session_id.clone(),
            outcome,
        });
    }

    Ok(report)
}

fn select_candidates(
    state_directory: &Path,
    config: &StoredConfig,
    endpoint: &str,
    vault: &str,
    selection: &Selection,
    force: bool,
) -> Result<Vec<SessionRecord>, DeliveryError> {
    let sessions = StateStore::open(state_directory)?.list_sessions()?;
    // Delivery rows are resolved through a session's own source scope, so a per-source store cache
    // is used to look up each record's delivery — a Copilot-scoped store cannot see Claude/Codex
    // delivery rows.
    let mut stores: std::collections::BTreeMap<crate::source::SourceKind, StateStore> =
        std::collections::BTreeMap::new();
    let deliverable = |record: &SessionRecord| -> bool {
        record.current_revision > 0
            && record.markdown_relative_path.is_some()
            && record
                .project
                .as_ref()
                .is_some_and(|project| !config.policy.disabled_projects.contains(&project.identity))
    };

    let mut selected = Vec::new();
    for record in sessions {
        if !deliverable(&record) {
            continue;
        }
        let store = match stores.entry(record.source) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(StateStore::open_for_source(state_directory, record.source)?)
            }
        };
        let existing = store.get_delivery(&record.session_id, endpoint, vault)?;
        let include = match selection {
            Selection::Backfill => match existing {
                Some(delivery) => !is_current_delivery(&delivery, &record),
                None => true,
            },
            Selection::Failed => existing.as_ref().is_some_and(|delivery| {
                delivery.delivery_state == "failed"
                    || delivery.delivery_state == "blocked"
                    || (delivery.delivery_state == "dead-letter" && force)
            }),
            Selection::One { source, session_id } => {
                record.session_id == *session_id
                    && source.is_none_or(|wanted| record.source == wanted)
            }
        };
        if include {
            selected.push(record);
        }
    }
    Ok(selected)
}

fn is_current_delivery(delivery: &DeliveryRecord, record: &SessionRecord) -> bool {
    delivery.delivery_state == "delivered"
        && delivery.delivered_revision == Some(record.current_revision)
        && delivery.delivered_summary_hash.is_some()
        && delivery.delivered_summary_hash == record.current_summary_hash
}

impl DeliveryStatusReport {
    pub fn print_human(&self) {
        print_settings(&self.settings);
        println!(
            "deliveries total={} delivered={} pending={} failed={} dead-letter={} blocked={}",
            self.total, self.delivered, self.pending, self.failed, self.dead_letter, self.blocked
        );
        for item in &self.items {
            println!(
                "{}  {}  {}{}{}",
                item.session_id,
                item.state,
                item.note_path.as_deref().unwrap_or("<no-note>"),
                item.delivered_revision
                    .map(|revision| format!(" rev={revision}"))
                    .unwrap_or_default(),
                item.last_error_category
                    .as_deref()
                    .map(|code| format!(" error={code}"))
                    .unwrap_or_default(),
            );
        }
    }
}

impl DeliveryRunReport {
    pub fn print_human(&self) {
        print_settings(&self.settings);
        if self.confirmed {
            println!(
                "summary delivery run candidates={} created={} replaced={} already-delivered={} skipped={} blocked={} failed={}",
                self.candidates,
                self.created,
                self.replaced,
                self.already_delivered,
                self.skipped,
                self.blocked,
                self.failed
            );
        } else {
            println!(
                "dry run: {} summar{} would be published; re-run with --confirm to deliver",
                self.candidates,
                if self.candidates == 1 { "y" } else { "ies" }
            );
        }
        for item in &self.items {
            println!("{} -> {}", item.session_id, item.outcome.as_kind());
        }
    }
}

fn print_settings(settings: &DeliverySettings) {
    println!(
        "summary delivery {} (endpoint {}, vault {}, folder {}, credential {}, max-attempts {}, versioned {}, provision-history {})",
        if settings.enabled {
            "enabled"
        } else {
            "disabled"
        },
        settings.endpoint.as_deref().unwrap_or("<unset>"),
        settings.vault.as_deref().unwrap_or("<unset>"),
        settings.folder.as_deref().unwrap_or("<none>"),
        settings.credential_source.as_deref().unwrap_or("<none>"),
        settings.max_attempts,
        settings.versioned,
        settings.provision_history,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_route_uses_stable_project_and_session_identity() {
        let (dir, title) = note_route(Some("Munshi"), "acme-abc123", "copilot", "sess-1");
        assert_eq!(dir, "Munshi/acme-abc123");
        assert_eq!(title, "copilot-sess-1");
        assert_eq!(
            note_path(Some("Munshi"), "acme-abc123", "copilot", "sess-1"),
            "Munshi/acme-abc123/copilot-sess-1.md"
        );
    }

    #[test]
    fn note_route_without_folder_files_under_the_component() {
        let (dir, title) = note_route(None, "acme-abc123", "codex", "sess-2");
        assert_eq!(dir, "acme-abc123");
        assert_eq!(title, "codex-sess-2");
        // An empty folder is treated the same as no folder.
        assert_eq!(
            note_route(Some(""), "c", "codex", "s"),
            ("c".to_owned(), "codex-s".to_owned())
        );
    }

    #[test]
    fn backoff_is_bounded_and_monotonic() {
        assert_eq!(backoff_ms(1), BASE_BACKOFF_MS);
        assert_eq!(backoff_ms(2), BASE_BACKOFF_MS * 2);
        assert!(backoff_ms(3) > backoff_ms(2));
        assert_eq!(backoff_ms(100), MAX_BACKOFF_MS);
    }

    #[test]
    fn archive_body_strips_the_leading_frontmatter_block() {
        let archive =
            "---\nschema_version: 2\nid: \"copilot:abc\"\ntags: []\n---\n\n# Title\n\nBody.";
        assert_eq!(archive_body(archive), "# Title\n\nBody.");
        // Documents without frontmatter are returned unchanged.
        assert_eq!(archive_body("# Just a body"), "# Just a body");
    }

    #[test]
    fn delivery_document_has_exactly_one_frontmatter_block_with_identity() {
        let map = identity_frontmatter("copilot", "sess-1", "github.com/o/r", 3);
        let document = delivery_document(&map, "# Title\n\nBody.");
        // Exactly one opening and one closing frontmatter delimiter.
        assert_eq!(document.matches("\n---\n").count() + 1, 2);
        assert!(document.starts_with("---\n"));
        assert!(document.contains("munshi_session: \"sess-1\""));
        assert!(document.contains("munshi_source: \"copilot\""));
        assert!(document.contains("munshi_project: \"github.com/o/r\""));
        assert!(document.contains("munshi_revision: 3"));
        assert!(document.trim_end().ends_with("Body."));
    }

    #[test]
    fn yaml_quote_escapes_quotes_and_backslashes() {
        assert_eq!(yaml_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn reads_and_sets_vault_git_enabled_in_config() {
        let mut config = serde_json::json!({ "name": "work", "git": { "enabled": false } });
        assert!(!git_enabled_in_config(&config));
        set_git_enabled(&mut config, true).unwrap();
        assert!(git_enabled_in_config(&config));
        // A config that omits the git section defaults to disabled and gains one when set.
        let mut bare = serde_json::json!({ "name": "work" });
        assert!(!git_enabled_in_config(&bare));
        set_git_enabled(&mut bare, true).unwrap();
        assert_eq!(bare["git"]["enabled"], serde_json::json!(true));
    }

    #[test]
    fn history_capability_helpers_report_mechanism_and_configured_flag() {
        let available = HistoryCapability::available(true);
        assert!(available.available);
        assert!(available.configured);
        assert_eq!(available.mechanism, "git");
        let unavailable = HistoryCapability::unavailable("disabled");
        assert!(!unavailable.available);
        assert!(!unavailable.configured);
        assert_eq!(unavailable.detail.as_deref(), Some("disabled"));
    }

    #[test]
    fn resolves_a_credential_from_the_environment() {
        // SAFETY: single-threaded unit test manipulating a uniquely-named variable.
        unsafe {
            std::env::set_var("MUNSHI_TEST_DELIVERY_TOKEN", "secret-token");
        }
        let token = resolve_credential(&StoredCredential::Env {
            var: "MUNSHI_TEST_DELIVERY_TOKEN".to_owned(),
        })
        .unwrap();
        assert_eq!(token, "secret-token");
        unsafe {
            std::env::remove_var("MUNSHI_TEST_DELIVERY_TOKEN");
        }
        assert!(
            resolve_credential(&StoredCredential::Env {
                var: "MUNSHI_TEST_DELIVERY_TOKEN".to_owned(),
            })
            .is_err()
        );
    }
}
