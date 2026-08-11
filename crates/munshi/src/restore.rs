//! Record restore: `munshi restore` (issue #70).
//!
//! Patwari holds a verified, self-contained snapshot of every archived session (ADR 0009), but
//! every read primitive Munshi had was addressed one artifact at a time: `retrieve` needs a hash
//! you already know, `verify-archive-parse` downloads to audit and keeps nothing, and
//! `hook recover --rebuild-state` rebuilds operational state from *local* Markdown — precisely what
//! a wiped machine no longer has. This module closes that loop: it walks the archive, reproduces
//! the local record the archival path would have written, and hands the result to the existing
//! rebuild authority.
//!
//! # What "the local record" means here
//!
//! Local archival writes exactly two things per session: the rendered `summary.md` at
//! `<component>/[<source-prefix>/]<session-id>.md`, and, for harnesses that stage them, the
//! sidecar set beside it in `<session-id>.sidecar/`. The verbatim transcript lives in the *harness
//! home*, not the archive, and extracted outputs are re-derived from it on demand and never stored
//! locally at all. A restore therefore has two kinds of artifact to place:
//!
//! - `summary.md` and `sidecar/<path>` go exactly where archival puts them, so the existing
//!   rebuild path recognizes them and a later archive upload re-assembles a byte-identical
//!   snapshot from the restored files. Restore places sidecars file by file and, unlike archival's
//!   staging, never clears the directory first: archival replaces a revision's whole set and may
//!   drop a checkpoint the harness deleted, whereas restore is a recovery operation with no
//!   business deleting local files it was not asked about.
//! - `transcript.jsonl` and `outputs/<sha256>` have no archival-produced home, so they land in a
//!   sibling `<session-id>.restored/` directory. Nothing else writes there, which keeps restore
//!   from ever colliding with the archival path, and the transcript's location is then recorded on
//!   the session row so the record reads as a whole (see *State*, below).
//!
//! # One snapshot per session
//!
//! A session accumulates one snapshot per summary revision, but locally only the newest revision
//! has a home — `<session-id>.md` is replaced in place. Restore therefore reproduces the newest
//! snapshot per session and counts the rest as superseded. Patwari orders the snapshot listing
//! newest-first, so the first snapshot seen for a session is the one to restore; restoring an older
//! revision on top would either clobber the newer record or force a second, invented layout for
//! history that no local reader knows how to interpret.
//!
//! # Idempotent and resumable
//!
//! Every artifact's archived original sha256 is known from the listing before any transfer, so a
//! local copy is compared by *hash*, not by existence: a matching file is skipped without a byte
//! crossing the wire, a differing file is refused unless `--force`, and an absent file is
//! downloaded through the shared three-stage verified stack and written atomically. An interrupted
//! run therefore resumes by rerunning, and a completed run rerun transfers nothing — including the
//! summary, which would otherwise have to be fetched every time just to learn where its record
//! belongs (see [`LocalRecordIndex`]).
//!
//! # State
//!
//! Restored Markdown is fed to the same importer `rebuild-state` uses
//! ([`StateStore::hydrate_session_from_archives`]), one session at a time rather than through the
//! whole-database rebuild: the per-session path is non-destructive, where
//! [`crate::state::rebuild_database`] renames the existing database aside and loses upload and
//! delivery history that a restore has no business discarding. Imported rows land `archived` with
//! no observation, so they are never claimable and can never park as `transcript-missing` (issue
//! #58) or deadlock as origin-less interrupted work (issue #39). Because the transcript restored
//! here sits at a restored path rather than a harness-home path, the row's re-derivation (issue
//! #53) finds nothing, so restore records the restored path itself — but only for a session with no
//! readable transcript already, so a healthy machine's live harness path is never overwritten by an
//! archived copy.
//!
//! # Registration
//!
//! Restore needs two things to run at all: somewhere to read from and somewhere to write to. Both
//! come from a registration when there is one and from `--endpoint` / `--output-dir` when there is
//! not, so a brand-new machine can recover its record before it is registered. State import is the
//! part that genuinely requires a registration — the harness homes it derives from live in the
//! stored configuration — so on an unregistered machine it is reported as skipped with the command
//! to run afterwards, never guessed at.
//!
//! # Exit codes
//!
//! Mirroring `retrieve` and `verify-archive-parse`, each failure class has a distinct, stable
//! process exit code:
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | every selected snapshot restored; nothing refused, skipped, or failed |
//! | 1 | local error (configuration, or writing the restored record) |
//! | 2 | invalid input |
//! | 3 | no archive server configured, or no archive output directory known |
//! | 4 | findings: refused overwrites, skipped artifacts, or a `--session` that matched nothing |
//! | 5 | server/transport failure |
//! | 6 | verification/integrity failure on at least one artifact |
//!
//! A completed run that observed several classes reports the most severe: verification (6) over
//! transport (5) over local (1) over findings (4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

use munshi_transcript::{MIN_SUPPORTED_ARTIFACT_SET_VERSION, SUPPORTED_ARTIFACT_SET_VERSION};

use crate::http::HttpError;
use crate::patwari::{
    self, PatwariError, SIDECAR_LOGICAL_PREFIX, SUMMARY_LOGICAL_PATH, TRANSCRIPT_LOGICAL_PATH,
};
use crate::patwari_read::{
    DownloadError, ListedSnapshot, MAX_ARCHIVE_LISTING_PAGES, MAX_ARTIFACT_DOWNLOAD_BYTES,
    ReadClient, ReadError, SizeDimension, SizeRefusal, SnapshotArtifact, sha256_hex,
};
use crate::registration::load_stored_config;
use crate::render::{
    ArchivedMarkdown, archive_relative_path, atomic_replace, parse_archive_markdown,
    restored_relative_directory, sidecar_relative_directory,
};
use crate::source::{SourceHomes, SourceKind, validate_session_id};
use crate::state::StateStore;

/// The logical-path prefix of extracted-output artifacts (ADR 0010).
const OUTPUTS_LOGICAL_PREFIX: &str = "outputs/";
/// The restored transcript's file name inside a session's restored-artifact directory. It keeps the
/// snapshot's own logical path so the directory reads as what it is — a snapshot mirror.
const RESTORED_TRANSCRIPT_FILE: &str = "transcript.jsonl";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// One `munshi restore` invocation. Both overrides exist so a machine with no registration at all —
/// the disaster-recovery case this command is for — can still name a server and an output
/// directory explicitly.
#[derive(Debug, Clone)]
pub struct RestoreConfig {
    /// Munshi state directory; supplies the configured endpoint, output directory, and harness
    /// homes when the machine is registered.
    pub state_directory: PathBuf,
    /// Restore from this archive server instead of the configured archive-upload endpoint.
    pub endpoint_override: Option<String>,
    /// Write the restored record here instead of the registered archive output directory.
    pub output_directory_override: Option<PathBuf>,
    /// Restore only the snapshots of this *Patwari* session id (the same identity
    /// `verify-archive-parse --session` takes), rather than the whole archive.
    pub session_filter: Option<String>,
    /// Replace local files whose content differs from the archived original.
    pub force: bool,
    /// Report what would be written without transferring or writing anything.
    pub dry_run: bool,
    /// Leave `outputs/<sha256>` extracted outputs in the archive. They are re-derivable from the
    /// restored transcript, so skipping them costs no recoverable content.
    pub skip_outputs: bool,
    /// Import the restored Markdown into operational state when the run finishes.
    pub rebuild_state: bool,
    /// Raise the per-artifact stored/original download cap.
    pub max_download_bytes: Option<usize>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A restore-aborting failure: nothing was written (or the walk could not start). Per-snapshot and
/// per-artifact problems never surface here — they become report entries instead.
#[derive(Debug, Error)]
pub enum RestoreError {
    /// The server rejected the request parameters (for example a malformed `--session` id).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// No archive-upload endpoint is configured, so there is no archive to restore from.
    #[error(
        "no archive server is configured; pass --endpoint, or set an archive-upload endpoint first"
    )]
    NotConfigured,
    /// No archive output directory is known: this machine has no usable registration and no
    /// `--output-dir` was given, so restore has nowhere to write the record.
    #[error(
        "no archive output directory is known ({0} holds no usable registration); pass --output-dir and --endpoint, or register this machine first"
    )]
    NotRegistered(PathBuf),
    /// `--session` named a session the archive does not hold. Reported rather than treated as a
    /// satisfied-by-zero success: an explicitly named target that restores nothing is a failure
    /// (issue #54's lesson).
    #[error("the archive holds no snapshots for session {0}")]
    SessionNotFound(String),
    /// The archive server could not be reached (connection refused, DNS, timeout).
    #[error("archive server is unreachable: {0}")]
    Unreachable(String),
    /// The server spoke unexpectedly (malformed body, non-terminating pagination).
    #[error("archive server protocol error: {0}")]
    Protocol(String),
    /// The server returned a non-success status for the snapshot listing.
    #[error("archive server returned status {status}: {code}")]
    Server { status: u16, code: String },
    /// Reading the configured endpoint failed.
    #[error(transparent)]
    Config(PatwariError),
}

