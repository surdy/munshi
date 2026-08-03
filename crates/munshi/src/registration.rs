use std::collections::BTreeMap;
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
use crate::exhaust::{ExhaustPolicy, conflicting_source_home, default_copilot_home};
use crate::policy::{GlobalPolicy, ResolvedPolicy, resolve_policy};
use crate::project::{ProjectIdentityError, inspect_project};
use crate::source::SourceHomes;
use crate::state::{StateStore, migrate_legacy_state};

const HOOK_FILE_NAME: &str = "munshi.json";
const CONFIG_FILE_NAME: &str = "config.json";
/// Current `config.json` schema version. Version 2 (issue #36) replaced the scattered v1 pair of a
/// top-level `remote_delivery` bool plus a `delivery` section with one self-contained
/// `summary_delivery` section carrying `enabled` inside, mirroring `archive_upload`'s shape.
/// Version 1 files still load: they are migrated forward losslessly and persisted as version 2.
pub(crate) const CONFIG_VERSION: u32 = 2;
/// Dot-prefixed so it can never collide with a `locks/<session_id>.lock` file.
const REGISTRATION_LOCK_NAME: &str = ".munshi-registration.lock";
const DEFAULT_MAX_CALLS_PER_HOUR: u32 = 10;
const DEFAULT_MAX_CALLS_PER_DAY: u32 = 50;
const DEFAULT_MAX_CONCURRENCY: usize = 2;
/// Bounded delivery attempts before a session's delivery is parked as a dead letter.
pub(crate) const DEFAULT_MAX_DELIVERY_ATTEMPTS: u32 = 5;
/// Bounded archive-upload attempts before a session's upload is parked as a dead letter.
pub(crate) const DEFAULT_MAX_ARCHIVE_UPLOAD_ATTEMPTS: u32 = 5;
/// Retention window applied to the isolated summarizer home when one is configured (issue #60).
/// A week outlives any troubleshooting window for a summarization that already succeeded, and the
/// exhaust it protects is a third copy of content already in the archive and in Patwari.
pub const DEFAULT_SUMMARIZER_EXHAUST_RETENTION_DAYS: u32 = 7;

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
    /// Environment variables set on every summarizer invocation (repeatable `--summarizer-env
    /// KEY=VALUE`, validated by [`crate::summary::parse_summarizer_env`]). Opaque to Munshi.
    pub summarizer_env: Vec<(String, String)>,
    pub timeout: Duration,
    pub max_source_bytes: usize,
    pub max_input_bytes: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
    pub chunk_threshold_bytes: usize,
    pub chunk_size_bytes: usize,
    pub max_calls_per_hour: u32,
    pub max_calls_per_day: u32,
    pub max_concurrency: usize,
    /// Isolated summarizer home whose Copilot exhaust `munshi tick` prunes (issue #60), or `None`
    /// to keep everything. Refused when it overlaps a harness home this registration captures
    /// from, or the default `~/.copilot`.
    pub summarizer_exhaust_home: Option<PathBuf>,
    /// Age above which the exhaust home's `session-state/` entries are deleted. `0` keeps
    /// everything, as does an absent `summarizer_exhaust_home`.
    pub summarizer_exhaust_retention_days: u32,
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
    #[error(
        "summarizer exhaust home {home} overlaps the harness home {registered}; \
         retention never deletes inside a captured harness home"
    )]
    SummarizerExhaustOverlap { home: PathBuf, registered: PathBuf },
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
    #[serde(default)]
    pub summary_delivery: StoredSummaryDelivery,
    #[serde(default = "StoredPolicy::defaults")]
    pub policy: StoredPolicy,
    /// Opt-in Patwari archive-upload configuration (ADR 0009). Disabled by default; it holds the
    /// persistent client identity that must survive an operational-database rebuild, so it lives
    /// here in durable configuration rather than in rebuildable SQLite state.
    #[serde(default)]
    pub archive_upload: StoredArchiveUpload,
    /// Which harness installations this registration manages hooks for. The state store is
    /// harness-neutral (ADR 0008); each recorded home locates that harness's hook installation.
    #[serde(default)]
    pub harnesses: StoredHarnesses,
    /// Retention for the isolated summarizer home's exhaust (issue #60). Defaulted to the
    /// keep-everything shape, so configurations written before this feature load unchanged and
    /// behave exactly as they did.
    #[serde(default)]
    pub summarizer_exhaust: StoredSummarizerExhaust,
}

