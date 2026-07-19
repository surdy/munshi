use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tempfile::Builder;
use thiserror::Error;

use crate::archive_git::{ArchiveGitError, ensure_archive_repository};
use crate::policy::{GlobalPolicy, ResolvedPolicy, resolve_policy};
use crate::project::{ProjectIdentityError, inspect_project};
use crate::state::{StateStore, migrate_legacy_state};

const HOOK_FILE_NAME: &str = "munshi.json";
const CONFIG_FILE_NAME: &str = "config.json";
/// Dot-prefixed so it can never collide with a `locks/<session_id>.lock` file.
const REGISTRATION_LOCK_NAME: &str = ".munshi-registration.lock";
const DEFAULT_MAX_CALLS_PER_HOUR: u32 = 10;
const DEFAULT_MAX_CALLS_PER_DAY: u32 = 50;
const DEFAULT_MAX_CONCURRENCY: usize = 2;
/// Bounded delivery attempts before a session's delivery is parked as a dead letter.
pub(crate) const DEFAULT_MAX_DELIVERY_ATTEMPTS: u32 = 5;

pub const DISCLOSURE: &str = "\
IMPORTANT: MUNSHI TRANSCRIPT PROCESSING DISCLOSURE

After registration, local transcript summarization is ON by default for all projects.
The full session transcript from each registered harness (Copilot CLI, Claude Code) is sent again to the configured summarizer and may consume credits or incur cost.
Munshi v1 has NO secret redaction or granular transcript filtering.
Summaries are written as local Markdown files in the configured output directory.
Remote delivery remains DISABLED.
Disabling future project capture does not delete summaries already written.
";

/// A Copilot CLI installation whose `hooks/` directory receives Munshi's dedicated hook file.
#[derive(Debug, Clone)]
pub struct CopilotTarget {
    pub home: PathBuf,
}