impl RestoreError {
    /// The distinct process exit code for this failure class (see the module docs table).
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidInput(_) => 2,
            Self::NotConfigured | Self::NotRegistered(_) => 3,
            Self::SessionNotFound(_) => 4,
            Self::Unreachable(_) | Self::Protocol(_) | Self::Server { .. } => 5,
            Self::Config(_) => 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// What became of one artifact's local copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactResult {
    /// Downloaded, verified, and written; no local copy existed.
    Written,
    /// A local copy already hashes to the archived original, so nothing was transferred.
    Present,
    /// A differing local copy was replaced, because `--force` was given.
    Replaced,
    /// A differing local copy was left alone. Rerun with `--force` to replace it.
    RefusedDiffers,
    /// `--dry-run`: absent locally, so a real run would download and write it.
    WouldWrite,
    /// `--dry-run` with `--force`: a differing local copy a real run would replace.
    WouldReplace,
    /// Set aside without transfer: past the download cap, an extracted output under
    /// `--skip-outputs`, or a logical path this build does not know how to place.
    Skipped,
    /// The download, its verification, or the local write failed.
    Failed,
}

/// One artifact of one restored snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactReport {
    /// The server-side artifact identity, so a restored file can be traced back to the archive it
    /// came from without re-running the listing.
    pub artifact_id: String,
    /// The snapshot's reserved logical path (`summary.md`, `sidecar/plan.md`, …).
    pub logical_path: String,
    /// Where it belongs under the archive output directory; absent for an artifact that was never
    /// placed (an unknown logical path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
    pub result: ArtifactResult,
    /// The archived original's size, so a `--dry-run` reports the transfer it would make.
    pub original_size_bytes: u64,
    /// How a [`ArtifactResult::Failed`] artifact failed, which is what grades its whole snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    /// Why, for every outcome that is not a plain success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Why a snapshot was set aside without restoring anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// The capture's `artifact_set_version` is not one this build interprets, so its reserved
    /// logical paths might not mean what this build thinks they mean.
    UnsupportedArtifactSetVersion,
    /// The snapshot has no `summary.md`, which is the artifact that says where the record belongs.
    NoSummaryArtifact,
    /// `summary.md` is not Munshi-owned archive Markdown, or names a session identity that could
    /// not be turned into a safe local path.
    UnusableSummary,
    /// `summary.md` exceeds the download cap; rerun with `--max-download-bytes`.
    SummaryTooLarge,
}

/// The class of a non-fatal per-snapshot failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClass {
    /// Downloaded bytes failed size/hash verification or could not be decompressed.
    Verification,
    /// A request failed in transit or the server answered unexpectedly.
    Transport,
    /// The restored bytes could not be written locally.
    Local,
}

/// The outcome for one snapshot.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum SnapshotStatus {
    /// Every artifact is now present locally (written, or already matching).
    Restored,
    /// At least one artifact was left alone because the local copy differs.
    Refused,
    /// At least one artifact could not be restored.
    Failed {
        class: FailureClass,
        message: String,
    },
    /// Set aside before anything was placed.
    Skipped { reason: SkipReason, message: String },
}

/// One snapshot's restore, keyed by the local record it reproduces.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotReport {
    pub snapshot_id: String,
    /// Patwari's session identity — the one `--session` filters on.
    pub patwari_session_id: String,
    /// When the archive completed this snapshot. Reported because it is how an operator tells which
    /// revision of a session the restored record is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// The harness session ID read from the restored summary's frontmatter; `None` when the summary
    /// never became readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Manifest `session.source_agent`; `None` when the manifest could not be fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<String>,
    /// Manifest `capture.artifact_set_version`; `None` when the manifest could not be fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_set_version: Option<u64>,
    /// The archive Markdown path this snapshot restores, relative to the output directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
    /// How many older snapshots of this session the newest-wins rule set aside.
    pub superseded_snapshots: u64,
    pub status: SnapshotStatus,
    pub artifacts: Vec<ArtifactReport>,
}

/// Whether operational state was rebuilt from the restored Markdown, and why not when it was not.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum StateOutcome {
    /// Rows imported through the same archive importer `rebuild-state` uses.
    Rebuilt {
        /// Sessions whose restored Markdown was found and imported. The importer never lowers a
        /// row's revision, so a session already at this revision counts here and changes nothing.
        sessions: usize,
        /// Sessions whose row was additionally pointed at its restored transcript, because it had
        /// no readable transcript of its own.
        transcripts_linked: usize,
    },
    /// Deliberately not attempted; `reason` is stable, `message` says what to do about it.
    Skipped { reason: StateSkip, message: String },
    /// Attempted and failed. The restored files are on disk regardless, so a later
    /// `munshi hook recover --rebuild-state` still picks them up.
    Failed { message: String },
}

/// Why state import did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StateSkip {
    /// `--no-rebuild-state`.
    Disabled,
    /// `--dry-run` writes nothing, so there is nothing to import.
    DryRun,
    /// No session was restored, so no row could change.
    NothingRestored,
    /// The machine has no usable registration, so the harness homes the importer needs are unknown.
    Unregistered,
    /// `--output-dir` names a directory the registration does not archive to, so importing it would
    /// teach operational state about records the rest of Munshi will never look at.
    OutputDirectoryMismatch,
}

/// Whole-run totals.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    /// Snapshots the newest-per-session rule selected.
    pub snapshots: u64,
    pub restored: u64,
    pub refused: u64,
    pub skipped: u64,
    pub failed: u64,
    /// Older snapshots of a selected session, which have no local home.
    pub superseded: u64,
    pub artifacts_written: u64,
    /// Artifacts whose local copy already hashed to the archived original.
    pub artifacts_present: u64,
    pub artifacts_refused: u64,
    pub artifacts_skipped: u64,
    pub artifacts_failed: u64,
    /// Original bytes written locally this run (or, under `--dry-run`, that would be).
    pub bytes_written: u64,
}

/// The completed restore.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    pub schema_version: u32,
    pub command: &'static str,
    /// The `--session` filter, when one was given.
    pub session_filter: Option<String>,
    /// The archive output directory the record was written into.
    pub output_directory: String,
    pub dry_run: bool,
    pub force: bool,
    pub snapshots: Vec<SnapshotReport>,
    pub state: StateOutcome,
    pub totals: Totals,
}

