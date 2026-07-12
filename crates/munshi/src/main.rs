use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use munshi::{ArchiveConfig, ArchiveOutcome, SessionReference, archive_session};

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
}

fn main() -> ExitCode {
    match run() {
        Ok(ArchiveOutcome::Archived { id, relative_path }) => {
            println!("archived {id} -> {}", relative_path.display());
            ExitCode::SUCCESS
        }
        Ok(ArchiveOutcome::NotArchiveWorthy { id }) => {
            eprintln!("not archived: {id} is not archive-worthy");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ArchiveOutcome, munshi::ArchiveError> {
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
        } => archive_session(&ArchiveConfig {
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
        }),
    }
}
