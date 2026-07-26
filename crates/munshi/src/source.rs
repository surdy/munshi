use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The default per-event extraction threshold: when a normalized event's content exceeds this many
/// bytes it is extracted as its own content-addressed snapshot artifact and elided from summarizer
/// input, rather than failing the load (ADR 0010). The default preserves the historical 128 KB cap
/// on per-event summarizer input size; it is configurable via `limits.max_event_text_bytes`.
pub const DEFAULT_MAX_EVENT_TEXT_BYTES: usize = 128 * 1024;
pub const NORMALIZER_VERSION: u32 = 2;

/// Vendor-neutral identity of the coding-agent harness that produced a session.
///
/// The variants form the adapter boundary: each maps a source-specific transcript
/// envelope to the same [`NormalizedSession`] model so that summarizer, renderer,
/// state, and delivery paths remain independent from the capturing harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// GitHub Copilot CLI (`events.jsonl`), the version-pinned default adapter.
    #[default]
    Copilot,
    /// Anthropic Claude Code session transcripts (`<uuid>.jsonl`).
    ClaudeCode,
    /// OpenAI Codex CLI rollout files (`rollout-*.jsonl`).
    Codex,
}

impl SourceKind {
    /// Stable identity prefix used in the durable archive `id` (`<prefix>:<session>`).
    pub fn id_prefix(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// Human- and machine-readable agent label recorded in archive frontmatter,
    /// summarizer input, and the SQLite `source_kind` column.
    pub fn agent_label(self) -> &'static str {
        match self {
            Self::Copilot => "copilot-cli",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex-cli",
        }
    }

    /// Selector accepted on the command line and in configuration.
    pub fn as_selector(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// Parse a user- or config-provided selector into a source kind.
    pub fn parse_selector(value: &str) -> Option<Self> {
        match value {
            "copilot" | "copilot-cli" => Some(Self::Copilot),
            "claude-code" | "claude" => Some(Self::ClaudeCode),
            "codex" | "codex-cli" => Some(Self::Codex),
            _ => None,
        }
    }

    /// Recover the source kind from a persisted agent label.
    pub fn from_agent_label(label: &str) -> Option<Self> {
        match label {
            "copilot-cli" => Some(Self::Copilot),
            "claude-code" => Some(Self::ClaudeCode),
            "codex-cli" => Some(Self::Codex),
            _ => None,
        }
    }

    /// Whether the source supports resolving a transcript from a session ID alone.
    /// Only the version-pinned Copilot session-state fallback is supported; other
    /// harnesses require an explicit transcript path.
    fn supports_session_id_lookup(self) -> bool {
        matches!(self, Self::Copilot)
    }
}

#[derive(Debug, Clone)]
pub struct SessionReference {
    pub source: SourceKind,
    pub session_id: Option<String>,
    pub events_path: Option<PathBuf>,
    pub copilot_home: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSession {
    pub source: SourceKind,
    pub session_id: String,
    pub events_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedEvent {
    pub kind: &'static str,
    pub content: String,
}

/// The complete content of an oversized normalized event, preserved as its own content-addressed
/// snapshot artifact instead of being truncated away (ADR 0010, CONTEXT.md "extracted output").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedOutput {
    /// Lowercase hex sha256 of `content` (unprefixed) — the stem of the `outputs/<sha256>` artifact
    /// logical path and the address the summarizer's claim ticket carries.
    pub sha256: String,
    /// The source event's media type, when known. Normalized event content is always UTF-8 text.
    pub media_type: Option<String>,
    /// A short human label — the normalized event's kind (`user`/`assistant`/`tool`) — that the
    /// claim ticket and the frontmatter artifact index reproduce so a reader can tell at a glance
    /// what kind of content was elided. Deduplication keeps the label of the first occurrence.
    pub label: String,
    /// The complete original content bytes.
    pub content: Vec<u8>,
}

/// One entry in a revision's snapshot artifact index (ADR 0010, CONTEXT.md "snapshot artifact set"):
/// the content address, original size, and short label of one extracted output. Mirrors the
/// claim-ticket marker the summarizer saw in its place, and points at the `outputs/<sha256>` artifact
/// the snapshot uploads. Derived purely from local extraction, never from any upload result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIndexEntry {
    /// Bare lowercase-hex sha256, matching the `outputs/<sha256>` logical path and the claim ticket.
    pub sha256: String,
    /// The original (uncompressed) content size in bytes.
    pub bytes: u64,
    /// The extracted event's kind label (`user`/`assistant`/`tool`).
    pub label: String,
}

/// A revision's snapshot artifact index, derived from the transcript bytes read during
/// normalization (ADR 0010). Carried on [`NormalizedSession`] so the renderer can record it in
/// archive frontmatter without a later re-read of the transcript, keeping the index consistent with
/// the exact bytes this revision summarized. Empty when no event exceeded the extraction threshold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotArtifactIndex {
    /// Every extracted output for the full transcript, deduplicated and sorted ascending by hash —
    /// byte-for-byte the set [`extract_outputs`] re-derives at upload time.
    pub extracted_outputs: Vec<ArtifactIndexEntry>,
}

/// Builds the snapshot artifact index for a full transcript from the bytes read during normalization.
/// A pure, deterministic function of the transcript bytes, the source adapter, and the extraction
/// threshold — identical to the ordered set [`assemble_artifact_sources`] uploads — so the frontmatter
/// index, the claim tickets in summarizer input, and the `outputs/<sha256>` artifacts always agree.
pub fn snapshot_artifact_index(
    bytes: &[u8],
    source: SourceKind,
    max_event_text_bytes: usize,
) -> SnapshotArtifactIndex {
    SnapshotArtifactIndex {
        extracted_outputs: extract_outputs(bytes, source, max_event_text_bytes)
            .into_iter()
            .map(|output| ArtifactIndexEntry {
                bytes: output.content.len() as u64,
                sha256: output.sha256,
                label: output.label,
            })
            .collect(),
    }
}