impl RestoreReport {
    /// The process exit code the completed run deserves (see the module docs table): verification
    /// failure (6) over transport failure (5) over a local write failure (1) over findings (4).
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        let mut worst = 0u8;
        for snapshot in &self.snapshots {
            let code = match &snapshot.status {
                SnapshotStatus::Failed {
                    class: FailureClass::Verification,
                    ..
                } => 6,
                SnapshotStatus::Failed {
                    class: FailureClass::Transport,
                    ..
                } => 5,
                SnapshotStatus::Failed {
                    class: FailureClass::Local,
                    ..
                } => 1,
                SnapshotStatus::Refused | SnapshotStatus::Skipped { .. } => 4,
                SnapshotStatus::Restored => 0,
            };
            worst = rank_max(worst, code);
        }
        if self.totals.artifacts_skipped > 0 {
            worst = rank_max(worst, 4);
        }
        if matches!(self.state, StateOutcome::Failed { .. }) {
            worst = rank_max(worst, 1);
        }
        worst
    }

    /// Prints the human rendering to stdout: one line per snapshot that needs saying, then totals.
    /// A clean restore stays terse — the per-artifact detail is what `--json` is for.
    pub fn print_human(&self) {
        let totals = &self.totals;
        println!(
            "restore: {} snapshot(s) — {} restored, {} refused, {} skipped, {} failed{}",
            totals.snapshots,
            totals.restored,
            totals.refused,
            totals.skipped,
            totals.failed,
            if self.dry_run { " (dry run)" } else { "" },
        );
        for snapshot in &self.snapshots {
            print_snapshot_human(snapshot);
        }
        println!(
            "artifacts: {} written, {} already present, {} refused, {} skipped, {} failed ({} byte(s))",
            totals.artifacts_written,
            totals.artifacts_present,
            totals.artifacts_refused,
            totals.artifacts_skipped,
            totals.artifacts_failed,
            totals.bytes_written,
        );
        match &self.state {
            StateOutcome::Rebuilt {
                sessions,
                transcripts_linked,
            } => println!(
                "state: {sessions} session(s) imported from restored Markdown, {transcripts_linked} linked to a restored transcript"
            ),
            StateOutcome::Skipped { message, .. } => println!("state: not rebuilt — {message}"),
            StateOutcome::Failed { message } => println!("state: rebuild failed — {message}"),
        }
    }
}

/// Ranks two exit codes by severity, not by value: verification (6) beats transport (5) beats a
/// local failure (1) beats findings (4) beats clean (0).
fn rank_max(left: u8, right: u8) -> u8 {
    let rank = |code: u8| match code {
        6 => 4,
        5 => 3,
        1 => 2,
        4 => 1,
        _ => 0,
    };
    if rank(right) > rank(left) {
        right
    } else {
        left
    }
}

fn print_snapshot_human(snapshot: &SnapshotReport) {
    let label = snapshot
        .relative_path
        .as_deref()
        .unwrap_or(&snapshot.patwari_session_id);
    match &snapshot.status {
        // A clean restore that transferred nothing is the steady state; say nothing about it.
        SnapshotStatus::Restored => {
            let changed: Vec<&ArtifactReport> = snapshot
                .artifacts
                .iter()
                .filter(|artifact| {
                    matches!(
                        artifact.result,
                        ArtifactResult::Written
                            | ArtifactResult::Replaced
                            | ArtifactResult::WouldWrite
                            | ArtifactResult::WouldReplace
                    )
                })
                .collect();
            if !changed.is_empty() {
                println!("  restored {label} ({} artifact(s))", changed.len());
            }
        }
        SnapshotStatus::Refused => {
            println!("  refused {label}:");
            for artifact in &snapshot.artifacts {
                if artifact.result == ArtifactResult::RefusedDiffers {
                    println!(
                        "    {} differs from the archived original; pass --force to replace it",
                        artifact
                            .relative_path
                            .as_deref()
                            .unwrap_or(&artifact.logical_path)
                    );
                }
            }
        }
        SnapshotStatus::Failed { class, message } => {
            let class = match class {
                FailureClass::Verification => "verification",
                FailureClass::Transport => "transport",
                FailureClass::Local => "local",
            };
            println!("  failed {label} ({class}) — {message}");
        }
        SnapshotStatus::Skipped { message, .. } => {
            println!("  skipped snapshot {} — {message}", snapshot.snapshot_id);
        }
    }
}

// ---------------------------------------------------------------------------
// The restore
// ---------------------------------------------------------------------------

/// Restores the local record from the archive and returns the completed report.
///
/// Only startup problems return `Err`; every per-snapshot and per-artifact problem is folded into
/// the report, so one unreadable snapshot never costs the operator the rest of their archive.
pub fn restore(config: &RestoreConfig) -> Result<RestoreReport, RestoreError> {
    let stored = load_stored_config(&config.state_directory).ok();
    let output_directory = match &config.output_directory_override {
        Some(directory) => directory.clone(),
        None => stored
            .as_ref()
            .map(|stored| PathBuf::from(&stored.output_directory))
            .ok_or_else(|| RestoreError::NotRegistered(config.state_directory.clone()))?,
    };
    let endpoint = match &config.endpoint_override {
        Some(endpoint) => endpoint.clone(),
        None => configured_endpoint(&config.state_directory)?,
    };
    let cap = config
        .max_download_bytes
        .unwrap_or(MAX_ARTIFACT_DOWNLOAD_BYTES);
    let client = RestoreClient::connect(&endpoint)?;

    let listed = client.list_snapshots(config.session_filter.as_deref())?;
    let (selected, superseded) = select_newest_per_session(listed);
    if selected.is_empty()
        && let Some(session) = &config.session_filter
    {
        return Err(RestoreError::SessionNotFound(session.clone()));
    }

    // Strictly sequential — one manifest fetch, one artifact listing, one download at a time —
    // which keeps a whole-archive restore well under Patwari's download-concurrency permit count.
    let local = LocalRecordIndex::build(&output_directory);
    let outcomes: Vec<SnapshotOutcome> = selected
        .iter()
        .map(|snapshot| {
            restore_snapshot(
                &client,
                snapshot,
                &output_directory,
                &local,
                config,
                cap,
                superseded.get(&snapshot.session_id).copied().unwrap_or(0),
            )
        })
        .collect();

    let restored: Vec<&RestoredSession> = outcomes
        .iter()
        .filter_map(|outcome| outcome.restored.as_ref())
        .collect();
    let state = rebuild_state(config, &output_directory, stored.as_ref(), &restored);
    let snapshots = outcomes.into_iter().map(|outcome| outcome.report).collect();
    Ok(build_report(config, &output_directory, snapshots, state))
}

/// Reads the archive-upload endpoint recorded in configuration. Restore reuses upload's endpoint and
/// does not require upload to be enabled — only that a server address is configured — exactly as
/// claim-ticket retrieval and the archive walk do.
fn configured_endpoint(state_directory: &Path) -> Result<String, RestoreError> {
    let report = patwari::status(state_directory).map_err(RestoreError::Config)?;
    report
        .settings
        .endpoint
        .filter(|endpoint| !endpoint.is_empty())
        .ok_or(RestoreError::NotConfigured)
}

/// Keeps the newest snapshot per session and counts the rest.
///
/// Relies on Patwari's traversal order (`completed_at` descending, snapshot id descending) rather
/// than on parsing timestamps: the server states the order, and re-deriving it client-side from a
/// field the server also uses for cursor arithmetic would be a second, divergent opinion about
/// which revision is current.
fn select_newest_per_session(
    listed: Vec<ListedSnapshot>,
) -> (Vec<ListedSnapshot>, BTreeMap<String, u64>) {
    let mut selected: Vec<ListedSnapshot> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut superseded: BTreeMap<String, u64> = BTreeMap::new();
    for snapshot in listed {
        if seen.insert(snapshot.session_id.clone()) {
            selected.push(snapshot);
        } else {
            *superseded.entry(snapshot.session_id).or_insert(0) += 1;
        }
    }
    (selected, superseded)
}

