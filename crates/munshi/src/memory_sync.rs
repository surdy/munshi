//! Opt-in mirroring of harness auto-memory directories into a Notesmith vault (issue #59).
//!
//! Harness memory (`<claude_home>/projects/<slug>/memory/`) is the distilled, highest
//! value-per-byte artifact the agents produce, and nothing durable captures it: lose the disk,
//! lose the memory. This module mirrors those directories into a dedicated Notesmith document
//! folder — never the fact-memory vault — with the vault's per-vault Git history as the snapshot
//! mechanism, reusing the delivery sink and the issue #9 history machinery wholesale.
//!
//! The seam is deliberate: Munshi collects and delivers; Notesmith stores, versions, and serves.
//! Files are opaque content mirrored verbatim (Anthropic owns the memory format); identity and
//! correlation ride in a per-directory manifest note and the correlated commit message, never
//! injected into the mirrored files. One caveat to "verbatim": Notesmith's own save pipeline
//! normalizes line endings and stamps `created`/`modified` into a note's existing frontmatter on
//! every write — content survives intact, and the manifest records each source file's sha256 so
//! a restore can still verify what it recovered against the original. Collection is strictly read-only — nothing here ever writes
//! into a harness home — and the whole path is strictly downstream of archival (ADR 0006):
//! opt-in, disabled by default, bounded retries into a dead letter, and no failure ever touches
//! sessions or archives.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::delivery::{
    CommitCorrelation, DeliveryCredentialSource, DeliveryError, HistoryGate, HttpNotesmithSink,
    NotesmithSink, SinkError, backoff_ms, commit_and_correlate, normalize_note_path,
    resolve_credential,
};
use crate::http::parse_http_endpoint;
use crate::registration::{
    DEFAULT_MAX_DELIVERY_ATTEMPTS, StoredConfig, StoredMemorySync, load_stored_config,
    stored_config_exists, update_stored_config,
};
use crate::state::{MemorySyncRecord, MemorySyncSuccess, StateError, StateStore};

/// The mirrored source this collector understands today. Harness-neutral by framing (ADR 0008):
/// the feature is "harness memory artifacts" and Claude Code is merely the first source.
const CLAUDE_SOURCE: &str = "claude";

