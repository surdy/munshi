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
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::http::{self, Header, HttpError};
use crate::registration::{
    DEFAULT_MAX_ARCHIVE_UPLOAD_ATTEMPTS, RegistrationError, StoredConfig, load_stored_config,
    stored_config_exists, update_stored_config,
};
use crate::source::SourceKind;
use crate::state::{
    ArchiveUploadRecord, ArchiveUploadSuccess, SessionRecord, StateError, StateStore, now_ms,
    try_acquire_session_lock,
};

/// The API base path every Patwari route is nested under.
const API_BASE: &str = "/api/v1";
/// The current manifest schema version Patwari accepts.
const MANIFEST_SCHEMA_VERSION: u16 = 1;
/// The initial snapshot artifact-set version. Issue #20 will add artifact kinds under a bumped
/// version; this is exposed so manifest assembly can be extended without changing the wire shape.
pub const INITIAL_ARTIFACT_SET_VERSION: u16 = 1;
/// Custom chunk headers Patwari requires on each artifact chunk PUT.
const CHUNK_SHA256_HEADER: &str = "x-patwari-chunk-sha256";
const CHUNK_LENGTH_HEADER: &str = "x-patwari-chunk-length";
/// Base backoff between failed upload attempts; doubles per attempt up to [`MAX_BACKOFF_MS`].
const BASE_BACKOFF_MS: i64 = 60_000;
/// Upper bound on upload backoff so a long outage still retries roughly hourly.
const MAX_BACKOFF_MS: i64 = 3_600_000;
/// Network timeout for a single upload request. Larger than delivery's: chunk bodies are bigger.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
    let artifacts_json: Vec<Value> = artifacts
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
        .collect();
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
    host: String,
    port: u16,
    client_id: String,
    timeout: Duration,
}

