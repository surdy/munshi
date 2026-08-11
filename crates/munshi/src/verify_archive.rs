//! Archive-wide read-time parse verification: `munshi verify-archive-parse` (issue #28).
//!
//! The acceptance tool for ADR 0011 (transcripts are interpreted at read time through the shared
//! `munshi-transcript` streaming crate) and ADR 0012 (no analysis client is built yet, but the one
//! time-sensitive proof happens now). It walks the Patwari archive — every snapshot, or one
//! session's snapshots — and for each snapshot fetches the canonical manifest for capture
//! provenance (`source_agent`, `artifact_set_version`), resolves the `transcript.jsonl` artifact
//! through the artifact listing, downloads it with the same three-stage verification claim-ticket
//! retrieval performs (stored size + sha256, declared-compression decode, original size + sha256,
//! cross-checked against the listing's declared original hash), and stream-parses it, folding
//! per-snapshot accounting: records seen, share typed, content events by kind, `Empty` records,
//! `Ignored` kinds, `Unknown` kinds with bounded raw samples, and `RecordError`s with line
//! numbers. Its job is to reveal interpretation gaps while original local session files still
//! exist, so findings never abort the walk: every snapshot is scanned, the report aggregates
//! totals per `(source_agent, artifact_set_version)`, and the process exits non-zero afterwards.
//!
//! Snapshots this build cannot interpret are skipped without failing the walk — an unknown
//! `source_agent` (the field is free-form archival metadata), an `artifact_set_version` other
//! than the supported one, a snapshot with no `transcript.jsonl` artifact, or a transcript larger
//! than the download cap. Skips are accounting lines, not errors, but they still count as
//! findings: a clean exit asserts that every archived transcript was downloaded, verified, and
//! parsed with zero `Unknown` records and zero record errors.
//!
//! Downloads are strictly sequential — one artifact at a time — which keeps a whole-archive walk
//! well under Patwari's download-concurrency cap; this is a manual check rerun after format
//! bumps, not a scheduled job, so throughput is deliberately unimportant. Beyond the bounded
//! `Unknown` samples and per-record error context, no transcript content is retained or printed.
//!
//! # Exit codes
//!
//! Mirroring the `munshi retrieve` style, each failure class has a distinct, stable process exit
//! code so scripts can tell outcomes apart without parsing messages:
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | every transcript downloaded, verified, and parsed; zero `Unknown`, zero record errors, zero skips |
//! | 1 | local error (reading configuration) |
//! | 2 | invalid input (for example a malformed `--session` id) |
//! | 3 | no archive server configured |
//! | 4 | findings present: `Unknown` records, record errors, or skipped snapshots |
//! | 5 | server/transport failure (the walk could not start, or a snapshot's fetch failed) |
//! | 6 | verification/integrity failure on at least one artifact |
//!
//! When a completed walk observed several classes at once the most severe wins:
//! verification (6) over transport (5) over findings (4).

use std::collections::BTreeMap;
use std::path::Path;

use munshi_transcript::{
    Classification, Event, MIN_SUPPORTED_ARTIFACT_SET_VERSION, Record, RecordError,
    SUPPORTED_ARTIFACT_SET_VERSION, Source, TranscriptStream,
};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use crate::http::HttpError;
use crate::patwari::{self, PatwariError};
use crate::patwari_read::{
    DownloadError, ListedSnapshot, MAX_ARCHIVE_LISTING_PAGES, MAX_ARTIFACT_DOWNLOAD_BYTES,
    ReadClient, ReadError, SizeDimension, SizeRefusal, SnapshotArtifact,
};

/// The artifact-set-v1 logical path of the transcript artifact.
const TRANSCRIPT_LOGICAL_PATH: &str = "transcript.jsonl";
/// At most this many raw `Unknown` records are carried per snapshot as inspection samples.
const MAX_UNKNOWN_SAMPLES: usize = 3;
/// Each carried `Unknown` sample is truncated to this many characters.
const MAX_UNKNOWN_SAMPLE_CHARS: usize = 240;
/// At most this many record errors are carried per snapshot with line context.
const MAX_RECORD_ERROR_SAMPLES: usize = 5;

/// A walk-aborting failure: nothing was scanned (or the scan could not start). Per-snapshot
/// problems never surface here — they become [`SnapshotStatus`] entries in the report instead.
#[derive(Debug, Error)]
pub enum VerifyArchiveError {
    /// The server rejected the request parameters (for example a malformed `--session` id).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// No archive-upload endpoint is configured, so there is no archive to verify.
    #[error(
        "no archive server is configured; set an archive-upload endpoint before verifying the archive"
    )]
    NotConfigured,
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

impl VerifyArchiveError {
    /// The distinct process exit code for this failure class (see the module docs table).
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidInput(_) => 2,
            Self::NotConfigured => 3,
            Self::Unreachable(_) | Self::Protocol(_) | Self::Server { .. } => 5,
            Self::Config(_) => 1,
        }
    }
}