fn build_report(
    config: &RestoreConfig,
    output_directory: &Path,
    snapshots: Vec<SnapshotReport>,
    state: StateOutcome,
) -> RestoreReport {
    let mut totals = Totals::default();
    for snapshot in &snapshots {
        totals.snapshots += 1;
        totals.superseded += snapshot.superseded_snapshots;
        match &snapshot.status {
            SnapshotStatus::Restored => totals.restored += 1,
            SnapshotStatus::Refused => totals.refused += 1,
            SnapshotStatus::Skipped { .. } => totals.skipped += 1,
            SnapshotStatus::Failed { .. } => totals.failed += 1,
        }
        for artifact in &snapshot.artifacts {
            match artifact.result {
                ArtifactResult::Written
                | ArtifactResult::Replaced
                | ArtifactResult::WouldWrite
                | ArtifactResult::WouldReplace => {
                    totals.artifacts_written += 1;
                    totals.bytes_written += artifact.original_size_bytes;
                }
                ArtifactResult::Present => totals.artifacts_present += 1,
                ArtifactResult::RefusedDiffers => totals.artifacts_refused += 1,
                ArtifactResult::Skipped => totals.artifacts_skipped += 1,
                ArtifactResult::Failed => totals.artifacts_failed += 1,
            }
        }
    }
    RestoreReport {
        schema_version: 1,
        command: "restore",
        session_filter: config.session_filter.clone(),
        output_directory: output_directory.display().to_string(),
        dry_run: config.dry_run,
        force: config.force,
        snapshots,
        state,
        totals,
    }
}

/// One archived session whose Markdown is now on disk, carried out of the restore so state import
/// works from what was actually written rather than re-deriving it from report strings.
struct RestoredSession {
    source: SourceKind,
    /// The harness session ID, as the restored frontmatter states it.
    session_id: String,
    /// The archive Markdown path, relative to the output directory.
    markdown_relative: PathBuf,
}

/// One snapshot's restore: what to report, and — when a record reached disk — what to import.
struct SnapshotOutcome {
    report: SnapshotReport,
    restored: Option<RestoredSession>,
}

impl SnapshotOutcome {
    fn report_only(report: SnapshotReport) -> Self {
        Self {
            report,
            restored: None,
        }
    }
}

/// Restores one snapshot: provenance, artifact listing, the summary that says where the record
/// belongs, then every remaining artifact.
fn restore_snapshot(
    client: &RestoreClient,
    snapshot: &ListedSnapshot,
    output_directory: &Path,
    local: &LocalRecordIndex,
    config: &RestoreConfig,
    cap: usize,
    superseded_snapshots: u64,
) -> SnapshotOutcome {
    let mut report = SnapshotReport {
        snapshot_id: snapshot.snapshot_id.clone(),
        patwari_session_id: snapshot.session_id.clone(),
        completed_at: snapshot.completed_at.clone(),
        session_id: None,
        source_agent: None,
        artifact_set_version: None,
        relative_path: None,
        superseded_snapshots,
        status: SnapshotStatus::Restored,
        artifacts: Vec::new(),
    };
    let provenance = match client.snapshot_provenance(&snapshot.snapshot_id) {
        Ok(provenance) => provenance,
        Err(status) => {
            report.status = status;
            return SnapshotOutcome::report_only(report);
        }
    };
    report.source_agent = Some(provenance.source_agent);
    report.artifact_set_version = Some(provenance.artifact_set_version);
    // The artifact-set version is what gives reserved logical paths their meaning (Patwari's ADR
    // 0005), so a version this build does not know is set aside rather than placed by guesswork.
    if !(u64::from(MIN_SUPPORTED_ARTIFACT_SET_VERSION)..=u64::from(SUPPORTED_ARTIFACT_SET_VERSION))
        .contains(&provenance.artifact_set_version)
    {
        report.status = SnapshotStatus::Skipped {
            reason: SkipReason::UnsupportedArtifactSetVersion,
            message: format!(
                "artifact set version {} is not supported by this build (supported: {MIN_SUPPORTED_ARTIFACT_SET_VERSION}..={SUPPORTED_ARTIFACT_SET_VERSION})",
                provenance.artifact_set_version
            ),
        };
        return SnapshotOutcome::report_only(report);
    }

    let artifacts = match client.list_snapshot_artifacts(&snapshot.snapshot_id) {
        Ok(artifacts) => artifacts,
        Err(status) => {
            report.status = status;
            return SnapshotOutcome::report_only(report);
        }
    };
    let Some(summary_artifact) = artifacts
        .iter()
        .find(|artifact| artifact.logical_path == SUMMARY_LOGICAL_PATH)
    else {
        report.status = SnapshotStatus::Skipped {
            reason: SkipReason::NoSummaryArtifact,
            message: format!("snapshot has no {SUMMARY_LOGICAL_PATH} artifact"),
        };
        return SnapshotOutcome::report_only(report);
    };

    // Where the record belongs is stated by the summary's own frontmatter — the source, the project
    // component and the harness session ID — and by nothing the server holds. It is therefore
    // resolved before anything else: from the local record that already carries this exact archived
    // digest when there is one, so a rerun transfers nothing at all, and by fetching it otherwise.
    let (identity, summary_bytes) = match local.get(&summary_artifact.original_sha256) {
        Some(identity) => (identity.clone(), None),
        None => {
            let bytes = match client.download_verified(summary_artifact, cap) {
                Ok(bytes) => bytes,
                Err(DownloadFailure::TooLarge(message)) => {
                    report.status = SnapshotStatus::Skipped {
                        reason: SkipReason::SummaryTooLarge,
                        message,
                    };
                    return SnapshotOutcome::report_only(report);
                }
                Err(DownloadFailure::Failed(status)) => {
                    report.status = status;
                    return SnapshotOutcome::report_only(report);
                }
            };
            match parse_restored_summary(&bytes) {
                Ok(identity) => (identity, Some(bytes)),
                Err(message) => {
                    report.status = SnapshotStatus::Skipped {
                        reason: SkipReason::UnusableSummary,
                        message,
                    };
                    return SnapshotOutcome::report_only(report);
                }
            }
        }
    };
    let markdown_relative = identity.markdown_relative.clone();
    report.session_id = Some(identity.session_id.clone());
    report.relative_path = Some(markdown_relative.display().to_string());

    // A freshly downloaded summary is placed from the bytes already in hand; everything else is
    // resolved to a local path first and only transferred if the local copy is absent or differs.
    let placer = Placer {
        output_directory,
        markdown_relative: &markdown_relative,
        config,
        client,
        cap,
    };
    report
        .artifacts
        .push(placer.place_known(summary_artifact, &markdown_relative, summary_bytes));
    for artifact in &artifacts {
        if artifact.logical_path == SUMMARY_LOGICAL_PATH {
            continue;
        }
        report.artifacts.push(placer.place(artifact));
    }
    report.status = snapshot_status(&report.artifacts);

    // Only a snapshot whose Markdown actually reached disk is offered to state import: a refused or
    // failed summary means the local record is not this revision, and importing it would teach the
    // database a revision the archive file does not hold.
    let summary_placed = report.artifacts.iter().any(|artifact| {
        artifact.logical_path == SUMMARY_LOGICAL_PATH
            && matches!(
                artifact.result,
                ArtifactResult::Written | ArtifactResult::Replaced | ArtifactResult::Present
            )
    });
    let restored = summary_placed.then_some(RestoredSession {
        source: identity.source,
        session_id: identity.session_id,
        markdown_relative,
    });
    SnapshotOutcome { report, restored }
}

/// Grades a snapshot from its artifact outcomes: any failure wins over any refusal, and a snapshot
/// with neither is restored even if some artifacts were deliberately set aside.
fn snapshot_status(artifacts: &[ArtifactReport]) -> SnapshotStatus {
    if let Some(failed) = artifacts
        .iter()
        .find(|artifact| artifact.result == ArtifactResult::Failed)
    {
        return SnapshotStatus::Failed {
            class: failed.failure_class.unwrap_or(FailureClass::Local),
            message: format!(
                "{}: {}",
                failed.logical_path,
                failed.reason.as_deref().unwrap_or("restore failed")
            ),
        };
    }
    if artifacts
        .iter()
        .any(|artifact| artifact.result == ArtifactResult::RefusedDiffers)
    {
        return SnapshotStatus::Refused;
    }
    SnapshotStatus::Restored
}

