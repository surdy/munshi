//! Finding the `munshi` executable, and installing the bundled copy onto `PATH`.
//!
//! The app ships the CLI inside its own bundle, but it deliberately *prefers* the copy the user
//! has installed. The installed binary is the one Munshi's harness hooks and the launchd tick
//! actually execute, so reading status from it is the only way the GUI reports what is really
//! happening on the machine rather than what a second, unused copy would say.
//!
//! Installing is therefore a copy to `~/.local/bin`, not a symlink into the bundle. Three reasons:
//! capture keeps working when the app is deleted or moved; on macOS the copy is signed with the
//! app's own stable identity, so TCC grants survive rebuilds instead of being orphaned by a
//! changing cdhash; and the hook paths Munshi writes into the harness config stay valid across
//! app updates.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli;

/// Where the bundled CLI is installed to. `~/.local/bin` is what `contrib/dev-deploy.sh` uses and
/// what the launchd plist references, so the GUI, the hooks and the scheduler agree on one path.
const INSTALL_DIR: &str = ".local/bin";

/// How Munshi was found, so the UI can say which copy it is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    /// `MUNSHI_BIN` pointed at it. Always wins, for development against a working tree.
    Override,
    /// Found at `~/.local/bin/munshi` — the copy hooks and the tick run.
    Installed,
    /// Found on `PATH` somewhere else.
    Path,
    /// Nothing installed; the app fell back to the copy inside its own bundle.
    Bundled,
}

/// What the app knows about the CLI, refreshed on demand and shown in the UI's setup panel.
#[derive(Debug, Clone, Serialize)]
pub struct CliInfo {
    /// The executable every contract is read from, if one was resolved at all.
    pub path: Option<String>,
    /// How that path was found.
    pub origin: Option<Origin>,
    /// `munshi --version` for the resolved binary.
    pub version: Option<String>,
    /// The copy inside this app bundle, when the app is bundled at all (absent in `tauri dev`).
    pub bundled_path: Option<String>,
    /// `munshi --version` for the bundled copy.
    pub bundled_version: Option<String>,
    /// Where `install_cli` would write.
    pub install_target: Option<String>,
    /// Whether `install_target` currently holds a file.
    pub installed: bool,
    /// True when a bundled copy exists and differs in version from the resolved one — the app is
    /// newer (or older) than the binary actually doing the capturing, and the UI offers to sync.
    pub update_available: bool,
    /// Whether `install_target`'s directory is on `PATH`, so a shell would find it after install.
    pub install_dir_on_path: bool,
}

/// Resolves the executable to read contracts from, in the order documented on [`Origin`].
pub fn resolve(bundled: Option<&Path>) -> Option<(PathBuf, Origin)> {
    if let Some(value) = std::env::var_os("MUNSHI_BIN") {
        let path = PathBuf::from(value);
        if is_executable(&path) {
            return Some((path, Origin::Override));
        }
    }

    let installed = install_target();
    if let Some(path) = installed.as_ref().filter(|path| is_executable(path)) {
        return Some((path.clone(), Origin::Installed));
    }

    if let Some(path) = find_on_path("munshi") {
        return Some((path, Origin::Path));
    }

    bundled
        .filter(|path| is_executable(path))
        .map(|path| (path.to_path_buf(), Origin::Bundled))
}

/// Assembles the full picture, including both versions, for the setup panel.
pub fn info(bundled: Option<&Path>) -> CliInfo {
    let resolved = resolve(bundled);
    let bundled_version = bundled.and_then(version_of);
    let resolved_version = resolved.as_ref().and_then(|(path, _)| version_of(path));
    let target = install_target();

    // Only claim an update when both versions are actually known: an unreadable version is a
    // reason to stay quiet, not a reason to nag.
    let update_available = match (&bundled_version, &resolved_version) {
        (Some(bundled), Some(resolved)) => bundled != resolved,
        _ => false,
    };

    CliInfo {
        path: resolved.as_ref().map(|(path, _)| path.display().to_string()),
        origin: resolved.as_ref().map(|(_, origin)| *origin),
        version: resolved_version,
        bundled_path: bundled.map(|path| path.display().to_string()),
        bundled_version,
        install_target: target.as_ref().map(|path| path.display().to_string()),
        installed: target.as_ref().is_some_and(|path| is_executable(path)),
        update_available,
        install_dir_on_path: target
            .as_ref()
            .and_then(|path| path.parent().map(path_contains))
            .unwrap_or(false),
    }
}