/// Why a snapshot was set aside without downloading or parsing its transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// The manifest's free-form `source_agent` names no harness this build interprets.
    UnknownSourceAgent,
    /// The capture's `artifact_set_version` is not the version this build supports.
    UnsupportedArtifactSetVersion,
    /// The snapshot has no `transcript.jsonl` artifact.
    NoTranscriptArtifact,
    /// The transcript's declared stored *or* original size exceeds the download cap; rerun with
    /// `--max-download-bytes`. Both dimensions are gated before transfer, so a highly compressible
    /// transcript is set aside rather than decompressed into memory.
    TranscriptTooLarge,
}

/// The class of a non-fatal per-snapshot failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClass {
    /// Downloaded bytes failed size/hash verification or could not be decompressed.
    Verification,
    /// A per-snapshot request failed in transit or the server answered unexpectedly.
    Transport,
}

/// The outcome for one walked snapshot.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum SnapshotStatus {
    /// Transcript downloaded, verified, and stream-parsed; accounting is attached.
    Parsed,
    /// Set aside without download/parse; an accounting line, not an error.
    Skipped { reason: SkipReason, message: String },
    /// Download or verification failed; the walk continued with the next snapshot.
    Failed {
        class: FailureClass,
        message: String,
    },
}

/// One raw `Unknown` record carried for inspection, truncated to a bounded length.
#[derive(Debug, Clone, Serialize)]
pub struct UnknownSample {
    /// 1-based physical line number in the transcript.
    pub line: u64,
    /// The extracted record-kind discriminator: the record's top-level `type`, refined with the
    /// Codex `payload.type` when present. Best effort — an undiscriminated record groups under a
    /// placeholder kind such as `<untyped>`.
    pub kind: String,
    /// The raw record, truncated to a bounded number of characters.
    pub raw: String,
}

/// One record error carried with its line context.
#[derive(Debug, Clone, Serialize)]
pub struct RecordErrorSample {
    /// 1-based physical line number the error occurred on.
    pub line: u64,
    pub message: String,
}

/// The lossless-parse accounting folded from one transcript's stream.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ParseAccounting {
    /// One per non-empty transcript line: parsed records plus record errors.
    pub records_seen: u64,
    pub user_events: u64,
    pub assistant_events: u64,
    pub tool_events: u64,
    /// Recognized content records whose content is empty or blank.
    pub empty_records: u64,
    /// Recognized, deliberately-unarchived records.
    pub ignored_records: u64,
    pub ignored_kinds: BTreeMap<String, u64>,
    /// Records the parser does not recognize at all — the gaps issue #29 hunts.
    pub unknown_records: u64,
    pub unknown_kinds: BTreeMap<String, u64>,
    /// The first few unknown records, truncated — enough to inspect a gap, not enough to leak a
    /// transcript.
    pub unknown_samples: Vec<UnknownSample>,
    /// Malformed lines (including a truncated trailing record).
    pub record_errors: u64,
    /// The first few record errors with line numbers.
    pub record_error_samples: Vec<RecordErrorSample>,
}

impl ParseAccounting {
    fn observe(&mut self, item: &Result<Record, RecordError>) {
        self.records_seen += 1;
        let record = match item {
            Ok(record) => record,
            Err(error) => {
                self.record_errors += 1;
                if self.record_error_samples.len() < MAX_RECORD_ERROR_SAMPLES {
                    self.record_error_samples.push(RecordErrorSample {
                        line: error.line(),
                        message: error.to_string(),
                    });
                }
                return;
            }
        };
        match &record.classification {
            Classification::Content { events } => {
                for event in events {
                    match event {
                        Event::User { .. } => self.user_events += 1,
                        Event::Assistant { .. } => self.assistant_events += 1,
                        Event::Tool(_) => self.tool_events += 1,
                    }
                }
            }
            Classification::Empty => self.empty_records += 1,
            Classification::Ignored { kind } => {
                self.ignored_records += 1;
                *self.ignored_kinds.entry(kind.clone()).or_insert(0) += 1;
            }
            Classification::Unknown { raw } => {
                self.unknown_records += 1;
                let kind = unknown_kind(raw);
                *self.unknown_kinds.entry(kind.clone()).or_insert(0) += 1;
                if self.unknown_samples.len() < MAX_UNKNOWN_SAMPLES {
                    self.unknown_samples.push(UnknownSample {
                        line: record.line,
                        kind,
                        raw: truncate_chars(raw, MAX_UNKNOWN_SAMPLE_CHARS),
                    });
                }
            }
        }
    }

    /// Total typed content events (user + assistant + tool).
    #[must_use]
    pub fn content_events(&self) -> u64 {
        self.user_events + self.assistant_events + self.tool_events
    }