/// The identity a `summary.md` states about itself: which harness session it records, and therefore
/// where in the local archive layout it — and the rest of its snapshot — belongs.
#[derive(Debug, Clone)]
struct RecordIdentity {
    source: SourceKind,
    session_id: String,
    /// The archive Markdown path, relative to the archive output directory.
    markdown_relative: PathBuf,
}

/// Reads a `summary.md` as the archive record it claims to be, refusing anything whose identity
/// could not be turned into a safe local path.
///
/// Downloaded bytes are hash-verified against the archive by the time they arrive here, which proves
/// they are the archived original — not that the archived original is safe to write *by*. A session
/// ID or project component carrying path syntax would escape the output directory, so both are
/// validated against the same rules the capture path applies before either reaches a `join`.
fn parse_restored_summary(bytes: &[u8]) -> Result<RecordIdentity, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "summary.md is not valid UTF-8 Munshi archive Markdown".to_owned())?;
    let markdown: ArchivedMarkdown = parse_archive_markdown(text)
        .map_err(|_| "summary.md is not a Munshi-owned archive record".to_owned())?;
    validate_session_id(&markdown.session_id).map_err(|_| {
        format!(
            "summary.md names an unusable session id `{}`",
            markdown.session_id
        )
    })?;
    // `parse_archive_markdown` already rejects `.`, `..` and separators in the component; the check
    // is repeated here because this is the one caller that turns the value into a filesystem path
    // from bytes an archive server supplied.
    let component = &markdown.project.component;
    if component.is_empty()
        || Path::new(component)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!(
            "summary.md names an unusable project component `{component}`"
        ));
    }
    Ok(RecordIdentity {
        source: markdown.source,
        markdown_relative: archive_relative_path(markdown.source, component, &markdown.session_id),
        session_id: markdown.session_id,
    })
}

/// The archive Markdown already on disk, indexed by its content digest.
///
/// It exists to make a rerun genuinely free. Every other artifact's local copy can be proved
/// identical to the archived original from the listing alone, but the summary cannot: it is the
/// artifact that *says* where the record belongs, so resolving a snapshot's location would otherwise
/// mean downloading it every time — turning "safe to rerun" into "cheap only for the large
/// artifacts". Indexing the local records by digest first closes that gap: a snapshot whose summary
/// digest is already on disk resolves its location locally and transfers nothing.
///
/// Only Markdown that sits at the path its own frontmatter implies is indexed, which is the same
/// rule the state rebuild's archive scan applies. A file moved or planted elsewhere therefore never
/// teaches restore a location, and the worst a stale index can do is cost one download: a digest
/// that no longer matches the file falls through to [`Placer::place_known`]'s own hash check.
struct LocalRecordIndex {
    by_digest: BTreeMap<String, RecordIdentity>,
}

impl LocalRecordIndex {
    fn build(output_directory: &Path) -> Self {
        let mut by_digest = BTreeMap::new();
        index_archives(output_directory, output_directory, 0, &mut by_digest);
        Self { by_digest }
    }

    fn get(&self, original_sha256: &str) -> Option<&RecordIdentity> {
        self.by_digest.get(original_sha256)
    }
}

/// Depth-bounded, symlink-refusing walk of the archive output directory, matching the state
/// rebuild's own scan: archive Markdown lives at `<component>/[<source-prefix>/]<session-id>.md`, so
/// three levels reach every record and nothing follows a link out of the tree. Every failure is
/// silent — an unreadable or unparseable file simply is not a known local record, and the restore it
/// informs still works, just with one more download.
fn index_archives(
    output_directory: &Path,
    directory: &Path,
    depth: usize,
    by_digest: &mut BTreeMap<String, RecordIdentity>,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            index_archives(output_directory, &path, depth + 1, by_digest);
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(identity) = parse_restored_summary(&bytes) else {
            continue;
        };
        if path.strip_prefix(output_directory) != Ok(identity.markdown_relative.as_path()) {
            continue;
        }
        by_digest.insert(sha256_hex(&bytes), identity);
    }
}

// ---------------------------------------------------------------------------
// Placing artifacts
// ---------------------------------------------------------------------------

/// Resolves each artifact of one snapshot to its local path and puts its bytes there.
struct Placer<'a> {
    output_directory: &'a Path,
    /// The archive Markdown path this snapshot restores, relative to the output directory. Every
    /// other artifact's location is derived from it, so the whole set moves together.
    markdown_relative: &'a Path,
    config: &'a RestoreConfig,
    client: &'a RestoreClient,
    cap: usize,
}

impl Placer<'_> {
    /// Places one artifact, resolving its logical path first.
    fn place(&self, artifact: &SnapshotArtifact) -> ArtifactReport {
        match self.local_relative_path(&artifact.logical_path) {
            Ok(relative) => self.place_known(artifact, &relative, None),
            Err(reason) => ArtifactReport {
                artifact_id: artifact.artifact_id.clone(),
                logical_path: artifact.logical_path.clone(),
                relative_path: None,
                result: ArtifactResult::Skipped,
                original_size_bytes: artifact.original_size_bytes,
                reason: Some(reason),
                failure_class: None,
            },
        }
    }

    /// Places one artifact at an already-resolved path, using `bytes` when the caller already
    /// downloaded them (the summary) and fetching otherwise.
    fn place_known(
        &self,
        artifact: &SnapshotArtifact,
        relative: &Path,
        bytes: Option<Vec<u8>>,
    ) -> ArtifactReport {
        let target = self.output_directory.join(relative);
        let mut report = ArtifactReport {
            artifact_id: artifact.artifact_id.clone(),
            logical_path: artifact.logical_path.clone(),
            relative_path: Some(relative.display().to_string()),
            result: ArtifactResult::Written,
            original_size_bytes: artifact.original_size_bytes,
            reason: None,
            failure_class: None,
        };

        // Verify-and-skip, not exists-and-skip: the listing declares the archived original's digest,
        // so a local copy can be proved identical without transferring anything, and a local copy
        // that is *not* identical is never silently accepted as restored.
        match local_state(&target, &artifact.original_sha256) {
            Ok(LocalState::Absent) => {
                if self.config.dry_run {
                    report.result = ArtifactResult::WouldWrite;
                    return report;
                }
            }
            Ok(LocalState::Matches) => {
                report.result = ArtifactResult::Present;
                return report;
            }
            Ok(LocalState::Differs) => {
                if !self.config.force {
                    report.result = ArtifactResult::RefusedDiffers;
                    report.reason = Some(
                        "local file differs from the archived original; pass --force to replace it"
                            .to_owned(),
                    );
                    return report;
                }
                if self.config.dry_run {
                    report.result = ArtifactResult::WouldReplace;
                    return report;
                }
                report.result = ArtifactResult::Replaced;
            }
            Err(error) => {
                report.result = ArtifactResult::Failed;
                report.failure_class = Some(FailureClass::Local);
                report.reason = Some(format!("could not read the local copy: {error}"));
                return report;
            }
        }

        let bytes = match bytes {
            Some(bytes) => bytes,
            None => match self.client.download_verified(artifact, self.cap) {
                Ok(bytes) => bytes,
                Err(DownloadFailure::TooLarge(message)) => {
                    report.result = ArtifactResult::Skipped;
                    report.reason = Some(message);
                    return report;
                }
                Err(DownloadFailure::Failed(SnapshotStatus::Failed { class, message })) => {
                    report.result = ArtifactResult::Failed;
                    report.failure_class = Some(class);
                    report.reason = Some(message);
                    return report;
                }
                Err(DownloadFailure::Failed(_)) => {
                    // `download_verified` only ever produces the failure shape above.
                    report.result = ArtifactResult::Failed;
                    report.failure_class = Some(FailureClass::Transport);
                    report.reason = Some("download failed".to_owned());
                    return report;
                }
            },
        };
        if let Err(error) = atomic_replace(&target, &bytes) {
            report.result = ArtifactResult::Failed;
            report.failure_class = Some(FailureClass::Local);
            report.reason = Some(format!("could not write {}: {error}", target.display()));
        }
        report
    }

    /// Where an artifact's logical path belongs under the archive output directory.
    ///
    /// The two roles local archival already writes keep their archival homes, so the rebuild path
    /// and a later re-upload both find them unchanged. The two it does not — the transcript and the
    /// extracted outputs — go into the sibling restored-artifact directory, which nothing else
    /// writes to. Every path component comes from an archive server, so each is validated before it
    /// reaches a `join`: a `sidecar/../../…` logical path resolves to a refusal, not to a write
    /// outside the output directory.
    fn local_relative_path(&self, logical_path: &str) -> Result<PathBuf, String> {
        if logical_path == SUMMARY_LOGICAL_PATH {
            return Ok(self.markdown_relative.to_path_buf());
        }
        if logical_path == TRANSCRIPT_LOGICAL_PATH {
            return Ok(
                restored_relative_directory(self.markdown_relative).join(RESTORED_TRANSCRIPT_FILE)
            );
        }
        if let Some(relative) = logical_path.strip_prefix(SIDECAR_LOGICAL_PREFIX) {
            let relative = safe_relative_path(relative)
                .ok_or_else(|| format!("unsafe sidecar path `{logical_path}`"))?;
            return Ok(sidecar_relative_directory(self.markdown_relative).join(relative));
        }
        if let Some(digest) = logical_path.strip_prefix(OUTPUTS_LOGICAL_PREFIX) {
            if self.config.skip_outputs {
                return Err(
                    "extracted outputs skipped by --skip-outputs (re-derivable from the transcript)"
                        .to_owned(),
                );
            }
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(format!("unsafe extracted-output path `{logical_path}`"));
            }
            return Ok(restored_relative_directory(self.markdown_relative)
                .join(OUTPUTS_LOGICAL_PREFIX.trim_end_matches('/'))
                .join(digest));
        }
        Err(format!(
            "logical path `{logical_path}` has no place in the local archive layout"
        ))
    }
}

