//! A local, read-only dashboard for Munshi's session-archiving backlog.
//!
//! The binary serves two routes on a loopback socket — an embedded single-file page and a JSON
//! snapshot refreshed at most every 30 seconds — and reads nothing from the Munshi state directory
//! itself: every figure comes from a `munshi ... --json` invocation (ADR 0007). It runs in the
//! foreground until interrupted; there is no daemon, no state of its own, and nothing to clean up.
//!
//! It replaces the Python dashboard spike and reproduces that spike's `/api/data` payload exactly,
//! so the page it embeds is the spike's page unmodified.

mod collect;
mod db;
mod server;

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use thiserror::Error;

use crate::collect::Collector;

#[derive(Debug, Parser)]
#[command(
    about = "Serve a local read-only dashboard for Munshi's archiving backlog",
    version
)]
struct Cli {
    /// Address to listen on. Only loopback addresses are accepted: the dashboard publishes session
    /// metadata without authentication.
    #[arg(long, default_value = "127.0.0.1:8877")]
    bind: SocketAddr,
    /// The `munshi` executable every figure is read from. A bare name is resolved against `PATH`.
    #[arg(long, default_value = "munshi")]
    munshi: PathBuf,
}

#[derive(Debug, Error)]
enum DashboardError {
    #[error(
        "{0} is not a loopback address; the dashboard serves unauthenticated session data and \
         binds loopback addresses only"
    )]
    NonLoopbackBind(SocketAddr),
    #[error("could not bind {0}: {1}")]
    Bind(SocketAddr, std::io::Error),
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Binds the listener and serves until the process is signalled; it returns only if the accept loop
/// ever ends, which a listening socket does not do on its own.
fn run(cli: Cli) -> Result<(), DashboardError> {
    let bind = server::loopback_bind(cli.bind)?;
    let listener = TcpListener::bind(bind).map_err(|error| DashboardError::Bind(bind, error))?;
    let collector = Arc::new(Collector::new(cli.munshi));
    println!("munshi dashboard listening on http://{bind}");
    server::serve(&listener, &collector);
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_bind_loopback_and_resolve_munshi_from_path() {
        let cli = Cli::parse_from(["munshi-dashboard"]);
        assert_eq!(
            cli.bind,
            "127.0.0.1:8877".parse().expect("a socket address")
        );
        assert_eq!(cli.munshi, PathBuf::from("munshi"));
    }

    #[test]
    fn both_flags_are_accepted() {
        let cli = Cli::parse_from([
            "munshi-dashboard",
            "--bind",
            "127.0.0.1:9999",
            "--munshi",
            "/Users/example/.local/bin/munshi",
        ]);
        assert_eq!(
            cli.bind,
            "127.0.0.1:9999".parse().expect("a socket address")
        );
        assert_eq!(
            cli.munshi,
            PathBuf::from("/Users/example/.local/bin/munshi")
        );
    }

    #[test]
    fn a_routable_bind_fails_before_the_socket_is_opened() {
        let cli = Cli::parse_from(["munshi-dashboard", "--bind", "0.0.0.0:8877"]);
        let error = run(cli).expect_err("a routable bind is refused");
        assert!(
            matches!(error, DashboardError::NonLoopbackBind(_)),
            "{error}"
        );
    }
}