    /// The share of seen records the parser fully interprets — everything except `Unknown`
    /// records and record errors. `None` for an empty transcript.
    #[must_use]
    pub fn typed_share(&self) -> Option<f64> {
        if self.records_seen == 0 {
            return None;
        }
        let typed = self.records_seen - self.unknown_records - self.record_errors;
        #[allow(clippy::cast_precision_loss)]
        Some(typed as f64 / self.records_seen as f64)
    }
}

/// The report entry for one walked snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotReport {
    pub snapshot_id: String,
    /// The Patwari session the snapshot belongs to.
    pub session_id: String,
    /// Manifest `session.source_agent`; `None` when the manifest could not be fetched.
    pub source_agent: Option<String>,
    /// Manifest `capture.artifact_set_version`; `None` when the manifest could not be fetched.
    pub artifact_set_version: Option<u64>,
    pub status: SnapshotStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<ParseAccounting>,
}

/// Aggregate totals for one `(source_agent, artifact_set_version)` provenance group.
#[derive(Debug, Clone, Serialize)]
pub struct GroupAggregate {
    /// `None` groups snapshots whose manifest could not be fetched.
    pub source_agent: Option<String>,
    pub artifact_set_version: Option<u64>,
    pub snapshots: u64,
    pub parsed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub records_seen: u64,
    pub content_events: u64,
    pub unknown_records: u64,
    pub record_errors: u64,
}

/// Whole-run totals.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub snapshots: u64,
    pub parsed: u64,
    pub skipped: u64,
    pub failed_verification: u64,
    pub failed_transport: u64,
    pub records_seen: u64,
    pub content_events: u64,
    pub empty_records: u64,
    pub ignored_records: u64,
    pub unknown_records: u64,
    pub record_errors: u64,
}

/// The completed walk: per-snapshot outcomes, provenance-group aggregates, and run totals.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyArchiveReport {
    pub schema_version: u32,
    pub command: &'static str,
    /// The `--session` filter, when one was given; `None` for a whole-archive walk.
    pub session_filter: Option<String>,
    pub snapshots: Vec<SnapshotReport>,
    pub aggregates: Vec<GroupAggregate>,
    pub totals: Totals,
}

impl VerifyArchiveReport {
    /// The process exit code the completed walk deserves (see the module docs table):
    /// verification failure (6) over transport failure (5) over findings (4) over clean (0).
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        if self.totals.failed_verification > 0 {
            6
        } else if self.totals.failed_transport > 0 {
            5
        } else if self.has_findings() {
            4
        } else {
            0
        }
    }

    fn has_findings(&self) -> bool {
        self.totals.unknown_records > 0 || self.totals.record_errors > 0 || self.totals.skipped > 0
    }

    /// Prints the human rendering to stdout. Transcript content appears only through the bounded
    /// `Unknown` samples.
    pub fn print_human(&self) {
        let totals = &self.totals;
        println!(
            "verify-archive-parse: {} snapshot(s) — {} parsed, {} skipped, {} failed",
            totals.snapshots,
            totals.parsed,
            totals.skipped,
            totals.failed_verification + totals.failed_transport,
        );
        for snapshot in &self.snapshots {
            print_snapshot_human(snapshot);
        }
        if !self.aggregates.is_empty() {
            println!("by source agent and artifact-set version:");
            for group in &self.aggregates {
                println!(
                    "  {} v{}: {} snapshot(s) ({} parsed, {} skipped, {} failed), {} record(s), {} content event(s), {} unknown, {} record error(s)",
                    group.source_agent.as_deref().unwrap_or("<unknown>"),
                    group
                        .artifact_set_version
                        .map_or_else(|| "?".to_owned(), |version| version.to_string()),
                    group.snapshots,
                    group.parsed,
                    group.skipped,
                    group.failed,
                    group.records_seen,
                    group.content_events,
                    group.unknown_records,
                    group.record_errors,
                );
            }
        }
        println!(
            "totals: {} record(s), {} content event(s), {} empty, {} ignored, {} unknown, {} record error(s)",
            totals.records_seen,
            totals.content_events,
            totals.empty_records,
            totals.ignored_records,
            totals.unknown_records,
            totals.record_errors,
        );
        if self.exit_code() == 0 {
            println!(
                "no findings: every transcript parsed with zero unknown records and zero record errors"
            );
        } else {
            println!(
                "findings: {} unknown record(s), {} record error(s), {} skipped snapshot(s), {} verification failure(s), {} transport failure(s)",
                totals.unknown_records,
                totals.record_errors,
                totals.skipped,
                totals.failed_verification,
                totals.failed_transport,
            );
        }
    }
}