#[derive(Debug, Error)]
pub enum MemorySyncError {
    #[error(transparent)]
    Registration(#[from] crate::registration::RegistrationError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("memory sync I/O failed")]
    Io(#[source] std::io::Error),
    #[error("memory sync is not enabled; run `munshi memory-sync enable`")]
    NotEnabled,
    #[error("memory-sync sink is not configured; run `munshi memory-sync configure`")]
    NotConfigured,
    #[error("memory-sync credential could not be resolved: {0}")]
    Credential(String),
    #[error("memory-sync endpoint {0} is not a supported http URL")]
    UnsupportedEndpoint(String),
}

impl From<DeliveryError> for MemorySyncError {
    fn from(error: DeliveryError) -> Self {
        match error {
            DeliveryError::Registration(inner) => Self::Registration(inner),
            DeliveryError::State(inner) => Self::State(inner),
            DeliveryError::Io(inner) => Self::Io(inner),
            DeliveryError::NotEnabled => Self::NotEnabled,
            DeliveryError::NotConfigured => Self::NotConfigured,
            DeliveryError::Credential(detail) => Self::Credential(detail),
            DeliveryError::UnsupportedEndpoint(endpoint) => Self::UnsupportedEndpoint(endpoint),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The `munshi memory-sync configure` input. `machine_label` is optional: when absent, a default
/// is derived from the hostname exactly once, here at configure time, and persisted — never
/// re-derived per run (issue #59's one-canonical-label rule).
#[derive(Debug, Clone)]
pub struct MemorySinkConfig {
    pub endpoint: String,
    pub vault: String,
    pub folder: Option<String>,
    pub credential: Option<DeliveryCredentialSource>,
    pub max_attempts: Option<u32>,
    pub machine_label: Option<String>,
    pub provision_history: Option<bool>,
}

/// The secret-free settings view reported by configure/enable/status.
#[derive(Debug, Clone, Serialize)]
pub struct MemorySyncSettings {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub vault: Option<String>,
    pub folder: Option<String>,
    pub credential: Option<String>,
    pub max_attempts: u32,
    pub machine_label: Option<String>,
    pub machine_id: Option<String>,
    pub provision_history: bool,
}

impl MemorySyncSettings {
    fn from_config(config: &StoredConfig) -> Self {
        let section = &config.memory_sync;
        Self {
            enabled: section.enabled,
            endpoint: section.endpoint.clone(),
            vault: section.vault.clone(),
            folder: section.folder.clone(),
            credential: section.credential.as_ref().map(describe_credential),
            max_attempts: section.max_attempts,
            machine_label: section.machine_label.clone(),
            machine_id: section.machine_id.clone(),
            provision_history: section.provision_history,
        }
    }

    fn unregistered() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            vault: None,
            folder: None,
            credential: None,
            max_attempts: DEFAULT_MAX_DELIVERY_ATTEMPTS,
            machine_label: None,
            machine_id: None,
            provision_history: false,
        }
    }
}

fn describe_credential(credential: &crate::registration::StoredCredential) -> String {
    match credential {
        crate::registration::StoredCredential::Env { var } => format!("env:{var}"),
        crate::registration::StoredCredential::Keychain { service, account } => {
            format!("keychain:{service}/{account}")
        }
    }
}

/// Configures the memory-sync sink without enabling it. The canonical machine label is resolved
/// here — explicit flag first, else derived from the hostname — and persisted; the
/// `archive_upload` client UUID is captured alongside when that section is configured.
pub fn configure_sink(
    state_directory: &Path,
    sink: MemorySinkConfig,
) -> Result<MemorySyncSettings, MemorySyncError> {
    parse_http_endpoint(&sink.endpoint).map_err(DeliveryError::from)?;
    let label = match sink.machine_label.as_deref().map(sanitize_machine_label) {
        Some(label) if !label.is_empty() => label,
        Some(_) => return Err(MemorySyncError::NotConfigured),
        None => default_machine_label(),
    };
    let (config, ()) = update_stored_config(state_directory, |config| {
        config.memory_sync.endpoint = Some(sink.endpoint.clone());
        config.memory_sync.vault = Some(sink.vault.clone());
        config.memory_sync.folder = sink.folder.clone().filter(|value| !value.is_empty());
        config.memory_sync.credential = sink
            .credential
            .as_ref()
            .map(DeliveryCredentialSource::to_stored);
        config.memory_sync.max_attempts = sink
            .max_attempts
            .filter(|value| *value >= 1)
            .unwrap_or(DEFAULT_MAX_DELIVERY_ATTEMPTS);
        config.memory_sync.machine_label = Some(label.clone());
        // Captured once, at configure time, so the mirror and the Patwari archive of this
        // machine correlate; absent when archive upload has never been configured.
        config.memory_sync.machine_id = config
            .archive_upload
            .client_id
            .clone()
            .filter(|value| !value.is_empty());
        if let Some(provision) = sink.provision_history {
            config.memory_sync.provision_history = provision;
        }
        Ok(())
    })?;
    Ok(MemorySyncSettings::from_config(&config))
}

/// Enables or disables memory sync. Disabling stops future syncs while retaining sync history.
pub fn set_enabled(
    state_directory: &Path,
    enabled: bool,
) -> Result<MemorySyncSettings, MemorySyncError> {
    let result = update_stored_config(state_directory, |config| {
        if enabled && !config.memory_sync.is_addressable() {
            return Err(crate::registration::RegistrationError::MalformedOwnedFile);
        }
        config.memory_sync.enabled = enabled;
        Ok(())
    });
    match result {
        Ok((config, ())) => Ok(MemorySyncSettings::from_config(&config)),
        Err(crate::registration::RegistrationError::MalformedOwnedFile) if enabled => {
            Err(MemorySyncError::NotConfigured)
        }
        Err(error) => Err(error.into()),
    }
}

/// The persisted canonical machine label default: the hostname, sanitized. Derived exactly once
/// (configure time); the prior-art defect this rule exists to avoid is one physical machine
/// mirrored as two because different APIs answered differently on different days.
fn default_machine_label() -> String {
    let hostname = hostname_string();
    let label = sanitize_machine_label(&hostname);
    if label.is_empty() {
        "machine".to_owned()
    } else {
        label
    }
}

/// The machine's hostname as the OS reports it, unsanitized. Shared with capture provenance
/// (`patwari::capture_hostname`) rather than re-derived there: the one-machine-mirrored-as-two
/// defect noted above is exactly what a second, differently-spelled lookup would reintroduce.
pub(crate) fn hostname_string() -> String {
    let mut buffer = [0u8; 256];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return String::new();
    }
    let end = buffer.iter().position(|&byte| byte == 0).unwrap_or(0);
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// Reduces a label to a routing-safe slug: ASCII alphanumerics, `.`, `_`, and `-`; everything
/// else becomes `-`; a trailing `.local` (macOS mDNS suffix) is dropped. Shared with capture
/// provenance so a machine's `hostname` key and its memory-sync label are the same string.
pub(crate) fn sanitize_machine_label(label: &str) -> String {
    let trimmed = label
        .trim()
        .trim_end_matches(".local")
        .trim_matches('.')
        .to_ascii_lowercase();
    trimmed
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Collection (read-only)
// ---------------------------------------------------------------------------

/// One harness memory directory and the manifest of its current contents.
#[derive(Debug, Clone)]
pub struct MemoryDirectory {
    /// The harness's own project slug (the directory name under `projects/`).
    pub slug: String,
    pub path: PathBuf,
    /// Relative path -> (sha256, size in bytes), sorted by path.
    pub files: BTreeMap<String, (String, u64)>,
    /// sha256 over the canonical manifest serialization; the change detector.
    pub manifest_hash: String,
}

/// Collects every non-empty memory directory under the harness home, hashing file contents.
/// Strictly read-only, and never follows the collection outside the memory directory: symlinked
/// entries are skipped so a link cannot pull unrelated (or TCC-protected) content into the mirror.
fn collect_memory_directories(claude_home: &Path) -> Result<Vec<MemoryDirectory>, MemorySyncError> {
    let projects = claude_home.join("projects");
    let mut directories = Vec::new();
    let entries = match fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(directories),
        Err(error) => return Err(MemorySyncError::Io(error)),
    };
    let mut project_names: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(MemorySyncError::Io)?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let memory = entry.path().join("memory");
        if memory.is_dir() && !memory.is_symlink() {
            project_names.push((name, memory));
        }
    }
    project_names.sort();
    for (slug, memory) in project_names {
        let mut files = BTreeMap::new();
        collect_files(&memory, &memory, &mut files)?;
        if files.is_empty() {
            continue;
        }
        let manifest_hash = manifest_hash(&files);
        directories.push(MemoryDirectory {
            slug,
            path: memory,
            files,
            manifest_hash,
        });
    }
    Ok(directories)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, (String, u64)>,
) -> Result<(), MemorySyncError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(MemorySyncError::Io(error)),
    };
    for entry in entries {
        let entry = entry.map_err(MemorySyncError::Io)?;
        let path = entry.path();
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, files)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(relative) = relative.to_str() else {
            continue;
        };
        let bytes = fs::read(&path).map_err(MemorySyncError::Io)?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        files.insert(relative.to_owned(), (digest, bytes.len() as u64));
    }
    Ok(())
}