/// Where the summarizer's isolated home lives and how long its byproduct is kept.
///
/// Munshi has no other record of that home: the isolation lives inside the summarizer wrapper
/// (`contrib/copilot-summarizer.sh` sets `COPILOT_HOME`), which Munshi treats as an opaque
/// executable. Naming it here is what makes retention possible at all — and the reason the
/// overlap guard exists, since nothing else proves the named path is not a captured harness home.
///
/// Registration-owned like `summarizer.executable`/`args`: each `munshi register` rewrites this
/// section from its flags, so omitting the flags turns retention off again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredSummarizerExhaust {
    /// Absolute path of the isolated summarizer home, or absent for no retention at all.
    #[serde(default)]
    pub home: Option<String>,
    /// Age in whole days above which a `session-state/` entry is deleted. `0` keeps everything.
    #[serde(default)]
    pub retention_days: u32,
}

impl StoredSummarizerExhaust {
    /// The active retention policy, or `None` when this configuration keeps everything.
    pub(crate) fn policy(&self) -> Option<ExhaustPolicy> {
        ExhaustPolicy::new(self.home.as_deref(), self.retention_days)
    }

    fn path_is_absolute(&self) -> bool {
        self.home
            .as_deref()
            .is_none_or(|home| Path::new(home).is_absolute())
    }
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
    /// The recorded homes as the transcript-discovery paths consume them (issue #53). Only a home
    /// this registration manages is ever searched for a transcript, so an unregistered harness
    /// simply has nothing to derive from.
    pub(crate) fn source_homes(&self) -> SourceHomes {
        SourceHomes {
            copilot_home: self.copilot_home.as_deref().map(PathBuf::from),
            claude_home: self.claude_home.as_deref().map(PathBuf::from),
        }
    }

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
    /// Environment variables set on every summarizer invocation, on top of the inherited
    /// environment. Opaque to Munshi — the wrapper contract (docs/summarizers.md) gives the keys
    /// meaning — and always merged before Munshi's own per-invocation variables, which win on
    /// conflict. Additive with a serde default (no config version bump), so configurations
    /// written before this field load unchanged. Registration-owned like `executable`/`args`:
    /// each `munshi register` rewrites it from its `--summarizer-env` flags.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// The Munshi-owned Notesmith summary-delivery configuration. Summary delivery is opt-in and
/// disabled by default via `enabled`; the section records *where* to deliver and *how to find* a
/// credential, mirroring `archive_upload`'s self-contained shape (issue #36). It never stores the
/// credential itself: `credential` names an environment variable or an operating-system
/// credential-store entry that is read at delivery time (khata-handoff.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredSummaryDelivery {
    /// Whether current summary revisions are delivered after a successful local archive. Opt-in;
    /// disabled by default.
    #[serde(default)]
    pub enabled: bool,
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

impl Default for StoredSummaryDelivery {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            vault: None,
            folder: None,
            credential: None,
            max_attempts: DEFAULT_MAX_DELIVERY_ATTEMPTS,
            provision_history: false,
        }
    }
}

impl StoredSummaryDelivery {
    /// A sink is fully addressable only when both the endpoint and vault are present.
    pub(crate) fn is_addressable(&self) -> bool {
        self.endpoint
            .as_deref()
            .is_some_and(|value| !value.is_empty())
            && self.vault.as_deref().is_some_and(|value| !value.is_empty())
    }
}

