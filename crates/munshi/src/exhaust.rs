//! Retention for the isolated summarizer home's exhaust (issue #60).
//!
//! `contrib/copilot-summarizer.sh` points `COPILOT_HOME` at a home Munshi does not capture from,
//! so the summarizer's own sessions never feed back into the pipeline. Every summarization run
//! therefore deposits a complete Copilot session there — a `session-state/<id>/` directory plus
//! rows in the monolithic `session-store.db` — that nothing ever removes.
//!
//! The exhaust is derived byproduct: every summary it produced is persisted as local Markdown, in
//! Notesmith, and in Patwari, and the source transcript lives in its harness home and in Patwari.
//! Age-gated deletion is therefore safe by construction, and this module is the only place in
//! Munshi that deletes anything outside its own state and archive directories. It refuses rather
//! than guesses: the home must be configured, must not overlap any registered harness source home
//! or the default `~/.copilot`, must exist, and no summarization may be in flight.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use thiserror::Error;

use crate::registration::{RegistrationError, load_stored_config};
use crate::source::SourceHomes;
use crate::state::{StateError, StateStore};

/// Directories removed in one retention pass. A tick is a background maintenance sweep that must
/// finish quickly and predictably; 200 directories is roughly a week of a heavy machine's exhaust
/// (the 2026-08-02 measurement was 905 directories in a month) and bounds a single pass's I/O at a
/// few hundred `remove_dir_all` calls. A backlog larger than the bound drains over successive
/// ticks, never in one long stall.
pub const EXHAUST_PRUNE_LIMIT: usize = 200;

/// Recency floor: an entry whose newest file was modified within this window is never pruned,
/// whatever the configured retention is. It backstops the active-processing check for the gap
/// between a summarizer process exiting and Munshi releasing its claim.
pub const EXHAUST_QUIET_PERIOD: Duration = Duration::from_secs(600);

/// Exhaust-home size at which `munshi doctor` warns instead of merely reporting. 1 GiB is well
/// above a correctly pruned home under any sane retention window and well below the 5.6 GB that
/// went unnoticed for a month.
pub const EXHAUST_SIZE_WARN_BYTES: u64 = 1024 * 1024 * 1024;

/// The files making up Copilot's session store, in removal order: the main database first, so an
/// interrupted removal leaves inert sidecars rather than a database missing its write-ahead log.
pub const SESSION_STORE_FILES: [&str; 3] = [
    "session-store.db",
    "session-store.db-wal",
    "session-store.db-shm",
];

const SESSION_STATE_DIRECTORY: &str = "session-state";
const SECONDS_PER_DAY: u64 = 86_400;

/// An active retention configuration: where the exhaust lives and how old an entry must be before
/// it is deleted. Only constructed from a configuration that names a home and a non-zero window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExhaustPolicy {
    /// Absolute path of the isolated summarizer home.
    pub home: PathBuf,
    /// Minimum age of a `session-state/` entry's newest file before it may be deleted.
    pub retention: Duration,
}

impl ExhaustPolicy {
    /// The policy for a configured home and window, or `None` when either is absent — an absent
    /// home or a zero window keeps everything, which is Munshi's behavior without this feature.
    pub(crate) fn new(home: Option<&str>, retention_days: u32) -> Option<Self> {
        let home = home.filter(|value| !value.is_empty())?;
        (retention_days > 0).then(|| Self {
            home: PathBuf::from(home),
            retention: Duration::from_secs(u64::from(retention_days) * SECONDS_PER_DAY),
        })
    }
}

/// How one retention pass ended. Every variant but [`ExhaustStatus::Swept`] is a refusal that did
/// not delete anything, and carries the reason it refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExhaustStatus {
    /// No exhaust home, or a zero retention window: the feature is off and stays silent.
    NotConfigured,
    /// The configured home overlaps a path Munshi captures from. Never pruned, at any age.
    HomeConflict {
        home: PathBuf,
        /// The registered source home (or default `~/.copilot`) the exhaust home overlaps.
        registered: PathBuf,
    },
    /// The configured home does not exist: nothing has been deposited there yet.
    HomeMissing { home: PathBuf },
    /// A summarization claim is live, so the summarizer may be writing into the home right now.
    ProcessingActive,
    /// The pass ran to completion.
    Swept,
}

