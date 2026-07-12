use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    pub identity: String,
    pub component: String,
    pub project: String,
    pub repository: Option<String>,
    pub branch: Option<String>,
}

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
    })
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
