//! The Patwari archive-upload client: registration, manifest assembly, and the resumable chunked
//! upload flow (ADR 0009, issue #19).
//!
//! This is the transport foundation the later artifact-set (#20) and retrieval (#22) work builds
//! on. Archive upload submits one summary revision's full snapshot to a Patwari archive server: a
//! versioned manifest plus a set of zstd-compressed artifacts, transferred in resumable chunks and
//! finalized into a verified receipt. It runs strictly downstream of local archival and in parallel
//! with Notesmith delivery — a Patwari failure never blocks either (CONTEXT.md "archive upload").
//!
//! Munshi is fully synchronous: this speaks the Patwari HTTP API over the shared blocking
//! [`crate::http`] client with no async or TLS dependency, exactly as delivery does.
//!
//! ## Self-containment
//! Every snapshot is *full*: `summary.md`, the verbatim `transcript.jsonl`, and every re-derived
//! `outputs/<sha256>` extracted output. There is one artifact-assembly path ([`collect_artifacts`])
//! and it refuses to assemble a reduced set, so no upload path can publish a snapshot that is not
//! self-contained; a session whose transcript is not readable is skipped instead (issue #47). The
//! upload ledger records the artifact set each snapshot carried, which is what lets
//! `archive-upload backfill` find and re-upload snapshots that predate this guarantee.
//!
//! ## Idempotency
//! Patwari keys capture idempotency on `(owner, client, capture_id)` and rejects a reused
//! `capture_id` whose canonical manifest changed. Munshi therefore mints a fresh `capture_id` (and
//! a stable `captured_at`) per distinct snapshot attempt and reuses that exact pair on retry, so an
//! interrupted upload resumes rather than duplicates. The client UUID is persistent and durable.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::http::{self, Header, HttpError};
use crate::patwari_read::{ReadClient, ReadError};
use crate::registration::{
    DEFAULT_MAX_ARCHIVE_UPLOAD_ATTEMPTS, RegistrationError, StoredConfig, load_stored_config,
    stored_config_exists, update_stored_config,
};
use crate::source::{SidecarFile, SourceHomes, SourceKind, derive_transcript_path};
use crate::state::{
    ArchiveUploadRecord, ArchiveUploadSuccess, SessionRecord, StateError, StateStore, now_ms,
    try_acquire_session_lock,
};

/// The API base path every Patwari route is nested under.
const API_BASE: &str = "/api/v1";
/// The current manifest schema version Patwari accepts.
const MANIFEST_SCHEMA_VERSION: u16 = 1;
/// The initial snapshot artifact-set version: `summary.md`, `transcript.jsonl`, and
/// `outputs/<sha256>` extracted outputs.
pub const INITIAL_ARTIFACT_SET_VERSION: u16 = 1;
/// The artifact-set version new snapshots record. Version 2 (issue #23) additionally allows
/// optional `sidecar/<relative-path>` artifacts carrying harness sidecar state staged at archive
/// time; presence is per-adapter conditional (Copilot stages an allowlisted set, Claude Code and
/// Codex stage nothing) and consumers must tolerate absent kinds, so transcript interpretation is
/// unchanged from v1.
pub const CURRENT_ARTIFACT_SET_VERSION: u16 = 2;
/// The logical-path prefix of staged sidecar artifacts (artifact set v2, issue #23).
pub(crate) const SIDECAR_LOGICAL_PREFIX: &str = "sidecar/";
const OUTPUTS_LOGICAL_PREFIX: &str = "outputs/";
/// Custom chunk headers Patwari requires on each artifact chunk PUT.
const CHUNK_SHA256_HEADER: &str = "x-patwari-chunk-sha256";
const CHUNK_LENGTH_HEADER: &str = "x-patwari-chunk-length";
/// Base backoff between failed upload attempts; doubles per attempt up to [`MAX_BACKOFF_MS`].
const BASE_BACKOFF_MS: i64 = 60_000;
/// Upper bound on upload backoff so a long outage still retries roughly hourly.
const MAX_BACKOFF_MS: i64 = 3_600_000;
/// Network timeout for a single upload request. Larger than delivery's: chunk bodies are bigger.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Completion verifies and decompresses the entire snapshot server-side. Large transcripts can
/// legitimately take much longer than an individual chunk or status request.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Error)]
pub enum PatwariError {
    #[error(transparent)]
    Registration(#[from] RegistrationError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("archive upload is not enabled; enable it in Munshi configuration")]
    NotEnabled,
    #[error("archive upload server is not configured")]
    NotConfigured,
    #[error("archive upload I/O failed")]
    Io(#[source] std::io::Error),
    #[error("archive upload endpoint {0} is not a supported http URL")]
    UnsupportedEndpoint(String),
    #[error("archive upload transport failed: {0}")]
    Transport(String),
    #[error("archive upload protocol error: {0}")]
    Protocol(String),
    #[error("Patwari returned status {status}: {code}")]
    Server { status: u16, code: String },
    /// The capture id was reused for a changed manifest — a client bug, never retried blindly.
    #[error("capture identifier conflicts with a prior manifest")]
    CaptureConflict,
    /// A chunk index was already accepted with different bytes.
    #[error("chunk conflicts with previously accepted bytes")]
    ChunkConflict,
    /// The live transcript changed between archival and this upload read: its sha256 no longer
    /// matches the archived revision's source hash (the frontmatter `transcript_sha256` and the
    /// claim tickets). Retryable — a later revision re-archives the grown transcript and converges
    /// (ADR 0009).
    #[error(
        "transcript changed under the upload; its hash no longer matches the archived revision"
    )]
    TranscriptChanged,
    /// A required artifact is not readable locally, so the assembled set would not be a
    /// self-contained snapshot (ADR 0009, issue #47). Carries the missing artifact's logical path.
    /// Never uploaded as a partial snapshot: the upload is skipped until the artifact is readable.
    #[error("the snapshot is not self-contained: {0} is unavailable locally")]
    SnapshotIncomplete(&'static str),
}

impl PatwariError {
    /// A stable, safe category for retry bookkeeping and diagnostics.
    pub fn category(&self) -> &'static str {
        match self {
            Self::Registration(_) => "upload-registration",
            Self::State(_) => "upload-state",
            Self::NotEnabled => "upload-not-enabled",
            Self::NotConfigured => "upload-not-configured",
            Self::Io(_) => "upload-io",
            Self::UnsupportedEndpoint(_) => "upload-endpoint",
            Self::Transport(_) => "upload-transport",
            Self::Protocol(_) => "upload-protocol",
            Self::Server { .. } => "upload-server",
            Self::CaptureConflict => "capture-conflict",
            Self::ChunkConflict => "chunk-conflict",
            Self::TranscriptChanged => "transcript-changed",
            Self::SnapshotIncomplete(_) => "snapshot-incomplete",
        }
    }
}

fn from_http(error: HttpError) -> PatwariError {
    match error {
        HttpError::UnsupportedEndpoint(endpoint) => PatwariError::UnsupportedEndpoint(endpoint),
        HttpError::Transport(message) => PatwariError::Transport(message),
        HttpError::Protocol(message) => PatwariError::Protocol(message),
        HttpError::Tls(message) => PatwariError::Transport(format!("tls setup failed: {message}")),
    }
}

// ---------------------------------------------------------------------------
// Artifact preparation (zstd compression + hashing)
// ---------------------------------------------------------------------------

/// The zstd compression level. Level 3 is zstd's default: a strong size/speed balance, and
/// deterministic for a given input so a retry produces byte-identical stored bytes.
const ZSTD_LEVEL: i32 = 3;

/// The rendered summary's reserved logical path.
pub const SUMMARY_LOGICAL_PATH: &str = "summary.md";
/// The verbatim transcript's reserved logical path.
pub const TRANSCRIPT_LOGICAL_PATH: &str = "transcript.jsonl";
/// The artifacts every snapshot must contain to be self-contained (ADR 0009, issue #47). Extracted
/// `outputs/<sha256>` artifacts are re-derived from the transcript, so requiring these two requires
/// the whole set: a snapshot carrying both is complete by construction.
const REQUIRED_LOGICAL_PATHS: [&str; 2] = [SUMMARY_LOGICAL_PATH, TRANSCRIPT_LOGICAL_PATH];

/// One artifact to include in a snapshot, identified by its reserved logical path (ADR 0009). The
/// artifact list is deliberately open so issue #20 can extend the set without changing this API.
#[derive(Debug, Clone)]
pub struct ArtifactSource {
    pub logical_path: String,
    pub media_type: Option<String>,
    /// The original (uncompressed) content bytes.
    pub bytes: Vec<u8>,
}

/// An artifact after local compression, carrying both the original and stored representations and
/// their sizes and sha256 digests (hex, unprefixed). `stored_bytes` are the bytes actually uploaded.
#[derive(Debug, Clone)]
pub struct PreparedArtifact {
    pub logical_path: String,
    pub media_type: Option<String>,
    pub original_size_bytes: u64,
    pub original_sha256: String,
    pub stored_size_bytes: u64,
    pub stored_sha256: String,
    /// `"zstd"` or `"identity"` — Patwari's accepted compression tokens.
    pub compression: &'static str,
    pub stored_bytes: Vec<u8>,
}

/// Compresses one artifact's bytes with zstd and records both representations. When compression
/// does not shrink the content (tiny or incompressible input) the identity representation is stored
/// instead, so the stored bytes are never larger than the original.
pub fn prepare_artifact(source: ArtifactSource) -> PreparedArtifact {
    let ArtifactSource {
        logical_path,
        media_type,
        bytes,
    } = source;
    let original_size_bytes = bytes.len() as u64;
    let original_sha256 = sha256_hex(&bytes);
    // Keep the compressed representation only when it actually shrinks the content; otherwise the
    // owned original bytes are moved into the identity representation rather than cloned.
    let compressed = zstd::encode_all(bytes.as_slice(), ZSTD_LEVEL)
        .ok()
        .filter(|compressed| compressed.len() < bytes.len());
    let (compression, stored_bytes) = match compressed {
        Some(compressed) => ("zstd", compressed),
        None => ("identity", bytes),
    };
    PreparedArtifact {
        logical_path,
        media_type,
        original_size_bytes,
        original_sha256,
        stored_size_bytes: stored_bytes.len() as u64,
        stored_sha256: sha256_hex(&stored_bytes),
        compression,
        stored_bytes,
    }
}

/// Compresses and hashes an ordered artifact set.
pub fn prepare_artifacts(sources: Vec<ArtifactSource>) -> Vec<PreparedArtifact> {
    sources.into_iter().map(prepare_artifact).collect()
}

// ---------------------------------------------------------------------------
// Manifest assembly
// ---------------------------------------------------------------------------

/// The session block of a manifest: the source harness and its stable session id.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub source_agent: String,
    pub source_session_id: String,
}

/// The capture block of a manifest: provenance for one durable capture observation. Every field is
/// deterministic for a given attempt so a reused `capture_id` re-serializes to the same canonical
/// manifest on retry.
#[derive(Debug, Clone)]
pub struct CaptureContext {
    pub captured_at: String,
    pub source_cursor: Option<String>,
    pub source_state_hash: Option<String>,
    /// Opaque capture provenance, returned verbatim by Patwari and never interpreted by it. Keys
    /// are lowercase snake with short sanitized values; today `origin` (issue #40) plus the
    /// capture machine's `utc_offset` and `hostname` (issue #77, consumed by qanungo's activity
    /// heatmap and per-device scope). Every key is individually optional and readers ignore ones
    /// they do not know, so this map extends without a schema change and older captures that lack
    /// a key stay readable. See `capture_source_metadata`.
    ///
    /// Instruction-file provenance (issue #77) adds `claude_md` and `agents_md`, which qanungo's
    /// instructions-doctor anchors "an instruction edit landed" on by watching the value change
    /// between captures:
    ///
    /// - A value of 64 lowercase hex is the sha256 of `<project root>/CLAUDE.md` or
    ///   `<project root>/AGENTS.md`. The root is the directory the session's project identity is
    ///   itself derived from, so the digest and the identity always name the same project. Only
    ///   that one file is considered — no ancestor walk, no `~/.claude/CLAUDE.md`. The hash is all
    ///   that travels; the file's content is never uploaded.
    /// - The value `absent` means the root was readable and the file provably was not there
    ///   (`ErrorKind::NotFound`). That is a positive observation: a capture that says `absent`
    ///   followed by one that says a digest is an instruction file being *created*.
    /// - The key is *omitted* whenever munshi could not look: origin access was withheld from this
    ///   attempt (issue #61's background worker), the session records no `origin_cwd` (codex
    ///   sessions record none, so they never carry these keys), the root did not resolve,
    ///   permission was denied, the path is a symlink, a directory, or a device, or the file is
    ///   larger than `MAX_INSTRUCTION_BYTES`. Omitted and `absent` are deliberately different
    ///   answers: "we did not look" must never read as "it is not there".
    pub source_metadata: BTreeMap<String, String>,
    pub project: Option<String>,
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub source_agent_version: Option<String>,
    pub artifact_set_version: u16,
    pub munshi_version: Option<String>,
}

/// Assembles the `ManifestInput` document Patwari's `POST /uploads` accepts. Artifact digests are
/// emitted in Patwari's `sha256:<hex>` form. The artifact list is passed by reference so a caller
/// can build one manifest over an easily extended set of prepared artifacts.
pub fn build_manifest(
    session: &SessionContext,
    capture: &CaptureContext,
    artifacts: &[PreparedArtifact],
) -> Value {
    let artifacts_json = prepared_artifacts_json(artifacts);
    json!({
        "schema_version": MANIFEST_SCHEMA_VERSION,
        "session": {
            "source_agent": session.source_agent,
            "source_session_id": session.source_session_id,
        },
        "capture": {
            "captured_at": capture.captured_at,
            "source_cursor": capture.source_cursor,
            "source_state_hash": capture.source_state_hash,
            "source_metadata": capture.source_metadata,
            "project": capture.project,
            "repository": capture.repository,
            "branch": capture.branch,
            "source_agent_version": capture.source_agent_version,
            "artifact_set_version": capture.artifact_set_version,
            "munshi_version": capture.munshi_version,
        },
        "artifacts": artifacts_json,
    })
}

