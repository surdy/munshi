use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use thiserror::Error;

/// The identity a session's project is recorded under, and how that identity was derived.
///
/// Both types live in `munshi-transcript` since issue #79: an archive's frontmatter states them
/// (`project_identity`, `project_origin: "recorded"`), so a reader parsing that frontmatter must
/// be able to name them without depending on this crate. What stays here is the *deriving* —
/// [`inspect_project`] and [`recorded_project_identity`] need git, the filesystem, and a hash of
/// the origin path, none of which a reader has or wants.
pub use munshi_transcript::{ProjectIdentity, ProjectOrigin};

#[derive(Debug, Error)]
pub enum ProjectIdentityError {
    #[error("project directory could not be resolved")]
    InvalidDirectory,
}

pub fn inspect_project(directory: &Path) -> Result<ProjectIdentity, ProjectIdentityError> {
    let canonical = directory
        .canonicalize()
        .map_err(|_| ProjectIdentityError::InvalidDirectory)?;
    if !canonical.is_dir() {
        return Err(ProjectIdentityError::InvalidDirectory);
    }

    let root = git_output(&canonical, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok())
        .unwrap_or(canonical);
    let remote = find_remote(&root);
    let project = remote
        .as_deref()
        .and_then(|identity| identity.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            root.file_name()
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "project".to_owned());
    let identity = remote
        .clone()
        .unwrap_or_else(|| format!("local:sha256:{:x}", Sha256::digest(path_bytes(&root))));
    let repository = remote.as_deref().and_then(repository_name);
    let branch = git_output(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let component = filesystem_component(&project, &identity);

    Ok(ProjectIdentity {
        identity,
        component,
        project,
        repository,
        branch,
        origin: ProjectOrigin::Live,
    })
}

/// Derives a project identity from recorded origin evidence when the origin directory no
/// longer exists on disk (issue #40). This is munshi's existing remote-less identity rule —
/// a stable hash of the root path string, with the same slug-and-digest filesystem component —
/// applied to the recorded path instead of a canonicalized live one, so a session that was
/// previously archived under a remote-less root keeps the same identity and component after
/// the directory is deleted. No filesystem or git inspection happens: the recorded path is
/// taken verbatim (it was recorded canonical by the harness), the branch is the one the
/// transcript recorded, and the repository stays unknown.
pub fn recorded_project_identity(recorded_root: &Path, branch: Option<String>) -> ProjectIdentity {
    let project = recorded_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "project".to_owned());
    let identity = format!(
        "local:sha256:{:x}",
        Sha256::digest(path_bytes(recorded_root))
    );
    let component = filesystem_component(&project, &identity);
    ProjectIdentity {
        identity,
        component,
        project,
        repository: None,
        branch,
        origin: ProjectOrigin::Recorded,
    }
}

/// The human-facing project label for a captured session, derived from the origin evidence the
/// session row records (issue #56). Priority is strict and stops at the first value present:
/// the resolved project name, then the archive path component with its identity digest stripped,
/// then the final segment of the origin working directory. `None` means the session recorded no
/// origin evidence at all — a caller rendering the label chooses its own placeholder.
///
/// Display only, never identity: two unrelated projects whose directories share a basename share
/// a label, which is why routing and archival keep using `ProjectIdentity::identity` and
/// `ProjectIdentity::component` instead.
pub fn project_label(
    project_name: Option<&str>,
    project_component: Option<&str>,
    origin_cwd: Option<&Path>,
) -> Option<String> {
    if let Some(name) = project_name.filter(|name| !name.is_empty()) {
        return Some(name.to_owned());
    }
    if let Some(component) = project_component.filter(|component| !component.is_empty()) {
        return Some(strip_identity_digest(component).to_owned());
    }
    origin_cwd
        .and_then(Path::file_name)
        .and_then(|segment| segment.to_str())
        .map(ToOwned::to_owned)
}

/// Recovers the project slug from a filesystem component by removing the identity digest
/// [`filesystem_component`] appends. Only a trailing hyphen-delimited run of at least eight
/// lowercase hex characters is removed, and only when a non-empty slug remains, so a component
/// that never carried a digest — or that is nothing but one — survives unchanged.
fn strip_identity_digest(component: &str) -> &str {
    match component.rsplit_once('-') {
        Some((slug, digest))
            if !slug.is_empty()
                && digest.len() >= 8
                && digest.chars().all(|character| {
                    character.is_ascii_digit() || matches!(character, 'a'..='f')
                }) =>
        {
            slug
        }
        _ => component,
    }
}

