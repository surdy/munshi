use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::ops::ControlFlow;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use munshi_transcript::{Classification, RecordError, SessionSummary, TranscriptStream};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The default per-event extraction threshold: when a normalized event's content exceeds this many
/// bytes it is extracted as its own content-addressed snapshot artifact and elided from summarizer
/// input, rather than failing the load (ADR 0010). The default preserves the historical 128 KB cap
/// on per-event summarizer input size; it is configurable via `limits.max_event_text_bytes`.
pub const DEFAULT_MAX_EVENT_TEXT_BYTES: usize = 128 * 1024;
/// Version 3: the Copilot tool-activity kinds (`skill.invoked`, `tool.user_requested`,
/// `external_tool.requested`, `external_tool.completed`) became typed `tool` events
/// instead of unknowns (issue #51). The change is count-affecting — `tool_activities`
/// grows for sessions containing them — so any cursor persisted under an older version
/// falls back to a full re-normalization (`CursorFallbackReason::NormalizerChanged`).
pub const NORMALIZER_VERSION: u32 = 3;

/// The normalized event kind carrying a human request, as the shared transcript interpreter
/// labels it (`munshi_transcript::ContentEvent::kind`).
const USER_EVENT_KIND: &str = "user";

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

/// The capture-side [`SourceKind`] and the read-time [`munshi_transcript::Source`] name
/// the same three-harness identity; the crate defines its own copy so it never depends on
/// `munshi` (ADR 0011).
impl From<SourceKind> for munshi_transcript::Source {
    fn from(source: SourceKind) -> Self {
        match source {
            SourceKind::Copilot => Self::Copilot,
            SourceKind::ClaudeCode => Self::ClaudeCode,
            SourceKind::Codex => Self::Codex,
        }
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

/// The harness installations this registration manages, each identified by its home directory
/// (ADR 0008: the state directory is harness-neutral, so a home can no longer be derived from it).
///
/// This is the only place a transcript re-derivation is allowed to look. Nothing here falls back to
/// an ambient `$HOME`: a source with no registered home has no derivable transcript, which keeps
/// derivation confined to installations the operator explicitly registered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceHomes {
    /// Copilot home holding the version-pinned `session-state/<id>/events.jsonl` layout.
    pub copilot_home: Option<PathBuf>,
    /// Claude Code home holding the `projects/<project>/<session-id>.jsonl` layout.
    pub claude_home: Option<PathBuf>,
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
    /// Whether this session's very first user message is one of Munshi's own summary-request
    /// envelopes — the marker of a session a summarizer harness recorded while answering Munshi
    /// (issue #37). Set only on [`TranscriptLoadMode::Full`] loads, where the normalized batch
    /// really does start at the transcript's first record; a delta load's first user event is an
    /// arbitrary mid-session message, so the flag stays `false` there and callers must not read it
    /// as evidence either way.
    ///
    /// Decided before oversize elision, on the original event content: a summary request for a
    /// substantial session easily exceeds `max_event_text_bytes`, and the claim-ticket marker that
    /// replaces an elided event carries none of the envelope's text.
    pub opening_summary_request: bool,
}

impl NormalizedSession {
    pub fn is_archive_worthy(&self) -> bool {
        self.user_requests > 0 && (self.assistant_messages > 0 || self.tool_activities > 0)
    }