/// The legacy version-1 `config.json` shape: a top-level `remote_delivery` bool plus a `delivery`
/// section. Superseded by the self-contained `summary_delivery` section in version 2 (issue #36);
/// kept only so existing configurations load losslessly and migrate forward.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredConfigV1 {
    #[allow(dead_code)]
    version: u32,
    summarizer: StoredCommand,
    output_directory: String,
    state_directory: String,
    #[serde(default)]
    archive_git_history: bool,
    local_archival_enabled: bool,
    transcript_processing_accepted: bool,
    project_origin: String,
    limits: StoredLimits,
    remote_delivery: bool,
    #[serde(default)]
    delivery: StoredSummaryDelivery,
    #[serde(default = "StoredPolicy::defaults")]
    policy: StoredPolicy,
    #[serde(default)]
    archive_upload: StoredArchiveUpload,
    #[serde(default)]
    harnesses: StoredHarnesses,
}

impl From<StoredConfigV1> for StoredConfig {
    /// The lossless v1 -> v2 migration: `remote_delivery` becomes `summary_delivery.enabled` and
    /// every `delivery.*` field carries over verbatim.
    fn from(previous: StoredConfigV1) -> Self {
        let mut summary_delivery = previous.delivery;
        summary_delivery.enabled = previous.remote_delivery;
        Self {
            version: CONFIG_VERSION,
            summarizer: previous.summarizer,
            output_directory: previous.output_directory,
            state_directory: previous.state_directory,
            archive_git_history: previous.archive_git_history,
            local_archival_enabled: previous.local_archival_enabled,
            transcript_processing_accepted: previous.transcript_processing_accepted,
            project_origin: previous.project_origin,
            limits: previous.limits,
            summary_delivery,
            policy: previous.policy,
            archive_upload: previous.archive_upload,
            harnesses: previous.harnesses,
            summarizer_exhaust: StoredSummarizerExhaust::default(),
        }
    }
}

/// The `version` recorded in raw `config.json` bytes, read leniently so version dispatch works on
/// both the v1 and v2 shapes.
fn stored_config_version(bytes: &[u8]) -> Option<u32> {
    #[derive(Deserialize)]
    struct VersionProbe {
        #[serde(default)]
        version: Option<u32>,
    }
    serde_json::from_slice::<VersionProbe>(bytes)
        .ok()
        .and_then(|probe| probe.version)
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

/// The Munshi-owned Patwari archive-upload configuration (ADR 0009). Archive upload is opt-in and
/// disabled by default via `enabled`; it records where to upload and the persistent client UUID
/// Munshi registers with Patwari. The client UUID is generated once and stored here because it must
/// survive an operational-database rebuild (ADR 0004): SQLite state is rebuildable, this is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredArchiveUpload {
    /// Whether archive upload runs after a successful local archive. Opt-in; disabled by default.
    #[serde(default)]
    pub enabled: bool,
    /// Base URL of the Patwari archive server, for example `http://127.0.0.1:8080`.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The persistent client UUID Munshi registers and uploads under. Generated once, reused
    /// verbatim, and durable across database rebuilds.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Bounded number of upload attempts before a session's upload is parked as a dead letter.
    #[serde(default = "default_max_archive_upload_attempts")]
    pub max_attempts: u32,
}

fn default_max_archive_upload_attempts() -> u32 {
    DEFAULT_MAX_ARCHIVE_UPLOAD_ATTEMPTS
}

impl Default for StoredArchiveUpload {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            client_id: None,
            max_attempts: DEFAULT_MAX_ARCHIVE_UPLOAD_ATTEMPTS,
        }
    }
}

impl StoredArchiveUpload {
    /// A server is addressable only when an endpoint is present.
    pub(crate) fn is_addressable(&self) -> bool {
        self.endpoint
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    }
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
    /// Per-event extraction threshold: content larger than this is extracted as an
    /// `outputs/<sha256>` snapshot artifact and elided from summarizer input (ADR 0010). Defaulted
    /// so configurations written before issue #20 keep the historical 128 KB cap.
    #[serde(default = "default_max_event_text_bytes")]
    pub max_event_text_bytes: usize,
    /// Chunked map-reduce trigger (issue #48): a session whose measured one-shot summarizer
    /// request exceeds this is summarized in per-segment chunks plus a reduce pass, instead of
    /// one shot (or the input-limit placeholder floor). Also the per-invocation cap on any single
    /// chunk/reduce request — the empirically calibrated size real summarizer backends reject
    /// past. Defaulted (additively, no config version bump) so configurations written before
    /// issue #48 load unchanged.
    #[serde(default = "default_chunk_threshold_bytes")]
    pub chunk_threshold_bytes: usize,
    /// Approximate serialized-events payload each chunk request targets on the chunked path
    /// (issue #48). Chunks split only on event boundaries, so individual chunks may run over or
    /// under. Defaulted like `chunk_threshold_bytes`.
    #[serde(default = "default_chunk_size_bytes")]
    pub chunk_size_bytes: usize,
}

