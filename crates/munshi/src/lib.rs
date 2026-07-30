mod archive;
mod archive_git;
mod claude_settings;
mod delivery;
mod hooks;
mod http;
mod patwari;
mod policy;
mod project;
mod registration;
mod render;
mod retrieve;
mod source;
mod state;
mod summary;
mod verify_archive;

pub use archive::{ArchiveConfig, ArchiveError, ArchiveOutcome, SessionReference, archive_session};
pub use claude_settings::{ClaudeHookStatus, claude_hooks_status};
pub use delivery::{
    DeliveryCredentialSource, DeliveryError, DeliveryItem, DeliveryOutcome, DeliveryRunItem,
    DeliveryRunReport, DeliverySettings, DeliverySinkConfig, DeliveryStatusReport,
    HistoryCapability, HistoryReport, HttpNotesmithSink, NotesmithSink,
    backfill as delivery_backfill, configure_sink as configure_delivery,
    load_settings as delivery_settings, retry as delivery_retry,
    set_enabled as set_delivery_enabled, status as delivery_status,
    verify_history as delivery_verify_history,
};
pub use hooks::{
    HookEvent, HookFailure, HookResult, HookWorkerError, handle_hook,
    lift_stale_source_limit_parks, read_last_failure, run_archive_worker,
    run_archive_worker_for_source, run_recovery, wait_for_hook_result,
    wait_for_hook_result_for_source,
};
pub use patwari::{
    ArchiveUploadItem, ArchiveUploadRunItem, ArchiveUploadRunReport, ArchiveUploadSettings,
    ArchiveUploadStatusReport, ArtifactSource, CaptureContext, INITIAL_ARTIFACT_SET_VERSION,
    PatwariClient, PatwariError, PreparedArtifact, SessionContext, UploadOutcome, UploadReceipt,
    assemble_artifact_sources, backfill as archive_upload_backfill, build_manifest,
    configure as configure_archive_upload, prepare_artifact, prepare_artifacts,
    retry as archive_upload_retry, set_enabled as set_archive_upload_enabled,
    status as archive_upload_status,
};
pub use policy::{DisabledReason, GlobalPolicy, PolicyError, ResolvedPolicy, resolve_policy};
pub use project::{
    ProjectIdentity, ProjectIdentityError, ProjectOrigin, inspect_project, normalize_git_remote,
    recorded_project_identity,
};
pub use registration::{
    ClaudeTarget, CopilotTarget, DEFAULT_CHUNK_THRESHOLD_BYTES, DisclosureDecision, ProjectStatus,
    RegisterConfig, RegistrationError, accept_disclosure, accept_disclosure_from_terminal,
    configured_chunk_threshold_bytes, configured_max_event_text_bytes, project_status, register,
    set_project_enabled, unregister,
};
pub use render::{
    ArchiveMetadata, ArchiveVersion, ArchivedCursor, ArchivedMarkdown, atomic_replace,
    content_hash, parse_archive_markdown, render_markdown, render_revision_markdown,
};
pub use retrieve::{
    ArtifactMatch, MatchLine, QUERY_CONTEXT_LINES, RetrieveError, RetrieveResult, RetrievedContent,
    SearchResults, retrieve, search_content, write_output as write_retrieved_output,
};
pub use source::{
    ArtifactIndexEntry, ClaudeRecordedOrigin, CursorFallbackReason, DEFAULT_MAX_EVENT_TEXT_BYTES,
    ExtractedOutput, NormalizedEvent, NormalizedSession, PreviousSource, SnapshotArtifactIndex,
    SourceError, SourceKind, TranscriptLoadMode, TranscriptUpdate, claude_transcript_origin,
    claude_transcript_recorded_origin, copilot_workspace_origin, extract_outputs, load_session,
    load_session_update, resolve_session_reference, snapshot_artifact_index,
    validate_transcript_envelope,
};
pub use state::{
    ArchiveUploadRecord, ArchiveUploadSuccess, BudgetOutcome, CapturePrep, ClaimOutcome,
    CompletionReason, DeliveryRecord, DeliverySuccess, Diagnostic, SessionRecord, StateError,
    StateStore, WaitState, try_acquire_session_lock,
};
pub use summary::{
    ChunkingLimits, PLACEHOLDER_SUMMARY_TAG, PlaceholderReason, RESERVED_SUMMARIZER_ENV_PREFIX,
    SUMMARIZER_PHASE_ENV, SUMMARY_CONTRACT_VERSION, StructuredSummary, SummaryError, SummaryPhase,
    SummaryStrategy, build_revision_summary_input, build_summary_input, chunk_event_ranges,
    parse_summarizer_env, placeholder_summary, plan_summary_input, run_chunked_summary,
    run_summary, validate_input_cap_relation, validate_structured_summary,
};
pub use verify_archive::{VerifyArchiveError, VerifyArchiveReport, verify_archive_parse};