fn print_snapshot_human(snapshot: &SnapshotReport) {
    let provenance = format!(
        "{} v{}",
        snapshot.source_agent.as_deref().unwrap_or("<unknown>"),
        snapshot
            .artifact_set_version
            .map_or_else(|| "?".to_owned(), |version| version.to_string()),
    );
    match &snapshot.status {
        SnapshotStatus::Parsed => {
            let accounting = snapshot.accounting.as_ref();
            let Some(accounting) = accounting else {
                // Unreachable by construction; keep the rendering total anyway.
                println!(
                    "snapshot {} (session {}, {provenance}): parsed",
                    snapshot.snapshot_id, snapshot.session_id
                );
                return;
            };
            let share = accounting
                .typed_share()
                .map_or_else(|| "-".to_owned(), |share| format!("{:.1}%", share * 100.0));
            println!(
                "snapshot {} (session {}, {provenance}): parsed — {} record(s), {share} typed, {} user / {} assistant / {} tool, {} empty, {} ignored, {} unknown, {} record error(s)",
                snapshot.snapshot_id,
                snapshot.session_id,
                accounting.records_seen,
                accounting.user_events,
                accounting.assistant_events,
                accounting.tool_events,
                accounting.empty_records,
                accounting.ignored_records,
                accounting.unknown_records,
                accounting.record_errors,
            );
            if !accounting.ignored_kinds.is_empty() {
                let kinds: Vec<String> = accounting
                    .ignored_kinds
                    .iter()
                    .map(|(kind, count)| format!("{kind}={count}"))
                    .collect();
                println!("  ignored kinds: {}", kinds.join(" "));
            }
            if !accounting.unknown_kinds.is_empty() {
                let kinds: Vec<String> = accounting
                    .unknown_kinds
                    .iter()
                    .map(|(kind, count)| format!("{kind}={count}"))
                    .collect();
                println!("  unknown kinds: {}", kinds.join(" "));
                for sample in &accounting.unknown_samples {
                    println!("    line {} [{}]: {}", sample.line, sample.kind, sample.raw);
                }
            }
            for sample in &accounting.record_error_samples {
                println!("  record error line {}: {}", sample.line, sample.message);
            }
        }
        SnapshotStatus::Skipped { message, .. } => {
            println!(
                "snapshot {} (session {}, {provenance}): skipped — {message}",
                snapshot.snapshot_id, snapshot.session_id
            );
        }
        SnapshotStatus::Failed { class, message } => {
            let class = match class {
                FailureClass::Verification => "verification",
                FailureClass::Transport => "transport",
            };
            println!(
                "snapshot {} (session {}, {provenance}): failed ({class}) — {message}",
                snapshot.snapshot_id, snapshot.session_id
            );
        }
    }
}

/// Walks the archive and returns the completed report. `session_id` limits the walk to one
/// Patwari session; `None` walks every snapshot. `endpoint_override` bypasses configuration;
/// otherwise the endpoint recorded by archive-upload configuration is used (upload does not need
/// to be enabled — only a server address is required, exactly as claim-ticket retrieval).
/// `max_download_bytes` overrides the default per-artifact stored-byte cap.
///
/// Only walk-startup problems return `Err`; every per-snapshot problem is folded into the report
/// so a single bad snapshot never hides the rest of the archive.
pub fn verify_archive_parse(
    state_directory: &Path,
    endpoint_override: Option<&str>,
    session_id: Option<&str>,
    max_download_bytes: Option<usize>,
) -> Result<VerifyArchiveReport, VerifyArchiveError> {
    let cap = max_download_bytes.unwrap_or(MAX_ARTIFACT_DOWNLOAD_BYTES);
    let endpoint = match endpoint_override {
        Some(endpoint) => endpoint.to_owned(),
        None => configured_endpoint(state_directory)?,
    };
    let client = VerifyClient::connect(&endpoint)?;
    let listed = client.list_snapshots(session_id)?;
    // Strictly sequential: one manifest fetch, one artifact listing, one download at a time.
    let snapshots: Vec<SnapshotReport> = listed
        .iter()
        .map(|snapshot| verify_snapshot(&client, snapshot, cap))
        .collect();
    Ok(build_report(session_id, snapshots))
}

/// Reads the archive-upload endpoint recorded in configuration.
fn configured_endpoint(state_directory: &Path) -> Result<String, VerifyArchiveError> {
    let report = patwari::status(state_directory).map_err(VerifyArchiveError::Config)?;
    report
        .settings
        .endpoint
        .filter(|endpoint| !endpoint.is_empty())
        .ok_or(VerifyArchiveError::NotConfigured)
}

/// Maps a manifest `source_agent` label onto the parser's source identity. The labels mirror
/// `SourceKind::agent_label`, implemented locally so this module stays self-contained; any other
/// label — the field is free-form archival metadata — is a reported skip, never an error.
fn parse_source_agent(label: &str) -> Option<Source> {
    match label {
        "copilot-cli" => Some(Source::Copilot),
        "claude-code" => Some(Source::ClaudeCode),
        "codex-cli" => Some(Source::Codex),
        _ => None,
    }
}

