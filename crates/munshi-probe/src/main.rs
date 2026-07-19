use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use munshi_probe::capture::{CaptureMode, capture_hook, capture_hook_in_directory};
use munshi_probe::inspect::inspect_transcript;
use munshi_probe::summary::{SummaryProbeConfig, run_summary_probe};

#[derive(Debug, Parser)]
#[command(about = "Phase 0 compatibility probes for Munshi", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate and atomically capture one hook JSON payload from stdin.
    CaptureHook {
        #[arg(
            long,
            required_unless_present = "output_dir",
            conflicts_with = "output_dir"
        )]
        output: Option<PathBuf>,
        /// Capture into this directory as `<hook_event_name>-<unix-ms>-<pid>.json`.
        #[arg(long = "output-dir")]
        output_dir: Option<PathBuf>,
        #[arg(long)]
        sanitize: bool,
        #[arg(long = "preserve-value", requires = "sanitize")]
        preserved_values: Vec<String>,
        #[arg(long, requires = "sanitize")]
        replacement: Option<String>,
    },
    /// Inspect a JSONL transcript without printing transcript content.
    InspectTranscript {
        #[arg(long)]
        input: PathBuf,
        #[arg(long = "discriminator-key")]
        discriminator_keys: Vec<String>,
    },
    /// Invoke an explicitly selected executable and validate its Phase 0 JSON result.
    Summarize {
        #[arg(long)]
        binary: PathBuf,
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<OsString>,
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 1_048_576)]
        max_stdout_bytes: usize,
        #[arg(long, default_value_t = 65_536)]
        max_stderr_bytes: usize,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    match Cli::parse().command {
        Command::CaptureHook {
            output,
            output_dir,
            sanitize,
            preserved_values,
            replacement,
        } => {
            let mode = if sanitize {
                CaptureMode::Sanitized {
                    replacement: replacement.unwrap_or_else(|| "<redacted>".to_owned()),
                    preserved_values: preserved_values.into_iter().collect(),
                }
            } else {
                CaptureMode::Raw
            };
            let report = match (output, output_dir) {
                (Some(output), None) => capture_hook(io::stdin().lock(), &output, mode)?,
                (None, Some(directory)) => {
                    capture_hook_in_directory(io::stdin().lock(), &directory, mode)?
                }
                _ => unreachable!("clap enforces exactly one destination"),
            };
            print_json(&report)?;
        }
        Command::InspectTranscript {
            input,
            discriminator_keys,
        } => {
            let report = inspect_transcript(
                &input,
                &discriminator_keys.into_iter().collect::<BTreeSet<_>>(),
            )?;
            print_json(&report)?;
        }
        Command::Summarize {
            binary,
            args,
            input,
            timeout_ms,
            max_stdout_bytes,
            max_stderr_bytes,
        } => {
            let mut bytes = Vec::new();
            match input {
                Some(path) => File::open(path)?.read_to_end(&mut bytes)?,
                None => io::stdin().lock().read_to_end(&mut bytes)?,
            };
            let summary = run_summary_probe(
                &SummaryProbeConfig {
                    binary,
                    args,
                    timeout: Duration::from_millis(timeout_ms),
                    stdout_limit: max_stdout_bytes,
                    stderr_limit: max_stderr_bytes,
                },
                bytes,
            )?;
            print_json(&summary)?;
        }
    }
    Ok(())
}

fn print_json(value: &impl serde::Serialize) -> Result<(), serde_json::Error> {
    serde_json::to_writer_pretty(io::stdout().lock(), value)?;
    println!();
    Ok(())
}