fn prepared_artifacts_json(artifacts: &[PreparedArtifact]) -> Vec<Value> {
    artifacts
        .iter()
        .map(|artifact| {
            json!({
                "logical_path": artifact.logical_path,
                "media_type": artifact.media_type,
                "original_size_bytes": artifact.original_size_bytes,
                "original_sha256": prefixed_digest(&artifact.original_sha256),
                "stored_size_bytes": artifact.stored_size_bytes,
                "stored_sha256": prefixed_digest(&artifact.stored_sha256),
                "compression": artifact.compression,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Upload receipt
// ---------------------------------------------------------------------------

/// The verified outcome of a completed snapshot upload.
#[derive(Debug, Clone, Serialize)]
pub struct UploadReceipt {
    pub snapshot_id: String,
    pub session_id: String,
    pub snapshot_fingerprint: String,
    pub manifest_sha256: String,
    pub upload_id: String,
    pub capture_id: String,
    pub artifact_count: u32,
    pub total_original_bytes: u64,
    pub total_stored_bytes: u64,
    /// The bytes actually transferred for this upload (0 when every artifact deduplicated).
    pub upload_transfer_bytes: u64,
}

// ---------------------------------------------------------------------------
// The Patwari HTTP client
// ---------------------------------------------------------------------------

/// A negotiated upload's resumable state, shared by the create and status responses.
struct UploadSession {
    upload_id: String,
    capture_id: String,
    chunk_size_bytes: u64,
    artifacts: Vec<ArtifactStatus>,
}

/// Per-artifact chunk status: which chunk indexes Patwari still needs. The entry is identified by
/// its canonical `logical_path`; `artifact_index` is the server's position in its canonicalized
/// (path-sorted) artifact list and is only ever used to address the chunk PUT route, never to
/// index into the locally assembled artifact list (issue #33).
struct ArtifactStatus {
    logical_path: String,
    artifact_index: u32,
    missing_chunk_indexes: Vec<u64>,
}

/// The result of a resume-status probe.
enum StatusOutcome {
    Resumable(UploadSession),
    AlreadyCompleted,
    Gone,
}

/// A synchronous Patwari archive-upload client bound to one server and client identity.
pub struct PatwariClient {
    endpoint: http::HttpEndpoint,
    client_id: String,
    timeout: Duration,
}

impl PatwariClient {
    /// Connects a client for `endpoint` (`http://` or `https://` `host[:port]`, ADR 0013)
    /// uploading under `client_id`.
    pub fn connect(endpoint: &str, client_id: &str) -> Result<Self, PatwariError> {
        let endpoint = http::parse_http_endpoint(endpoint).map_err(from_http)?;
        Ok(Self {
            endpoint,
            client_id: client_id.to_owned(),
            timeout: REQUEST_TIMEOUT,
        })
    }

    /// Idempotently registers this client with Patwari (`PUT /clients/{client_id}`), recording an
    /// optional hostname, display name, and metadata.
    pub fn register_client(
        &self,
        hostname: Option<&str>,
        display_name: Option<&str>,
        metadata: &BTreeMap<String, String>,
    ) -> Result<(), PatwariError> {
        let body = json!({
            "hostname": hostname,
            "display_name": display_name,
            "metadata": metadata,
        });
        let payload = serde_json::to_vec(&body).map_err(|error| protocol(&error))?;
        let path = format!("{API_BASE}/clients/{}", http::encode_path(&self.client_id));
        let response = self.send_json("PUT", &path, Some(&payload))?;
        match response.status {
            200 | 201 => Ok(()),
            status => Err(self.server_error(status, &response.body)),
        }
    }

    /// Uploads one snapshot: negotiate an upload for `capture_id` + `manifest`, transfer every
    /// missing chunk of every artifact, and finalize into a verified receipt.
    ///
    /// `resume_upload_id` is a previously created server upload id to try to resume via a status GET
    /// before creating a new one; `on_upload_id` is invoked with the negotiated upload id so the
    /// caller can persist it for a future resume. The flow is idempotent end to end: re-`PUT`ting an
    /// already-accepted chunk and re-creating with the same capture are no-ops server-side.
    pub fn upload_snapshot(
        &self,
        capture_id: &str,
        manifest: &Value,
        artifacts: &[PreparedArtifact],
        resume_upload_id: Option<&str>,
        mut on_upload_id: impl FnMut(&str),
    ) -> Result<UploadReceipt, PatwariError> {
        // Prefer resuming a known upload via GET; fall back to creating one.
        let session = match resume_upload_id {
            Some(upload_id) => match self.upload_status(upload_id)? {
                StatusOutcome::Resumable(session) => Some(session),
                StatusOutcome::AlreadyCompleted => {
                    return self.complete_upload(upload_id, capture_id);
                }
                StatusOutcome::Gone => None,
            },
            None => None,
        };
        let session = match session {
            Some(session) => session,
            None => self.create_upload(capture_id, manifest)?,
        };
        on_upload_id(&session.upload_id);

        self.upload_missing_chunks(&session, artifacts)?;
        // Resume-verify via a status GET before completing: upload anything still missing (e.g. a
        // chunk lost to an interruption between the create response and here).
        if let StatusOutcome::Resumable(refreshed) = self.upload_status(&session.upload_id)? {
            self.upload_missing_chunks(&refreshed, artifacts)?;
        }
        self.complete_upload(&session.upload_id, &session.capture_id)
    }

    /// `POST /uploads` — negotiates a new (or duplicate) upload for one capture and manifest.
    fn create_upload(
        &self,
        capture_id: &str,
        manifest: &Value,
    ) -> Result<UploadSession, PatwariError> {
        let body = json!({
            "client_id": self.client_id,
            "capture_id": capture_id,
            "manifest": manifest,
        });
        let payload = serde_json::to_vec(&body).map_err(|error| protocol(&error))?;
        let response = self.send_json("POST", &format!("{API_BASE}/uploads"), Some(&payload))?;
        match response.status {
            // 201 new, 200 resumed/duplicate capture — both return the current chunk status.
            200 | 201 => parse_upload_session(&response.body),
            409 => Err(self.conflict_error(&response.body)),
            status => Err(self.server_error(status, &response.body)),
        }
    }

    /// `GET /uploads/{upload_id}` — the resumable-upload status document.
    fn upload_status(&self, upload_id: &str) -> Result<StatusOutcome, PatwariError> {
        let path = format!("{API_BASE}/uploads/{}", http::encode_path(upload_id));
        let response = self.send_json("GET", &path, None)?;
        match response.status {
            200 => {
                let value = parse_json(&response.body)?;
                match value.get("status").and_then(Value::as_str) {
                    Some("completed") => Ok(StatusOutcome::AlreadyCompleted),
                    Some("abandoned") | Some("expired") => Ok(StatusOutcome::Gone),
                    _ => Ok(StatusOutcome::Resumable(upload_session_from_value(&value)?)),
                }
            }
            404 => Ok(StatusOutcome::Gone),
            status => Err(self.server_error(status, &response.body)),
        }
    }

    /// Uploads every currently-missing chunk of every artifact in the negotiated session.
    ///
    /// Server entries are matched to the locally prepared artifacts by canonical `logical_path`,
    /// never by position: Patwari orders its `artifacts[]` canonically (sorted by logical path),
    /// which need not agree with the local assembly order (issue #33). An unknown, locally
    /// missing, or repeated path is a protocol error naming the path.
    fn upload_missing_chunks(
        &self,
        session: &UploadSession,
        artifacts: &[PreparedArtifact],
    ) -> Result<(), PatwariError> {
        let by_path = index_artifacts_by_path(artifacts)?;
        let mut seen_paths = BTreeSet::new();
        for status in &session.artifacts {
            if !seen_paths.insert(status.logical_path.as_str()) {
                return Err(PatwariError::Protocol(format!(
                    "server repeated artifact logical path {}",
                    status.logical_path
                )));
            }
            let Some(artifact) = by_path.get(status.logical_path.as_str()) else {
                return Err(PatwariError::Protocol(format!(
                    "server referenced artifact logical path {} that is not in this snapshot",
                    status.logical_path
                )));
            };
            // The server supplies the missing chunk indexes; validate each against the locally
            // computed chunk count (mirroring Patwari's own `chunk_count`) before slicing, so a
            // malformed or out-of-range index fails as a protocol error naming it rather than
            // reaching a slice.
            let count = chunk_count(artifact.stored_bytes.len(), session.chunk_size_bytes);
            for &chunk_index in &status.missing_chunk_indexes {
                if chunk_index >= count {
                    return Err(PatwariError::Protocol(format!(
                        "server requested chunk index {chunk_index} outside the {count}-chunk range of artifact {}",
                        status.logical_path
                    )));
                }
                self.put_chunk(
                    &session.upload_id,
                    status.artifact_index,
                    chunk_index,
                    chunk_bytes(
                        &artifact.stored_bytes,
                        session.chunk_size_bytes,
                        chunk_index,
                    )?,
                )?;
            }
        }
        Ok(())
    }

    /// `PUT /uploads/{id}/artifacts/{ai}/chunks/{ci}` — one raw stored-bytes chunk.
    fn put_chunk(
        &self,
        upload_id: &str,
        artifact_index: u32,
        chunk_index: u64,
        chunk: &[u8],
    ) -> Result<(), PatwariError> {
        let path = format!(
            "{API_BASE}/uploads/{}/artifacts/{artifact_index}/chunks/{chunk_index}",
            http::encode_path(upload_id),
        );
        let sha256 = prefixed_digest(&sha256_hex(chunk));
        let length = chunk.len().to_string();
        let headers = [
            Header {
                name: "Content-Type",
                value: "application/octet-stream",
            },
            Header {
                name: CHUNK_SHA256_HEADER,
                value: &sha256,
            },
            Header {
                name: CHUNK_LENGTH_HEADER,
                value: &length,
            },
        ];
        let response = http::send(
            &self.endpoint,
            self.timeout,
            &http::HttpRequest {
                method: "PUT",
                path: &path,
                headers: &headers,
                body: Some(chunk),
            },
        )
        .map_err(from_http)?;
        match response.status {
            // 204 accepted / idempotent re-PUT; 200 for compatibility.
            200 | 204 => Ok(()),
            409 => Err(self.conflict_error(&response.body)),
            status => Err(self.server_error(status, &response.body)),
        }
    }

    /// `POST /uploads/{id}/complete` — verifies, dedups, and returns the receipt.
    fn complete_upload(
        &self,
        upload_id: &str,
        capture_id: &str,
    ) -> Result<UploadReceipt, PatwariError> {
        let path = format!(
            "{API_BASE}/uploads/{}/complete",
            http::encode_path(upload_id)
        );
        let response = self.send_json_with_timeout("POST", &path, None, COMPLETION_TIMEOUT)?;
        match response.status {
            200 | 201 => parse_completion(&response.body, upload_id, capture_id),
            409 => Err(self.conflict_error(&response.body)),
            status => Err(self.server_error(status, &response.body)),
        }
    }

    /// Sends a JSON request (or a bodyless GET/POST) with `Accept: application/json`.
    fn send_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<http::HttpResponse, PatwariError> {
        self.send_json_with_timeout(method, path, body, self.timeout)
    }

    fn send_json_with_timeout(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<http::HttpResponse, PatwariError> {
        let mut headers = vec![Header {
            name: "Accept",
            value: "application/json",
        }];
        if body.is_some() {
            headers.push(Header {
                name: "Content-Type",
                value: "application/json",
            });
        }
        http::send(
            &self.endpoint,
            timeout,
            &http::HttpRequest {
                method,
                path,
                headers: &headers,
                body,
            },
        )
        .map_err(from_http)
    }

    /// Classifies a 409 by Patwari's stable error code, distinguishing the two idempotency faults.
    fn conflict_error(&self, body: &[u8]) -> PatwariError {
        match error_code(body).as_deref() {
            Some("capture_id_conflict") => PatwariError::CaptureConflict,
            Some("chunk_conflict") => PatwariError::ChunkConflict,
            _ => self.server_error(409, body),
        }
    }

    fn server_error(&self, status: u16, body: &[u8]) -> PatwariError {
        PatwariError::Server {
            status,
            code: error_code(body).unwrap_or_else(|| "unknown".to_owned()),
        }
    }
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

fn parse_json(body: &[u8]) -> Result<Value, PatwariError> {
    serde_json::from_slice(body).map_err(|error| protocol(&error))
}

fn parse_upload_session(body: &[u8]) -> Result<UploadSession, PatwariError> {
    upload_session_from_value(&parse_json(body)?)
}

fn upload_session_from_value(value: &Value) -> Result<UploadSession, PatwariError> {
    let upload_id = required_str(value, "upload_id")?;
    let capture_id = required_str(value, "capture_id")?;
    let chunk_size_bytes = value
        .get("chunk_size_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(|| PatwariError::Protocol("response missing chunk_size_bytes".to_owned()))?;
    let artifacts = value
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| PatwariError::Protocol("response missing artifacts".to_owned()))?
        .iter()
        .map(|artifact| {
            // The path is the artifact's identity for chunk routing (issue #33); the index only
            // addresses the PUT route. Both are required.
            let logical_path = artifact
                .get("logical_path")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    PatwariError::Protocol("artifact missing logical_path".to_owned())
                })?;
            let artifact_index = artifact
                .get("artifact_index")
                .and_then(Value::as_u64)
                .ok_or_else(|| PatwariError::Protocol("artifact missing index".to_owned()))?
                as u32;
            let missing_chunk_indexes = artifact
                .get("missing_chunk_indexes")
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            Ok(ArtifactStatus {
                logical_path,
                artifact_index,
                missing_chunk_indexes,
            })
        })
        .collect::<Result<Vec<_>, PatwariError>>()?;
    Ok(UploadSession {
        upload_id,
        capture_id,
        chunk_size_bytes,
        artifacts,
    })
}

fn parse_completion(
    body: &[u8],
    upload_id: &str,
    capture_id: &str,
) -> Result<UploadReceipt, PatwariError> {
    let value = parse_json(body)?;
    let receipt = value
        .get("receipt")
        .ok_or_else(|| PatwariError::Protocol("completion missing receipt".to_owned()))?;
    let transfer = value.get("transfer");
    Ok(UploadReceipt {
        snapshot_id: required_str(receipt, "snapshot_id")?,
        session_id: required_str(receipt, "session_id")?,
        snapshot_fingerprint: required_str(receipt, "snapshot_fingerprint")?,
        manifest_sha256: required_str(receipt, "manifest_sha256")?,
        upload_id: transfer
            .and_then(|transfer| transfer.get("upload_id"))
            .and_then(Value::as_str)
            .unwrap_or(upload_id)
            .to_owned(),
        capture_id: transfer
            .and_then(|transfer| transfer.get("capture_id"))
            .and_then(Value::as_str)
            .unwrap_or(capture_id)
            .to_owned(),
        artifact_count: receipt
            .get("artifact_count")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        total_original_bytes: receipt
            .get("total_original_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_stored_bytes: receipt
            .get("total_stored_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        upload_transfer_bytes: transfer
            .and_then(|transfer| transfer.get("upload_transfer_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn required_str(value: &Value, key: &str) -> Result<String, PatwariError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| PatwariError::Protocol(format!("response missing {key}")))
}

/// Reads Patwari's stable `error.code` from an error response body, if present.
fn error_code(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("error")?
        .get("code")?
        .as_str()
        .map(ToOwned::to_owned)
}

fn protocol(error: &impl std::fmt::Display) -> PatwariError {
    PatwariError::Protocol(error.to_string())
}

// ---------------------------------------------------------------------------
// Small pure helpers
// ---------------------------------------------------------------------------

/// Indexes the locally prepared artifacts by logical path for matching server status entries
/// (issue #33). A repeated local path is a protocol error naming the path: routing chunks by path
/// would be ambiguous, and Patwari rejects such a manifest anyway.
fn index_artifacts_by_path(
    artifacts: &[PreparedArtifact],
) -> Result<BTreeMap<&str, &PreparedArtifact>, PatwariError> {
    let mut by_path = BTreeMap::new();
    for artifact in artifacts {
        if by_path
            .insert(artifact.logical_path.as_str(), artifact)
            .is_some()
        {
            return Err(PatwariError::Protocol(format!(
                "snapshot repeats artifact logical path {}",
                artifact.logical_path
            )));
        }
    }
    Ok(by_path)
}

/// The number of `chunk_size`-byte chunks the stored artifact is split into, mirroring Patwari's
/// server-side `chunk_count` exactly (ingestion.rs): an empty artifact has zero chunks, otherwise
/// `ceil(stored_len / chunk_size)`. Used to reject a server-supplied missing index that falls
/// outside the locally computed range before it can reach [`chunk_bytes`].
fn chunk_count(stored_len: usize, chunk_size: u64) -> u64 {
    if chunk_size == 0 || stored_len == 0 {
        return 0;
    }
    // ceil(stored_len / chunk_size) computed as (stored_len - 1) / chunk_size + 1, matching the
    // server. `stored_len` and `chunk_size` are both non-zero here, so this never overflows.
    (stored_len as u64 - 1) / chunk_size + 1
}

/// The chunk of `stored_bytes` at `chunk_index` for a `chunk_size` layout (the last chunk is
/// smaller). Chunking is over the stored (compressed) representation, matching Patwari. Total: an
/// index past the artifact returns a protocol error rather than slicing out of bounds. The sole
/// valid chunk of an empty artifact is the empty chunk at index 0.
fn chunk_bytes(
    stored_bytes: &[u8],
    chunk_size: u64,
    chunk_index: u64,
) -> Result<&[u8], PatwariError> {
    let chunk_size = usize::try_from(chunk_size)
        .map_err(|_| PatwariError::Protocol("chunk size exceeds addressable memory".to_owned()))?;
    if chunk_size == 0 {
        return Err(PatwariError::Protocol(
            "server negotiated a zero chunk size".to_owned(),
        ));
    }
    let start = usize::try_from(chunk_index)
        .ok()
        .and_then(|index| index.checked_mul(chunk_size))
        .ok_or_else(|| PatwariError::Protocol("chunk index is out of range".to_owned()))?;
    // Every index past the artifact is out of range. Index 0 of an empty artifact is the exception:
    // its start (0) is not strictly past the (zero) length, so the guard below admits it and the
    // slice `[0..0]` yields the empty chunk.
    if start > stored_bytes.len() || (start == stored_bytes.len() && !stored_bytes.is_empty()) {
        return Err(PatwariError::Protocol(
            "chunk index is past the artifact".to_owned(),
        ));
    }
    let end = start.saturating_add(chunk_size).min(stored_bytes.len());
    Ok(&stored_bytes[start..end])
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn prefixed_digest(hex: &str) -> String {
    format!("sha256:{hex}")
}

/// An RFC3339 UTC timestamp for a fresh capture's `captured_at`. Fixed at mint time and persisted,
/// so it never drifts across retries of the same attempt.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is representable"))
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// Capture-machine provenance (issue #77)
// ---------------------------------------------------------------------------
//
// Two keys ride the opaque `source_metadata` map so they need no Patwari change: the server returns
// the map verbatim through the manifest qanungo already parses. Both are absence-tolerant by
// construction — the map is opaque and readers ignore keys they do not know — so a capture from an
// older munshi, or one whose machine could not answer, simply lacks them and every consumer keeps
// working. Consumers are qanungo's activity heatmap (`utc_offset`, to place a session in the
// operator's own day rather than in UTC) and its per-device scope (`hostname`).
//
// Their value accrues only from ship time: a session captured without them stays heatmap-blind
// forever, since nothing outside the capture machine can reconstruct what its clock read.

/// The capture machine's hostname, sanitized to the same slug memory-sync already persists as the
/// machine label. Deliberately reuses `memory_sync`'s pair rather than re-deriving: a machine must
/// present one spelling of itself across the whole system, so qanungo can scope by device and have
/// it line up with everything else munshi labels. `None` when the OS declines to answer or the
/// hostname sanitizes away to nothing — better an absent key than an empty one.
fn capture_hostname(machine_label: Option<&str>) -> Option<String> {
    let configured = machine_label
        .map(str::to_owned)
        .filter(|label| !label.is_empty());
    configured.or_else(|| {
        let sanitized =
            crate::memory_sync::sanitize_machine_label(&crate::memory_sync::hostname_string());
        (!sanitized.is_empty()).then_some(sanitized)
    })
}

/// The capture machine's UTC offset at `captured_at`, as RFC3339 spells it (`+05:30`, `-08:00`,
/// `+00:00`).
///
/// Computed at that persisted instant rather than at "now", honoring `CaptureContext`'s contract
/// that a reused `capture_id` re-serializes to the same canonical manifest: a retry that crossed a
/// DST boundary would otherwise report a different offset than the capture it is retrying. The
/// determinism is in `(record, captured_at)`, not in the ambient environment — a zone-identity
/// change (a machine that travels) or a hostname change between attempts still alters the manifest.
/// That window is narrow by construction: the normal retry path resumes the persisted upload id and
/// never re-sends the manifest, so surfacing it as a `capture_id_conflict` also takes a lost or
/// expired upload.
///
/// Capture-time offset stands in for the whole session by consumer decision: the heatmap is about
/// habits, so a session that spans a DST change or a flight is close enough. We record honestly what
/// the clock said and interpret no further.
///
/// `None` rather than a malformed value whenever the platform hands back something that will not
/// spell as `[+-]HH:MM` — a zone at sub-minute precision (the pre-standardization LMT entries still
/// in the tz database) or an hour count too large to render in two digits. A consumer parsing this
/// key may assume the shape or assume nothing.
fn capture_utc_offset(captured_at: &str) -> Option<String> {
    let instant = DateTime::parse_from_rfc3339(captured_at).ok()?;
    format_utc_offset(local_gmtoff_seconds(instant.timestamp())?)
}

/// Seconds east of UTC that the machine's local zone applied at `unix_seconds`, via the platform's
/// own tz database. `None` if the C library rejects the instant.
fn local_gmtoff_seconds(unix_seconds: i64) -> Option<i64> {
    let time = unix_seconds as libc::time_t;
    // SAFETY: `localtime_r` fills the caller-owned `tm` (a plain integer struct, sound to zero) and
    // returns null on failure. It is the reentrant variant precisely so it holds no shared state.
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::localtime_r(&time, &mut parts) };
    if result.is_null() {
        return None;
    }
    Some(parts.tm_gmtoff as i64)
}

/// Renders seconds east of UTC as `[+-]HH:MM`, or `None` when that shape cannot represent them.
fn format_utc_offset(gmtoff_seconds: i64) -> Option<String> {
    // A zone offset at sub-minute precision has no RFC3339 spelling, and one past 23 hours has no
    // RFC3339 spelling either (offsets cap at +/-23:59). Both are refused rather than truncated: a
    // consumer's parse must not have to distinguish a real offset from a rounded one.
    if gmtoff_seconds % 60 != 0 {
        return None;
    }
    let sign = if gmtoff_seconds < 0 { '-' } else { '+' };
    let magnitude = gmtoff_seconds.unsigned_abs();
    let hours = magnitude / 3600;
    let minutes = (magnitude % 3600) / 60;
    if hours > 23 {
        return None;
    }
    Some(format!("{sign}{hours:02}:{minutes:02}"))
}

// ---------------------------------------------------------------------------
// Instruction-file provenance (issue #77)
// ---------------------------------------------------------------------------
//
// Two more keys ride the same opaque map: `claude_md` and `agents_md`, the sha256 of the project
// root's `CLAUDE.md` / `AGENTS.md`. Only the digest travels — the file's content is never uploaded,
// which is what makes recording it on every capture acceptable at all. Downstream, qanungo's
// instructions-doctor anchors "an instruction edit landed" on the value changing between two
// captures, which is a question nothing in the archive can answer today.
//
// Like the machine keys, these accrue value only from ship time: a capture taken before this shipped
// carries no record of what the instructions said at that moment, and nothing can reconstruct it.
//
// Snapshot-fingerprint consequence, checked rather than assumed: none, on both sides. Patwari's
// `snapshot_fingerprint` projects the capture through a `StableCapture` naming five fields —
// project, repository, branch, source_agent_version, artifact_set_version — so `source_metadata` is
// structurally unreachable from it, and a server test mutates the map and pins the fingerprint
// unchanged. On this side the map feeds `build_manifest` and nothing else: no munshi-transcript file
// is touched, so no rendered content and no content address moves. A re-capture whose only change is
// an instruction digest therefore adds a capture row and coalesces, rather than minting a snapshot.

/// Whether this upload attempt may touch the session's origin directory (issue #61).
///
/// The upload path runs both from user-attributed processes and from a scheduler-descended worker,
/// and the latter "must not touch the origin directory — not even to check that it exists": a stat
/// of a TCC-protected root (`~/Documents` and friends, which real sessions live under) raises the
/// permission prompt the background context exists to avoid. Callers therefore state which they
/// are, rather than the capture code guessing from ambient state.
///
/// This is the same distinction [`crate::hooks::WorkerContext`] draws for project inspection, kept
/// as a separate type because it travels a different path and answers a narrower question: not
/// "may I resolve an identity" but "may I read a byte off the origin disk at all".
///
/// Every public upload entry point takes one, with no default. That is deliberate and was arrived
/// at the hard way: `retry` and `backfill` read like operator commands, and the first cut let them
/// hardcode `Allowed` on that reading — but `munshi tick` calls `retry` on every scheduler pass to
/// drain failed rows, so a Patwari outage (the tick's *designed* steady state) would have walked
/// the scheduler straight into a TCC-protected origin. A caller-set invariant nothing enforces is
/// not an invariant, so the type demands the answer instead of assuming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginAccess {
    /// Descended from a user-attributed process: the origin directory may be read.
    Allowed,
    /// Descended from a platform scheduler: no filesystem contact with the origin directory, not
    /// even an existence check. Instruction-file provenance is omitted entirely.
    Withheld,
}

/// The instruction files whose digests a capture records, paired with the `source_metadata` key
/// each is reported under. Filenames are exact and case-sensitive, matching what the harnesses
/// themselves read.
const INSTRUCTION_FILES: [(&str, &str); 2] =
    [("CLAUDE.md", "claude_md"), ("AGENTS.md", "agents_md")];

/// The value recording that the root was readable and the instruction file provably was not there.
/// Distinct from an omitted key, which records only that munshi did not look.
const INSTRUCTION_ABSENT: &str = "absent";

/// The largest instruction file munshi will read to hash. An instruction file is prose a human
/// maintains; a megabyte of it is not one, and reading an arbitrarily large file on every capture
/// is a cost the provenance does not justify. Over the cap the key is *omitted*, never `absent` —
/// the file exists, we declined to look.
const MAX_INSTRUCTION_BYTES: u64 = 1024 * 1024;

/// Instruction-file provenance for this capture: `claude_md` / `agents_md` for the project root the
/// session's identity is derived from, each either a digest, `absent`, or missing entirely.
///
/// Re-hashed on every attempt, deliberately. This matches `capture_hostname` — ambient machine state
/// read at the moment of the attempt — rather than `capture_utc_offset`, which is a pure function of
/// the persisted `captured_at` and so is fixed for the life of a capture id. There is no
/// `captured_at`-equivalent to resolve an instruction file against: the file has exactly one state,
/// the one it has now, and munshi keeps no history of it. So the honest bound is the one `a31b218`
/// states for the hostname: determinism is in `(record, captured_at)` plus the ambient environment,
/// not in `(record, captured_at)` alone. An instruction edit landing between two attempts of one
/// capture id alters the manifest, and that window is narrow by construction — the normal retry path
/// resumes the persisted upload id and never re-sends the manifest, so surfacing it as a
/// `capture_id_conflict` also takes a lost or expired upload. Freezing the first attempt's digest
/// instead would need persisted state and would buy nothing the consumer wants: the digest is
/// evidence about the working tree, and the freshest reading is the truest one.
///
/// Returns an empty map, touching no filesystem at all, when origin access is withheld.
fn capture_instruction_provenance(
    record: &SessionRecord,
    origin_access: OriginAccess,
) -> BTreeMap<String, String> {
    let mut provenance = BTreeMap::new();
    // Issue #61: a scheduler-descended worker gets no further than this line — not a stat, not an
    // existence check. Every key is simply absent from the capture.
    if origin_access == OriginAccess::Withheld {
        return provenance;
    }
    // `origin_cwd` is the session's recorded working directory, often a subdirectory of the
    // project; `project.identity` is not a path and can never stand in for one. A session that
    // recorded no origin (codex records none) yields no keys, per the contract.
    let Some(origin_cwd) = record.origin_cwd.as_deref() else {
        return provenance;
    };
    let Some(root) = crate::project::project_root(origin_cwd) else {
        return provenance;
    };
    for (filename, key) in INSTRUCTION_FILES {
        if let Some(state) = instruction_file_state(&root.join(filename)) {
            provenance.insert(key.to_owned(), state);
        }
    }
    provenance
}

/// What the capture should report for one instruction file: `Some(<64 lowercase hex>)` for a
/// readable regular file within the cap, `Some("absent")` when it provably is not there, and `None`
/// — omit the key — for every other outcome.
///
/// Symlinks are omitted rather than followed on purpose. A digest of a file living outside the
/// project is worse than no provenance at all: the consumer would read it as a statement about this
/// project's instructions and be wrong, with nothing in the record to reveal it. Directories and
/// devices are refused for the same reason. Only `NotFound` earns `absent`; a permission error means
/// munshi could not look, which is exactly what an omitted key says.
fn instruction_file_state(path: &Path) -> Option<String> {
    // `symlink_metadata`, not `metadata`: the question is what is *at* this path, not what it
    // eventually points to.
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(INSTRUCTION_ABSENT.to_owned());
        }
        Err(_) => return None,
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_INSTRUCTION_BYTES {
        return None;
    }
    hash_bounded_file(path)
}

/// Streams a file into a sha256, refusing anything over [`MAX_INSTRUCTION_BYTES`]. `None` on any
/// read failure — instruction provenance never errors an upload, it just goes unreported.
///
/// The reader is capped at one byte past the limit so the size is re-checked against what was
/// actually read, not only against the `stat` that preceded it: a file that grew past the cap
/// between the two is refused rather than hashed in part. Bytes are consumed in a fixed buffer, so
/// no file size can make this allocate.
fn hash_bounded_file(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file).take(MAX_INSTRUCTION_BYTES + 1);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut read_bytes: u64 = 0;
    loop {
        let filled = reader.read(&mut buffer).ok()?;
        if filled == 0 {
            break;
        }
        read_bytes += filled as u64;
        if read_bytes > MAX_INSTRUCTION_BYTES {
            return None;
        }
        hasher.update(&buffer[..filled]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// Generates a version-4 UUID string from operating-system randomness, used for the persistent
/// client identity and per-attempt capture ids (both must parse as UUIDs server-side).
fn new_uuid() -> String {
    let mut bytes = [0_u8; 16];
    if !fill_random(&mut bytes) {
        // Fall back to a hash of high-resolution time and pid — still 16 well-mixed bytes.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let seed = format!("{nanos}-{}", std::process::id());
        let digest = Sha256::digest(seed.as_bytes());
        bytes.copy_from_slice(&digest[..16]);
    }
    // Set the version (4) and variant (RFC 4122) bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32],
    )
}

/// Fills `buffer` from `/dev/urandom`, returning whether it succeeded.
fn fill_random(buffer: &mut [u8]) -> bool {
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(buffer))
        .is_ok()
}

// ---------------------------------------------------------------------------
// Orchestration: upload one archived session downstream of local archival
// ---------------------------------------------------------------------------

fn backoff_ms(attempts: u32) -> i64 {
    let shift = attempts.saturating_sub(1).min(16);
    BASE_BACKOFF_MS
        .saturating_mul(1_i64 << shift)
        .min(MAX_BACKOFF_MS)
}

/// The outcome of one session's archive-upload attempt.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case", tag = "result")]
pub enum UploadOutcome {
    Uploaded {
        snapshot_id: String,
        revision: u64,
    },
    AlreadyUploaded {
        snapshot_id: Option<String>,
        revision: u64,
    },
    Skipped {
        reason: String,
    },
    Failed {
        category: String,
        dead_letter: bool,
    },
}

impl UploadOutcome {
    /// A stable, human-readable token for the outcome kind, used in retry-run human output.
    fn as_kind(&self) -> &'static str {
        match self {
            Self::Uploaded { .. } => "uploaded",
            Self::AlreadyUploaded { .. } => "already-uploaded",
            Self::Skipped { .. } => "skipped",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Ensures the persistent client UUID exists in durable configuration, generating and storing one
/// on first use. Because it lives in `config.json` it survives an operational-database rebuild.
pub(crate) fn ensure_client_id(state_directory: &Path) -> Result<String, PatwariError> {
    let config = load_stored_config(state_directory)?;
    if let Some(client_id) = config.archive_upload.client_id.clone() {
        return Ok(client_id);
    }
    let client_id = new_uuid();
    let (_, stored) = update_stored_config(state_directory, |config| {
        // Another process may have won the race under the registration lock; keep theirs.
        if config.archive_upload.client_id.is_none() {
            config.archive_upload.client_id = Some(client_id.clone());
        }
        Ok(config
            .archive_upload
            .client_id
            .clone()
            .unwrap_or(client_id.clone()))
    })?;
    Ok(stored)
}

/// The default machine hostname recorded at client registration, when resolvable.
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.is_empty())
}

/// Reads an archived session's snapshot artifacts from disk and assembles the artifact-set-v1
/// sources (see [`assemble_artifact_sources`]). Both the rendered `summary.md` and the verbatim
/// `transcript.jsonl` are required: every snapshot ADR 0009 archives is self-contained, and the
/// transcript additionally seeds the re-derived extracted outputs.
///
/// An unreadable required artifact — a session with no recorded transcript path (a session
/// reconstructed by `rebuild-state` from its archive Markdown alone, which never learns one), or a
/// transcript the harness has since removed — yields [`PatwariError::SnapshotIncomplete`] rather
/// than a reduced artifact set. Uploading the reduced set is what produced summary-only snapshots
/// (issue #47): the summary stays durable locally and in Notesmith, so refusing the partial
/// snapshot loses nothing and keeps the archive's artifact-set contract intact.
fn collect_artifacts(
    output_directory: &Path,
    record: &SessionRecord,
    max_event_text_bytes: usize,
) -> Result<Vec<ArtifactSource>, PatwariError> {
    let summary = match record.markdown_relative_path.as_ref() {
        Some(relative) => {
            Some(std::fs::read(output_directory.join(relative)).map_err(PatwariError::Io)?)
        }
        None => return Err(PatwariError::SnapshotIncomplete(SUMMARY_LOGICAL_PATH)),
    };
    let transcript = record
        .transcript_path
        .as_ref()
        .and_then(|path| std::fs::read(path).ok());
    if transcript.is_none() {
        return Err(PatwariError::SnapshotIncomplete(TRANSCRIPT_LOGICAL_PATH));
    }
    // Guard the upload against a transcript that changed under it (ADR 0009). The transcript is
    // re-read live here, after the archived revision was normalized; an append between the
    // summarizer's `verify_unchanged` and this read would upload bytes whose sha256 no longer
    // matches the archived revision's `source_hash` (the frontmatter `transcript_sha256` and the
    // claim tickets, all derived from the normalization-time hash) and the re-derived extracted
    // outputs. When the archived revision's source hash is known, fail rather than upload
    // inconsistent bytes; the failure is retryable and a later revision re-archives the grown
    // transcript and converges. `source_hash` is `sha256:<hex>`; a truncated (oversized) transcript
    // never archives (`read_stable_source` rejects it), so a full-file re-read cannot false-positive.
    if let (Some(bytes), Some(expected)) = (
        transcript.as_ref(),
        record
            .previous_source
            .as_ref()
            .map(|previous| previous.source_hash.as_str()),
    ) && prefixed_digest(&sha256_hex(bytes)) != expected
    {
        return Err(PatwariError::TranscriptChanged);
    }
    Ok(assemble_artifact_sources(
        summary,
        transcript,
        record.source,
        max_event_text_bytes,
        read_staged_sidecars(output_directory, record.markdown_relative_path.as_deref()),
    ))
}

/// [`collect_artifacts`], with one repair attempt when the transcript is what is missing: the path
/// is re-derived from the session's ID through its source's own version-pinned discovery, persisted
/// on the session row, and the artifact set assembled again (issue #53).
///
/// The sessions issue #47 correctly refuses to upload as summary-only snapshots are overwhelmingly
/// sessions whose transcript is *on disk* and whose row simply forgot where — `rebuild-state`
/// reconstructs a session from its archive Markdown, which never records a transcript path. Skipping
/// those honestly is right but not sufficient: nothing would ever re-teach the row its path, so the
/// session could never upload in full. Derivation runs at the moment of the skip, so the very same
/// upload proceeds with the complete artifact set, and the recovered path is written back so every
/// later read finds it without re-deriving.
///
/// The repair is narrow on purpose. It applies only to a missing transcript — a missing `summary.md`
/// is a local-archive question, not a discovery one — and only when derivation produces a path that
/// passes its source's safety validation ([`derive_transcript_path`]). A session whose transcript is
/// genuinely gone, whose harness home is unregistered, or whose source (Codex) has no safe
/// session-ID lookup falls straight through to the unchanged issue #47 skip.
fn collect_recoverable_artifacts(
    state: &mut StateStore,
    output_directory: &Path,
    record: &SessionRecord,
    homes: &SourceHomes,
    max_event_text_bytes: usize,
) -> Result<Vec<ArtifactSource>, PatwariError> {
    let first = collect_artifacts(output_directory, record, max_event_text_bytes);
    if !matches!(
        first,
        Err(PatwariError::SnapshotIncomplete(TRANSCRIPT_LOGICAL_PATH))
    ) {
        return first;
    }
    let Some(derived) = derive_transcript_path(record.source, &record.session_id, homes) else {
        return first;
    };
    state.record_derived_transcript_path(&record.session_id, &derived)?;
    let mut recovered = record.clone();
    recovered.transcript_path = Some(derived);
    collect_artifacts(output_directory, &recovered, max_event_text_bytes)
}

/// Assembles the ordered snapshot artifact set (ADR 0009/0010, v2 per issue #23) from
/// already-read bytes: `summary.md` (this revision's rendered summary), `transcript.jsonl` (the
/// verbatim source bytes), every re-derived `outputs/<sha256>` extracted output, and any staged
/// `sidecar/<relative-path>` files.
///
/// Extracted outputs are re-derived from the exact transcript bytes this snapshot uploads
/// (`extract_outputs`, ADR 0010 option a), so the `outputs/<sha256>` set is always consistent with
/// `transcript.jsonl` and, for a reused capture id, byte-identical across retries. The list is
/// returned in Patwari's canonical order — ascending by logical path (issue #33) — so the locally
/// built manifest lists artifacts exactly as the server's canonicalized `artifacts[]` does; the
/// ordering is fully deterministic, the canonical manifest is stable, and identical content dedups
/// to one artifact and one blob server-side. Chunk routing during upload matches artifacts by
/// logical path and never relies on this order agreement. Pure and I/O-free so the exact set the
/// upload path builds is unit-testable.
///
/// Both inputs are optional only so the assembly stays pure over whatever a caller has read; the
/// upload path never omits either. [`collect_artifacts`] is the single I/O boundary that supplies
/// them, and it refuses to assemble anything but the complete set (issue #47).
pub fn assemble_artifact_sources(
    summary_md: Option<Vec<u8>>,
    transcript_jsonl: Option<Vec<u8>>,
    source: SourceKind,
    max_event_text_bytes: usize,
    sidecars: Vec<SidecarFile>,
) -> Vec<ArtifactSource> {
    let mut sources = Vec::new();
    if let Some(summary) = summary_md {
        sources.push(ArtifactSource {
            logical_path: "summary.md".to_owned(),
            media_type: Some("text/markdown".to_owned()),
            bytes: summary,
        });
    }
    for sidecar in sidecars {
        sources.push(ArtifactSource {
            logical_path: format!("{SIDECAR_LOGICAL_PREFIX}{}", sidecar.relative_path),
            media_type: Some(sidecar_media_type(&sidecar.relative_path).to_owned()),
            bytes: sidecar.bytes,
        });
    }
    if let Some(transcript) = transcript_jsonl {
        let extracted = crate::source::extract_outputs(&transcript, source, max_event_text_bytes);
        sources.push(ArtifactSource {
            logical_path: "transcript.jsonl".to_owned(),
            media_type: Some("application/jsonl".to_owned()),
            bytes: transcript,
        });
        for output in extracted {
            sources.push(ArtifactSource {
                logical_path: format!("outputs/{}", output.sha256),
                media_type: output.media_type,
                bytes: output.content,
            });
        }
    }
    // Canonicalize: Patwari sorts `artifacts[]` by logical path (`outputs/…` before `sidecar/…`
    // before `summary.md` before `transcript.jsonl`). Logical paths are unique here (fixed roles,
    // content-addressed outputs, and distinct staged relative paths), so the sort is a total order
    // and stable across retries.
    sources.sort_by(|a, b| a.logical_path.cmp(&b.logical_path));
    sources
}

/// Media type of a staged sidecar artifact, keyed by its allowlisted extension.
fn sidecar_media_type(relative_path: &str) -> &'static str {
    match relative_path
        .rsplit_once('.')
        .map(|(_, extension)| extension)
    {
        Some("md") => "text/markdown",
        Some("json") => "application/json",
        Some("yaml") => "application/yaml",
        _ => "text/plain; charset=utf-8",
    }
}

/// Reads the staged sidecar set for `record`'s current archive Markdown, if any (issue #23).
///
/// The staged directory — written by the archive step from the live session-state allowlist — is
/// the only sidecar source uploads read, so a reused capture id re-serializes the same manifest
/// even when the live files have since mutated. The read is defensively bounded with the same caps
/// as capture and refuses symlinked entries; an unreadable or absent directory yields an empty
/// set, never an error, because sidecars are optional by contract.
fn read_staged_sidecars(
    output_directory: &Path,
    markdown_relative: Option<&Path>,
) -> Vec<SidecarFile> {
    let Some(relative) = markdown_relative else {
        return Vec::new();
    };
    let directory = crate::render::sidecar_directory(output_directory, relative);
    let mut relative_paths = Vec::new();
    collect_staged_paths(&directory, &directory, 0, &mut relative_paths);
    relative_paths.sort_unstable();
    let mut files = Vec::new();
    let mut total_bytes = 0usize;
    for relative_path in relative_paths {
        if files.len() >= crate::source::SIDECAR_MAX_FILES {
            break;
        }
        let Ok(bytes) = std::fs::read(directory.join(&relative_path)) else {
            continue;
        };
        if bytes.len() > crate::source::SIDECAR_MAX_FILE_BYTES {
            continue;
        }
        let Some(next_total) = total_bytes.checked_add(bytes.len()) else {
            break;
        };
        if next_total > crate::source::SIDECAR_MAX_TOTAL_BYTES {
            continue;
        }
        total_bytes = next_total;
        files.push(SidecarFile {
            relative_path,
            bytes,
        });
    }
    files
}

/// Walks a staged sidecar directory up to two levels deep, collecting forward-slash relative file
/// paths. Symlinked entries and deeper nesting are ignored — staging never writes either, so
/// anything else is not ours to upload.
fn collect_staged_paths(root: &Path, directory: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 1 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_staged_paths(root, &path, depth + 1, out);
        } else if metadata.is_file()
            && let Ok(relative) = path.strip_prefix(root)
            && let Some(relative) = relative.to_str()
        {
            out.push(relative.replace(std::path::MAIN_SEPARATOR, "/"));
        }
    }
}

/// Uploads one freshly archived summary revision to Patwari, invoked by the archive worker
/// downstream of a successful local archive and Notesmith delivery.
///
/// This never mutates the session's archival lifecycle. It returns `Ok(None)` when archive upload
/// is disabled or unconfigured, records network/server failures as a bounded retry, and reuses the
/// persisted capture id (resuming an interrupted upload) or mints a fresh one for a new revision.
///
/// `origin_access` is the worker's own context (issue #61) carried through to capture provenance: a
/// scheduler-descended worker withholds it and the capture records no instruction-file digests.
pub(crate) fn upload_after_archive(
    state: &mut StateStore,
    config: &StoredConfig,
    session_id: &str,
    origin_access: OriginAccess,
) -> Result<Option<UploadOutcome>, PatwariError> {
    if !config.archive_upload.enabled || !config.archive_upload.is_addressable() {
        return Ok(None);
    }
    let Some(record) = state.get_session(session_id)? else {
        return Ok(None);
    };
    let endpoint = config.archive_upload.endpoint.clone().unwrap();
    let client_id = ensure_client_id(Path::new(&config.state_directory))?;
    let output_directory = PathBuf::from(&config.output_directory);
    let outcome = upload_one(
        state,
        &config.archive_upload,
        &client_id,
        &endpoint,
        config.memory_sync.machine_label.as_deref(),
        &output_directory,
        &record,
        &config.harnesses.source_homes(),
        config.limits.max_event_text_bytes,
        origin_access,
    )?;
    Ok(Some(outcome))
}

/// Retries every archive upload whose backoff has elapsed, independent of a new summary revision
/// (ADR 0009, issue #19). Invoked by the recovery sweep (`munshi hook recover`) so a transient
/// Patwari outage recovers without waiting for the session to produce a new revision. Returns
/// `Ok(())` (a no-op) when archive upload is disabled or unconfigured.
///
/// Each attempt takes the session's advisory lock — the same one the archive worker uses — so it
/// never races a worker uploading the same session; a locked session is skipped this pass and
/// re-scanned next time. A per-session failure is recorded as a bounded retry and never affects
/// local archival; a store or lock error aborts the sweep so the caller can record a diagnostic.
///
/// `origin_access` is the recovery sweep's own context (issue #61) carried through to capture
/// provenance: a scheduler-descended sweep withholds it and records no instruction-file digests.
pub(crate) fn retry_pending_uploads(
    state_directory: &Path,
    limit: usize,
    origin_access: OriginAccess,
) -> Result<(), PatwariError> {
    let config = load_stored_config(state_directory)?;
    if !config.archive_upload.enabled || !config.archive_upload.is_addressable() {
        return Ok(());
    }
    let endpoint = config.archive_upload.endpoint.clone().unwrap();
    let client_id = ensure_client_id(Path::new(&config.state_directory))?;
    let output_directory = PathBuf::from(&config.output_directory);
    let eligible = StateStore::open(state_directory)?.eligible_archive_uploads(now_ms(), limit)?;
    for record in eligible {
        // Only rows for the currently configured server are retried; a row recorded for a different
        // endpoint (e.g. after reconfiguration) is left untouched.
        if record.endpoint != endpoint {
            continue;
        }
        let Some(_lock) = try_acquire_session_lock(state_directory, &record.session_id)? else {
            continue;
        };
        let mut state = StateStore::open_for_source(state_directory, record.source)?;
        let Some(session) = state.get_session(&record.session_id)? else {
            continue;
        };
        // `upload_one` re-honors the backoff against the freshly read row and records its own
        // failure, so a row that just became due proceeds while one that is not is skipped quietly.
        upload_one(
            &mut state,
            &config.archive_upload,
            &client_id,
            &endpoint,
            config.memory_sync.machine_label.as_deref(),
            &output_directory,
            &session,
            &config.harnesses.source_homes(),
            config.limits.max_event_text_bytes,
            origin_access,
        )?;
    }
    Ok(())
}

/// Uploads one session's current snapshot to the configured server, recording the result in
/// operational state. Never mutates the session's archival lifecycle.
///
/// `origin_access` states whether this attempt may read the session's origin directory (issue #61);
/// it reaches only the capture's instruction-file provenance, which is omitted when it is
/// [`OriginAccess::Withheld`]. No other part of the upload touches the origin.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_one(
    state: &mut StateStore,
    settings: &crate::registration::StoredArchiveUpload,
    client_id: &str,
    endpoint: &str,
    machine_label: Option<&str>,
    output_directory: &Path,
    record: &SessionRecord,
    homes: &SourceHomes,
    max_event_text_bytes: usize,
    origin_access: OriginAccess,
) -> Result<UploadOutcome, PatwariError> {
    if record.current_revision == 0 {
        return Ok(UploadOutcome::Skipped {
            reason: "not-archived".to_owned(),
        });
    }
    let existing = match state.ensure_archive_upload_target(&record.session_id, endpoint)? {
        Some(existing) => existing,
        None => {
            return Ok(UploadOutcome::Skipped {
                reason: "session-unknown".to_owned(),
            });
        }
    };
    if matches!(
        existing.upload_state.as_str(),
        "rearchive-pending" | "rearchive-failed"
    ) {
        return Ok(UploadOutcome::Skipped {
            reason: "rearchive-parked".to_owned(),
        });
    }
    if existing.upload_state == "dead-letter" {
        return Ok(UploadOutcome::Skipped {
            reason: "dead-letter".to_owned(),
        });
    }
    // Honor the recorded backoff: a failed row whose next attempt is still in the future is not yet
    // due. Skip quietly (not an error) so the recovery driver and the retry CLI can scan broadly and
    // let this row's own schedule gate it. A forced retry clears `next_attempt_at_ms` first, so it
    // never lands here.
    if existing.upload_state == "failed"
        && existing
            .next_attempt_at_ms
            .is_some_and(|next| next > now_ms())
    {
        return Ok(UploadOutcome::Skipped {
            reason: "retry-not-due".to_owned(),
        });
    }
    // Already uploaded this exact revision and markdown as a self-contained snapshot: idempotent
    // no-op that never contacts the server. Matching the revision and summary hash is not enough
    // (issue #73) — a cursor-only re-render (hooks.rs `cursor_only`) rewrites the markdown at the
    // same revision and summary, so the markdown hash must match too or the archive's newest snapshot
    // silently lags the local markdown and `restore` refuses the session. A recorded snapshot that is
    // not known self-contained (issue #47), or a row that predates markdown-hash recording (whose
    // `uploaded_markdown_hash` is `None`), falls through and re-uploads the complete set.
    if existing.upload_state == "uploaded"
        && existing.uploaded_revision == Some(record.current_revision)
        && existing.uploaded_summary_hash == record.current_summary_hash
        && record.current_summary_hash.is_some()
        && existing.uploaded_markdown_hash == record.markdown_hash
        && record.markdown_hash.is_some()
        && records_full_snapshot(&existing)
    {
        return Ok(UploadOutcome::AlreadyUploaded {
            snapshot_id: existing.snapshot_id,
            revision: record.current_revision,
        });
    }

    // Assemble the artifact set. A transcript that changed under the upload (ADR 0009) surfaces here
    // as a distinct retryable failure recorded against this row, not a propagated error, so the
    // recovery driver retries it; other collection errors (a genuine I/O fault) still propagate.
    // A required artifact that is not readable at all is neither a failure nor an upload: no
    // bounded attempt is burned, and the session uploads as soon as the artifact is readable.
    let sources = match collect_recoverable_artifacts(
        state,
        output_directory,
        record,
        homes,
        max_event_text_bytes,
    ) {
        Ok(sources) => sources,
        Err(error @ PatwariError::TranscriptChanged) => {
            return record_upload_failure(state, settings, endpoint, record, &existing, error);
        }
        Err(PatwariError::SnapshotIncomplete(missing)) => {
            return Ok(UploadOutcome::Skipped {
                reason: format!("missing-{missing}"),
            });
        }
        Err(error) => return Err(error),
    };
    let artifacts = prepare_artifacts(sources);
    if artifacts.is_empty() {
        return Ok(UploadOutcome::Skipped {
            reason: "no-artifacts".to_owned(),
        });
    }

    match run_upload(
        state,
        client_id,
        endpoint,
        machine_label,
        record,
        &artifacts,
        origin_access,
    ) {
        Ok(receipt) => {
            state.record_archive_upload_success(
                &record.session_id,
                endpoint,
                &ArchiveUploadSuccess {
                    uploaded_revision: record.current_revision,
                    uploaded_summary_hash: record.current_summary_hash.clone().unwrap_or_default(),
                    uploaded_markdown_hash: record.markdown_hash.clone(),
                    snapshot_id: receipt.snapshot_id.clone(),
                    patwari_session_id: receipt.session_id.clone(),
                    uploaded_artifact_paths: artifacts
                        .iter()
                        .map(|artifact| artifact.logical_path.clone())
                        .collect(),
                    transfer_bytes: receipt.upload_transfer_bytes,
                    total_stored_bytes: receipt.total_stored_bytes,
                    total_original_bytes: receipt.total_original_bytes,
                },
            )?;
            Ok(UploadOutcome::Uploaded {
                snapshot_id: receipt.snapshot_id,
                revision: record.current_revision,
            })
        }
        Err(error) => record_upload_failure(state, settings, endpoint, record, &existing, error),
    }
}

/// Whether the ledger proves this row's uploaded snapshot was self-contained (issue #47): it
/// recorded an artifact set, and that set carries every required logical path.
///
/// A row written before the ledger recorded artifact paths has none, and is therefore *not* proven
/// self-contained — the summary-only snapshots this issue fixes are exactly such rows. Treating
/// unrecorded as unproven re-verifies each pre-existing row once (the re-upload is cheap: Patwari
/// deduplicates blobs by content hash and coalesces an identical snapshot fingerprint), after which
/// the row records its set and is never re-uploaded again.
fn records_full_snapshot(record: &ArchiveUploadRecord) -> bool {
    record
        .uploaded_artifact_paths
        .as_ref()
        .is_some_and(|paths| {
            REQUIRED_LOGICAL_PATHS
                .iter()
                .all(|required| paths.iter().any(|path| path == required))
        })
}

/// Records a failed upload attempt against the session's row (bounded backoff, then a dead letter
/// once attempts are exhausted) and projects it into a [`UploadOutcome::Failed`]. Never touches the
/// session's archival lifecycle.
fn record_upload_failure(
    state: &mut StateStore,
    settings: &crate::registration::StoredArchiveUpload,
    endpoint: &str,
    record: &SessionRecord,
    existing: &ArchiveUploadRecord,
    error: PatwariError,
) -> Result<UploadOutcome, PatwariError> {
    let category = error.category().to_owned();
    let updated = state.record_archive_upload_failure(
        &record.session_id,
        endpoint,
        &category,
        settings.max_attempts.max(1),
        now_ms().saturating_add(backoff_ms(existing.attempts.saturating_add(1))),
    )?;
    Ok(UploadOutcome::Failed {
        category,
        dead_letter: updated.upload_state == "dead-letter",
    })
}

/// Builds the capture's opaque `source_metadata` map: everything munshi knows about *this
/// observation* that has no typed home in the manifest. Every key is lowercase snake, every value a
/// short sanitized string, and every one of them is optional — the map is opaque to Patwari, which
/// returns it verbatim, so a reader that does not recognize a key ignores it.
fn capture_source_metadata(
    record: &SessionRecord,
    captured_at: &str,
    machine_label: Option<&str>,
    origin_access: OriginAccess,
) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    // A recorded-evidence identity (issue #40) is flagged in the capture metadata so a consumer can
    // distinguish it from a live-resolved one; live identities add nothing.
    if let Some(marker) = record
        .project
        .as_ref()
        .and_then(|project| project.origin.recorded_marker())
    {
        metadata.insert("origin".to_owned(), marker.to_owned());
    }
    // Capture-machine provenance (issue #77), each omitted rather than guessed when unavailable.
    if let Some(offset) = capture_utc_offset(captured_at) {
        metadata.insert("utc_offset".to_owned(), offset);
    }
    if let Some(hostname) = capture_hostname(machine_label) {
        metadata.insert("hostname".to_owned(), hostname);
    }
    // Instruction-file provenance (issue #77), omitted wholesale when this attempt may not touch the
    // origin directory (issue #61) and per-file whenever munshi could not look.
    metadata.extend(capture_instruction_provenance(record, origin_access));
    metadata
}

/// Performs the network upload for one revision: resolve the capture identity, assemble the
/// manifest, connect, and run the resumable upload, persisting the server upload id for resume.
fn run_upload(
    state: &mut StateStore,
    client_id: &str,
    endpoint: &str,
    machine_label: Option<&str>,
    record: &SessionRecord,
    artifacts: &[PreparedArtifact],
    origin_access: OriginAccess,
) -> Result<UploadReceipt, PatwariError> {
    // Mint a fresh capture id + captured_at for this revision, or reuse the persisted pair (and any
    // resumable upload id) when this is a retry of the same attempt.
    let prep = state.prepare_archive_capture(
        &record.session_id,
        endpoint,
        record.current_revision,
        &new_uuid(),
        &now_rfc3339(),
    )?;

    let session = SessionContext {
        source_agent: record.source.agent_label().to_owned(),
        source_session_id: record.session_id.clone(),
    };
    let capture = CaptureContext {
        captured_at: prep.captured_at.clone(),
        source_cursor: Some(record.current_revision.to_string()),
        source_state_hash: record.current_summary_hash.clone(),
        source_metadata: capture_source_metadata(
            record,
            &prep.captured_at,
            machine_label,
            origin_access,
        ),
        project: record
            .project
            .as_ref()
            .map(|project| project.identity.clone()),
        repository: record
            .project
            .as_ref()
            .and_then(|project| project.repository.clone()),
        branch: record
            .project
            .as_ref()
            .and_then(|project| project.branch.clone()),
        source_agent_version: None,
        artifact_set_version: CURRENT_ARTIFACT_SET_VERSION,
        munshi_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
    };
    let manifest = build_manifest(&session, &capture, artifacts);

    let client = PatwariClient::connect(endpoint, client_id)?;
    let mut metadata = BTreeMap::new();
    metadata.insert(
        "munshi_version".to_owned(),
        env!("CARGO_PKG_VERSION").to_owned(),
    );
    client.register_client(hostname().as_deref(), None, &metadata)?;

    let session_id = record.session_id.clone();
    let endpoint_owned = endpoint.to_owned();
    // Persisting the upload id is best-effort: a failure to record it only costs a re-create on the
    // next attempt (which is idempotent by capture id), never a duplicate snapshot.
    let mut persist = |upload_id: &str| {
        let _ = state.record_archive_upload_id(&session_id, &endpoint_owned, upload_id);
    };
    client.upload_snapshot(
        &prep.capture_id,
        &manifest,
        artifacts,
        prep.resume_upload_id.as_deref(),
        &mut persist,
    )
}

// ---------------------------------------------------------------------------
// Read-only status contract
// ---------------------------------------------------------------------------

/// A safe, secret-free view of the archive-upload configuration for reporting.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveUploadSettings {
    pub enabled: bool,
    pub addressable: bool,
    pub endpoint: Option<String>,
    pub client_id: Option<String>,
    pub max_attempts: u32,
}

impl ArchiveUploadSettings {
    fn from_config(config: &StoredConfig) -> Self {
        Self {
            enabled: config.archive_upload.enabled,
            addressable: config.archive_upload.is_addressable(),
            endpoint: config.archive_upload.endpoint.clone(),
            client_id: config.archive_upload.client_id.clone(),
            max_attempts: config.archive_upload.max_attempts,
        }
    }

    fn unregistered() -> Self {
        Self {
            enabled: false,
            addressable: false,
            endpoint: None,
            client_id: None,
            max_attempts: DEFAULT_MAX_ARCHIVE_UPLOAD_ATTEMPTS,
        }
    }
}

/// One archive-upload record projected for reporting.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveUploadItem {
    pub source: String,
    pub session_id: String,
    pub state: String,
    pub snapshot_id: Option<String>,
    /// Patwari's own session id (issue #76) — the identity `restore --session` filters on, surfaced
    /// here because the harness `session_id` this row is keyed by is not what restore accepts.
    /// `null` on a row whose upload predates schema 10 until `archive-upload reconcile` fills it.
    pub patwari_session_id: Option<String>,
    pub uploaded_revision: Option<u64>,
    pub attempts: u32,
    pub next_attempt_at_ms: Option<i64>,
    pub last_error_category: Option<String>,
    /// Lifetime bytes actually transferred for this session's uploads (issue #65); 0 when every
    /// artifact deduplicated server-side, and 0 on rows recorded before transfer accounting.
    pub transfer_bytes_total: u64,
    /// The latest uploaded snapshot's stored (compressed) byte total, when measured.
    pub last_stored_bytes: Option<u64>,
}

/// The `archive upload status` contract.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveUploadStatusReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub settings: ArchiveUploadSettings,
    pub total: usize,
    pub uploaded: usize,
    pub pending: usize,
    pub failed: usize,
    pub dead_letter: usize,
    /// Lifetime bytes actually transferred to this endpoint across every recorded upload
    /// (issue #65): the sum of each row's accumulated receipt `upload_transfer_bytes`. This is
    /// the measured number issue #24's "real transfer-volume pain" trigger asks for. Rows
    /// recorded before transfer accounting contribute 0, so the total is a floor.
    pub transfer_bytes_total: u64,
    /// The stored (compressed) byte total of every session's *latest* snapshot summed —
    /// approximately what the archive's current generation occupies, before cross-session blob
    /// dedup. Latest, not lifetime, because successive revisions overlap almost entirely.
    pub stored_bytes_latest_total: u64,
    pub items: Vec<ArchiveUploadItem>,
}

/// Configures the Patwari server endpoint without enabling upload, generating and persisting the
/// durable client UUID if one does not exist yet.
pub fn configure(
    state_directory: &Path,
    endpoint: &str,
) -> Result<ArchiveUploadSettings, PatwariError> {
    http::parse_http_endpoint(endpoint).map_err(from_http)?;
    let (config, ()) = update_stored_config(state_directory, |config| {
        config.archive_upload.endpoint = Some(endpoint.to_owned());
        if config.archive_upload.client_id.is_none() {
            config.archive_upload.client_id = Some(new_uuid());
        }
        Ok(())
    })?;
    Ok(ArchiveUploadSettings::from_config(&config))
}

/// Enables or disables archive upload. Enabling requires a configured, addressable server.
pub fn set_enabled(
    state_directory: &Path,
    enabled: bool,
) -> Result<ArchiveUploadSettings, PatwariError> {
    let result = update_stored_config(state_directory, |config| {
        if enabled && !config.archive_upload.is_addressable() {
            return Err(RegistrationError::MalformedOwnedFile);
        }
        config.archive_upload.enabled = enabled;
        Ok(())
    });
    let (config, ()) = match result {
        Ok(value) => value,
        Err(RegistrationError::MalformedOwnedFile) if enabled => {
            return Err(PatwariError::NotConfigured);
        }
        Err(error) => return Err(PatwariError::Registration(error)),
    };
    Ok(ArchiveUploadSettings::from_config(&config))
}

/// Builds the archive-upload status contract: current settings plus every recorded upload. An
/// unregistered state directory degrades to an empty, disabled report, matching delivery status.
pub fn status(state_directory: &Path) -> Result<ArchiveUploadStatusReport, PatwariError> {
    if !stored_config_exists(state_directory) {
        return Ok(ArchiveUploadStatusReport {
            schema_version: 1,
            command: "archive-upload-status",
            settings: ArchiveUploadSettings::unregistered(),
            total: 0,
            uploaded: 0,
            pending: 0,
            failed: 0,
            dead_letter: 0,
            transfer_bytes_total: 0,
            stored_bytes_latest_total: 0,
            items: Vec::new(),
        });
    }
    let config = load_stored_config(state_directory)?;
    let settings = ArchiveUploadSettings::from_config(&config);
    let uploads = if StateStore::database_path(state_directory).exists() {
        StateStore::open(state_directory)?.list_archive_uploads()?
    } else {
        Vec::new()
    };

    let mut uploaded = 0;
    let mut pending = 0;
    let mut failed = 0;
    let mut dead_letter = 0;
    let mut transfer_bytes_total: u64 = 0;
    let mut stored_bytes_latest_total: u64 = 0;
    let items = uploads
        .iter()
        .map(|record| {
            match record.upload_state.as_str() {
                "uploaded" => uploaded += 1,
                "pending" => pending += 1,
                "failed" => failed += 1,
                "dead-letter" => dead_letter += 1,
                _ => {}
            }
            transfer_bytes_total = transfer_bytes_total.saturating_add(record.transfer_bytes_total);
            stored_bytes_latest_total =
                stored_bytes_latest_total.saturating_add(record.last_stored_bytes.unwrap_or(0));
            ArchiveUploadItem {
                source: record.source.as_selector().to_owned(),
                session_id: record.session_id.clone(),
                state: record.upload_state.clone(),
                snapshot_id: record.snapshot_id.clone(),
                patwari_session_id: record.patwari_session_id.clone(),
                uploaded_revision: record.uploaded_revision,
                attempts: record.attempts,
                next_attempt_at_ms: record.next_attempt_at_ms,
                last_error_category: record.last_error_category.clone(),
                transfer_bytes_total: record.transfer_bytes_total,
                last_stored_bytes: record.last_stored_bytes,
            }
        })
        .collect::<Vec<_>>();

    Ok(ArchiveUploadStatusReport {
        schema_version: 1,
        command: "archive-upload-status",
        settings,
        total: items.len(),
        uploaded,
        pending,
        failed,
        dead_letter,
        transfer_bytes_total,
        stored_bytes_latest_total,
        items,
    })
}

impl ArchiveUploadStatusReport {
    pub fn print_human(&self) {
        print_settings(&self.settings);
        println!(
            "archive uploads total={} uploaded={} pending={} failed={} dead-letter={}",
            self.total, self.uploaded, self.pending, self.failed, self.dead_letter
        );
        println!(
            "archive transfer lifetime-bytes={} latest-snapshots-stored-bytes={}",
            self.transfer_bytes_total, self.stored_bytes_latest_total
        );
        for item in &self.items {
            println!(
                "{}  {}  {}{}{}{}",
                item.session_id,
                item.state,
                item.snapshot_id.as_deref().unwrap_or("<no-snapshot>"),
                item.patwari_session_id
                    .as_deref()
                    .map(|id| format!(" patwari={id}"))
                    .unwrap_or_default(),
                item.uploaded_revision
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

// ---------------------------------------------------------------------------
// Reconcile (issue #76)
// ---------------------------------------------------------------------------

/// One upload row `archive-upload reconcile` filled a Patwari session id into (issue #76).
#[derive(Debug, Clone, Serialize)]
pub struct ReconciledUpload {
    pub source: String,
    /// The harness session id the row is keyed by.
    pub session_id: String,
    pub snapshot_id: String,
    /// The Patwari session id filled from the server's snapshot listing — the identity
    /// `restore --session` filters on.
    pub patwari_session_id: String,
}

/// One uploaded row whose recorded snapshot was absent and was reset for a fresh backfill.
#[derive(Debug, Clone, Serialize)]
pub struct RepairedUpload {
    pub source: String,
    pub session_id: String,
    pub missing_snapshot_id: String,
}

/// The `archive-upload reconcile` contract: backfill old Patwari session ids and, when requested,
/// reset uploaded rows whose recorded snapshot no longer exists.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveUploadReconcileReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub settings: ArchiveUploadSettings,
    /// Uploaded rows that already carried a Patwari session id, left untouched.
    pub already_present: usize,
    /// Rows missing the id that this run filled from the listing.
    pub filled: usize,
    /// Rows missing the id whose snapshot the listing did not contain — pruned server-side, or
    /// belonging to a different server — left `null` for a later run.
    pub unmatched: usize,
    /// Rows that never recorded a snapshot id (no successful upload); nothing to reconcile.
    pub no_snapshot: usize,
    /// Uploaded rows whose absent snapshot was reset for a fresh backfill.
    pub repaired_missing: usize,
    /// The rows filled this run.
    pub reconciled: Vec<ReconciledUpload>,
    /// The rows reset this run.
    pub repaired: Vec<RepairedUpload>,
}

impl ArchiveUploadReconcileReport {
    pub fn print_human(&self) {
        print_settings(&self.settings);
        println!(
            "archive reconcile filled={} already-present={} unmatched={} no-snapshot={} repaired-missing={}",
            self.filled,
            self.already_present,
            self.unmatched,
            self.no_snapshot,
            self.repaired_missing
        );
        for row in &self.reconciled {
            println!(
                "{}  {} -> patwari={}",
                row.session_id, row.snapshot_id, row.patwari_session_id
            );
        }
        for row in &self.repaired {
            println!(
                "{}  missing {} -> pending",
                row.session_id, row.missing_snapshot_id
            );
        }
    }
}

/// Maps a read-time listing error onto the archive-upload error type, reusing the upload path's
/// HTTP mapping for transport faults so a caller sees one error vocabulary.
fn from_read(error: ReadError) -> PatwariError {
    match error {
        ReadError::Http(http) => from_http(http),
        ReadError::Status { status, code } => PatwariError::Protocol(format!(
            "archive snapshot listing returned status {status}{}",
            code.map(|code| format!(" ({code})")).unwrap_or_default()
        )),
        ReadError::Protocol(message) => PatwariError::Protocol(message),
    }
}

/// Backfills the Patwari session id (issue #76) onto uploaded rows that predate schema 10. With
/// `repair_missing`, also verifies every row's recorded snapshot directly and resets a 404 to a
/// fresh pending attempt. This includes rows that moved from `uploaded` to `failed` or
/// `dead-letter` while retaining their last successful snapshot id. The direct check avoids
/// treating an incomplete archive listing as proof of deletion. Idempotent: present snapshots and
/// already-reset rows are left untouched.
/// Needs a configured, addressable endpoint but not an enabled one; backfill still requires upload
/// to be enabled. Never mutates archival lifecycle.
pub fn reconcile(
    state_directory: &Path,
    repair_missing: bool,
) -> Result<ArchiveUploadReconcileReport, PatwariError> {
    let config = load_stored_config(state_directory)?;
    let settings = ArchiveUploadSettings::from_config(&config);
    if !config.archive_upload.is_addressable() {
        return Err(PatwariError::NotConfigured);
    }
    let endpoint = config.archive_upload.endpoint.clone().unwrap();

    let client = ReadClient::connect(&endpoint).map_err(from_http)?;
    let listing = client.list_snapshots(None).map_err(from_read)?;
    let mapping: BTreeMap<String, String> = listing
        .items
        .into_iter()
        .map(|snapshot| (snapshot.snapshot_id, snapshot.session_id))
        .collect();

    let mut already_present = 0usize;
    let mut unmatched = 0usize;
    let mut no_snapshot = 0usize;
    let mut reconciled = Vec::new();
    let mut repaired = Vec::new();

    if StateStore::database_path(state_directory).exists() {
        let mut store = StateStore::open(state_directory)?;
        let uploads = store.list_archive_uploads()?;
        for record in &uploads {
            if record.endpoint != endpoint {
                continue;
            }
            let Some(snapshot_id) = record.snapshot_id.as_deref() else {
                no_snapshot += 1;
                continue;
            };
            if repair_missing && !client.snapshot_exists(snapshot_id).map_err(from_read)? {
                let mut source_store = StateStore::open_for_source(state_directory, record.source)?;
                if source_store.repair_missing_archive_upload(
                    &record.session_id,
                    &endpoint,
                    snapshot_id,
                )? {
                    repaired.push(RepairedUpload {
                        source: record.source.as_selector().to_owned(),
                        session_id: record.session_id.clone(),
                        missing_snapshot_id: snapshot_id.to_owned(),
                    });
                }
                continue;
            }
            if record.patwari_session_id.is_some() {
                already_present += 1;
                continue;
            }
            match mapping.get(snapshot_id) {
                Some(patwari) => {
                    if store.backfill_patwari_session_id(&endpoint, snapshot_id, patwari)? {
                        reconciled.push(ReconciledUpload {
                            source: record.source.as_selector().to_owned(),
                            session_id: record.session_id.clone(),
                            snapshot_id: snapshot_id.to_owned(),
                            patwari_session_id: patwari.clone(),
                        });
                    }
                }
                None => unmatched += 1,
            }
        }
    }

    Ok(ArchiveUploadReconcileReport {
        schema_version: 1,
        command: "archive-upload-reconcile",
        settings,
        already_present,
        filled: reconciled.len(),
        unmatched,
        no_snapshot,
        repaired_missing: repaired.len(),
        reconciled,
        repaired,
    })
}

// ---------------------------------------------------------------------------
// Fingerprint-preserving rearchive
// ---------------------------------------------------------------------------

fn rearchive_artifact_path(
    output_directory: &Path,
    markdown_relative: &Path,
    logical_path: &str,
) -> Result<PathBuf, PatwariError> {
    let relative = if logical_path == SUMMARY_LOGICAL_PATH {
        return Ok(output_directory.join(markdown_relative));
    } else if logical_path == TRANSCRIPT_LOGICAL_PATH {
        crate::render::restored_relative_directory(markdown_relative).join(TRANSCRIPT_LOGICAL_PATH)
    } else if let Some(digest) = logical_path.strip_prefix(OUTPUTS_LOGICAL_PREFIX) {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(PatwariError::Protocol(format!(
                "saved snapshot contains invalid output path {logical_path}"
            )));
        }
        crate::render::restored_relative_directory(markdown_relative)
            .join(OUTPUTS_LOGICAL_PREFIX.trim_end_matches('/'))
            .join(digest)
    } else if let Some(relative) = logical_path.strip_prefix(SIDECAR_LOGICAL_PREFIX) {
        let relative = Path::new(relative);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(PatwariError::Protocol(format!(
                "saved snapshot contains invalid sidecar path {logical_path}"
            )));
        }
        crate::render::sidecar_relative_directory(markdown_relative).join(relative)
    } else {
        return Err(PatwariError::Protocol(format!(
            "saved snapshot contains unsupported artifact {logical_path}"
        )));
    };
    Ok(output_directory.join(relative))
}

fn rearchive_sources(
    snapshot: &Value,
    output_directory: &Path,
    record: &SessionRecord,
) -> Result<Vec<ArtifactSource>, PatwariError> {
    let markdown_relative = record
        .markdown_relative_path
        .as_deref()
        .ok_or(PatwariError::SnapshotIncomplete(SUMMARY_LOGICAL_PATH))?;
    let artifacts = snapshot
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            PatwariError::Protocol("saved snapshot response omitted artifacts".to_owned())
        })?;
    let mut seen = BTreeSet::new();
    let mut sources = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let logical_path = artifact
            .get("logical_path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PatwariError::Protocol("saved snapshot artifact omitted logical_path".to_owned())
            })?;
        if !seen.insert(logical_path.to_owned()) {
            return Err(PatwariError::Protocol(format!(
                "saved snapshot repeated artifact {logical_path}"
            )));
        }
        let expected_size = artifact
            .get("original_size_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                PatwariError::Protocol(format!(
                    "saved snapshot artifact {logical_path} omitted original_size_bytes"
                ))
            })?;
        let expected_sha = artifact
            .get("original_sha256")
            .and_then(Value::as_str)
            .map(|value| value.strip_prefix("sha256:").unwrap_or(value))
            .ok_or_else(|| {
                PatwariError::Protocol(format!(
                    "saved snapshot artifact {logical_path} omitted original_sha256"
                ))
            })?;
        let path = rearchive_artifact_path(output_directory, markdown_relative, logical_path)?;
        let bytes = std::fs::read(&path).map_err(PatwariError::Io)?;
        if bytes.len() as u64 != expected_size || sha256_hex(&bytes) != expected_sha {
            return Err(PatwariError::Protocol(format!(
                "restored artifact {logical_path} does not match the saved snapshot"
            )));
        }
        sources.push(ArtifactSource {
            logical_path: logical_path.to_owned(),
            media_type: artifact
                .get("media_type")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            bytes,
        });
    }
    if !REQUIRED_LOGICAL_PATHS
        .iter()
        .all(|required| seen.contains(*required))
    {
        return Err(PatwariError::Protocol(
            "saved snapshot is not self-contained".to_owned(),
        ));
    }
    sources.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    Ok(sources)
}

