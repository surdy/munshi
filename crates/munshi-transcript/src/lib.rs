//! Streaming, lossless interpretation of coding-agent transcripts (ADR 0011, issue #26).
//!
//! What an archived transcript *means* is decided when it is read, not when it is captured
//! (`docs/adr/0011-interpret-transcripts-at-read-time-through-a-shared-streaming-crate.md`).
//! This crate holds the version-pinned format knowledge for the Copilot CLI, Claude Code,
//! and Codex rollout transcript envelopes, keyed by [`Source`] and the
//! `artifact_set_version` that capture provenance already carries, so format knowledge
//! stays in exactly one place as agents and artifact sets evolve.
//!
//! # The lossless contract
//!
//! [`TranscriptStream`] takes any [`std::io::BufRead`] and yields exactly one item per
//! non-empty transcript line — every record is accounted for exactly once:
//!
//! - a parseable record becomes a [`Record`] whose [`Classification`] is either typed
//!   [`Event`]s carrying **complete** content (nothing is elided or truncated here; size
//!   policies such as claim-ticket elision belong to consumers), a recognized-but-empty
//!   marker ([`Classification::Empty`]), a deliberately-unarchived record kind
//!   ([`Classification::Ignored`]), or a record the parser does not recognize at all
//!   ([`Classification::Unknown`], carrying the raw record so interpretation gaps stay
//!   inspectable);
//! - a malformed line — including an incomplete trailing record — becomes a per-record
//!   [`RecordError`] item, never an aborted parse; the stream continues past it.
//!
//! [`SessionSummary`] restates the legacy `NormalizedSession` counting fold on top of the
//! stream: `user_requests` / `assistant_messages` / `tool_activities` count content events
//! by kind, `ignored_events` counts [`Classification::Ignored`] plus
//! [`Classification::Unknown`] records (matching the historical lumped count), and
//! `started_at` / `updated_at` are the minimum/maximum top-level record `timestamp`.
//!
//! Typed signals grow one field at a time, pulled by a named consumer metric (issue #77).
//! A field promoted after the legacy contract froze is *additive*: the raw record it came
//! from is read again, never rewritten, and the promotion is invisible to
//! [`Event::legacy_content`]. Tool fields are named in [`ToolEvent::derived`] so they stay
//! out of the byte-identical legacy rendering (see [`ToolEvent`] for why that matters);
//! [`AssistantMeta`], which carries per-message model and token usage, hangs off
//! [`Record`] rather than off any event, because it describes the record — what one API
//! message was billed — and not the text or tool calls that record splits into.
//! [`Compaction`] hangs off [`Record`] for a stronger version of the same reason: the
//! records marking a compaction are bookkeeping this crate has always set aside, and typing
//! them as [`Event`]s would move them out of their census into a rendering they never had.
//!
//! [`envelope_matches`], [`claude_origin_cwd`], [`claude_git_branch`], and
//! [`claude_agent_version`] expose the pure, privacy-safe envelope predicates behind `munshi`'s
//! transcript validation, Claude Code origin recovery (issues #27, #40), and resume restore
//! (issue #71); the bounded-I/O wrappers around them stay in `munshi`.
//!
//! # Reading the archive record itself
//!
//! A transcript is only half of what a snapshot preserves; the other half is the `summary.md`
//! Munshi writes beside it. [`parse_archive_markdown`] reads one back into an
//! [`ArchivedMarkdown`] — session and [`ProjectIdentity`], cursor, snapshot artifact index, and
//! the [`StructuredSummary`] itself — for the same reason the transcript interpreters live here:
//! the format belongs to the crate its readers pin, so there is never a second copy of it
//! downstream (issue #79). Writing archives stays in `munshi`, the only place the capture-side
//! state a render reads from exists; [`RenderError`] carries both directions' failures, and the
//! `archive` module explains where that seam falls and why.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

mod archive;
mod classify;
mod envelope;
mod summary;

pub use archive::{
    ArchivedCursor, ArchivedMarkdown, ArtifactIndexEntry, ProjectIdentity, ProjectOrigin,
    RenderError, SourceKind, parse_archive_markdown,
};
pub use envelope::{claude_agent_version, claude_git_branch, claude_origin_cwd, envelope_matches};
pub use summary::{
    PLACEHOLDER_SUMMARY_TAG, StructuredSummary, SummaryValidationError, validate_structured_summary,
};

