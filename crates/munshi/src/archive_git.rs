use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use munshi_runner::{RunnerConfig, RunnerError, run_bounded};
use thiserror::Error;

use crate::project::inspect_project;
use crate::state::{StateError, try_acquire_session_lock};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_STDOUT_LIMIT: usize = 16 * 1024;
const GIT_STDERR_LIMIT: usize = 16 * 1024;
const ARCHIVE_REPOSITORY_LOCK: &str = "_archive-repository";
const ARCHIVE_REPOSITORY_LOCK_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum ArchiveGitError {
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error("archive Git state lock failed")]
    StateLock,
    #[error("archive Git I/O failed")]
    Io(#[from] io::Error),
    #[error("archive output directory is invalid")]
    InvalidOutputDirectory,
    #[error("archive Git repository is not rooted at the configured output directory")]
    RepositoryNotDedicated,
    #[error("archive Git history cannot target the origin project repository")]
    SourceRepositoryForbidden,
    #[error("archive file path is invalid for Git history")]
    InvalidArchivePath,
    #[error("archive Git repository is busy")]
    LockBusy,
}

pub fn ensure_archive_repository(output_directory: &Path) -> Result<PathBuf, ArchiveGitError> {
    fs::create_dir_all(output_directory).map_err(|_| ArchiveGitError::InvalidOutputDirectory)?;
    let output_directory = output_directory
        .canonicalize()
        .map_err(|_| ArchiveGitError::InvalidOutputDirectory)?;
    match repository_toplevel(&output_directory)? {
        Some(top_level) if top_level == output_directory => {}
        Some(_) if output_directory.join(".git").exists() => {
            return Err(ArchiveGitError::RepositoryNotDedicated);
        }
        _ => initialize_archive_repository(&output_directory)?,
    }
    let Some(top_level) = repository_toplevel(&output_directory)? else {
        return Err(ArchiveGitError::RepositoryNotDedicated);
    };
    if top_level != output_directory || repository_is_bare(&output_directory)? {
        return Err(ArchiveGitError::RepositoryNotDedicated);
    }
    Ok(output_directory)
}

pub fn commit_archive_revision(
    state_directory: &Path,
    output_directory: &Path,
    markdown_relative_path: &Path,
    source_project_identity: Option<&str>,
    session_id: &str,
    summary_revision: u64,
) -> Result<(), ArchiveGitError> {
    validate_archive_relative_path(markdown_relative_path)?;
    let lock_deadline = Instant::now() + ARCHIVE_REPOSITORY_LOCK_WAIT;
    let _repository_lock = loop {
        if let Some(lock) = try_acquire_session_lock(state_directory, ARCHIVE_REPOSITORY_LOCK)
            .map_err(map_state_lock_error)?
        {
            break lock;
        }
        if Instant::now() >= lock_deadline {
            return Err(ArchiveGitError::LockBusy);
        }
        thread::sleep(Duration::from_millis(10));
    };

    let repository_root = ensure_archive_repository(output_directory)?;
    if let Some(source_project_identity) = source_project_identity {
        let archive_project = inspect_project(&repository_root)
            .map_err(|_| ArchiveGitError::RepositoryNotDedicated)?;
        if archive_project.identity == source_project_identity {
            return Err(ArchiveGitError::SourceRepositoryForbidden);
        }
    }

    let archive_path = repository_root.join(markdown_relative_path);
    let canonical_archive = archive_path
        .canonicalize()
        .map_err(|_| ArchiveGitError::InvalidArchivePath)?;
    if !canonical_archive.starts_with(&repository_root) {
        return Err(ArchiveGitError::InvalidArchivePath);
    }

    let commit_subject = format!("archive: copilot:{session_id} revision {summary_revision}");
    let commit_body = format!("session_id: {session_id}\nsummary_revision: {summary_revision}\n");
    run_git(
        &repository_root,
        vec![
            OsString::from("add"),
            OsString::from("--"),
            markdown_relative_path.as_os_str().to_owned(),
        ],
        Vec::new(),
    )?;
    let commit_result = run_git(
        &repository_root,
        vec![
            OsString::from("-c"),
            OsString::from("user.name=Munshi"),
            OsString::from("-c"),
            OsString::from("user.email=munshi@localhost"),
            OsString::from("commit"),
            OsString::from("--quiet"),
            OsString::from("-m"),
            OsString::from(commit_subject),
            OsString::from("-m"),
            OsString::from(commit_body),
            OsString::from("--"),
            markdown_relative_path.as_os_str().to_owned(),
        ],
        Vec::new(),
    );
    if let Err(error) = commit_result {
        let _ = clear_staged_path(&repository_root, markdown_relative_path);
        return Err(error.into());
    }
    Ok(())
}