fn rearchive_manifest(
    snapshot: &Value,
    record: &SessionRecord,
    prep: &crate::state::CapturePrep,
    artifacts: &[PreparedArtifact],
    machine_label: Option<&str>,
    origin_access: OriginAccess,
) -> Result<Value, PatwariError> {
    let mut manifest = snapshot.get("manifest").cloned().ok_or_else(|| {
        PatwariError::Protocol("saved snapshot response omitted manifest".to_owned())
    })?;
    let session = manifest
        .get("session")
        .and_then(Value::as_object)
        .ok_or_else(|| PatwariError::Protocol("saved manifest omitted session".to_owned()))?;
    if session.get("source_session_id").and_then(Value::as_str) != Some(record.session_id.as_str())
        || session.get("source_agent").and_then(Value::as_str) != Some(record.source.agent_label())
    {
        return Err(PatwariError::Protocol(
            "saved manifest session does not match the requested Munshi session".to_owned(),
        ));
    }
    let capture = manifest
        .get_mut("capture")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| PatwariError::Protocol("saved manifest omitted capture".to_owned()))?;
    let metadata = capture
        .entry("source_metadata")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            PatwariError::Protocol("saved source_metadata is not an object".to_owned())
        })?;
    // Freshly observed provenance is merged *over* the saved snapshot's map rather than replacing
    // it, so a key this capture omits leaves the prior value standing — already true of
    // `utc_offset` and `hostname`, and now of `claude_md` / `agents_md`: a rearchive that could not
    // read the project root reports the digests the tombstoned snapshot recorded, not `absent`.
    for (key, value) in
        capture_source_metadata(record, &prep.captured_at, machine_label, origin_access)
    {
        metadata.insert(key, Value::String(value));
    }
    capture.insert(
        "captured_at".to_owned(),
        Value::String(prep.captured_at.clone()),
    );
    capture.insert(
        "source_cursor".to_owned(),
        Value::String(record.current_revision.to_string()),
    );
    capture.insert(
        "source_state_hash".to_owned(),
        record
            .current_summary_hash
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    capture.insert(
        "munshi_version".to_owned(),
        Value::String(env!("CARGO_PKG_VERSION").to_owned()),
    );
    manifest
        .as_object_mut()
        .ok_or_else(|| PatwariError::Protocol("saved manifest is not an object".to_owned()))?
        .insert(
            "artifacts".to_owned(),
            Value::Array(prepared_artifacts_json(artifacts)),
        );
    Ok(manifest)
}

