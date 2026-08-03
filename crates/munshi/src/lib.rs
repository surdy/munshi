mod archive;
mod archive_git;
mod claude_settings;
mod delivery;
mod exhaust;
mod hooks;
mod http;
mod memory_sync;
mod patwari;
mod patwari_read;
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
pub use exhaust::{
    EXHAUST_PRUNE_LIMIT, EXHAUST_QUIET_PERIOD, EXHAUST_SIZE_WARN_BYTES, ExhaustError,
    ExhaustPolicy, ExhaustReport, ExhaustStatus, SESSION_STORE_FILES, conflicting_source_home,
    default_copilot_home, prune_summarizer_exhaust, summarizer_exhaust_bytes,
};
pub use hooks::{
    HookEvent, HookFailure, HookResult, HookWorkerError, SUMMARIZER_EXHAUST_DIAGNOSTIC,
    WorkerContext, handle_hook, lift_stale_source_limit_parks, reactivate_regrown_lost_transcripts,
    read_last_failure, run_archive_worker, run_archive_worker_for_source, run_recovery,
    tick_recovery_sweep, wait_for_hook_result, wait_for_hook_result_for_source,
};
pub use memory_sync::{
    MemorySinkConfig, MemorySyncError, MemorySyncItem, MemorySyncRunItem, MemorySyncRunReport,
    MemorySyncSettings, MemorySyncStatusReport, configure_sink as configure_memory_sync,
    run as memory_sync_run, set_enabled as set_memory_sync_enabled, status as memory_sync_status,
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
    project_label, recorded_project_identity,
};
pub use registration::{
    ClaudeTarget, CopilotTarget, DEFAULT_CHUNK_THRESHOLD_BYTES,
    DEFAULT_SUMMARIZER_EXHAUST_RETENTION_DAYS, DisclosureDecision, ProjectStatus, RegisterConfig,
    RegistrationError, accept_disclosure, accept_disclosure_from_terminal,
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
    SourceError, SourceHomes, SourceKind, TranscriptLoadMode, TranscriptUpdate,
    claude_transcript_origin, claude_transcript_recorded_origin, copilot_workspace_origin,
    derive_transcript_path, extract_outputs, load_session, load_session_update,
    resolve_session_reference, snapshot_artifact_index, validate_transcript_envelope,
};
pub use state::{
    ArchiveUploadRecord, ArchiveUploadSuccess, AttemptRecord, BudgetOutcome, CapturePrep,
    ClaimOutcome, CompletionReason, DeliveryRecord, DeliverySuccess, Diagnostic, MemorySyncRecord,
    MemorySyncSuccess, SessionRecord, StateError, StateStore, WaitState, try_acquire_session_lock,
};
pub use summary::{
    ChunkingLimits, PLACEHOLDER_SUMMARY_TAG, PlaceholderReason, RESERVED_SUMMARIZER_ENV_PREFIX,
    SUMMARIZER_PHASE_ENV, SUMMARY_CONTRACT_VERSION, StructuredSummary, SummaryError, SummaryPhase,
    SummaryStrategy, build_revision_summary_input, build_summary_input, chunk_event_ranges,
    is_summary_request_envelope, parse_summarizer_env, placeholder_summary, plan_summary_input,
    run_chunked_summary, run_summary, validate_input_cap_relation, validate_structured_summary,
};
pub use verify_archive::{VerifyArchiveError, VerifyArchiveReport, verify_archive_parse};