impl PatwariClient {
    /// Connects a client for `endpoint` (`http://host[:port]`) uploading under `client_id`.
    pub fn connect(endpoint: &str, client_id: &str) -> Result<Self, PatwariError> {
        let (host, port) = http::parse_http_endpoint(endpoint).map_err(from_http)?;
        Ok(Self {
            host,
            port,
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
            &self.host,
            self.port,
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
        let response = self.send_json("POST", &path, None)?;
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
    ))
}

/// Assembles the ordered snapshot artifact set v1 (ADR 0009/0010) from already-read bytes:
/// `summary.md` (this revision's rendered summary), `transcript.jsonl` (the verbatim source bytes),
/// and every re-derived `outputs/<sha256>` extracted output.
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
) -> Vec<ArtifactSource> {
    let mut sources = Vec::new();
    if let Some(summary) = summary_md {
        sources.push(ArtifactSource {
            logical_path: "summary.md".to_owned(),
            media_type: Some("text/markdown".to_owned()),
            bytes: summary,
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
    // Canonicalize: Patwari sorts `artifacts[]` by logical path (`outputs/…` before `summary.md`
    // before `transcript.jsonl`). Logical paths are unique here (fixed roles plus content-addressed
    // outputs), so the sort is a total order and stable across retries.
    sources.sort_by(|a, b| a.logical_path.cmp(&b.logical_path));
    sources
}

/// Uploads one freshly archived summary revision to Patwari, invoked by the archive worker
/// downstream of a successful local archive and Notesmith delivery.
///
/// This never mutates the session's archival lifecycle. It returns `Ok(None)` when archive upload
/// is disabled or unconfigured, records network/server failures as a bounded retry, and reuses the
/// persisted capture id (resuming an interrupted upload) or mints a fresh one for a new revision.
pub(crate) fn upload_after_archive(
    state: &mut StateStore,
    config: &StoredConfig,
    session_id: &str,
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
        &output_directory,
        &record,
        config.limits.max_event_text_bytes,
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
pub(crate) fn retry_pending_uploads(
    state_directory: &Path,
    limit: usize,
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
            &output_directory,
            &session,
            config.limits.max_event_text_bytes,
        )?;
    }
    Ok(())
}

/// Uploads one session's current snapshot to the configured server, recording the result in
/// operational state. Never mutates the session's archival lifecycle.
pub(crate) fn upload_one(
    state: &mut StateStore,
    settings: &crate::registration::StoredArchiveUpload,
    client_id: &str,
    endpoint: &str,
    output_directory: &Path,
    record: &SessionRecord,
    max_event_text_bytes: usize,
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
    // Already uploaded this exact revision as a self-contained snapshot: idempotent no-op that never
    // contacts the server. A recorded snapshot that is not known self-contained (issue #47) falls
    // through and re-uploads the complete set even though the revision and summary hash match.
    if existing.upload_state == "uploaded"
        && existing.uploaded_revision == Some(record.current_revision)
        && existing.uploaded_summary_hash == record.current_summary_hash
        && record.current_summary_hash.is_some()
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
    let sources = match collect_artifacts(output_directory, record, max_event_text_bytes) {
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

    match run_upload(state, client_id, endpoint, record, &artifacts) {
        Ok(receipt) => {
            state.record_archive_upload_success(
                &record.session_id,
                endpoint,
                &ArchiveUploadSuccess {
                    uploaded_revision: record.current_revision,
                    uploaded_summary_hash: record.current_summary_hash.clone().unwrap_or_default(),
                    snapshot_id: receipt.snapshot_id.clone(),
                    uploaded_artifact_paths: artifacts
                        .iter()
                        .map(|artifact| artifact.logical_path.clone())
                        .collect(),
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

/// Performs the network upload for one revision: resolve the capture identity, assemble the
/// manifest, connect, and run the resumable upload, persisting the server upload id for resume.
fn run_upload(
    state: &mut StateStore,
    client_id: &str,
    endpoint: &str,
    record: &SessionRecord,
    artifacts: &[PreparedArtifact],
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
        // A recorded-evidence identity (issue #40) is flagged in the capture metadata so a
        // consumer can distinguish it from a live-resolved one; live identities add nothing.
        source_metadata: record
            .project
            .as_ref()
            .and_then(|project| project.origin.recorded_marker())
            .map(|marker| BTreeMap::from([("origin".to_owned(), marker.to_owned())]))
            .unwrap_or_default(),
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
        artifact_set_version: INITIAL_ARTIFACT_SET_VERSION,
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
    pub uploaded_revision: Option<u64>,
    pub attempts: u32,
    pub next_attempt_at_ms: Option<i64>,
    pub last_error_category: Option<String>,
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
            ArchiveUploadItem {
                source: record.source.as_selector().to_owned(),
                session_id: record.session_id.clone(),
                state: record.upload_state.clone(),
                snapshot_id: record.snapshot_id.clone(),
                uploaded_revision: record.uploaded_revision,
                attempts: record.attempts,
                next_attempt_at_ms: record.next_attempt_at_ms,
                last_error_category: record.last_error_category.clone(),
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
        for item in &self.items {
            println!(
                "{}  {}  {}{}{}",
                item.session_id,
                item.state,
                item.snapshot_id.as_deref().unwrap_or("<no-snapshot>"),
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
            _ => "archive-upload retry",
        };
        println!(
            "{label} candidates={} uploaded={} already-uploaded={} skipped={} failed={}",
            self.candidates, self.uploaded, self.already_uploaded, self.skipped, self.failed
        );
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

/// Retries failed uploads, or one session's upload, against the configured server. Requires archive
/// upload to be enabled and addressable. Each candidate's backoff is cleared before the attempt
/// (`force` additionally revives a dead-letter row and resets its bounded attempt count), then the
/// upload runs under the session's advisory lock. A locked session is reported skipped this run.
/// Never mutates the session's archival lifecycle.
pub fn retry(
    state_directory: &Path,
    source: Option<SourceKind>,
    session_id: Option<String>,
    all: bool,
    force: bool,
    limit: usize,
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
    let mut candidates: Vec<ArchiveUploadRecord> = recorded
        .into_iter()
        .filter(|record| record.endpoint == endpoint)
        .filter(|record| match &session_id {
            // One session: retry it whatever its state, optionally narrowed by source.
            Some(id) => {
                record.session_id == *id && source.is_none_or(|wanted| record.source == wanted)
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
        items: Vec::new(),
    };
    for record in candidates {
        let outcome = locked_upload_one(
            state_directory,
            &config.archive_upload,
            &client_id,
            &endpoint,
            &output_directory,
            record.source,
            &record.session_id,
            config.limits.max_event_text_bytes,
            Some(force),
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
/// (Patwari snapshots are immutable); the fresh capture adds the complete one beside it.
///
/// Requires archive upload to be enabled and addressable; candidates are bounded by `limit`; a
/// session whose advisory lock is held (an archive worker is on it) is reported skipped this run.
/// Never mutates the session's archival lifecycle.
pub fn backfill(
    state_directory: &Path,
    limit: usize,
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
    // one exception: an `uploaded` row whose snapshot is not proven self-contained is this run's
    // to re-upload (issue #47). A row recorded for a different endpoint (e.g. before
    // reconfiguration) does not count here at all, matching `retry`'s endpoint scoping.
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
                // `dead-letter` rows are the retry paths' business and are left untouched.
                Some(record) => record.upload_state == "uploaded" && !records_full_snapshot(record),
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
        items: Vec::new(),
    };
    for session in candidates {
        let outcome = locked_upload_one(
            state_directory,
            &config.archive_upload,
            &client_id,
            &endpoint,
            &output_directory,
            session.source,
            &session.session_id,
            config.limits.max_event_text_bytes,
            None,
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
#[allow(clippy::too_many_arguments)]
fn locked_upload_one(
    state_directory: &Path,
    settings: &crate::registration::StoredArchiveUpload,
    client_id: &str,
    endpoint: &str,
    output_directory: &Path,
    source: SourceKind,
    session_id: &str,
    max_event_text_bytes: usize,
    reset_for_retry: Option<bool>,
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
        output_directory,
        &session,
        max_event_text_bytes,
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
                artifact_set_version: INITIAL_ARTIFACT_SET_VERSION,
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
            snapshot_id: Some("snap-1".to_owned()),
            uploaded_artifact_paths: paths
                .map(|paths| paths.into_iter().map(ToOwned::to_owned).collect()),
            upload_state: "uploaded".to_owned(),
            attempts: 0,
            next_attempt_at_ms: None,
            last_error_category: None,
            updated_at_ms: 0,
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
                &output_dir,
                &record,
                DEFAULT_MAX_EVENT_TEXT_BYTES,
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
            last_error_category: None,
            next_retry_at_ms: None,
            failure_streak: 0,
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
            &output_dir,
            &record,
            DEFAULT_MAX_EVENT_TEXT_BYTES,
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