/// Re-archives one tombstoned snapshot from its saved inspection document and restore output.
/// Unlike the normal retry path, this preserves every fingerprint-bearing manifest field and uses
/// exactly the artifact logical paths and original bytes the old snapshot recorded. Only
/// fingerprint-excluded capture observation fields are refreshed.
pub fn rearchive(
    state_directory: &Path,
    source: SourceKind,
    session_id: &str,
    snapshot_file: &Path,
) -> Result<ArchiveUploadRunReport, PatwariError> {
    let config = load_stored_config(state_directory)?;
    let settings = ArchiveUploadSettings::from_config(&config);
    if !config.archive_upload.enabled {
        return Err(PatwariError::NotEnabled);
    }
    if !config.archive_upload.is_addressable() {
        return Err(PatwariError::NotConfigured);
    }
    let endpoint = config.archive_upload.endpoint.clone().unwrap();
    let client_id = ensure_client_id(Path::new(&config.state_directory))?;
    let output_directory = PathBuf::from(&config.output_directory);
    let snapshot: Value =
        serde_json::from_slice(&std::fs::read(snapshot_file).map_err(PatwariError::Io)?).map_err(
            |error| PatwariError::Protocol(format!("saved snapshot is not valid JSON: {error}")),
        )?;

    let mut report = ArchiveUploadRunReport {
        schema_version: 1,
        command: "archive-upload-rearchive",
        settings,
        candidates: 1,
        uploaded: 0,
        already_uploaded: 0,
        skipped: 0,
        failed: 0,
        note: None,
        items: Vec::new(),
    };
    let Some(_lock) = try_acquire_session_lock(state_directory, session_id)? else {
        report.record(
            source,
            session_id,
            UploadOutcome::Skipped {
                reason: "worker-busy".to_owned(),
            },
        );
        return Ok(report);
    };
    let mut state = StateStore::open_for_source(state_directory, source)?;
    let Some(record) = state.get_session(session_id)? else {
        report.record(
            source,
            session_id,
            UploadOutcome::Skipped {
                reason: "session-unknown".to_owned(),
            },
        );
        return Ok(report);
    };
    let old_snapshot_id = snapshot
        .get("snapshot_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            PatwariError::Protocol("saved snapshot response omitted snapshot_id".to_owned())
        })?;
    // Validate every local byte and every saved-manifest structural precondition before clearing
    // the stale snapshot linkage. A malformed template must leave the ledger naming the tombstone,
    // not turn the row into a generic pending candidate that normal backfill could pick up.
    let sources = rearchive_sources(&snapshot, &output_directory, &record)?;
    let artifacts = prepare_artifacts(sources);
    let validation_prep = crate::state::CapturePrep {
        capture_id: "validation-only".to_owned(),
        captured_at: now_rfc3339(),
        resume_upload_id: None,
    };
    let machine_label = config.memory_sync.machine_label.as_deref();
    // `munshi archive-upload rearchive` is an operator-invoked CLI command, never scheduler-driven,
    // so reading the project root here cannot raise a background permission prompt (issue #61).
    let origin_access = OriginAccess::Allowed;
    let _ = rearchive_manifest(
        &snapshot,
        &record,
        &validation_prep,
        &artifacts,
        machine_label,
        origin_access,
    )?;
    let expected_fingerprint = snapshot
        .get("snapshot_fingerprint")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            PatwariError::Protocol(
                "saved snapshot response omitted snapshot_fingerprint".to_owned(),
            )
        })?;
    let client = PatwariClient::connect(&endpoint, &client_id)?;
    let mut client_metadata = BTreeMap::new();
    client_metadata.insert(
        "munshi_version".to_owned(),
        env!("CARGO_PKG_VERSION").to_owned(),
    );
    client.register_client(hostname().as_deref(), None, &client_metadata)?;

    // A restored operational database can legitimately lack the old upload ledger row even when
    // the immutable snapshot and all local artifacts prove the session identity. Create the row
    // only after those checks and client registration have succeeded.
    let existing = match state.get_archive_upload(session_id, &endpoint)? {
        Some(existing) => existing,
        None => state
            .ensure_archive_rearchive_target(session_id, &endpoint)?
            .ok_or_else(|| {
                PatwariError::Protocol(
                    "requested session disappeared before ledger repair".to_owned(),
                )
            })?,
    };
    if let Some(recorded_snapshot_id) = existing.snapshot_id.as_deref() {
        if recorded_snapshot_id != old_snapshot_id {
            return Err(PatwariError::Protocol(format!(
                "session records snapshot {recorded_snapshot_id}, not saved snapshot {old_snapshot_id}"
            )));
        }
        let read = ReadClient::connect(&endpoint).map_err(from_http)?;
        if read.snapshot_exists(old_snapshot_id).map_err(from_read)? {
            return Err(PatwariError::Protocol(
                "saved snapshot is still live; tombstone it before rearchiving".to_owned(),
            ));
        }
        if !state.repair_missing_archive_upload_for_rearchive(
            session_id,
            &endpoint,
            old_snapshot_id,
        )? {
            return Err(PatwariError::Protocol(
                "missing snapshot ledger row could not be repaired".to_owned(),
            ));
        }
        state
            .get_archive_upload(session_id, &endpoint)?
            .ok_or_else(|| {
                PatwariError::Protocol("repaired archive-upload row disappeared".to_owned())
            })?;
    } else {
        state
            .ensure_archive_rearchive_target(session_id, &endpoint)?
            .ok_or_else(|| {
                PatwariError::Protocol("archive-upload rearchive row disappeared".to_owned())
            })?;
    }
    let prep = state.prepare_archive_capture(
        session_id,
        &endpoint,
        record.current_revision,
        &new_uuid(),
        &now_rfc3339(),
    )?;
    let manifest = rearchive_manifest(
        &snapshot,
        &record,
        &prep,
        &artifacts,
        machine_label,
        origin_access,
    )?;
    let endpoint_owned = endpoint.clone();
    let receipt = client.upload_snapshot(
        &prep.capture_id,
        &manifest,
        &artifacts,
        prep.resume_upload_id.as_deref(),
        |upload_id| {
            let _ = state.record_archive_upload_id(session_id, &endpoint_owned, upload_id);
        },
    );
    match receipt {
        Ok(receipt) if receipt.snapshot_fingerprint != expected_fingerprint => {
            state.record_archive_rearchive_fingerprint_mismatch(session_id, &endpoint)?;
            return Err(PatwariError::Protocol(format!(
                "rearchive created snapshot {} with fingerprint {}, expected \
                 {expected_fingerprint}; tombstone that snapshot before retrying",
                receipt.snapshot_id, receipt.snapshot_fingerprint
            )));
        }
        Ok(receipt) => {
            state.record_archive_upload_success(
                session_id,
                &endpoint,
                &ArchiveUploadSuccess {
                    uploaded_revision: record.current_revision,
                    uploaded_summary_hash: record.current_summary_hash.clone().unwrap_or_default(),
                    uploaded_markdown_hash: record.markdown_hash.clone(),
                    snapshot_id: receipt.snapshot_id.clone(),
                    patwari_session_id: receipt.session_id,
                    uploaded_artifact_paths: artifacts
                        .iter()
                        .map(|artifact| artifact.logical_path.clone())
                        .collect(),
                    transfer_bytes: receipt.upload_transfer_bytes,
                    total_stored_bytes: receipt.total_stored_bytes,
                    total_original_bytes: receipt.total_original_bytes,
                },
            )?;
            report.record(
                source,
                session_id,
                UploadOutcome::Uploaded {
                    snapshot_id: receipt.snapshot_id,
                    revision: record.current_revision,
                },
            );
        }
        Err(error) => {
            let category = error.category().to_owned();
            state.record_archive_rearchive_failure(session_id, &endpoint, &category)?;
            report.record(
                source,
                session_id,
                UploadOutcome::Failed {
                    category,
                    dead_letter: false,
                },
            );
        }
    }
    Ok(report)
}