/// The canonical manifest serialization: one `sha256  bytes  path` line per file, sorted by path
/// (the `BTreeMap` order). Its sha256 is the change detector persisted per directory.
fn manifest_hash(files: &BTreeMap<String, (String, u64)>) -> String {
    let mut canonical = String::new();
    for (path, (digest, size)) in files {
        canonical.push_str(digest);
        canonical.push_str("  ");
        canonical.push_str(&size.to_string());
        canonical.push_str("  ");
        canonical.push_str(path);
        canonical.push('\n');
    }
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// The vault-relative folder one memory directory mirrors into: `[folder/]<machine>/<slug>`.
fn directory_folder(folder: Option<&str>, machine: &str, slug: &str) -> String {
    match folder {
        Some(folder) if !folder.is_empty() => format!("{folder}/{machine}/{slug}"),
        _ => format!("{machine}/{slug}"),
    }
}

/// The manifest note path: a *sibling* of the mirrored tree (`.../<slug>.manifest.md`), so it can
/// never collide with a mirrored file inside the directory.
fn manifest_note_path(folder: Option<&str>, machine: &str, slug: &str) -> String {
    format!("{}.manifest.md", directory_folder(folder, machine, slug))
}

/// The correlated history commit message for one synced revision. Matched exactly (never by
/// prefix) on recovery, like delivery's `munshi: <source>:<session> revision <n>`.
fn commit_message(machine: &str, slug: &str, revision: u64) -> String {
    format!("munshi memory {machine}:{slug} revision {revision}")
}

// ---------------------------------------------------------------------------
// Manifest note
// ---------------------------------------------------------------------------

/// Renders the per-directory manifest note: the identity/correlation carrier the mirrored files
/// deliberately do not embed. Deterministic for a given (config, directory, revision).
fn render_manifest_note(
    section: &StoredMemorySync,
    machine: &str,
    directory: &MemoryDirectory,
    revision: u64,
) -> String {
    let mut note = String::new();
    note.push_str("---\n");
    note.push_str(&format!("munshi_machine: {machine}\n"));
    if let Some(machine_id) = section.machine_id.as_deref() {
        note.push_str(&format!("munshi_machine_id: {machine_id}\n"));
    }
    note.push_str(&format!("munshi_memory_source: {CLAUDE_SOURCE}\n"));
    note.push_str(&format!("munshi_memory_slug: {}\n", directory.slug));
    note.push_str(&format!("munshi_revision: {revision}\n"));
    note.push_str(&format!(
        "munshi_manifest_sha256: {}\n",
        directory.manifest_hash
    ));
    note.push_str("---\n\n");
    note.push_str(&format!(
        "# Memory manifest: {machine}/{}\n\nMirrored verbatim by munshi memory-sync (issue #59). \
         Files no longer listed here have been deleted at the source and remain only in this \
         vault's git history.\n\n| file | sha256 | bytes |\n| --- | --- | --- |\n",
        directory.slug
    ));
    for (path, (digest, size)) in &directory.files {
        note.push_str(&format!("| {path} | {digest} | {size} |\n"));
    }
    note
}

// ---------------------------------------------------------------------------
// Sync orchestration
// ---------------------------------------------------------------------------

/// One directory's outcome in a sync run.
#[derive(Debug, Clone, Serialize)]
pub struct MemorySyncRunItem {
    pub slug: String,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// The `munshi memory-sync run` / tick-drain contract.
#[derive(Debug, Clone, Serialize)]
pub struct MemorySyncRunReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub directories: usize,
    pub unchanged: usize,
    pub synced: usize,
    pub failed: usize,
    pub blocked: usize,
    pub deferred: usize,
    pub items: Vec<MemorySyncRunItem>,
}

impl MemorySyncRunReport {
    fn new(directories: usize) -> Self {
        Self {
            schema_version: 1,
            command: "memory-sync",
            directories,
            unchanged: 0,
            synced: 0,
            failed: 0,
            blocked: 0,
            deferred: 0,
            items: Vec::new(),
        }
    }

    pub fn print_human(&self) {
        if self.synced == 0 && self.failed == 0 && self.blocked == 0 {
            println!(
                "memory-sync: {} directories, all unchanged",
                self.directories
            );
            return;
        }
        for item in &self.items {
            match (&item.code, item.revision) {
                (Some(code), _) => println!("memory-sync: {} {} ({code})", item.slug, item.result),
                (None, Some(revision)) => println!(
                    "memory-sync: {} {} revision {revision}",
                    item.slug, item.result
                ),
                (None, None) => println!("memory-sync: {} {}", item.slug, item.result),
            }
        }
    }
}

/// Runs one sync pass over every collected memory directory.
///
/// Unchanged directories never contact the sink; the network is touched only when at least one
/// directory needs syncing, and the credential and history gate are resolved once for the whole
/// run so every candidate gates consistently. `force` additionally revives dead-letter rows and
/// ignores retry backoff.
pub fn run(state_directory: &Path, force: bool) -> Result<MemorySyncRunReport, MemorySyncError> {
    if !stored_config_exists(state_directory) {
        return Err(MemorySyncError::NotConfigured);
    }
    let config = load_stored_config(state_directory)?;
    if !config.memory_sync.enabled {
        return Err(MemorySyncError::NotEnabled);
    }
    run_with_config(state_directory, &config, force)
}

/// The post-archival trigger (ADR 0006 discipline): quiet no-op when disabled, and every failure
/// is contained to the memory-sync state machine — archival is never affected.
pub(crate) fn sync_after_archive(
    state_directory: &Path,
    config: &StoredConfig,
) -> Result<Option<MemorySyncRunReport>, MemorySyncError> {
    if !config.memory_sync.enabled || !config.memory_sync.is_addressable() {
        return Ok(None);
    }
    run_with_config(state_directory, config, false).map(Some)
}

fn run_with_config(
    state_directory: &Path,
    config: &StoredConfig,
    force: bool,
) -> Result<MemorySyncRunReport, MemorySyncError> {
    let section = &config.memory_sync;
    if !section.is_addressable() {
        return Err(MemorySyncError::NotConfigured);
    }
    let (Some(endpoint), Some(vault), Some(machine)) = (
        section.endpoint.as_deref(),
        section.vault.as_deref(),
        section.machine_label.as_deref(),
    ) else {
        return Err(MemorySyncError::NotConfigured);
    };
    let directories = match config.harnesses.claude_home.as_deref() {
        Some(home) => collect_memory_directories(Path::new(home))?,
        None => Vec::new(),
    };
    let mut report = MemorySyncRunReport::new(directories.len());
    let mut state = StateStore::open(state_directory)?;

    // Decide the work list before any network contact.
    let now = now_ms();
    let mut candidates: Vec<&MemoryDirectory> = Vec::new();
    for directory in &directories {
        let record = state.get_memory_sync(&directory.slug, endpoint, vault)?;
        match eligibility(record.as_ref(), &directory.manifest_hash, now, force) {
            Eligibility::Unchanged => {
                report.unchanged += 1;
            }
            Eligibility::Deferred => {
                report.deferred += 1;
                report.items.push(MemorySyncRunItem {
                    slug: directory.slug.clone(),
                    result: "deferred".to_owned(),
                    code: Some("backoff-pending".to_owned()),
                    revision: None,
                });
            }
            Eligibility::Sync => candidates.push(directory),
        }
    }
    if candidates.is_empty() {
        return Ok(report);
    }

    let token = section
        .credential
        .as_ref()
        .map(|credential| resolve_credential(credential).map_err(MemorySyncError::from))
        .transpose()?;
    let sink = HttpNotesmithSink::connect(endpoint, vault, token).map_err(MemorySyncError::from)?;
    // History is the feature (issue #59), so the gate is unconditionally required.
    let gate = crate::delivery::resolve_history_gate(&sink, true, section.provision_history);

    for directory in candidates {
        state.ensure_memory_sync_target(&directory.slug, endpoint, vault, machine)?;
        if force {
            state.reset_memory_sync_for_retry(&directory.slug, endpoint, vault, true)?;
        }
        if let HistoryGate::Blocked { reason, .. } = &gate {
            // Only the genuinely-absent capability is a configuration gate (attempt-neutral
            // `blocked`); a transport or server error while resolving it is a transient failure
            // that must burn bounded attempts like any other, or an outage would retry forever.
            if reason == "remote-history-unavailable" {
                state.record_memory_sync_blocked(&directory.slug, endpoint, vault, reason)?;
                report.blocked += 1;
                report.items.push(MemorySyncRunItem {
                    slug: directory.slug.clone(),
                    result: "blocked".to_owned(),
                    code: Some(reason.clone()),
                    revision: None,
                });
            } else {
                let backoff = backoff_next(&state, directory, endpoint, vault)?;
                let record = state.record_memory_sync_failure(
                    &directory.slug,
                    endpoint,
                    vault,
                    reason,
                    section.max_attempts,
                    now_ms().saturating_add(backoff),
                )?;
                report.failed += 1;
                report.items.push(MemorySyncRunItem {
                    slug: directory.slug.clone(),
                    result: record.sync_state,
                    code: Some(reason.clone()),
                    revision: None,
                });
            }
            continue;
        }
        match sync_directory(
            &sink, section, machine, directory, &mut state, endpoint, vault,
        ) {
            Ok(record) => {
                report.synced += 1;
                report.items.push(MemorySyncRunItem {
                    slug: directory.slug.clone(),
                    result: "synced".to_owned(),
                    code: None,
                    revision: Some(record.synced_revision),
                });
            }
            Err(category) => {
                let record = state.record_memory_sync_failure(
                    &directory.slug,
                    endpoint,
                    vault,
                    category,
                    section.max_attempts,
                    now_ms().saturating_add(backoff_next(&state, directory, endpoint, vault)?),
                )?;
                report.failed += 1;
                report.items.push(MemorySyncRunItem {
                    slug: directory.slug.clone(),
                    result: record.sync_state,
                    code: Some(category.to_owned()),
                    revision: None,
                });
            }
        }
    }
    Ok(report)
}

fn backoff_next(
    state: &StateStore,
    directory: &MemoryDirectory,
    endpoint: &str,
    vault: &str,
) -> Result<i64, MemorySyncError> {
    let attempts = state
        .get_memory_sync(&directory.slug, endpoint, vault)?
        .map(|record| record.attempts)
        .unwrap_or(0);
    Ok(backoff_ms(attempts.saturating_add(1)))
}

enum Eligibility {
    Unchanged,
    Deferred,
    Sync,
}

/// Decides whether one directory needs syncing this run, before any network contact.
fn eligibility(
    record: Option<&MemorySyncRecord>,
    manifest_hash: &str,
    now: i64,
    force: bool,
) -> Eligibility {
    let Some(record) = record else {
        return Eligibility::Sync;
    };
    let changed = record.manifest_hash.as_deref() != Some(manifest_hash);
    match record.sync_state.as_str() {
        "synced" if !changed => Eligibility::Unchanged,
        "synced" | "pending" | "blocked" => Eligibility::Sync,
        "failed" => {
            if force || record.next_attempt_at_ms.is_none_or(|at| at <= now) {
                Eligibility::Sync
            } else {
                Eligibility::Deferred
            }
        }
        // A dead letter stays parked until an operator revives it with `--force` — the same
        // discipline as delivery, so a persistently failing sink cannot retry forever.
        "dead-letter" => {
            if force {
                Eligibility::Sync
            } else {
                Eligibility::Unchanged
            }
        }
        _ => Eligibility::Sync,
    }
}

/// Mirrors one directory: clean-tree preflight, verbatim file writes, the manifest note, then the
/// correlated history commit. Returns the persisted record on success, or a retry category.
fn sync_directory(
    sink: &dyn NotesmithSink,
    section: &StoredMemorySync,
    machine: &str,
    directory: &MemoryDirectory,
    state: &mut StateStore,
    endpoint: &str,
    vault: &str,
) -> Result<MemorySyncRecord, &'static str> {
    let folder = directory_folder(section.folder.as_deref(), machine, &directory.slug);
    let manifest_path = manifest_note_path(section.folder.as_deref(), machine, &directory.slug);

    // Notesmith commits stage the entire tree, so refuse to bundle unrelated dirty files into
    // the correlated commit. This mirror's own paths are expected to be dirty mid-crash-recovery
    // and are allowed.
    match sink.history_status() {
        Ok(status) if !status.clean => {
            let own_prefix = format!("{folder}/");
            let unrelated = status.dirty_paths.iter().any(|path| {
                let path = normalize_note_path(path);
                !path.starts_with(&own_prefix) && path != normalize_note_path(&manifest_path)
            });
            if unrelated {
                return Err("remote-history-dirty");
            }
        }
        Ok(_) => {}
        Err(error) => return Err(error.category()),
    }

    for relative in directory.files.keys() {
        let content = match fs::read_to_string(directory.path.join(relative)) {
            Ok(content) => content,
            // The file changed underneath us (memory is live); the next pass will catch up.
            Err(_) => return Err("memory-source-changed"),
        };
        let path = format!("{folder}/{relative}");
        if let Err(error) = write_document(sink, &path, &content) {
            return Err(error.category());
        }
    }

    let revision = state
        .get_memory_sync(&directory.slug, endpoint, vault)
        .ok()
        .flatten()
        .map(|record| record.synced_revision)
        .unwrap_or(0)
        .saturating_add(1);
    let manifest = render_manifest_note(section, machine, directory, revision);
    if let Err(error) = write_document(sink, &manifest_path, &manifest) {
        return Err(error.category());
    }

    let message = commit_message(machine, &directory.slug, revision);
    let history_commit = match commit_and_correlate(sink, &message) {
        CommitCorrelation::Committed(commit) => commit.sha,
        CommitCorrelation::Failed { category } => return Err(category),
    };
    state
        .record_memory_sync_success(
            &directory.slug,
            endpoint,
            vault,
            &MemorySyncSuccess {
                manifest_hash: directory.manifest_hash.clone(),
                file_count: directory.files.len() as u64,
                history_commit: Some(history_commit),
            },
        )
        .map_err(|_| "state-write-failed")
}

