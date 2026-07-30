use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use munshi_runner::{RunnerConfig, RunnerError, run_bounded};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::project::ProjectIdentity;
use crate::source::NormalizedSession;

const TITLE_LIMIT: usize = 200;
const GOAL_LIMIT: usize = 4_000;
const ITEM_LIMIT: usize = 4_000;
const TAG_LIMIT: usize = 100;
const LIST_LIMIT: usize = 200;

/// Current summarizer request-envelope version (issue #48). Version 2 added `contract_version`,
/// `phase`, and the chunked map-reduce fields (`chunk`, `chunk_summaries`) as a strictly additive
/// change: a below-threshold session's request differs from v1 only by the two new marker fields,
/// so phase-unaware wrappers that pipe the request through unchanged keep working.
pub const SUMMARY_CONTRACT_VERSION: u32 = 2;

/// Environment variable carrying the request's `phase` value on every summarizer invocation, so
/// shell wrappers can select per-phase behavior (for example a different model) without parsing
/// the request JSON. Absent under pre-v2 versions of Munshi; wrappers should default to the
/// one-shot behavior when unset.
pub const SUMMARIZER_PHASE_ENV: &str = "MUNSHI_SUMMARIZER_PHASE";

/// Environment-variable namespace reserved for Munshi's own per-invocation markers (currently
/// [`SUMMARIZER_PHASE_ENV`]). Operator-configured summarizer environment must stay outside it so
/// a configured value can never shadow — or appear to shadow — a variable Munshi itself exports.
pub const RESERVED_SUMMARIZER_ENV_PREFIX: &str = "MUNSHI_SUMMARIZER_";

/// Parses one repeatable `--summarizer-env KEY=VALUE` assignment into a `(key, value)` pair.
/// The key must be non-empty and everything after the first `=` is the value verbatim (so values
/// may contain `=`, keys cannot). Keys in Munshi's own reserved namespace
/// ([`RESERVED_SUMMARIZER_ENV_PREFIX`]) are rejected: Munshi's per-invocation variables always
/// win, so accepting them would only record configuration that can never take effect.
pub fn parse_summarizer_env(assignment: &str) -> Result<(String, String), String> {
    let (key, value) = assignment
        .split_once('=')
        .ok_or_else(|| "expected KEY=VALUE".to_owned())?;
    if key.is_empty() {
        return Err("the key before `=` must be non-empty".to_owned());
    }
    if key.starts_with(RESERVED_SUMMARIZER_ENV_PREFIX) {
        return Err(format!(
            "`{RESERVED_SUMMARIZER_ENV_PREFIX}*` keys are reserved for Munshi's own \
             per-invocation variables (such as {SUMMARIZER_PHASE_ENV})"
        ));
    }
    Ok((key.to_owned(), value.to_owned()))
}

/// Rejects an inverted summarizer-size relation (issue #52): `max_input_bytes` must be at least
/// `chunk_threshold_bytes`.
///
/// The two knobs are not peers. Since issue #48 the threshold is the *operative* bound — the
/// measured one-shot request size above which a session is chunked, and the ceiling on every
/// single chunk/reduce request — while the input cap is an absolute never-exceed backstop that
/// only stops pathological input from reaching the summarizer at all. Above the threshold the cap
/// is unreachable by construction (over-threshold requests chunk), so a cap *below* the threshold
/// is the only way it binds, and it binds destructively: it silently recreates the pre-issue-#48
/// band in which every request between the two values fails deterministically on
/// `SummaryError::InputLimit` and floors to a placeholder summary instead of being chunked or
/// summarized. Validated on both configuring surfaces (`munshi register`, `munshi archive`) before
/// any work or config write; `munshi doctor` re-checks a hand-edited configuration.
pub fn validate_input_cap_relation(
    max_input_bytes: usize,
    chunk_threshold_bytes: usize,
) -> Result<(), String> {
    if max_input_bytes >= chunk_threshold_bytes {
        return Ok(());
    }
    Err(format!(
        "--max-input-bytes ({max_input_bytes}) must be at least chunk_threshold_bytes \
         ({chunk_threshold_bytes}, set by --chunk-threshold-bytes at registration): the input cap \
         is an absolute never-exceed backstop that sits above the chunking threshold, and a \
         smaller cap would needlessly floor every request between the two values to a placeholder \
         summary instead of chunking or summarizing it"
    ))
}