/// Explicitly abandons a parked fingerprint-preserving rearchive so future revisions can use the
/// ordinary current-version upload path.
pub fn abandon_rearchive(
    state_directory: &Path,
    source: SourceKind,
    session_id: &str,
) -> Result<ArchiveUploadRunReport, PatwariError> {
    let config = load_stored_config(state_directory)?;
    let settings = ArchiveUploadSettings::from_config(&config);
    let endpoint = config
        .archive_upload
        .endpoint
        .clone()
        .ok_or(PatwariError::NotConfigured)?;
    let mut report = ArchiveUploadRunReport {
        schema_version: 1,
        command: "archive-upload-rearchive-abandon",
        settings,
        candidates: 1,
        uploaded: 0,
        already_uploaded: 0,
        skipped: 0,
        failed: 0,
        note: None,
        items: Vec::new(),
    };
    let Some(_lock) = try_acquire_session_lock(state_directory, session_id)? else {
        report.record(
            source,
            session_id,
            UploadOutcome::Skipped {
                reason: "worker-busy".to_owned(),
            },
        );
        return Ok(report);
    };
    let mut state = StateStore::open_for_source(state_directory, source)?;
    if !state.abandon_archive_rearchive(session_id, &endpoint)? {
        return Err(PatwariError::Protocol(
            "session has no parked rearchive to abandon".to_owned(),
        ));
    }
    report.record(
        source,
        session_id,
        UploadOutcome::Skipped {
            reason: "rearchive-abandoned".to_owned(),
        },
    );
    Ok(report)
}

// ---------------------------------------------------------------------------
// Retry run contract
// ---------------------------------------------------------------------------

/// The `archive-upload retry` contract: current settings plus the outcome of each attempted upload.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveUploadRunReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub settings: ArchiveUploadSettings,
    pub candidates: usize,
    pub uploaded: usize,
    pub already_uploaded: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Issue #54: set when a named session matched no candidate because its upload rows all
    /// belong to endpoints other than the configured one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub items: Vec<ArchiveUploadRunItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArchiveUploadRunItem {
    pub source: String,
    pub session_id: String,
    pub outcome: UploadOutcome,
}

