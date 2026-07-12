use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tempfile::Builder;
use thiserror::Error;

const HOOK_FILE_NAME: &str = "munshi.json";
const CONFIG_FILE_NAME: &str = "config.json";

pub const DISCLOSURE: &str = "\
IMPORTANT: MUNSHI TRANSCRIPT PROCESSING DISCLOSURE

After registration, transcript summarization is ON by default for cleanly ended sessions.
Munshi v1 has NO secret redaction or granular transcript filtering.
Transcript content is sent to the summarizer executable configured below.
Summaries are written as local Markdown files.
Remote delivery remains DISABLED.
";

#[derive(Debug, Clone)]
pub struct RegisterConfig {
    pub copilot_home: PathBuf,
    pub state_directory: PathBuf,
    pub output_directory: PathBuf,
    pub summarizer_binary: PathBuf,
    pub summarizer_args: Vec<OsString>,
    pub timeout: Duration,
    pub max_source_bytes: usize,
    pub max_input_bytes: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub executable: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisclosureDecision {
    AcceptedByFlag,
    Prompt,
}

#[derive(Debug, Error)]
pub enum RegistrationError {
    #[error("registration disclosure was not accepted")]
    DisclosureDeclined,
    #[error("noninteractive registration requires --accept-disclosure")]
    NoninteractiveAcceptanceRequired,
    #[error("registration paths and executables must be absolute")]
    RelativePath,
    #[error("refusing an unsafe symlink, file type, or ownership at {0}")]
    UnsafePath(PathBuf),
    #[error("the existing Munshi-owned file is malformed or was not created by this version")]
    MalformedOwnedFile,
    #[error("configuration contains a non-UTF-8 argument or path")]
    NonUtf8Configuration,
    #[error("registration I/O failed")]
    Io(#[source] io::Error),
    #[error("registration JSON failed")]
    Json(#[source] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredConfig {
    pub version: u32,
    pub summarizer: StoredCommand,
    pub output_directory: String,
    pub state_directory: String,
    pub limits: StoredLimits,
    pub remote_delivery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredCommand {
    pub executable: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredLimits {
    pub timeout_ms: u64,
    pub max_source_bytes: usize,
    pub max_input_bytes: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookFile {
    version: u32,
    hooks: HookEvents,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookEvents {
    #[serde(rename = "agentStop")]
    agent_stop: Vec<HookCommand>,
    #[serde(rename = "sessionEnd")]
    session_end: Vec<HookCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookCommand {
    #[serde(rename = "type")]
    kind: String,
    exec: String,
    args: Vec<String>,
    #[serde(rename = "timeoutSec")]
    timeout_seconds: u64,
}

pub fn accept_disclosure(
    accepted: bool,
    stdin: &mut impl Read,
    stdin_is_terminal: bool,
    output: &mut impl Write,
) -> Result<DisclosureDecision, RegistrationError> {
    output
        .write_all(DISCLOSURE.as_bytes())
        .map_err(RegistrationError::Io)?;
    if accepted {
        return Ok(DisclosureDecision::AcceptedByFlag);
    }
    if !stdin_is_terminal {
        return Err(RegistrationError::NoninteractiveAcceptanceRequired);
    }
    output
        .write_all(b"\nType I ACCEPT to install the hooks: ")
        .map_err(RegistrationError::Io)?;
    output.flush().map_err(RegistrationError::Io)?;
    let mut bytes = Vec::new();
    stdin
        .take(128)
        .read_to_end(&mut bytes)
        .map_err(RegistrationError::Io)?;
    if String::from_utf8_lossy(&bytes).trim() == "I ACCEPT" {
        Ok(DisclosureDecision::Prompt)
    } else {
        Err(RegistrationError::DisclosureDeclined)
    }
}

pub fn accept_disclosure_from_terminal(
    accepted: bool,
) -> Result<DisclosureDecision, RegistrationError> {
    accept_disclosure(
        accepted,
        &mut io::stdin().lock(),
        io::stdin().is_terminal(),
        &mut io::stderr().lock(),
    )
}

pub fn register(config: &RegisterConfig) -> Result<(), RegistrationError> {
    validate_absolute_paths(config)?;
    let copilot_home = ensure_directory(&config.copilot_home)?;
    let hooks_directory = ensure_child_directory(&copilot_home, "hooks")?;
    let state_directory = ensure_directory(&config.state_directory)?;
    ensure_child_directory(&state_directory, "pending")?;
    ensure_child_directory(&state_directory, "workers")?;
    ensure_child_directory(&state_directory, "results")?;
    ensure_child_directory(&state_directory, "failures")?;

    let stored = StoredConfig::from_register(config)?;
    let hook = HookFile::new(config)?;
    let config_path = state_directory.join(CONFIG_FILE_NAME);
    let hook_path = hooks_directory.join(HOOK_FILE_NAME);
    validate_owned_file::<StoredConfig>(&config_path)?;
    validate_owned_file::<HookFile>(&hook_path)?;
    atomic_json_replace(&config_path, &stored)?;
    atomic_json_replace(&hook_path, &hook)?;
    Ok(())
}

pub fn unregister(copilot_home: &Path, state_directory: &Path) -> Result<(), RegistrationError> {
    if !copilot_home.is_absolute() || !state_directory.is_absolute() {
        return Err(RegistrationError::RelativePath);
    }
    let hooks = copilot_home.join("hooks");
    validate_existing_directory_if_present(copilot_home)?;
    validate_existing_directory_if_present(&hooks)?;
    validate_existing_directory_if_present(state_directory)?;
    remove_owned_file::<HookFile>(&hooks.join(HOOK_FILE_NAME))?;
    remove_owned_file::<StoredConfig>(&state_directory.join(CONFIG_FILE_NAME))?;
    Ok(())
}

impl StoredConfig {
    fn from_register(config: &RegisterConfig) -> Result<Self, RegistrationError> {
        Ok(Self {
            version: 1,
            summarizer: StoredCommand {
                executable: utf8(&config.summarizer_binary)?,
                args: config
                    .summarizer_args
                    .iter()
                    .map(|argument| {
                        argument
                            .to_str()
                            .map(ToOwned::to_owned)
                            .ok_or(RegistrationError::NonUtf8Configuration)
                    })
                    .collect::<Result<_, _>>()?,
            },
            output_directory: utf8(&config.output_directory)?,
            state_directory: utf8(&config.state_directory)?,
            limits: StoredLimits {
                timeout_ms: config.timeout.as_millis().try_into().unwrap_or(u64::MAX),
                max_source_bytes: config.max_source_bytes,
                max_input_bytes: config.max_input_bytes,
                max_stdout_bytes: config.max_stdout_bytes,
                max_stderr_bytes: config.max_stderr_bytes,
            },
            remote_delivery: false,
        })
    }
}

impl HookFile {
    fn new(config: &RegisterConfig) -> Result<Self, RegistrationError> {
        let executable = utf8(&config.executable)?;
        let state = utf8(&config.state_directory)?;
        let command = |event: &str| HookCommand {
            kind: "command".to_owned(),
            exec: executable.clone(),
            args: vec![
                "hook".to_owned(),
                event.to_owned(),
                "--state-dir".to_owned(),
                state.clone(),
            ],
            timeout_seconds: 5,
        };
        Ok(Self {
            version: 1,
            hooks: HookEvents {
                agent_stop: vec![command("agent-stop")],
                session_end: vec![command("session-end")],
            },
        })
    }
}

pub(crate) fn load_stored_config(
    state_directory: &Path,
) -> Result<StoredConfig, RegistrationError> {
    let path = state_directory.join(CONFIG_FILE_NAME);
    validate_regular_owned_file(&path)?;
    let bytes = fs::read(path).map_err(RegistrationError::Io)?;
    let config: StoredConfig = serde_json::from_slice(&bytes).map_err(RegistrationError::Json)?;
    if config.version != 1
        || config.remote_delivery
        || Path::new(&config.state_directory) != state_directory
    {
        return Err(RegistrationError::MalformedOwnedFile);
    }
    Ok(config)
}

pub(crate) fn atomic_json_replace(
    path: &Path,
    value: &impl Serialize,
) -> Result<(), RegistrationError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(RegistrationError::Json)?;
    bytes.push(b'\n');
    atomic_bytes_replace(path, &bytes)
}

pub(crate) fn atomic_bytes_replace(path: &Path, bytes: &[u8]) -> Result<(), RegistrationError> {
    let parent = path
        .parent()
        .ok_or_else(|| RegistrationError::UnsafePath(path.to_path_buf()))?;
    validate_existing_directory_if_present(parent)?;
    if path.exists() {
        validate_regular_owned_file(path)?;
    } else if fs::symlink_metadata(path).is_ok() {
        return Err(RegistrationError::UnsafePath(path.to_path_buf()));
    }
    let mut temporary = Builder::new()
        .prefix(".munshi-")
        .tempfile_in(parent)
        .map_err(RegistrationError::Io)?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(RegistrationError::Io)?;
    temporary.write_all(bytes).map_err(RegistrationError::Io)?;
    temporary.flush().map_err(RegistrationError::Io)?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(RegistrationError::Io)?;
    let file = temporary
        .persist(path)
        .map_err(|error| RegistrationError::Io(error.error))?;
    file.sync_all().map_err(RegistrationError::Io)?;
    sync_directory(parent)
}

pub(crate) fn durable_remove(path: &Path) -> Result<(), RegistrationError> {
    if !path.exists() {
        if fs::symlink_metadata(path).is_ok() {
            return Err(RegistrationError::UnsafePath(path.to_path_buf()));
        }
        return Ok(());
    }
    validate_regular_owned_file(path)?;
    fs::remove_file(path).map_err(RegistrationError::Io)?;
    sync_directory(
        path.parent()
            .ok_or_else(|| RegistrationError::UnsafePath(path.to_path_buf()))?,
    )
}

pub(crate) fn create_owned_lock(path: &Path) -> Result<Option<File>, RegistrationError> {
    let parent = path
        .parent()
        .ok_or_else(|| RegistrationError::UnsafePath(path.to_path_buf()))?;
    validate_existing_directory_if_present(parent)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            file.sync_all().map_err(RegistrationError::Io)?;
            sync_directory(parent)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_regular_owned_file(path)?;
            Ok(None)
        }
        Err(error) => Err(RegistrationError::Io(error)),
    }
}

fn validate_absolute_paths(config: &RegisterConfig) -> Result<(), RegistrationError> {
    for path in [
        &config.copilot_home,
        &config.state_directory,
        &config.output_directory,
        &config.summarizer_binary,
        &config.executable,
    ] {
        if !path.is_absolute() {
            return Err(RegistrationError::RelativePath);
        }
    }
    for executable in [&config.summarizer_binary, &config.executable] {
        validate_regular_file(executable)?;
    }
    Ok(())
}

fn utf8(path: &Path) -> Result<String, RegistrationError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or(RegistrationError::NonUtf8Configuration)
}

fn ensure_directory(path: &Path) -> Result<PathBuf, RegistrationError> {
    if !path.is_absolute() {
        return Err(RegistrationError::RelativePath);
    }
    if path.exists() {
        validate_existing_directory_if_present(path)?;
        return Ok(path.to_path_buf());
    }
    let parent = path
        .parent()
        .ok_or_else(|| RegistrationError::UnsafePath(path.to_path_buf()))?;
    if !parent.exists() {
        ensure_directory(parent)?;
    }
    validate_existing_directory_if_present(parent)?;
    fs::create_dir(path).map_err(RegistrationError::Io)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(RegistrationError::Io)?;
    sync_directory(parent)?;
    Ok(path.to_path_buf())
}

fn ensure_child_directory(parent: &Path, child: &str) -> Result<PathBuf, RegistrationError> {
    validate_existing_directory_if_present(parent)?;
    ensure_directory(&parent.join(child))
}

fn validate_existing_directory_if_present(path: &Path) -> Result<(), RegistrationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o022 == 0 =>
        {
            Ok(())
        }
        Ok(_) => Err(RegistrationError::UnsafePath(path.to_path_buf())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RegistrationError::Io(error)),
    }
}

pub(crate) fn validate_regular_owned_file(path: &Path) -> Result<(), RegistrationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o022 == 0 =>
        {
            Ok(())
        }
        Ok(_) => Err(RegistrationError::UnsafePath(path.to_path_buf())),
        Err(error) => Err(RegistrationError::Io(error)),
    }
}

fn validate_regular_file(path: &Path) -> Result<(), RegistrationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(RegistrationError::UnsafePath(path.to_path_buf())),
        Err(error) => Err(RegistrationError::Io(error)),
    }
}

fn validate_owned_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<(), RegistrationError> {
    if !path.exists() {
        if fs::symlink_metadata(path).is_ok() {
            return Err(RegistrationError::UnsafePath(path.to_path_buf()));
        }
        return Ok(());
    }
    validate_regular_owned_file(path)?;
    let bytes = fs::read(path).map_err(RegistrationError::Io)?;
    serde_json::from_slice::<T>(&bytes)
        .map(|_| ())
        .map_err(|_| RegistrationError::MalformedOwnedFile)
}

fn remove_owned_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<(), RegistrationError> {
    validate_owned_file::<T>(path)?;
    durable_remove(path)
}

fn sync_directory(path: &Path) -> Result<(), RegistrationError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(RegistrationError::Io)
}