/// A relative path built from untrusted components, or `None` when any component is anything but a
/// plain name — the single check that keeps an archive server from writing outside the archive.
fn safe_relative_path(value: &str) -> Option<PathBuf> {
    if value.is_empty() || value.starts_with('/') || value.contains('\\') {
        return None;
    }
    let path = Path::new(value);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_path_buf())
}

/// How a local file compares to the archived original.
enum LocalState {
    Absent,
    /// Byte-identical to the archived original, proved by hash.
    Matches,
    Differs,
}

fn local_state(path: &Path, expected_sha256: &str) -> std::io::Result<LocalState> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(if sha256_hex(&bytes) == expected_sha256 {
            LocalState::Matches
        } else {
            LocalState::Differs
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LocalState::Absent),
        Err(error) => Err(error),
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Imports the restored Markdown into operational state, one session at a time.
///
/// This is deliberately the per-session importer rather than [`crate::state::rebuild_database`]:
/// the whole-database rebuild renames the existing database aside, which discards archive-upload
/// and delivery history a restore has no reason to touch. Both paths run the same
/// `import_archive_record`, so a fresh machine restoring its whole archive ends up with exactly the
/// rows a `--rebuild-state` would have produced.
fn rebuild_state(
    config: &RestoreConfig,
    output_directory: &Path,
    stored: Option<&crate::registration::StoredConfig>,
    restored: &[&RestoredSession],
) -> StateOutcome {
    if !config.rebuild_state {
        return skipped(
            StateSkip::Disabled,
            "--no-rebuild-state was given; run `munshi hook recover --rebuild-state` to import the restored Markdown",
        );
    }
    if config.dry_run {
        return skipped(StateSkip::DryRun, "--dry-run wrote nothing to import");
    }
    if restored.is_empty() {
        return skipped(StateSkip::NothingRestored, "no session was restored");
    }
    let Some(stored) = stored else {
        return skipped(
            StateSkip::Unregistered,
            "this machine has no usable registration; run `munshi register` and then `munshi hook recover --rebuild-state`",
        );
    };
    // Importing records the registration will never read back would be worse than not importing:
    // every other Munshi read resolves archives through the registered output directory. Compared
    // through the filesystem so a differently-spelled path for the same directory — a relative
    // `--output-dir`, a trailing slash, a symlinked home — is not mistaken for a different one.
    if !same_directory(Path::new(&stored.output_directory), output_directory) {
        return skipped(
            StateSkip::OutputDirectoryMismatch,
            &format!(
                "--output-dir {} is not the registered archive output directory {}",
                output_directory.display(),
                stored.output_directory
            ),
        );
    }

    let homes = stored.harnesses.source_homes();
    match import_restored_sessions(config, output_directory, &homes, restored) {
        Ok((sessions, transcripts_linked)) => StateOutcome::Rebuilt {
            sessions,
            transcripts_linked,
        },
        Err(message) => StateOutcome::Failed { message },
    }
}

/// Whether two paths name the same directory, resolved through the filesystem when both resolve
/// and compared literally when they do not (a directory that does not exist yet is still comparable
/// by name, and nothing here should fail merely because a path is not there).
fn same_directory(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn skipped(reason: StateSkip, message: &str) -> StateOutcome {
    StateOutcome::Skipped {
        reason,
        message: message.to_owned(),
    }
}

/// Imports every restored session, then points rows with no readable transcript at the restored
/// one.
///
/// The link is conditional on purpose. A rebuilt row re-derives its transcript from the harness
/// home (issue #53), which is exactly right on a machine whose harness still holds the session and
/// exactly useless on a wiped one. Overwriting a live path with an archived copy would quietly move
/// every later read — upload, local claim-ticket redemption, recovery — off the file the harness is
/// still appending to, so the restored path is recorded only when the row has nothing readable.
fn import_restored_sessions(
    config: &RestoreConfig,
    output_directory: &Path,
    homes: &SourceHomes,
    restored: &[&RestoredSession],
) -> Result<(usize, usize), String> {
    let mut stores: BTreeMap<SourceKind, StateStore> = BTreeMap::new();
    let mut imported = 0usize;
    let mut linked = 0usize;
    for session in restored {
        // Session rows are scoped by source, so each source gets its own store — reused across
        // sessions because opening one runs the schema migration.
        let store = match stores.entry(session.source) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => entry.insert(
                StateStore::open_for_source(&config.state_directory, session.source)
                    .map_err(|error| error.to_string())?,
            ),
        };
        if store
            .hydrate_session_from_archives(output_directory, &session.session_id, homes)
            .map_err(|error| error.to_string())?
        {
            imported += 1;
        }
        let transcript = output_directory
            .join(restored_relative_directory(&session.markdown_relative))
            .join(RESTORED_TRANSCRIPT_FILE);
        if !transcript.is_file() {
            continue;
        }
        let readable = store
            .get_session(&session.session_id)
            .map_err(|error| error.to_string())?
            .and_then(|record| record.transcript_path)
            .is_some_and(|path| path.is_file());
        if !readable
            && store
                .record_derived_transcript_path(&session.session_id, &transcript)
                .map_err(|error| error.to_string())?
        {
            linked += 1;
        }
    }
    Ok((imported, linked))
}

// ---------------------------------------------------------------------------
// Patwari read client
// ---------------------------------------------------------------------------

/// The two ways a download stops: a refusal by the size gate, which is an accounting line, and
/// everything else, which is a graded failure.
enum DownloadFailure {
    TooLarge(String),
    Failed(SnapshotStatus),
}

/// A synchronous restore client bound to one server: the shared Patwari read stack
/// ([`crate::patwari_read`]) plus restore's own error surface. Every wire rule — pagination, the
/// size gate, the three-stage verification — lives in the shared module; what stays here is the
/// mapping onto run-aborting [`RestoreError`]s and per-snapshot [`SnapshotStatus`] entries.
struct RestoreClient {
    client: ReadClient,
}

impl RestoreClient {
    fn connect(endpoint: &str) -> Result<Self, RestoreError> {
        Ok(Self {
            client: ReadClient::connect(endpoint).map_err(from_http)?,
        })
    }

    /// The archive's snapshots, newest first.
    ///
    /// A listing that hit the page bound aborts the run: restoring "the archive" from a silently
    /// truncated view of it would leave a machine believing its record is whole.
    fn list_snapshots(
        &self,
        session_id: Option<&str>,
    ) -> Result<Vec<ListedSnapshot>, RestoreError> {
        let listing = self
            .client
            .list_snapshots(session_id)
            .map_err(|error| match error {
                // 422 is the server rejecting restore's own parameters (a malformed --session).
                ReadError::Status {
                    status: 422, code, ..
                } => RestoreError::InvalidInput(format!(
                    "the archive server rejected the request: {}",
                    code.unwrap_or_else(|| "invalid_request".to_owned())
                )),
                other => from_read(other),
            })?;
        if !listing.terminated {
            return Err(RestoreError::Protocol(format!(
                "snapshot listing did not terminate within {MAX_ARCHIVE_LISTING_PAGES} pages"
            )));
        }
        Ok(listing.items)
    }

    fn snapshot_provenance(
        &self,
        snapshot_id: &str,
    ) -> Result<crate::patwari_read::SnapshotProvenance, SnapshotStatus> {
        self.client
            .snapshot_provenance(snapshot_id)
            .map_err(|error| snapshot_failure("manifest fetch", error))
    }

    /// One snapshot's artifacts. A listing that hit the page bound would silently drop artifacts
    /// from the restored record, so it fails this snapshot rather than restoring a partial set.
    fn list_snapshot_artifacts(
        &self,
        snapshot_id: &str,
    ) -> Result<Vec<SnapshotArtifact>, SnapshotStatus> {
        let listing = self
            .client
            .list_snapshot_artifacts(snapshot_id)
            .map_err(|error| snapshot_failure("artifact listing", error))?;
        if !listing.terminated {
            return Err(SnapshotStatus::Failed {
                class: FailureClass::Transport,
                message: format!(
                    "artifact listing did not terminate within {MAX_ARCHIVE_LISTING_PAGES} pages"
                ),
            });
        }
        Ok(listing.items)
    }

    /// Downloads through the shared three-stage verification, gated on both declared sizes before
    /// any transfer. No unverified byte is ever written to the archive output directory.
    fn download_verified(
        &self,
        artifact: &SnapshotArtifact,
        cap: usize,
    ) -> Result<Vec<u8>, DownloadFailure> {
        self.client
            .download_verified(&artifact.listed(), cap)
            .map_err(|error| match error {
                DownloadError::TooLarge(SizeRefusal {
                    dimension,
                    size_bytes,
                    cap,
                }) => {
                    let dimension = match dimension {
                        SizeDimension::Stored => "stored",
                        SizeDimension::Original => "original",
                    };
                    DownloadFailure::TooLarge(format!(
                        "{dimension} size {size_bytes} bytes exceeds the {cap}-byte download cap; pass --max-download-bytes to raise it"
                    ))
                }
                DownloadError::Http(error) => {
                    DownloadFailure::Failed(transport_failure(error.to_string()))
                }
                DownloadError::Status { status, code } => {
                    DownloadFailure::Failed(transport_failure(format!(
                        "content download returned status {status}: {}",
                        code.unwrap_or_else(|| "unknown".to_owned())
                    )))
                }
                DownloadError::Protocol(message) => {
                    DownloadFailure::Failed(transport_failure(message))
                }
                DownloadError::Verification(message) => {
                    DownloadFailure::Failed(SnapshotStatus::Failed {
                        class: FailureClass::Verification,
                        message,
                    })
                }
                DownloadError::Decompression(message) => {
                    DownloadFailure::Failed(SnapshotStatus::Failed {
                        class: FailureClass::Verification,
                        message: format!("could not decompress stored content: {message}"),
                    })
                }
            })
    }
}

fn transport_failure(message: String) -> SnapshotStatus {
    SnapshotStatus::Failed {
        class: FailureClass::Transport,
        message,
    }
}

/// Grades a shared-stack read failure as a per-snapshot transport failure, naming the call.
fn snapshot_failure(call: &str, error: ReadError) -> SnapshotStatus {
    match error {
        ReadError::Http(error) => transport_failure(error.to_string()),
        ReadError::Status { status, code } => transport_failure(format!(
            "{call} returned status {status}: {}",
            code.unwrap_or_else(|| "unknown".to_owned())
        )),
        ReadError::Protocol(message) => transport_failure(message),
    }
}

fn from_read(error: ReadError) -> RestoreError {
    match error {
        ReadError::Http(error) => from_http(error),
        ReadError::Status { status, code } => RestoreError::Server {
            status,
            code: code.unwrap_or_else(|| "unknown".to_owned()),
        },
        ReadError::Protocol(message) => RestoreError::Protocol(message),
    }
}

fn from_http(error: HttpError) -> RestoreError {
    match error {
        HttpError::UnsupportedEndpoint(endpoint) => {
            RestoreError::Unreachable(format!("{endpoint} is not a supported http(s) URL"))
        }
        HttpError::Transport(message) => RestoreError::Unreachable(message),
        HttpError::Protocol(message) => RestoreError::Protocol(message),
        HttpError::Tls(message) => {
            RestoreError::Unreachable(format!("tls setup failed: {message}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(snapshot_id: &str, session_id: &str) -> ListedSnapshot {
        ListedSnapshot {
            snapshot_id: snapshot_id.to_owned(),
            session_id: session_id.to_owned(),
            completed_at: None,
        }
    }

    /// The newest-wins rule is the whole reason restore reproduces a coherent local record: locally
    /// only one revision per session has a home, so every older snapshot must be counted and left.
    #[test]
    fn selection_keeps_the_first_snapshot_seen_per_session_and_counts_the_rest() {
        let (selected, superseded) = select_newest_per_session(vec![
            listed("snap-a2", "sess-a"),
            listed("snap-b1", "sess-b"),
            listed("snap-a1", "sess-a"),
            listed("snap-a0", "sess-a"),
        ]);
        let ids: Vec<&str> = selected
            .iter()
            .map(|snapshot| snapshot.snapshot_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["snap-a2", "snap-b1"],
            "the server lists newest first, so the first seen wins"
        );
        assert_eq!(superseded.get("sess-a"), Some(&2));
        assert_eq!(superseded.get("sess-b"), None);
    }

    #[test]
    fn selection_of_an_empty_archive_yields_nothing_rather_than_failing() {
        let (selected, superseded) = select_newest_per_session(Vec::new());
        assert!(selected.is_empty());
        assert!(superseded.is_empty());
    }

    /// Every logical path an artifact-set-v2 snapshot can carry maps onto the local layout: the two
    /// roles archival writes keep their archival homes, and the two it does not go beside them.
    #[test]
    fn logical_paths_map_onto_the_local_archive_layout() {
        let markdown_relative = PathBuf::from("munshi/claude-code/sess-1.md");
        let config = test_config(false);
        let plan = |logical_path: &str| plan_path(&config, &markdown_relative, logical_path);

        assert_eq!(
            plan("summary.md").unwrap(),
            PathBuf::from("munshi/claude-code/sess-1.md")
        );
        assert_eq!(
            plan("transcript.jsonl").unwrap(),
            PathBuf::from("munshi/claude-code/sess-1.restored/transcript.jsonl")
        );
        assert_eq!(
            plan("sidecar/checkpoints/one.md").unwrap(),
            PathBuf::from("munshi/claude-code/sess-1.sidecar/checkpoints/one.md")
        );
        let digest = "ab".repeat(32);
        assert_eq!(
            plan(&format!("outputs/{digest}")).unwrap(),
            PathBuf::from(format!(
                "munshi/claude-code/sess-1.restored/outputs/{digest}"
            ))
        );
    }

    /// Logical paths come from an archive server, so the placement rules are a security boundary,
    /// not a formatting nicety: anything that could resolve outside the output directory is refused
    /// rather than written.
    #[test]
    fn unsafe_and_unknown_logical_paths_are_refused() {
        let markdown_relative = PathBuf::from("munshi/sess-1.md");
        let config = test_config(false);
        let plan = |logical_path: &str| plan_path(&config, &markdown_relative, logical_path);

        assert!(plan("sidecar/../../escape.md").is_err());
        assert!(plan("sidecar/").is_err());
        assert!(plan("sidecar/nested/../../escape.md").is_err());
        // Backslashes are refused outright rather than reasoned about per platform.
        assert!(plan("sidecar/a\\..\\b.md").is_err());
        // An extracted output's stem must be the bare digest that addresses it.
        assert!(plan("outputs/../summary.md").is_err());
        assert!(plan("outputs/NOTAHASH").is_err());
        assert!(plan(&format!("outputs/{}", "AB".repeat(32))).is_err());
        // A role this build does not know is set aside, never guessed at.
        assert!(plan("attachments/whatever.bin").is_err());
    }

    #[test]
    fn skip_outputs_refuses_the_re_derivable_artifacts_by_name() {
        let markdown_relative = PathBuf::from("munshi/sess-1.md");
        let config = test_config(true);
        let digest = "ab".repeat(32);
        let refusal = plan_path(&config, &markdown_relative, &format!("outputs/{digest}"))
            .expect_err("--skip-outputs sets extracted outputs aside");
        assert!(refusal.contains("--skip-outputs"), "reason: {refusal}");
        // Everything else still places normally.
        assert!(plan_path(&config, &markdown_relative, "transcript.jsonl").is_ok());
    }

    #[test]
    fn safe_relative_paths_admit_plain_names_only() {
        assert_eq!(
            safe_relative_path("checkpoints/one.md"),
            Some(PathBuf::from("checkpoints/one.md"))
        );
        assert_eq!(
            safe_relative_path("plan.md"),
            Some(PathBuf::from("plan.md"))
        );
        assert_eq!(safe_relative_path(""), None);
        assert_eq!(safe_relative_path("/absolute"), None);
        assert_eq!(safe_relative_path("../up"), None);
        assert_eq!(safe_relative_path("./here"), None);
        assert_eq!(safe_relative_path("a\\b"), None);
    }

    /// A summary is only usable if its own frontmatter can name a safe local path; the digest check
    /// proves the bytes are the archive's, not that the archive's bytes are safe to write by.
    #[test]
    fn an_unusable_summary_is_reported_rather_than_placed() {
        assert!(parse_restored_summary(b"# not an archive").is_err());
        assert!(parse_restored_summary(&[0xff, 0xfe]).is_err());
    }

    /// Severity, not numeric order: a run that both refused an overwrite and failed a verification
    /// must exit on the verification.
    #[test]
    fn exit_codes_rank_verification_over_transport_over_local_over_findings() {
        assert_eq!(rank_max(0, 4), 4);
        assert_eq!(rank_max(4, 1), 1);
        assert_eq!(rank_max(1, 5), 5);
        assert_eq!(rank_max(5, 6), 6);
        assert_eq!(rank_max(6, 5), 6);
        assert_eq!(rank_max(6, 4), 6);
        assert_eq!(rank_max(1, 4), 1);
    }

    #[test]
    fn report_exit_code_reflects_the_worst_snapshot_outcome() {
        let report = |status: SnapshotStatus| RestoreReport {
            schema_version: 1,
            command: "restore",
            session_filter: None,
            output_directory: "/tmp/out".to_owned(),
            dry_run: false,
            force: false,
            snapshots: vec![SnapshotReport {
                snapshot_id: "snap".to_owned(),
                patwari_session_id: "sess".to_owned(),
                completed_at: None,
                session_id: None,
                source_agent: None,
                artifact_set_version: None,
                relative_path: None,
                superseded_snapshots: 0,
                status,
                artifacts: Vec::new(),
            }],
            state: StateOutcome::Skipped {
                reason: StateSkip::Disabled,
                message: String::new(),
            },
            totals: Totals::default(),
        };
        assert_eq!(report(SnapshotStatus::Restored).exit_code(), 0);
        assert_eq!(report(SnapshotStatus::Refused).exit_code(), 4);
        assert_eq!(
            report(SnapshotStatus::Skipped {
                reason: SkipReason::NoSummaryArtifact,
                message: String::new(),
            })
            .exit_code(),
            4
        );
        for (class, code) in [
            (FailureClass::Local, 1),
            (FailureClass::Transport, 5),
            (FailureClass::Verification, 6),
        ] {
            assert_eq!(
                report(SnapshotStatus::Failed {
                    class,
                    message: String::new(),
                })
                .exit_code(),
                code
            );
        }

        // A clean run whose state import failed still reports the local failure.
        let mut failed_state = report(SnapshotStatus::Restored);
        failed_state.state = StateOutcome::Failed {
            message: "database is locked".to_owned(),
        };
        assert_eq!(failed_state.exit_code(), 1);

        // An artifact set aside without transfer is a finding even when its snapshot is restored.
        let mut skipped_artifact = report(SnapshotStatus::Restored);
        skipped_artifact.totals.artifacts_skipped = 1;
        assert_eq!(skipped_artifact.exit_code(), 4);
    }

    #[test]
    fn error_exit_codes_are_distinct_per_failure_class() {
        assert_eq!(RestoreError::InvalidInput(String::new()).exit_code(), 2);
        assert_eq!(RestoreError::NotConfigured.exit_code(), 3);
        assert_eq!(
            RestoreError::NotRegistered(PathBuf::from("/tmp")).exit_code(),
            3
        );
        assert_eq!(RestoreError::SessionNotFound(String::new()).exit_code(), 4);
        assert_eq!(RestoreError::Unreachable(String::new()).exit_code(), 5);
        assert_eq!(RestoreError::Protocol(String::new()).exit_code(), 5);
        assert_eq!(
            RestoreError::Server {
                status: 500,
                code: String::new()
            }
            .exit_code(),
            5
        );
    }

    /// Builds a config whose only meaningful field for path planning is `skip_outputs`.
    fn test_config(skip_outputs: bool) -> RestoreConfig {
        RestoreConfig {
            state_directory: PathBuf::from("/tmp/state"),
            endpoint_override: None,
            output_directory_override: None,
            session_filter: None,
            force: false,
            dry_run: false,
            skip_outputs,
            rebuild_state: false,
            max_download_bytes: None,
        }
    }

    /// Runs the placement rules without touching a socket or the filesystem: path planning is pure,
    /// which is what makes the traversal refusals unit-testable rather than only observable through
    /// an end-to-end restore. Port 1 on loopback refuses connections, so a plan that accidentally
    /// reached the network would fail loudly rather than silently pass.
    fn plan_path(
        config: &RestoreConfig,
        markdown_relative: &Path,
        logical_path: &str,
    ) -> Result<PathBuf, String> {
        let client = RestoreClient::connect("http://127.0.0.1:1").unwrap();
        Placer {
            output_directory: Path::new("/tmp/out"),
            markdown_relative,
            config,
            client: &client,
            cap: MAX_ARTIFACT_DOWNLOAD_BYTES,
        }
        .local_relative_path(logical_path)
    }
}