pub fn normalize_git_remote(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/');
    if remote.is_empty() || remote.starts_with('/') || remote.starts_with("file:") {
        return None;
    }

    let (host, path) = if let Some((scheme, remainder)) = remote.split_once("://") {
        if !matches!(scheme, "http" | "https" | "ssh" | "git") {
            return None;
        }
        let (authority, path) = remainder.split_once('/')?;
        let host_port = authority.rsplit('@').next()?;
        let host = host_port
            .strip_suffix(":22")
            .or_else(|| host_port.strip_suffix(":443"))
            .unwrap_or(host_port);
        (host, path)
    } else {
        let (authority, path) = remote.split_once(':')?;
        (authority.rsplit('@').next()?, path)
    };
    let host = host.trim().to_ascii_lowercase();
    let path = path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_matches('/'))
        .trim_matches('/');
    if host.is_empty()
        || path.is_empty()
        || host.contains(['/', '\\'])
        || path.split('/').any(|part| part.is_empty() || part == "..")
    {
        return None;
    }
    Some(format!("{host}/{path}"))
}

fn git_output(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > 8 * 1024 {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn find_remote(directory: &Path) -> Option<String> {
    git_output(directory, &["config", "--get", "remote.origin.url"])
        .and_then(|remote| normalize_git_remote(&remote))
        .or_else(|| {
            let name = git_output(directory, &["remote"])?
                .lines()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .min()?
                .to_owned();
            git_output(directory, &["remote", "get-url", &name])
                .and_then(|remote| normalize_git_remote(&remote))
        })
}

fn repository_name(identity: &str) -> Option<String> {
    identity.split_once('/').map(|(_, path)| path.to_owned())
}

fn filesystem_component(project: &str, identity: &str) -> String {
    let slug: String = project
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "project" } else { slug };
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    format!(
        "{}-{}",
        slug.chars().take(48).collect::<String>(),
        &digest[..12]
    )
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recorded fallback must reproduce the live remote-less rule byte for byte: a
    /// remote-less directory inspected live and the same path derived from recorded evidence
    /// after deletion share one identity and one filesystem component, so the session keeps
    /// archiving into the same place.
    #[test]
    fn recorded_identity_matches_the_live_remote_less_rule_for_the_same_path() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("scratch-project");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let live = inspect_project(&root).unwrap();
        assert_eq!(live.origin, ProjectOrigin::Live);

        let recorded = recorded_project_identity(&root, Some("main".to_owned()));
        assert_eq!(recorded.origin, ProjectOrigin::Recorded);
        assert_eq!(recorded.identity, live.identity);
        assert_eq!(recorded.component, live.component);
        assert_eq!(recorded.project, live.project);
        assert_eq!(recorded.repository, None);
        assert_eq!(recorded.branch.as_deref(), Some("main"));

        // Stability: the same recorded path always yields the same identity, gone or not.
        std::fs::remove_dir_all(&root).unwrap();
        let after_delete = recorded_project_identity(&root, None);
        assert_eq!(after_delete.identity, recorded.identity);
        assert_eq!(after_delete.component, recorded.component);
    }

    /// The display label falls through name, then component, then origin basename, and the
    /// component fallback recovers exactly the slug [`filesystem_component`] built from — never
    /// a truncated real name and never an empty string.
    #[test]
    fn project_label_falls_through_name_component_then_origin_basename() {
        let component = filesystem_component("munshi", "github.com/surdy/munshi");
        assert!(component.starts_with("munshi-"), "{component}");

        assert_eq!(
            project_label(
                Some("munshi"),
                Some(&component),
                Some(Path::new("/tmp/other"))
            ),
            Some("munshi".to_owned())
        );
        assert_eq!(
            project_label(None, Some(&component), Some(Path::new("/tmp/other"))),
            Some("munshi".to_owned())
        );
        assert_eq!(
            project_label(Some(""), Some(""), Some(Path::new("/tmp/other"))),
            Some("other".to_owned()),
            "empty stored strings must fall through, not become the label"
        );
        assert_eq!(project_label(None, None, None), None);

        // A hyphenated slug keeps every segment that is not the digest.
        assert_eq!(
            project_label(None, Some("my-cool-project-0123456789ab"), None),
            Some("my-cool-project".to_owned())
        );
        // Too short, non-hex, and slug-less components are left alone.
        assert_eq!(
            project_label(None, Some("release-2024"), None),
            Some("release-2024".to_owned())
        );
        assert_eq!(
            project_label(None, Some("project-notahexdigest"), None),
            Some("project-notahexdigest".to_owned())
        );
        assert_eq!(
            project_label(None, Some("-0123456789ab"), None),
            Some("-0123456789ab".to_owned())
        );
    }
}
