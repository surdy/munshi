use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use munshi::{
    ArchiveConfig, ArchiveOutcome, DeliveryCredentialSource, DeliveryRunReport, DeliverySinkConfig,
    DeliveryStatusReport, HistoryReport, HookEvent, HookFailure, HookResult, ProjectStatus,
    RegisterConfig, SessionRecord, SessionReference, SourceKind, StateStore, StructuredSummary,
    accept_disclosure_from_terminal, archive_session, configure_delivery, delivery_backfill,
    delivery_retry, delivery_status, delivery_verify_history, handle_hook, parse_archive_markdown,
    project_status, read_last_failure, register, run_archive_worker_for_source, run_recovery,
    set_delivery_enabled, set_project_enabled, unregister, wait_for_hook_result,
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
        #[arg(long, default_value_t = 300_000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 8_388_608)]
        max_source_bytes: usize,
        #[arg(long, default_value_t = 1_048_576)]
        max_input_bytes: usize,
        #[arg(long, default_value_t = 262_144)]
        max_stdout_bytes: usize,
        #[arg(long, default_value_t = 65_536)]
        max_stderr_bytes: usize,
    },
    /// Disclose transcript processing, save configuration, and install user hooks.
    Register {
        /// Explicitly accept the displayed v1 transcript-processing disclosure.
        #[arg(long, visible_alias = "accept-disclosure")]
        accept_transcript_processing: bool,
        /// Print the intended managed paths without writing files.
        #[arg(long)]
        dry_run: bool,
        /// Copilot home whose hooks directory should contain Munshi's dedicated file.
        #[arg(long)]
        copilot_home: Option<PathBuf>,
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
        #[arg(long, default_value_t = 300_000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 8_388_608)]
        max_source_bytes: usize,
        #[arg(long, default_value_t = 1_048_576)]
        max_input_bytes: usize,
        #[arg(long, default_value_t = 262_144)]
        max_stdout_bytes: usize,
        #[arg(long, default_value_t = 65_536)]
        max_stderr_bytes: usize,
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
        #[arg(long)]
        copilot_home: Option<PathBuf>,
    },
    /// Enable, disable, or inspect future processing and delivery for one project.
    #[command(subcommand)]
    Project(ProjectCommand),
    /// Configure and operate opt-in Notesmith delivery of current summaries.
    #[command(subcommand)]
    Delivery(DeliveryCommand),
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
        copilot_home: Option<PathBuf>,
    },
    /// Resume future processing and delivery for a previously disabled project.
    Enable {
        /// Project directory whose canonical identity should be re-enabled.
        project_dir: PathBuf,
        #[arg(long)]
        copilot_home: Option<PathBuf>,
    },
    /// Print the effective enabled state and budgets for a project.
    Status {
        /// Project directory to inspect.
        project_dir: PathBuf,
        #[arg(long)]
        copilot_home: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum DeliveryCommand {
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
        copilot_home: Option<PathBuf>,
    },
    /// Enable delivery. Reports the pending backfill count; existing summaries need confirmation.
    Enable {
        #[arg(long)]
        copilot_home: Option<PathBuf>,
    },
    /// Disable delivery. Future delivery stops while delivery history is retained.
    Disable {
        #[arg(long)]
        copilot_home: Option<PathBuf>,
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
enum HookCommand {
    AgentStop {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    SessionEnd {
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    Wait {
        #[arg(long)]
        state_dir: PathBuf,
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
        hook_path: PathBuf,
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
    Status {
        report: Box<StatusReport>,
        json: bool,
    },
    Sessions {
        report: Box<SessionsReport>,
        json: bool,
    },
    Show {
        report: Box<ShowReport>,
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
    remote_delivery: Option<bool>,
    #[serde(default)]
    delivery: Option<RawDelivery>,
    policy: Option<RawPolicy>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawDelivery {
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
        Ok(Outcome::Registered { hook_path }) => {
            println!("registered Munshi hooks at {}", hook_path.display());
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
                "delivery enabled (endpoint {}, vault {})",
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
            println!("delivery disabled; existing delivery history is retained");
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
            timeout_ms,
            max_source_bytes,
            max_input_bytes,
            max_stdout_bytes,
            max_stderr_bytes,
        } => {
            let source = SourceKind::parse_selector(&source)
                .ok_or_else(|| format!("unsupported source: {source}"))?;
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
                timeout: Duration::from_millis(timeout_ms),
                max_source_bytes,
                max_input_bytes,
                max_stdout_bytes,
                max_stderr_bytes,
            })?))
        }
        Command::Register {
            accept_transcript_processing,
            dry_run,
            copilot_home,
            output_dir,
            archive_git_history,
            summarizer,
            summarizer_args,
            timeout_ms,
            max_source_bytes,
            max_input_bytes,
            max_stdout_bytes,
            max_stderr_bytes,
            max_calls_per_hour,
            max_calls_per_day,
            max_concurrency,
        } => {
            eprintln!(
                "Configured local output directory: {}",
                output_dir.display()
            );
            accept_disclosure_from_terminal(accept_transcript_processing)?;
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let state_directory = copilot_home.join("munshi");
            let executable = std::env::current_exe()?.canonicalize()?;
            if dry_run {
                println!(
                    "would write {} and {}",
                    copilot_home.join("hooks/munshi.json").display(),
                    state_directory.join("config.json").display()
                );
                return Ok(Outcome::DryRun);
            }
            register(&RegisterConfig {
                copilot_home: copilot_home.clone(),
                state_directory,
                output_directory: output_dir,
                archive_git_history,
                summarizer_binary: summarizer,
                summarizer_args,
                timeout: Duration::from_millis(timeout_ms),
                max_source_bytes,
                max_input_bytes,
                max_stdout_bytes,
                max_stderr_bytes,
                max_calls_per_hour,
                max_calls_per_day,
                max_concurrency,
                executable,
            })?;
            Ok(Outcome::Registered {
                hook_path: copilot_home.join("hooks/munshi.json"),
            })
        }
        Command::Unregister { copilot_home } => {
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let state_directory = copilot_home.join("munshi");
            unregister(&copilot_home, &state_directory)?;
            Ok(Outcome::Unregistered)
        }
        Command::Project(ProjectCommand::Disable {
            project_dir,
            copilot_home,
        }) => {
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let state_directory = copilot_home.join("munshi");
            Ok(Outcome::Project(set_project_enabled(
                &copilot_home,
                &state_directory,
                &project_dir,
                false,
            )?))
        }
        Command::Project(ProjectCommand::Enable {
            project_dir,
            copilot_home,
        }) => {
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let state_directory = copilot_home.join("munshi");
            Ok(Outcome::Project(set_project_enabled(
                &copilot_home,
                &state_directory,
                &project_dir,
                true,
            )?))
        }
        Command::Project(ProjectCommand::Status {
            project_dir,
            copilot_home,
        }) => {
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let state_directory = copilot_home.join("munshi");
            Ok(Outcome::Project(project_status(
                &state_directory,
                &project_dir,
            )?))
        }
        Command::Delivery(command) => run_delivery(command),
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
        Command::Hook(HookCommand::AgentStop { state_dir }) => {
            if let Ok(state_dir) = resolve_state_directory(state_dir) {
                handle_hook(HookEvent::AgentStop, &state_dir, std::io::stdin().lock());
            }
            Ok(Outcome::Hook)
        }
        Command::Hook(HookCommand::SessionEnd { state_dir }) => {
            if let Ok(state_dir) = resolve_state_directory(state_dir) {
                handle_hook(HookEvent::SessionEnd, &state_dir, std::io::stdin().lock());
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
            session_id,
            timeout_ms,
        }) => Ok(Outcome::Wait(wait_for_hook_result(
            &state_dir,
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

fn run_delivery(command: DeliveryCommand) -> Result<Outcome, Box<dyn Error>> {
    match command {
        DeliveryCommand::Configure {
            endpoint,
            vault,
            folder,
            credential_env,
            credential_keychain,
            max_attempts,
            provision_history,
            no_provision_history,
            copilot_home,
        } => {
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let state_directory = copilot_home.join("munshi");
            let credential = resolve_credential_source(credential_env, credential_keychain)?;
            let provision = if provision_history {
                Some(true)
            } else if no_provision_history {
                Some(false)
            } else {
                None
            };
            let settings = configure_delivery(
                &copilot_home,
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
        DeliveryCommand::Enable { copilot_home } => {
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let state_directory = copilot_home.join("munshi");
            let settings = set_delivery_enabled(&copilot_home, &state_directory, true)?;
            // Report the pending backfill as a dry run so existing summaries need confirmation.
            let backfill = delivery_backfill(&state_directory, false, usize::MAX)
                .ok()
                .map(Box::new);
            Ok(Outcome::DeliveryEnabled {
                settings: Box::new(settings),
                backfill,
            })
        }
        DeliveryCommand::Disable { copilot_home } => {
            let copilot_home = resolve_copilot_home(copilot_home)?;
            let state_directory = copilot_home.join("munshi");
            let settings = set_delivery_enabled(&copilot_home, &state_directory, false)?;
            Ok(Outcome::DeliveryDisabled {
                settings: Box::new(settings),
            })
        }
        DeliveryCommand::History {
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
        DeliveryCommand::Status { state_dir, json } => {
            let state_directory = resolve_state_directory(state_dir)?;
            Ok(Outcome::DeliveryStatus {
                report: Box::new(delivery_status(&state_directory)?),
                json,
            })
        }
        DeliveryCommand::Backfill {
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
        DeliveryCommand::Retry {
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
    let hook_path = state_directory
        .parent()
        .map(|parent| parent.join("hooks/munshi.json"))
        .unwrap_or_else(|| PathBuf::from("hooks/munshi.json"));

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
                    if config.version == Some(1) {
                        push_check(
                            &mut checks,
                            "config-version",
                            CheckStatus::Ok,
                            "version 1".to_owned(),
                        );
                    } else {
                        push_check(
                            &mut checks,
                            "config-version",
                            CheckStatus::Warning,
                            format!("unsupported version {:?}; expected 1", config.version),
                        );
                    }

                    summarizer_executable =
                        config.summarizer.and_then(|command| command.executable);
                    output_directory = config.output_directory;
                    archive_git_history = config.archive_git_history;
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
                    delivery_state = match config.remote_delivery {
                        Some(false) => DeliveryState::Disabled,
                        Some(true) => {
                            let addressable = config
                                .delivery
                                .as_ref()
                                .is_some_and(RawDelivery::is_addressable);
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
                            "delivery disabled".to_owned(),
                        ),
                        DeliveryState::Enabled => push_check(
                            &mut checks,
                            "delivery-state",
                            CheckStatus::Ok,
                            "delivery enabled with an addressable Notesmith sink".to_owned(),
                        ),
                        DeliveryState::DeliveryRelated => push_check(
                            &mut checks,
                            "delivery-state",
                            CheckStatus::Warning,
                            "delivery enabled but the Notesmith sink is not addressable".to_owned(),
                        ),
                        DeliveryState::Unknown => push_check(
                            &mut checks,
                            "delivery-state",
                            CheckStatus::Warning,
                            "delivery state is unknown".to_owned(),
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
                    // history. Doctor reports this statically; `munshi delivery history` probes the
                    // live capability.
                    let versioned =
                        archive_git_history == Some(true) && config.remote_delivery == Some(true);
                    let provision = config
                        .delivery
                        .as_ref()
                        .and_then(|delivery| delivery.provision_history)
                        .unwrap_or(false);
                    versioned_delivery = Some(versioned);
                    provision_remote_history = Some(provision);
                    if versioned {
                        let hint = if provision {
                            "versioned delivery: Munshi will configure the Notesmith vault's revision history; verify with `munshi delivery history --configure`"
                        } else {
                            "versioned delivery requires remote revision history; verify with `munshi delivery history` (add `--configure` to enable it)"
                        };
                        push_check(
                            &mut checks,
                            "delivery-remote-history",
                            CheckStatus::Warning,
                            hint.to_owned(),
                        );
                    } else if config.remote_delivery == Some(true) {
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

                    config_recognized = config.version == Some(1)
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

    let hook_recognized = if !hook_path.exists() {
        push_check(
            &mut checks,
            "hook-file",
            CheckStatus::Error,
            format!("missing {}", hook_path.display()),
        );
        false
    } else {
        match fs::read(&hook_path) {
            Ok(bytes) => match serde_json::from_slice::<RawHookFile>(&bytes) {
                Ok(hook) => {
                    if hook_is_recognized(&hook) {
                        push_check(
                            &mut checks,
                            "hook-contract",
                            CheckStatus::Ok,
                            "hook file matches the 1.0.70 managed contract".to_owned(),
                        );
                        true
                    } else {
                        push_check(
                            &mut checks,
                            "hook-contract",
                            CheckStatus::Error,
                            "hook file does not match the managed contract".to_owned(),
                        );
                        false
                    }
                }
                Err(error) => {
                    push_check(
                        &mut checks,
                        "hook-parse",
                        CheckStatus::Error,
                        format!("invalid JSON at {}: {error}", hook_path.display()),
                    );
                    false
                }
            },
            Err(error) => {
                push_check(
                    &mut checks,
                    "hook-read",
                    CheckStatus::Error,
                    format!("failed to read {}: {error}", hook_path.display()),
                );
                false
            }
        }
    };

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
        hook_path: hook_path.display().to_string(),
        summarizer_executable,
        output_directory,
        checks,
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
    }
}

fn session_record_to_item(record: &SessionRecord) -> SessionListItem {
    build_session_item(record)
}

fn summarize_sessions(records: &[SessionRecord]) -> SessionStateSummary {
    let mut summary = SessionStateSummary {
        total: records.len(),
        ..SessionStateSummary::default()
    };
    for record in records {
        match operational_state(record) {
            "archived" => summary.archived += 1,
            "revision-pending" => summary.revision_pending += 1,
            "summary-pending" => summary.summary_pending += 1,
            "interrupted" => summary.interrupted += 1,
            "failed" => summary.failed += 1,
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
        "observed" if record.current_revision == 0 && record.last_session_end_ms.is_some() => {
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
        "configuration: {} (capture {}, delivery {}, git-history {}, disabled-projects {}, runtime-compatible {})",
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
        "sessions total={} archived={} revision-pending={} summary-pending={} interrupted={} failed={} delivery-related={} disabled-project={} processing={} observed={} not-archive-worthy={} unknown={}",
        report.sessions.total,
        report.sessions.archived,
        report.sessions.revision_pending,
        report.sessions.summary_pending,
        report.sessions.interrupted,
        report.sessions.failed,
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
    if let Some(delivery) = session.delivery.as_ref() {
        println!(
            "delivery: {}{}{}",
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
        "capture: {}, delivery: {}, git-history: {}, disabled-projects: {}, runtime-compatible: {}",
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
        "capture: {}, delivery: {}, git-history: {}, disabled-projects: {}, runtime-compatible: {}",
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
        "sessions total={} archived={} revision-pending={} summary-pending={} interrupted={} failed={} delivery-related={} disabled-project={} processing={} observed={} not-archive-worthy={} unknown={}",
        report.sessions.total,
        report.sessions.archived,
        report.sessions.revision_pending,
        report.sessions.summary_pending,
        report.sessions.interrupted,
        report.sessions.failed,
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

fn emit_json(value: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("JSON serialization succeeds")
    );
}

fn resolve_state_directory(value: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
    match value {
        Some(value) => Ok(value),
        None => Ok(resolve_copilot_home(None)?.join("munshi")),
    }
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