/// The oldest artifact-set version any source supports.
pub const MIN_SUPPORTED_ARTIFACT_SET_VERSION: u16 = 1;
/// The newest artifact-set version any source supports. Version 2 (munshi issue #23) added
/// optional `sidecar/<path>` snapshot artifacts without changing transcript interpretation, so
/// versions 1..=2 share one interpreter per source. [`TranscriptStream::new`] rejects versions
/// outside the range so future artifact sets fail loudly instead of being misinterpreted with
/// stale assumptions.
pub const SUPPORTED_ARTIFACT_SET_VERSION: u16 = 2;

/// The [`Classification::Ignored`] kind recorded for a line that is valid JSON but not a
/// JSON object. No transcript envelope produces such records, but the historical
/// normalizer tolerated and counted them as ignored, and so does this crate.
pub const NON_OBJECT_JSON_KIND: &str = "non-object-json";

/// Vendor-neutral identity of the coding-agent harness that produced a transcript.
///
/// Deliberately defined here rather than borrowed from the `munshi` crate so that this
/// crate stays standalone for external consumers; the serialized form (kebab-case) matches
/// [`SourceKind`], the capture-side identity an archive's `agent` key spells, which since
/// issue #79 also lives here and converts into this one.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// GitHub Copilot CLI (`events.jsonl`), version-pinned to 1.0.70.
    #[default]
    Copilot,
    /// Anthropic Claude Code session transcripts (`<uuid>.jsonl`), pinned to 2.1.44 and
    /// structurally re-validated at 2.1.205.
    ClaudeCode,
    /// OpenAI Codex CLI rollout files (`rollout-*.jsonl`).
    Codex,
}

/// A parser-selection error: the requested `(source, artifact_set_version)` pair has no
/// interpreter in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error(
    "artifact set version {version} of source {source_kind:?} is not supported \
     (supported: {MIN_SUPPORTED_ARTIFACT_SET_VERSION}..={SUPPORTED_ARTIFACT_SET_VERSION})"
)]
pub struct UnsupportedVersion {
    // Named `source_kind` rather than `source` so thiserror does not treat the field as
    // the error's source.
    pub source_kind: Source,
    pub version: u16,
}

/// A per-record failure. Errors are stream items, not aborts: the malformed line is
/// carried raw for inspection and iteration continues with the next line (except after an
/// I/O error, which ends the stream).
#[derive(Debug, Error)]
pub enum RecordError {
    /// The line is not valid JSON — including a truncated, incomplete trailing record.
    #[error("transcript record {record} (line {line}) is not valid JSON")]
    MalformedJson {
        /// 1-based physical line number.
        line: u64,
        /// 1-based ordinal among non-empty lines (the legacy record numbering).
        record: u64,
        /// The raw line bytes, without the trailing newline.
        raw: Vec<u8>,
    },
    /// Reading from the underlying reader failed. The stream ends after this item.
    #[error("transcript I/O failed at line {line}")]
    Io {
        /// 1-based physical line number at which the read failed.
        line: u64,
        #[source]
        source: io::Error,
    },
}

impl RecordError {
    /// The 1-based physical line number the error occurred on.
    pub fn line(&self) -> u64 {
        match self {
            Self::MalformedJson { line, .. } | Self::Io { line, .. } => *line,
        }
    }
}