#[derive(Debug, Clone)]
pub struct NormalizedSession {
    pub source: SourceKind,
    pub session_id: String,
    pub events: Vec<NormalizedEvent>,
    pub user_requests: usize,
    pub assistant_messages: usize,
    pub tool_activities: usize,
    pub ignored_events: usize,
    pub source_cursor: u64,
    pub source_byte_cursor: u64,
    pub source_prefix_hash: String,
    pub source_hash: String,
    pub source_bytes: u64,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    /// This revision's snapshot artifact index (ADR 0010), derived from the same transcript read
    /// that produced `source_hash`. Populated for full transcript bytes even on delta loads, so it
    /// always describes the complete snapshot the renderer records and the upload path assembles.
    pub artifact_index: SnapshotArtifactIndex,
}

impl NormalizedSession {
    pub fn is_archive_worthy(&self) -> bool {
        self.user_requests > 0 && (self.assistant_messages > 0 || self.tool_activities > 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousSource {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorFallbackReason {
    CursorMismatch,
    NormalizerChanged,
    SourceTruncated,
}

impl CursorFallbackReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::CursorMismatch => "cursor-mismatch",
            Self::NormalizerChanged => "normalizer-changed",
            Self::SourceTruncated => "source-truncated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptLoadMode {
    Full,
    Delta,
    Unchanged,
}

#[derive(Debug, Clone)]
pub struct SourceSnapshot {
    path: PathBuf,
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

impl SourceSnapshot {
    pub fn verify_unchanged(&self) -> Result<(), SourceError> {
        let metadata = fs::metadata(&self.path).map_err(SourceError::Io)?;
        if metadata.dev() != self.device
            || metadata.ino() != self.inode
            || metadata.len() != self.length
            || metadata.mtime() != self.modified_seconds
            || metadata.mtime_nsec() != self.modified_nanoseconds
        {
            return Err(SourceError::ChangedDuringRead);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptUpdate {
    pub session: NormalizedSession,
    pub mode: TranscriptLoadMode,
    pub fallback_reason: Option<CursorFallbackReason>,
    pub snapshot: SourceSnapshot,
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("provide a session ID, an explicit events.jsonl path, or both")]
    MissingReference,
    #[error("session ID is not a safe supported identifier")]
    InvalidSessionId,
    #[error("explicit transcript path must name a regular events.jsonl file")]
    UnsupportedTranscriptPath,
    #[error("session ID does not match the events.jsonl parent directory")]
    SessionIdMismatch,
    #[error("COPILOT_HOME or HOME is required when resolving a session ID")]
    MissingCopilotHome,
    #[error("session transcript could not be resolved")]
    TranscriptNotFound,
    #[error("resolved session transcript escapes the supported Copilot session-state directory")]
    UnsafeResolvedPath,
    #[error("transcript I/O failed")]
    Io(#[source] io::Error),
    #[error("transcript exceeds the configured {limit}-byte source limit")]
    SourceLimit { limit: usize },
    #[error("transcript line {line} is not valid JSON")]
    MalformedJson { line: u64 },
    #[error("transcript changed while it was being read")]
    ChangedDuringRead,
    #[error("transcript ends with an incomplete JSON record")]
    IncompleteTrailingRecord,
    #[error("transcript does not match the version-pinned Copilot event envelope")]
    UnsupportedEnvelope,
}

pub fn resolve_session_reference(
    reference: &SessionReference,
) -> Result<ResolvedSession, SourceError> {
    if reference.session_id.is_none() && reference.events_path.is_none() {
        return Err(SourceError::MissingReference);
    }

    let source = reference.source;
    let supplied_id = reference
        .session_id
        .as_deref()
        .map(validate_session_id)
        .transpose()?;

    if let Some(path) = &reference.events_path {
        let metadata = std::fs::symlink_metadata(path).map_err(SourceError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SourceError::UnsupportedTranscriptPath);
        }
        let canonical = path.canonicalize().map_err(SourceError::Io)?;
        let derived_id = derive_session_id_from_path(source, &canonical)?;
        if supplied_id.is_some_and(|session_id| session_id != derived_id) {
            return Err(SourceError::SessionIdMismatch);
        }
        return Ok(ResolvedSession {
            source,
            session_id: supplied_id.unwrap_or(&derived_id).to_owned(),
            events_path: canonical,
        });
    }

    let session_id = supplied_id.expect("a session ID is present when no path was supplied");
    if !source.supports_session_id_lookup() {
        // Only the version-pinned Copilot session-state directory is a supported
        // session-ID fallback; other harnesses require an explicit transcript path.
        return Err(SourceError::TranscriptNotFound);
    }
    let home = reference
        .copilot_home
        .clone()
        .or_else(|| std::env::var_os("COPILOT_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".copilot")))
        .ok_or(SourceError::MissingCopilotHome)?;
    let session_state = home.join("session-state");
    let expected_directory = session_state.join(session_id);
    let candidate = expected_directory.join("events.jsonl");
    if !candidate.exists() {
        return Err(SourceError::TranscriptNotFound);
    }
    let canonical_state = session_state
        .canonicalize()
        .map_err(|_| SourceError::TranscriptNotFound)?;
    let canonical_directory = expected_directory
        .canonicalize()
        .map_err(|_| SourceError::TranscriptNotFound)?;
    let canonical = candidate
        .canonicalize()
        .map_err(|_| SourceError::TranscriptNotFound)?;
    if !canonical_directory.starts_with(&canonical_state)
        || !canonical.starts_with(&canonical_directory)
        || canonical.file_name().and_then(|name| name.to_str()) != Some("events.jsonl")
        || !canonical.is_file()
    {
        return Err(SourceError::UnsafeResolvedPath);
    }

    Ok(ResolvedSession {
        source,
        session_id: session_id.to_owned(),
        events_path: canonical,
    })
}

/// Derive a stable session ID from an explicit transcript path for the given source.
///
/// Copilot keeps its version-pinned `session-state/<id>/events.jsonl` layout where the
/// parent directory is the session ID. Claude Code and Codex name the transcript file
/// itself after the session, so the sanitized file stem is used.
fn derive_session_id_from_path(
    source: SourceKind,
    canonical: &Path,
) -> Result<String, SourceError> {
    match source {
        SourceKind::Copilot => {
            if canonical.file_name().and_then(|name| name.to_str()) != Some("events.jsonl") {
                return Err(SourceError::UnsupportedTranscriptPath);
            }
            canonical
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .ok_or(SourceError::InvalidSessionId)
                .and_then(validate_session_id)
                .map(ToOwned::to_owned)
        }
        SourceKind::ClaudeCode | SourceKind::Codex => {
            if canonical
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("jsonl")
            {
                return Err(SourceError::UnsupportedTranscriptPath);
            }
            canonical
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or(SourceError::InvalidSessionId)
                .and_then(validate_session_id)
                .map(ToOwned::to_owned)
        }
    }
}

pub fn load_session(
    resolved: &ResolvedSession,
    max_source_bytes: usize,
) -> Result<NormalizedSession, SourceError> {
    Ok(load_session_update(
        resolved,
        max_source_bytes,
        None,
        DEFAULT_MAX_EVENT_TEXT_BYTES,
    )?
    .session)
}

pub fn load_session_update(
    resolved: &ResolvedSession,
    max_source_bytes: usize,
    previous: Option<&PreviousSource>,
    max_event_text_bytes: usize,
) -> Result<TranscriptUpdate, SourceError> {
    let (bytes, snapshot) = read_stable_source(&resolved.events_path, max_source_bytes)?;
    validate_trailing_record(&bytes)?;
    let source_hash = sha256(&bytes);

    let (mode, fallback_reason, normalized, total_records) = match previous {
        None => {
            let normalized = normalize_records(&bytes, 0, resolved.source, max_event_text_bytes)?;
            (
                TranscriptLoadMode::Full,
                None,
                normalized,
                count_records(&bytes),
            )
        }
        Some(previous) if previous.normalizer_version != NORMALIZER_VERSION => {
            let normalized = normalize_records(&bytes, 0, resolved.source, max_event_text_bytes)?;
            (
                TranscriptLoadMode::Full,
                Some(CursorFallbackReason::NormalizerChanged),
                normalized,
                count_records(&bytes),
            )
        }
        Some(previous) if bytes.len() < previous.byte_offset as usize => {
            let normalized = normalize_records(&bytes, 0, resolved.source, max_event_text_bytes)?;
            (
                TranscriptLoadMode::Full,
                Some(CursorFallbackReason::SourceTruncated),
                normalized,
                count_records(&bytes),
            )
        }
        Some(previous) => {
            let offset = previous.byte_offset as usize;
            let prefix = &bytes[..offset];
            let prefix_valid = sha256(prefix) == previous.prefix_hash
                && count_records(prefix) == previous.record_count;
            if !prefix_valid {
                let normalized =
                    normalize_records(&bytes, 0, resolved.source, max_event_text_bytes)?;
                (
                    TranscriptLoadMode::Full,
                    Some(CursorFallbackReason::CursorMismatch),
                    normalized,
                    count_records(&bytes),
                )
            } else {
                let delta = &bytes[offset..];
                if !delta.is_empty()
                    && offset > 0
                    && bytes[offset - 1] != b'\n'
                    && delta[0] != b'\n'
                {
                    let normalized =
                        normalize_records(&bytes, 0, resolved.source, max_event_text_bytes)?;
                    (
                        TranscriptLoadMode::Full,
                        Some(CursorFallbackReason::CursorMismatch),
                        normalized,
                        count_records(&bytes),
                    )
                } else if count_records(delta) == 0 {
                    (
                        TranscriptLoadMode::Unchanged,
                        None,
                        NormalizedRecords::default(),
                        previous.record_count,
                    )
                } else {
                    let normalized = normalize_records(
                        delta,
                        previous.record_count,
                        resolved.source,
                        max_event_text_bytes,
                    )?;
                    (
                        TranscriptLoadMode::Delta,
                        None,
                        normalized,
                        previous.record_count + count_records(delta),
                    )
                }
            }
        }
    };

    let (user_requests, assistant_messages, tool_activities, started_at, updated_at) =
        if mode == TranscriptLoadMode::Delta {
            let previous = previous.expect("delta loads have a previous cursor");
            (
                previous.user_requests + normalized.user_requests,
                previous.assistant_messages + normalized.assistant_messages,
                previous.tool_activities + normalized.tool_activities,
                previous
                    .started_at
                    .clone()
                    .or_else(|| normalized.first_timestamp.map(format_timestamp)),
                normalized
                    .last_timestamp
                    .map(format_timestamp)
                    .or_else(|| previous.updated_at.clone()),
            )
        } else if mode == TranscriptLoadMode::Unchanged {
            let previous = previous.expect("unchanged loads have a previous cursor");
            (
                previous.user_requests,
                previous.assistant_messages,
                previous.tool_activities,
                previous.started_at.clone(),
                previous.updated_at.clone(),
            )
        } else {
            (
                normalized.user_requests,
                normalized.assistant_messages,
                normalized.tool_activities,
                normalized.first_timestamp.map(format_timestamp),
                normalized.last_timestamp.map(format_timestamp),
            )
        };

    let source_prefix_hash = sha256(&bytes);
    // Derive the snapshot artifact index from the full transcript bytes this load read — never a
    // later re-read (ADR 0010) — so the frontmatter index the renderer writes matches the exact
    // bytes summarized. Computed over the whole file even on a delta load, since the uploaded
    // snapshot is always the full transcript.
    let artifact_index = snapshot_artifact_index(&bytes, resolved.source, max_event_text_bytes);
    Ok(TranscriptUpdate {
        session: NormalizedSession {
            source: resolved.source,
            session_id: resolved.session_id.clone(),
            events: normalized.events,
            user_requests,
            assistant_messages,
            tool_activities,
            ignored_events: normalized.ignored_events,
            source_cursor: total_records,
            source_byte_cursor: bytes.len() as u64,
            source_prefix_hash,
            source_hash,
            source_bytes: bytes.len() as u64,
            started_at,
            updated_at,
            artifact_index,
        },
        mode,
        fallback_reason,
        snapshot,
    })
}

/// Bounded, privacy-safe read of a Claude Code transcript's origin project directory: the first
/// absolute top-level `cwd` value among the leading records (bookkeeping records such as
/// `queue-operation` may precede the first turn and lack one). Only the pinned `cwd` key is
/// inspected, mirroring the envelope-validation read discipline; content is never read. Lets the
/// recovery sweep hand `mark_recovery_interrupted` an origin so swept sessions can compute
/// project identity instead of parking as origin-unresolved.
pub fn claude_transcript_origin(path: &Path) -> Option<PathBuf> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let mut reader = BufReader::new(File::open(path).ok()?);
    let limit = 256 * 1024;
    let mut line = Vec::new();
    for _ in 0..32 {
        line.clear();
        let read = reader
            .by_ref()
            .take(limit as u64 + 1)
            .read_until(b'\n', &mut line)
            .ok()?;
        if read == 0 || line.len() > limit {
            return None;
        }
        while line
            .last()
            .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
        {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_slice(&line).ok()?;
        let object = value.as_object()?;
        if let Some(cwd) = object.get("cwd").and_then(Value::as_str) {
            if Path::new(cwd).is_absolute() {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    None
}

pub fn validate_transcript_envelope(
    source: SourceKind,
    path: &Path,
    max_source_bytes: usize,
) -> Result<(), SourceError> {
    let metadata = fs::symlink_metadata(path).map_err(SourceError::Io)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SourceError::UnsupportedTranscriptPath);
    }
    let mut reader = BufReader::new(File::open(path).map_err(SourceError::Io)?);
    let mut line = Vec::new();
    let limit = max_source_bytes.min(256 * 1024);
    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take(limit.saturating_add(1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(SourceError::Io)?;
        if read == 0 {
            return Err(SourceError::UnsupportedEnvelope);
        }
        if line.len() > limit {
            return Err(SourceError::UnsupportedEnvelope);
        }
        while line
            .last()
            .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
        {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_slice(&line).map_err(|_| SourceError::UnsupportedEnvelope)?;
        let object = value.as_object().ok_or(SourceError::UnsupportedEnvelope)?;
        if !envelope_matches(source, object) {
            return Err(SourceError::UnsupportedEnvelope);
        }
        return Ok(());
    }
}

/// Structural envelope recognition for the first meaningful transcript record.
///
/// Each check is intentionally shallow and privacy-safe: it inspects only the
/// version-pinned discriminator keys, never record content, so a different
/// harness's transcript is rejected before any normalization occurs.
fn envelope_matches(source: SourceKind, object: &Map<String, Value>) -> bool {
    match source {
        SourceKind::Copilot => {
            object.get("id").is_some_and(Value::is_string)
                && object.contains_key("timestamp")
                && object.contains_key("parentId")
                && object.get("type").is_some_and(Value::is_string)
                && object.get("data").is_some_and(Value::is_object)
        }
        SourceKind::ClaudeCode => {
            let has_type = object.get("type").is_some_and(Value::is_string);
            let claude_shaped = object.get("message").is_some_and(Value::is_object)
                || object.contains_key("leafUuid")
                || object.contains_key("sessionId")
                || object.contains_key("uuid");
            has_type && claude_shaped && !object.contains_key("payload")
        }
        SourceKind::Codex => {
            object.get("type").is_some_and(Value::is_string)
                && object.contains_key("timestamp")
                && object.contains_key("payload")
        }
    }
}

fn read_stable_source(
    path: &Path,
    max_source_bytes: usize,
) -> Result<(Vec<u8>, SourceSnapshot), SourceError> {
    let mut bytes = Vec::new();
    let file = File::open(path).map_err(SourceError::Io)?;
    let before = file.metadata().map_err(SourceError::Io)?;
    file.take(max_source_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(SourceError::Io)?;
    if bytes.len() > max_source_bytes {
        return Err(SourceError::SourceLimit {
            limit: max_source_bytes,
        });
    }
    let after = fs::metadata(path).map_err(SourceError::Io)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || after.len() != bytes.len() as u64
    {
        return Err(SourceError::ChangedDuringRead);
    }
    let snapshot = SourceSnapshot {
        path: path.to_path_buf(),
        device: after.dev(),
        inode: after.ino(),
        length: after.len(),
        modified_seconds: after.mtime(),
        modified_nanoseconds: after.mtime_nsec(),
    };
    Ok((bytes, snapshot))
}

#[derive(Default)]
struct NormalizedRecords {
    events: Vec<NormalizedEvent>,
    user_requests: usize,
    assistant_messages: usize,
    tool_activities: usize,
    ignored_events: usize,
    line_count: u64,
    first_timestamp: Option<DateTime<Utc>>,
    last_timestamp: Option<DateTime<Utc>>,
}

/// Outcome of classifying one raw transcript record for a source adapter.
struct RecordClass {
    events: Vec<NormalizedEvent>,
    ignored: usize,
}

impl RecordClass {
    fn ignored() -> Self {
        Self {
            events: Vec::new(),
            ignored: 1,
        }
    }

    fn skipped() -> Self {
        Self {
            events: Vec::new(),
            ignored: 0,
        }
    }

    fn event(kind: &'static str, content: String) -> Self {
        Self {
            events: vec![NormalizedEvent { kind, content }],
            ignored: 0,
        }
    }
}

fn normalize_records(
    bytes: &[u8],
    prior_records: u64,
    source: SourceKind,
    max_event_text_bytes: usize,
) -> Result<NormalizedRecords, SourceError> {
    let mut normalized = NormalizedRecords::default();
    for raw_line in bytes.split(|byte| *byte == b'\n') {
        if raw_line.is_empty() {
            continue;
        }
        normalized.line_count += 1;
        let value: Value =
            serde_json::from_slice(raw_line).map_err(|_| SourceError::MalformedJson {
                line: prior_records + normalized.line_count,
            })?;
        let Some(object) = value.as_object() else {
            normalized.ignored_events += 1;
            continue;
        };

        if let Some(timestamp) = object.get("timestamp").and_then(parse_timestamp) {
            normalized.first_timestamp = Some(
                normalized
                    .first_timestamp
                    .map_or(timestamp, |old| old.min(timestamp)),
            );
            normalized.last_timestamp = Some(
                normalized
                    .last_timestamp
                    .map_or(timestamp, |old| old.max(timestamp)),
            );
        }

        let class = classify_record(source, object)?;
        normalized.ignored_events += class.ignored;
        for event in class.events {
            match event.kind {
                "user" => normalized.user_requests += 1,
                "assistant" => normalized.assistant_messages += 1,
                "tool" => normalized.tool_activities += 1,
                _ => {}
            }
            // Oversized content is extracted, not truncated: the full bytes become an
            // `outputs/<sha256>` snapshot artifact (re-derived at upload time from the same
            // transcript, see `extract_outputs`) and the summarizer sees a hash+size marker in
            // their place. The activity counts above reflect the original event either way.
            normalized
                .events
                .push(elide_if_oversized(event, max_event_text_bytes));
        }
    }
    Ok(normalized)
}

/// Replaces an oversized event's content with its claim-ticket marker, leaving smaller events
/// intact. The ticket carries the content's sha256, size, and label — the same address
/// `extract_outputs` computes — so summaries render before any upload and stay losslessly expandable
/// (ADR 0010, CONTEXT.md "claim ticket"). See [`claim_ticket_marker`] for the exact format.
fn elide_if_oversized(event: NormalizedEvent, max_event_text_bytes: usize) -> NormalizedEvent {
    if event.content.len() <= max_event_text_bytes {
        return event;
    }
    let content = claim_ticket_marker(event.kind, &event.content);
    NormalizedEvent {
        kind: event.kind,
        content,
    }
}

/// Formats the claim-ticket marker that stands in for an elided oversized event in summarizer input
/// (ADR 0010, CONTEXT.md "claim ticket"), documented in `docs/summarizers.md` so summarizers
/// reference these markers rather than invent them.
///
/// The format is a single line, unambiguous, and deterministic — a pure function of the event
/// content (its sha256 and byte size) and its classification (`label`, the normalized event kind):
///
/// ```text
/// [munshi claim-ticket sha256:<hex> bytes:<n> label:<label>]
/// ```
///
/// `sha256` is bare lowercase hex (matching the `outputs/<sha256>` artifact path and the frontmatter
/// artifact index), `bytes` the original content size, and `label` a whitespace-free event kind. A
/// holder redeems the ticket by its sha256 through `munshi retrieve` once the snapshot is archived.
fn claim_ticket_marker(label: &str, content: &str) -> String {
    format!(
        "[munshi claim-ticket sha256:{} bytes:{} label:{}]",
        content_sha256_hex(content.as_bytes()),
        content.len(),
        label,
    )
}

/// Re-derives every extracted output for a full transcript: the complete content of each normalized
/// event whose size exceeds `max_event_text_bytes`, content-addressed and deduplicated by sha256.
///
/// ADR 0010 stages nothing at capture time (option a). Extracted outputs are re-derived here from
/// the same verbatim transcript bytes the snapshot uploads, so no raw content lands in the
/// rebuildable SQLite state (ADR 0004) and the upload path stays retryable after process exit.
/// Extraction is a pure function of the transcript bytes, the source adapter, and the threshold, and
/// classification is pinned by [`NORMALIZER_VERSION`]; every retry of a reused capture id therefore
/// re-produces a byte-identical, deterministically ordered set (ascending by hash), and each
/// `outputs/<sha256>` resolves to exactly the bytes the summarizer's elision marker names. Malformed
/// lines are skipped so a partial trailing record appended after archival never fails the upload.
pub fn extract_outputs(
    bytes: &[u8],
    source: SourceKind,
    max_event_text_bytes: usize,
) -> Vec<ExtractedOutput> {
    let mut extracted: BTreeMap<String, ExtractedOutput> = BTreeMap::new();
    for raw_line in bytes.split(|byte| *byte == b'\n') {
        if raw_line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(raw_line) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        let Ok(class) = classify_record(source, object) else {
            continue;
        };
        for event in class.events {
            if event.content.len() <= max_event_text_bytes {
                continue;
            }
            let sha256 = content_sha256_hex(event.content.as_bytes());
            extracted
                .entry(sha256.clone())
                .or_insert_with(|| ExtractedOutput {
                    sha256,
                    media_type: Some("text/plain; charset=utf-8".to_owned()),
                    label: event.kind.to_owned(),
                    content: event.content.into_bytes(),
                });
        }
    }
    extracted.into_values().collect()
}

fn classify_record(
    source: SourceKind,
    object: &Map<String, Value>,
) -> Result<RecordClass, SourceError> {
    match source {
        SourceKind::Copilot => classify_copilot(object),
        SourceKind::ClaudeCode => classify_claude(object),
        SourceKind::Codex => classify_codex(object),
    }
}

fn classify_copilot(object: &Map<String, Value>) -> Result<RecordClass, SourceError> {
    let Some(event_type) = object.get("type").and_then(Value::as_str) else {
        return Ok(RecordClass::ignored());
    };
    match event_type {
        "user.message" => {
            let Some(data) = event_data(object) else {
                return Ok(RecordClass::ignored());
            };
            let Some(content) = data.get("content").and_then(Value::as_str) else {
                return Ok(RecordClass::ignored());
            };
            match nonempty(content) {
                Some(content) => Ok(RecordClass::event("user", content)),
                None => Ok(RecordClass::skipped()),
            }
        }
        "assistant.message" => {
            let Some(data) = event_data(object) else {
                return Ok(RecordClass::ignored());
            };
            if !data.get("messageId").is_some_and(Value::is_string) {
                return Ok(RecordClass::ignored());
            }
            let Some(content) = data.get("content").and_then(Value::as_str) else {
                return Ok(RecordClass::ignored());
            };
            match nonempty(content) {
                Some(content) => Ok(RecordClass::event("assistant", content)),
                None => Ok(RecordClass::skipped()),
            }
        }
        "tool.execution_start" => {
            let Some(data) = event_data(object).filter(|data| valid_tool_start(data)) else {
                return Ok(RecordClass::ignored());
            };
            Ok(RecordClass::event("tool", extract_tool_start(data)?))
        }
        "tool.execution_complete" => {
            let Some(data) = event_data(object).filter(|data| valid_tool_complete(data)) else {
                return Ok(RecordClass::ignored());
            };
            Ok(RecordClass::event("tool", extract_tool_complete(data)?))
        }
        _ => Ok(RecordClass::ignored()),
    }
}

/// Classify one Anthropic Claude Code transcript record.
///
/// Version-pinned assumption (documented in `docs/harness-adapters.md`): each line is a
/// JSON object with a string `type`. Genuine user prompts and assistant replies live under
/// `message.content` (a string or an array of typed blocks). `tool_use` blocks on assistant
/// messages and `tool_result` blocks on user messages are normalized as tool activity, while
/// `summary`, `system`, and queue/bookkeeping records are treated as ignored metadata.
fn classify_claude(object: &Map<String, Value>) -> Result<RecordClass, SourceError> {
    let Some(record_type) = object.get("type").and_then(Value::as_str) else {
        return Ok(RecordClass::ignored());
    };
    match record_type {
        "user" | "assistant" => {
            let Some(message) = object.get("message").and_then(Value::as_object) else {
                return Ok(RecordClass::ignored());
            };
            let assistant = record_type == "assistant";
            let Some(content) = message.get("content") else {
                return Ok(RecordClass::ignored());
            };
            classify_claude_content(content, assistant)
        }
        // Compaction summaries, system notices, and queue bookkeeping carry no
        // archive-worthy user or agent content.
        _ => Ok(RecordClass::ignored()),
    }
}

fn classify_claude_content(content: &Value, assistant: bool) -> Result<RecordClass, SourceError> {
    match content {
        Value::String(text) => match nonempty(text) {
            Some(text) => Ok(RecordClass::event(
                if assistant { "assistant" } else { "user" },
                text,
            )),
            None => Ok(RecordClass::skipped()),
        },
        Value::Array(blocks) => {
            let mut events = Vec::new();
            let mut recognized = false;
            for block in blocks {
                let Some(block) = block.as_object() else {
                    continue;
                };
                let Some(block_type) = block.get("type").and_then(Value::as_str) else {
                    continue;
                };
                recognized = true;
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            if let Some(text) = nonempty(text) {
                                events.push(NormalizedEvent {
                                    kind: if assistant { "assistant" } else { "user" },
                                    content: text,
                                });
                            }
                        }
                    }
                    "tool_use" if assistant => {
                        if let Some(event) = extract_claude_tool_use(block)? {
                            events.push(event);
                        }
                    }
                    "tool_result" if !assistant => {
                        if let Some(event) = extract_claude_tool_result(block)? {
                            events.push(event);
                        }
                    }
                    _ => {}
                }
            }
            if events.is_empty() && !recognized {
                Ok(RecordClass::ignored())
            } else {
                Ok(RecordClass { events, ignored: 0 })
            }
        }
        _ => Ok(RecordClass::ignored()),
    }
}

fn extract_claude_tool_use(
    block: &Map<String, Value>,
) -> Result<Option<NormalizedEvent>, SourceError> {
    let Some(name) = block.get("name").and_then(Value::as_str).and_then(nonempty) else {
        return Ok(None);
    };
    let mut fields = BTreeMap::new();
    fields.insert("event", "tool_use".to_owned());
    if let Some(id) = block.get("id").and_then(Value::as_str).and_then(nonempty) {
        fields.insert("tool_use_id", id);
    }
    fields.insert("name", name);
    if let Some(input) = block.get("input").and_then(compact_value) {
        fields.insert("input", input);
    }
    Ok(Some(NormalizedEvent {
        kind: "tool",
        content: render_tool_fields(fields),
    }))
}

fn extract_claude_tool_result(
    block: &Map<String, Value>,
) -> Result<Option<NormalizedEvent>, SourceError> {
    let mut fields = BTreeMap::new();
    fields.insert("event", "tool_result".to_owned());
    if let Some(id) = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        fields.insert("tool_use_id", id);
    }
    if block.get("is_error").and_then(Value::as_bool) == Some(true) {
        fields.insert("is_error", "true".to_owned());
    }
    if let Some(output) = block.get("content").and_then(extract_claude_result_text) {
        fields.insert("output", output);
    }
    if fields.len() == 1 {
        return Ok(None);
    }
    Ok(Some(NormalizedEvent {
        kind: "tool",
        content: render_tool_fields(fields),
    }))
}

fn extract_claude_result_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => nonempty(text),
        Value::Array(items) => {
            let parts: Vec<_> = items
                .iter()
                .filter_map(extract_claude_result_text)
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text") => object
                .get("text")
                .and_then(Value::as_str)
                .and_then(nonempty),
            _ => None,
        },
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

/// Classify one OpenAI Codex CLI rollout record.
///
/// Version-pinned assumption (documented in `docs/harness-adapters.md`): each line is a
/// `RolloutLine` wrapping a tagged `RolloutItem` as `{"type": <kind>, "payload": {..}}`.
/// Only `response_item` payloads carry conversation content; `session_meta`, `turn_context`,
/// `compacted`, `event_msg`, and world-state records are ignored metadata. Within a
/// `response_item`, user/assistant messages, function/custom tool calls, and their outputs
/// are normalized; model reasoning is deliberately dropped.
fn classify_codex(object: &Map<String, Value>) -> Result<RecordClass, SourceError> {
    let Some(record_type) = object.get("type").and_then(Value::as_str) else {
        return Ok(RecordClass::ignored());
    };
    if record_type != "response_item" {
        return Ok(RecordClass::ignored());
    }
    let Some(payload) = object.get("payload").and_then(Value::as_object) else {
        return Ok(RecordClass::ignored());
    };
    let Some(item_type) = payload.get("type").and_then(Value::as_str) else {
        return Ok(RecordClass::ignored());
    };
    match item_type {
        "message" => {
            let Some(role) = payload.get("role").and_then(Value::as_str) else {
                return Ok(RecordClass::ignored());
            };
            let text = payload
                .get("content")
                .and_then(Value::as_array)
                .map(|blocks| extract_codex_message_text(blocks))
                .unwrap_or_default();
            match nonempty(&text) {
                Some(text) => match role {
                    "user" => Ok(RecordClass::event("user", text)),
                    "assistant" => Ok(RecordClass::event("assistant", text)),
                    _ => Ok(RecordClass::ignored()),
                },
                None => Ok(RecordClass::skipped()),
            }
        }
        "function_call" | "custom_tool_call" => Ok(codex_tool_call(payload, item_type)?
            .map_or_else(RecordClass::ignored, |event| RecordClass {
                events: vec![event],
                ignored: 0,
            })),
        "function_call_output" | "custom_tool_call_output" => Ok(codex_tool_output(payload)?
            .map_or_else(RecordClass::ignored, |event| RecordClass {
                events: vec![event],
                ignored: 0,
            })),
        "local_shell_call" => Ok(codex_local_shell_call(payload)?.map_or_else(
            RecordClass::ignored,
            |event| RecordClass {
                events: vec![event],
                ignored: 0,
            },
        )),
        // Reasoning is internal model output and is intentionally not archived.
        _ => Ok(RecordClass::ignored()),
    }
}

fn extract_codex_message_text(blocks: &[Value]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        let Some(block) = block.as_object() else {
            continue;
        };
        if let Some("input_text" | "output_text" | "text") =
            block.get("type").and_then(Value::as_str)
        {
            if let Some(text) = block.get("text").and_then(Value::as_str).and_then(nonempty) {
                parts.push(text);
            }
        }
    }
    parts.join("\n")
}

fn codex_tool_call(
    payload: &Map<String, Value>,
    item_type: &str,
) -> Result<Option<NormalizedEvent>, SourceError> {
    let Some(call_id) = payload
        .get("call_id")
        .and_then(Value::as_str)
        .and_then(nonempty)
    else {
        return Ok(None);
    };
    let mut fields = BTreeMap::new();
    fields.insert("event", item_type.to_owned());
    fields.insert("call_id", call_id);
    if let Some(name) = payload
        .get("name")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        fields.insert("name", name);
    }
    if let Some(arguments) = payload
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        fields.insert("arguments", arguments);
    } else if let Some(input) = payload
        .get("input")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        fields.insert("input", input);
    }
    Ok(Some(NormalizedEvent {
        kind: "tool",
        content: render_tool_fields(fields),
    }))
}

fn codex_tool_output(payload: &Map<String, Value>) -> Result<Option<NormalizedEvent>, SourceError> {
    let Some(call_id) = payload
        .get("call_id")
        .and_then(Value::as_str)
        .and_then(nonempty)
    else {
        return Ok(None);
    };
    let mut fields = BTreeMap::new();
    fields.insert("event", "function_call_output".to_owned());
    fields.insert("call_id", call_id);
    if let Some(output) = payload.get("output").and_then(extract_codex_output_text) {
        fields.insert("output", output);
    }
    Ok(Some(NormalizedEvent {
        kind: "tool",
        content: render_tool_fields(fields),
    }))
}

fn extract_codex_output_text(value: &Value) -> Option<String> {
    match value {
        // `function_call_output.output` is either a plain string or an array of
        // structured content items on the wire.
        Value::String(text) => nonempty(text),
        Value::Array(items) => {
            let parts: Vec<_> = items.iter().filter_map(extract_codex_output_text).collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(object) => object
            .get("content")
            .and_then(extract_codex_output_text)
            .or_else(|| {
                object
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(nonempty)
            }),
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

fn codex_local_shell_call(
    payload: &Map<String, Value>,
) -> Result<Option<NormalizedEvent>, SourceError> {
    let mut fields = BTreeMap::new();
    fields.insert("event", "local_shell_call".to_owned());
    if let Some(call_id) = payload
        .get("call_id")
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        fields.insert("call_id", call_id);
    }
    if let Some(command) = payload
        .get("action")
        .and_then(Value::as_object)
        .and_then(|action| action.get("command"))
        .and_then(compact_value)
    {
        fields.insert("command", command);
    }
    if fields.len() == 1 {
        return Ok(None);
    }
    Ok(Some(NormalizedEvent {
        kind: "tool",
        content: render_tool_fields(fields),
    }))
}

fn count_records(bytes: &[u8]) -> u64 {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count() as u64
}

fn validate_trailing_record(bytes: &[u8]) -> Result<(), SourceError> {
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return Ok(());
    }
    let tail = bytes
        .rsplit(|byte| *byte == b'\n')
        .next()
        .expect("a nonempty byte slice has a final segment");
    serde_json::from_slice::<Value>(tail)
        .map(|_| ())
        .map_err(|_| SourceError::IncompleteTrailingRecord)
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Bare lowercase-hex sha256, unprefixed — the content address used for extracted-output logical
/// paths and elision markers. Matches Patwari's stored digest hex (minus the `sha256:` prefix).
fn content_sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn event_data(object: &Map<String, Value>) -> Option<&Map<String, Value>> {
    object.get("data").and_then(Value::as_object)
}

fn validate_session_id(value: &str) -> Result<&str, SourceError> {
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
        Err(SourceError::InvalidSessionId)
    } else {
        Ok(value)
    }
}

fn parse_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn nonempty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn valid_tool_start(data: &Map<String, Value>) -> bool {
    data.get("toolCallId").is_some_and(Value::is_string)
        && data.get("toolName").is_some_and(Value::is_string)
}

fn valid_tool_complete(data: &Map<String, Value>) -> bool {
    if !data.get("toolCallId").is_some_and(Value::is_string)
        || !data.get("success").is_some_and(Value::is_boolean)
    {
        return false;
    }
    if let Some(result) = data.get("result") {
        if !valid_tool_result(result) {
            return false;
        }
    }
    if let Some(error) = data.get("error") {
        let Some(error) = error.as_object() else {
            return false;
        };
        if !error.get("message").is_some_and(Value::is_string) {
            return false;
        }
    }
    true
}

fn valid_tool_result(result: &Value) -> bool {
    let Some(result) = result.as_object() else {
        return false;
    };
    let has_content = match result.get("content") {
        Some(Value::String(_)) => true,
        Some(_) => return false,
        None => false,
    };
    let has_textual_contents = match result.get("contents") {
        Some(Value::Array(contents)) => contents
            .iter()
            .any(|content| extract_tool_result_text(content).is_some()),
        Some(_) => return false,
        None => false,
    };
    has_content || has_textual_contents
}

fn extract_tool_start(data: &Map<String, Value>) -> Result<String, SourceError> {
    let mut fields = BTreeMap::new();
    fields.insert("event", "tool.execution_start".to_owned());
    fields.insert(
        "tool_call_id",
        data["toolCallId"]
            .as_str()
            .expect("validated tool call ID")
            .to_owned(),
    );
    fields.insert(
        "name",
        data["toolName"]
            .as_str()
            .expect("validated tool name")
            .to_owned(),
    );
    if let Some(arguments) = data.get("arguments").and_then(compact_value) {
        fields.insert("arguments", arguments);
    }
    Ok(render_tool_fields(fields))
}

fn extract_tool_complete(data: &Map<String, Value>) -> Result<String, SourceError> {
    let mut fields = BTreeMap::new();
    fields.insert("event", "tool.execution_complete".to_owned());
    fields.insert(
        "tool_call_id",
        data["toolCallId"]
            .as_str()
            .expect("validated tool call ID")
            .to_owned(),
    );
    fields.insert(
        "success",
        data["success"]
            .as_bool()
            .expect("validated tool success")
            .to_string(),
    );
    if let Some(message) = data
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .and_then(nonempty)
    {
        fields.insert("error", message);
    }
    if let Some(result) = data.get("result").and_then(Value::as_object) {
        let mut output = Vec::new();
        if let Some(content) = result
            .get("content")
            .and_then(Value::as_str)
            .and_then(nonempty)
        {
            output.push(content);
        }
        if let Some(contents) = result.get("contents").and_then(Value::as_array) {
            output.extend(contents.iter().filter_map(extract_tool_result_text));
        }
        if !output.is_empty() {
            fields.insert("output", output.join("\n"));
        }
    }
    Ok(render_tool_fields(fields))
}

fn extract_tool_result_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => nonempty(text),
        Value::Array(items) => {
            let parts: Vec<_> = items.iter().filter_map(extract_tool_result_text).collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text" | "terminal") => object
                .get("text")
                .and_then(Value::as_str)
                .and_then(nonempty),
            Some("shell_exit") => object
                .get("outputPreview")
                .and_then(Value::as_str)
                .and_then(nonempty),
            Some("resource") => object
                .get("resource")
                .and_then(Value::as_object)
                .and_then(|resource| resource.get("text"))
                .and_then(Value::as_str)
                .and_then(nonempty),
            Some("resource_link" | "image" | "audio") => None,
            _ => None,
        },
        Value::Null | Value::Bool(_) | Value::Number(_) => None,
    }
}

fn render_tool_fields(fields: BTreeMap<&str, String>) -> String {
    fields
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => nonempty(text),
        Value::Null => None,
        Value::Array(_) | Value::Object(_) | Value::Bool(_) | Value::Number(_) => {
            serde_json::to_string(value).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copilot_tool_complete(call_id: &str, output: &str) -> String {
        serde_json::json!({
            "id": call_id,
            "timestamp": "2026-07-25T00:00:00Z",
            "parentId": "root",
            "type": "tool.execution_complete",
            "data": { "toolCallId": call_id, "success": true, "result": { "content": output } },
        })
        .to_string()
    }

    #[test]
    fn claim_ticket_marker_is_single_line_and_carries_hash_size_label() {
        let content = "x".repeat(500);
        let marker = claim_ticket_marker("tool", &content);
        assert!(!marker.contains('\n'), "the claim ticket is a single line");
        assert_eq!(
            marker,
            format!(
                "[munshi claim-ticket sha256:{} bytes:500 label:tool]",
                content_sha256_hex(content.as_bytes())
            )
        );
    }

    #[test]
    fn markers_and_artifact_index_are_deterministic_across_normalizations() {
        let threshold = 64;
        let transcript =
            format!("{}\n", copilot_tool_complete("call-1", &"a".repeat(300))).into_bytes();

        // The snapshot artifact index is a pure function of the transcript: two normalizations of
        // the same bytes produce byte-identical index entries (ADR 0010 determinism).
        let first = snapshot_artifact_index(&transcript, SourceKind::Copilot, threshold);
        let second = snapshot_artifact_index(&transcript, SourceKind::Copilot, threshold);
        assert_eq!(first, second);
        assert_eq!(first.extracted_outputs.len(), 1);
        let entry = &first.extracted_outputs[0];
        assert_eq!(entry.label, "tool");

        // The ticket the summarizer sees in the normalized events carries the same address, size,
        // and label as the matching artifact-index entry.
        let normalized = normalize_records(&transcript, 0, SourceKind::Copilot, threshold).unwrap();
        let ticket = normalized
            .events
            .iter()
            .find(|event| event.content.starts_with("[munshi claim-ticket"))
            .expect("the oversized event is replaced by a claim ticket");
        assert_eq!(
            ticket.content,
            format!(
                "[munshi claim-ticket sha256:{} bytes:{} label:{}]",
                entry.sha256, entry.bytes, entry.label
            )
        );
        // The address matches the content-addressed extracted output byte-for-byte.
        let outputs = extract_outputs(&transcript, SourceKind::Copilot, threshold);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].sha256, entry.sha256);
        assert_eq!(outputs[0].content.len() as u64, entry.bytes);
    }
}