/// Copies the bundled CLI to `~/.local/bin/munshi`.
///
/// Writes to a temporary name in the same directory and renames over the target, so a copy that
/// fails half-way cannot leave a truncated binary where the hooks and the tick expect a working
/// one. Renaming also replaces a *running* binary safely, where writing in place would not.
pub fn install_cli(bundled: &Path) -> Result<String, String> {
    if !is_executable(bundled) {
        return Err(format!(
            "the bundled munshi at {} is missing or not executable",
            bundled.display()
        ));
    }
    let target = install_target().ok_or_else(|| "could not determine your home directory".to_string())?;
    let directory = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;

    fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;

    let staging = directory.join(".munshi.install");
    // A leftover from an interrupted install would fail the copy; clear it first.
    let _ = fs::remove_file(&staging);
    fs::copy(bundled, &staging)
        .map_err(|error| format!("could not copy into {}: {error}", directory.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).map_err(|error| {
            let _ = fs::remove_file(&staging);
            format!("could not make {} executable: {error}", staging.display())
        })?;
    }

    fs::rename(&staging, &target).map_err(|error| {
        let _ = fs::remove_file(&staging);
        format!("could not install to {}: {error}", target.display())
    })?;

    Ok(target.display().to_string())
}

/// `~/.local/bin/munshi`.
fn install_target() -> Option<PathBuf> {
    home().map(|home| home.join(INSTALL_DIR).join("munshi"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|path| !path.as_os_str().is_empty())
}

/// `munshi --version`, reduced to its version token. A binary that cannot report a version is
/// treated as unknown rather than broken — it may still serve every contract.
fn version_of(path: &Path) -> Option<String> {
    let stdout = cli::run(path, &["--version"]).ok()?;
    let line = stdout.lines().next()?.trim();
    // clap prints "munshi 0.1.0"; keep the last token so a rename does not break parsing.
    line.split_whitespace().last().map(str::to_string).filter(|token| !token.is_empty())
}

/// Whether `directory` appears in `PATH`.
fn path_contains(directory: &Path) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|entry| entry == directory)
}

/// First executable named `name` on `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

/// An existing regular file carrying an execute bit.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return metadata.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_is_not_an_executable() {
        assert!(!is_executable(Path::new("/tmp")));
        assert!(is_executable(Path::new("/bin/sh")));
    }

    #[test]
    fn version_is_the_last_token_of_the_first_line() {
        // /bin/sh stands in for a binary answering `--version`.
        let temp = std::env::temp_dir().join("munshi-gui-version-probe.sh");
        fs::write(&temp, "#!/bin/sh\necho 'munshi 9.9.9'\n").expect("write probe");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o755)).expect("chmod probe");
        }
        assert_eq!(version_of(&temp).as_deref(), Some("9.9.9"));
        let _ = fs::remove_file(&temp);
    }

    #[test]
    fn a_binary_without_a_version_is_unknown_not_an_error() {
        assert_eq!(version_of(Path::new("/nonexistent/munshi")), None);
    }

    #[test]
    fn install_refuses_a_missing_bundled_binary() {
        let error = install_cli(Path::new("/nonexistent/munshi")).expect_err("should refuse");
        assert!(error.contains("missing or not executable"), "{error}");
    }

    #[test]
    fn find_on_path_locates_a_real_tool() {
        // `sh` is on PATH on every platform this app supports.
        assert!(find_on_path("sh").is_some());
        assert!(find_on_path("munshi-definitely-not-a-real-tool").is_none());
    }
}
