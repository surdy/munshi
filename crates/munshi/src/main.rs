use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use munshi::{
    ArchiveConfig, ArchiveOutcome, ArchiveUploadRunReport, ArchiveUploadSettings,
    ArchiveUploadStatusReport, ArtifactMatch, AttemptRecord, DeliveryCredentialSource,
    DeliveryRunReport, DeliverySinkConfig, DeliveryStatusReport, Diagnostic, HistoryReport,
    HookEvent, HookFailure, HookResult, ProjectStatus, RegisterConfig, RetrieveError,
    RetrieveResult, SearchResults, SessionRecord, SessionReference, SourceKind, StateStore,
    StructuredSummary, VerifyArchiveError, VerifyArchiveReport, accept_disclosure_from_terminal,
    archive_session, archive_upload_backfill, archive_upload_retry, archive_upload_status,
    configure_archive_upload, configure_delivery, delivery_backfill, delivery_retry,
    delivery_status, delivery_verify_history, handle_hook, lift_stale_source_limit_parks,
    parse_archive_markdown, parse_summarizer_env, project_label, project_status, read_last_failure,
    register, retrieve, run_archive_worker_for_source, run_recovery, set_archive_upload_enabled,
    set_delivery_enabled, set_project_enabled, unregister, verify_archive_parse,
    wait_for_hook_result_for_source,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(about = "Archive coding-agent sessions as durable Markdown", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manually summarize and archive one coding-agent session.
    #[command(visible_alias = "summarize")]
    Archive {
        /// The source harness's stable session ID.
        session_id: Option<String>,
        /// Capturing harness. Source selection is independent of the summarizer.
        #[arg(long, default_value = "copilot")]
        source: String,
        /// Explicit transcript path. Copilot expects an `events.jsonl` file; Claude Code and
        /// Codex expect the harness's `<session>.jsonl` transcript or rollout file.
        #[arg(long)]
        events: Option<PathBuf>,
        /// Copilot home used for the version-pinned session-state fallback.
        #[arg(long)]
        copilot_home: Option<PathBuf>,
        /// Origin project directory for identity and routing.
        #[arg(long)]
        project_dir: PathBuf,
        /// Root directory for Munshi-owned Markdown archives.
        #[arg(long)]
        output_dir: PathBuf,
        /// Explicit Copilot-compatible summary executable.
        #[arg(long)]
        summarizer: PathBuf,
        /// Argument forwarded to the summary executable. Transcript content is never forwarded.
        #[arg(long = "summarizer-arg", allow_hyphen_values = true)]
        summarizer_args: Vec<OsString>,
        /// Environment variable set on the summarizer invocation (KEY=VALUE). Repeatable. Opaque
        /// to Munshi; `MUNSHI_SUMMARIZER_*` keys are reserved.
        #[arg(long = "summarizer-env", value_parser = parse_summarizer_env)]
        summarizer_env: Vec<(String, String)>,
        #[arg(long, default_value_t = 300_000)]
        timeout_ms: u64,
        /// Largest raw transcript read, in bytes. Default sized for real agentic sessions
        /// (issue #41): 64 MiB covers the 10–60 MiB transcripts long sessions produce.
        #[arg(long, default_value_t = 67_108_864)]
        max_source_bytes: usize,
        /// Hard cap on normalized summarizer input, in bytes. Default 8 MiB (issue #41) keeps
        /// the ~8x raw:normalized ratio; chunking engages well below it.
        #[arg(long, default_value_t = 8_388_608)]
        max_input_bytes: usize,
        #[arg(long, default_value_t = 262_144)]
        max_stdout_bytes: usize,
        #[arg(long, default_value_t = 65_536)]
        max_stderr_bytes: usize,
        /// Munshi state directory whose registration supplies the extraction threshold. Defaults to
        /// `$MUNSHI_HOME`, then `~/.munshi`; when unregistered the built-in default threshold is used.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Disclose transcript processing, save configuration, and install user hooks.
    Register {
        /// Explicitly accept the displayed v1 transcript-processing disclosure.
        #[arg(long, visible_alias = "accept-disclosure")]
        accept_transcript_processing: bool,
        /// Print the intended managed paths without writing files.
        #[arg(long)]
        dry_run: bool,
        /// Harness to install lifecycle hooks for. Repeatable. Defaults to every harness whose
        /// home directory exists.
        #[arg(long = "harness", value_enum)]
        harnesses: Vec<HarnessSelector>,
        /// Copilot home whose hooks directory should contain Munshi's dedicated file.
        #[arg(long)]
        copilot_home: Option<PathBuf>,
        /// Claude Code home whose `settings.json` receives Munshi's hook entries.
        #[arg(long)]
        claude_home: Option<PathBuf>,
        /// Munshi state directory. Defaults to `$MUNSHI_HOME`, then `~/.munshi`.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Root directory for Munshi-owned Markdown archives.
        #[arg(long)]
        output_dir: PathBuf,
        /// Enable one Git commit per successful non-cursor summary revision.
        #[arg(long)]
        archive_git_history: bool,
        /// Explicit compatible summary executable.
        #[arg(long)]
        summarizer: PathBuf,
        /// Argument forwarded to the summarizer; transcript content is sent only on stdin.
        #[arg(long = "summarizer-arg", allow_hyphen_values = true)]
        summarizer_args: Vec<OsString>,
        /// Environment variable set on every summarizer invocation (KEY=VALUE). Repeatable.
        /// Opaque to Munshi — the summarizer wrapper contract gives keys meaning (for example
        /// MUNSHI_CHUNK_MODEL / MUNSHI_REDUCE_MODEL for the contrib wrappers). Reserved
        /// `MUNSHI_SUMMARIZER_*` keys are rejected; Munshi's own variables win on conflict.
        #[arg(long = "summarizer-env", value_parser = parse_summarizer_env)]
        summarizer_env: Vec<(String, String)>,
        #[arg(long, default_value_t = 300_000)]
        timeout_ms: u64,
        /// Largest raw transcript read, in bytes. Default sized for real agentic sessions
        /// (issue #41): 64 MiB covers the 10–60 MiB transcripts long sessions produce.
        #[arg(long, default_value_t = 67_108_864)]
        max_source_bytes: usize,
        /// Hard cap on normalized summarizer input, in bytes. Default 8 MiB (issue #41) keeps
        /// the ~8x raw:normalized ratio; chunking engages well below it.
        #[arg(long, default_value_t = 8_388_608)]
        max_input_bytes: usize,
        #[arg(long, default_value_t = 262_144)]
        max_stdout_bytes: usize,
        #[arg(long, default_value_t = 65_536)]
        max_stderr_bytes: usize,
        /// Measured one-shot request size above which a session is summarized in chunks plus a
        /// reduce pass (issue #48), and the cap on any single chunk/reduce request.
        #[arg(long, default_value_t = 2_621_440)]
        chunk_threshold_bytes: usize,
        /// Approximate serialized-events payload each chunk request targets on the chunked path.
        #[arg(long, default_value_t = 1_572_864)]
        chunk_size_bytes: usize,
        /// Maximum summarizer invocations allowed per project per rolling hour.
        #[arg(long, default_value_t = 10)]
        max_calls_per_hour: u32,
        /// Maximum summarizer invocations allowed per project per rolling day.
        #[arg(long, default_value_t = 50)]
        max_calls_per_day: u32,
        /// Maximum number of sessions summarized concurrently across all projects.
        #[arg(long, default_value_t = 2)]
        max_concurrency: usize,
    },
    /// Remove only Munshi's dedicated user hook and active configuration.
    Unregister {
        /// Copilot home checked for an orphaned hook file when no configuration records one.
        #[arg(long)]
        copilot_home: Option<PathBuf>,
        /// Munshi state directory. Defaults to `$MUNSHI_HOME`, then `~/.munshi`.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Enable, disable, or inspect future processing and delivery for one project.
    #[command(subcommand)]
    Project(ProjectCommand),
    /// Configure and operate opt-in Notesmith delivery of current summaries.
    #[command(subcommand, visible_alias = "delivery")]
    SummaryDelivery(SummaryDeliveryCommand),
    /// Configure and operate opt-in Patwari archive upload of full session snapshots.
    #[command(subcommand)]
    ArchiveUpload(ArchiveUploadCommand),
    /// Show overall operational status.
    Status {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// List sessions and their operational states.
    Sessions {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum)]
        state: Option<SessionStateFilter>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// List recent processing attempts and how they ended.
    Attempts {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Keep only attempts active at or after this Unix millisecond: finished then, or
        /// started then and still running.
        #[arg(long)]
        since_ms: Option<i64>,
    },
    /// List the most recently recorded diagnostics.
    Diagnostics {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show one session and its current summary.
    Show {
        session_id: String,
        /// Disambiguate when the same session ID exists under multiple sources.
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// Redeem a claim ticket: retrieve original content from Patwari by its sha256.
    Retrieve {
        /// The original content sha256 (64-char lowercase hex, optional `sha256:` prefix).
        sha256: String,
        /// Search the retrieved content for a case-insensitive substring instead of emitting it.
        #[arg(long, conflicts_with_all = ["output", "force", "list"])]
        query: Option<String>,
        /// Write the original bytes to this file instead of stdout.
        #[arg(long, conflicts_with = "list")]
        output: Option<PathBuf>,
        /// Overwrite an existing `--output` file.
        #[arg(long, requires = "output")]
        force: bool,
        /// List every matching artifact across snapshots without downloading anything.
        #[arg(long)]
        list: bool,
        /// Retrieve from this archive server instead of the configured archive-upload endpoint.
        #[arg(long)]
        endpoint: Option<String>,
        /// Raise the maximum stored bytes downloaded for one artifact (default 128 MiB). Needed to
        /// deliberately retrieve an artifact larger than the default cap.
        #[arg(long)]
        max_download_bytes: Option<usize>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract (for `--list` and `--query`).
        #[arg(long)]
        json: bool,
    },
    /// Walk the Patwari archive, download and verify each snapshot's transcript, and stream-parse
    /// it with the shared read-time interpreter, reporting per-session parse accounting
    /// (the ADR 0011/0012 acceptance check; rerun manually after format bumps).
    VerifyArchiveParse {
        /// Verify only the snapshots belonging to this Patwari session ID.
        #[arg(long, required_unless_present = "all", conflicts_with = "all")]
        session: Option<String>,
        /// Verify every snapshot in the archive.
        #[arg(long)]
        all: bool,
        /// Verify against this archive server instead of the configured archive-upload endpoint.
        #[arg(long)]
        endpoint: Option<String>,
        /// Raise the maximum stored bytes downloaded for one artifact (default 128 MiB). A larger
        /// transcript is otherwise skipped with an accounting line instead of downloaded.
        #[arg(long)]
        max_download_bytes: Option<usize>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// Retry one pending/failed session using the normal worker state machine.
    Retry {
        session_id: String,
        /// Disambiguate when the same session ID exists under multiple sources.
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Force failed sessions past backoff/permanent retry markers.
        #[arg(long)]
        force: bool,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// Retry all currently eligible sessions using the normal worker state machine.
    RetryAll {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Force failed sessions past backoff/permanent retry markers.
        #[arg(long)]
        force: bool,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 32)]
        limit: usize,
    },
    /// Diagnose registration, dependencies, and runtime readiness.
    Doctor {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// Validate current registration/configuration contracts.
    ConfigurationCheck {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    #[command(hide = true, subcommand)]
    Hook(HookCommand),
    #[command(hide = true)]
    HookWorker {
        #[arg(long)]
        state_dir: PathBuf,
        /// Capturing harness whose state machine and source adapter drive this worker.
        #[arg(long, default_value = "copilot")]
        source: String,
        #[arg(long)]
        session_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum HarnessSelector {
    Copilot,
    ClaudeCode,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum SessionStateFilter {
    Archived,
    RevisionPending,
    SummaryPending,
    Interrupted,
    Failed,
    DeliveryRelated,
    DisabledProject,
    Processing,
    Observed,
    NotArchiveWorthy,
    Unknown,
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Stop future processing and delivery for a project. Existing archives are left untouched.
    Disable {
        /// Project directory whose canonical identity should be disabled.
        project_dir: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Resume future processing and delivery for a previously disabled project.
    Enable {
        /// Project directory whose canonical identity should be re-enabled.
        project_dir: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Print the effective enabled state and budgets for a project.
    Status {
        /// Project directory to inspect.
        project_dir: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum SummaryDeliveryCommand {
    /// Record the Notesmith sink (endpoint, vault, folder, credential source) without enabling it.
    Configure {
        /// Base URL of the Notesmith daemon, for example `http://127.0.0.1:27183`.
        #[arg(long)]
        endpoint: String,
        /// Target Notesmith vault name.
        #[arg(long)]
        vault: String,
        /// Optional vault-relative folder that Munshi-owned session notes are filed under.
        #[arg(long)]
        folder: Option<String>,
        /// Name of the environment variable holding the bearer credential.
        #[arg(long, conflicts_with = "credential_keychain")]
        credential_env: Option<String>,
        /// OS credential-store entry as `service:account` holding the bearer credential.
        #[arg(long, conflicts_with = "credential_env")]
        credential_keychain: Option<String>,
        /// Bounded number of delivery attempts before a session is parked as a dead letter.
        #[arg(long)]
        max_attempts: Option<u32>,
        /// For versioned delivery (issue #9), explicitly configure the Notesmith vault's revision
        /// history capability when absent instead of only verifying it. Use `--no-provision-history`
        /// to return to verify-only.
        #[arg(long, overrides_with = "no_provision_history")]
        provision_history: bool,
        #[arg(long, overrides_with = "provision_history", hide = true)]
        no_provision_history: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Enable delivery. Reports the pending backfill count; existing summaries need confirmation.
    Enable {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Disable delivery. Future delivery stops while delivery history is retained.
    Disable {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Verify (or, with `--configure`, explicitly enable) the Notesmith vault's revision-history
    /// capability required for versioned delivery.
    History {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Explicitly enable the remote history capability if it is absent.
        #[arg(long)]
        configure: bool,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// Show delivery configuration and per-session delivery state.
    Status {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// Publish existing current archives. A dry run by default; `--confirm` publishes.
    Backfill {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Publish the reported summaries instead of performing a dry run.
        #[arg(long)]
        confirm: bool,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// Retry failed deliveries, or one session's delivery.
    Retry {
        /// A single session ID to retry; omit with `--all` to retry every failed delivery.
        session_id: Option<String>,
        /// Disambiguate when the same session ID exists under multiple sources.
        #[arg(long)]
        source: Option<String>,
        /// Retry every failed delivery.
        #[arg(long)]
        all: bool,
        /// Revive dead-letter deliveries and reset their bounded attempt count.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ArchiveUploadCommand {
    /// Record the Patwari archive server endpoint without enabling upload.
    Configure {
        /// Base URL of the Patwari archive server, for example `http://127.0.0.1:8080`.
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Enable archive upload. Requires a configured, addressable server.
    Enable {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Disable archive upload. Future upload stops while upload history is retained.
    Disable {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Show archive-upload configuration and per-session upload state.
    Status {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// Upload archived sessions with no recorded upload for the configured server.
    Backfill {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
    /// Retry failed uploads, or one session's upload.
    Retry {
        /// A single session ID to retry; omit with `--all` to retry every failed upload.
        session_id: Option<String>,
        /// Disambiguate when the same session ID exists under multiple sources.
        #[arg(long)]
        source: Option<String>,
        /// Retry every failed upload.
        #[arg(long)]
        all: bool,
        /// Revive dead-letter uploads and reset their bounded attempt count.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Emit a stable machine-readable contract.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    AgentStop {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Capturing harness whose payload shape and state scope this hook uses.
        #[arg(long, default_value = "copilot")]
        source: String,
    },
    SessionEnd {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Capturing harness whose payload shape and state scope this hook uses.
        #[arg(long, default_value = "copilot")]
        source: String,
    },
    Wait {
        #[arg(long)]
        state_dir: PathBuf,
        /// Capturing harness whose state scope holds the awaited session.
        #[arg(long, default_value = "copilot")]
        source: String,
        #[arg(long)]
        session_id: String,
        #[arg(long, default_value_t = 10_000)]
        timeout_ms: u64,
    },
    Recover {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long, default_value_t = 1_800_000)]
        stale_after_ms: u64,
        #[arg(long)]
        force_retry: bool,
        #[arg(long)]
        rebuild_state: bool,
    },
}

enum Outcome {
    Archive(ArchiveOutcome),
    Registered {
        hook_paths: Vec<PathBuf>,
    },
    Unregistered,
    DryRun,
    Hook,
    Worker,
    Wait(HookResult),
    Project(ProjectStatus),
    DeliveryConfigured {
        settings: Box<munshi::DeliverySettings>,
    },
    DeliveryEnabled {
        settings: Box<munshi::DeliverySettings>,
        backfill: Option<Box<DeliveryRunReport>>,
    },
    DeliveryDisabled {
        settings: Box<munshi::DeliverySettings>,
    },
    DeliveryStatus {
        report: Box<DeliveryStatusReport>,
        json: bool,
    },
    DeliveryRun {
        report: Box<DeliveryRunReport>,
        json: bool,
    },
    DeliveryHistory {
        report: Box<HistoryReport>,
        json: bool,
    },
    ArchiveUploadConfigured {
        settings: Box<ArchiveUploadSettings>,
    },
    ArchiveUploadEnabled {
        settings: Box<ArchiveUploadSettings>,
    },
    ArchiveUploadDisabled {
        settings: Box<ArchiveUploadSettings>,
    },
    ArchiveUploadStatus {
        report: Box<ArchiveUploadStatusReport>,
        json: bool,
    },
    ArchiveUploadRun {
        report: Box<ArchiveUploadRunReport>,
        json: bool,
    },
    Status {
        report: Box<StatusReport>,
        json: bool,
    },
    Sessions {
        report: Box<SessionsReport>,
        json: bool,
    },
    Attempts {
        report: Box<AttemptsReport>,
        json: bool,
    },
    Diagnostics {
        report: Box<DiagnosticsReport>,
        json: bool,
    },
    Show {
        report: Box<ShowReport>,
        json: bool,
    },
    Retrieve {
        result: Box<Result<RetrieveResult, RetrieveError>>,
        query: Option<String>,
        output: Option<PathBuf>,
        force: bool,
        json: bool,
    },
    VerifyArchiveParse {
        result: Box<Result<VerifyArchiveReport, VerifyArchiveError>>,
        json: bool,
    },
    Retry {
        report: Box<RetryReport>,
        json: bool,
    },
    RetryAll {
        report: Box<RetryAllReport>,
        json: bool,
    },
    Doctor {
        report: Box<DoctorReport>,
        json: bool,
    },
    ConfigurationCheck {
        report: Box<ConfigurationCheckReport>,
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum CheckStatus {
    Ok,
    Warning,
    Error,
}

impl CheckStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Ok => "[ok]",
            Self::Warning => "[warn]",
            Self::Error => "[error]",
        }
    }

    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Error, _) | (_, Self::Error) => Self::Error,
            (Self::Warning, _) | (_, Self::Warning) => Self::Warning,
            _ => Self::Ok,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum CaptureState {
    Enabled,
    DisabledProject,
    Unknown,
}

impl CaptureState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::DisabledProject => "disabled-project",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum DeliveryState {
    Disabled,
    Enabled,
    DeliveryRelated,
    Unknown,
}

impl DeliveryState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::DeliveryRelated => "delivery-related",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct CheckResult {
    code: &'static str,
    status: CheckStatus,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct ConfigurationAssessment {
    status: CheckStatus,
    runtime_compatible: bool,
    capture_state: CaptureState,
    delivery_state: DeliveryState,
    archive_git_history: Option<bool>,
    /// Whether versioned delivery is required (local Git history + delivery enabled) — issue #9.
    versioned_delivery: Option<bool>,
    /// Whether Munshi will explicitly configure the remote history capability (versus verify-only).
    provision_remote_history: Option<bool>,
    disabled_projects: usize,
    config_path: String,
    hook_path: String,
    /// Claude Code settings file carrying Munshi's managed hook entries, when that harness is
    /// registered.
    claude_settings_path: Option<String>,
    summarizer_executable: Option<String>,
    output_directory: Option<String>,
    checks: Vec<CheckResult>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct SessionStateSummary {
    total: usize,
    archived: usize,
    revision_pending: usize,
    summary_pending: usize,
    interrupted: usize,
    failed: usize,
    /// The subset of `failed` parked permanently (`next_retry_at_ms < 0`): repeat deterministic
    /// failures (issue #38) and non-retryable verdicts. Sweeps skip these until an explicit
    /// `retry`/`--force` (or, for `source-failed`, a raised source limit) lifts the park.
    parked: usize,
    /// Sessions whose current summary is a machine-generated placeholder (issue #43): the
    /// transcript is archived and uploaded, but a real summary is still owed and a targeted
    /// `retry` re-attempts it. Counted by the placeholder tag on the stored summary, so the count
    /// survives lifecycle transitions and state rebuilds.
    placeholder: usize,
    delivery_related: usize,
    disabled_project: usize,
    processing: usize,
    observed: usize,
    not_archive_worthy: usize,
    unknown: usize,
}

#[derive(Debug, Clone, Serialize)]
struct StatusReport {
    schema_version: u32,
    command: &'static str,
    state_directory: String,
    configuration: ConfigurationAssessment,
    sessions: SessionStateSummary,
    last_failure: Option<HookFailure>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionsReport {
    schema_version: u32,
    command: &'static str,
    filter: Option<String>,
    total: usize,
    returned: usize,
    items: Vec<SessionListItem>,
}

#[derive(Debug, Clone, Serialize)]
struct SessionListItem {
    source: String,
    session_id: String,
    state: String,
    lifecycle_state: String,
    revision: u64,
    completion_reason: Option<String>,
    summary_title: Option<String>,
    archive_path: Option<String>,
    last_error_code: Option<String>,
    /// The session's display project label (issue #56). Additive on the `schema_version: 1`
    /// contract, like the harness-adapter status fields: a reader pinned to the older shape sees
    /// the same fields it always did. `null` when the session recorded no origin evidence.
    project: Option<String>,
    /// When Munshi first observed the session, and when its row was last written. Both are Unix
    /// milliseconds. `created_at_ms` is Munshi's first sighting, not the session's first turn.
    created_at_ms: i64,
    updated_at_ms: i64,
}

/// The `attempts` contract (issue #56): the processing-attempt log a dashboard bins into outcome
/// and failure views, exposed as rows so every rollup stays on the caller's side.
#[derive(Debug, Clone, Serialize)]
struct AttemptsReport {
    schema_version: u32,
    command: &'static str,
    since_ms: Option<i64>,
    total: usize,
    returned: usize,
    items: Vec<AttemptListItem>,
}

#[derive(Debug, Clone, Serialize)]
struct AttemptListItem {
    source: String,
    session_id: String,
    project: Option<String>,
    outcome: String,
    error_category: Option<String>,
    started_at_ms: i64,
    /// `null` while the attempt still holds its lease.
    finished_at_ms: Option<i64>,
}

/// The `diagnostics` contract (issue #56): the same operator-facing records `status` already
/// surfaces one of as `last_failure`, as a bounded newest-first tail.
#[derive(Debug, Clone, Serialize)]
struct DiagnosticsReport {
    schema_version: u32,
    command: &'static str,
    total: usize,
    returned: usize,
    items: Vec<DiagnosticListItem>,
}

#[derive(Debug, Clone, Serialize)]
struct DiagnosticListItem {
    /// `null` together with `session_id` when the diagnostic named no session.
    source: Option<String>,
    session_id: Option<String>,
    operation: String,
    category: String,
    cause_category: Option<String>,
    recorded_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
struct ShowReport {
    schema_version: u32,
    command: &'static str,
    found: bool,
    session: Option<ShowSessionView>,
}

#[derive(Debug, Clone, Serialize)]
struct ShowSessionView {
    source_kind: String,
    session_id: String,
    state: String,
    lifecycle_state: String,
    revision: u64,
    completion_reason: Option<String>,
    summary_title: Option<String>,
    archive_path: Option<String>,
    last_error_code: Option<String>,
    /// Consecutive same-category failures on the current session content (issue #38).
    failure_streak: i64,
    /// `None`: immediately retry-eligible; a timestamp: scheduled backoff; negative: parked.
    next_retry_at_ms: Option<i64>,
    project: Option<ProjectView>,
    source: Option<SourceProgressView>,
    summary: Option<StructuredSummary>,
    delivery: Option<DeliveryView>,
}

#[derive(Debug, Clone, Serialize)]
struct DeliveryView {
    state: String,
    note_path: Option<String>,
    note_link: Option<String>,
    delivered_revision: Option<u64>,
    history_commit: Option<String>,
    attempts: u32,
    last_error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectView {
    identity: String,
    component: String,
    project: String,
    repository: Option<String>,
    branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SourceProgressView {
    normalizer_version: u32,
    record_count: u64,
    byte_offset: u64,
    prefix_hash: String,
    source_hash: String,
    source_bytes: u64,
    started_at: Option<String>,
    updated_at: Option<String>,
    user_requests: usize,
    assistant_messages: usize,
    tool_activities: usize,
    fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RetryReport {
    schema_version: u32,
    command: &'static str,
    source: Option<String>,
    session_id: String,
    force: bool,
    result: String,
    code: Option<String>,
    state_before: Option<String>,
    state_after: Option<String>,
    archive_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RetryAllReport {
    schema_version: u32,
    command: &'static str,
    force: bool,
    requested_limit: usize,
    attempted: usize,
    archived: usize,
    not_archive_worthy: usize,
    not_eligible: usize,
    failed: usize,
    items: Vec<RetryItem>,
}

#[derive(Debug, Clone, Serialize)]
struct RetryItem {
    source: String,
    session_id: String,
    result: String,
    code: Option<String>,
    archive_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ConfigurationCheckReport {
    schema_version: u32,
    command: &'static str,
    state_directory: String,
    configuration: ConfigurationAssessment,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorReport {
    schema_version: u32,
    command: &'static str,
    state_directory: String,
    status: CheckStatus,
    configuration: ConfigurationAssessment,
    checks: Vec<CheckResult>,
    sessions: SessionStateSummary,
    last_failure: Option<HookFailure>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawStoredConfig {
    version: Option<u32>,
    summarizer: Option<RawStoredCommand>,
    output_directory: Option<String>,
    state_directory: Option<String>,
    archive_git_history: Option<bool>,
    local_archival_enabled: Option<bool>,
    transcript_processing_accepted: Option<bool>,
    project_origin: Option<String>,
    /// The unified v2 summary-delivery section (issue #36).
    #[serde(default)]
    summary_delivery: Option<RawDelivery>,
    /// Legacy v1 enablement flag, read so doctor can still describe an unmigrated configuration.
    remote_delivery: Option<bool>,
    /// Legacy v1 sink section, read so doctor can still describe an unmigrated configuration.
    #[serde(default)]
    delivery: Option<RawDelivery>,
    policy: Option<RawPolicy>,
    #[serde(default)]
    limits: Option<RawLimits>,
    #[serde(default)]
    harnesses: Option<RawHarnesses>,
}

/// The subset of the `limits` section doctor re-checks: the two knobs whose relation `register`
/// and `archive` enforce but a hand-edited `config.json` can still violate (issue #52).
#[derive(Debug, Clone, Default, Deserialize)]
struct RawLimits {
    max_input_bytes: Option<usize>,
    /// Absent in configurations written before issue #48; those load on the built-in default.
    chunk_threshold_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawHarnesses {
    copilot_home: Option<String>,
    #[allow(dead_code)]
    claude_home: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawDelivery {
    #[serde(default)]
    enabled: Option<bool>,
    endpoint: Option<String>,
    vault: Option<String>,
    #[serde(default)]
    provision_history: Option<bool>,
}

impl RawDelivery {
    fn is_addressable(&self) -> bool {
        self.endpoint
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            && self.vault.as_deref().is_some_and(|value| !value.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawStoredCommand {
    executable: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPolicy {
    max_calls_per_hour: Option<u32>,
    max_calls_per_day: Option<u32>,
    max_concurrency: Option<usize>,
    disabled_projects: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawHookFile {
    version: Option<u32>,
    hooks: Option<RawHookEvents>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawHookEvents {
    #[serde(rename = "agentStop")]
    agent_stop: Option<Vec<RawHookCommand>>,
    #[serde(rename = "sessionEnd")]
    session_end: Option<Vec<RawHookCommand>>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawHookCommand {
    #[serde(rename = "type")]
    kind: Option<String>,
    exec: Option<String>,
    args: Option<Vec<String>>,
    #[serde(rename = "timeoutSec")]
    timeout_seconds: Option<u64>,
}

fn main() -> ExitCode {
    match run() {
        Ok(Outcome::Archive(ArchiveOutcome::Archived { id, relative_path })) => {
            println!("archived {id} -> {}", relative_path.display());
            ExitCode::SUCCESS
        }
        Ok(Outcome::Archive(ArchiveOutcome::NotArchiveWorthy { id })) => {
            eprintln!("not archived: {id} is not archive-worthy");
            ExitCode::from(2)
        }
        Ok(Outcome::Registered { hook_paths }) => {
            for hook_path in hook_paths {
                println!("registered Munshi hooks at {}", hook_path.display());
            }
            ExitCode::SUCCESS
        }
        Ok(Outcome::Unregistered) => {
            println!("unregistered Munshi hooks");
            ExitCode::SUCCESS
        }
        Ok(Outcome::DryRun) => ExitCode::SUCCESS,
        Ok(Outcome::Hook | Outcome::Worker) => ExitCode::SUCCESS,
        Ok(Outcome::Wait(result)) => {
            println!(
                "{}",
                serde_json::to_string(&result).expect("hook result serializes")
            );
            if matches!(result, HookResult::Failed { .. }) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Outcome::Project(status)) => {
            println!(
                "project {} enabled={} reason={} max_calls_per_hour={} max_calls_per_day={}",
                status.identity,
                status.enabled,
                status.disabled_reason.unwrap_or("none"),
                status.max_calls_per_hour,
                status.max_calls_per_day
            );
            ExitCode::SUCCESS
        }
        Ok(Outcome::DeliveryConfigured { settings }) => {
            println!(
                "configured Notesmith sink endpoint={} vault={}",
                settings.endpoint.as_deref().unwrap_or("<unset>"),
                settings.vault.as_deref().unwrap_or("<unset>")
            );
            ExitCode::SUCCESS
        }
        Ok(Outcome::DeliveryEnabled { settings, backfill }) => {
            println!(
                "summary delivery enabled (endpoint {}, vault {})",
                settings.endpoint.as_deref().unwrap_or("<unset>"),
                settings.vault.as_deref().unwrap_or("<unset>")
            );
            if let Some(backfill) = backfill {
                backfill.print_human();
            }
            ExitCode::SUCCESS
        }
        Ok(Outcome::DeliveryDisabled { settings }) => {
            let _ = settings;
            println!("summary delivery disabled; existing delivery history is retained");
            ExitCode::SUCCESS
        }
        Ok(Outcome::DeliveryStatus { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                report.print_human();
            }
            ExitCode::SUCCESS
        }
        Ok(Outcome::DeliveryRun { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                report.print_human();
            }
            if report.failed > 0 || report.blocked > 0 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Outcome::DeliveryHistory { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                report.print_human();
            }
            if report.ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Ok(Outcome::ArchiveUploadConfigured { settings }) => {
            println!(
                "configured Patwari archive server endpoint={}",
                settings.endpoint.as_deref().unwrap_or("<unset>")
            );
            ExitCode::SUCCESS
        }
        Ok(Outcome::ArchiveUploadEnabled { settings }) => {
            println!(
                "archive upload enabled (endpoint {})",
                settings.endpoint.as_deref().unwrap_or("<unset>")
            );
            ExitCode::SUCCESS
        }
        Ok(Outcome::ArchiveUploadDisabled { settings }) => {
            let _ = settings;
            println!("archive upload disabled; existing upload history is retained");
            ExitCode::SUCCESS
        }
        Ok(Outcome::ArchiveUploadStatus { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                report.print_human();
            }
            ExitCode::SUCCESS
        }
        Ok(Outcome::ArchiveUploadRun { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                report.print_human();
            }
            if report.failed > 0 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Outcome::Status { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                print_status_human(&report);
            }
            ExitCode::SUCCESS
        }
        Ok(Outcome::Sessions { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                print_sessions_human(&report);
            }
            ExitCode::SUCCESS
        }
        Ok(Outcome::Attempts { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                print_attempts_human(&report);
            }
            ExitCode::SUCCESS
        }
        Ok(Outcome::Diagnostics { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                print_diagnostics_human(&report);
            }
            ExitCode::SUCCESS
        }
        Ok(Outcome::Show { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                print_show_human(&report);
            }
            if report.found {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            }
        }
        Ok(Outcome::Retrieve {
            result,
            query,
            output,
            force,
            json,
        }) => emit_retrieve(*result, query, output, force, json),
        Ok(Outcome::VerifyArchiveParse { result, json }) => {
            emit_verify_archive_parse(*result, json)
        }
        Ok(Outcome::Retry { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                print_retry_human(&report);
            }
            if matches!(report.result.as_str(), "failed" | "not-found") {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Outcome::RetryAll { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                print_retry_all_human(&report);
            }
            if report.failed > 0 {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Outcome::ConfigurationCheck { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                print_configuration_check_human(&report);
            }
            if report.configuration.status == CheckStatus::Error {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Outcome::Doctor { report, json }) => {
            if json {
                emit_json(&report);
            } else {
                print_doctor_human(&report);
            }
            if report.status == CheckStatus::Error {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<Outcome, Box<dyn Error>> {
    match Cli::parse().command {
        Command::Archive {
            session_id,
            source,
            events,
            copilot_home,
            project_dir,
            output_dir,
            summarizer,
            summarizer_args,
            summarizer_env,
            timeout_ms,
            max_source_bytes,
            max_input_bytes,
            max_stdout_bytes,
            max_stderr_bytes,
            state_dir,
        } => {
            let source = SourceKind::parse_selector(&source)
                .ok_or_else(|| format!("unsupported source: {source}"))?;
            // Elide oversized events on the registered threshold so manual archival matches the hook
            // path; fall back to the built-in default when the directory is not a registration.
            let state_directory = resolve_state_directory(state_dir);
            let registered = state_directory.as_deref().ok();
            let max_event_text_bytes = registered
                .map(munshi::configured_max_event_text_bytes)
                .unwrap_or(munshi::DEFAULT_MAX_EVENT_TEXT_BYTES);
            // Manual archival is one-shot, so its input cap is the only bound in play — but it
            // must still respect the registered relation (issue #52), or a manual run would fail
            // deterministically on size exactly where the hook path would chunk.
            let chunk_threshold_bytes = registered
                .map(munshi::configured_chunk_threshold_bytes)
                .unwrap_or(munshi::DEFAULT_CHUNK_THRESHOLD_BYTES);
            munshi::validate_input_cap_relation(max_input_bytes, chunk_threshold_bytes)?;
            Ok(Outcome::Archive(archive_session(&ArchiveConfig {
                reference: SessionReference {
                    source,
                    session_id,
                    events_path: events,
                    copilot_home,
                },
                project_directory: project_dir,
                output_directory: output_dir,
                summarizer_binary: summarizer,
                summarizer_args,
                summarizer_env,
                timeout: Duration::from_millis(timeout_ms),
                max_source_bytes,
                max_input_bytes,
                max_stdout_bytes,
                max_stderr_bytes,
                max_event_text_bytes,
            })?))
        }
        Command::Register {
            accept_transcript_processing,
            dry_run,
            harnesses,
            copilot_home,
            claude_home,
            state_dir,
            output_dir,
            archive_git_history,
            summarizer,
            summarizer_args,
            summarizer_env,
            timeout_ms,
            max_source_bytes,
            max_input_bytes,
            max_stdout_bytes,
            max_stderr_bytes,
            chunk_threshold_bytes,
            chunk_size_bytes,
            max_calls_per_hour,
            max_calls_per_day,
            max_concurrency,
        } => {
            // Cross-flag validation runs before the disclosure prompt and any file write, so a
            // rejected registration leaves nothing behind (issue #52).
            munshi::validate_input_cap_relation(max_input_bytes, chunk_threshold_bytes)?;
            eprintln!(
                "Configured local output directory: {}",
                output_dir.display()
            );
            accept_disclosure_from_terminal(accept_transcript_processing)?;
            let copilot_home_selected = copilot_home.is_some();
            let claude_home_selected = claude_home.is_some();
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let claude_home = resolve_claude_home(claude_home)?;
            let state_directory = resolve_state_directory(state_dir)?;
            let executable = std::env::current_exe()?.canonicalize()?;
            let selected = if !harnesses.is_empty() {
                harnesses
            } else if copilot_home_selected || claude_home_selected {
                // An explicit home flag is an explicit harness selection; never widen it to
                // other harnesses the machine happens to have installed.
                let mut selected = Vec::new();
                if copilot_home_selected {
                    selected.push(HarnessSelector::Copilot);
                }
                if claude_home_selected {
                    selected.push(HarnessSelector::ClaudeCode);
                }
                selected
            } else {
                // Nothing specified: target every harness that appears installed.
                let mut detected = Vec::new();
                if copilot_home.is_dir() {
                    detected.push(HarnessSelector::Copilot);
                }
                if claude_home.is_dir() {
                    detected.push(HarnessSelector::ClaudeCode);
                }
                if detected.is_empty() {
                    return Err(format!(
                        "no harness detected at {} or {}; pass --harness copilot or --harness claude-code",
                        copilot_home.display(),
                        claude_home.display()
                    )
                    .into());
                }
                detected
            };
            let copilot =
                selected
                    .contains(&HarnessSelector::Copilot)
                    .then(|| munshi::CopilotTarget {
                        home: copilot_home.clone(),
                    });
            let claude =
                selected
                    .contains(&HarnessSelector::ClaudeCode)
                    .then(|| munshi::ClaudeTarget {
                        home: claude_home.clone(),
                    });
            let mut hook_paths = Vec::new();
            if copilot.is_some() {
                hook_paths.push(copilot_home.join("hooks/munshi.json"));
            }
            if claude.is_some() {
                hook_paths.push(claude_home.join("settings.json"));
            }
            if dry_run {
                for hook_path in &hook_paths {
                    println!("would write {}", hook_path.display());
                }
                println!(
                    "would write {}",
                    state_directory.join("config.json").display()
                );
                return Ok(Outcome::DryRun);
            }
            register(&RegisterConfig {
                copilot,
                claude,
                state_directory,
                output_directory: output_dir,
                archive_git_history,
                summarizer_binary: summarizer,
                summarizer_args,
                summarizer_env,
                timeout: Duration::from_millis(timeout_ms),
                max_source_bytes,
                max_input_bytes,
                max_stdout_bytes,
                max_stderr_bytes,
                chunk_threshold_bytes,
                chunk_size_bytes,
                max_calls_per_hour,
                max_calls_per_day,
                max_concurrency,
                executable,
            })?;
            Ok(Outcome::Registered { hook_paths })
        }
        Command::Unregister {
            copilot_home,
            state_dir,
        } => {
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let state_directory = resolve_state_directory(state_dir)?;
            unregister(&state_directory, &copilot_home)?;
            Ok(Outcome::Unregistered)
        }
        Command::Project(ProjectCommand::Disable {
            project_dir,
            state_dir,
        }) => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::Project(set_project_enabled(
                &state_directory,
                &project_dir,
                false,
            )?))
        }
        Command::Project(ProjectCommand::Enable {
            project_dir,
            state_dir,
        }) => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::Project(set_project_enabled(
                &state_directory,
                &project_dir,
                true,
            )?))
        }
        Command::Project(ProjectCommand::Status {
            project_dir,
            state_dir,
        }) => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::Project(project_status(
                &state_directory,
                &project_dir,
            )?))
        }
        Command::SummaryDelivery(command) => run_summary_delivery(command),
        Command::ArchiveUpload(command) => run_archive_upload(command),
        Command::Status { state_dir, json } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::Status {
                report: Box::new(build_status_report(&state_directory)?),
                json,
            })
        }
        Command::Sessions {
            state_dir,
            json,
            state,
            limit,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::Sessions {
                report: Box::new(build_sessions_report(&state_directory, state, limit)?),
                json,
            })
        }
        Command::Attempts {
            state_dir,
            json,
            limit,
            since_ms,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::Attempts {
                report: Box::new(build_attempts_report(&state_directory, since_ms, limit)?),
                json,
            })
        }
        Command::Diagnostics {
            state_dir,
            json,
            limit,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::Diagnostics {
                report: Box::new(build_diagnostics_report(&state_directory, limit)?),
                json,
            })
        }
        Command::Show {
            session_id,
            source,
            state_dir,
            json,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            let source = source.as_deref().map(parse_source_selector).transpose()?;
            Ok(Outcome::Show {
                report: Box::new(build_show_report(&state_directory, source, &session_id)?),
                json,
            })
        }
        Command::Retrieve {
            sha256,
            query,
            output,
            force,
            list,
            endpoint,
            max_download_bytes,
            state_dir,
            json,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            let result = retrieve(
                &state_directory,
                endpoint.as_deref(),
                &sha256,
                list,
                max_download_bytes,
            );
            Ok(Outcome::Retrieve {
                result: Box::new(result),
                query,
                output,
                force,
                json,
            })
        }
        Command::VerifyArchiveParse {
            session,
            all,
            endpoint,
            max_download_bytes,
            state_dir,
            json,
        } => {
            // clap guarantees exactly one of --session/--all; `all` needs no further inspection.
            let _ = all;
            let state_directory = resolve_state_directory(state_dir)?;
            let result = verify_archive_parse(
                &state_directory,
                endpoint.as_deref(),
                session.as_deref(),
                max_download_bytes,
            );
            Ok(Outcome::VerifyArchiveParse {
                result: Box::new(result),
                json,
            })
        }
        Command::Retry {
            session_id,
            source,
            state_dir,
            force,
            json,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            let source = source.as_deref().map(parse_source_selector).transpose()?;
            Ok(Outcome::Retry {
                report: Box::new(build_retry_report(
                    &state_directory,
                    source,
                    &session_id,
                    force,
                )?),
                json,
            })
        }
        Command::RetryAll {
            state_dir,
            force,
            json,
            limit,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::RetryAll {
                report: Box::new(build_retry_all_report(&state_directory, force, limit)?),
                json,
            })
        }
        Command::Doctor { state_dir, json } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::Doctor {
                report: Box::new(build_doctor_report(&state_directory)?),
                json,
            })
        }
        Command::ConfigurationCheck { state_dir, json } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::ConfigurationCheck {
                report: Box::new(build_configuration_check_report(&state_directory)),
                json,
            })
        }
        Command::Hook(HookCommand::AgentStop { state_dir, source }) => {
            if let (Ok(state_dir), Ok(source)) = (
                resolve_state_directory(state_dir),
                parse_source_selector(&source),
            ) {
                handle_hook(
                    HookEvent::AgentStop,
                    source,
                    &state_dir,
                    std::io::stdin().lock(),
                );
            }
            Ok(Outcome::Hook)
        }
        Command::Hook(HookCommand::SessionEnd { state_dir, source }) => {
            if let (Ok(state_dir), Ok(source)) = (
                resolve_state_directory(state_dir),
                parse_source_selector(&source),
            ) {
                handle_hook(
                    HookEvent::SessionEnd,
                    source,
                    &state_dir,
                    std::io::stdin().lock(),
                );
            }
            Ok(Outcome::Hook)
        }
        Command::HookWorker {
            state_dir,
            source,
            session_id,
        } => {
            let source = parse_source_selector(&source)?;
            let _ = run_archive_worker_for_source(&state_dir, source, &session_id)?;
            Ok(Outcome::Worker)
        }
        Command::Hook(HookCommand::Wait {
            state_dir,
            source,
            session_id,
            timeout_ms,
        }) => Ok(Outcome::Wait(wait_for_hook_result_for_source(
            &state_dir,
            parse_source_selector(&source)?,
            &session_id,
            Duration::from_millis(timeout_ms),
        )?)),
        Command::Hook(HookCommand::Recover {
            state_dir,
            stale_after_ms,
            force_retry,
            rebuild_state,
        }) => {
            run_recovery(
                &state_dir,
                Duration::from_millis(stale_after_ms),
                force_retry,
                rebuild_state,
            )?;
            Ok(Outcome::Worker)
        }
    }
}

fn run_summary_delivery(command: SummaryDeliveryCommand) -> Result<Outcome, Box<dyn Error>> {
    match command {
        SummaryDeliveryCommand::Configure {
            endpoint,
            vault,
            folder,
            credential_env,
            credential_keychain,
            max_attempts,
            provision_history,
            no_provision_history,
            state_dir,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            let credential = resolve_credential_source(credential_env, credential_keychain)?;
            let provision = if provision_history {
                Some(true)
            } else if no_provision_history {
                Some(false)
            } else {
                None
            };
            let settings = configure_delivery(
                &state_directory,
                DeliverySinkConfig {
                    endpoint,
                    vault,
                    folder,
                    credential,
                    max_attempts,
                    provision_history: provision,
                },
            )?;
            Ok(Outcome::DeliveryConfigured {
                settings: Box::new(settings),
            })
        }
        SummaryDeliveryCommand::Enable { state_dir } => {
            let state_directory = resolve_state_directory(state_dir)?;
            let settings = set_delivery_enabled(&state_directory, true)?;
            // Report the pending backfill as a dry run so existing summaries need confirmation.
            let backfill = delivery_backfill(&state_directory, false, usize::MAX)
                .ok()
                .map(Box::new);
            Ok(Outcome::DeliveryEnabled {
                settings: Box::new(settings),
                backfill,
            })
        }
        SummaryDeliveryCommand::Disable { state_dir } => {
            let state_directory = resolve_state_directory(state_dir)?;
            let settings = set_delivery_enabled(&state_directory, false)?;
            Ok(Outcome::DeliveryDisabled {
                settings: Box::new(settings),
            })
        }
        SummaryDeliveryCommand::History {
            state_dir,
            configure,
            json,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::DeliveryHistory {
                report: Box::new(delivery_verify_history(&state_directory, configure)?),
                json,
            })
        }
        SummaryDeliveryCommand::Status { state_dir, json } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::DeliveryStatus {
                report: Box::new(delivery_status(&state_directory)?),
                json,
            })
        }
        SummaryDeliveryCommand::Backfill {
            state_dir,
            confirm,
            limit,
            json,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::DeliveryRun {
                report: Box::new(delivery_backfill(&state_directory, confirm, limit)?),
                json,
            })
        }
        SummaryDeliveryCommand::Retry {
            session_id,
            source,
            all,
            force,
            state_dir,
            limit,
            json,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            if session_id.is_none() && !all {
                return Err("pass a session ID or --all to retry deliveries".into());
            }
            let source = source.as_deref().map(parse_source_selector).transpose()?;
            Ok(Outcome::DeliveryRun {
                report: Box::new(delivery_retry(
                    &state_directory,
                    source,
                    session_id,
                    all,
                    force,
                    limit,
                )?),
                json,
            })
        }
    }
}

fn run_archive_upload(command: ArchiveUploadCommand) -> Result<Outcome, Box<dyn Error>> {
    match command {
        ArchiveUploadCommand::Configure {
            endpoint,
            state_dir,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            let settings = configure_archive_upload(&state_directory, &endpoint)?;
            Ok(Outcome::ArchiveUploadConfigured {
                settings: Box::new(settings),
            })
        }
        ArchiveUploadCommand::Enable { state_dir } => {
            let state_directory = resolve_state_directory(state_dir)?;
            let settings = set_archive_upload_enabled(&state_directory, true)?;
            Ok(Outcome::ArchiveUploadEnabled {
                settings: Box::new(settings),
            })
        }
        ArchiveUploadCommand::Disable { state_dir } => {
            let state_directory = resolve_state_directory(state_dir)?;
            let settings = set_archive_upload_enabled(&state_directory, false)?;
            Ok(Outcome::ArchiveUploadDisabled {
                settings: Box::new(settings),
            })
        }
        ArchiveUploadCommand::Status { state_dir, json } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::ArchiveUploadStatus {
                report: Box::new(archive_upload_status(&state_directory)?),
                json,
            })
        }
        ArchiveUploadCommand::Backfill {
            state_dir,
            limit,
            json,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::ArchiveUploadRun {
                report: Box::new(archive_upload_backfill(&state_directory, limit)?),
                json,
            })
        }
        ArchiveUploadCommand::Retry {
            session_id,
            source,
            all,
            force,
            state_dir,
            limit,
            json,
        } => {
            let state_directory = resolve_state_directory(state_dir)?;
            if session_id.is_none() && !all {
                return Err("pass a session ID or --all to retry archive uploads".into());
            }
            let source = source.as_deref().map(parse_source_selector).transpose()?;
            Ok(Outcome::ArchiveUploadRun {
                report: Box::new(archive_upload_retry(
                    &state_directory,
                    source,
                    session_id,
                    all,
                    force,
                    limit,
                )?),
                json,
            })
        }
    }
}

fn resolve_credential_source(
    credential_env: Option<String>,
    credential_keychain: Option<String>,
) -> Result<Option<DeliveryCredentialSource>, Box<dyn Error>> {
    if let Some(var) = credential_env {
        return Ok(Some(DeliveryCredentialSource::Env { var }));
    }
    if let Some(entry) = credential_keychain {
        let (service, account) = entry
            .split_once(':')
            .ok_or("--credential-keychain must be formatted as service:account")?;
        if service.is_empty() || account.is_empty() {
            return Err("--credential-keychain must be formatted as service:account".into());
        }
        return Ok(Some(DeliveryCredentialSource::Keychain {
            service: service.to_owned(),
            account: account.to_owned(),
        }));
    }
    Ok(None)
}

fn build_status_report(state_directory: &Path) -> Result<StatusReport, Box<dyn Error>> {
    let configuration = inspect_configuration(state_directory);
    let sessions = load_sessions(state_directory)?;
    Ok(StatusReport {
        schema_version: 1,
        command: "status",
        state_directory: state_directory.display().to_string(),
        configuration,
        sessions: summarize_sessions(&sessions),
        last_failure: read_last_failure_if_available(state_directory),
    })
}

fn build_sessions_report(
    state_directory: &Path,
    filter: Option<SessionStateFilter>,
    limit: usize,
) -> Result<SessionsReport, Box<dyn Error>> {
    let sessions = load_sessions(state_directory)?;
    let wanted = filter.map(session_filter_name);
    let mut items = sessions
        .iter()
        .map(session_record_to_item)
        .collect::<Vec<_>>();
    if let Some(wanted) = wanted {
        items.retain(|item| item.state == wanted);
    }
    let total = items.len();
    items.truncate(limit);
    Ok(SessionsReport {
        schema_version: 1,
        command: "sessions",
        filter: filter.map(session_filter_name).map(ToOwned::to_owned),
        total,
        returned: items.len(),
        items,
    })
}

fn build_attempts_report(
    state_directory: &Path,
    since_ms: Option<i64>,
    limit: usize,
) -> Result<AttemptsReport, Box<dyn Error>> {
    let (total, records) = load_attempts(state_directory, since_ms, limit)?;
    let items = records
        .into_iter()
        .map(|record| AttemptListItem {
            source: record.source.as_selector().to_owned(),
            session_id: record.session_id,
            project: record.project,
            outcome: record.outcome,
            error_category: record.error_category,
            started_at_ms: record.started_at_ms,
            finished_at_ms: record.finished_at_ms,
        })
        .collect::<Vec<_>>();
    Ok(AttemptsReport {
        schema_version: 1,
        command: "attempts",
        since_ms,
        total,
        returned: items.len(),
        items,
    })
}

fn build_diagnostics_report(
    state_directory: &Path,
    limit: usize,
) -> Result<DiagnosticsReport, Box<dyn Error>> {
    let (total, records) = load_diagnostics(state_directory, limit)?;
    let items = records
        .into_iter()
        .map(|record| DiagnosticListItem {
            source: record.source.map(|source| source.as_selector().to_owned()),
            session_id: record.session_id,
            operation: record.operation,
            category: record.category,
            cause_category: record.cause_category,
            recorded_at_ms: record.recorded_at_ms,
        })
        .collect::<Vec<_>>();
    Ok(DiagnosticsReport {
        schema_version: 1,
        command: "diagnostics",
        total,
        returned: items.len(),
        items,
    })
}

fn build_show_report(
    state_directory: &Path,
    source: Option<SourceKind>,
    session_id: &str,
) -> Result<ShowReport, Box<dyn Error>> {
    let record = match resolve_session_target(state_directory, source, session_id)? {
        SessionTarget::One(record) => *record,
        SessionTarget::NotFound => {
            return Ok(ShowReport {
                schema_version: 1,
                command: "show",
                found: false,
                session: None,
            });
        }
        SessionTarget::Ambiguous(sources) => {
            return Err(ambiguous_source_error(session_id, &sources));
        }
    };

    let configuration = inspect_configuration(state_directory);
    let mut summary = record.current_summary.clone();
    let mut project = record.project.clone().map(|project| ProjectView {
        identity: project.identity,
        component: project.component,
        project: project.project,
        repository: project.repository,
        branch: project.branch,
    });

    if let (None, Some(output_directory), Some(relative)) = (
        summary.as_ref(),
        configuration.output_directory.as_deref(),
        record.markdown_relative_path.as_deref(),
    ) {
        if let Ok(markdown) = fs::read_to_string(Path::new(output_directory).join(relative))
            && let Ok(parsed) = parse_archive_markdown(&markdown)
        {
            summary = Some(parsed.summary);
            if project.is_none() {
                project = Some(ProjectView {
                    identity: parsed.project.identity,
                    component: parsed.project.component,
                    project: parsed.project.project,
                    repository: parsed.project.repository,
                    branch: parsed.project.branch,
                });
            }
        }
    }

    let source = record
        .previous_source
        .clone()
        .map(|source| SourceProgressView {
            normalizer_version: source.normalizer_version,
            record_count: source.record_count,
            byte_offset: source.byte_offset,
            prefix_hash: source.prefix_hash,
            source_hash: source.source_hash,
            source_bytes: source.source_bytes,
            started_at: source.started_at,
            updated_at: source.updated_at,
            user_requests: source.user_requests,
            assistant_messages: source.assistant_messages,
            tool_activities: source.tool_activities,
            fallback_reason: record.fallback_reason.clone(),
        });

    let state = operational_state(&record).to_owned();
    let delivery = lookup_delivery_view(state_directory, record.source, &record.session_id);
    let view = ShowSessionView {
        source_kind: record.source.as_selector().to_owned(),
        session_id: record.session_id.clone(),
        state,
        lifecycle_state: record.lifecycle_state.clone(),
        revision: record.current_revision,
        completion_reason: record.completion_reason.clone(),
        summary_title: summary.as_ref().map(|summary| summary.title.clone()),
        archive_path: record
            .markdown_relative_path
            .map(|path| path.to_string_lossy().into_owned()),
        last_error_code: record.last_error_category.clone(),
        failure_streak: record.failure_streak,
        next_retry_at_ms: record.next_retry_at_ms,
        project,
        source,
        summary,
        delivery,
    };

    Ok(ShowReport {
        schema_version: 1,
        command: "show",
        found: true,
        session: Some(view),
    })
}

fn build_retry_report(
    state_directory: &Path,
    source: Option<SourceKind>,
    session_id: &str,
    force: bool,
) -> Result<RetryReport, Box<dyn Error>> {
    let before = match resolve_session_target(state_directory, source, session_id)? {
        SessionTarget::One(record) => *record,
        SessionTarget::NotFound => {
            return Ok(RetryReport {
                schema_version: 1,
                command: "retry",
                source: source.map(|source| source.as_selector().to_owned()),
                session_id: session_id.to_owned(),
                force,
                result: "not-found".to_owned(),
                code: None,
                state_before: None,
                state_after: None,
                archive_path: None,
            });
        }
        SessionTarget::Ambiguous(sources) => {
            return Err(ambiguous_source_error(session_id, &sources));
        }
    };
    let target_source = before.source;
    let before_state = operational_state(&before).to_owned();

    if !is_retryable_lifecycle(&before.lifecycle_state) {
        return Ok(RetryReport {
            schema_version: 1,
            command: "retry",
            source: Some(target_source.as_selector().to_owned()),
            session_id: session_id.to_owned(),
            force,
            result: "not-eligible".to_owned(),
            code: None,
            state_before: Some(before_state.clone()),
            state_after: Some(before_state),
            archive_path: None,
        });
    }

    if state_database_exists(state_directory) {
        let mut state = StateStore::open_for_source(state_directory, target_source)?;
        // A targeted retry is an explicit operator action: lift a repeat-failure park
        // (issue #38) so the worker makes a real attempt with a fresh failure streak instead
        // of replaying the parked verdict. Plain sweeps and `retry-all` without `--force`
        // never lift these.
        let _ = state.lift_failure_park(session_id)?;
        let _ = state.reserve_worker(session_id, force)?;
    }

    let hook = run_archive_worker_for_source(state_directory, target_source, session_id).unwrap_or(
        HookResult::Failed {
            code: "worker-error".to_owned(),
        },
    );
    let (result, code, archive_path) = retry_fields_from_hook(hook);
    let state_after = resolved_operational_state(state_directory, target_source, session_id)?
        .or(Some(before_state.clone()));

    Ok(RetryReport {
        schema_version: 1,
        command: "retry",
        source: Some(target_source.as_selector().to_owned()),
        session_id: session_id.to_owned(),
        force,
        result,
        code,
        state_before: Some(before_state),
        state_after,
        archive_path,
    })
}

/// Reads the current operational state of a specific source's session, scoping the state
/// store to that source so the lookup can never observe a different source's same-ID row.
fn resolved_operational_state(
    state_directory: &Path,
    source: SourceKind,
    session_id: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    if !state_database_exists(state_directory) {
        return Ok(None);
    }
    let state = StateStore::open_for_source(state_directory, source)?;
    Ok(state
        .get_session(session_id)?
        .map(|record| operational_state(&record).to_owned()))
}

fn build_retry_all_report(
    state_directory: &Path,
    force: bool,
    limit: usize,
) -> Result<RetryAllReport, Box<dyn Error>> {
    if !state_database_exists(state_directory) {
        return Ok(RetryAllReport {
            schema_version: 1,
            command: "retry-all",
            force,
            requested_limit: limit,
            attempted: 0,
            archived: 0,
            not_archive_worthy: 0,
            not_eligible: 0,
            failed: 0,
            items: Vec::new(),
        });
    }

    // Re-evaluate permanent `source-failed` parks against the currently configured source limit
    // (issue #44) so sessions that failed under a since-raised limit are eligible again below.
    lift_stale_source_limit_parks(state_directory)?;
    let mut state = StateStore::open(state_directory)?;
    let reserved = state.reserve_eligible_workers(force, limit)?;
    drop(state);

    let mut items = Vec::new();
    let mut archived = 0;
    let mut not_archive_worthy = 0;
    let mut not_eligible = 0;
    let mut failed = 0;

    for (source, session_id) in reserved {
        let hook = run_archive_worker_for_source(state_directory, source, &session_id).unwrap_or(
            HookResult::Failed {
                code: "worker-error".to_owned(),
            },
        );
        let (result, code, archive_path) = retry_fields_from_hook(hook);
        match result.as_str() {
            "archived" => archived += 1,
            "not-archive-worthy" => not_archive_worthy += 1,
            "not-eligible" => not_eligible += 1,
            _ => failed += 1,
        }
        items.push(RetryItem {
            source: source.as_selector().to_owned(),
            session_id,
            result,
            code,
            archive_path,
        });
    }

    Ok(RetryAllReport {
        schema_version: 1,
        command: "retry-all",
        force,
        requested_limit: limit,
        attempted: items.len(),
        archived,
        not_archive_worthy,
        not_eligible,
        failed,
        items,
    })
}

fn build_configuration_check_report(state_directory: &Path) -> ConfigurationCheckReport {
    ConfigurationCheckReport {
        schema_version: 1,
        command: "configuration-check",
        state_directory: state_directory.display().to_string(),
        configuration: inspect_configuration(state_directory),
    }
}

fn build_doctor_report(state_directory: &Path) -> Result<DoctorReport, Box<dyn Error>> {
    let configuration = inspect_configuration(state_directory);
    let mut checks = configuration.checks.clone();

    if state_directory.exists() {
        if is_writable_directory(state_directory) {
            push_check(
                &mut checks,
                "state-directory",
                CheckStatus::Ok,
                format!("{} is writable", state_directory.display()),
            );
        } else {
            push_check(
                &mut checks,
                "state-directory",
                CheckStatus::Error,
                format!("{} is not writable", state_directory.display()),
            );
        }
    } else {
        push_check(
            &mut checks,
            "state-directory",
            CheckStatus::Warning,
            format!("{} does not exist", state_directory.display()),
        );
    }

    if state_database_exists(state_directory) {
        match StateStore::open(state_directory) {
            Ok(state) => match state.schema_version() {
                Ok(version) => push_check(
                    &mut checks,
                    "state-schema",
                    CheckStatus::Ok,
                    format!("schema_migrations version {version}"),
                ),
                Err(error) => push_check(
                    &mut checks,
                    "state-schema",
                    CheckStatus::Error,
                    format!("failed to read schema version: {error}"),
                ),
            },
            Err(error) => push_check(
                &mut checks,
                "state-open",
                CheckStatus::Error,
                format!("failed to open SQLite state: {error}"),
            ),
        }
    } else {
        push_check(
            &mut checks,
            "state-open",
            CheckStatus::Warning,
            "no SQLite state database found".to_owned(),
        );
    }

    if let Some(executable) = configuration.summarizer_executable.as_deref() {
        let path = Path::new(executable);
        if is_executable_file(path) {
            push_check(
                &mut checks,
                "summarizer-executable",
                CheckStatus::Ok,
                format!("{} is executable", path.display()),
            );
        } else {
            push_check(
                &mut checks,
                "summarizer-executable",
                CheckStatus::Error,
                format!("{} is missing or not executable", path.display()),
            );
        }
    } else {
        push_check(
            &mut checks,
            "summarizer-executable",
            CheckStatus::Warning,
            "summarizer executable is not configured".to_owned(),
        );
    }

    if let Some(output) = configuration.output_directory.as_deref() {
        let output_path = Path::new(output);
        if output_path.exists() {
            if is_writable_directory(output_path) {
                push_check(
                    &mut checks,
                    "output-directory",
                    CheckStatus::Ok,
                    format!("{} is writable", output_path.display()),
                );
            } else {
                push_check(
                    &mut checks,
                    "output-directory",
                    CheckStatus::Error,
                    format!("{} is not writable", output_path.display()),
                );
            }
        } else if output_path
            .parent()
            .is_some_and(|parent| parent.exists() && is_writable_directory(parent))
        {
            push_check(
                &mut checks,
                "output-directory",
                CheckStatus::Ok,
                format!("{} is creatable", output_path.display()),
            );
        } else {
            push_check(
                &mut checks,
                "output-directory",
                CheckStatus::Error,
                format!("{} is not writable or creatable", output_path.display()),
            );
        }
    }

    match configuration.archive_git_history {
        Some(true) => {
            if let Some(output) = configuration.output_directory.as_deref() {
                let output_path = Path::new(output);
                if output_path.exists() {
                    if output_path.join(".git").is_dir() {
                        push_check(
                            &mut checks,
                            "archive-git-repository",
                            CheckStatus::Ok,
                            format!("{} is a Git repository", output_path.display()),
                        );
                    } else {
                        push_check(
                            &mut checks,
                            "archive-git-repository",
                            CheckStatus::Error,
                            format!(
                                "{} is missing .git while archive Git history is enabled",
                                output_path.display()
                            ),
                        );
                    }
                } else {
                    push_check(
                        &mut checks,
                        "archive-git-repository",
                        CheckStatus::Warning,
                        format!(
                            "{} does not exist yet; archive repository will be created on demand",
                            output_path.display()
                        ),
                    );
                }
            } else {
                push_check(
                    &mut checks,
                    "archive-git-repository",
                    CheckStatus::Warning,
                    "archive Git history enabled but output directory is not configured".to_owned(),
                );
            }
        }
        Some(false) => push_check(
            &mut checks,
            "archive-git-repository",
            CheckStatus::Ok,
            "archive Git history disabled".to_owned(),
        ),
        None => push_check(
            &mut checks,
            "archive-git-repository",
            CheckStatus::Warning,
            "archive Git history setting is unknown".to_owned(),
        ),
    }

    let sessions = load_sessions(state_directory)?;
    push_size_cap_park_check(&mut checks, &sessions);
    let status = overall_status(&checks);

    Ok(DoctorReport {
        schema_version: 1,
        command: "doctor",
        state_directory: state_directory.display().to_string(),
        status,
        configuration,
        checks,
        sessions: summarize_sessions(&sessions),
        last_failure: read_last_failure_if_available(state_directory),
    })
}

fn inspect_configuration(state_directory: &Path) -> ConfigurationAssessment {
    let config_path = state_directory.join("config.json");
    // The state directory is harness-neutral (ADR 0008); hook locations come from the
    // configuration's recorded harness homes, not from the state directory's parent.
    let mut copilot_home_recorded: Option<PathBuf> = None;
    let mut claude_home_recorded: Option<PathBuf> = None;

    let mut checks = Vec::new();
    let mut capture_state = CaptureState::Unknown;
    let mut delivery_state = DeliveryState::Unknown;
    let mut summarizer_executable = None;
    let mut output_directory = None;
    let mut archive_git_history = None;
    let mut versioned_delivery = None;
    let mut provision_remote_history = None;
    let mut disabled_projects = 0usize;

    let mut config_recognized = false;
    if !config_path.exists() {
        push_check(
            &mut checks,
            "config-file",
            CheckStatus::Error,
            format!("missing {}", config_path.display()),
        );
    } else {
        match fs::read(&config_path) {
            Ok(bytes) => match serde_json::from_slice::<RawStoredConfig>(&bytes) {
                Ok(config) => {
                    push_check(
                        &mut checks,
                        "config-file",
                        CheckStatus::Ok,
                        format!("loaded {}", config_path.display()),
                    );
                    if config.version == Some(2) {
                        push_check(
                            &mut checks,
                            "config-version",
                            CheckStatus::Ok,
                            "version 2".to_owned(),
                        );
                    } else if config.version == Some(1) {
                        push_check(
                            &mut checks,
                            "config-version",
                            CheckStatus::Ok,
                            "version 1 (superseded; migrates to version 2 on the next configuration load)"
                                .to_owned(),
                        );
                    } else {
                        push_check(
                            &mut checks,
                            "config-version",
                            CheckStatus::Warning,
                            format!("unsupported version {:?}; expected 2", config.version),
                        );
                    }

                    push_input_cap_relation_check(&mut checks, config.limits.as_ref());

                    summarizer_executable =
                        config.summarizer.and_then(|command| command.executable);
                    output_directory = config.output_directory;
                    archive_git_history = config.archive_git_history;
                    copilot_home_recorded = config
                        .harnesses
                        .as_ref()
                        .and_then(|harnesses| harnesses.copilot_home.as_deref())
                        .map(PathBuf::from);
                    claude_home_recorded = config
                        .harnesses
                        .as_ref()
                        .and_then(|harnesses| harnesses.claude_home.as_deref())
                        .map(PathBuf::from);
                    let policy = config.policy.unwrap_or(RawPolicy {
                        max_calls_per_hour: None,
                        max_calls_per_day: None,
                        max_concurrency: None,
                        disabled_projects: None,
                    });
                    let disabled = policy.disabled_projects.clone().unwrap_or_default();
                    disabled_projects = disabled.len();

                    capture_state = if config.local_archival_enabled == Some(true) {
                        if disabled_projects > 0 {
                            CaptureState::DisabledProject
                        } else {
                            CaptureState::Enabled
                        }
                    } else {
                        CaptureState::Unknown
                    };
                    // Prefer the unified v2 `summary_delivery` section; fall back to the legacy v1
                    // `remote_delivery` + `delivery` pair for a not-yet-migrated configuration.
                    let delivery_enabled = config
                        .summary_delivery
                        .as_ref()
                        .and_then(|section| section.enabled)
                        .or(config.remote_delivery);
                    let delivery_section = config
                        .summary_delivery
                        .as_ref()
                        .or(config.delivery.as_ref());
                    delivery_state = match delivery_enabled {
                        Some(false) => DeliveryState::Disabled,
                        Some(true) => {
                            let addressable =
                                delivery_section.is_some_and(RawDelivery::is_addressable);
                            if addressable {
                                DeliveryState::Enabled
                            } else {
                                DeliveryState::DeliveryRelated
                            }
                        }
                        None => DeliveryState::Unknown,
                    };

                    match capture_state {
                        CaptureState::Enabled => push_check(
                            &mut checks,
                            "capture-state",
                            CheckStatus::Ok,
                            "local archival enabled".to_owned(),
                        ),
                        CaptureState::DisabledProject => push_check(
                            &mut checks,
                            "capture-state",
                            CheckStatus::Warning,
                            format!(
                                "{} explicitly disabled project identity{}",
                                disabled_projects,
                                if disabled_projects == 1 { "" } else { "ies" }
                            ),
                        ),
                        CaptureState::Unknown => push_check(
                            &mut checks,
                            "capture-state",
                            CheckStatus::Warning,
                            "local archival state is unknown or malformed".to_owned(),
                        ),
                    }

                    match delivery_state {
                        DeliveryState::Disabled => push_check(
                            &mut checks,
                            "delivery-state",
                            CheckStatus::Ok,
                            "summary delivery disabled".to_owned(),
                        ),
                        DeliveryState::Enabled => push_check(
                            &mut checks,
                            "delivery-state",
                            CheckStatus::Ok,
                            "summary delivery enabled with an addressable Notesmith sink"
                                .to_owned(),
                        ),
                        DeliveryState::DeliveryRelated => push_check(
                            &mut checks,
                            "delivery-state",
                            CheckStatus::Warning,
                            "summary delivery enabled but the Notesmith sink is not addressable"
                                .to_owned(),
                        ),
                        DeliveryState::Unknown => push_check(
                            &mut checks,
                            "delivery-state",
                            CheckStatus::Warning,
                            "summary delivery state is unknown".to_owned(),
                        ),
                    }

                    let state_dir_matches = config
                        .state_directory
                        .as_deref()
                        .is_some_and(|value| Path::new(value) == state_directory);
                    if state_dir_matches {
                        push_check(
                            &mut checks,
                            "state-directory-match",
                            CheckStatus::Ok,
                            "config state_directory matches command scope".to_owned(),
                        );
                    } else {
                        push_check(
                            &mut checks,
                            "state-directory-match",
                            CheckStatus::Error,
                            "config state_directory does not match command scope".to_owned(),
                        );
                    }

                    if config.transcript_processing_accepted == Some(true) {
                        push_check(
                            &mut checks,
                            "transcript-disclosure",
                            CheckStatus::Ok,
                            "transcript disclosure accepted".to_owned(),
                        );
                    } else {
                        push_check(
                            &mut checks,
                            "transcript-disclosure",
                            CheckStatus::Warning,
                            "transcript disclosure not accepted".to_owned(),
                        );
                    }

                    if config.project_origin.as_deref() == Some("agent_stop_cwd") {
                        push_check(
                            &mut checks,
                            "project-origin",
                            CheckStatus::Ok,
                            "project_origin=agent_stop_cwd".to_owned(),
                        );
                    } else {
                        push_check(
                            &mut checks,
                            "project-origin",
                            CheckStatus::Warning,
                            format!("unexpected project_origin {:?}", config.project_origin),
                        );
                    }

                    let policy_checks = (
                        policy.max_calls_per_hour,
                        policy.max_calls_per_day,
                        policy.max_concurrency,
                    );
                    if let (Some(hourly), Some(daily), Some(concurrency)) = policy_checks {
                        let status = if hourly > 0 && daily > 0 && concurrency >= 1 {
                            CheckStatus::Ok
                        } else {
                            CheckStatus::Error
                        };
                        let message = format!(
                            "policy max_calls_per_hour={hourly}, max_calls_per_day={daily}, max_concurrency={concurrency}"
                        );
                        push_check(&mut checks, "policy", status, message);
                    } else {
                        push_check(
                            &mut checks,
                            "policy",
                            CheckStatus::Warning,
                            "policy section is missing expected budget/concurrency values"
                                .to_owned(),
                        );
                    }

                    match archive_git_history {
                        Some(true) => push_check(
                            &mut checks,
                            "archive-git-history",
                            CheckStatus::Ok,
                            "archive Git history is enabled".to_owned(),
                        ),
                        Some(false) => push_check(
                            &mut checks,
                            "archive-git-history",
                            CheckStatus::Ok,
                            "archive Git history is disabled".to_owned(),
                        ),
                        None => push_check(
                            &mut checks,
                            "archive-git-history",
                            CheckStatus::Warning,
                            "archive_git_history is missing from configuration".to_owned(),
                        ),
                    }

                    // Issue #9: when local Git history is enabled alongside delivery, versioned
                    // delivery is required and the Notesmith vault must preserve correlated revision
                    // history. Doctor reports this statically; `munshi summary-delivery history`
                    // probes the live capability.
                    let versioned =
                        archive_git_history == Some(true) && delivery_enabled == Some(true);
                    let provision = delivery_section
                        .and_then(|delivery| delivery.provision_history)
                        .unwrap_or(false);
                    versioned_delivery = Some(versioned);
                    provision_remote_history = Some(provision);
                    if versioned {
                        let hint = if provision {
                            "versioned delivery: Munshi will configure the Notesmith vault's revision history; verify with `munshi summary-delivery history --configure`"
                        } else {
                            "versioned delivery requires remote revision history; verify with `munshi summary-delivery history` (add `--configure` to enable it)"
                        };
                        push_check(
                            &mut checks,
                            "delivery-remote-history",
                            CheckStatus::Warning,
                            hint.to_owned(),
                        );
                    } else if delivery_enabled == Some(true) {
                        push_check(
                            &mut checks,
                            "delivery-remote-history",
                            CheckStatus::Ok,
                            "latest-only delivery (local Git history disabled)".to_owned(),
                        );
                    }

                    let summarizer_absolute = summarizer_executable
                        .as_deref()
                        .is_some_and(|path| Path::new(path).is_absolute());
                    let output_absolute = output_directory
                        .as_deref()
                        .is_some_and(|path| Path::new(path).is_absolute());
                    if summarizer_absolute {
                        push_check(
                            &mut checks,
                            "summarizer-path",
                            CheckStatus::Ok,
                            "summarizer executable path is absolute".to_owned(),
                        );
                    } else {
                        push_check(
                            &mut checks,
                            "summarizer-path",
                            CheckStatus::Error,
                            "summarizer executable path is missing or relative".to_owned(),
                        );
                    }
                    if output_absolute {
                        push_check(
                            &mut checks,
                            "output-path",
                            CheckStatus::Ok,
                            "output directory path is absolute".to_owned(),
                        );
                    } else {
                        push_check(
                            &mut checks,
                            "output-path",
                            CheckStatus::Error,
                            "output directory path is missing or relative".to_owned(),
                        );
                    }

                    // Version 1 remains runtime-compatible: the runtime migrates it forward
                    // losslessly on load (issue #36).
                    config_recognized = matches!(config.version, Some(1) | Some(2))
                        && config.local_archival_enabled == Some(true)
                        && matches!(
                            delivery_state,
                            DeliveryState::Disabled | DeliveryState::Enabled
                        )
                        && config.transcript_processing_accepted == Some(true)
                        && config.project_origin.as_deref() == Some("agent_stop_cwd")
                        && policy.max_concurrency.unwrap_or_default() >= 1
                        && state_dir_matches
                        && summarizer_absolute
                        && output_absolute;
                }
                Err(error) => push_check(
                    &mut checks,
                    "config-parse",
                    CheckStatus::Error,
                    format!("invalid JSON at {}: {error}", config_path.display()),
                ),
            },
            Err(error) => push_check(
                &mut checks,
                "config-read",
                CheckStatus::Error,
                format!("failed to read {}: {error}", config_path.display()),
            ),
        }
    }

    let hook_path = copilot_home_recorded.map(|home| home.join("hooks/munshi.json"));
    let claude_settings_path = claude_home_recorded.map(|home| home.join("settings.json"));
    let copilot_recognized = hook_path
        .as_deref()
        .map(|hook_path| inspect_copilot_hook(hook_path, &mut checks));
    let claude_recognized = claude_settings_path
        .as_deref()
        .map(|settings_path| inspect_claude_hooks(settings_path, &mut checks));
    if copilot_recognized.is_none() && claude_recognized.is_none() {
        push_check(
            &mut checks,
            "hook-file",
            CheckStatus::Error,
            "no harness hook installation recorded in configuration".to_owned(),
        );
    }
    let hook_recognized = (copilot_recognized.is_some() || claude_recognized.is_some())
        && copilot_recognized.unwrap_or(true)
        && claude_recognized.unwrap_or(true);

    let runtime_compatible = config_recognized && hook_recognized;
    if runtime_compatible {
        push_check(
            &mut checks,
            "runtime-compatible",
            CheckStatus::Ok,
            "configuration is compatible with current automatic workers".to_owned(),
        );
    } else {
        push_check(
            &mut checks,
            "runtime-compatible",
            CheckStatus::Warning,
            "configuration is not fully compatible with current automatic workers".to_owned(),
        );
    }

    ConfigurationAssessment {
        status: overall_status(&checks),
        runtime_compatible,
        capture_state,
        delivery_state,
        archive_git_history,
        versioned_delivery,
        provision_remote_history,
        disabled_projects,
        config_path: config_path.display().to_string(),
        hook_path: hook_path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<not-recorded>".to_owned()),
        claude_settings_path: claude_settings_path.map(|path| path.display().to_string()),
        summarizer_executable,
        output_directory,
        checks,
    }
}

/// Doctor check for Munshi's managed entries inside Claude Code's `settings.json`. Compared
/// against the current executable, mirroring how the Copilot hook contract pins absolute paths.
fn inspect_claude_hooks(settings_path: &Path, checks: &mut Vec<CheckResult>) -> bool {
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .unwrap_or_default();
    match munshi::claude_hooks_status(settings_path, &executable) {
        munshi::ClaudeHookStatus::Installed => {
            push_check(
                checks,
                "claude-hook-contract",
                CheckStatus::Ok,
                "Claude Code settings carry the managed hook entries".to_owned(),
            );
            true
        }
        munshi::ClaudeHookStatus::Missing => {
            push_check(
                checks,
                "claude-hook-contract",
                CheckStatus::Error,
                format!(
                    "missing managed hook entries in {}",
                    settings_path.display()
                ),
            );
            false
        }
        munshi::ClaudeHookStatus::Stale => {
            push_check(
                checks,
                "claude-hook-contract",
                CheckStatus::Error,
                "Claude Code hook entries do not match the managed contract for this executable"
                    .to_owned(),
            );
            false
        }
        munshi::ClaudeHookStatus::Foreign => {
            push_check(
                checks,
                "claude-hook-contract",
                CheckStatus::Error,
                format!(
                    "cannot interpret {} as a settings object",
                    settings_path.display()
                ),
            );
            false
        }
    }
}

fn inspect_copilot_hook(hook_path: &Path, checks: &mut Vec<CheckResult>) -> bool {
    if !hook_path.exists() {
        push_check(
            checks,
            "hook-file",
            CheckStatus::Error,
            format!("missing {}", hook_path.display()),
        );
        false
    } else {
        match fs::read(hook_path) {
            Ok(bytes) => match serde_json::from_slice::<RawHookFile>(&bytes) {
                Ok(hook) => {
                    if hook_is_recognized(&hook) {
                        push_check(
                            checks,
                            "hook-contract",
                            CheckStatus::Ok,
                            "hook file matches the 1.0.70 managed contract".to_owned(),
                        );
                        true
                    } else {
                        push_check(
                            checks,
                            "hook-contract",
                            CheckStatus::Error,
                            "hook file does not match the managed contract".to_owned(),
                        );
                        false
                    }
                }
                Err(error) => {
                    push_check(
                        checks,
                        "hook-parse",
                        CheckStatus::Error,
                        format!("invalid JSON at {}: {error}", hook_path.display()),
                    );
                    false
                }
            },
            Err(error) => {
                push_check(
                    checks,
                    "hook-read",
                    CheckStatus::Error,
                    format!("failed to read {}: {error}", hook_path.display()),
                );
                false
            }
        }
    }
}

fn hook_is_recognized(hook: &RawHookFile) -> bool {
    if hook.version != Some(1) {
        return false;
    }
    let Some(events) = hook.hooks.as_ref() else {
        return false;
    };
    hook_command_is_recognized(events.agent_stop.as_deref(), "agent-stop")
        && hook_command_is_recognized(events.session_end.as_deref(), "session-end")
}

fn hook_command_is_recognized(commands: Option<&[RawHookCommand]>, event: &str) -> bool {
    let Some(commands) = commands else {
        return false;
    };
    if commands.len() != 1 {
        return false;
    }
    let command = &commands[0];
    command.kind.as_deref() == Some("command")
        && command.timeout_seconds == Some(2)
        && command
            .exec
            .as_deref()
            .is_some_and(|exec| Path::new(exec).is_absolute())
        && command.args.as_deref() == Some(&["hook".to_owned(), event.to_owned()])
}

fn push_check(
    checks: &mut Vec<CheckResult>,
    code: &'static str,
    status: CheckStatus,
    message: String,
) {
    checks.push(CheckResult {
        code,
        status,
        message,
    });
}

fn overall_status(checks: &[CheckResult]) -> CheckStatus {
    checks.iter().fold(CheckStatus::Ok, |current, check| {
        current.combine(check.status)
    })
}

fn build_session_item(record: &SessionRecord) -> SessionListItem {
    SessionListItem {
        source: record.source.as_selector().to_owned(),
        session_id: record.session_id.clone(),
        state: operational_state(record).to_owned(),
        lifecycle_state: record.lifecycle_state.clone(),
        revision: record.current_revision,
        completion_reason: record.completion_reason.clone(),
        summary_title: record
            .current_summary
            .as_ref()
            .map(|summary| summary.title.clone()),
        archive_path: record
            .markdown_relative_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        last_error_code: record.last_error_category.clone(),
        project: project_label(
            record
                .project
                .as_ref()
                .map(|project| project.project.as_str()),
            record
                .project
                .as_ref()
                .map(|project| project.component.as_str()),
            record.origin_cwd.as_deref(),
        ),
        created_at_ms: record.created_at_ms,
        updated_at_ms: record.updated_at_ms,
    }
}

fn session_record_to_item(record: &SessionRecord) -> SessionListItem {
    build_session_item(record)
}

/// Doctor hint for sessions parked permanently on a size cap (issue #41). A deterministic
/// `source-failed` or `summary-input-limit` verdict parks a session (`next_retry_at_ms < 0`,
/// issues #38/#44), where it is indistinguishable from any other permanent failure in the session
/// counters; this check names the limit flag whose raise lifts the park, so an undersized limit is
/// visible instead of looking like a generic failure.
fn push_size_cap_park_check(checks: &mut Vec<CheckResult>, records: &[SessionRecord]) {
    let parked_on = |category: &str| {
        records
            .iter()
            .filter(|record| {
                record.lifecycle_state == "failed"
                    && record.next_retry_at_ms.is_some_and(|next| next < 0)
                    && record.last_error_category.as_deref() == Some(category)
            })
            .count()
    };
    let source_failed = parked_on("source-failed");
    let input_limited = parked_on("summary-input-limit");
    if source_failed == 0 && input_limited == 0 {
        return;
    }
    let mut parts = Vec::new();
    if source_failed > 0 {
        parts.push(format!(
            "{source_failed} source-failed (raise --max-source-bytes)"
        ));
    }
    if input_limited > 0 {
        parts.push(format!(
            "{input_limited} summary-input-limit (raise --chunk-threshold-bytes)"
        ));
    }
    push_check(
        checks,
        "size-cap-parked",
        CheckStatus::Warning,
        format!(
            "{} session(s) parked on a size cap: {}; re-register with a larger limit, then `munshi retry-all --force`",
            source_failed + input_limited,
            parts.join(", ")
        ),
    );
}

/// Doctor check for the summarizer-size knob relation (issue #52). `register` and `archive` reject
/// `max_input_bytes < chunk_threshold_bytes` at the flag, but a hand-edited `config.json` can still
/// hold the inverted relation, where it is invisible: sessions between the two values floor to
/// placeholder summaries under `summary-input-limit` and park, looking like genuine capacity
/// failures rather than a misconfiguration. This names the relation so the fix is a re-register,
/// not a chase.
fn push_input_cap_relation_check(checks: &mut Vec<CheckResult>, limits: Option<&RawLimits>) {
    let Some(max_input_bytes) = limits.and_then(|limits| limits.max_input_bytes) else {
        return;
    };
    let chunk_threshold_bytes = limits
        .and_then(|limits| limits.chunk_threshold_bytes)
        .unwrap_or(munshi::DEFAULT_CHUNK_THRESHOLD_BYTES);
    if max_input_bytes >= chunk_threshold_bytes {
        return;
    }
    push_check(
        checks,
        "input-cap-relation",
        CheckStatus::Warning,
        format!(
            "max_input_bytes ({max_input_bytes}) is below chunk_threshold_bytes \
             ({chunk_threshold_bytes}): requests between the two floor to placeholder summaries \
             under `summary-input-limit` instead of chunking; re-register with \
             `--max-input-bytes` at or above `--chunk-threshold-bytes`, then \
             `munshi retry-all --force`"
        ),
    );
}

fn summarize_sessions(records: &[SessionRecord]) -> SessionStateSummary {
    let mut summary = SessionStateSummary {
        total: records.len(),
        ..SessionStateSummary::default()
    };
    for record in records {
        if record
            .current_summary
            .as_ref()
            .is_some_and(|current| current.is_placeholder())
        {
            summary.placeholder += 1;
        }
        match operational_state(record) {
            "archived" => summary.archived += 1,
            "revision-pending" => summary.revision_pending += 1,
            "summary-pending" => summary.summary_pending += 1,
            "interrupted" => summary.interrupted += 1,
            "failed" => {
                summary.failed += 1;
                if record.next_retry_at_ms.is_some_and(|next| next < 0) {
                    summary.parked += 1;
                }
            }
            "delivery-related" => summary.delivery_related += 1,
            "disabled-project" => summary.disabled_project += 1,
            "processing" => summary.processing += 1,
            "observed" => summary.observed += 1,
            "not-archive-worthy" => summary.not_archive_worthy += 1,
            _ => summary.unknown += 1,
        }
    }
    summary
}

fn operational_state(record: &SessionRecord) -> &'static str {
    let disabled_project = record.last_error_category.as_deref().is_some_and(|code| {
        matches!(
            code,
            "project-disabled" | "project-override-disabled" | "project-override-invalid"
        )
    });
    if disabled_project && record.lifecycle_state != "archived" {
        return "disabled-project";
    }
    match record.lifecycle_state.as_str() {
        "archived" => "archived",
        "revision-pending" => "revision-pending",
        "summary-pending" => "summary-pending",
        "interrupted" => "interrupted",
        "failed" => {
            if record
                .last_error_category
                .as_deref()
                .is_some_and(|code| code.starts_with("delivery-"))
            {
                "delivery-related"
            } else {
                "failed"
            }
        }
        "processing" => "processing",
        // A recorded verdict on unarchived content: either the hook path (a session-end was
        // ingested before the worker judged it) or the sweep path (the worker stamped
        // `not_archive_worthy_at_ms` while settling the row, issue #50). The stored
        // lifecycle stays `observed` so the row remains reactivatable when the transcript
        // grows; only this label moves.
        "observed"
            if record.current_revision == 0
                && (record.last_session_end_ms.is_some()
                    || record.not_archive_worthy_at_ms.is_some()) =>
        {
            "not-archive-worthy"
        }
        "observed" => "observed",
        _ => "unknown",
    }
}

fn is_retryable_lifecycle(state: &str) -> bool {
    matches!(
        state,
        "summary-pending" | "revision-pending" | "interrupted" | "failed" | "processing"
    )
}

fn retry_fields_from_hook(hook: HookResult) -> (String, Option<String>, Option<String>) {
    match hook {
        HookResult::Archived { relative_path } => {
            ("archived".to_owned(), None, Some(relative_path))
        }
        HookResult::NotArchiveWorthy => ("not-archive-worthy".to_owned(), None, None),
        HookResult::Failed { code } if code == "work-not-claimable" => {
            ("not-eligible".to_owned(), Some(code), None)
        }
        HookResult::Failed { code } => ("failed".to_owned(), Some(code), None),
    }
}

fn session_filter_name(filter: SessionStateFilter) -> &'static str {
    match filter {
        SessionStateFilter::Archived => "archived",
        SessionStateFilter::RevisionPending => "revision-pending",
        SessionStateFilter::SummaryPending => "summary-pending",
        SessionStateFilter::Interrupted => "interrupted",
        SessionStateFilter::Failed => "failed",
        SessionStateFilter::DeliveryRelated => "delivery-related",
        SessionStateFilter::DisabledProject => "disabled-project",
        SessionStateFilter::Processing => "processing",
        SessionStateFilter::Observed => "observed",
        SessionStateFilter::NotArchiveWorthy => "not-archive-worthy",
        SessionStateFilter::Unknown => "unknown",
    }
}

fn state_database_exists(state_directory: &Path) -> bool {
    StateStore::database_path(state_directory).exists()
}

fn load_sessions(state_directory: &Path) -> Result<Vec<SessionRecord>, Box<dyn Error>> {
    if !state_database_exists(state_directory) {
        return Ok(Vec::new());
    }
    let state = StateStore::open(state_directory)?;
    Ok(state.list_sessions()?)
}

/// The matching total and the bounded page of processing attempts, or an empty pair when the
/// state directory has never been registered — a read-only caller polling before `munshi
/// register` must get a valid empty contract, not an error (ADR 0007). Never opens the store in
/// that state, so the query itself cannot create the database it is reporting on.
fn load_attempts(
    state_directory: &Path,
    since_ms: Option<i64>,
    limit: usize,
) -> Result<(usize, Vec<AttemptRecord>), Box<dyn Error>> {
    if !state_database_exists(state_directory) {
        return Ok((0, Vec::new()));
    }
    let state = StateStore::open(state_directory)?;
    Ok((
        state.count_processing_attempts(since_ms)?,
        state.list_processing_attempts(since_ms, limit)?,
    ))
}

/// The recorded total and the bounded newest-first tail of diagnostics, degrading to an empty
/// pair on an unregistered state directory exactly as [`load_attempts`] does.
fn load_diagnostics(
    state_directory: &Path,
    limit: usize,
) -> Result<(usize, Vec<Diagnostic>), Box<dyn Error>> {
    if !state_database_exists(state_directory) {
        return Ok((0, Vec::new()));
    }
    let state = StateStore::open(state_directory)?;
    Ok((state.count_diagnostics()?, state.list_diagnostics(limit)?))
}

/// Projects the delivery record for one session (if any) into the `show` contract, deriving a
/// Notesmith deep link for a delivered note.
fn lookup_delivery_view(
    state_directory: &Path,
    source: SourceKind,
    session_id: &str,
) -> Option<DeliveryView> {
    if !state_database_exists(state_directory) {
        return None;
    }
    let state = StateStore::open(state_directory).ok()?;
    let deliveries = state.list_deliveries().ok()?;
    let record = deliveries
        .into_iter()
        .find(|delivery| delivery.source == source && delivery.session_id == session_id)?;
    let note_link = record.note_path.as_ref().map(|path| {
        format!(
            "notesmith://app/v/{}/{}",
            record.vault,
            path.trim_start_matches('/')
        )
    });
    Some(DeliveryView {
        state: record.delivery_state,
        note_path: record.note_path,
        note_link,
        delivered_revision: record.delivered_revision,
        history_commit: record.history_commit,
        attempts: record.attempts,
        last_error_code: record.last_error_category,
    })
}

/// Result of resolving a session ID that may exist under more than one source.
enum SessionTarget {
    NotFound,
    One(Box<SessionRecord>),
    Ambiguous(Vec<String>),
}

fn parse_source_selector(value: &str) -> Result<SourceKind, Box<dyn Error>> {
    SourceKind::parse_selector(value)
        .ok_or_else(|| -> Box<dyn Error> { format!("unsupported source: {value}").into() })
}

/// Resolve a session ID to a single record. An explicit source selector narrows the
/// match; without one, a session ID shared across sources is reported as ambiguous so a
/// retry can never silently target the wrong source's session.
fn resolve_session_target(
    state_directory: &Path,
    source: Option<SourceKind>,
    session_id: &str,
) -> Result<SessionTarget, Box<dyn Error>> {
    let mut matches: Vec<SessionRecord> = load_sessions(state_directory)?
        .into_iter()
        .filter(|record| record.session_id == session_id)
        .filter(|record| source.is_none_or(|wanted| record.source == wanted))
        .collect();
    match matches.len() {
        0 => Ok(SessionTarget::NotFound),
        1 => Ok(SessionTarget::One(Box::new(
            matches.pop().expect("one match"),
        ))),
        _ => {
            let mut sources: Vec<String> = matches
                .iter()
                .map(|record| record.source.as_selector().to_owned())
                .collect();
            sources.sort_unstable();
            sources.dedup();
            Ok(SessionTarget::Ambiguous(sources))
        }
    }
}

fn ambiguous_source_error(session_id: &str, sources: &[String]) -> Box<dyn Error> {
    format!(
        "session {session_id} exists under multiple sources ({}); pass --source to disambiguate",
        sources.join(", ")
    )
    .into()
}

fn read_last_failure_if_available(state_directory: &Path) -> Option<HookFailure> {
    if !state_database_exists(state_directory) {
        return None;
    }
    read_last_failure(state_directory).ok().flatten()
}

fn is_writable_directory(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_dir() && metadata.permissions().mode() & 0o200 != 0)
        .unwrap_or(false)
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn print_status_human(report: &StatusReport) {
    println!("state directory: {}", report.state_directory);
    println!(
        "configuration: {} (capture {}, summary-delivery {}, git-history {}, disabled-projects {}, runtime-compatible {})",
        report.configuration.status.as_str(),
        report.configuration.capture_state.as_str(),
        report.configuration.delivery_state.as_str(),
        report
            .configuration
            .archive_git_history
            .map(|value| if value { "enabled" } else { "disabled" })
            .unwrap_or("unknown"),
        report.configuration.disabled_projects,
        report.configuration.runtime_compatible
    );
    println!(
        "sessions total={} archived={} revision-pending={} summary-pending={} interrupted={} failed={} parked={} placeholder={} delivery-related={} disabled-project={} processing={} observed={} not-archive-worthy={} unknown={}",
        report.sessions.total,
        report.sessions.archived,
        report.sessions.revision_pending,
        report.sessions.summary_pending,
        report.sessions.interrupted,
        report.sessions.failed,
        report.sessions.parked,
        report.sessions.placeholder,
        report.sessions.delivery_related,
        report.sessions.disabled_project,
        report.sessions.processing,
        report.sessions.observed,
        report.sessions.not_archive_worthy,
        report.sessions.unknown
    );
    if let Some(failure) = &report.last_failure {
        println!(
            "last failure: operation={} code={} session={}",
            failure.operation,
            failure.code,
            failure.session_id.as_deref().unwrap_or("<none>")
        );
    }
}

fn print_sessions_human(report: &SessionsReport) {
    if report.items.is_empty() {
        println!("no sessions");
        return;
    }
    println!("sessions returned {} of {}", report.returned, report.total);
    for item in &report.items {
        println!(
            "{}  {}  rev={}{}",
            item.session_id,
            item.state,
            item.revision,
            item.last_error_code
                .as_deref()
                .map(|code| format!(" error={code}"))
                .unwrap_or_default()
        );
    }
}

/// One line per attempt, in the two-space-separated shape `sessions` uses: identity, then the
/// outcome, then only the bookkeeping the row actually carries.
fn print_attempts_human(report: &AttemptsReport) {
    if report.items.is_empty() {
        println!("no attempts");
        return;
    }
    println!("attempts returned {} of {}", report.returned, report.total);
    for item in &report.items {
        println!(
            "{}  {}  project={}  started-at-ms={}{}{}",
            item.session_id,
            item.outcome,
            item.project.as_deref().unwrap_or("<unknown>"),
            item.started_at_ms,
            item.finished_at_ms
                .map(|finished| format!("  finished-at-ms={finished}"))
                .unwrap_or_default(),
            item.error_category
                .as_deref()
                .map(|category| format!("  error={category}"))
                .unwrap_or_default(),
        );
    }
}

/// One line per diagnostic, matching the `operation=`/`code=`/`session=` shape `status` already
/// prints for the single most recent one.
fn print_diagnostics_human(report: &DiagnosticsReport) {
    if report.items.is_empty() {
        println!("no diagnostics");
        return;
    }
    println!(
        "diagnostics returned {} of {}",
        report.returned, report.total
    );
    for item in &report.items {
        println!(
            "{}  operation={} code={}{} session={}",
            item.recorded_at_ms,
            item.operation,
            item.category,
            item.cause_category
                .as_deref()
                .map(|cause| format!(" cause={cause}"))
                .unwrap_or_default(),
            item.session_id.as_deref().unwrap_or("<none>"),
        );
    }
}

fn print_show_human(report: &ShowReport) {
    let Some(session) = report.session.as_ref() else {
        println!("session not found");
        return;
    };
    println!("session: {}", session.session_id);
    println!(
        "state: {} (lifecycle {})",
        session.state, session.lifecycle_state
    );
    println!("revision: {}", session.revision);
    if let Some(completion) = session.completion_reason.as_deref() {
        println!("completion: {completion}");
    }
    if let Some(path) = session.archive_path.as_deref() {
        println!("archive: {path}");
    }
    if let Some(code) = session.last_error_code.as_deref() {
        let schedule = match session.next_retry_at_ms {
            Some(next) if next < 0 => " parked".to_owned(),
            Some(next) => format!(" next-retry-at-ms={next}"),
            None => String::new(),
        };
        println!(
            "last error: {code} failure-streak={}{schedule}",
            session.failure_streak
        );
    }
    if let Some(delivery) = session.delivery.as_ref() {
        println!(
            "summary delivery: {}{}{}",
            delivery.state,
            delivery
                .note_link
                .as_deref()
                .map(|link| format!(" {link}"))
                .unwrap_or_default(),
            delivery
                .last_error_code
                .as_deref()
                .map(|code| format!(" error={code}"))
                .unwrap_or_default(),
        );
    }
    if let Some(summary) = session.summary.as_ref() {
        println!("title: {}", summary.title);
        println!("goal: {}", summary.goal);
        if !summary.work_completed.is_empty() {
            println!("work completed:");
            for item in &summary.work_completed {
                println!("- {item}");
            }
        }
    }
}

fn print_retry_human(report: &RetryReport) {
    println!(
        "retry {} -> {}{}",
        report.session_id,
        report.result,
        report
            .code
            .as_deref()
            .map(|code| format!(" ({code})"))
            .unwrap_or_default()
    );
}

fn print_retry_all_human(report: &RetryAllReport) {
    println!(
        "retry-all attempted={} archived={} not-archive-worthy={} not-eligible={} failed={}",
        report.attempted,
        report.archived,
        report.not_archive_worthy,
        report.not_eligible,
        report.failed
    );
    for item in &report.items {
        println!(
            "{} -> {}{}",
            item.session_id,
            item.result,
            item.code
                .as_deref()
                .map(|code| format!(" ({code})"))
                .unwrap_or_default()
        );
    }
}

fn print_configuration_check_human(report: &ConfigurationCheckReport) {
    println!(
        "configuration status: {}",
        report.configuration.status.as_str()
    );
    println!(
        "capture: {}, summary-delivery: {}, git-history: {}, disabled-projects: {}, runtime-compatible: {}",
        report.configuration.capture_state.as_str(),
        report.configuration.delivery_state.as_str(),
        report
            .configuration
            .archive_git_history
            .map(|value| if value { "enabled" } else { "disabled" })
            .unwrap_or("unknown"),
        report.configuration.disabled_projects,
        report.configuration.runtime_compatible
    );
    for check in &report.configuration.checks {
        println!(
            "{} {} - {}",
            check.status.marker(),
            check.code,
            check.message
        );
    }
}

fn print_doctor_human(report: &DoctorReport) {
    println!("doctor status: {}", report.status.as_str());
    println!(
        "capture: {}, summary-delivery: {}, git-history: {}, disabled-projects: {}, runtime-compatible: {}",
        report.configuration.capture_state.as_str(),
        report.configuration.delivery_state.as_str(),
        report
            .configuration
            .archive_git_history
            .map(|value| if value { "enabled" } else { "disabled" })
            .unwrap_or("unknown"),
        report.configuration.disabled_projects,
        report.configuration.runtime_compatible
    );
    for check in &report.checks {
        println!(
            "{} {} - {}",
            check.status.marker(),
            check.code,
            check.message
        );
    }
    println!(
        "sessions total={} archived={} revision-pending={} summary-pending={} interrupted={} failed={} parked={} placeholder={} delivery-related={} disabled-project={} processing={} observed={} not-archive-worthy={} unknown={}",
        report.sessions.total,
        report.sessions.archived,
        report.sessions.revision_pending,
        report.sessions.summary_pending,
        report.sessions.interrupted,
        report.sessions.failed,
        report.sessions.parked,
        report.sessions.placeholder,
        report.sessions.delivery_related,
        report.sessions.disabled_project,
        report.sessions.processing,
        report.sessions.observed,
        report.sessions.not_archive_worthy,
        report.sessions.unknown
    );
    if let Some(failure) = &report.last_failure {
        println!(
            "last failure: operation={} code={} session={}",
            failure.operation,
            failure.code,
            failure.session_id.as_deref().unwrap_or("<none>")
        );
    }
}

#[derive(Debug, Serialize)]
struct RetrieveListingReport<'a> {
    schema_version: u32,
    command: &'static str,
    hash_matched: bool,
    total: usize,
    items: &'a [ArtifactMatch],
}

/// Emits a completed retrieval: the listing, the search results, the output file, or the raw
/// verified bytes to stdout. Domain errors carry their own distinguishable process exit code, and
/// no bytes are ever written on the error path (verification happens before this point).
fn emit_retrieve(
    result: Result<RetrieveResult, RetrieveError>,
    query: Option<String>,
    output: Option<PathBuf>,
    force: bool,
    json: bool,
) -> ExitCode {
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(error.exit_code());
        }
    };
    match result {
        RetrieveResult::Listing(matches) => {
            if json {
                emit_json(&RetrieveListingReport {
                    schema_version: 1,
                    command: "retrieve",
                    hash_matched: !matches.is_empty(),
                    total: matches.len(),
                    items: &matches,
                });
            } else {
                print_matches_human(&matches);
            }
            ExitCode::SUCCESS
        }
        RetrieveResult::Retrieved(content) => {
            if let Some(query) = query {
                let results = munshi::search_content(
                    &content.original_bytes,
                    &query,
                    munshi::QUERY_CONTEXT_LINES,
                );
                if json {
                    emit_json(&results);
                } else {
                    print_search_human(&results);
                }
                ExitCode::SUCCESS
            } else if let Some(path) = output {
                match munshi::write_retrieved_output(&path, &content.original_bytes, force) {
                    Ok(()) => {
                        eprintln!(
                            "wrote {} bytes to {}",
                            content.original_bytes.len(),
                            path.display()
                        );
                        ExitCode::SUCCESS
                    }
                    Err(error) => {
                        eprintln!("error: {error}");
                        ExitCode::from(error.exit_code())
                    }
                }
            } else {
                use std::io::Write;
                if let Err(error) = std::io::stdout().write_all(&content.original_bytes) {
                    eprintln!("error: could not write to stdout: {error}");
                    return ExitCode::FAILURE;
                }
                ExitCode::SUCCESS
            }
        }
    }
}

/// Emits a completed archive parse verification. A finished walk always prints its full report
/// (human or `--json`) before exiting with the report's own code, so findings never hide the
/// accounting; only a walk that could not run at all takes the error path.
fn emit_verify_archive_parse(
    result: Result<VerifyArchiveReport, VerifyArchiveError>,
    json: bool,
) -> ExitCode {
    match result {
        Ok(report) => {
            if json {
                emit_json(&report);
            } else {
                report.print_human();
            }
            ExitCode::from(report.exit_code())
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn print_matches_human(matches: &[ArtifactMatch]) {
    if matches.is_empty() {
        eprintln!("no matching artifacts");
        return;
    }
    println!("{} matching artifact(s), newest first:", matches.len());
    for artifact in matches {
        let media = artifact.media_type.as_deref().unwrap_or("-");
        println!(
            "  {}  snapshot={}  path={}  original={}B  stored={}B  {}  {}  {}",
            artifact.artifact_id,
            artifact.snapshot_id,
            artifact.logical_path,
            artifact.original_size_bytes,
            artifact.stored_size_bytes,
            artifact.compression,
            media,
            artifact.created_at,
        );
    }
}

fn print_search_human(results: &SearchResults) {
    if results.groups.is_empty() {
        eprintln!(
            "no lines matched \"{}\" ({} line(s) searched)",
            results.query, results.total_lines
        );
        return;
    }
    for (index, group) in results.groups.iter().enumerate() {
        if index > 0 {
            println!("--");
        }
        for line in group {
            let separator = if line.matched { ':' } else { '-' };
            println!("{}{}{}", line.line_number, separator, line.text);
        }
    }
    eprintln!(
        "{} matching line(s) across {} line(s)",
        results.match_count, results.total_lines
    );
}

fn emit_json(value: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("JSON serialization succeeds")
    );
}

/// The harness-neutral Munshi home (ADR 0008): the state directory holding `config.json`,
/// `munshi.db`, and `locks/`. Explicit flag, then `$MUNSHI_HOME`, then `~/.munshi`.
fn resolve_state_directory(value: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = value {
        return Ok(value);
    }
    if let Some(value) = std::env::var_os("MUNSHI_HOME") {
        return Ok(PathBuf::from(value));
    }
    let home = std::env::var_os("HOME").ok_or("MUNSHI_HOME or HOME is required")?;
    Ok(Path::new(&home).join(".munshi"))
}

/// The Claude Code configuration home: explicit flag, then `$CLAUDE_CONFIG_DIR`, then `~/.claude`.
fn resolve_claude_home(value: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = value {
        return Ok(value);
    }
    if let Some(value) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(value));
    }
    let home = std::env::var_os("HOME").ok_or("CLAUDE_CONFIG_DIR or HOME is required")?;
    Ok(Path::new(&home).join(".claude"))
}

fn resolve_copilot_home(value: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(value) = value {
        return Ok(value);
    }
    if let Some(value) = std::env::var_os("COPILOT_HOME") {
        return Ok(PathBuf::from(value));
    }
    let home = std::env::var_os("HOME").ok_or("COPILOT_HOME or HOME is required")?;
    Ok(Path::new(&home).join(".copilot"))
}
