mod archive;
mod archive_git;
mod hooks;
mod policy;
mod project;
mod registration;
mod render;
mod source;
mod state;
mod summary;

pub use archive::{ArchiveConfig, ArchiveError, ArchiveOutcome, SessionReference, archive_session};
pub use hooks::{
    HookEvent, HookFailure, HookResult, HookWorkerError, handle_hook, read_last_failure,
    run_archive_worker, run_recovery, wait_for_hook_result,
};
pub use policy::{DisabledReason, GlobalPolicy, PolicyError, ResolvedPolicy, resolve_policy};
pub use project::{ProjectIdentity, ProjectIdentityError, inspect_project, normalize_git_remote};
pub use registration::{
    DisclosureDecision, ProjectStatus, RegisterConfig, RegistrationError, accept_disclosure,
    accept_disclosure_from_terminal, project_status, register, set_project_enabled, unregister,
};
pub use render::{
    ArchiveMetadata, ArchiveVersion, ArchivedCursor, ArchivedMarkdown, atomic_replace,
    content_hash, parse_archive_markdown, render_markdown, render_revision_markdown,
};
pub use source::{
    CursorFallbackReason, NormalizedEvent, NormalizedSession, PreviousSource, SourceError,
    TranscriptLoadMode, TranscriptUpdate, load_session, load_session_update,
    resolve_session_reference, validate_transcript_envelope,
};
pub use state::{
    BudgetOutcome, ClaimOutcome, CompletionReason, Diagnostic, SessionRecord, StateError,
    StateStore, WaitState, try_acquire_session_lock,
};
pub use summary::{
    StructuredSummary, SummaryError, build_revision_summary_input, build_summary_input,
    run_summary, validate_structured_summary,
};