/// One successfully parsed transcript record.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// 1-based physical line number in the transcript.
    pub line: u64,
    /// 1-based ordinal among non-empty lines (the legacy record numbering).
    pub record: u64,
    /// The record's top-level `timestamp` value, verbatim, when present.
    pub raw_timestamp: Option<Value>,
    /// `raw_timestamp` parsed as RFC 3339 and normalized to UTC, when it is one.
    pub timestamp: Option<DateTime<Utc>>,
    /// What the record means to the source's version-pinned envelope.
    pub classification: Classification,
    /// What the record says about the API message behind it — model, token usage, message
    /// id — when it is an assistant record whose source records any (issue #77).
    ///
    /// It hangs off the record and not off [`Event::Assistant`] because that is the unit it
    /// describes: one record is one message's worth of usage, whatever mixture of text and
    /// tool-call events its content splits into, and *whether or not it splits into any*.
    /// A Claude Code message that only calls tools yields tool events and no assistant
    /// event; a Copilot message that only calls tools records blank `content` and yields no
    /// event at all ([`Classification::Empty`]); both were billed. Carrying the figures on
    /// assistant events would therefore have left 30.5M of the mirror cache's 78.8M
    /// deduplicated output tokens — 39% — unreachable at any price.
    ///
    /// Boxed because it is ten `Option`s wide (232 bytes) against a record that is
    /// otherwise 136, most records are not assistant records, and
    /// [`TranscriptStream::collect_records`] buffers every record of a transcript at once.
    pub assistant_meta: Option<Box<AssistantMeta>>,
    /// What the record says about a context compaction, when it is one of the records a
    /// harness writes to mark one (issue #77, for qanungo's Context Management lane).
    ///
    /// Like [`Self::assistant_meta`] this is read in a pass of its own, independent of
    /// [`Self::classification`], and for a sharper reason: every record it reads is
    /// [`Classification::Ignored`] bookkeeping, and none of them may move. A compaction is a
    /// fact about the *session's* context window, not conversation content — see
    /// [`Compaction`].
    ///
    /// Boxed for the same reason as the meta beside it: eight `Option`s and a discriminant
    /// come to 128 bytes, which unboxed would take [`Record`] from 152 to 272, and 743
    /// records of the mirror cache's 689,160 carry one.
    pub compaction: Option<Box<Compaction>>,
}

/// The read-time meaning of one transcript record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// The record carries archive-worthy conversation content.
    Content { events: Vec<Event> },
    /// The record is a recognized content record whose content is empty or blank. The
    /// legacy normalizer counted these nowhere — neither as events nor as ignored.
    Empty,
    /// The format recognizes this record kind but deliberately does not archive it
    /// (bookkeeping, session metadata, model reasoning, non-object JSON). `kind` names
    /// the record type so consumers can see *what* was set aside.
    Ignored { kind: String },
    /// The parser does not recognize this record at all. The raw record is carried so a
    /// lossless reader surfaces interpretation gaps instead of hiding them.
    Unknown { raw: String },
}

/// One fully-typed, full-content conversation event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A user request; `text` is the complete user-authored content.
    User { text: String },
    /// An assistant reply; `text` is the complete assistant-authored content. What the
    /// message behind it cost belongs to the record, not to this event — see
    /// [`Record::assistant_meta`].
    Assistant { text: String },
    /// Tool activity (invocation or result), with structured fields.
    Tool(ToolEvent),
}

impl Event {
    /// The legacy normalized-event kind string (`user` / `assistant` / `tool`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::Tool(_) => "tool",
        }
    }

    /// The exact content string the legacy `munshi` normalizer builds for this event:
    /// the message text for user/assistant events, and the sorted `key=value` rendering
    /// for tool events, excluding [`ToolEvent::derived`] keys. Byte-identical to
    /// `NormalizedEvent.content` (pre-elision).
    pub fn legacy_content(&self) -> String {
        match self {
            Self::User { text } | Self::Assistant { text } => text.clone(),
            Self::Tool(tool) => tool.rendered(),
        }
    }
}

/// What an assistant record says about the API message behind it: which model produced it,
/// what it cost in tokens, and the id that identifies it (issue #77, for qanungo's cost
/// lane). Promoted read-time only, onto [`Record`] — never into any event's
/// [`Event::legacy_content`], which is what the archive is addressed by.
///
/// Only present when the source recorded at least one of the three. An absent field is an
/// under-claim the consumer can see and handle; a guessed one silently corrupts a total.
///
/// # Summing without deduplicating by `message_id` is wrong, not merely imprecise
///
/// One Claude API message reaches the transcript as *several* records — the assistant
/// text, then each of its tool calls — that share one `message.id` and repeat that
/// message's `usage` verbatim on every one of them. Every one of those records carries
/// this meta, because every one of them is that message. Adding them up counts the same
/// message two or three times over: the mirror cache holds 61,184 claude-code assistant
/// records for 29,591 message ids, and summing records rather than ids over-counts output
/// tokens 2.6-fold (68.0M against the true 26.2M).
///
/// So a cost fold is over *distinct message ids*, taking one record's usage per id, and
/// this is a correctness requirement rather than a refinement — the crate cannot do it for
/// the consumer, because deduplication needs the whole transcript and the stream hands out
/// one record at a time. Copilot records one message per record, so deduplicating its
/// `messageId` changes nothing there; the rule is stated once, for the type, because a
/// consumer folding both sources must apply it to both.
///
/// A record whose usage this crate reads but whose `message_id` it does not (no source in
/// the archive omits one, but a future envelope might) cannot be deduplicated at all, and
/// is the one case where a consumer must choose between over- and under-counting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssistantMeta {
    /// The model id as the transcript spells it — verbatim modulo edge whitespace, which
    /// is trimmed, a blank string reading as absent — never normalized and never mapped
    /// onto a pricing family, Claude Code's `<synthetic>` placeholder for locally-generated
    /// messages included, whose tokens no vendor billed. Harnesses spell the same model
    /// differently (`claude-opus-4-8` in Claude Code, `claude-opus-4.8` in Copilot);
    /// reconciling those spellings is a consumer's job, not this crate's. Every other
    /// string promoted onto this type or [`TokenUsage`] passes through on the same terms.
    pub model: Option<String>,
    /// The message's token figures, when the source records any.
    pub usage: Option<TokenUsage>,
    /// The source's own per-message id (Claude `message.id`, Copilot `data.messageId`) —
    /// the key `usage` must be deduplicated by, per this type's note.
    pub message_id: Option<String>,
}