    /// Whether this session is a summarizer's own exhaust: a session some harness recorded purely
    /// because Munshi asked it for a summary (issue #37). See [`Self::opening_summary_request`] for
    /// the recognition rule and its full-load-only validity.
    pub fn is_summarizer_exhaust(&self) -> bool {
        self.opening_summary_request
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
    #[error("transcript does not match the version-pinned event envelope for its source")]
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

/// Line-scan bound for the envelope validation a re-derived path must pass. Envelope validation
/// reads only the first meaningful record, and its `max_source_bytes` argument caps that single
/// line (itself clamped to 256 KB), so this is a read-discipline bound and never a statement about
/// how large a transcript may be — matching the bound the recovery sweep's Copilot fallback uses.
const ENVELOPE_SCAN_BYTES: usize = 8 * 1024 * 1024;

/// Re-derives a session's transcript path from its session ID alone, using each source's own
/// version-pinned discovery machinery (issue #53).
///
/// A session row can hold no transcript path — `rebuild-state` reconstructs a session from its
/// archive Markdown, which never records one — or hold a path the harness has since moved. Both
/// leave a session that cannot be read, archived in full, or uploaded as a self-contained snapshot
/// even though its transcript is sitting on disk. Derivation is per source and never a guess:
///
/// - **Copilot** resolves through [`resolve_session_reference`]'s version-pinned
///   `session-state/<id>/events.jsonl` fallback, which canonicalizes and confines the result to the
///   registered home's session-state directory.
/// - **Claude Code** scans the registered home's `projects/*/` for `<session-id>.jsonl` exactly the
///   way the recovery sweep does, rejecting symlinked project directories and transcripts.
/// - **Codex** has no safe session-ID-only lookup — its rollout files are not named after the
///   session — so it never derives.
///
/// Every candidate must then match its source's pinned event envelope
/// ([`validate_transcript_envelope`]), so an unrelated file that merely occupies the expected path
/// is rejected rather than trusted. Failure is always `None`: derivation is an opportunistic
/// repair, never an error a caller has to handle.
pub fn derive_transcript_path(
    source: SourceKind,
    session_id: &str,
    homes: &SourceHomes,
) -> Option<PathBuf> {
    let session_id = validate_session_id(session_id).ok()?;
    let candidate = match source {
        SourceKind::Copilot => {
            resolve_session_reference(&SessionReference {
                source,
                session_id: Some(session_id.to_owned()),
                events_path: None,
                copilot_home: Some(homes.copilot_home.clone()?),
            })
            .ok()?
            .events_path
        }
        SourceKind::ClaudeCode => {
            // Canonical like the Copilot fallback's resolved path: the result is persisted on the
            // session row, so it should be a stable absolute path rather than one carrying whatever
            // `..` segments the registered home was configured with.
            find_claude_project_transcript(
                &homes.claude_home.as_ref()?.join("projects"),
                session_id,
            )?
            .canonicalize()
            .ok()?
        }
        SourceKind::Codex => return None,
    };
    validate_transcript_envelope(source, &candidate, ENVELOPE_SCAN_BYTES).ok()?;
    Some(candidate)
}

/// The transcript a Claude Code home's `projects/*/` layout holds for `session_id`, if any. Shares
/// [`for_each_claude_project_transcript`]'s scan — and therefore its safety discipline — with the
/// recovery sweep, and stops at the first match. An I/O error part-way through simply yields no
/// path, matching the rest of derivation's opportunistic contract.
fn find_claude_project_transcript(claude_projects: &Path, session_id: &str) -> Option<PathBuf> {
    let mut found = None;
    let _ =
        for_each_claude_project_transcript::<io::Error, _>(claude_projects, |candidate, path| {
            if candidate != session_id {
                return Ok(ControlFlow::Continue(()));
            }
            found = Some(path.to_path_buf());
            Ok(ControlFlow::Break(()))
        });
    found
}

/// Visits every transcript a Claude Code home's `projects/*/` layout holds, passing each session ID
/// and its explicit path to `visit` until the walk is exhausted or `visit` breaks.
///
/// This is the single description of where Claude Code keeps its transcripts, shared by the
/// recovery sweep (which reserves the unknown sessions it finds) and by
/// [`derive_transcript_path`] (which looks one session up). Sessions are regular
/// `<session-id>.jsonl` files inside per-project subdirectories; sibling `<uuid>/` directories and
/// entries like `memory/` are not sessions and are skipped by the file-type and extension checks.
/// Symlinked project directories and symlinked transcripts are refused outright, and every yielded
/// stem has passed [`validate_session_id`] — so a caller always receives an explicit, in-home path,
/// preserving the "no session-ID-only transcript lookup for Claude Code" rule.
pub(crate) fn for_each_claude_project_transcript<E, F>(
    claude_projects: &Path,
    mut visit: F,
) -> Result<(), E>
where
    E: From<io::Error>,
    F: FnMut(&str, &Path) -> Result<ControlFlow<()>, E>,
{
    if !claude_projects.is_dir() {
        return Ok(());
    }
    for project_entry in fs::read_dir(claude_projects).map_err(E::from)? {
        let project_entry = project_entry.map_err(E::from)?;
        if project_entry.file_type().map_err(E::from)?.is_symlink()
            || !project_entry.metadata().map_err(E::from)?.is_dir()
        {
            continue;
        }
        for entry in fs::read_dir(project_entry.path()).map_err(E::from)? {
            let entry = entry.map_err(E::from)?;
            if entry.file_type().map_err(E::from)?.is_symlink()
                || !entry.metadata().map_err(E::from)?.is_file()
            {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(session_id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| validate_session_id(stem).is_ok())
                .map(ToOwned::to_owned)
            else {
                continue;
            };
            if visit(&session_id, &path)?.is_break() {
                return Ok(());
            }
        }
    }
    Ok(())
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
                previous.user_requests + normalized.summary.user_requests,
                previous.assistant_messages + normalized.summary.assistant_messages,
                previous.tool_activities + normalized.summary.tool_activities,
                previous
                    .started_at
                    .clone()
                    .or_else(|| normalized.summary.started_at()),
                normalized
                    .summary
                    .updated_at()
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
                normalized.summary.user_requests,
                normalized.summary.assistant_messages,
                normalized.summary.tool_activities,
                normalized.summary.started_at(),
                normalized.summary.updated_at(),
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
            ignored_events: normalized.summary.ignored_events,
            source_cursor: total_records,
            source_byte_cursor: bytes.len() as u64,
            source_prefix_hash,
            source_hash,
            source_bytes: bytes.len() as u64,
            started_at,
            updated_at,
            artifact_index,
            // Only a full load's batch begins at the transcript's first record, so only a full
            // load can testify about the session's *first* user message (issue #37).
            opening_summary_request: mode == TranscriptLoadMode::Full
                && normalized.opening_summary_request,
        },
        mode,
        fallback_reason,
        snapshot,
    })
}

/// Bounded, privacy-safe read of a Claude Code transcript's origin project directory: the first
/// absolute top-level `cwd` value among the leading records (bookkeeping records such as
/// `queue-operation` may precede the first turn and lack one). The pinned-`cwd` format predicate is
/// [`munshi_transcript::claude_origin_cwd`]; this wrapper keeps only the I/O discipline — symlink
/// rejection, a bounded number of bounded-size lines — mirroring the envelope-validation read
/// discipline; content is never read. Lets the recovery sweep hand `mark_recovery_interrupted` an
/// origin so swept sessions can compute project identity instead of parking as origin-unresolved.
pub fn claude_transcript_origin(path: &Path) -> Option<PathBuf> {
    claude_transcript_recorded_origin(path).map(|origin| origin.cwd)
}

/// Recorded origin evidence a Claude Code transcript carries in its leading records
/// (issue #40): the origin `cwd` plus, when present, the recorded `gitBranch` — the branch
/// that was checked out when the record was written. This is the evidence the
/// recorded-origin fallback derives a project identity from once the origin directory no
/// longer exists on disk, so nothing here touches the filesystem beyond the transcript
/// itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeRecordedOrigin {
    pub cwd: PathBuf,
    pub git_branch: Option<String>,
}