#[derive(Debug, Clone)]
pub struct SummarizerConfig {
    pub binary: PathBuf,
    pub args: Vec<OsString>,
    /// Operator-configured environment set on every summarizer invocation (`summarizer.env` /
    /// `--summarizer-env`). Opaque to Munshi — it defines no keys itself; the wrapper contract
    /// (docs/summarizers.md) gives them meaning. Munshi's own per-invocation variables are merged
    /// after these and win on any conflict.
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
}

/// Tag carried by every machine-generated placeholder summary (issue #43), so a placeholder is
/// recognizable from the summary alone — in the archive frontmatter, the state database, and any
/// delivered note — without consulting operational state.
pub const PLACEHOLDER_SUMMARY_TAG: &str = "munshi-placeholder-summary";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredSummary {
    pub title: String,
    pub goal: String,
    pub work_completed: Vec<String>,
    pub decisions: Vec<String>,
    pub files_changed: Vec<String>,
    pub commands_and_validation: Vec<String>,
    pub open_items: Vec<String>,
    pub tags: Vec<String>,
}

impl StructuredSummary {
    /// Whether this is a machine-generated placeholder (issue #43) rather than a real summary.
    pub fn is_placeholder(&self) -> bool {
        self.tags.iter().any(|tag| tag == PLACEHOLDER_SUMMARY_TAG)
    }
}

/// The deterministic input-capacity class that triggered a placeholder archival (issue #43):
/// either the summarizer process itself rejected the input (a repeat-failure park, issue #38) or
/// Munshi's own `max_input_bytes` cap refused to build the input at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderReason {
    SummarizerRejected,
    InputCapExceeded,
}

impl PlaceholderReason {
    /// The human marker line carried in the placeholder's goal and work list.
    pub fn marker(self) -> &'static str {
        match self {
            Self::SummarizerRejected => {
                "Summary unavailable: summarizer rejected oversized input (munshi#43)."
            }
            Self::InputCapExceeded => concat!(
                "Summary unavailable: normalized input exceeds the configured summarizer ",
                "input limit (munshi#43)."
            ),
        }
    }
}

/// Builds the machine-generated placeholder summary the durability floor archives (issue #43).
/// The title comes from session metadata only, the lists carry an explicit self-describing
/// marker, and the [`PLACEHOLDER_SUMMARY_TAG`] makes the placeholder recognizable everywhere the
/// summary travels. Always passes [`validate_structured_summary`] so it renders, re-parses, and
/// delivers exactly like a real summary.
pub fn placeholder_summary(
    agent_label: &str,
    session_id: &str,
    reason: PlaceholderReason,
) -> StructuredSummary {
    let marker = reason.marker();
    StructuredSummary {
        title: format!("{agent_label} session {session_id} (summary unavailable)"),
        goal: format!(
            "{marker} The full transcript is archived unchanged; run `munshi retry {session_id}` \
             to attempt a real summary, which replaces this placeholder."
        ),
        work_completed: vec![marker.to_owned()],
        decisions: Vec::new(),
        files_changed: Vec::new(),
        commands_and_validation: Vec::new(),
        open_items: Vec::new(),
        tags: vec![PLACEHOLDER_SUMMARY_TAG.to_owned()],
    }
}

/// Which kind of summarizer invocation a request represents (issue #48). `Complete` is the
/// one-shot contract every session used before chunking; `Chunk` summarizes one segment of an
/// oversized session; `Reduce` synthesizes segment summaries into one session summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryPhase {
    Complete,
    Chunk,
    Reduce,
}

impl SummaryPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Chunk => "chunk",
            Self::Reduce => "reduce",
        }
    }
}

/// The chunked map-reduce size limits (issue #48). `chunk_threshold_bytes` is the measured
/// one-shot request size above which a session is summarized in chunks — and, symmetrically, the
/// hard cap on any single chunk/reduce request, since it marks the empirically calibrated point
/// past which real summarizer backends reject input. `chunk_size_bytes` is the approximate
/// serialized-events payload each chunk request targets.
#[derive(Debug, Clone, Copy)]
pub struct ChunkingLimits {
    pub chunk_threshold_bytes: usize,
    pub chunk_size_bytes: usize,
}

