use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use munshi::{
    ArchiveConfig, ArchiveOutcome, HookEvent, HookResult, ProjectStatus, RegisterConfig,
    SessionReference, accept_disclosure_from_terminal, archive_session, handle_hook,
    project_status, register, run_archive_worker, run_recovery, set_project_enabled, unregister,
    wait_for_hook_result,
};

#[derive(Debug, Parser)]
#[command(about = "Archive coding-agent sessions as durable Markdown", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manually summarize and archive one Copilot CLI session.
    #[command(visible_alias = "summarize")]
    Archive {
        /// Copilot's stable source session ID.
        session_id: Option<String>,
        /// Explicit transcript path. It must be a regular events.jsonl file.
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
    #[command(hide = true, subcommand)]
    Hook(HookCommand),
    #[command(hide = true)]
    HookWorker {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        session_id: String,
    },
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
    Registered { hook_path: PathBuf },
    Unregistered,
    DryRun,
    Hook,
    Worker,
    Wait(HookResult),
    Project(ProjectStatus),
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
        } => Ok(Outcome::Archive(archive_session(&ArchiveConfig {
            reference: SessionReference {
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
        })?)),
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
            session_id,
        } => {
            let _ = run_archive_worker(&state_dir, &session_id)?;
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