/// Bounded read of the recorded origin evidence behind [`claude_transcript_origin`]: the
/// same I/O discipline (symlink rejection, a bounded number of bounded-size leading lines)
/// scanning for the pinned [`munshi_transcript::claude_origin_cwd`] and
/// [`munshi_transcript::claude_git_branch`] keys. The scan stops at the first record
/// carrying both, and a window that yields a `cwd` but no branch still returns the cwd —
/// branch evidence is optional provenance, never a gate.
pub fn claude_transcript_recorded_origin(path: &Path) -> Option<ClaudeRecordedOrigin> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let mut reader = BufReader::new(File::open(path).ok()?);
    let limit = 256 * 1024;
    let mut line = Vec::new();
    let mut cwd: Option<PathBuf> = None;
    let mut git_branch: Option<String> = None;
    for _ in 0..32 {
        line.clear();
        let Ok(read) = reader
            .by_ref()
            .take(limit as u64 + 1)
            .read_until(b'\n', &mut line)
        else {
            break;
        };
        if read == 0 || line.len() > limit {
            break;
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
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            break;
        };
        let Some(object) = value.as_object() else {
            break;
        };
        if cwd.is_none() {
            cwd = munshi_transcript::claude_origin_cwd(object).map(PathBuf::from);
        }
        if git_branch.is_none() {
            git_branch = munshi_transcript::claude_git_branch(object).map(ToOwned::to_owned);
        }
        if cwd.is_some() && git_branch.is_some() {
            break;
        }
    }
    cwd.map(|cwd| ClaudeRecordedOrigin { cwd, git_branch })
}

