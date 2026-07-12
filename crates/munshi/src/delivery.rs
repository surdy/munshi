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
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use crate::registration::{
    DEFAULT_MAX_DELIVERY_ATTEMPTS, RegistrationError, StoredConfig, StoredCredential,
    StoredDelivery, load_stored_config, update_stored_config,
};
use crate::state::{
    DeliveryRecord, DeliverySuccess, SessionRecord, StateError, StateStore, now_ms,
};

/// Base backoff between failed delivery attempts; doubles per attempt up to [`MAX_BACKOFF_MS`].
const BASE_BACKOFF_MS: i64 = 60_000;
/// Upper bound on delivery backoff so a long outage still retries roughly hourly.
const MAX_BACKOFF_MS: i64 = 3_600_000;
/// Bounds the delivery HTTP response body Munshi will read from a sink.
const MAX_RESPONSE_BYTES: usize = 1_048_576;
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
    #[error("Notesmith delivery is not enabled; run `munshi delivery enable`")]
    NotEnabled,
    #[error("Notesmith sink is not configured; run `munshi delivery configure`")]
    NotConfigured,
    #[error("delivery credential could not be resolved: {0}")]
    Credential(String),
    #[error("delivery endpoint {0} is not a supported http URL")]
    UnsupportedEndpoint(String),
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

/// Sink details supplied by `munshi delivery configure`.
#[derive(Debug, Clone)]
pub struct DeliverySinkConfig {
    pub endpoint: String,
    pub vault: String,
    pub folder: Option<String>,
    pub credential: Option<DeliveryCredentialSource>,
    pub max_attempts: Option<u32>,
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
}

impl DeliverySettings {
    fn from_config(config: &StoredConfig) -> Self {
        Self {
            enabled: config.remote_delivery,
            addressable: config.delivery.is_addressable(),
            endpoint: config.delivery.endpoint.clone(),
            vault: config.delivery.vault.clone(),
            folder: config.delivery.folder.clone(),
            credential_source: config
                .delivery
                .credential
                .as_ref()
                .map(DeliveryCredentialSource::describe),
            max_attempts: config.delivery.max_attempts,
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
    pub attempts: u32,
    pub next_attempt_at_ms: Option<i64>,
    pub last_error_category: Option<String>,
}

/// The `munshi delivery status` contract.
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
            Self::Failed { .. } => "failed",
        }
    }
}

/// The `munshi delivery backfill` / `munshi delivery retry` contract.
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

/// A create request for a Munshi-owned note.
#[derive(Debug, Clone)]
pub struct CreateNote<'a> {
    pub title: &'a str,
    pub folder: &'a str,
    pub content: &'a str,
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
}

impl SinkError {
    fn category(&self) -> &'static str {
        match self {
            Self::AlreadyExists { .. } => "delivery-conflict",
            Self::NotFound => "delivery-not-found",
            Self::Transport(_) => "delivery-transport",
            Self::Server { .. } => "delivery-server",
            Self::Protocol(_) => "delivery-protocol",
        }
    }
}