fn verify_snapshot(client: &VerifyClient, snapshot: &ListedSnapshot, cap: usize) -> SnapshotReport {
    let mut report = SnapshotReport {
        snapshot_id: snapshot.snapshot_id.clone(),
        session_id: snapshot.session_id.clone(),
        source_agent: None,
        artifact_set_version: None,
        status: SnapshotStatus::Parsed,
        accounting: None,
    };
    let provenance = match client.snapshot_provenance(&snapshot.snapshot_id) {
        Ok(provenance) => provenance,
        Err(issue) => {
            report.status = issue.into_status();
            return report;
        }
    };
    report.source_agent = Some(provenance.source_agent.clone());
    report.artifact_set_version = Some(provenance.artifact_set_version);

    // Skip checks come before any artifact resolution or download: a snapshot this build cannot
    // interpret is reported without transferring bytes.
    let Some(source) = parse_source_agent(&provenance.source_agent) else {
        report.status = SnapshotStatus::Skipped {
            reason: SkipReason::UnknownSourceAgent,
            message: format!(
                "source agent `{}` is not interpreted by this build",
                provenance.source_agent
            ),
        };
        return report;
    };
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
        return report;
    }

    let artifacts = match client.list_snapshot_artifacts(&snapshot.snapshot_id) {
        Ok(artifacts) => artifacts,
        Err(issue) => {
            report.status = issue.into_status();
            return report;
        }
    };
    let Some(transcript) = artifacts
        .into_iter()
        .find(|artifact| artifact.logical_path == TRANSCRIPT_LOGICAL_PATH)
    else {
        report.status = SnapshotStatus::Skipped {
            reason: SkipReason::NoTranscriptArtifact,
            message: format!("snapshot has no {TRANSCRIPT_LOGICAL_PATH} artifact"),
        };
        return report;
    };
    // The download's size gate refuses an oversized or amplifying transcript before any transfer;
    // for a walk that is an accounting line, not a failure.
    let original_bytes = match client.download_verified(&transcript, cap) {
        Ok(bytes) => bytes,
        Err(status) => {
            report.status = status;
            return report;
        }
    };

    // The version was checked against SUPPORTED_ARTIFACT_SET_VERSION above, so stream selection
    // cannot fail; fold the whole stream — findings are items, never aborts.
    let stream = match TranscriptStream::new(
        source,
        SUPPORTED_ARTIFACT_SET_VERSION,
        original_bytes.as_slice(),
    ) {
        Ok(stream) => stream,
        Err(error) => {
            report.status = SnapshotStatus::Skipped {
                reason: SkipReason::UnsupportedArtifactSetVersion,
                message: error.to_string(),
            };
            return report;
        }
    };
    let mut accounting = ParseAccounting::default();
    for item in stream {
        accounting.observe(&item);
    }
    report.status = SnapshotStatus::Parsed;
    report.accounting = Some(accounting);
    report
}

fn build_report(
    session_filter: Option<&str>,
    snapshots: Vec<SnapshotReport>,
) -> VerifyArchiveReport {
    let mut totals = Totals::default();
    let mut groups: BTreeMap<(Option<String>, Option<u64>), GroupAggregate> = BTreeMap::new();
    for snapshot in &snapshots {
        totals.snapshots += 1;
        let key = (snapshot.source_agent.clone(), snapshot.artifact_set_version);
        let group = groups.entry(key).or_insert_with(|| GroupAggregate {
            source_agent: snapshot.source_agent.clone(),
            artifact_set_version: snapshot.artifact_set_version,
            snapshots: 0,
            parsed: 0,
            skipped: 0,
            failed: 0,
            records_seen: 0,
            content_events: 0,
            unknown_records: 0,
            record_errors: 0,
        });
        group.snapshots += 1;
        match &snapshot.status {
            SnapshotStatus::Parsed => {
                totals.parsed += 1;
                group.parsed += 1;
            }
            SnapshotStatus::Skipped { .. } => {
                totals.skipped += 1;
                group.skipped += 1;
            }
            SnapshotStatus::Failed { class, .. } => {
                match class {
                    FailureClass::Verification => totals.failed_verification += 1,
                    FailureClass::Transport => totals.failed_transport += 1,
                }
                group.failed += 1;
            }
        }
        if let Some(accounting) = &snapshot.accounting {
            totals.records_seen += accounting.records_seen;
            totals.content_events += accounting.content_events();
            totals.empty_records += accounting.empty_records;
            totals.ignored_records += accounting.ignored_records;
            totals.unknown_records += accounting.unknown_records;
            totals.record_errors += accounting.record_errors;
            group.records_seen += accounting.records_seen;
            group.content_events += accounting.content_events();
            group.unknown_records += accounting.unknown_records;
            group.record_errors += accounting.record_errors;
        }
    }
    VerifyArchiveReport {
        schema_version: 1,
        command: "verify-archive-parse",
        session_filter: session_filter.map(ToOwned::to_owned),
        snapshots,
        aggregates: groups.into_values().collect(),
        totals,
    }
}