fn map_state_lock_error(error: StateError) -> ArchiveGitError {
    match error {
        StateError::Io(error) => ArchiveGitError::Io(error),
        StateError::LockBusy => ArchiveGitError::LockBusy,
        _ => ArchiveGitError::StateLock,
    }
}

fn validate_archive_relative_path(path: &Path) -> Result<(), ArchiveGitError> {
    if path.is_absolute()
        || path.extension().and_then(OsStr::to_str) != Some("md")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        Err(ArchiveGitError::InvalidArchivePath)
    } else {
        Ok(())
    }
}

fn initialize_archive_repository(directory: &Path) -> Result<(), ArchiveGitError> {
    match run_git(
        directory,
        vec![
            OsString::from("init"),
            OsString::from("-q"),
            OsString::from("-b"),
            OsString::from("main"),
        ],
        Vec::new(),
    ) {
        Ok(_) => Ok(()),
        Err(RunnerError::NonZeroExit { .. }) => {
            run_git(
                directory,
                vec![OsString::from("init"), OsString::from("-q")],
                Vec::new(),
            )?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn repository_toplevel(directory: &Path) -> Result<Option<PathBuf>, ArchiveGitError> {
    match run_git(
        directory,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ],
        Vec::new(),
    ) {
        Ok(bytes) => {
            let value =
                String::from_utf8(bytes).map_err(|_| ArchiveGitError::RepositoryNotDedicated)?;
            let path = PathBuf::from(value.trim());
            let canonical = path
                .canonicalize()
                .map_err(|_| ArchiveGitError::RepositoryNotDedicated)?;
            Ok(Some(canonical))
        }
        Err(RunnerError::NonZeroExit { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn repository_is_bare(directory: &Path) -> Result<bool, ArchiveGitError> {
    let bytes = run_git(
        directory,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--is-bare-repository"),
        ],
        Vec::new(),
    )?;
    let value = String::from_utf8(bytes).map_err(|_| ArchiveGitError::RepositoryNotDedicated)?;
    Ok(value.trim() == "true")
}

fn clear_staged_path(directory: &Path, relative_path: &Path) -> Result<(), ArchiveGitError> {
    run_git(
        directory,
        vec![
            OsString::from("reset"),
            OsString::from("--quiet"),
            OsString::from("HEAD"),
            OsString::from("--"),
            relative_path.as_os_str().to_owned(),
        ],
        Vec::new(),
    )?;
    Ok(())
}

fn run_git(directory: &Path, args: Vec<OsString>, input: Vec<u8>) -> Result<Vec<u8>, RunnerError> {
    let mut full_args = Vec::with_capacity(args.len() + 2);
    full_args.push(OsString::from("-C"));
    full_args.push(directory.as_os_str().to_owned());
    full_args.extend(args);
    run_bounded(
        &RunnerConfig {
            binary: PathBuf::from("git"),
            args: full_args,
            timeout: GIT_TIMEOUT,
            stdout_limit: GIT_STDOUT_LIMIT,
            stderr_limit: GIT_STDERR_LIMIT,
        },
        input,
    )
}