/// Bounded, privacy-safe read of a Copilot session's origin project directory: the top-level
/// `cwd` scalar of the `workspace.yaml` beside the session's `events.jsonl` in the
/// version-pinned `session-state/<id>/` layout (observed on installed Copilot CLI 1.0.7x,
/// where every session directory carries one). Copilot transcripts themselves declare no
/// origin, so this sibling record is the only origin evidence once the hook-provided `cwd`
/// is gone — it is what lets recovery hydrate rebuilt or swept sessions instead of parking
/// them as origin-unresolved. Read discipline mirrors [`claude_transcript_origin`]: symlink
/// rejection, a small size cap, a bounded number of leading lines, and only the pinned key
/// is interpreted — no other workspace content is read. A missing file, foreign layout, or
/// non-absolute value yields `None` rather than a guess.
pub fn copilot_workspace_origin(events_path: &Path) -> Option<PathBuf> {
    let workspace = events_path.parent()?.join("workspace.yaml");
    let metadata = fs::symlink_metadata(&workspace).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    let contents = fs::read_to_string(&workspace).ok()?;
    for line in contents.lines().take(64) {
        let Some(value) = line.strip_prefix("cwd:") else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|inner| inner.strip_suffix('\''))
            })
            .unwrap_or(value);
        return Path::new(value).is_absolute().then(|| PathBuf::from(value));
    }
    None
}

/// Validates that the first meaningful record of `path` matches `source`'s version-pinned
/// envelope. The structural predicate is [`munshi_transcript::envelope_matches`] (ADR 0011,
/// issue #27); this wrapper keeps only the I/O discipline — symlink rejection, the bounded
/// first-line read, and the line-size cap.
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
        if !munshi_transcript::envelope_matches(source.into(), object) {
            return Err(SourceError::UnsupportedEnvelope);
        }
        return Ok(());
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

/// One normalization pass folded off the shared transcript stream: the (possibly elided)
/// normalized events plus the crate's [`SessionSummary`] counting fold, which carries the
/// event counts, the lumped ignored count, and the started/updated timestamp window.
#[derive(Default)]
struct NormalizedRecords {
    events: Vec<NormalizedEvent>,
    summary: SessionSummary,
    /// Whether the batch's first user event carried one of Munshi's own summary-request envelopes
    /// (issue #37), judged on the original content before oversize elision. Only the caller knows
    /// whether the batch starts at the transcript's first record, so only a full load promotes this
    /// to [`NormalizedSession::opening_summary_request`].
    opening_summary_request: bool,
}