fn default_max_event_text_bytes() -> usize {
    crate::source::DEFAULT_MAX_EVENT_TEXT_BYTES
}

/// Token-calibrated threshold (issue #48 live-calibration comment): the summarizer backend's real
/// boundary is tokens (~922k), not bytes, and observed byte/token ratios of ~3.2–4.5 mean the
/// byte-calibrated 6 MiB threshold still admitted one-shot rejections. 2.5 MiB stays under the
/// token limit even at the densest observed ratio (2,621,440 / 3.2 ≈ 819k tokens).
pub const DEFAULT_CHUNK_THRESHOLD_BYTES: usize = 2_621_440;
/// Chunk payload target sized against the token-calibrated threshold above (issue #48): 1.5 MiB
/// keeps every chunk request comfortably inside the accepted-input range.
pub(crate) const DEFAULT_CHUNK_SIZE_BYTES: usize = 1_572_864;

fn default_chunk_threshold_bytes() -> usize {
    DEFAULT_CHUNK_THRESHOLD_BYTES
}

fn default_chunk_size_bytes() -> usize {
    DEFAULT_CHUNK_SIZE_BYTES
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
    validate_summarizer_exhaust_home(config)?;
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
    // Re-registration must not silently re-enable projects an explicit `project disable` excluded,
    // nor reset the delivery and archive-upload sections their own commands configured (issue #31):
    // the existing configuration is carried into the freshly written one.
    let existing = existing_stored_config(&config_path)?;
    let stored = StoredConfig::from_register(config, existing)?;
    if config.archive_git_history {
        ensure_archive_repository(&config.output_directory)
            .map_err(RegistrationError::ArchiveGit)?;
    }
    install_or_update_json(&config_path, &stored)?;
    let mut state = StateStore::open(&state_directory).map_err(state_registration_error)?;
    // The freshly written configuration is what names the harness homes, so the rebuild can
    // re-derive transcript paths for the sessions it imports (issue #53).
    state
        .rebuild_from_archives(&config.output_directory, &stored.harnesses.source_homes())
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

/// The existing Munshi-owned configuration at `config_path`, when one is present. Re-registration
/// reads it so sections owned by other commands survive a config rewrite: the explicit
/// disabled-projects list, the self-contained summary-delivery section, and the archive-upload
/// section with the persistent client UUID (issue #31). A version-1 file is converted forward
/// here, so re-registering over an unmigrated configuration also persists it as version 2. The
/// file was already validated as recognized by `validate_owned_file` under the registration lock
/// before this runs.
fn existing_stored_config(config_path: &Path) -> Result<Option<StoredConfig>, RegistrationError> {
    if !config_path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(config_path).map_err(RegistrationError::Io)?;
    parse_stored_config(&bytes)
        .map(Some)
        .map_err(|_| RegistrationError::MalformedOwnedFile)
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
        existing: Option<StoredConfig>,
    ) -> Result<Self, RegistrationError> {
        // Carry sections owned by other commands forward from the existing configuration verbatim
        // (issue #31): a re-register (e.g. to raise policy budgets) must not silently disable a
        // configured summary-delivery sink or archive upload, and the persistent Patwari client
        // UUID must survive because it is the durable identity uploads are keyed under
        // (ADR 0004/0009). Each carried section is self-contained (enablement lives inside it), so
        // nothing else needs to travel with it.
        let (disabled_projects, summary_delivery, archive_upload) = match existing {
            Some(previous) => (
                previous.policy.disabled_projects,
                previous.summary_delivery,
                previous.archive_upload,
            ),
            None => (
                Vec::new(),
                StoredSummaryDelivery::default(),
                StoredArchiveUpload::default(),
            ),
        };
        Ok(Self {
            version: CONFIG_VERSION,
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
                env: config.summarizer_env.iter().cloned().collect(),
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
                max_event_text_bytes: crate::source::DEFAULT_MAX_EVENT_TEXT_BYTES,
                chunk_threshold_bytes: config.chunk_threshold_bytes,
                chunk_size_bytes: config.chunk_size_bytes,
            },
            summary_delivery,
            archive_upload,
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
            summarizer_exhaust: StoredSummarizerExhaust {
                home: config
                    .summarizer_exhaust_home
                    .as_deref()
                    .map(utf8)
                    .transpose()?,
                retention_days: config.summarizer_exhaust_retention_days,
            },
        })
    }
}