/// The Notesmith wire protocol Munshi depends on. Isolating it keeps the delivery orchestration
/// testable and leaves room for issue #9's versioned, revision-history-preserving sink.
pub trait NotesmithSink {
    fn create_note(&self, request: &CreateNote<'_>) -> Result<NoteRef, SinkError>;
    /// Replaces a note's content, overwriting any remote edits (Munshi owns delivered notes).
    fn replace_note(&self, path: &str, content: &str) -> Result<NoteRef, SinkError>;
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
    delivery: &StoredDelivery,
    disabled_projects: &[String],
    output_directory: &Path,
    record: &SessionRecord,
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

    let content = fs::read_to_string(output_directory.join(relative)).map_err(DeliveryError::Io)?;
    let source_selector = record.source.as_selector();
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

    let result = if let Some(path) = existing.note_path.as_deref() {
        replace_or_create(
            sink,
            path,
            &content,
            &CreateNote {
                title: &title,
                folder: &dir,
                content: &content,
                session_id: &record.session_id,
                source: source_selector,
                project_identity: &project.identity,
                revision: record.current_revision,
            },
            &deterministic_path,
        )
    } else {
        create_or_adopt(
            sink,
            &CreateNote {
                title: &title,
                folder: &dir,
                content: &content,
                session_id: &record.session_id,
                source: source_selector,
                project_identity: &project.identity,
                revision: record.current_revision,
            },
            &deterministic_path,
        )
    };

    match result {
        Ok((note, created)) => {
            state.record_delivery_success(
                &record.session_id,
                endpoint,
                vault,
                &DeliverySuccess {
                    note_path: note.path.clone(),
                    delivered_revision: record.current_revision,
                    delivered_summary_hash: summary_hash.unwrap_or_default(),
                    remote_hash: note.hash,
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

/// Replaces an existing note, falling back to a create when the remote note has been deleted.
fn replace_or_create(
    sink: &dyn NotesmithSink,
    path: &str,
    content: &str,
    create: &CreateNote<'_>,
    deterministic_path: &str,
) -> Result<(NoteRef, bool), SinkError> {
    match sink.replace_note(path, content) {
        Ok(note) => Ok((note, false)),
        Err(SinkError::NotFound) => create_or_adopt(sink, create, deterministic_path),
        Err(other) => Err(other),
    }
}

/// Creates a note, adopting the deterministic path when the note already exists (for example after
/// a rebuilt operational database) by switching to a content replace.
fn create_or_adopt(
    sink: &dyn NotesmithSink,
    create: &CreateNote<'_>,
    deterministic_path: &str,
) -> Result<(NoteRef, bool), SinkError> {
    match sink.create_note(create) {
        Ok(note) => Ok((note, true)),
        Err(SinkError::AlreadyExists { path }) => {
            let adopt = path.unwrap_or_else(|| deterministic_path.to_owned());
            let note = sink.replace_note(&adopt, create.content)?;
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
        http_request(
            &self.host,
            self.port,
            self.timeout,
            method,
            path,
            self.token.as_deref(),
            body,
        )
    }
}

impl NotesmithSink for HttpNotesmithSink {
    fn create_note(&self, request: &CreateNote<'_>) -> Result<NoteRef, SinkError> {
        let body = serde_json::json!({
            "title": request.title,
            "folder": request.folder,
            "content": request.content,
            "frontmatter": {
                "munshi_session": request.session_id,
                "munshi_source": request.source,
                "munshi_project": request.project_identity,
                "munshi_revision": request.revision,
            },
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

/// Percent-encodes a path segment string for safe inclusion in a request-target, preserving `/`.
fn encode_path(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

fn parse_http_endpoint(endpoint: &str) -> Result<(String, u16), DeliveryError> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| DeliveryError::UnsupportedEndpoint(endpoint.to_owned()))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|_| DeliveryError::UnsupportedEndpoint(endpoint.to_owned()))?;
            (host.to_owned(), port)
        }
        None => (authority.to_owned(), 80),
    };
    if host.is_empty() {
        return Err(DeliveryError::UnsupportedEndpoint(endpoint.to_owned()));
    }
    Ok((host, port))
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

impl HttpResponse {
    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body)
            .chars()
            .take(512)
            .collect()
    }
}

fn http_request(
    host: &str,
    port: u16,
    timeout: Duration,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> Result<HttpResponse, SinkError> {
    let address = (host, port)
        .to_socket_addrs()
        .map_err(|error| SinkError::Transport(error.to_string()))?
        .next()
        .ok_or_else(|| SinkError::Transport(format!("could not resolve {host}:{port}")))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| SinkError::Transport(error.to_string()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| SinkError::Transport(error.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| SinkError::Transport(error.to_string()))?;

    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(body) = body {
        request.push_str("Content-Type: application/json\r\n");
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");

    stream
        .write_all(request.as_bytes())
        .map_err(|error| SinkError::Transport(error.to_string()))?;
    if let Some(body) = body {
        stream
            .write_all(body)
            .map_err(|error| SinkError::Transport(error.to_string()))?;
    }
    stream
        .flush()
        .map_err(|error| SinkError::Transport(error.to_string()))?;

    let mut raw = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES as u64)
        .read_to_end(&mut raw)
        .map_err(|error| SinkError::Transport(error.to_string()))?;
    parse_http_response(&raw)
}

fn parse_http_response(raw: &[u8]) -> Result<HttpResponse, SinkError> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| SinkError::Protocol("response has no header terminator".to_owned()))?;
    let head = &raw[..split];
    let body = &raw[split + 4..];
    let head = String::from_utf8_lossy(head);
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| SinkError::Protocol("empty response".to_owned()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| SinkError::Protocol("unparseable status line".to_owned()))?;
    let chunked = lines.any(|line| {
        let line = line.to_ascii_lowercase();
        line.starts_with("transfer-encoding:") && line.contains("chunked")
    });
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Ok(HttpResponse { status, body })
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, SinkError> {
    let mut output = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| SinkError::Protocol("malformed chunk header".to_owned()))?;
        let size_text = String::from_utf8_lossy(&body[..line_end]);
        let size =
            usize::from_str_radix(size_text.trim().split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| SinkError::Protocol("malformed chunk size".to_owned()))?;
        body = &body[line_end + 2..];
        if size == 0 {
            break;
        }
        if body.len() < size {
            return Err(SinkError::Protocol("truncated chunk body".to_owned()));
        }
        output.extend_from_slice(&body[..size]);
        body = &body[size..];
        if body.starts_with(b"\r\n") {
            body = &body[2..];
        }
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Public configuration + run entry points
// ---------------------------------------------------------------------------

/// Builds an HTTP sink from configuration, resolving the credential from the environment or OS
/// credential store. Returns an error if the sink is not addressable or the credential is missing.
fn sink_from_config(config: &StoredConfig) -> Result<HttpNotesmithSink, DeliveryError> {
    let endpoint = config
        .delivery
        .endpoint
        .as_deref()
        .ok_or(DeliveryError::NotConfigured)?;
    let vault = config
        .delivery
        .vault
        .as_deref()
        .ok_or(DeliveryError::NotConfigured)?;
    let token = match config.delivery.credential.as_ref() {
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
pub(crate) fn deliver_after_archive(
    state: &mut StateStore,
    config: &StoredConfig,
    session_id: &str,
) -> Result<Option<DeliveryOutcome>, DeliveryError> {
    if !config.remote_delivery || !config.delivery.is_addressable() {
        return Ok(None);
    }
    let Some(record) = state.get_session(session_id)? else {
        return Ok(None);
    };
    let sink = sink_from_config(config)?;
    let output_directory = PathBuf::from(&config.output_directory);
    let outcome = deliver_one(
        state,
        &sink,
        &config.delivery,
        &config.policy.disabled_projects,
        &output_directory,
        &record,
    )?;
    Ok(Some(outcome))
}

/// Reads the safe delivery settings view from the Munshi-owned configuration.
pub fn load_settings(state_directory: &Path) -> Result<DeliverySettings, DeliveryError> {
    let config = load_stored_config(state_directory)?;
    Ok(DeliverySettings::from_config(&config))
}

/// Configures the Notesmith sink without enabling delivery. The credential source is recorded, but
/// never the secret itself.
pub fn configure_sink(
    copilot_home: &Path,
    state_directory: &Path,
    sink: DeliverySinkConfig,
) -> Result<DeliverySettings, DeliveryError> {
    // Validate the endpoint eagerly so a bad URL is rejected at configure time.
    parse_http_endpoint(&sink.endpoint)?;
    let (config, ()) = update_stored_config(copilot_home, state_directory, |config| {
        config.delivery.endpoint = Some(sink.endpoint.clone());
        config.delivery.vault = Some(sink.vault.clone());
        config.delivery.folder = sink.folder.clone().filter(|value| !value.is_empty());
        config.delivery.credential = sink
            .credential
            .as_ref()
            .map(DeliveryCredentialSource::to_stored);
        config.delivery.max_attempts = sink
            .max_attempts
            .filter(|value| *value >= 1)
            .unwrap_or(DEFAULT_MAX_DELIVERY_ATTEMPTS);
        Ok(())
    })?;
    Ok(DeliverySettings::from_config(&config))
}

/// Enables or disables delivery. Disabling stops future delivery while retaining delivery history.
pub fn set_enabled(
    copilot_home: &Path,
    state_directory: &Path,
    enabled: bool,
) -> Result<DeliverySettings, DeliveryError> {
    let result = update_stored_config(copilot_home, state_directory, |config| {
        if enabled && !config.delivery.is_addressable() {
            return Err(RegistrationError::MalformedOwnedFile);
        }
        config.remote_delivery = enabled;
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
pub fn status(state_directory: &Path) -> Result<DeliveryStatusReport, DeliveryError> {
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
    let items = deliveries
        .iter()
        .map(|record| {
            match record.delivery_state.as_str() {
                "delivered" => delivered += 1,
                "pending" => pending += 1,
                "failed" => failed += 1,
                "dead-letter" => dead_letter += 1,
                _ => {}
            }
            delivery_item(record)
        })
        .collect::<Vec<_>>();

    Ok(DeliveryStatusReport {
        schema_version: 1,
        command: "delivery-status",
        settings,
        total: items.len(),
        delivered,
        pending,
        failed,
        dead_letter,
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
        "delivery-backfill",
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
        "delivery-retry",
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
    if !config.remote_delivery {
        return Err(DeliveryError::NotEnabled);
    }
    if !config.delivery.is_addressable() {
        return Err(DeliveryError::NotConfigured);
    }
    let endpoint = config.delivery.endpoint.clone().unwrap();
    let vault = config.delivery.vault.clone().unwrap();
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
        failed: 0,
        items: Vec::new(),
    };

    if !confirm {
        // Dry run: report the count without contacting the sink or resolving credentials.
        return Ok(report);
    }

    let token = match config.delivery.credential.as_ref() {
        Some(credential) => Some(resolve_credential(credential)?),
        None => None,
    };
    let sink = HttpNotesmithSink::connect(&endpoint, &vault, token)?;

    for record in candidates {
        let mut state = StateStore::open_for_source(state_directory, record.source)?;
        if force {
            state.reset_delivery_for_retry(&record.session_id, &endpoint, &vault, true)?;
        }
        let outcome = deliver_one(
            &mut state,
            &sink,
            &config.delivery,
            &config.policy.disabled_projects,
            &output_directory,
            &record,
        )?;
        match &outcome {
            DeliveryOutcome::Created { .. } => report.created += 1,
            DeliveryOutcome::Replaced { .. } => report.replaced += 1,
            DeliveryOutcome::AlreadyDelivered { .. } => report.already_delivered += 1,
            DeliveryOutcome::Skipped { .. } => report.skipped += 1,
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
    let state = StateStore::open(state_directory)?;
    let sessions = state.list_sessions()?;
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
        let existing = state.get_delivery(&record.session_id, endpoint, vault)?;
        let include = match selection {
            Selection::Backfill => match existing {
                Some(delivery) => !is_current_delivery(&delivery, &record),
                None => true,
            },
            Selection::Failed => existing.as_ref().is_some_and(|delivery| {
                delivery.delivery_state == "failed"
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
            "deliveries total={} delivered={} pending={} failed={} dead-letter={}",
            self.total, self.delivered, self.pending, self.failed, self.dead_letter
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
                "delivery run candidates={} created={} replaced={} already-delivered={} skipped={} failed={}",
                self.candidates,
                self.created,
                self.replaced,
                self.already_delivered,
                self.skipped,
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
        "delivery {} (endpoint {}, vault {}, folder {}, credential {}, max-attempts {})",
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
    );
}