impl AssistantMeta {
    /// `Some` only when the record said something: a meta with all three fields absent is
    /// no claim at all, and consumers distinguish "no meta" from "meta with no usage".
    pub(crate) fn recorded(self) -> Option<Self> {
        (self != Self::default()).then_some(self)
    }
}

/// The token figures of one assistant message, each present only where the source records
/// it as a non-negative integer (issue #77).
///
/// A field is `None` when the source did not record it *or* recorded something this crate
/// will not read as a count — the old Claude Code key sets predate the cache and thinking
/// figures entirely, and Copilot records no per-message input or cache figures at all.
/// `None` is never a zero: `Some(0)` is a real count a source reported, and treating
/// absence as zero turns an under-claim into a wrong total.
///
/// These figures are per *message*, not per event: see [`AssistantMeta`] for the
/// `message_id` deduplication a consumer must do before summing them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Tokens written to the prompt cache, as `usage.cache_creation_input_tokens` states
    /// the total. Can disagree with the per-tier buckets, which win when present — see
    /// [`Self::cache_5m_input_tokens`].
    pub cache_creation_input_tokens: Option<u64>,
    /// Tokens written to the 5-minute cache tier, from Claude Code's
    /// `usage.cache_creation.ephemeral_5m_input_tokens`.
    ///
    /// This and [`Self::cache_1h_input_tokens`] are the billing-tier split of
    /// `cache_creation_input_tokens`: the two TTLs bill at different multiples of the base
    /// input rate, so a cache write cannot be priced from the total alone. Promoted for
    /// that reason and no other — the rest of the `cache_creation` object, and the
    /// `server_tool_use` and `iterations` figures beside it, stay in the raw record.
    ///
    /// The buckets are read as a second, independent statement of the same quantity, never
    /// derived from the total nor it from them, because the archive shows the two can
    /// disagree: across its 61,184 usage records every one carries both, and they agree on
    /// all but a single message (repeated over four records) whose total reads 0 while its
    /// 1-hour bucket reads 2,277 — a per-record sum 9,108 tokens above the total. Prefer
    /// the buckets where they are present, since the tiers are what bill; treat a
    /// disagreement as the source's, not this crate's.
    pub cache_5m_input_tokens: Option<u64>,
    /// Tokens written to the 1-hour cache tier, from Claude Code's
    /// `usage.cache_creation.ephemeral_1h_input_tokens`. See [`Self::cache_5m_input_tokens`]
    /// for why both halves are promoted and how they relate to the total.
    pub cache_1h_input_tokens: Option<u64>,
    /// Tokens served from the prompt cache.
    pub cache_read_input_tokens: Option<u64>,
    /// Claude Code's `usage.output_tokens_details.thinking_tokens`: the share of
    /// `output_tokens` spent on extended thinking, so never added to them.
    pub thinking_tokens: Option<u64>,
    /// The service tier the message was billed at, verbatim (`standard`, ...).
    pub service_tier: Option<String>,
    /// Claude Code's `usage.speed`, verbatim: the serving mode the message was billed at.
    /// Promoted because it is a rate multiplier and not a description — fast mode bills the
    /// same model at a higher per-token rate — and this is the only place a transcript says
    /// which mode a message ran in, so dropping it silently understates a session's cost.
    pub speed: Option<String>,
    /// Claude Code's `usage.inference_geo`, verbatim: the inference region, a further
    /// billing modifier. Promoted for the same reason as `speed`; like every other field
    /// here it is passed through, never mapped onto a rate — pricing tables belong to the
    /// consumer.
    pub inference_geo: Option<String>,
}