/// A Claude Code installation whose `settings.json` receives Munshi's managed hook entries.
#[derive(Debug, Clone)]
pub struct ClaudeTarget {
    pub home: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RegisterConfig {
    /// Install lifecycle hooks into this Copilot home, when targeting Copilot.
    pub copilot: Option<CopilotTarget>,
    /// Merge lifecycle hook entries into this Claude Code home's `settings.json`.
    pub claude: Option<ClaudeTarget>,
    pub state_directory: PathBuf,
    pub output_directory: PathBuf,
    pub archive_git_history: bool,
    pub summarizer_binary: PathBuf,
    pub summarizer_args: Vec<OsString>,
    pub timeout: Duration,
    pub max_source_bytes: usize,
    pub max_input_bytes: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub max_calls_per_hour: u32,
    pub max_calls_per_day: u32,
    pub max_concurrency: usize,
    pub executable: PathBuf,
}

impl RegisterConfig {
    fn harnesses_selected(&self) -> bool {
        self.copilot.is_some() || self.claude.is_some()
    }
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
    #[error("registration requires at least one harness target")]
    NoHarnessSelected,
    #[error("noninteractive registration requires --accept-transcript-processing")]
    NoninteractiveAcceptanceRequired,
    #[error("registration paths and executables must be absolute")]
    RelativePath,
    #[error("refusing an unsafe symlink, file type, or ownership at {0}")]
    UnsafePath(PathBuf),
    #[error("the existing Munshi-owned file is malformed or was not created by this version")]
    MalformedOwnedFile,
    #[error("the harness settings file at {0} is not a JSON settings object Munshi can merge into")]
    ForeignSettingsUnrecognized(PathBuf),
    #[error("another Munshi registration operation is active")]
    RegistrationBusy,
    #[error("configuration contains a non-UTF-8 argument or path")]
    NonUtf8Configuration,
    #[error("registration I/O failed")]
    Io(#[source] io::Error),
    #[error("registration JSON failed")]
    Json(#[source] serde_json::Error),
    #[error("archive Git history registration failed")]
    ArchiveGit(#[source] ArchiveGitError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredConfig {
    pub version: u32,
    pub summarizer: StoredCommand,
    pub output_directory: String,
    pub state_directory: String,
    #[serde(default)]
    pub archive_git_history: bool,
    pub local_archival_enabled: bool,
    pub transcript_processing_accepted: bool,
    pub project_origin: String,
    pub limits: StoredLimits,
    pub remote_delivery: bool,
    #[serde(default)]
    pub delivery: StoredDelivery,
    #[serde(default = "StoredPolicy::defaults")]
    pub policy: StoredPolicy,
    /// Which harness installations this registration manages hooks for. The state store is
    /// harness-neutral (ADR 0008); each recorded home locates that harness's hook installation.
    #[serde(default)]
    pub harnesses: StoredHarnesses,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredHarnesses {
    /// Copilot home whose `hooks/munshi.json` this registration owns.
    #[serde(default)]
    pub copilot_home: Option<String>,
    /// Claude Code home whose `settings.json` carries Munshi's hook entries.
    #[serde(default)]
    pub claude_home: Option<String>,
}

impl StoredHarnesses {
    fn paths_are_absolute(&self) -> bool {
        self.copilot_home
            .as_deref()
            .is_none_or(|home| Path::new(home).is_absolute())
            && self
                .claude_home
                .as_deref()
                .is_none_or(|home| Path::new(home).is_absolute())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredCommand {
    pub executable: String,
    pub args: Vec<String>,
}

/// The Munshi-owned Notesmith sink configuration. Delivery is opt-in and disabled by default via
/// `remote_delivery`; this section records only *where* to deliver and *how to find* a credential.
/// It never stores the credential itself: `credential` names an environment variable or an
/// operating-system credential-store entry that is read at delivery time (khata-handoff.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredDelivery {
    /// Base URL of the Notesmith daemon, for example `http://127.0.0.1:27183`.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Target Notesmith vault name.
    #[serde(default)]
    pub vault: Option<String>,
    /// Optional vault-relative folder that Munshi-owned session notes are filed under.
    #[serde(default)]
    pub folder: Option<String>,
    /// Where the bearer credential is read from, or `None` for an unauthenticated local daemon.
    #[serde(default)]
    pub credential: Option<StoredCredential>,
    /// Bounded number of delivery attempts before a session is parked as a dead letter.
    #[serde(default = "default_max_delivery_attempts")]
    pub max_attempts: u32,
    /// Issue #9: when versioned delivery is required but the remote revision-history capability is
    /// absent, whether Munshi should explicitly configure it (`true`) or only verify it (`false`,
    /// the default). Configuring mutates the Notesmith vault config to enable per-vault Git.
    #[serde(default)]
    pub provision_history: bool,
}

fn default_max_delivery_attempts() -> u32 {
    DEFAULT_MAX_DELIVERY_ATTEMPTS
}

impl Default for StoredDelivery {
    fn default() -> Self {
        Self {
            endpoint: None,
            vault: None,
            folder: None,
            credential: None,
            max_attempts: DEFAULT_MAX_DELIVERY_ATTEMPTS,
            provision_history: false,
        }
    }
}

impl StoredDelivery {
    /// A sink is fully addressable only when both the endpoint and vault are present.
    pub(crate) fn is_addressable(&self) -> bool {
        self.endpoint
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            && self.vault.as_deref().is_some_and(|value| !value.is_empty())
    }
}

/// The source of the Notesmith bearer credential. Munshi resolves the actual secret at delivery
/// time and never persists it in configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StoredCredential {
    /// Read the bearer token from the named environment variable.
    Env { var: String },
    /// Read the bearer token from an operating-system credential store entry.
    Keychain { service: String, account: String },
}

/// Global project-policy defaults: default-on processing, bounded summarization cost, and bounded
/// worker concurrency. `disabled_projects` holds canonical project identities excluded from future
/// processing and delivery by an explicit `munshi project disable`; existing archives are untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredPolicy {
    pub max_calls_per_hour: u32,
    pub max_calls_per_day: u32,
    pub max_concurrency: usize,
    #[serde(default)]
    pub disabled_projects: Vec<String>,
}

impl StoredPolicy {
    fn defaults() -> Self {
        Self {
            max_calls_per_hour: DEFAULT_MAX_CALLS_PER_HOUR,
            max_calls_per_day: DEFAULT_MAX_CALLS_PER_DAY,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            disabled_projects: Vec::new(),
        }
    }

    pub(crate) fn as_global(&self) -> GlobalPolicy {
        GlobalPolicy {
            max_calls_per_hour: self.max_calls_per_hour,
            max_calls_per_day: self.max_calls_per_day,
            max_concurrency: self.max_concurrency,
        }
    }
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
    if !config.harnesses_selected() {
        return Err(RegistrationError::NoHarnessSelected);
    }
    let state_directory = ensure_directory(&config.state_directory)?;
    let locks_directory = ensure_child_directory(&state_directory, "locks")?;
    let config_path = state_directory.join(CONFIG_FILE_NAME);
    let lock_path = locks_directory.join(REGISTRATION_LOCK_NAME);
    let _lock = acquire_registration_lock(&lock_path)?;
    validate_owned_file::<StoredConfig>(&config_path)?;
    let copilot_hook = config
        .copilot
        .as_ref()
        .map(|target| {
            let home = ensure_directory(&target.home)?;
            let hooks_directory = ensure_child_directory(&home, "hooks")?;
            let hook_path = hooks_directory.join(HOOK_FILE_NAME);
            validate_owned_file::<HookFile>(&hook_path)?;
            Ok::<_, RegistrationError>((hook_path, HookFile::new(config)?))
        })
        .transpose()?;
    let claude_settings = config
        .claude
        .as_ref()
        .map(|target| {
            let home = ensure_directory(&target.home)?;
            let settings_path = home.join("settings.json");
            crate::claude_settings::validate_claude_settings(&settings_path)?;
            Ok::<_, RegistrationError>(settings_path)
        })
        .transpose()?;
    // Re-registration must not silently re-enable projects an explicit `project disable` excluded.
    let disabled_projects = existing_disabled_projects(&config_path)?;
    let stored = StoredConfig::from_register(config, disabled_projects)?;
    if config.archive_git_history {
        ensure_archive_repository(&config.output_directory)
            .map_err(RegistrationError::ArchiveGit)?;
    }
    install_or_update_json(&config_path, &stored)?;
    let mut state = StateStore::open(&state_directory).map_err(state_registration_error)?;
    state
        .rebuild_from_archives(&config.output_directory)
        .map_err(state_registration_error)?;
    migrate_legacy_state(
        &mut state,
        &state_directory,
        config.timeout.saturating_add(Duration::from_secs(60)),
    )
    .map_err(state_registration_error)?;
    if let Some((hook_path, hook)) = &copilot_hook {
        install_or_update_json(hook_path, hook)?;
    }
    if let Some(settings_path) = &claude_settings {
        crate::claude_settings::install_claude_hooks(settings_path, &config.executable)?;
    }
    Ok(())
}

fn existing_disabled_projects(config_path: &Path) -> Result<Vec<String>, RegistrationError> {
    if !config_path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(config_path).map_err(RegistrationError::Io)?;
    let config: StoredConfig =
        serde_json::from_slice(&bytes).map_err(|_| RegistrationError::MalformedOwnedFile)?;
    Ok(config.policy.disabled_projects)
}

/// Removes Munshi's hook installations and active configuration. Hook locations come from the
/// stored configuration's harness records; `copilot_home_fallback` covers an orphaned Copilot
/// hook file left behind without a readable configuration.
pub fn unregister(
    state_directory: &Path,
    copilot_home_fallback: &Path,
) -> Result<(), RegistrationError> {
    if !copilot_home_fallback.is_absolute() || !state_directory.is_absolute() {
        return Err(RegistrationError::RelativePath);
    }
    validate_existing_directory_if_present(state_directory)?;
    validate_existing_directory_if_present(copilot_home_fallback)?;
    let config_path = state_directory.join(CONFIG_FILE_NAME);
    let config_exists = recognized_owned_file_exists::<StoredConfig>(&config_path)?;
    let (copilot_home, claude_home) = if config_exists {
        let config = load_stored_config(state_directory)?;
        (
            config.harnesses.copilot_home.map(PathBuf::from),
            config.harnesses.claude_home.map(PathBuf::from),
        )
    } else {
        (Some(copilot_home_fallback.to_path_buf()), None)
    };
    let hook_path = copilot_home.map(|home| home.join("hooks").join(HOOK_FILE_NAME));
    let hook_exists = match hook_path.as_deref() {
        Some(path) => {
            if let Some(hooks) = path.parent() {
                validate_existing_directory_if_present(hooks)?;
            }
            recognized_owned_file_exists::<HookFile>(path)?
        }
        None => false,
    };
    if !hook_exists && !config_exists {
        return Ok(());
    }
    let _lock = if state_directory.exists() {
        let locks_directory = ensure_child_directory(state_directory, "locks")?;
        Some(acquire_registration_lock(
            &locks_directory.join(REGISTRATION_LOCK_NAME),
        )?)
    } else {
        None
    };
    if hook_exists {
        let hook_path = hook_path.expect("hook existence implies a path");
        validate_owned_file::<HookFile>(&hook_path)?;
        durable_remove(&hook_path)?;
    }
    if let Some(home) = claude_home {
        crate::claude_settings::remove_claude_hooks(&home.join("settings.json"))?;
    }
    if config_exists {
        validate_owned_file::<StoredConfig>(&config_path)?;
        durable_remove(&config_path)?;
    }
    Ok(())
}

impl StoredConfig {
    fn from_register(
        config: &RegisterConfig,
        disabled_projects: Vec<String>,
    ) -> Result<Self, RegistrationError> {
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
            archive_git_history: config.archive_git_history,
            local_archival_enabled: true,
            transcript_processing_accepted: true,
            project_origin: "agent_stop_cwd".to_owned(),
            limits: StoredLimits {
                timeout_ms: config.timeout.as_millis().try_into().unwrap_or(u64::MAX),
                max_source_bytes: config.max_source_bytes,
                max_input_bytes: config.max_input_bytes,
                max_stdout_bytes: config.max_stdout_bytes,
                max_stderr_bytes: config.max_stderr_bytes,
            },
            remote_delivery: false,
            delivery: StoredDelivery::default(),
            policy: StoredPolicy {
                max_calls_per_hour: config.max_calls_per_hour,
                max_calls_per_day: config.max_calls_per_day,
                max_concurrency: config.max_concurrency,
                disabled_projects,
            },
            harnesses: StoredHarnesses {
                copilot_home: config
                    .copilot
                    .as_ref()
                    .map(|target| utf8(&target.home))
                    .transpose()?,
                claude_home: config
                    .claude
                    .as_ref()
                    .map(|target| utf8(&target.home))
                    .transpose()?,
            },
        })
    }
}

trait ManagedFile {
    fn is_recognized(&self) -> bool;
}

impl ManagedFile for StoredConfig {
    fn is_recognized(&self) -> bool {
        self.version == 1
            && self.local_archival_enabled
            && self.transcript_processing_accepted
            && self.project_origin == "agent_stop_cwd"
            && self.policy.max_concurrency >= 1
            && (!self.remote_delivery || self.delivery.is_addressable())
            && Path::new(&self.output_directory).is_absolute()
            && Path::new(&self.state_directory).is_absolute()
            && Path::new(&self.summarizer.executable).is_absolute()
            && self.harnesses.paths_are_absolute()
    }
}

impl HookFile {
    fn new(config: &RegisterConfig) -> Result<Self, RegistrationError> {
        let executable = utf8(&config.executable)?;
        let command = |event: &str| HookCommand {
            kind: "command".to_owned(),
            exec: executable.clone(),
            args: vec!["hook".to_owned(), event.to_owned()],
            timeout_seconds: 2,
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

impl ManagedFile for HookFile {
    fn is_recognized(&self) -> bool {
        let valid = |commands: &[HookCommand], event: &str| {
            commands.len() == 1
                && commands[0].kind == "command"
                && commands[0].timeout_seconds == 2
                && Path::new(&commands[0].exec).is_absolute()
                && commands[0].args == ["hook", event]
        };
        self.version == 1
            && valid(&self.hooks.agent_stop, "agent-stop")
            && valid(&self.hooks.session_end, "session-end")
    }
}

/// Whether a Munshi-owned `config.json` exists in `state_directory`, without validating its
/// contents. Lets read-only status queries (e.g. `munshi delivery status`) distinguish "never
/// registered here" — which should degrade to empty/default output, like `sessions`/`status`/`show`
/// already do — from a genuine I/O or malformed-file error while loading it.
pub(crate) fn stored_config_exists(state_directory: &Path) -> bool {
    state_directory.join(CONFIG_FILE_NAME).is_file()
}

pub(crate) fn load_stored_config(
    state_directory: &Path,
) -> Result<StoredConfig, RegistrationError> {
    let path = state_directory.join(CONFIG_FILE_NAME);
    validate_regular_owned_file(&path)?;
    let bytes = fs::read(path).map_err(RegistrationError::Io)?;
    let config: StoredConfig = serde_json::from_slice(&bytes).map_err(RegistrationError::Json)?;
    if config.version != 1
        || !config.local_archival_enabled
        || !config.transcript_processing_accepted
        || config.project_origin != "agent_stop_cwd"
        || config.policy.max_concurrency < 1
        || (config.remote_delivery && !config.delivery.is_addressable())
        || Path::new(&config.state_directory) != state_directory
        || !config.harnesses.paths_are_absolute()
    {
        return Err(RegistrationError::MalformedOwnedFile);
    }
    Ok(config)
}

/// The effective enable/disable state and budgets for one project directory, combining an explicit
/// `munshi project disable` with any nearest-parent `.munshi.toml` override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectStatus {
    pub identity: String,
    pub enabled: bool,
    pub disabled_reason: Option<&'static str>,
    pub max_calls_per_hour: u32,
    pub max_calls_per_day: u32,
}

/// Serializes a mutation of the Munshi-owned `config.json` against `register`/project commands by
/// holding the registration lock, loading the recognized config, applying `update`, and writing it
/// atomically. Returns the resulting config and the closure's value.
pub(crate) fn update_stored_config<T>(
    state_directory: &Path,
    update: impl FnOnce(&mut StoredConfig) -> Result<T, RegistrationError>,
) -> Result<(StoredConfig, T), RegistrationError> {
    let locks_directory = ensure_child_directory(state_directory, "locks")?;
    let lock_path = locks_directory.join(REGISTRATION_LOCK_NAME);
    let _lock = acquire_registration_lock(&lock_path)?;
    let config_path = state_directory.join(CONFIG_FILE_NAME);
    let mut config = load_stored_config(state_directory)?;
    let value = update(&mut config)?;
    atomic_json_replace(&config_path, &config)?;
    Ok((config, value))
}

/// Adds or removes `project_directory`'s canonical identity from the explicitly disabled-projects
/// list in global configuration, using the registration lock to serialize with `register`.
/// Disabling stops future processing and delivery for the project; it never touches archives
/// already written locally or delivered remotely.
pub fn set_project_enabled(
    state_directory: &Path,
    project_directory: &Path,
    enabled: bool,
) -> Result<ProjectStatus, RegistrationError> {
    let identity = inspect_project(project_directory)
        .map_err(project_identity_error)?
        .identity;
    let locks_directory = ensure_child_directory(state_directory, "locks")?;
    let lock_path = locks_directory.join(REGISTRATION_LOCK_NAME);
    let _lock = acquire_registration_lock(&lock_path)?;
    let config_path = state_directory.join(CONFIG_FILE_NAME);
    let mut config = load_stored_config(state_directory)?;
    if enabled {
        config
            .policy
            .disabled_projects
            .retain(|value| value != &identity);
    } else if !config
        .policy
        .disabled_projects
        .iter()
        .any(|value| value == &identity)
    {
        config.policy.disabled_projects.push(identity.clone());
    }
    atomic_json_replace(&config_path, &config)?;
    Ok(ProjectStatus {
        identity,
        enabled,
        disabled_reason: (!enabled).then_some("project-disabled"),
        max_calls_per_hour: config.policy.max_calls_per_hour,
        max_calls_per_day: config.policy.max_calls_per_day,
    })
}

/// Reports the effective policy for a project directory without changing anything, merging the
/// explicit disabled-projects list and any nearest-parent `.munshi.toml` override over global
/// configuration.
pub fn project_status(
    state_directory: &Path,
    project_directory: &Path,
) -> Result<ProjectStatus, RegistrationError> {
    let identity_info = inspect_project(project_directory).map_err(project_identity_error)?;
    let config = load_stored_config(state_directory)?;
    let ResolvedPolicy {
        enabled,
        disabled_reason,
        max_calls_per_hour,
        max_calls_per_day,
        ..
    } = resolve_policy(
        &config.policy.as_global(),
        &config.policy.disabled_projects,
        &identity_info.identity,
        Some(project_directory),
    );
    Ok(ProjectStatus {
        identity: identity_info.identity,
        enabled,
        disabled_reason: disabled_reason.map(|reason| reason.as_category()),
        max_calls_per_hour,
        max_calls_per_day,
    })
}

fn project_identity_error(error: ProjectIdentityError) -> RegistrationError {
    RegistrationError::Io(io::Error::other(error.to_string()))
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

fn install_or_update_json<T>(path: &Path, value: &T) -> Result<(), RegistrationError>
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + ManagedFile,
{
    if path.exists() {
        validate_regular_owned_file(path)?;
        let existing: T = serde_json::from_slice(&fs::read(path).map_err(RegistrationError::Io)?)
            .map_err(|_| RegistrationError::MalformedOwnedFile)?;
        if !existing.is_recognized() {
            return Err(RegistrationError::MalformedOwnedFile);
        }
        if &existing == value {
            return Ok(());
        }
        return atomic_json_replace(path, value);
    }
    if fs::symlink_metadata(path).is_ok() {
        return Err(RegistrationError::UnsafePath(path.to_path_buf()));
    }
    let parent = path
        .parent()
        .ok_or_else(|| RegistrationError::UnsafePath(path.to_path_buf()))?;
    validate_existing_directory_if_present(parent)?;
    let mut bytes = serde_json::to_vec_pretty(value).map_err(RegistrationError::Json)?;
    bytes.push(b'\n');
    let mut temporary = Builder::new()
        .prefix(".munshi-")
        .tempfile_in(parent)
        .map_err(RegistrationError::Io)?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(RegistrationError::Io)?;
    temporary.write_all(&bytes).map_err(RegistrationError::Io)?;
    temporary.flush().map_err(RegistrationError::Io)?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(RegistrationError::Io)?;
    let file = temporary
        .persist_noclobber(path)
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

struct RegistrationLock {
    _file: File,
}

fn acquire_registration_lock(path: &Path) -> Result<RegistrationLock, RegistrationError> {
    let parent = path
        .parent()
        .ok_or_else(|| RegistrationError::UnsafePath(path.to_path_buf()))?;
    validate_existing_directory_if_present(parent)?;

    let (file, created) = loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                validate_registration_lock_metadata(path, &metadata)?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)
                    .map_err(|error| lock_open_error(path, error))?;
                break (file, false);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)
                {
                    Ok(file) => break (file, true),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(lock_open_error(path, error)),
                }
            }
            Err(error) => return Err(RegistrationError::Io(error)),
        }
    };