/// Writes a document at its exact path. Notesmith's `PUT` upserts — the vault engine creates
/// missing parents and files when no expected hash is supplied — so the mirror never needs the
/// create route, whose frontmatter assembly would wrap the verbatim body in a second block.
/// Munshi owns mirrored documents and overwrites remote edits.
fn write_document(sink: &dyn NotesmithSink, path: &str, content: &str) -> Result<(), SinkError> {
    sink.replace_note(path, content).map(|_| ())
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// One recorded directory in the status report.
#[derive(Debug, Clone, Serialize)]
pub struct MemorySyncItem {
    pub slug: String,
    pub machine: String,
    pub state: String,
    pub synced_revision: u64,
    pub file_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_commit: Option<String>,
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_category: Option<String>,
}

/// The `munshi memory-sync status` contract. Never contacts the sink.
#[derive(Debug, Clone, Serialize)]
pub struct MemorySyncStatusReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub registered: bool,
    pub settings: MemorySyncSettings,
    pub synced: usize,
    pub pending: usize,
    pub failed: usize,
    pub dead_letter: usize,
    pub blocked: usize,
    pub items: Vec<MemorySyncItem>,
}

impl MemorySyncStatusReport {
    pub fn print_human(&self) {
        if !self.registered {
            println!("memory-sync: unregistered");
            return;
        }
        let settings = &self.settings;
        println!(
            "memory-sync: {} endpoint={} vault={} machine={}",
            if settings.enabled {
                "enabled"
            } else {
                "disabled"
            },
            settings.endpoint.as_deref().unwrap_or("-"),
            settings.vault.as_deref().unwrap_or("-"),
            settings.machine_label.as_deref().unwrap_or("-"),
        );
        println!(
            "memory-sync: synced={} pending={} failed={} dead-letter={} blocked={}",
            self.synced, self.pending, self.failed, self.dead_letter, self.blocked
        );
        for item in &self.items {
            println!(
                "memory-sync: {} {} revision {} files {}{}",
                item.slug,
                item.state,
                item.synced_revision,
                item.file_count,
                item.last_error_category
                    .as_deref()
                    .map(|category| format!(" ({category})"))
                    .unwrap_or_default(),
            );
        }
    }
}