impl TokenUsage {
    /// `Some` only when the source recorded at least one figure, so an empty or entirely
    /// unreadable `usage` object reads as no usage rather than as a set of zeroes.
    pub(crate) fn recorded(self) -> Option<Self> {
        (self != Self::default()).then_some(self)
    }
}

/// Which half of a compaction a record marks (issue #77).
///
/// Claude Code writes one record per compaction and writes it afterwards, so its
/// `compact_boundary` reads as [`Self::Complete`]: the record states how large the context
/// was before and how large it was left, which only a finished compaction knows. Copilot
/// writes both halves as separate records. Reading them onto one discriminant is what lets a
/// consumer state one counting rule for both harnesses — see [`Compaction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CompactionPhase {
    /// The harness announced that it was about to compact (Copilot
    /// `session.compaction_start`). Nothing has been dropped yet, and the compaction may
    /// still fail.
    Start,
    /// The compaction is over, successfully or not (Copilot `session.compaction_complete`,
    /// Claude Code's `compact_boundary`).
    Complete,
}

/// What a record says about a context compaction: that one happened, which half of it this
/// record marks, and whatever the source states about the size of the context around it
/// (issue #77, for qanungo's Context Management lane).
///
/// The *position* of a compaction is [`Record::record`] and [`Record::timestamp`], which
/// every record already carries: a consumer folding pre-compaction utilization from the
/// assistant usage of the messages before a boundary reads it from the stream it is already
/// walking, and this type does not restate it.
///
/// # Counting compactions means counting one phase, not counting records
///
/// Copilot writes a `session.compaction_start` and a `session.compaction_complete` for every
/// compaction — 367 of each across the mirror cache, strictly alternating in all 83 sessions
/// that compacted at all — so a fold over records counts every Copilot compaction twice and
/// every Claude Code one once. Counting [`CompactionPhase::Complete`] gives a figure that
/// means the same thing in both harnesses, which is why Claude Code's single record is read
/// as that phase and not as a third one.
///
/// Two things that rule under-claims, and does so knowingly. A compaction whose session ended
/// between the two records leaves a `Start` a `Complete`-fold does not see; the archive holds
/// no such session, but a transcript captured mid-compaction could. And a `Complete` may have
/// failed — [`Self::succeeded`] is `Some(false)` on 3 of the cache's 367 — so a fold counting
/// *effective* compactions must filter on it, while a fold counting how often the operator
/// hit the context wall should not.
///
/// # Absence is not "no compaction"
///
/// Unlike [`AssistantMeta`], this type has no "recorded" gate: a marker record with no
/// readable figure at all still reports that a compaction happened, because the record's
/// existence *is* the claim. What stays absent is every figure the source did not state, and
/// the sources state very different amounts — see the fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    /// Which half of the compaction this record marks.
    pub phase: CompactionPhase,
    /// Why the harness compacted, verbatim: Claude Code's `compactMetadata.trigger`
    /// (`manual` on all 9 of the cache's boundary records) and Copilot's `trigger` (`threshold`,
    /// on 5 of its 367 pairs). **Nearly always absent for Copilot** — the key is a recent
    /// addition to that envelope — so a consumer that splits sessions by manual against
    /// automatic compaction can do it for Claude Code and must report the Copilot split as
    /// unknown rather than defaulting it either way.
    pub trigger: Option<String>,
    /// Copilot's `success`. `None` where the source states nothing, which is every
    /// [`CompactionPhase::Start`] (the outcome is not known yet) and every Claude Code
    /// record (a `compact_boundary` is written only for a compaction that happened).
    /// `None` is therefore not a failure, and not a success either.
    pub succeeded: Option<bool>,
    /// How large the context was before the compaction, as the source states it in one
    /// figure: Claude Code's `compactMetadata.preTokens`, Copilot's `preCompactionTokens`
    /// on a `Complete` and `currentTokens` on a `Start`.
    ///
    /// Present on every Claude Code boundary and on the 364 Copilot completions that
    /// succeeded, but on only 5 Copilot starts. A start that lacks it still states the
    /// three components below, whose sum the archive shows equal to the paired completion's
    /// `preCompactionTokens` in 363 of the 364 pairs both figures exist for — and *unequal*
    /// in the remaining one (400,754 against 403,971). So a consumer may add the components
    /// up knowing what it is doing; this crate will not do the addition for it, because a
    /// derived figure that disagrees with a recorded one in one case out of 364 is a figure
    /// no source ever wrote.
    pub pre_tokens: Option<u64>,
    /// How large the context was left, from Claude Code's `compactMetadata.postTokens` and
    /// Copilot's `postCompactionTokens`. Copilot records it on only 3 of 367 completions,
    /// so *reclaim* (`pre_tokens - post_tokens`) is a Claude Code figure in practice.
    pub post_tokens: Option<u64>,
    /// The system prompt's share of the pre-compaction context, from Copilot's
    /// `systemTokens` on a [`CompactionPhase::Start`] — recorded on all 367 of them, which
    /// makes this breakdown the one pre-compaction size figure Copilot always states.
    ///
    /// This and the two fields below are read on `Start` records **only**, and that is a
    /// safety rule rather than a scope choice: Copilot spells the same three keys on a
    /// completion, where they describe the context it was *left* with, not the one it
    /// started from. The cache holds one such completion, whose `conversationTokens` reads
    /// 11,189 beside a `postCompactionTokens` of 11,193 and a `preCompactionTokens` of
    /// 403,971. Reading them by key alone would silently file a post-compaction figure as a
    /// pre-compaction one.
    pub system_tokens: Option<u64>,
    /// The conversation's share of the pre-compaction context, from Copilot's
    /// `conversationTokens` on a [`CompactionPhase::Start`]. See [`Self::system_tokens`].
    pub conversation_tokens: Option<u64>,
    /// The tool definitions' share of the pre-compaction context, from Copilot's
    /// `toolDefinitionsTokens` on a [`CompactionPhase::Start`]. See [`Self::system_tokens`];
    /// it is the share a session cannot shrink by talking less.
    pub tool_definition_tokens: Option<u64>,
    /// The context window the harness was compacting against, from Copilot's `tokenLimit`
    /// on either half. **Absent on every Claude Code boundary and on all but 5 Copilot
    /// pairs**, so utilization as a *ratio* is not a thing this archive can be folded into.
    /// A consumer wanting one supplies the denominator itself, from the model, and must
    /// never read an absent limit as an unbounded context.
    pub token_limit: Option<u64>,
}