    if created {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(RegistrationError::Io)?;
        file.sync_all().map_err(RegistrationError::Io)?;
        sync_directory(parent)?;
    }
    let opened = file.metadata().map_err(RegistrationError::Io)?;
    validate_registration_lock_metadata(path, &opened)?;

    let lock_result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if lock_result == -1 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::WouldBlock {
            Err(RegistrationError::RegistrationBusy)
        } else {
            Err(RegistrationError::Io(error))
        };
    }

    let current = fs::symlink_metadata(path)
        .map_err(|_| RegistrationError::UnsafePath(path.to_path_buf()))?;
    validate_registration_lock_metadata(path, &current)?;
    if current.dev() != opened.dev() || current.ino() != opened.ino() {
        return Err(RegistrationError::UnsafePath(path.to_path_buf()));
    }
    Ok(RegistrationLock { _file: file })
}

fn validate_registration_lock_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), RegistrationError> {
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        Err(RegistrationError::UnsafePath(path.to_path_buf()))
    } else {
        Ok(())
    }
}

fn lock_open_error(path: &Path, error: io::Error) -> RegistrationError {
    if error.raw_os_error() == Some(libc::ELOOP) {
        RegistrationError::UnsafePath(path.to_path_buf())
    } else {
        RegistrationError::Io(error)
    }
}