impl ExhaustStatus {
    /// Stable machine-readable code for the `munshi tick` JSON contract.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotConfigured => "off",
            Self::HomeConflict { .. } => "conflict",
            Self::HomeMissing { .. } => "absent",
            Self::ProcessingActive => "busy",
            Self::Swept => "swept",
        }
    }

    /// The refusal reason, for refusals an operator must act on. `None` for the states a
    /// scheduler sees routinely — unconfigured, not yet created, and busy are all quiet
    /// non-events, while an overlapping home is a misconfiguration that silently disables
    /// retention forever and must be said out loud.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::HomeConflict { home, registered } => Some(format!(
                "summarizer exhaust home {} overlaps the registered source home {}; \
                 retention never deletes inside a captured harness home",
                home.display(),
                registered.display()
            )),
            _ => None,
        }
    }
}

/// The outcome of one retention pass. Counts are zero for every refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExhaustReport {
    pub status: ExhaustStatus,
    /// `session-state/` entries deleted this pass.
    pub pruned_directories: usize,
    /// Bytes in the deleted entries, plus the session store when it was removed.
    pub reclaimed_bytes: u64,
    /// `session-state/` entries left behind: too new, over the per-pass bound, unreadable, or not
    /// a plain directory. Non-zero means the session store is kept.
    pub remaining_directories: usize,
    /// Whether the session store files were removed this pass.
    pub store_removed: bool,
}