/// Structured fields of a tool event, keyed exactly as the legacy renderer keys them
/// (`event`, `name`, `tool_use_id` / `tool_call_id` / `call_id`, `arguments` / `input`,
/// `output`, `success`, `error`, `is_error`, `command`), plus the Copilot tool-activity
/// keys added by issue #51 (`request_id` correlating `external_tool.requested` /
/// `external_tool.completed`, and the `skill.invoked` card fields `path`, `description`,
/// `source`, `trigger`, `model`, `content`). The map is ordered so the legacy rendering
/// is reproducible byte-for-byte.
///
/// # Derived fields
///
/// `derived` names the subset of `fields` promoted *after* that legacy rendering was
/// frozen — the read-time signals this crate types for analysis consumers (issue #77,
/// starting with `command` on shell-tool events). They are read exactly like every other
/// field, so `fields["command"]` means the same thing whichever harness supplied it, but
/// [`Self::rendered`] deliberately leaves them out.
///
/// Excluding them is not tidiness, it is the losslessness rule: `rendered()` is capture's
/// `NormalizedEvent.content`, which is (a) what the summarizer reads, so a new key inside
/// it would silently redraft every re-captured session's summary, and (b) what oversized
/// events are content-addressed by, so a new key would change the sha256 behind every
/// already-minted claim ticket and orphan it in the archive. A derived field is added to
/// `fields` and named in `derived`; nothing already in the legacy rendering is ever moved
/// there (Codex `local_shell_call`'s `command`, which predates the split, keeps rendering).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEvent {
    pub fields: BTreeMap<String, String>,
    /// Keys of `fields` that are read-time-only signals, excluded from [`Self::rendered`].
    pub derived: BTreeSet<String>,
}

impl ToolEvent {
    /// A tool event whose every field belongs to the legacy rendering.
    pub(crate) fn legacy(fields: BTreeMap<String, String>) -> Self {
        Self {
            fields,
            derived: BTreeSet::new(),
        }
    }

