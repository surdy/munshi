mod archive;
mod project;
mod render;
mod source;
mod summary;

pub use archive::{ArchiveConfig, ArchiveError, ArchiveOutcome, SessionReference, archive_session};
pub use project::{ProjectIdentity, ProjectIdentityError, inspect_project, normalize_git_remote};
pub use render::{ArchiveMetadata, atomic_replace, render_markdown};
pub use source::{
    NormalizedEvent, NormalizedSession, SourceError, load_session, resolve_session_reference,
};
pub use summary::{StructuredSummary, SummaryError, build_summary_input, run_summary};