// ---------------------------------------------------------------------------
// Patwari read client (snapshot walking)
// ---------------------------------------------------------------------------

/// A non-fatal per-snapshot problem, mapped onto a [`SnapshotStatus::Failed`] entry.
struct SnapshotIssue {
    class: FailureClass,
    message: String,
}

impl SnapshotIssue {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            class: FailureClass::Transport,
            message: message.into(),
        }
    }

    fn verification(message: impl Into<String>) -> Self {
        Self {
            class: FailureClass::Verification,
            message: message.into(),
        }
    }

    fn into_status(self) -> SnapshotStatus {
        SnapshotStatus::Failed {
            class: self.class,
            message: self.message,
        }
    }
}

/// A synchronous archive-walking client bound to one server: the shared Patwari read stack
/// ([`crate::patwari_read`]) plus the walk's own error surface. Every wire rule — pagination, the
/// size gate, the three-stage verification — lives in the shared module; what stays here is the
/// mapping onto walk-aborting [`VerifyArchiveError`]s and per-snapshot [`SnapshotStatus`] entries.
struct VerifyClient {
    client: ReadClient,
}

impl VerifyClient {
    fn connect(endpoint: &str) -> Result<Self, VerifyArchiveError> {
        Ok(Self {
            client: ReadClient::connect(endpoint).map_err(from_http)?,
        })
    }

    /// The archive's snapshots, newest first. The traversal itself lives in the shared read stack;
    /// what the walk decides here is that a listing which hit the page bound is a hard failure —
    /// an acceptance walk that silently skipped snapshots is worse than no walk at all.
    fn list_snapshots(
        &self,
        session_id: Option<&str>,
    ) -> Result<Vec<ListedSnapshot>, VerifyArchiveError> {
        let listing = self
            .client
            .list_snapshots(session_id)
            .map_err(|error| match error {
                // 422 is the server rejecting the walk's own parameters (a malformed --session).
                ReadError::Status {
                    status: 422, code, ..
                } => VerifyArchiveError::InvalidInput(format!(
                    "the archive server rejected the request: {}",
                    code.unwrap_or_else(|| "invalid_request".to_owned())
                )),
                other => from_read(other),
            })?;
        if !listing.terminated {
            return Err(VerifyArchiveError::Protocol(format!(
                "snapshot listing did not terminate within {MAX_ARCHIVE_LISTING_PAGES} pages"
            )));
        }
        Ok(listing.items)
    }

    /// The snapshot's capture provenance, with every read failure graded as a per-snapshot
    /// transport problem: an unreadable manifest is one snapshot's misfortune, not the walk's.
    fn snapshot_provenance(
        &self,
        snapshot_id: &str,
    ) -> Result<crate::patwari_read::SnapshotProvenance, SnapshotIssue> {
        self.client
            .snapshot_provenance(snapshot_id)
            .map_err(|error| snapshot_transport("manifest fetch", error))
    }

    /// The snapshot's artifacts. An artifact listing that hit the page bound would silently hide
    /// the transcript, so it is graded as a failure for this snapshot rather than an empty set.
    fn list_snapshot_artifacts(
        &self,
        snapshot_id: &str,
    ) -> Result<Vec<SnapshotArtifact>, SnapshotIssue> {
        let listing = self
            .client
            .list_snapshot_artifacts(snapshot_id)
            .map_err(|error| snapshot_transport("artifact listing", error))?;
        if !listing.terminated {
            return Err(SnapshotIssue::transport(format!(
                "artifact listing did not terminate within {MAX_ARCHIVE_LISTING_PAGES} pages"
            )));
        }
        Ok(listing.items)
    }