    /// Records a post-legacy typed field: readable through [`Self::fields`] like any
    /// other, absent from [`Self::rendered`]. See the type's "Derived fields" note.
    ///
    /// Never call this with a key the legacy rendering already carries: that would
    /// silently replace the legacy value *and* drop it from the rendering — exactly the
    /// drift the `derived` split exists to prevent. The assert makes the type's
    /// "nothing already in the rendering is ever moved out" claim self-enforcing.
    pub(crate) fn insert_derived(&mut self, key: &str, value: String) {
        debug_assert!(
            !self.fields.contains_key(key),
            "insert_derived would shadow legacy field {key:?}"
        );
        self.fields.insert(key.to_owned(), value);
        self.derived.insert(key.to_owned());
    }

    /// The source-specific event discriminator (`tool_use`, `tool_result`,
    /// `tool.execution_start`, `tool.user_requested`, `skill.invoked`,
    /// `external_tool.requested`, `external_tool.completed`, `function_call`,
    /// `local_shell_call`, ...). Always present.
    pub fn event(&self) -> Option<&str> {
        self.fields.get("event").map(String::as_str)
    }

    /// The tool name, when the source records one.
    pub fn name(&self) -> Option<&str> {
        self.fields.get("name").map(String::as_str)
    }

    /// The tool invocation correlation id, whichever key the source uses.
    pub fn call_id(&self) -> Option<&str> {
        ["tool_use_id", "tool_call_id", "call_id"]
            .iter()
            .find_map(|key| self.fields.get(*key).map(String::as_str))
    }

    /// The shell command line a shell-tool event carries, when its source records one and
    /// this crate can read it unambiguously (issue #77). Rendered as the harness itself
    /// represents it: a command string stays a string, an argv array stays its compact
    /// JSON array text — normalizing *across* harnesses is a consumer's job, not this
    /// crate's.
    pub fn command(&self) -> Option<&str> {
        self.fields.get("command").map(String::as_str)
    }

    /// The legacy space-joined `key=value` rendering, sorted by key — byte-identical to
    /// the `NormalizedEvent.content` string the `munshi` normalizer produces. [`Self::derived`]
    /// keys are excluded so promoting a new typed field never moves this string; see the
    /// type's "Derived fields" note.
    pub fn rendered(&self) -> String {
        self.fields
            .iter()
            .filter(|(key, _)| !self.derived.contains(*key))
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A streaming transcript reader: an [`Iterator`] of `Result<Record, RecordError>` over
/// any buffered reader, yielding one item per non-empty transcript line.
pub struct TranscriptStream<R> {
    reader: R,
    source: Source,
    buffer: Vec<u8>,
    line: u64,
    record: u64,
    done: bool,
}

impl<R: BufRead> TranscriptStream<R> {
    /// Selects the interpreter for `(source, artifact_set_version)` over `reader`.
    /// Unknown artifact-set versions are rejected up front.
    pub fn new(
        source: Source,
        artifact_set_version: u16,
        reader: R,
    ) -> Result<Self, UnsupportedVersion> {
        if !(MIN_SUPPORTED_ARTIFACT_SET_VERSION..=SUPPORTED_ARTIFACT_SET_VERSION)
            .contains(&artifact_set_version)
        {
            return Err(UnsupportedVersion {
                source_kind: source,
                version: artifact_set_version,
            });
        }
        Ok(Self {
            reader,
            source,
            buffer: Vec::new(),
            line: 0,
            record: 0,
            done: false,
        })
    }

    /// Collect-to-`Vec` convenience over the streaming iterator.
    pub fn collect_records(self) -> Vec<Result<Record, RecordError>> {
        self.collect()
    }
}

impl<R: BufRead> Iterator for TranscriptStream<R> {
    type Item = Result<Record, RecordError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            self.buffer.clear();
            match self.reader.read_until(b'\n', &mut self.buffer) {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(_) => {}
                Err(source) => {
                    self.done = true;
                    return Some(Err(RecordError::Io {
                        line: self.line + 1,
                        source,
                    }));
                }
            }
            self.line += 1;
            if self.buffer.last() == Some(&b'\n') {
                self.buffer.pop();
            }
            // Empty segments are skipped, exactly as the legacy normalizer skips them; a
            // lone carriage return is deliberately *not* stripped, matching the legacy
            // byte-level `\n` split.
            if self.buffer.is_empty() {
                continue;
            }
            self.record += 1;
            let (line, record) = (self.line, self.record);
            let value: Value = match serde_json::from_slice(&self.buffer) {
                Ok(value) => value,
                Err(_) => {
                    return Some(Err(RecordError::MalformedJson {
                        line,
                        record,
                        raw: self.buffer.clone(),
                    }));
                }
            };
            let Some(object) = value.as_object() else {
                return Some(Ok(Record {
                    line,
                    record,
                    raw_timestamp: None,
                    timestamp: None,
                    classification: Classification::Ignored {
                        kind: NON_OBJECT_JSON_KIND.to_owned(),
                    },
                    assistant_meta: None,
                    compaction: None,
                }));
            };
            let raw_timestamp = object.get("timestamp").cloned();
            let timestamp = raw_timestamp.as_ref().and_then(parse_timestamp);
            let classification = match classify::classify(self.source, object) {
                classify::Class::Content(events) => Classification::Content { events },
                classify::Class::Empty => Classification::Empty,
                classify::Class::Ignored(kind) => Classification::Ignored { kind },
                classify::Class::Unknown => Classification::Unknown {
                    // The line parsed as JSON, so it is valid UTF-8.
                    raw: String::from_utf8_lossy(&self.buffer).into_owned(),
                },
            };
            // Read independently of the classification: a record's usage is what it was
            // billed, whether its content classified as events, as empty, or as ignored.
            let assistant_meta = classify::assistant_meta(self.source, object).map(Box::new);
            // Read independently for the same reason and one more: the records that mark a
            // compaction are bookkeeping this crate deliberately does not archive, so the
            // only way to type them without moving them out of their census is beside it.
            let compaction = classify::compaction(self.source, object).map(Box::new);
            return Some(Ok(Record {
                line,
                record,
                raw_timestamp,
                timestamp,
                classification,
                assistant_meta,
                compaction,
            }));
        }
    }
}