impl ArchiveUploadRunReport {
    pub fn print_human(&self) {
        print_settings(&self.settings);
        let label = match self.command {
            "archive-upload-backfill" => "archive-upload backfill",
            "archive-upload-rearchive-abandon" => "archive-upload rearchive --abandon",
            _ => "archive-upload retry",
        };
        println!(
            "{label} candidates={} uploaded={} already-uploaded={} skipped={} failed={}",
            self.candidates, self.uploaded, self.already_uploaded, self.skipped, self.failed
        );
        if let Some(note) = &self.note {
            println!("note: {note}");
        }
        for item in &self.items {
            println!("{} -> {}", item.session_id, item.outcome.as_kind());
        }
    }

    /// Tallies one session's outcome into the counts and appends its per-session item.
    fn record(&mut self, source: SourceKind, session_id: &str, outcome: UploadOutcome) {
        match &outcome {
            UploadOutcome::Uploaded { .. } => self.uploaded += 1,
            UploadOutcome::AlreadyUploaded { .. } => self.already_uploaded += 1,
            UploadOutcome::Skipped { .. } => self.skipped += 1,
            UploadOutcome::Failed { .. } => self.failed += 1,
        }
        self.items.push(ArchiveUploadRunItem {
            source: source.as_selector().to_owned(),
            session_id: session_id.to_owned(),
            outcome,
        });
    }
}

fn print_settings(settings: &ArchiveUploadSettings) {
    println!(
        "archive upload {} (endpoint {}, client {}, max-attempts {})",
        if settings.enabled {
            "enabled"
        } else {
            "disabled"
        },
        settings.endpoint.as_deref().unwrap_or("<unset>"),
        settings.client_id.as_deref().unwrap_or("<unset>"),
        settings.max_attempts,
    );
}

/// Issue #54: a named session whose only upload rows belong to endpoints other than the
/// configured one would otherwise produce a silent zero-candidate retry — which reads as
/// "nothing wrong" while the session sits unreconciled after an endpoint change. This names
/// the stale endpoint(s) and the designed reconciliation path (`backfill`) instead.
fn stale_endpoint_note(
    recorded: &[ArchiveUploadRecord],
    endpoint: &str,
    session_id: &str,
    source: Option<SourceKind>,
) -> Option<String> {
    let row_matches = |record: &ArchiveUploadRecord| {
        record.session_id == session_id && source.is_none_or(|wanted| record.source == wanted)
    };
    let mut foreign: Vec<&str> = recorded
        .iter()
        .filter(|record| row_matches(record) && record.endpoint != endpoint)
        .map(|record| record.endpoint.as_str())
        .collect();
    foreign.sort_unstable();
    foreign.dedup();
    let any_configured = recorded
        .iter()
        .any(|record| row_matches(record) && record.endpoint == endpoint);
    (!any_configured && !foreign.is_empty()).then(|| {
        format!(
            "session has upload history only for {}; run `munshi archive-upload backfill` to reconcile against the configured server",
            foreign.join(", ")
        )
    })
}

/// Retries failed uploads, or one session's upload, against the configured server. Requires archive
/// upload to be enabled and addressable. Each candidate's backoff is cleared before the attempt
/// (`force` additionally revives a dead-letter row and resets its bounded attempt count), then the
/// upload runs under the session's advisory lock. A locked session is reported skipped this run.
/// Never mutates the session's archival lifecycle.
///
/// `origin_access` is not incidental here. This reads like an operator command, but `munshi tick`
/// calls it on every scheduler pass to drain failed rows, which is exactly the state a Patwari
/// outage leaves the ledger in. A scheduler-driven caller must pass [`OriginAccess::Withheld`] so
/// the retry records no instruction-file provenance rather than reaching into a possibly
/// TCC-protected origin directory (issue #61); the `munshi archive-upload retry` CLI passes
/// [`OriginAccess::Allowed`].
#[allow(clippy::too_many_arguments)]
pub fn retry(
    state_directory: &Path,
    source: Option<SourceKind>,
    session_id: Option<String>,
    all: bool,
    force: bool,
    limit: usize,
    origin_access: OriginAccess,
) -> Result<ArchiveUploadRunReport, PatwariError> {
    let _ = all;
    let config = load_stored_config(state_directory)?;
    let settings = ArchiveUploadSettings::from_config(&config);
    if !config.archive_upload.enabled {
        return Err(PatwariError::NotEnabled);
    }
    if !config.archive_upload.is_addressable() {
        return Err(PatwariError::NotConfigured);
    }
    let endpoint = config.archive_upload.endpoint.clone().unwrap();
    let client_id = ensure_client_id(Path::new(&config.state_directory))?;
    let output_directory = PathBuf::from(&config.output_directory);

    let recorded = if StateStore::database_path(state_directory).exists() {
        StateStore::open(state_directory)?.list_archive_uploads()?
    } else {
        Vec::new()
    };
    let stale_endpoint_note = session_id
        .as_ref()
        .and_then(|id| stale_endpoint_note(&recorded, &endpoint, id, source));
    let mut candidates: Vec<ArchiveUploadRecord> = recorded
        .into_iter()
        .filter(|record| record.endpoint == endpoint)
        .filter(|record| match &session_id {
            // One session: retry any ordinary state, optionally narrowed by source. A parked
            // fingerprint-preserving rearchive is resumed only by `archive-upload rearchive`.
            Some(id) => {
                record.session_id == *id
                    && source.is_none_or(|wanted| record.source == wanted)
                    && !matches!(
                        record.upload_state.as_str(),
                        "rearchive-pending" | "rearchive-failed"
                    )
            }
            // --all: every failed upload, plus dead-letter rows only when forced.
            None => {
                record.upload_state == "failed" || (force && record.upload_state == "dead-letter")
            }
        })
        .collect();
    candidates.truncate(limit);

    let mut report = ArchiveUploadRunReport {
        schema_version: 1,
        command: "archive-upload-retry",
        settings,
        candidates: candidates.len(),
        uploaded: 0,
        already_uploaded: 0,
        skipped: 0,
        failed: 0,
        note: stale_endpoint_note,
        items: Vec::new(),
    };
    for record in candidates {
        let outcome = locked_upload_one(
            state_directory,
            &config.archive_upload,
            &client_id,
            &endpoint,
            config.memory_sync.machine_label.as_deref(),
            &output_directory,
            record.source,
            &record.session_id,
            &config.harnesses.source_homes(),
            config.limits.max_event_text_bytes,
            Some(force),
            origin_access,
        )?;
        report.record(record.source, &record.session_id, outcome);
    }
    Ok(report)
}

/// Uploads every archived session the configured server holds no self-contained snapshot for
/// (issues #32 and #47).
///
/// `upload_after_archive` runs only in the worker downstream of a fresh archive, and the retry
/// paths operate on existing `archive_uploads` rows, so a session archived while upload was
/// disabled (or before configuration) is otherwise never uploaded. Backfill scans archived
/// sessions across every source and keeps two kinds of candidate for the currently configured
/// endpoint:
///
/// - sessions with no upload row at all (issue #32); and
/// - sessions whose `uploaded` row does not record a self-contained snapshot (issue #47) — an
///   older client uploaded a summary-only snapshot for a session whose transcript it could not
///   read, or the row predates the ledger recording its artifact set at all.
///
/// Both run through the normal `upload_one` path — row creation, capture-id minting, bounded
/// attempts, and failure recording behave exactly like a post-archive upload — so a re-upload
/// candidate whose transcript is still unreadable is reported skipped rather than re-uploaded
/// incomplete. The old summary-only snapshot stays in the archive as historical provenance
/// (Patwari snapshots are immutable). A re-upload only *adds* a snapshot when its content is
/// genuinely new: Patwari's snapshot fingerprint covers session identity, artifact-set version,
/// and the artifacts' logical paths/sizes/hashes, so an identical set coalesces into whichever
/// snapshot already carries that fingerprint — adding a capture row but never advancing the
/// snapshot's `completed_at`, and therefore never advancing the session's `latest_snapshot`.
/// If a *newer* degenerate snapshot shadows an older complete one, a backfill re-upload cannot
/// displace it (munshi#78: the 2026-07-28 burst left 56 sessions in exactly that state, repaired
/// by tombstoning the degenerate snapshots archive-side). Superseding a published snapshot
/// requires changing something inside the fingerprint — i.e. re-archiving genuinely new content.
///
/// Requires archive upload to be enabled and addressable; candidates are bounded by `limit`; a
/// session whose advisory lock is held (an archive worker is on it) is reported skipped this run.
/// Never mutates the session's archival lifecycle.
///
/// `origin_access` states whether this run may read session origin directories for instruction-file
/// provenance (issue #61). Nothing scheduler-driven calls backfill today, but it takes the answer
/// explicitly rather than assuming one, for the reason [`OriginAccess`] records.
pub fn backfill(
    state_directory: &Path,
    limit: usize,
    origin_access: OriginAccess,
) -> Result<ArchiveUploadRunReport, PatwariError> {
    let config = load_stored_config(state_directory)?;
    let settings = ArchiveUploadSettings::from_config(&config);
    if !config.archive_upload.enabled {
        return Err(PatwariError::NotEnabled);
    }
    if !config.archive_upload.is_addressable() {
        return Err(PatwariError::NotConfigured);
    }
    let endpoint = config.archive_upload.endpoint.clone().unwrap();
    let client_id = ensure_client_id(Path::new(&config.state_directory))?;
    let output_directory = PathBuf::from(&config.output_directory);

    let (sessions, uploads) = if StateStore::database_path(state_directory).exists() {
        let store = StateStore::open(state_directory)?;
        (store.list_sessions()?, store.list_archive_uploads()?)
    } else {
        (Vec::new(), Vec::new())
    };
    // Sessions already holding a row for this endpoint belong to the worker and retry paths, with
    // three exceptions this run reconciles: an `uploaded` row whose snapshot is not proven
    // self-contained (issue #47), and one whose recorded markdown hash no longer matches the
    // session's current markdown (issue #73), or a formerly uploaded row whose missing snapshot
    // `reconcile --repair-missing` reset to pending. A row recorded for a different endpoint (e.g.
    // before reconfiguration) does not count here at all, matching `retry`'s endpoint scoping.
    let recorded: BTreeMap<(SourceKind, &str), &ArchiveUploadRecord> = uploads
        .iter()
        .filter(|record| record.endpoint == endpoint)
        .map(|record| ((record.source, record.session_id.as_str()), record))
        .collect();
    let mut candidates: Vec<&SessionRecord> = sessions
        .iter()
        .filter(|session| session.lifecycle_state == "archived")
        .filter(|session| {
            match recorded.get(&(session.source, session.session_id.as_str())) {
                None => true,
                // Only a terminal `uploaded` row is re-verified here: `pending`, `failed`, and
                // `dead-letter` rows are the retry paths' business and are left untouched. It is
                // re-verified when its snapshot is not proven self-contained, or when its recorded
                // markdown hash has drifted from the session's current markdown. `upload_one`'s own
                // idempotency check is the backstop: a candidate that is in fact current still
                // reports `already-uploaded` and never reaches the server.
                Some(record) => {
                    (record.upload_state == "uploaded"
                        && (!records_full_snapshot(record)
                            || record.uploaded_markdown_hash != session.markdown_hash))
                        || (record.upload_state == "pending"
                            && record.snapshot_id.is_none()
                            && record.uploaded_revision.is_some())
                }
            }
        })
        .collect();
    candidates.truncate(limit);

    let mut report = ArchiveUploadRunReport {
        schema_version: 1,
        command: "archive-upload-backfill",
        settings,
        candidates: candidates.len(),
        uploaded: 0,
        already_uploaded: 0,
        skipped: 0,
        failed: 0,
        note: None,
        items: Vec::new(),
    };
    for session in candidates {
        let outcome = locked_upload_one(
            state_directory,
            &config.archive_upload,
            &client_id,
            &endpoint,
            config.memory_sync.machine_label.as_deref(),
            &output_directory,
            session.source,
            &session.session_id,
            &config.harnesses.source_homes(),
            config.limits.max_event_text_bytes,
            None,
            origin_access,
        )?;
        report.record(session.source, &session.session_id, outcome);
    }
    Ok(report)
}