/// Opens the shared streaming interpreter (ADR 0011) over in-memory transcript bytes,
/// keyed by the capture source and the artifact-set version capture provenance records
/// ([`crate::patwari::INITIAL_ARTIFACT_SET_VERSION`]). Both are compile-time constants, so
/// an unsupported pairing is a build defect, not a runtime condition.
fn transcript_stream(source: SourceKind, bytes: &[u8]) -> TranscriptStream<&[u8]> {
    TranscriptStream::new(
        source.into(),
        crate::patwari::INITIAL_ARTIFACT_SET_VERSION,
        bytes,
    )
    .expect("munshi-transcript supports the capture artifact-set version")
}

/// Folds the shared transcript stream (ADR 0011, issue #27) into the legacy normalized shape:
/// a [`NormalizedEvent`] per content event (from its kind and byte-identical `legacy_content`),
/// with counts and the timestamp window taken from the crate's [`SessionSummary`] fold.
///
/// The stream itself is lossless; strictness is capture-side policy. The first malformed record
/// aborts the whole load as [`SourceError::MalformedJson`], numbered exactly like the legacy
/// whole-parse normalizer: 1-based among non-empty lines, offset by `prior_records` on delta
/// loads.
fn normalize_records(
    bytes: &[u8],
    prior_records: u64,
    source: SourceKind,
    max_event_text_bytes: usize,
) -> Result<NormalizedRecords, SourceError> {
    let mut normalized = NormalizedRecords::default();
    let mut seen_user_event = false;
    for item in transcript_stream(source, bytes) {
        normalized.summary.observe(&item);
        let record = item.map_err(|error| match error {
            RecordError::MalformedJson { record, .. } => SourceError::MalformedJson {
                line: prior_records + record,
            },
            RecordError::Io { source, .. } => SourceError::Io(source),
        })?;
        if let Classification::Content { events } = record.classification {
            for event in events {
                let event = NormalizedEvent {
                    kind: event.kind(),
                    content: event.legacy_content(),
                };
                // Issue #37: judge the summarizer-exhaust marker on the first user event's
                // original content, before elision below can replace it with a claim-ticket
                // marker that carries none of the envelope's text.
                if event.kind == USER_EVENT_KIND && !seen_user_event {
                    seen_user_event = true;
                    normalized.opening_summary_request =
                        crate::summary::is_summary_request_envelope(&event.content);
                }
                // Oversized content is extracted, not truncated: the full bytes become an
                // `outputs/<sha256>` snapshot artifact (re-derived at upload time from the same
                // transcript, see `extract_outputs`) and the summarizer sees a hash+size marker in
                // their place. The activity counts in the summary reflect the original event
                // either way.
                normalized
                    .events
                    .push(elide_if_oversized(event, max_event_text_bytes));
            }
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
/// `outputs/<sha256>` resolves to exactly the bytes the summarizer's elision marker names. Unlike
/// the strict load, malformed records (per-record stream errors) are skipped so a partial trailing
/// record appended after archival never fails the upload.
pub fn extract_outputs(
    bytes: &[u8],
    source: SourceKind,
    max_event_text_bytes: usize,
) -> Vec<ExtractedOutput> {
    let mut extracted: BTreeMap<String, ExtractedOutput> = BTreeMap::new();
    for item in transcript_stream(source, bytes) {
        let Ok(record) = item else {
            continue;
        };
        let Classification::Content { events } = record.classification else {
            continue;
        };
        for event in events {
            let content = event.legacy_content();
            if content.len() <= max_event_text_bytes {
                continue;
            }
            let sha256 = content_sha256_hex(content.as_bytes());
            extracted
                .entry(sha256.clone())
                .or_insert_with(|| ExtractedOutput {
                    sha256,
                    media_type: Some("text/plain; charset=utf-8".to_owned()),
                    label: event.kind().to_owned(),
                    content: content.into_bytes(),
                });
        }
    }
    extracted.into_values().collect()
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