/// The legacy `NormalizedSession` counting fold, restated over the stream so consumers
/// can derive session-level counts and the started/updated window without buffering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSummary {
    /// Number of [`Event::User`] events.
    pub user_requests: usize,
    /// Number of [`Event::Assistant`] events.
    pub assistant_messages: usize,
    /// Number of [`Event::Tool`] events.
    pub tool_activities: usize,
    /// Number of [`Classification::Ignored`] plus [`Classification::Unknown`] records —
    /// the legacy normalizer lumped both into one `ignored_events` count.
    pub ignored_events: usize,
    /// Number of [`RecordError`] items observed. Always zero for transcripts the legacy
    /// whole-parse normalizer accepted.
    pub malformed_records: usize,
    /// Minimum parsed record timestamp.
    pub first_timestamp: Option<DateTime<Utc>>,
    /// Maximum parsed record timestamp.
    pub last_timestamp: Option<DateTime<Utc>>,
}

impl SessionSummary {
    /// Folds an entire stream into a summary.
    pub fn summarize<'a, I>(items: I) -> Self
    where
        I: IntoIterator<Item = &'a Result<Record, RecordError>>,
    {
        let mut summary = Self::default();
        for item in items {
            summary.observe(item);
        }
        summary
    }

    /// Folds one stream item into the summary.
    pub fn observe(&mut self, item: &Result<Record, RecordError>) {
        let record = match item {
            Ok(record) => record,
            Err(_) => {
                self.malformed_records += 1;
                return;
            }
        };
        if let Some(timestamp) = record.timestamp {
            self.first_timestamp = Some(
                self.first_timestamp
                    .map_or(timestamp, |old| old.min(timestamp)),
            );
            self.last_timestamp = Some(
                self.last_timestamp
                    .map_or(timestamp, |old| old.max(timestamp)),
            );
        }
        match &record.classification {
            Classification::Content { events } => {
                for event in events {
                    match event {
                        Event::User { .. } => self.user_requests += 1,
                        Event::Assistant { .. } => self.assistant_messages += 1,
                        Event::Tool(_) => self.tool_activities += 1,
                    }
                }
            }
            Classification::Empty => {}
            Classification::Ignored { .. } | Classification::Unknown { .. } => {
                self.ignored_events += 1;
            }
        }
    }

    /// `first_timestamp` in the legacy `started_at` format: RFC 3339 with millisecond
    /// precision and a `Z` suffix.
    pub fn started_at(&self) -> Option<String> {
        self.first_timestamp.map(format_timestamp)
    }

    /// `last_timestamp` in the legacy `updated_at` format.
    pub fn updated_at(&self) -> Option<String> {
        self.last_timestamp.map(format_timestamp)
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