fn validate_absolute_paths(config: &RegisterConfig) -> Result<(), RegistrationError> {
    let mut paths = vec![
        &config.state_directory,
        &config.output_directory,
        &config.summarizer_binary,
        &config.executable,
    ];
    if let Some(copilot) = &config.copilot {
        paths.push(&copilot.home);
    }
    if let Some(claude) = &config.claude {
        paths.push(&claude.home);
    }
    for path in paths {
        if !path.is_absolute() {
            return Err(RegistrationError::RelativePath);
        }
    }
    for executable in [&config.summarizer_binary, &config.executable] {
        validate_regular_file(executable)?;
    }
    Ok(())
}

pub(crate) fn utf8(path: &Path) -> Result<String, RegistrationError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or(RegistrationError::NonUtf8Configuration)
}

pub(crate) fn ensure_directory(path: &Path) -> Result<PathBuf, RegistrationError> {
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

pub(crate) fn validate_existing_directory_if_present(path: &Path) -> Result<(), RegistrationError> {
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
        Ok(metadata)
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.mode() & 0o111 != 0 =>
        {
            Ok(())
        }
        Ok(_) => Err(RegistrationError::UnsafePath(path.to_path_buf())),
        Err(error) => Err(RegistrationError::Io(error)),
    }
}

fn validate_owned_file<T: for<'de> Deserialize<'de> + ManagedFile>(
    path: &Path,
) -> Result<(), RegistrationError> {
    recognized_owned_file_exists::<T>(path).map(|_| ())
}

fn recognized_owned_file_exists<T: for<'de> Deserialize<'de> + ManagedFile>(
    path: &Path,
) -> Result<bool, RegistrationError> {
    if !path.exists() {
        if fs::symlink_metadata(path).is_ok() {
            return Err(RegistrationError::UnsafePath(path.to_path_buf()));
        }
        return Ok(false);
    }
    validate_regular_owned_file(path)?;
    let bytes = fs::read(path).map_err(RegistrationError::Io)?;
    let value =
        serde_json::from_slice::<T>(&bytes).map_err(|_| RegistrationError::MalformedOwnedFile)?;
    if value.is_recognized() {
        Ok(true)
    } else {
        Err(RegistrationError::MalformedOwnedFile)
    }
}

fn sync_directory(path: &Path) -> Result<(), RegistrationError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(RegistrationError::Io)
}

fn state_registration_error(error: crate::state::StateError) -> RegistrationError {
    RegistrationError::Io(io::Error::other(error.to_string()))
}