pub fn status(state_directory: &Path) -> Result<MemorySyncStatusReport, MemorySyncError> {
    if !stored_config_exists(state_directory) {
        return Ok(MemorySyncStatusReport {
            schema_version: 1,
            command: "memory-sync-status",
            registered: false,
            settings: MemorySyncSettings::unregistered(),
            synced: 0,
            pending: 0,
            failed: 0,
            dead_letter: 0,
            blocked: 0,
            items: Vec::new(),
        });
    }
    let config = load_stored_config(state_directory)?;
    let state = StateStore::open(state_directory)?;
    let records = state.list_memory_sync()?;
    let mut report = MemorySyncStatusReport {
        schema_version: 1,
        command: "memory-sync-status",
        registered: true,
        settings: MemorySyncSettings::from_config(&config),
        synced: 0,
        pending: 0,
        failed: 0,
        dead_letter: 0,
        blocked: 0,
        items: Vec::new(),
    };
    for record in records {
        match record.sync_state.as_str() {
            "synced" => report.synced += 1,
            "pending" => report.pending += 1,
            "failed" => report.failed += 1,
            "dead-letter" => report.dead_letter += 1,
            "blocked" => report.blocked += 1,
            _ => {}
        }
        report.items.push(MemorySyncItem {
            slug: record.slug,
            machine: record.machine,
            state: record.sync_state,
            synced_revision: record.synced_revision,
            file_count: record.file_count,
            history_commit: record.history_commit,
            attempts: record.attempts,
            last_error_category: record.last_error_category,
        });
    }
    Ok(report)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_labels_sanitize_to_routing_safe_slugs() {
        assert_eq!(
            sanitize_machine_label("Surdys-MacBook-Pro.local"),
            "surdys-macbook-pro"
        );
        assert_eq!(
            sanitize_machine_label("test's MacBook Pro"),
            "test-s-macbook-pro"
        );
        assert_eq!(sanitize_machine_label("  box.example  "), "box.example");
        assert_eq!(sanitize_machine_label("###"), "---");
    }

    #[test]
    fn manifest_hash_is_stable_and_content_sensitive() {
        let mut files = BTreeMap::new();
        files.insert("MEMORY.md".to_owned(), ("aa".to_owned(), 10));
        files.insert("fact.md".to_owned(), ("bb".to_owned(), 20));
        let first = manifest_hash(&files);
        assert_eq!(first, manifest_hash(&files.clone()));
        files.insert("fact.md".to_owned(), ("cc".to_owned(), 20));
        assert_ne!(first, manifest_hash(&files));
    }

    #[test]
    fn routes_compose_folder_machine_and_slug() {
        assert_eq!(
            directory_folder(Some("memory"), "mbp", "-Users-x-repos-y"),
            "memory/mbp/-Users-x-repos-y"
        );
        assert_eq!(directory_folder(None, "mbp", "s"), "mbp/s");
        assert_eq!(
            manifest_note_path(Some("memory"), "mbp", "s"),
            "memory/mbp/s.manifest.md"
        );
        assert_eq!(
            commit_message("mbp", "-Users-x", 3),
            "munshi memory mbp:-Users-x revision 3"
        );
    }
}