    /// Downloads the transcript through the shared three-stage verification (stored size + sha256,
    /// declared-compression decode, original size + sha256 cross-checked against the listing's
    /// declared hash), gated on both declared sizes against `cap` before any transfer. No
    /// unverified byte is ever parsed.
    ///
    /// The walk grades the outcomes itself: a refusal by the size gate is a skip — an accounting
    /// line — while an integrity or transport problem is a per-snapshot failure that never stops
    /// the walk.
    fn download_verified(
        &self,
        artifact: &SnapshotArtifact,
        cap: usize,
    ) -> Result<Vec<u8>, SnapshotStatus> {
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
                    SnapshotStatus::Skipped {
                        reason: SkipReason::TranscriptTooLarge,
                        message: format!(
                            "transcript {dimension} size {size_bytes} bytes exceeds the {cap}-byte download cap; pass --max-download-bytes to raise it"
                        ),
                    }
                }
                DownloadError::Http(error) => {
                    SnapshotIssue::transport(error.to_string()).into_status()
                }
                DownloadError::Status { status, code } => SnapshotIssue::transport(format!(
                    "content download returned status {status}: {}",
                    code.unwrap_or_else(|| "unknown".to_owned())
                ))
                .into_status(),
                DownloadError::Protocol(message) => {
                    SnapshotIssue::transport(message).into_status()
                }
                DownloadError::Verification(message) => {
                    SnapshotIssue::verification(message).into_status()
                }
                DownloadError::Decompression(message) => SnapshotIssue::verification(format!(
                    "could not decompress stored content: {message}"
                ))
                .into_status(),
            })
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Best-effort record-kind discriminator for an `Unknown` raw record: the top-level `type`,
/// refined with the Codex `payload.type` when present. Never fails — an undiscriminated record
/// groups under a placeholder kind.
fn unknown_kind(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return "<unparsed>".to_owned();
    };
    let Some(object) = value.as_object() else {
        return "<non-object>".to_owned();
    };
    let Some(kind) = object.get("type").and_then(Value::as_str) else {
        return "<untyped>".to_owned();
    };
    if let Some(item_type) = object
        .get("payload")
        .and_then(Value::as_object)
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str)
    {
        return format!("{kind}/{item_type}");
    }
    kind.to_owned()
}

/// Truncates to a bounded number of characters (never mid-code-point), marking the cut.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut taken = String::new();
    for (count, character) in text.chars().enumerate() {
        if count == max_chars {
            taken.push('…');
            return taken;
        }
        taken.push(character);
    }
    taken
}

/// Grades a shared-stack read failure as a per-snapshot transport problem, naming the call that
/// produced it. Every class collapses to transport here on purpose: from the walk's point of view a
/// snapshot whose metadata cannot be read is a snapshot it could not reach, whatever the reason.
fn snapshot_transport(call: &str, error: ReadError) -> SnapshotIssue {
    match error {
        ReadError::Http(error) => SnapshotIssue::transport(error.to_string()),
        ReadError::Status { status, code } => SnapshotIssue::transport(format!(
            "{call} returned status {status}: {}",
            code.unwrap_or_else(|| "unknown".to_owned())
        )),
        ReadError::Protocol(message) => SnapshotIssue::transport(message),
    }
}

/// Maps a shared-stack listing failure onto a walk-aborting error. The 422 case is handled by the
/// snapshot listing, which words it as rejected input rather than a server fault.
fn from_read(error: ReadError) -> VerifyArchiveError {
    match error {
        ReadError::Http(error) => from_http(error),
        ReadError::Status { status, code } => VerifyArchiveError::Server {
            status,
            code: code.unwrap_or_else(|| "unknown".to_owned()),
        },
        ReadError::Protocol(message) => VerifyArchiveError::Protocol(message),
    }
}