impl ExhaustReport {
    fn refused(status: ExhaustStatus) -> Self {
        Self {
            status,
            pruned_directories: 0,
            reclaimed_bytes: 0,
            remaining_directories: 0,
            store_removed: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum ExhaustError {
    #[error("summarizer-exhaust retention could not read the Munshi configuration")]
    Registration(#[from] RegistrationError),
    #[error("summarizer-exhaust retention could not read operational state")]
    State(#[from] StateError),
    #[error("summarizer-exhaust retention I/O failed")]
    Io(#[source] io::Error),
}

/// One retention pass over the configured exhaust home, idempotent and silent when there is
/// nothing to do — the `munshi tick` contract (issue #55).
///
/// Runs before the tick's recovery sweep so it cannot race the summarizer invocations that sweep
/// starts. Each guard returns a refusal report rather than an error: a scheduler fires this
/// forever without conditioning on state.
pub fn prune_summarizer_exhaust(state_directory: &Path) -> Result<ExhaustReport, ExhaustError> {
    let config = load_stored_config(state_directory)?;
    let Some(policy) = config.summarizer_exhaust.policy() else {
        return Ok(ExhaustReport::refused(ExhaustStatus::NotConfigured));
    };
    if let Some(registered) = conflicting_source_home(
        &policy.home,
        &config.harnesses.source_homes(),
        default_copilot_home().as_deref(),
    ) {
        return Ok(ExhaustReport::refused(ExhaustStatus::HomeConflict {
            home: policy.home,
            registered,
        }));
    }
    if !policy.home.is_dir() {
        return Ok(ExhaustReport::refused(ExhaustStatus::HomeMissing {
            home: policy.home,
        }));
    }
    // A live claim means a summarizer process may be writing into the home this instant. Skip the
    // whole pass rather than reason about which entry belongs to it; the next tick tries again.
    if StateStore::open(state_directory)?.count_active_processing()? > 0 {
        return Ok(ExhaustReport::refused(ExhaustStatus::ProcessingActive));
    }
    prune_home(&policy, SystemTime::now())
}

/// The registered source home (or the default `~/.copilot`) that `home` equals, contains, or is
/// contained by. `None` means the exhaust home is genuinely isolated and safe to prune.
///
/// Comparison is lexical over absolute paths, which registration already enforces for every
/// recorded home; it deliberately does not resolve symlinks, so a home that merely points at
/// captured content is still refused on its own path.
pub fn conflicting_source_home(
    home: &Path,
    sources: &SourceHomes,
    default_copilot_home: Option<&Path>,
) -> Option<PathBuf> {
    [
        sources.copilot_home.as_deref(),
        sources.claude_home.as_deref(),
        default_copilot_home,
    ]
    .into_iter()
    .flatten()
    .find(|candidate| home.starts_with(candidate) || candidate.starts_with(home))
    .map(Path::to_path_buf)
}

/// The literal `~/.copilot`, which is refused as an exhaust home whether or not this machine
/// registered Copilot. `None` when `HOME` is unset.
pub fn default_copilot_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".copilot"))
}

/// Total bytes held under `home`, following no symlinks — the size `munshi doctor` reports.
pub fn summarizer_exhaust_bytes(home: &Path) -> io::Result<u64> {
    measure(home).map(|measured| measured.bytes)
}

/// Prunes `<home>/session-state` against `now`, then removes the session store if and only if the
/// directory came out empty. Callers must have cleared every guard first.
fn prune_home(policy: &ExhaustPolicy, now: SystemTime) -> Result<ExhaustReport, ExhaustError> {
    let mut report = ExhaustReport::refused(ExhaustStatus::Swept);
    let retention_cutoff = now
        .checked_sub(policy.retention)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let quiet_cutoff = now
        .checked_sub(EXHAUST_QUIET_PERIOD)
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let session_state = policy.home.join(SESSION_STATE_DIRECTORY);
    let mut entries = match fs::read_dir(&session_state) {
        Ok(reader) => reader
            .collect::<Result<Vec<_>, _>>()
            .map_err(ExhaustError::Io)?,
        // No session-state directory at all is the fully drained state, not a failure.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(ExhaustError::Io(error)),
    };
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        // Only a plain directory is ever a candidate. `DirEntry::file_type` does not follow
        // symlinks, so a symlinked entry is counted as remaining and left untouched.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            report.remaining_directories += 1;
            continue;
        }
        let path = entry.path();
        let Ok(measured) = measure(&path) else {
            // An unreadable entry cannot be aged, so it is never deleted, and it keeps the
            // session store alive by counting as remaining.
            report.remaining_directories += 1;
            continue;
        };
        if measured.newest >= retention_cutoff || measured.newest > quiet_cutoff {
            report.remaining_directories += 1;
            continue;
        }
        if report.pruned_directories >= EXHAUST_PRUNE_LIMIT {
            report.remaining_directories += 1;
            continue;
        }
        fs::remove_dir_all(&path).map_err(ExhaustError::Io)?;
        report.pruned_directories += 1;
        report.reclaimed_bytes += measured.bytes;
    }

    if report.remaining_directories == 0 {
        let (reclaimed, removed) = remove_session_store(&policy.home)?;
        report.reclaimed_bytes += reclaimed;
        report.store_removed = removed;
    }
    Ok(report)
}

/// Removes the session store as one unit.
///
/// Copilot's `session-store.db` is a monolith holding rows for every session the summarizer ever
/// ran: deleting a subset of `session-state/` cannot shrink it, and Copilot exposes no per-session
/// eviction. The store is therefore only ever removed whole, and only once no `session-state/`
/// entry remains — the caller's precondition, which also means no surviving entry is left pointing
/// at rows that vanished.
fn remove_session_store(home: &Path) -> Result<(u64, bool), ExhaustError> {
    let mut reclaimed = 0;
    let mut removed = false;
    for name in SESSION_STORE_FILES {
        let path = home.join(name);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        // A symlink here is somebody else's file; refuse it rather than follow it.
        if !metadata.is_file() {
            continue;
        }
        fs::remove_file(&path).map_err(ExhaustError::Io)?;
        reclaimed += metadata.len();
        removed = true;
    }
    Ok((reclaimed, removed))
}

/// The newest modification time anywhere under `directory` (its own included) and the total bytes
/// it holds. Symlinks are measured as links and never traversed.
struct Measured {
    newest: SystemTime,
    bytes: u64,
}

fn measure(directory: &Path) -> io::Result<Measured> {
    let mut measured = Measured {
        newest: fs::symlink_metadata(directory)?.modified()?,
        bytes: 0,
    };
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let metadata = entry.metadata()?;
            if let Ok(modified) = metadata.modified()
                && modified > measured.newest
            {
                measured.newest = modified;
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else {
                measured.bytes += metadata.len();
            }
        }
    }
    Ok(measured)
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};

    use tempfile::TempDir;

    use super::*;

    /// A `session-state/<name>` entry holding `bytes` bytes, aged to `age`.
    fn session_entry(home: &Path, name: &str, bytes: usize, age: Duration) -> PathBuf {
        let directory = home.join(SESSION_STATE_DIRECTORY).join(name);
        fs::create_dir_all(&directory).unwrap();
        let events = directory.join("events.jsonl");
        fs::write(&events, vec![b'e'; bytes]).unwrap();
        let stamp = SystemTime::now() - age;
        let times = FileTimes::new().set_accessed(stamp).set_modified(stamp);
        File::options()
            .write(true)
            .open(&events)
            .unwrap()
            .set_times(times)
            .unwrap();
        File::open(&directory).unwrap().set_times(times).unwrap();
        directory
    }

    fn store(home: &Path, bytes: usize) {
        for name in SESSION_STORE_FILES {
            fs::write(home.join(name), vec![b's'; bytes]).unwrap();
        }
    }

    fn policy(home: &Path, retention_days: u32) -> ExhaustPolicy {
        ExhaustPolicy::new(Some(home.to_str().unwrap()), retention_days).unwrap()
    }

    #[test]
    fn absent_home_or_zero_window_keeps_everything() {
        assert_eq!(ExhaustPolicy::new(None, 7), None);
        assert_eq!(ExhaustPolicy::new(Some(""), 7), None);
        assert_eq!(ExhaustPolicy::new(Some("/exhaust"), 0), None);
        assert!(ExhaustPolicy::new(Some("/exhaust"), 7).is_some());
    }

    #[test]
    fn retention_boundary_prunes_old_entries_and_keeps_fresh_ones() {
        let root = TempDir::new().unwrap();
        let home = root.path();
        let old = session_entry(home, "old", 100, Duration::from_secs(30 * 86_400));
        let fresh = session_entry(home, "fresh", 100, Duration::from_secs(2 * 86_400));

        let report = prune_home(&policy(home, 7), SystemTime::now()).unwrap();

        assert_eq!(report.status, ExhaustStatus::Swept);
        assert_eq!(report.pruned_directories, 1);
        assert_eq!(report.remaining_directories, 1);
        assert_eq!(report.reclaimed_bytes, 100);
        assert!(!old.exists(), "an entry older than the window is deleted");
        assert!(fresh.is_dir(), "an entry inside the window survives");
    }

    #[test]
    fn a_directory_is_aged_by_its_newest_file() {
        let root = TempDir::new().unwrap();
        let home = root.path();
        let entry = session_entry(home, "resumed", 10, Duration::from_secs(30 * 86_400));
        // A resumed session rewrites one file inside an otherwise ancient directory: the whole
        // entry is young again.
        fs::write(entry.join("state.json"), b"{}").unwrap();

        let report = prune_home(&policy(home, 7), SystemTime::now()).unwrap();

        assert_eq!(report.pruned_directories, 0);
        assert_eq!(report.remaining_directories, 1);
        assert!(entry.is_dir());
    }

    #[test]
    fn the_quiet_period_floor_keeps_entries_touched_minutes_ago() {
        let root = TempDir::new().unwrap();
        let home = root.path();
        let recent = session_entry(home, "recent", 10, Duration::from_secs(60));

        // A one-day window would age this out on the calendar; the recency floor does not.
        let report = prune_home(
            &ExhaustPolicy {
                home: home.to_path_buf(),
                retention: Duration::from_secs(1),
            },
            SystemTime::now(),
        )
        .unwrap();

        assert_eq!(report.pruned_directories, 0);
        assert_eq!(report.remaining_directories, 1);
        assert!(recent.is_dir());
    }

    #[test]
    fn the_session_store_goes_only_when_no_entry_remains() {
        let root = TempDir::new().unwrap();
        let home = root.path();
        session_entry(home, "old", 10, Duration::from_secs(30 * 86_400));
        let kept = session_entry(home, "fresh", 10, Duration::from_secs(60));
        store(home, 1_000);

        let held = prune_home(&policy(home, 7), SystemTime::now()).unwrap();
        assert!(!held.store_removed, "a surviving entry keeps the store");
        assert!(home.join(SESSION_STORE_FILES[0]).is_file());
        assert_eq!(held.reclaimed_bytes, 10);

        fs::remove_dir_all(&kept).unwrap();
        let drained = prune_home(&policy(home, 7), SystemTime::now()).unwrap();
        assert!(drained.store_removed);
        assert_eq!(drained.pruned_directories, 0);
        assert_eq!(drained.reclaimed_bytes, 3_000, "all three store files");
        for name in SESSION_STORE_FILES {
            assert!(!home.join(name).exists(), "{name} is removed as a unit");
        }
    }

    #[test]
    fn a_pass_is_bounded_and_the_backlog_drains_over_later_passes() {
        let root = TempDir::new().unwrap();
        let home = root.path();
        for index in 0..EXHAUST_PRUNE_LIMIT + 3 {
            session_entry(
                home,
                &format!("session-{index:04}"),
                1,
                Duration::from_secs(30 * 86_400),
            );
        }
        store(home, 10);

        let first = prune_home(&policy(home, 7), SystemTime::now()).unwrap();
        assert_eq!(first.pruned_directories, EXHAUST_PRUNE_LIMIT);
        assert_eq!(first.remaining_directories, 3);
        assert!(!first.store_removed, "a bounded pass keeps the store");

        let second = prune_home(&policy(home, 7), SystemTime::now()).unwrap();
        assert_eq!(second.pruned_directories, 3);
        assert_eq!(second.remaining_directories, 0);
        assert!(second.store_removed);
    }

    #[test]
    fn a_home_without_session_state_is_a_quiet_no_op() {
        let root = TempDir::new().unwrap();
        let report = prune_home(&policy(root.path(), 7), SystemTime::now()).unwrap();
        assert_eq!(report.status, ExhaustStatus::Swept);
        assert_eq!(report.pruned_directories, 0);
        assert!(!report.store_removed);
    }

    #[test]
    fn a_symlinked_entry_is_never_followed_or_deleted() {
        let root = TempDir::new().unwrap();
        let home = root.path();
        let outside = root.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious.jsonl"), b"keep me").unwrap();
        fs::create_dir_all(home.join(SESSION_STATE_DIRECTORY)).unwrap();
        std::os::unix::fs::symlink(&outside, home.join(SESSION_STATE_DIRECTORY).join("linked"))
            .unwrap();
        store(home, 10);

        let report = prune_home(&policy(home, 7), SystemTime::now()).unwrap();

        assert_eq!(report.pruned_directories, 0);
        assert_eq!(report.remaining_directories, 1);
        assert!(!report.store_removed);
        assert!(outside.join("precious.jsonl").is_file());
    }

    #[test]
    fn an_overlapping_home_is_refused_from_either_direction() {
        let sources = SourceHomes {
            copilot_home: Some(PathBuf::from("/home/u/.copilot")),
            claude_home: Some(PathBuf::from("/home/u/.claude")),
        };
        let default = PathBuf::from("/home/other/.copilot");

        for candidate in [
            "/home/u/.copilot",
            "/home/u/.copilot/session-state",
            "/home/u/.claude/exhaust",
            "/home/other/.copilot",
        ] {
            assert!(
                conflicting_source_home(Path::new(candidate), &sources, Some(&default)).is_some(),
                "{candidate} overlaps a captured home"
            );
        }
        // A parent of a registered home is refused too: pruning it would delete the home.
        assert_eq!(
            conflicting_source_home(Path::new("/home/u"), &sources, Some(&default)),
            Some(PathBuf::from("/home/u/.copilot"))
        );
        // Sharing a name prefix is not overlap; comparison is by path component.
        assert_eq!(
            conflicting_source_home(
                Path::new("/home/u/.copilot-summarizer"),
                &sources,
                Some(&default)
            ),
            None
        );
    }

    #[test]
    fn measured_size_counts_the_whole_home() {
        let root = TempDir::new().unwrap();
        let home = root.path();
        session_entry(home, "one", 40, Duration::from_secs(60));
        session_entry(home, "two", 60, Duration::from_secs(60));
        store(home, 100);
        assert_eq!(summarizer_exhaust_bytes(home).unwrap(), 400);
    }
}