/// Runs one session's upload attempt under its advisory lock, opening the store in the session's
/// source scope. `reset_for_retry` is `Some(force)` for an explicit retry, which clears the row's
/// backoff first (and, with force, revives a dead-letter row and resets its bounded attempt
/// count) — this is the one caller of `reset_archive_upload_for_retry`; `None` (backfill) leaves
/// any row as found. A locked session (an archive worker is uploading it) is reported skipped
/// rather than contended.
///
/// `origin_access` is passed through from the public entry point rather than assumed here. An
/// earlier cut hardcoded [`OriginAccess::Allowed`] on the belief that this path was CLI-only; it is
/// not — `munshi tick` drives [`retry`] on every scheduler pass — and the belief was written down
/// as a comment where nothing could enforce it. Only the caller knows what it descended from.
#[allow(clippy::too_many_arguments)]
fn locked_upload_one(
    state_directory: &Path,
    settings: &crate::registration::StoredArchiveUpload,
    client_id: &str,
    endpoint: &str,
    machine_label: Option<&str>,
    output_directory: &Path,
    source: SourceKind,
    session_id: &str,
    homes: &SourceHomes,
    max_event_text_bytes: usize,
    reset_for_retry: Option<bool>,
    origin_access: OriginAccess,
) -> Result<UploadOutcome, PatwariError> {
    let Some(_lock) = try_acquire_session_lock(state_directory, session_id)? else {
        return Ok(UploadOutcome::Skipped {
            reason: "worker-busy".to_owned(),
        });
    };
    let mut state = StateStore::open_for_source(state_directory, source)?;
    if let Some(force) = reset_for_retry {
        state.reset_archive_upload_for_retry(session_id, endpoint, force)?;
    }
    let Some(session) = state.get_session(session_id)? else {
        return Ok(UploadOutcome::Skipped {
            reason: "session-unknown".to_owned(),
        });
    };
    upload_one(
        &mut state,
        settings,
        client_id,
        endpoint,
        machine_label,
        output_directory,
        &session,
        homes,
        max_event_text_bytes,
        origin_access,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::PreviousSource;

    #[test]
    fn prepare_artifact_records_both_representations() {
        let bytes = b"the quick brown fox ".repeat(64).to_vec();
        let prepared = prepare_artifact(ArtifactSource {
            logical_path: "summary.md".to_owned(),
            media_type: Some("text/markdown".to_owned()),
            bytes: bytes.clone(),
        });
        assert_eq!(prepared.original_size_bytes, bytes.len() as u64);
        assert_eq!(prepared.original_sha256, sha256_hex(&bytes));
        // Highly repetitive input compresses, so the stored representation is zstd and smaller.
        assert_eq!(prepared.compression, "zstd");
        assert!(prepared.stored_size_bytes < prepared.original_size_bytes);
        assert_eq!(prepared.stored_sha256, sha256_hex(&prepared.stored_bytes));
        // The stored bytes round-trip back to the original content.
        let restored = zstd::decode_all(prepared.stored_bytes.as_slice()).unwrap();
        assert_eq!(restored, bytes);
    }

    #[test]
    fn incompressible_input_falls_back_to_identity() {
        let prepared = prepare_artifact(ArtifactSource {
            logical_path: "x".to_owned(),
            media_type: None,
            bytes: b"hi".to_vec(),
        });
        assert_eq!(prepared.compression, "identity");
        assert_eq!(prepared.stored_bytes, b"hi");
        assert_eq!(prepared.stored_sha256, prepared.original_sha256);
    }

    #[test]
    fn manifest_uses_prefixed_digests_and_schema_one() {
        let artifacts = prepare_artifacts(vec![ArtifactSource {
            logical_path: "summary.md".to_owned(),
            media_type: Some("text/markdown".to_owned()),
            bytes: b"# Title\n".to_vec(),
        }]);
        let manifest = build_manifest(
            &SessionContext {
                source_agent: "copilot-cli".to_owned(),
                source_session_id: "sess-1".to_owned(),
            },
            &CaptureContext {
                captured_at: "2026-07-25T00:00:00Z".to_owned(),
                source_cursor: Some("1".to_owned()),
                source_state_hash: None,
                source_metadata: BTreeMap::new(),
                project: Some("github.com/o/r".to_owned()),
                repository: None,
                branch: None,
                source_agent_version: None,
                artifact_set_version: CURRENT_ARTIFACT_SET_VERSION,
                munshi_version: Some("0.1.0".to_owned()),
            },
            &artifacts,
        );
        assert_eq!(manifest["schema_version"], 1);
        assert_eq!(manifest["session"]["source_agent"], "copilot-cli");
        let digest = manifest["artifacts"][0]["original_sha256"]
            .as_str()
            .unwrap();
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), "sha256:".len() + 64);
    }

    /// The shape contract consumers may rely on: `[+-]HH:MM`, always two-digit, never anything else.
    fn assert_rfc3339_offset_shape(offset: &str) {
        let bytes = offset.as_bytes();
        assert_eq!(offset.len(), 6, "offset {offset:?} is not six characters");
        assert!(
            bytes[0] == b'+' || bytes[0] == b'-',
            "offset {offset:?} lacks a leading sign"
        );
        assert_eq!(bytes[3], b':', "offset {offset:?} lacks the HH:MM colon");
        assert!(
            offset[1..3].chars().all(|c| c.is_ascii_digit())
                && offset[4..6].chars().all(|c| c.is_ascii_digit()),
            "offset {offset:?} has non-digit fields"
        );
    }

    #[test]
    fn utc_offset_renders_every_real_zone_as_rfc3339() {
        // Whole hours either side of UTC, plus the half- and quarter-hour zones that exist precisely
        // to catch an implementation that assumed offsets are integral hours.
        assert_eq!(format_utc_offset(5 * 3600 + 30 * 60).unwrap(), "+05:30"); // Kolkata
        assert_eq!(format_utc_offset(-8 * 3600).unwrap(), "-08:00"); // Los Angeles
        assert_eq!(format_utc_offset(0).unwrap(), "+00:00"); // UTC renders positive
        assert_eq!(format_utc_offset(5 * 3600 + 45 * 60).unwrap(), "+05:45"); // Kathmandu
        assert_eq!(format_utc_offset(12 * 3600 + 45 * 60).unwrap(), "+12:45"); // Chatham
        assert_eq!(format_utc_offset(-(9 * 3600 + 30 * 60)).unwrap(), "-09:30"); // Marquesas
        assert_eq!(format_utc_offset(14 * 3600).unwrap(), "+14:00"); // Kiritimati, the maximum
        assert_eq!(format_utc_offset(-(60 * 60) - 30 * 60).unwrap(), "-01:30");
        for offset in [
            format_utc_offset(5 * 3600 + 30 * 60).unwrap(),
            format_utc_offset(-8 * 3600).unwrap(),
            format_utc_offset(0).unwrap(),
        ] {
            assert_rfc3339_offset_shape(&offset);
        }
    }

    #[test]
    fn utc_offset_is_omitted_rather_than_rendered_malformed() {
        // Amsterdam's pre-1937 LMT was +00:19:32. A zone at sub-minute precision has no RFC3339
        // spelling at all, so the key is dropped rather than silently rounded to +00:19.
        assert_eq!(format_utc_offset(19 * 60 + 32), None);
        assert_eq!(format_utc_offset(-1), None);
        // An hour count that will not fit two digits would break the shape contract.
        assert_eq!(format_utc_offset(24 * 3600), None);
        assert_eq!(format_utc_offset(-100 * 3600), None);
    }

    #[test]
    fn capture_utc_offset_is_stable_across_retries_of_one_attempt() {
        // `captured_at` is minted once and persisted, so resolving the offset at that instant (not
        // at "now") is what keeps a reused capture id re-serializing to the same manifest.
        let captured_at = "2026-07-25T00:00:00Z";
        let first = capture_utc_offset(captured_at);
        let second = capture_utc_offset(captured_at);
        assert_eq!(first, second);
        if let Some(offset) = first {
            assert_rfc3339_offset_shape(&offset);
        }
        // A `captured_at` that is not RFC3339 yields no key rather than a guess.
        assert_eq!(capture_utc_offset("not a timestamp"), None);
    }

    #[test]
    fn capture_hostname_reuses_the_memory_sync_sanitizer() {
        // The point of sharing is that a machine spells itself one way everywhere. These are the
        // rules memory-sync's own label test pins; asserting them here would be circular if the
        // function were forked, which is exactly why it is not.
        assert_eq!(
            crate::memory_sync::sanitize_machine_label("Alices-MacBook-Pro.local"),
            "alices-macbook-pro"
        );
        assert_eq!(
            crate::memory_sync::sanitize_machine_label("test's MacBook Pro"),
            "test-s-macbook-pro"
        );
        assert_eq!(
            crate::memory_sync::sanitize_machine_label("BOX.Example.COM"),
            "box.example.com"
        );

        // Whatever this machine is called, the capture key is that same sanitized label.
        let expected =
            crate::memory_sync::sanitize_machine_label(&crate::memory_sync::hostname_string());
        if expected.is_empty() {
            assert_eq!(capture_hostname(None), None);
        } else {
            assert_eq!(capture_hostname(None).as_deref(), Some(expected.as_str()));
            // Sanitized means routing-safe: no whitespace, no uppercase, no shell-hostile bytes.
            assert!(
                expected.chars().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || matches!(c, '.' | '_' | '-')),
                "sanitized hostname {expected:?} leaked an unsafe character"
            );
        }
    }

    #[test]
    fn built_manifest_carries_capture_machine_provenance() {
        use tempfile::TempDir;

        // Go through the real construction path — a `SessionRecord` into `capture_source_metadata`
        // into `build_manifest` — so this fails if the wiring is dropped, not just the helpers.
        let directory = TempDir::new().unwrap();
        let transcript_path = directory.path().join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();
        let record = archived_record("11111111-1111-1111-8111-111111111111", &transcript_path);

        let captured_at = "2026-07-25T00:00:00Z";
        let metadata = capture_source_metadata(
            &record,
            captured_at,
            Some("canonical-mac"),
            OriginAccess::Allowed,
        );
        assert_eq!(
            metadata.get("hostname").map(String::as_str),
            Some("canonical-mac")
        );
        let artifacts = prepare_artifacts(vec![ArtifactSource {
            logical_path: "summary.md".to_owned(),
            media_type: Some("text/markdown".to_owned()),
            bytes: b"# Title\n".to_vec(),
        }]);
        let manifest = build_manifest(
            &SessionContext {
                source_agent: "claude-code".to_owned(),
                source_session_id: record.session_id.clone(),
            },
            &CaptureContext {
                captured_at: captured_at.to_owned(),
                source_cursor: Some("1".to_owned()),
                source_state_hash: None,
                source_metadata: metadata,
                project: None,
                repository: None,
                branch: None,
                source_agent_version: None,
                artifact_set_version: CURRENT_ARTIFACT_SET_VERSION,
                munshi_version: Some("0.1.0".to_owned()),
            },
            &artifacts,
        );

        let source_metadata = &manifest["capture"]["source_metadata"];
        let offset = source_metadata["utc_offset"]
            .as_str()
            .expect("capture carries a utc_offset");
        assert_rfc3339_offset_shape(offset);
        let hostname = source_metadata["hostname"]
            .as_str()
            .expect("capture carries a hostname");
        assert_eq!(hostname, "canonical-mac");
        assert_eq!(
            capture_hostname(Some("mac.local")).as_deref(),
            Some("mac.local"),
            "the persisted canonical label must not be sanitized a second time"
        );
        // This record has a live-resolved identity, so `origin` stays absent — the machine keys do
        // not drag an unrelated marker in with them.
        assert!(source_metadata.get("origin").is_none());
    }

    #[test]
    fn capture_metadata_keeps_origin_beside_the_machine_keys() {
        use munshi_transcript::{ProjectIdentity, ProjectOrigin};
        use tempfile::TempDir;

        let directory = TempDir::new().unwrap();
        let transcript_path = directory.path().join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();
        let mut record = archived_record("22222222-2222-2222-8222-222222222222", &transcript_path);
        record.project = Some(ProjectIdentity {
            identity: "github.com/o/r".to_owned(),
            component: "r".to_owned(),
            project: "o/r".to_owned(),
            repository: Some("github.com/o/r".to_owned()),
            branch: Some("main".to_owned()),
            origin: ProjectOrigin::Recorded,
        });

        let metadata =
            capture_source_metadata(&record, "2026-07-25T00:00:00Z", None, OriginAccess::Allowed);
        assert_eq!(metadata.get("origin").map(String::as_str), Some("recorded"));
        assert!(metadata.contains_key("utc_offset"));
        assert!(metadata.contains_key("hostname"));
        // Keys are lowercase snake and values stay short enough to be provenance, not payload.
        for (key, value) in &metadata {
            assert!(
                key.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "metadata key {key:?} is not lowercase snake"
            );
            assert!(value.len() <= 64, "metadata value {value:?} is too long");
        }
    }

    /// The bytes and digest of the two instruction files these tests write, fixed here so a change
    /// to what munshi hashes (the whole file, verbatim, no normalization) fails loudly.
    const CLAUDE_MD_BYTES: &[u8] = b"# Project instructions\n\nAlways run the tests.\n";
    const CLAUDE_MD_SHA256: &str =
        "8640be1f2a16b1498de046498a617406d2f0569951e9437177098f0953a84d03";
    const AGENTS_MD_BYTES: &[u8] = b"agents\n";
    const AGENTS_MD_SHA256: &str =
        "38700dfad5711976e2f7aeab31013f04aed8c83118a1ef892f6d23bdfe944602";

    /// An archived record whose session ran inside `origin_cwd`, which the instruction-provenance
    /// path resolves to a project root. `archived_record` hardcodes `/tmp/project`, so tests that
    /// care about the origin directory point it at their own `TempDir` instead of relying on — or
    /// disturbing — that shared fixture.
    fn record_rooted_at(
        session_id: &str,
        transcript_path: &Path,
        origin_cwd: &Path,
    ) -> SessionRecord {
        let mut record = archived_record(session_id, transcript_path);
        record.origin_cwd = Some(origin_cwd.to_path_buf());
        record
    }

    #[test]
    fn instruction_provenance_hashes_the_file_at_the_project_root() {
        use tempfile::TempDir;

        let project = TempDir::new().unwrap();
        let root = project.path();
        std::fs::write(root.join("CLAUDE.md"), CLAUDE_MD_BYTES).unwrap();
        std::fs::write(root.join("AGENTS.md"), AGENTS_MD_BYTES).unwrap();
        let transcript_path = root.join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();
        let record = record_rooted_at(
            "33333333-3333-3333-8333-333333333333",
            &transcript_path,
            root,
        );

        let provenance = capture_instruction_provenance(&record, OriginAccess::Allowed);
        assert_eq!(
            provenance.get("claude_md").map(String::as_str),
            Some(CLAUDE_MD_SHA256),
            "the digest must be sha256 of the file's exact bytes"
        );
        assert_eq!(
            provenance.get("agents_md").map(String::as_str),
            Some(AGENTS_MD_SHA256)
        );
        // The shape a consumer may rely on: 64 lowercase hex, which also clears the map's own
        // value-length bound.
        for value in provenance.values() {
            assert_eq!(value.len(), 64, "digest {value:?} is not 64 characters");
            assert!(
                value
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f')),
                "digest {value:?} is not lowercase hex"
            );
        }
    }

    #[test]
    fn instruction_provenance_reports_absent_when_the_root_is_readable_and_the_file_is_not_there() {
        use tempfile::TempDir;

        let project = TempDir::new().unwrap();
        let root = project.path();
        std::fs::write(root.join("CLAUDE.md"), CLAUDE_MD_BYTES).unwrap();
        let transcript_path = root.join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();
        let record = record_rooted_at(
            "44444444-4444-4444-8444-444444444444",
            &transcript_path,
            root,
        );

        // `absent` is a positive observation — the root was read and the file provably was not
        // there — and must never be confused with the key simply being missing.
        let provenance = capture_instruction_provenance(&record, OriginAccess::Allowed);
        assert_eq!(
            provenance.get("agents_md").map(String::as_str),
            Some(INSTRUCTION_ABSENT)
        );
        assert_eq!(
            provenance.get("claude_md").map(String::as_str),
            Some(CLAUDE_MD_SHA256),
            "one file being absent says nothing about the other"
        );
    }

    #[test]
    fn instruction_provenance_is_omitted_when_munshi_cannot_look() {
        use tempfile::TempDir;

        let directory = TempDir::new().unwrap();
        let transcript_path = directory.path().join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();

        // No `origin_cwd` at all: codex sessions record none, so they carry no instruction keys.
        let mut record = archived_record("55555555-5555-5555-8555-555555555555", &transcript_path);
        record.origin_cwd = None;
        assert!(
            capture_instruction_provenance(&record, OriginAccess::Allowed).is_empty(),
            "a session with no recorded origin must report nothing, not `absent`"
        );

        // An origin that no longer resolves to a directory: the root is unknown, so which
        // `CLAUDE.md` to speak about is unknown too.
        let vanished = directory.path().join("gone");
        record.origin_cwd = Some(vanished);
        assert!(
            capture_instruction_provenance(&record, OriginAccess::Allowed).is_empty(),
            "an unresolvable root must report nothing, not `absent`"
        );
    }

    #[test]
    fn withheld_origin_access_records_no_instruction_provenance() {
        use tempfile::TempDir;

        let project = TempDir::new().unwrap();
        let root = project.path();
        std::fs::write(root.join("CLAUDE.md"), CLAUDE_MD_BYTES).unwrap();
        std::fs::write(root.join("AGENTS.md"), AGENTS_MD_BYTES).unwrap();
        let transcript_path = root.join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();
        let record = record_rooted_at(
            "66666666-6666-6666-8666-666666666666",
            &transcript_path,
            root,
        );

        // Issue #61: a scheduler-descended worker must not touch the origin directory, so both keys
        // vanish even though both files are sitting right there and readable.
        //
        // What this pins is the *output* — an empty map — not the absence of filesystem side
        // effects, which no assertion here can observe. The no-touch property rests on the
        // `Withheld` guard being the first statement of `capture_instruction_provenance`, ahead of
        // every path that could stat, canonicalize, or shell out to git. Read that guard, not this
        // test, to check the property.
        assert!(
            capture_instruction_provenance(&record, OriginAccess::Withheld).is_empty(),
            "withheld origin access must yield no instruction keys at all"
        );

        // And nothing else in the capture is collateral damage: the machine keys are ambient state,
        // not origin state, so they are unaffected by the withholding.
        let withheld = capture_source_metadata(
            &record,
            "2026-07-25T00:00:00Z",
            Some("mac"),
            OriginAccess::Withheld,
        );
        assert_eq!(withheld.get("hostname").map(String::as_str), Some("mac"));
        assert!(withheld.contains_key("utc_offset"));
        assert!(!withheld.contains_key("claude_md"));
        assert!(!withheld.contains_key("agents_md"));

        let allowed = capture_source_metadata(
            &record,
            "2026-07-25T00:00:00Z",
            Some("mac"),
            OriginAccess::Allowed,
        );
        assert_eq!(
            allowed.get("claude_md").map(String::as_str),
            Some(CLAUDE_MD_SHA256),
            "the same record under allowed access does report the digests"
        );
        assert_eq!(allowed.get("hostname"), withheld.get("hostname"));
        assert_eq!(allowed.get("utc_offset"), withheld.get("utc_offset"));
    }

    #[test]
    fn instruction_provenance_refuses_a_symlinked_instruction_file() {
        use tempfile::TempDir;

        let project = TempDir::new().unwrap();
        let root = project.path();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("shared-CLAUDE.md");
        std::fs::write(&target, CLAUDE_MD_BYTES).unwrap();
        std::os::unix::fs::symlink(&target, root.join("CLAUDE.md")).unwrap();
        let transcript_path = root.join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();
        let record = record_rooted_at(
            "77777777-7777-7777-8777-777777777777",
            &transcript_path,
            root,
        );

        // Deliberate: a digest of a file living outside the project would be read as a statement
        // about this project's instructions, with nothing in the record to reveal otherwise. Worse
        // than no provenance, so the key is omitted — and it is *not* `absent`, because the path
        // does exist.
        let provenance = capture_instruction_provenance(&record, OriginAccess::Allowed);
        assert!(
            !provenance.contains_key("claude_md"),
            "a symlinked instruction file must be omitted, got {provenance:?}"
        );

        // A directory at the same path is refused the same way, and for the same reason.
        std::fs::create_dir(root.join("AGENTS.md")).unwrap();
        let provenance = capture_instruction_provenance(&record, OriginAccess::Allowed);
        assert!(
            !provenance.contains_key("agents_md"),
            "a directory named AGENTS.md must be omitted, got {provenance:?}"
        );
    }

    #[test]
    fn instruction_provenance_omits_an_oversize_instruction_file() {
        use tempfile::TempDir;

        let project = TempDir::new().unwrap();
        let root = project.path();
        std::fs::write(
            root.join("CLAUDE.md"),
            vec![b'x'; (MAX_INSTRUCTION_BYTES + 1) as usize],
        )
        .unwrap();
        let transcript_path = root.join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();
        let record = record_rooted_at(
            "88888888-8888-8888-8888-888888888888",
            &transcript_path,
            root,
        );

        // Over the cap the key is omitted, never `absent`: the file exists, munshi declined to read
        // it. Reporting `absent` here would tell the doctor an instruction file was deleted.
        let provenance = capture_instruction_provenance(&record, OriginAccess::Allowed);
        assert!(
            !provenance.contains_key("claude_md"),
            "an oversize instruction file must be omitted, got {provenance:?}"
        );

        // Exactly at the cap is still read: the bound is inclusive.
        std::fs::write(
            root.join("CLAUDE.md"),
            vec![b'x'; MAX_INSTRUCTION_BYTES as usize],
        )
        .unwrap();
        let provenance = capture_instruction_provenance(&record, OriginAccess::Allowed);
        assert_eq!(
            provenance.get("claude_md").map(String::len),
            Some(64),
            "a file exactly at the cap is hashed"
        );
    }

    #[test]
    fn hash_bounded_file_re_checks_the_size_it_actually_read() {
        use tempfile::TempDir;

        // The test above goes through `instruction_file_state`, so the oversize file is refused by
        // the `stat` gate and the read-side re-check never runs. In production that branch is
        // reachable only by a file growing past the cap between the `stat` and the read, so it is
        // pinned here directly — otherwise the one line standing between a racing file and an
        // unbounded hash would have no coverage at all.
        let directory = TempDir::new().unwrap();
        let oversize = directory.path().join("oversize.md");
        std::fs::write(&oversize, vec![b'x'; (MAX_INSTRUCTION_BYTES + 1) as usize]).unwrap();
        assert_eq!(
            hash_bounded_file(&oversize),
            None,
            "a file over the cap must be refused by the read-side check, not hashed in part"
        );

        // The boundary itself is inclusive, and the digest is the real one.
        let at_cap = directory.path().join("at-cap.md");
        std::fs::write(&at_cap, vec![b'x'; MAX_INSTRUCTION_BYTES as usize]).unwrap();
        assert_eq!(
            hash_bounded_file(&at_cap),
            Some(sha256_hex(&vec![b'x'; MAX_INSTRUCTION_BYTES as usize]))
        );

        // And an ordinary file hashes to the digest the constants pin, so the streaming loop is not
        // quietly dropping or duplicating a chunk.
        let small = directory.path().join("CLAUDE.md");
        std::fs::write(&small, CLAUDE_MD_BYTES).unwrap();
        assert_eq!(hash_bounded_file(&small).as_deref(), Some(CLAUDE_MD_SHA256));

        // An unreadable path is `None`, never an error: provenance must not fail an upload.
        assert_eq!(
            hash_bounded_file(&directory.path().join("missing.md")),
            None
        );
    }

    #[test]
    fn instruction_provenance_is_rehashed_on_every_attempt() {
        use tempfile::TempDir;

        let project = TempDir::new().unwrap();
        let root = project.path();
        std::fs::write(root.join("CLAUDE.md"), CLAUDE_MD_BYTES).unwrap();
        let transcript_path = root.join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();
        let record = record_rooted_at(
            "99999999-9999-9999-8999-999999999999",
            &transcript_path,
            root,
        );

        // Same record, same `captured_at` — the pair `capture_utc_offset` is deterministic in.
        let captured_at = "2026-07-25T00:00:00Z";
        let first =
            capture_source_metadata(&record, captured_at, Some("mac"), OriginAccess::Allowed);
        assert_eq!(
            first.get("claude_md").map(String::as_str),
            Some(CLAUDE_MD_SHA256)
        );

        std::fs::write(
            root.join("CLAUDE.md"),
            b"# Project instructions\n\nEdited.\n",
        )
        .unwrap();
        let second =
            capture_source_metadata(&record, captured_at, Some("mac"), OriginAccess::Allowed);

        // The chosen semantics, pinned: instruction provenance is ambient state read at attempt
        // time (like `hostname`), not a pure function of `captured_at` (like `utc_offset`). An edit
        // between two attempts of one capture id changes the value, and that is the honest answer —
        // the digest is evidence about the working tree, and the freshest reading is the truest.
        assert_ne!(
            first.get("claude_md"),
            second.get("claude_md"),
            "an instruction edit between attempts must be reflected"
        );
        assert_eq!(
            first.get("utc_offset"),
            second.get("utc_offset"),
            "the offset stays fixed by `captured_at`, unlike the instruction digests"
        );
    }

    #[test]
    fn built_manifest_carries_instruction_provenance() {
        use tempfile::TempDir;

        // The full construction path again — a `SessionRecord` through `capture_source_metadata`
        // into `build_manifest` — so the round trip fails if the wiring is ever dropped.
        let project = TempDir::new().unwrap();
        let root = project.path();
        std::fs::write(root.join("CLAUDE.md"), CLAUDE_MD_BYTES).unwrap();
        let transcript_path = root.join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{\"type\":\"user\"}\n").unwrap();
        let record = record_rooted_at(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            &transcript_path,
            root,
        );

        let captured_at = "2026-07-25T00:00:00Z";
        let artifacts = prepare_artifacts(vec![ArtifactSource {
            logical_path: "summary.md".to_owned(),
            media_type: Some("text/markdown".to_owned()),
            bytes: b"# Title\n".to_vec(),
        }]);
        let manifest = build_manifest(
            &SessionContext {
                source_agent: "claude-code".to_owned(),
                source_session_id: record.session_id.clone(),
            },
            &CaptureContext {
                captured_at: captured_at.to_owned(),
                source_cursor: Some("1".to_owned()),
                source_state_hash: None,
                source_metadata: capture_source_metadata(
                    &record,
                    captured_at,
                    Some("canonical-mac"),
                    OriginAccess::Allowed,
                ),
                project: None,
                repository: None,
                branch: None,
                source_agent_version: None,
                artifact_set_version: CURRENT_ARTIFACT_SET_VERSION,
                munshi_version: Some("0.1.0".to_owned()),
            },
            &artifacts,
        );

        let source_metadata = &manifest["capture"]["source_metadata"];
        assert_eq!(
            source_metadata["claude_md"].as_str(),
            Some(CLAUDE_MD_SHA256),
            "the digest survives manifest assembly verbatim"
        );
        assert_eq!(
            source_metadata["agents_md"].as_str(),
            Some(INSTRUCTION_ABSENT)
        );
        // Nothing but the hash travels: the manifest carries no instruction-file content anywhere.
        let serialized = serde_json::to_string(&manifest).unwrap();
        assert!(
            !serialized.contains("Always run the tests"),
            "instruction file content must never reach the manifest"
        );
    }

    #[test]
    fn staged_sidecars_round_trip_into_canonical_artifact_order() {
        use tempfile::TempDir;

        let output_directory = TempDir::new().unwrap();
        let markdown_relative = Path::new("component/sess-1.md");
        let staged = vec![
            SidecarFile {
                relative_path: "workspace.yaml".to_owned(),
                bytes: b"cwd: /work\n".to_vec(),
            },
            SidecarFile {
                relative_path: "checkpoints/index.md".to_owned(),
                bytes: b"# checkpoint\n".to_vec(),
            },
        ];
        crate::render::stage_sidecar_files(output_directory.path(), markdown_relative, &staged)
            .unwrap();
        // A symlinked entry dropped into the staged directory is not ours and is never uploaded.
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            "/etc/hosts",
            output_directory
                .path()
                .join("component/sess-1.sidecar/evil.md"),
        )
        .unwrap();

        let read = read_staged_sidecars(output_directory.path(), Some(markdown_relative));
        assert_eq!(read, {
            let mut sorted = staged.clone();
            sorted.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            sorted
        });

        // Assembly places sidecars under `sidecar/…` in canonical logical-path order with
        // extension-derived media types.
        let sources = assemble_artifact_sources(
            Some(b"# Summary\n".to_vec()),
            Some(b"{}\n".to_vec()),
            SourceKind::Copilot,
            64,
            read,
        );
        let paths: Vec<&str> = sources
            .iter()
            .map(|source| source.logical_path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec![
                "sidecar/checkpoints/index.md",
                "sidecar/workspace.yaml",
                "summary.md",
                "transcript.jsonl",
            ]
        );
        assert_eq!(
            sources[0].media_type.as_deref(),
            Some("text/markdown"),
            "markdown sidecar media type"
        );
        assert_eq!(
            sources[1].media_type.as_deref(),
            Some("application/yaml"),
            "yaml sidecar media type"
        );

        // Re-staging an empty set removes the directory: the snapshot set never unions revisions.
        crate::render::stage_sidecar_files(output_directory.path(), markdown_relative, &[])
            .unwrap();
        assert!(read_staged_sidecars(output_directory.path(), Some(markdown_relative)).is_empty());
        assert!(read_staged_sidecars(output_directory.path(), None).is_empty());
    }

    #[test]
    fn chunking_splits_stored_bytes_with_a_smaller_tail() {
        let bytes: Vec<u8> = (0..250).map(|value| value as u8).collect();
        assert_eq!(chunk_bytes(&bytes, 100, 0).unwrap().len(), 100);
        assert_eq!(chunk_bytes(&bytes, 100, 1).unwrap().len(), 100);
        assert_eq!(chunk_bytes(&bytes, 100, 2).unwrap().len(), 50);
        // An empty artifact has exactly one empty chunk at index 0.
        assert_eq!(chunk_bytes(b"", 100, 0).unwrap().len(), 0);
    }

    #[test]
    fn chunk_bytes_is_total_for_phantom_and_out_of_range_indexes() {
        // Regression: an empty artifact with a phantom index 1 must error, not panic on a `[100..0]`
        // slice (the previous guard skipped the empty-input case).
        assert!(matches!(
            chunk_bytes(b"", 100, 1),
            Err(PatwariError::Protocol(_))
        ));
        // A non-empty artifact with an index past its final chunk errors rather than slicing out of
        // bounds. 150 bytes over a 100-byte layout has chunks 0 and 1 only; index 2 is past it.
        let bytes: Vec<u8> = (0..150).map(|value| value as u8).collect();
        assert_eq!(chunk_bytes(&bytes, 100, 1).unwrap().len(), 50);
        assert!(matches!(
            chunk_bytes(&bytes, 100, 2),
            Err(PatwariError::Protocol(_))
        ));
    }

    #[test]
    fn chunk_count_mirrors_server_semantics() {
        // Empty artifact: zero chunks, matching the server's `chunk_count` (ingestion.rs).
        assert_eq!(chunk_count(0, 100), 0);
        assert_eq!(chunk_count(1, 100), 1);
        assert_eq!(chunk_count(100, 100), 1);
        assert_eq!(chunk_count(101, 100), 2);
        assert_eq!(chunk_count(250, 100), 3);
        // A zero chunk size (rejected elsewhere) yields zero rather than dividing by zero.
        assert_eq!(chunk_count(100, 0), 0);
    }

    #[test]
    fn new_uuid_is_a_v4_shape() {
        let uuid = new_uuid();
        assert_eq!(uuid.len(), 36);
        let parts: Vec<&str> = uuid.split('-').collect();
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "version nibble is 4: {uuid}");
        assert!(uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_ne!(uuid, new_uuid(), "uuids are unique");
    }

    #[test]
    fn backoff_is_bounded_and_monotonic() {
        assert_eq!(backoff_ms(1), BASE_BACKOFF_MS);
        assert_eq!(backoff_ms(2), BASE_BACKOFF_MS * 2);
        assert!(backoff_ms(3) > backoff_ms(2));
        assert_eq!(backoff_ms(100), MAX_BACKOFF_MS);
    }

    #[test]
    fn chunk_routing_rejects_unknown_repeated_and_duplicated_logical_paths() {
        // Endpoint parsing succeeds but nothing listens on port 1; every failure below is raised
        // before any network I/O, proving the path checks front-run the chunk PUTs.
        let client = PatwariClient::connect("http://127.0.0.1:1", "client").unwrap();
        let source = |path: &str, bytes: &[u8]| ArtifactSource {
            logical_path: path.to_owned(),
            media_type: None,
            bytes: bytes.to_vec(),
        };
        let entry = |path: &str| ArtifactStatus {
            logical_path: path.to_owned(),
            artifact_index: 0,
            missing_chunk_indexes: Vec::new(),
        };
        let session = |entries: Vec<ArtifactStatus>| UploadSession {
            upload_id: "upl-00000000".to_owned(),
            capture_id: "capture".to_owned(),
            chunk_size_bytes: 8,
            artifacts: entries,
        };
        let artifacts = prepare_artifacts(vec![
            source("transcript.jsonl", b"{}\n"),
            source("summary.md", b"# s\n"),
        ]);

        // A server path that is not part of this snapshot is a protocol error naming the path.
        let error = client
            .upload_missing_chunks(&session(vec![entry("outputs/feed")]), &artifacts)
            .unwrap_err();
        assert!(
            matches!(&error, PatwariError::Protocol(message) if message.contains("outputs/feed")),
            "got {error:?}"
        );

        // A repeated server path is a protocol error naming the path.
        let error = client
            .upload_missing_chunks(
                &session(vec![entry("summary.md"), entry("summary.md")]),
                &artifacts,
            )
            .unwrap_err();
        assert!(
            matches!(&error, PatwariError::Protocol(message)
                if message.contains("repeated") && message.contains("summary.md")),
            "got {error:?}"
        );

        // A duplicated local path is rejected up front, before any server entry is consulted.
        let duplicated =
            prepare_artifacts(vec![source("summary.md", b"a"), source("summary.md", b"b")]);
        let error = client
            .upload_missing_chunks(&session(Vec::new()), &duplicated)
            .unwrap_err();
        assert!(
            matches!(&error, PatwariError::Protocol(message) if message.contains("summary.md")),
            "got {error:?}"
        );
    }

    #[test]
    fn assembled_artifact_sources_are_in_canonical_path_order() {
        // `outputs/…` sorts before the fixed roles; the assembled list is canonical (issue #33).
        let transcript = format!(
            "{{\"id\":\"call-1\",\"timestamp\":\"2026-07-25T00:00:00Z\",\"parentId\":\"root\",\
             \"type\":\"tool.execution_complete\",\"data\":{{\"toolCallId\":\"call-1\",\
             \"success\":true,\"result\":{{\"content\":\"{}\"}}}}}}\n",
            "x".repeat(500)
        )
        .into_bytes();
        let sources = assemble_artifact_sources(
            Some(b"# Summary\n".to_vec()),
            Some(transcript),
            SourceKind::Copilot,
            64,
            Vec::new(),
        );
        let paths: Vec<&str> = sources
            .iter()
            .map(|source| source.logical_path.as_str())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted, "assembly order is sorted by logical path");
        assert!(
            paths[0].starts_with("outputs/") && paths.contains(&"summary.md"),
            "the extracted output was assembled and sorts first: {paths:?}"
        );
    }

    #[test]
    fn error_code_reads_patwari_error_bodies() {
        let body = br#"{"error":{"code":"capture_id_conflict","message":"x"}}"#;
        assert_eq!(error_code(body).as_deref(), Some("capture_id_conflict"));
        assert_eq!(error_code(b"not json"), None);
    }

    #[test]
    fn stale_endpoint_note_fires_only_when_a_named_session_has_rows_elsewhere_only() {
        let row = |session_id: &str, endpoint: &str| ArchiveUploadRecord {
            session_database_id: 1,
            source: SourceKind::Copilot,
            session_id: session_id.to_owned(),
            endpoint: endpoint.to_owned(),
            capture_id: None,
            capture_revision: None,
            captured_at: None,
            upload_id: None,
            uploaded_revision: None,
            uploaded_summary_hash: None,
            uploaded_markdown_hash: None,
            patwari_session_id: None,
            snapshot_id: None,
            uploaded_artifact_paths: None,
            upload_state: "dead-letter".to_owned(),
            attempts: 5,
            next_attempt_at_ms: None,
            last_error_category: Some("transcript-changed".to_owned()),
            updated_at_ms: 0,
            transfer_bytes_total: 0,
            last_stored_bytes: None,
            last_original_bytes: None,
        };
        let configured = "https://patwari.example";
        let stale = "http://127.0.0.1:18787";

        // The issue #54 shape: history exists only under the pre-reconfiguration endpoint.
        let note = stale_endpoint_note(&[row("s1", stale)], configured, "s1", None)
            .expect("stale-only history must produce the note");
        assert!(note.contains(stale), "{note}");
        assert!(note.contains("archive-upload backfill"), "{note}");

        // A row under the configured endpoint means retry owns the session: no note.
        assert_eq!(
            stale_endpoint_note(
                &[row("s1", stale), row("s1", configured)],
                configured,
                "s1",
                None
            ),
            None
        );
        // Other sessions' rows never speak for the named one.
        assert_eq!(
            stale_endpoint_note(&[row("other", stale)], configured, "s1", None),
            None
        );
        // A source narrowing excludes rows from other sources.
        assert_eq!(
            stale_endpoint_note(
                &[row("s1", stale)],
                configured,
                "s1",
                Some(SourceKind::ClaudeCode)
            ),
            None
        );
    }

    #[test]
    fn records_full_snapshot_requires_every_required_path_to_be_recorded() {
        let row = |paths: Option<Vec<&str>>| ArchiveUploadRecord {
            session_database_id: 1,
            source: SourceKind::Copilot,
            session_id: "s".to_owned(),
            endpoint: "http://127.0.0.1:1".to_owned(),
            capture_id: None,
            capture_revision: None,
            captured_at: None,
            upload_id: None,
            uploaded_revision: Some(1),
            uploaded_summary_hash: Some("hash".to_owned()),
            uploaded_markdown_hash: Some("md-hash".to_owned()),
            patwari_session_id: Some("patwari-session".to_owned()),
            snapshot_id: Some("snap-1".to_owned()),
            uploaded_artifact_paths: paths
                .map(|paths| paths.into_iter().map(ToOwned::to_owned).collect()),
            upload_state: "uploaded".to_owned(),
            attempts: 0,
            next_attempt_at_ms: None,
            last_error_category: None,
            updated_at_ms: 0,
            transfer_bytes_total: 0,
            last_stored_bytes: None,
            last_original_bytes: None,
        };
        // A row written before the ledger recorded artifact sets proves nothing.
        assert!(!records_full_snapshot(&row(None)));
        // The summary-only snapshots issue #47 fixes.
        assert!(!records_full_snapshot(&row(Some(vec!["summary.md"]))));
        assert!(!records_full_snapshot(&row(Some(vec!["transcript.jsonl"]))));
        assert!(records_full_snapshot(&row(Some(vec![
            "summary.md",
            "transcript.jsonl"
        ]))));
        // Extracted outputs ride along with the transcript and never change the verdict.
        assert!(records_full_snapshot(&row(Some(vec![
            "outputs/abc",
            "summary.md",
            "transcript.jsonl"
        ]))));
    }

    #[test]
    fn upload_one_skips_rather_than_uploading_a_snapshot_without_its_transcript() {
        use crate::registration::StoredArchiveUpload;
        use crate::source::DEFAULT_MAX_EVENT_TEXT_BYTES;
        use tempfile::TempDir;

        let directory = TempDir::new().unwrap();
        let state_dir = directory.path().join("home");
        let output_dir = directory.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("summary.md"), b"# Title\n\nBody.\n").unwrap();

        let session_id = "44444444-4444-4444-8444-444444444445";
        let endpoint = "http://127.0.0.1:1";
        let transcript_path = directory.path().join("transcript.jsonl");
        let mut store = StateStore::open(&state_dir).unwrap();
        store
            .ingest_agent_stop(
                session_id,
                10_000,
                Path::new("/tmp/project"),
                &transcript_path,
            )
            .unwrap();
        let settings = StoredArchiveUpload {
            enabled: true,
            endpoint: Some(endpoint.to_owned()),
            client_id: Some("client".to_owned()),
            max_attempts: 5,
        };

        // A `rebuild-state` row knows its summary but never learns a transcript path; a session
        // whose transcript the harness has since removed reads the same way. Both must skip rather
        // than upload the summary alone — nothing below ever reaches the (dead) endpoint.
        for transcript in [None, Some(transcript_path.clone())] {
            let mut record = archived_record(session_id, &transcript_path);
            record.transcript_path = transcript;
            let outcome = upload_one(
                &mut store,
                &settings,
                "client",
                endpoint,
                None,
                &output_dir,
                &record,
                &SourceHomes::default(),
                DEFAULT_MAX_EVENT_TEXT_BYTES,
                OriginAccess::Allowed,
            )
            .unwrap();
            assert!(
                matches!(&outcome, UploadOutcome::Skipped { reason }
                    if reason == "missing-transcript.jsonl"),
                "got {outcome:?}"
            );
        }
        // The skip is not an attempt: no bounded retry budget is spent and nothing dead-letters.
        let recorded = store
            .get_archive_upload(session_id, endpoint)
            .unwrap()
            .unwrap();
        assert_eq!(recorded.upload_state, "pending");
        assert_eq!(recorded.attempts, 0);
    }

    #[test]
    fn upload_one_reuploads_a_cursor_only_rerender_but_no_ops_an_identical_snapshot() {
        use crate::registration::StoredArchiveUpload;
        use crate::source::DEFAULT_MAX_EVENT_TEXT_BYTES;
        use tempfile::TempDir;

        let directory = TempDir::new().unwrap();
        let state_dir = directory.path().join("home");
        let output_dir = directory.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("summary.md"), b"# Title\n\nBody.\n").unwrap();

        let session_id = "55555555-5555-4555-8555-555555555555";
        let endpoint = "http://127.0.0.1:1";
        let transcript_path = directory.path().join("transcript.jsonl");
        std::fs::write(&transcript_path, b"{}\n").unwrap();
        let mut store = StateStore::open(&state_dir).unwrap();
        store
            .ingest_agent_stop(
                session_id,
                10_000,
                Path::new("/tmp/project"),
                &transcript_path,
            )
            .unwrap();
        let settings = StoredArchiveUpload {
            enabled: true,
            endpoint: Some(endpoint.to_owned()),
            client_id: Some("client".to_owned()),
            max_attempts: 5,
        };

        // Seed a recorded, self-contained upload at revision 1 whose markdown hash is "md-hash" —
        // exactly what `archived_record` reports for this session.
        store
            .ensure_archive_upload_target(session_id, endpoint)
            .unwrap();
        store
            .record_archive_upload_success(
                session_id,
                endpoint,
                &ArchiveUploadSuccess {
                    uploaded_revision: 1,
                    uploaded_summary_hash: "summary-hash".to_owned(),
                    uploaded_markdown_hash: Some("md-hash".to_owned()),
                    snapshot_id: "snap-1".to_owned(),
                    patwari_session_id: "patwari-session".to_owned(),
                    uploaded_artifact_paths: vec![
                        "summary.md".to_owned(),
                        "transcript.jsonl".to_owned(),
                    ],
                    transfer_bytes: 1,
                    total_stored_bytes: 1,
                    total_original_bytes: 1,
                },
            )
            .unwrap();

        let upload = |store: &mut StateStore, record: &SessionRecord| {
            upload_one(
                store,
                &settings,
                "client",
                endpoint,
                None,
                &output_dir,
                record,
                &SourceHomes::default(),
                DEFAULT_MAX_EVENT_TEXT_BYTES,
                OriginAccess::Allowed,
            )
            .unwrap()
        };

        // Same revision, summary, and markdown: an idempotent no-op that never touches the (dead)
        // server — proven by the outcome, which is decided before any network work.
        let outcome = upload(&mut store, &archived_record(session_id, &transcript_path));
        assert!(
            matches!(outcome, UploadOutcome::AlreadyUploaded { revision: 1, .. }),
            "an unchanged snapshot must short-circuit, got {outcome:?}"
        );

        // A cursor-only re-render (hooks.rs `cursor_only`) leaves the revision and summary unchanged
        // but rewrites the markdown; the new hash must break the short-circuit so the fresh markdown
        // reaches the archive rather than the snapshot silently lagging the local file.
        let mut rerendered = archived_record(session_id, &transcript_path);
        rerendered.markdown_hash = Some("md-hash-after-cursor-move".to_owned());
        let outcome = upload(&mut store, &rerendered);
        assert!(
            !matches!(outcome, UploadOutcome::AlreadyUploaded { .. }),
            "a cursor-only re-render must not be mistaken for an already-uploaded snapshot, got {outcome:?}"
        );
    }

    /// An archived revision-1 record whose summary is `summary.md` under the output directory and
    /// whose archived source identity is the transcript at `transcript_path` as it reads right now.
    fn archived_record(session_id: &str, transcript_path: &Path) -> SessionRecord {
        let bytes = std::fs::read(transcript_path).unwrap_or_default();
        let source_hash = prefixed_digest(&sha256_hex(&bytes));
        SessionRecord {
            database_id: 0,
            source: SourceKind::Copilot,
            session_id: session_id.to_owned(),
            origin_cwd: Some(PathBuf::from("/tmp/project")),
            project: None,
            transcript_path: Some(transcript_path.to_path_buf()),
            lifecycle_state: "archived".to_owned(),
            completion_reason: Some("complete".to_owned()),
            source_end_reason: None,
            current_revision: 1,
            current_summary: None,
            current_summary_hash: Some("summary-hash".to_owned()),
            markdown_relative_path: Some(PathBuf::from("summary.md")),
            markdown_hash: Some("md-hash".to_owned()),
            previous_source: Some(PreviousSource {
                normalizer_version: crate::source::NORMALIZER_VERSION,
                record_count: 1,
                byte_offset: bytes.len() as u64,
                prefix_hash: source_hash.clone(),
                source_hash,
                source_bytes: bytes.len() as u64,
                started_at: None,
                updated_at: None,
                user_requests: 1,
                assistant_messages: 0,
                tool_activities: 0,
            }),
            fallback_reason: None,
            state_generation: 1,
            active: false,
            last_agent_stop_ms: Some(10_000),
            last_session_end_ms: None,
            not_archive_worthy_at_ms: None,
            transcript_lost_at_ms: None,
            last_error_category: None,
            next_retry_at_ms: None,
            failure_streak: 0,
            created_at_ms: 10_000,
            updated_at_ms: 10_000,
        }
    }

    #[test]
    fn upload_one_fails_retryably_when_transcript_changes_under_it() {
        use crate::registration::StoredArchiveUpload;
        use crate::source::DEFAULT_MAX_EVENT_TEXT_BYTES;
        use tempfile::TempDir;

        let directory = TempDir::new().unwrap();
        let state_dir = directory.path().join("home");
        let output_dir = directory.path().join("out");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("summary.md"), b"# Title\n\nBody.\n").unwrap();

        let transcript_path = directory.path().join("transcript.jsonl");
        let original = b"{\"type\":\"user\",\"data\":{\"text\":\"hello\"}}\n";
        std::fs::write(&transcript_path, original).unwrap();

        let session_id = "44444444-4444-4444-8444-444444444444";
        let endpoint = "http://127.0.0.1:1";
        let mut store = StateStore::open(&state_dir).unwrap();
        store
            .ingest_agent_stop(
                session_id,
                10_000,
                Path::new("/tmp/project"),
                &transcript_path,
            )
            .unwrap();
        // The archived revision's `source_hash` is the normalization-time hash of these exact bytes.
        let record = archived_record(session_id, &transcript_path);

        // Tamper the live transcript after archival: it grows under the upload.
        std::fs::write(
            &transcript_path,
            b"{\"type\":\"user\",\"data\":{\"text\":\"hello again\"}}\n",
        )
        .unwrap();

        let settings = StoredArchiveUpload {
            enabled: true,
            endpoint: Some(endpoint.to_owned()),
            client_id: Some("client".to_owned()),
            max_attempts: 5,
        };
        // The transcript check fails before any network use, so no server is contacted.
        let outcome = upload_one(
            &mut store,
            &settings,
            "client",
            endpoint,
            None,
            &output_dir,
            &record,
            &SourceHomes::default(),
            DEFAULT_MAX_EVENT_TEXT_BYTES,
            OriginAccess::Allowed,
        )
        .unwrap();
        assert!(matches!(
            &outcome,
            UploadOutcome::Failed { category, dead_letter: false } if category == "transcript-changed"
        ));
        // The failure is recorded as a retryable (scheduled) attempt, not a dead letter.
        let recorded = store
            .get_archive_upload(session_id, endpoint)
            .unwrap()
            .unwrap();
        assert_eq!(recorded.upload_state, "failed");
        assert_eq!(recorded.attempts, 1);
        assert!(recorded.next_attempt_at_ms.is_some());
    }
}