trait ManagedFile: Sized + for<'de> Deserialize<'de> {
    /// Parses managed-file bytes, accepting every on-disk shape this file type still supports.
    fn parse(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }

    fn is_recognized(&self) -> bool;
}

/// Parses `config.json` bytes as the current in-memory configuration, dispatching on the recorded
/// schema version: version-1 bytes are converted forward losslessly, everything else must be the
/// current shape.
fn parse_stored_config(bytes: &[u8]) -> Result<StoredConfig, serde_json::Error> {
    if stored_config_version(bytes) == Some(1) {
        serde_json::from_slice::<StoredConfigV1>(bytes).map(StoredConfig::from)
    } else {
        serde_json::from_slice(bytes)
    }
}

impl ManagedFile for StoredConfig {
    fn parse(bytes: &[u8]) -> Option<Self> {
        parse_stored_config(bytes).ok()
    }

    fn is_recognized(&self) -> bool {
        self.version == CONFIG_VERSION
            && self.local_archival_enabled
            && self.transcript_processing_accepted
            && self.project_origin == "agent_stop_cwd"
            && self.policy.max_concurrency >= 1
            && (!self.summary_delivery.enabled || self.summary_delivery.is_addressable())
            && (!self.archive_upload.enabled || self.archive_upload.is_addressable())
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
/// contents. Lets read-only status queries (e.g. `munshi summary-delivery status`) distinguish "never
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
    let bytes = fs::read(&path).map_err(RegistrationError::Io)?;
    let config = parse_stored_config(&bytes).map_err(RegistrationError::Json)?;
    if config.version != CONFIG_VERSION
        || !config.local_archival_enabled
        || !config.transcript_processing_accepted
        || config.project_origin != "agent_stop_cwd"
        || config.policy.max_concurrency < 1
        || (config.summary_delivery.enabled && !config.summary_delivery.is_addressable())
        || (config.archive_upload.enabled && !config.archive_upload.is_addressable())
        || Path::new(&config.state_directory) != state_directory
        || !config.harnesses.paths_are_absolute()
        || !config.summarizer_exhaust.path_is_absolute()
    {
        return Err(RegistrationError::MalformedOwnedFile);
    }
    if stored_config_version(&bytes) == Some(1) {
        // The file still holds the superseded v1 shape: persist the validated migration so the
        // configuration converges on version 2.
        persist_migrated_config(state_directory);
    }
    Ok(config)
}

/// Best-effort persistence of the v1 -> v2 configuration migration under the registration lock —
/// the same locking discipline every other `config.json` write uses, so concurrently running hook
/// workers and the recovery loop only ever observe a complete v1 or v2 file (both loadable).
///
/// If the lock is currently held — including by this very process inside `update_stored_config`,
/// whose own atomic write persists version 2 anyway — the migration is simply skipped; the caller
/// already holds a valid migrated in-memory view, and the next load tries again.
fn persist_migrated_config(state_directory: &Path) {
    let Ok(locks_directory) = ensure_child_directory(state_directory, "locks") else {
        return;
    };
    let Ok(_lock) = acquire_registration_lock(&locks_directory.join(REGISTRATION_LOCK_NAME)) else {
        return;
    };
    // Re-read under the lock: another process may have migrated or rewritten the file since the
    // caller's read.
    let config_path = state_directory.join(CONFIG_FILE_NAME);
    if validate_regular_owned_file(&config_path).is_err() {
        return;
    }
    let Ok(bytes) = fs::read(&config_path) else {
        return;
    };
    if stored_config_version(&bytes) != Some(1) {
        return;
    }
    let Ok(config) = parse_stored_config(&bytes) else {
        return;
    };
    let _ = atomic_json_replace(&config_path, &config);
}

/// The configured per-event extraction threshold for a registered state directory, or the built-in
/// default when the directory holds no readable Munshi registration. Manual archival reads this so
/// it elides oversized events on exactly the same threshold the hook path uses (ADR 0010).
pub fn configured_max_event_text_bytes(state_directory: &Path) -> usize {
    load_stored_config(state_directory)
        .map(|config| config.limits.max_event_text_bytes)
        .unwrap_or(crate::source::DEFAULT_MAX_EVENT_TEXT_BYTES)
}

/// The configured chunked map-reduce threshold for a registered state directory, or the built-in
/// default when the directory holds no readable Munshi registration. Manual archival reads this to
/// validate its own `--max-input-bytes` against the same never-exceed-backstop relation the hook
/// path is registered under (issue #52).
pub fn configured_chunk_threshold_bytes(state_directory: &Path) -> usize {
    load_stored_config(state_directory)
        .map(|config| config.limits.chunk_threshold_bytes)
        .unwrap_or(DEFAULT_CHUNK_THRESHOLD_BYTES)
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
    T: Serialize + ManagedFile,
{
    if path.exists() {
        validate_regular_owned_file(path)?;
        let existing_bytes = fs::read(path).map_err(RegistrationError::Io)?;
        let existing = T::parse(&existing_bytes).ok_or(RegistrationError::MalformedOwnedFile)?;
        if !existing.is_recognized() {
            return Err(RegistrationError::MalformedOwnedFile);
        }
        // Byte-level idempotence: an unchanged file keeps its inode, while a semantically equal
        // file in a superseded on-disk shape (a version-1 config) is still rewritten and thereby
        // migrated forward.
        let mut bytes = serde_json::to_vec_pretty(value).map_err(RegistrationError::Json)?;
        bytes.push(b'\n');
        if existing_bytes == bytes {
            return Ok(());
        }
        return atomic_bytes_replace(path, &bytes);
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
    if let Some(exhaust) = &config.summarizer_exhaust_home {
        paths.push(exhaust);
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

/// Refuses a summarizer exhaust home that overlaps a harness home this registration captures from,
/// or the default `~/.copilot`. Checked before anything is written, so a rejected registration
/// leaves no configuration behind — and so the misconfiguration is reported once, at the moment it
/// is made, rather than only by `munshi doctor` after retention has silently never run.
fn validate_summarizer_exhaust_home(config: &RegisterConfig) -> Result<(), RegistrationError> {
    let Some(home) = config.summarizer_exhaust_home.as_deref() else {
        return Ok(());
    };
    let sources = SourceHomes {
        copilot_home: config.copilot.as_ref().map(|target| target.home.clone()),
        claude_home: config.claude.as_ref().map(|target| target.home.clone()),
    };
    match conflicting_source_home(home, &sources, default_copilot_home().as_deref()) {
        Some(registered) => Err(RegistrationError::SummarizerExhaustOverlap {
            home: home.to_path_buf(),
            registered,
        }),
        None => Ok(()),
    }
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

fn validate_owned_file<T: ManagedFile>(path: &Path) -> Result<(), RegistrationError> {
    recognized_owned_file_exists::<T>(path).map(|_| ())
}

fn recognized_owned_file_exists<T: ManagedFile>(path: &Path) -> Result<bool, RegistrationError> {
    if !path.exists() {
        if fs::symlink_metadata(path).is_ok() {
            return Err(RegistrationError::UnsafePath(path.to_path_buf()));
        }
        return Ok(false);
    }
    validate_regular_owned_file(path)?;
    let bytes = fs::read(path).map_err(RegistrationError::Io)?;
    let value = T::parse(&bytes).ok_or(RegistrationError::MalformedOwnedFile)?;
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