fn from_http(error: HttpError) -> VerifyArchiveError {
    match error {
        HttpError::UnsupportedEndpoint(endpoint) => {
            VerifyArchiveError::Unreachable(format!("{endpoint} is not a supported http(s) URL"))
        }
        HttpError::Transport(message) => VerifyArchiveError::Unreachable(message),
        HttpError::Protocol(message) => VerifyArchiveError::Protocol(message),
        HttpError::Tls(message) => {
            VerifyArchiveError::Unreachable(format!("tls setup failed: {message}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_source_agent_labels_and_rejects_foreign_ones() {
        assert_eq!(parse_source_agent("copilot-cli"), Some(Source::Copilot));
        assert_eq!(parse_source_agent("claude-code"), Some(Source::ClaudeCode));
        assert_eq!(parse_source_agent("codex-cli"), Some(Source::Codex));
        assert_eq!(parse_source_agent("mystery-agent-9000"), None);
        assert_eq!(parse_source_agent(""), None);
    }

    #[test]
    fn unknown_kind_extracts_type_discriminators() {
        assert_eq!(unknown_kind(r#"{"type":"wibble"}"#), "wibble");
        assert_eq!(
            unknown_kind(r#"{"type":"response_item","payload":{"type":"hologram_call"}}"#),
            "response_item/hologram_call"
        );
        assert_eq!(unknown_kind(r#"{"no_type":true}"#), "<untyped>");
        assert_eq!(unknown_kind("not json"), "<unparsed>");
        assert_eq!(unknown_kind("[1,2]"), "<non-object>");
    }

    #[test]
    fn truncation_is_character_safe_and_marks_the_cut() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("exact", 5), "exact");
        assert_eq!(truncate_chars("überlong", 4), "über…");
    }

    #[test]
    fn accounting_folds_every_stream_item_and_bounds_samples() {
        let mut transcript = Vec::new();
        // A content record, an unknown record repeated past the sample bound, and a malformed line.
        transcript.extend_from_slice(
            br#"{"type":"user","message":{"content":"hello"},"timestamp":"2026-07-11T20:00:00.000Z"}"#,
        );
        transcript.push(b'\n');
        for _ in 0..(MAX_UNKNOWN_SAMPLES + 2) {
            transcript.extend_from_slice(br#"{"type":"wibble-2.2"}"#);
            transcript.push(b'\n');
        }
        transcript.extend_from_slice(b"{\"broken");
        transcript.push(b'\n');

        let stream = TranscriptStream::new(
            Source::ClaudeCode,
            SUPPORTED_ARTIFACT_SET_VERSION,
            transcript.as_slice(),
        )
        .unwrap();
        let mut accounting = ParseAccounting::default();
        for item in stream {
            accounting.observe(&item);
        }

        assert_eq!(accounting.records_seen, (MAX_UNKNOWN_SAMPLES + 4) as u64);
        assert_eq!(accounting.user_events, 1);
        assert_eq!(accounting.unknown_records, (MAX_UNKNOWN_SAMPLES + 2) as u64);
        assert_eq!(
            accounting.unknown_kinds.get("wibble-2.2"),
            Some(&((MAX_UNKNOWN_SAMPLES + 2) as u64))
        );
        assert_eq!(accounting.unknown_samples.len(), MAX_UNKNOWN_SAMPLES);
        assert_eq!(accounting.record_errors, 1);
        assert_eq!(accounting.record_error_samples.len(), 1);
        assert_eq!(
            accounting.record_error_samples[0].line,
            (MAX_UNKNOWN_SAMPLES + 4) as u64
        );
        let share = accounting.typed_share().unwrap();
        assert!(share > 0.0 && share < 1.0);
    }

    #[test]
    fn report_exit_codes_rank_verification_over_transport_over_findings() {
        fn report(totals: Totals) -> VerifyArchiveReport {
            VerifyArchiveReport {
                schema_version: 1,
                command: "verify-archive-parse",
                session_filter: None,
                snapshots: Vec::new(),
                aggregates: Vec::new(),
                totals,
            }
        }
        assert_eq!(report(Totals::default()).exit_code(), 0);
        assert_eq!(
            report(Totals {
                unknown_records: 1,
                ..Totals::default()
            })
            .exit_code(),
            4
        );
        assert_eq!(
            report(Totals {
                record_errors: 1,
                ..Totals::default()
            })
            .exit_code(),
            4
        );
        assert_eq!(
            report(Totals {
                skipped: 1,
                ..Totals::default()
            })
            .exit_code(),
            4
        );
        assert_eq!(
            report(Totals {
                skipped: 1,
                failed_transport: 1,
                ..Totals::default()
            })
            .exit_code(),
            5
        );
        assert_eq!(
            report(Totals {
                skipped: 1,
                failed_transport: 1,
                failed_verification: 1,
                ..Totals::default()
            })
            .exit_code(),
            6
        );
    }

    #[test]
    fn build_report_aggregates_by_provenance_group() {
        let parsed = |agent: &str, records: u64| SnapshotReport {
            snapshot_id: format!("snap-{agent}-{records}"),
            session_id: "sess".to_owned(),
            source_agent: Some(agent.to_owned()),
            artifact_set_version: Some(1),
            status: SnapshotStatus::Parsed,
            accounting: Some(ParseAccounting {
                records_seen: records,
                user_events: records,
                ..ParseAccounting::default()
            }),
        };
        let skipped = SnapshotReport {
            snapshot_id: "snap-skip".to_owned(),
            session_id: "sess".to_owned(),
            source_agent: Some("mystery".to_owned()),
            artifact_set_version: Some(1),
            status: SnapshotStatus::Skipped {
                reason: SkipReason::UnknownSourceAgent,
                message: "unknown".to_owned(),
            },
            accounting: None,
        };
        let report = build_report(
            Some("sess"),
            vec![
                parsed("claude-code", 3),
                parsed("claude-code", 2),
                parsed("codex-cli", 4),
                skipped,
            ],
        );
        assert_eq!(report.totals.snapshots, 4);
        assert_eq!(report.totals.parsed, 3);
        assert_eq!(report.totals.skipped, 1);
        assert_eq!(report.totals.records_seen, 9);
        assert_eq!(report.totals.content_events, 9);
        assert_eq!(report.aggregates.len(), 3);
        let claude = report
            .aggregates
            .iter()
            .find(|group| group.source_agent.as_deref() == Some("claude-code"))
            .unwrap();
        assert_eq!(claude.snapshots, 2);
        assert_eq!(claude.records_seen, 5);
        assert_eq!(report.session_filter.as_deref(), Some("sess"));
        // Skips are findings: the report exits non-zero without any parse-level finding.
        assert_eq!(report.exit_code(), 4);
    }
}