/// How one session's summary will be produced, decided from the measured size of the real
/// one-shot request (issue #48): either the request itself, ready to pipe, or the decision to
/// take the chunked map-reduce path.
#[derive(Debug)]
pub enum SummaryStrategy {
    OneShot(Vec<u8>),
    Chunked,
}

#[derive(Debug, Serialize)]
struct SummaryRequest<'a> {
    contract_version: u32,
    phase: &'static str,
    instruction: &'static str,
    required_schema: RequiredSchema,
    session: SummarySession<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk: Option<ChunkEnvelope<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_summary: Option<&'a StructuredSummary>,
    events: &'a [crate::source::NormalizedEvent],
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk_summaries: Option<&'a [StructuredSummary]>,
    ignored_unknown_event_count: usize,
}

/// The per-segment context of a `phase: "chunk"` request: this segment's 1-based ordinal, the
/// total segment count, and — for continuity — the previous segment's accepted summary. The
/// carried summary is bounded by construction: it already passed [`validate_structured_summary`],
/// and the built request is still measured against the chunk threshold before it is sent.
#[derive(Debug, Serialize)]
struct ChunkEnvelope<'a> {
    index: usize,
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_chunk_summary: Option<&'a StructuredSummary>,
}

#[derive(Debug, Serialize)]
struct RequiredSchema {
    title: &'static str,
    goal: &'static str,
    work_completed: &'static str,
    decisions: &'static str,
    files_changed: &'static str,
    commands_and_validation: &'static str,
    open_items: &'static str,
    tags: &'static str,
}

#[derive(Debug, Serialize)]
struct SummarySession<'a> {
    id: String,
    source_agent: &'static str,
    session_id: &'a str,
    project_identity: &'a str,
    repository: Option<&'a str>,
}

