mod archive;
mod hooks;
mod project;
mod registration;
mod render;
mod source;
mod summary;

pub use archive::{ArchiveConfig, ArchiveError, ArchiveOutcome, SessionReference, archive_session};
pub use hooks::{
    HookEvent, HookFailure, HookResult, handle_hook, read_last_failure, run_archive_worker,
    wait_for_hook_result,
};
pub use project::{ProjectIdentity, ProjectIdentityError, inspect_project, normalize_git_remote};
pub use registration::{
    DisclosureDecision, RegisterConfig, RegistrationError, accept_disclosure,
    accept_disclosure_from_terminal, register, unregister,
};
pub use render::{ArchiveMetadata, atomic_replace, render_markdown};
pub use source::{
    NormalizedEvent, NormalizedSession, SourceError, load_session, resolve_session_reference,
};
pub use summary::{StructuredSummary, SummaryError, build_summary_input, run_summary};