#[derive(Debug, Error)]
pub enum SummaryError {
    #[error("failed to construct summarizer input")]
    InputSerialization(#[source] serde_json::Error),
    #[error("normalized summarizer input exceeds the configured {limit}-byte limit")]
    InputLimit { limit: usize },
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error("summary stdout was not one valid JSON object")]
    MalformedJson(#[source] serde_json::Error),
    #[error("summary field {field} must contain between 1 and {max} characters")]
    InvalidText { field: &'static str, max: usize },
    #[error("summary list {field} exceeds its {max}-item limit")]
    InvalidListLength { field: &'static str, max: usize },
    #[error("summary list {field} must contain at least one item")]
    MissingListItems { field: &'static str },
}

pub fn build_summary_input(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    input_limit: usize,
) -> Result<Vec<u8>, SummaryError> {
    build_summary_input_inner(session, project, None, input_limit)
}

pub fn build_revision_summary_input(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    previous_summary: &StructuredSummary,
    input_limit: usize,
) -> Result<Vec<u8>, SummaryError> {
    build_summary_input_inner(session, project, Some(previous_summary), input_limit)
}

fn build_summary_input_inner(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    previous_summary: Option<&StructuredSummary>,
    input_limit: usize,
) -> Result<Vec<u8>, SummaryError> {
    let bytes = serialize_one_shot_request(session, project, previous_summary)?;
    if bytes.len() > input_limit {
        return Err(SummaryError::InputLimit { limit: input_limit });
    }
    Ok(bytes)
}

const COMPLETE_INSTRUCTION: &str = concat!(
    "Summarize this coding session as exactly one JSON object matching required_schema. ",
    "Return every required field. Capture goals, meaningful completed work, decisions, ",
    "files changed, commands and validation, and open items. Do not quote prompts, raw ",
    "tool output, secrets, or substantial code. Use concise strings and arrays of strings. ",
    "Return JSON only, with no Markdown fence or commentary."
);

const REVISION_INSTRUCTION: &str = concat!(
    "Revise previous_summary using only the new normalized events. Return a complete ",
    "replacement summary as exactly one JSON object matching required_schema, not a ",
    "patch or append-only fragment. Preserve still-correct prior work and incorporate ",
    "new goals, meaningful completed work, decisions, files changed, commands and ",
    "validation, and open items. Do not quote prompts, raw tool output, secrets, or ",
    "substantial code. Return JSON only, with no Markdown fence or commentary."
);

const CHUNK_INSTRUCTION: &str = concat!(
    "This request covers one SEGMENT of a longer coding session: segment chunk.index of ",
    "chunk.count, in order. Summarize ONLY the events in this segment as exactly one JSON ",
    "object matching required_schema. Return every required field. If chunk.previous_chunk_summary ",
    "is present it is the summary of the immediately preceding segment: use it for continuity of ",
    "goals and terminology, but do not restate its work as this segment's. Capture this segment's ",
    "goals, meaningful completed work, decisions, files changed, commands and validation, and open ",
    "items. Do not quote prompts, raw tool output, secrets, or substantial code. Use concise ",
    "strings and arrays of strings. Return JSON only, with no Markdown fence or commentary."
);

const REDUCE_INSTRUCTION: &str = concat!(
    "chunk_summaries holds per-segment summaries of ONE coding session, in segment order. ",
    "Synthesize them into exactly one JSON object matching required_schema that summarizes the ",
    "ENTIRE session: merge duplicate work, keep decisions and open items that still stand, and ",
    "unify files_changed, commands_and_validation, and tags across segments. Base the summary ",
    "only on the segment summaries; do not quote or invent raw session content beyond them. ",
    "Return JSON only, with no Markdown fence or commentary."
);

const REDUCE_REVISION_INSTRUCTION: &str = concat!(
    "chunk_summaries holds per-segment summaries of the new events of ONE resumed coding ",
    "session, in segment order, and previous_summary is the session's last accepted summary. ",
    "Synthesize them into exactly one JSON object matching required_schema that is a complete ",
    "replacement summary of the ENTIRE session: preserve still-correct prior work and ",
    "incorporate the segment summaries' new work, decisions, files changed, commands and ",
    "validation, and open items. Base the summary only on previous_summary and the segment ",
    "summaries; do not quote or invent raw session content beyond them. Return JSON only, with ",
    "no Markdown fence or commentary."
);

/// Every `instruction` value Munshi can put in a summary request, in one place so the
/// summarizer-exhaust guard below matches on the exact strings Munshi emits rather than a copy
/// that could drift from them.
const REQUEST_INSTRUCTIONS: [&str; 5] = [
    COMPLETE_INSTRUCTION,
    REVISION_INSTRUCTION,
    CHUNK_INSTRUCTION,
    REDUCE_INSTRUCTION,
    REDUCE_REVISION_INSTRUCTION,
];

/// The `"instruction":"` key of a serialized request. Munshi serializes requests compactly
/// (`serde_json::to_vec`), so the key and its value are always adjacent with no whitespace.
const INSTRUCTION_KEY: &str = "\"instruction\":\"";

/// Whether `text` is one of Munshi's own summary-request envelopes — the JSON object Munshi writes
/// to a summarizer's stdin (docs/summarizers.md "The input request").
///
/// This is the recognizer behind the summarizer-exhaust guard (issue #37). A summarizer that is
/// itself a session-recording harness records Munshi's request as the first user message of a
/// brand-new session of its own; if that session lands in a registered harness home, Munshi
/// discovers it as fresh work and summarizing N sessions creates N more.
///
/// Recognition is deliberately keyed on the `instruction` value rather than on the envelope's
/// leading bytes: the v1 envelope opened with `{"instruction":"…` while the v2 envelope
/// ([`SUMMARY_CONTRACT_VERSION`]) opens with `contract_version` and `phase` before it, and a
/// wrapper may hand the request to its harness with a prefix of its own. Matching the full
/// instruction text — which is a fixed several-hundred-character literal — keeps that tolerance
/// without making a false positive plausible.
pub fn is_summary_request_envelope(text: &str) -> bool {
    let Some(start) = text.find(INSTRUCTION_KEY) else {
        return false;
    };
    let value = &text[start + INSTRUCTION_KEY.len()..];
    REQUEST_INSTRUCTIONS
        .iter()
        .any(|instruction| value.starts_with(instruction))
}

fn required_schema() -> RequiredSchema {
    RequiredSchema {
        title: "non-empty string",
        goal: "non-empty string",
        work_completed: "array of strings",
        decisions: "array of strings",
        files_changed: "array of strings",
        commands_and_validation: "array of strings",
        open_items: "array of strings",
        tags: "array of strings",
    }
}

fn summary_session<'a>(
    session: &'a NormalizedSession,
    project: &'a ProjectIdentity,
) -> SummarySession<'a> {
    SummarySession {
        id: format!("{}:{}", session.source.id_prefix(), session.session_id),
        source_agent: session.source.agent_label(),
        session_id: &session.session_id,
        project_identity: &project.identity,
        repository: project.repository.as_deref(),
    }
}

fn serialize_request(request: &SummaryRequest<'_>) -> Result<Vec<u8>, SummaryError> {
    serde_json::to_vec(request).map_err(SummaryError::InputSerialization)
}

/// The exact one-shot (`phase: "complete"`) request for this session, unmeasured. This is the
/// request-size authority: chunking decisions measure these bytes rather than estimating from a
/// heuristic, so the trigger and what would actually be piped can never disagree.
fn serialize_one_shot_request(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    previous_summary: Option<&StructuredSummary>,
) -> Result<Vec<u8>, SummaryError> {
    serialize_request(&SummaryRequest {
        contract_version: SUMMARY_CONTRACT_VERSION,
        phase: SummaryPhase::Complete.as_str(),
        instruction: if previous_summary.is_some() {
            REVISION_INSTRUCTION
        } else {
            COMPLETE_INSTRUCTION
        },
        required_schema: required_schema(),
        session: summary_session(session, project),
        chunk: None,
        previous_summary,
        events: &session.events,
        chunk_summaries: None,
        ignored_unknown_event_count: session.ignored_events,
    })
}

fn serialize_chunk_request(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    events: &[crate::source::NormalizedEvent],
    index: usize,
    count: usize,
    previous_chunk_summary: Option<&StructuredSummary>,
) -> Result<Vec<u8>, SummaryError> {
    serialize_request(&SummaryRequest {
        contract_version: SUMMARY_CONTRACT_VERSION,
        phase: SummaryPhase::Chunk.as_str(),
        instruction: CHUNK_INSTRUCTION,
        required_schema: required_schema(),
        session: summary_session(session, project),
        chunk: Some(ChunkEnvelope {
            index,
            count,
            previous_chunk_summary,
        }),
        previous_summary: None,
        events,
        chunk_summaries: None,
        ignored_unknown_event_count: session.ignored_events,
    })
}

fn serialize_reduce_request(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    chunk_summaries: &[StructuredSummary],
    previous_summary: Option<&StructuredSummary>,
) -> Result<Vec<u8>, SummaryError> {
    serialize_request(&SummaryRequest {
        contract_version: SUMMARY_CONTRACT_VERSION,
        phase: SummaryPhase::Reduce.as_str(),
        instruction: if previous_summary.is_some() {
            REDUCE_REVISION_INSTRUCTION
        } else {
            REDUCE_INSTRUCTION
        },
        required_schema: required_schema(),
        session: summary_session(session, project),
        chunk: None,
        previous_summary,
        events: &[],
        chunk_summaries: Some(chunk_summaries),
        ignored_unknown_event_count: session.ignored_events,
    })
}

/// Decides how this session will be summarized (issue #48), from the measured size of the real
/// one-shot request:
///
/// - at or below `chunk_threshold_bytes` and `input_limit`: one shot, request ready to pipe —
///   byte-identical to the pre-chunking contract apart from the additive v2 envelope fields;
/// - over `chunk_threshold_bytes`: the chunked map-reduce path, regardless of `input_limit` —
///   this replaces the input-limit placeholder floor for chunkable marathon sessions;
/// - over `input_limit` but not over the chunk threshold: [`SummaryError::InputLimit`], exactly
///   the deterministic verdict the issue #43 floor archives under. Munshi's own input cap keeps
///   governing the one-shot path unchanged.
pub fn plan_summary_input(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    previous_summary: Option<&StructuredSummary>,
    input_limit: usize,
    chunking: &ChunkingLimits,
) -> Result<SummaryStrategy, SummaryError> {
    let bytes = serialize_one_shot_request(session, project, previous_summary)?;
    if bytes.len() > chunking.chunk_threshold_bytes {
        return Ok(SummaryStrategy::Chunked);
    }
    if bytes.len() > input_limit {
        return Err(SummaryError::InputLimit { limit: input_limit });
    }
    Ok(SummaryStrategy::OneShot(bytes))
}

/// Splits a normalized event stream on event (record) boundaries into contiguous, in-order,
/// non-empty index ranges whose serialized-events payload approximates `chunk_size_bytes` each.
/// Never splits inside an event: an event larger than the target gets a range of its own (the
/// built request is later measured against the chunk threshold, which decides whether such an
/// event is genuinely unchunkable). Deterministic in the events and the target alone.
pub fn chunk_event_ranges(
    events: &[crate::source::NormalizedEvent],
    chunk_size_bytes: usize,
) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut used = 0usize;
    for (position, event) in events.iter().enumerate() {
        // Exact per-record serialized size plus the array separator, from the same serializer
        // that builds the request.
        let size = serde_json::to_vec(event).map_or(event.content.len(), |bytes| bytes.len()) + 1;
        if position > start && used + size > chunk_size_bytes {
            ranges.push(start..position);
            start = position;
            used = 0;
        }
        used += size;
    }
    if start < events.len() {
        ranges.push(start..events.len());
    }
    ranges
}

/// Runs the chunked map-reduce summarization of one oversized session (issue #48): one bounded
/// summarizer invocation per chunk (each carrying the previous chunk's summary for continuity),
/// then reduce invocations over the chunk summaries, recursing while the reduce input itself
/// exceeds the chunk threshold. `reserve_call` runs before every invocation so each one is
/// individually charged against the per-project budget; any failure aborts the whole attempt —
/// no partial summary is ever returned — and surfaces through the caller's normal
/// backoff/park/floor machinery. A request no split can bring under the chunk threshold (a
/// single oversized event, or an irreducible reduce input) fails with
/// [`SummaryError::InputLimit`]: the deterministic, genuinely unchunkable verdict the issue #43
/// placeholder floor still archives under.
pub fn run_chunked_summary<E: From<SummaryError>>(
    config: &SummarizerConfig,
    session: &NormalizedSession,
    project: &ProjectIdentity,
    previous_summary: Option<&StructuredSummary>,
    chunking: &ChunkingLimits,
    reserve_call: &mut dyn FnMut() -> Result<(), E>,
) -> Result<StructuredSummary, E> {
    let unchunkable = || {
        E::from(SummaryError::InputLimit {
            limit: chunking.chunk_threshold_bytes,
        })
    };
    // Target no more than the per-invocation cap, so a misconfigured chunk size larger than the
    // threshold cannot make every chunk unchunkable by construction.
    let target = chunking
        .chunk_size_bytes
        .min(chunking.chunk_threshold_bytes);
    let ranges = chunk_event_ranges(&session.events, target);
    let count = ranges.len();
    let mut summaries = Vec::with_capacity(count);
    for (position, range) in ranges.into_iter().enumerate() {
        let input = serialize_chunk_request(
            session,
            project,
            &session.events[range],
            position + 1,
            count,
            summaries.last(),
        )
        .map_err(E::from)?;
        if input.len() > chunking.chunk_threshold_bytes {
            return Err(unchunkable());
        }
        reserve_call()?;
        summaries.push(run_summary(config, SummaryPhase::Chunk, input).map_err(E::from)?);
    }
    loop {
        let input = serialize_reduce_request(session, project, &summaries, previous_summary)
            .map_err(E::from)?;
        if input.len() <= chunking.chunk_threshold_bytes {
            reserve_call()?;
            return run_summary(config, SummaryPhase::Reduce, input).map_err(E::from);
        }
        // The reduce input itself exceeds the threshold (rare by construction): condense groups
        // of segment summaries into intermediate summaries and reduce again over those.
        if summaries.len() < 2 {
            return Err(unchunkable());
        }
        let groups =
            group_summary_ranges(session, project, &summaries, chunking.chunk_threshold_bytes)
                .map_err(E::from)?;
        if groups.len() >= summaries.len() {
            // Grouping made no progress; a further pass cannot shrink the reduce input.
            return Err(unchunkable());
        }
        let mut reduced = Vec::with_capacity(groups.len());
        for range in groups {
            let input = serialize_reduce_request(session, project, &summaries[range], None)
                .map_err(E::from)?;
            if input.len() > chunking.chunk_threshold_bytes {
                return Err(unchunkable());
            }
            reserve_call()?;
            reduced.push(run_summary(config, SummaryPhase::Reduce, input).map_err(E::from)?);
        }
        summaries = reduced;
    }
}

/// Groups segment summaries into contiguous, in-order ranges whose reduce requests each fit the
/// chunk threshold, budgeting each summary's exact serialized size against the measured size of
/// an empty reduce envelope — the same request-size accounting the built requests are checked
/// against.
fn group_summary_ranges(
    session: &NormalizedSession,
    project: &ProjectIdentity,
    summaries: &[StructuredSummary],
    threshold: usize,
) -> Result<Vec<std::ops::Range<usize>>, SummaryError> {
    let envelope = serialize_reduce_request(session, project, &[], None)?.len();
    let budget = threshold.saturating_sub(envelope);
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut used = 0usize;
    for (position, summary) in summaries.iter().enumerate() {
        let size = serde_json::to_vec(summary)
            .map_err(SummaryError::InputSerialization)?
            .len()
            + 1;
        if position > start && used + size > budget {
            ranges.push(start..position);
            start = position;
            used = 0;
        }
        used += size;
    }
    if start < summaries.len() {
        ranges.push(start..summaries.len());
    }
    Ok(ranges)
}

pub fn run_summary(
    config: &SummarizerConfig,
    phase: SummaryPhase,
    input: Vec<u8>,
) -> Result<StructuredSummary, SummaryError> {
    // Configured environment first, Munshi's own per-invocation variables after: later duplicate
    // keys override earlier ones in the spawned process's environment, so Munshi's variables win
    // on any conflict (defense in depth on top of the reserved-prefix rejection at registration).
    let mut envs: Vec<(OsString, OsString)> = config
        .env
        .iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect();
    envs.push((SUMMARIZER_PHASE_ENV.into(), OsString::from(phase.as_str())));
    let stdout = run_bounded(
        &RunnerConfig {
            binary: config.binary.clone(),
            args: config.args.clone(),
            envs,
            timeout: config.timeout,
            stdout_limit: config.stdout_limit,
            stderr_limit: config.stderr_limit,
        },
        input,
    )?;
    let summary: StructuredSummary =
        serde_json::from_slice(&stdout).map_err(SummaryError::MalformedJson)?;
    validate_structured_summary(summary)
}

pub fn validate_structured_summary(
    mut summary: StructuredSummary,
) -> Result<StructuredSummary, SummaryError> {
    summary.title = validate_text("title", summary.title, TITLE_LIMIT, true)?;
    summary.goal = validate_text("goal", summary.goal, GOAL_LIMIT, false)?;
    summary.work_completed = validate_list("work_completed", summary.work_completed, ITEM_LIMIT)?;
    if summary.work_completed.is_empty() {
        return Err(SummaryError::MissingListItems {
            field: "work_completed",
        });
    }
    summary.decisions = validate_list("decisions", summary.decisions, ITEM_LIMIT)?;
    summary.files_changed = validate_list("files_changed", summary.files_changed, ITEM_LIMIT)?;
    summary.commands_and_validation = validate_list(
        "commands_and_validation",
        summary.commands_and_validation,
        ITEM_LIMIT,
    )?;
    summary.open_items = validate_list("open_items", summary.open_items, ITEM_LIMIT)?;
    summary.tags = validate_list("tags", summary.tags, TAG_LIMIT)?;
    Ok(summary)
}

fn validate_list(
    field: &'static str,
    values: Vec<String>,
    item_limit: usize,
) -> Result<Vec<String>, SummaryError> {
    if values.len() > LIST_LIMIT {
        return Err(SummaryError::InvalidListLength {
            field,
            max: LIST_LIMIT,
        });
    }
    values
        .into_iter()
        .map(|value| validate_text(field, value, item_limit, true))
        .collect()
}

fn validate_text(
    field: &'static str,
    value: String,
    max: usize,
    single_line: bool,
) -> Result<String, SummaryError> {
    let value = value.trim().replace("\r\n", "\n").replace('\r', "\n");
    let valid_controls = value
        .chars()
        .all(|character| !character.is_control() || matches!(character, '\n' | '\t'));
    let length = value.chars().count();
    if length == 0
        || length > max
        || !valid_controls
        || (single_line && value.contains('\n'))
        || (!single_line && value.contains("\n## "))
    {
        Err(SummaryError::InvalidText { field, max })
    } else {
        Ok(value)
    }
}
