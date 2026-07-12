use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Nearest-parent project override file name, searched upward from a session's origin directory.
const PROJECT_OVERRIDE_FILE: &str = ".munshi.toml";
/// Bounds the size of a project override file so a runaway or hostile file cannot exhaust memory.
const MAX_OVERRIDE_BYTES: u64 = 64 * 1024;
/// Bounds ancestor traversal so a deeply nested or unusual filesystem cannot loop indefinitely.
const MAX_ANCESTOR_LOOKUPS: usize = 128;

/// Reason a project's processing is currently disabled, used only for safe diagnostic categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledReason {
    /// Explicitly disabled through `munshi project disable`.
    ExplicitlyDisabled,
    /// Disabled through a nearest-parent `.munshi.toml` override.
    ProjectOverride,
    /// A discovered project override file could not be parsed; processing fails closed.
    InvalidProjectOverride,
}

impl DisabledReason {
    pub fn as_category(self) -> &'static str {
        match self {
            Self::ExplicitlyDisabled => "project-disabled",
            Self::ProjectOverride => "project-override-disabled",
            Self::InvalidProjectOverride => "project-override-invalid",
        }
    }
}

/// Global budget and concurrency defaults applied when a project has no narrower override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalPolicy {
    pub max_calls_per_hour: u32,
    pub max_calls_per_day: u32,
    pub max_concurrency: usize,
}

/// The effective policy for one project after merging nearest-parent overrides over global
/// configuration. `max_input_bytes` and `timeout_ms` are `None` unless a project override narrows
/// them; callers fall back to their own configured defaults in that case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedPolicy {
    pub enabled: bool,
    pub disabled_reason: Option<DisabledReason>,
    pub max_calls_per_hour: u32,
    pub max_calls_per_day: u32,
    pub max_input_bytes: Option<usize>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("project override I/O failed")]
    Io(#[source] std::io::Error),
    #[error("project override TOML is malformed")]
    Toml(#[source] toml::de::Error),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideFile {
    #[serde(default)]
    project: OverrideProject,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideProject {
    enabled: Option<bool>,
    max_calls_per_hour: Option<u32>,
    max_calls_per_day: Option<u32>,
    max_input_bytes: Option<usize>,
    timeout_ms: Option<u64>,
}

/// Walks upward from `start_dir` looking for the nearest `.munshi.toml`. Symlinked or oversized
/// candidates are untrusted and skipped rather than followed, and the search continues upward.
fn find_project_override(start_dir: &Path) -> Option<PathBuf> {
    let mut current = Some(start_dir);
    let mut depth = 0;
    while let Some(dir) = current {
        if depth >= MAX_ANCESTOR_LOOKUPS {
            break;
        }
        let candidate = dir.join(PROJECT_OVERRIDE_FILE);
        if let Ok(metadata) = fs::symlink_metadata(&candidate) {
            if metadata.is_file() && metadata.len() <= MAX_OVERRIDE_BYTES {
                return Some(candidate);
            }
        }
        current = dir.parent();
        depth += 1;
    }
    None
}

fn load_override(path: &Path) -> Result<OverrideProject, PolicyError> {
    let bytes = fs::read(path).map_err(PolicyError::Io)?;
    let text = String::from_utf8_lossy(&bytes);
    let file: OverrideFile = toml::from_str(&text).map_err(PolicyError::Toml)?;
    Ok(file.project)
}

/// Resolves the effective policy for a project, applying explicit disablement first, then a
/// nearest-parent project override (if any and parseable), then global configuration.
///
/// A present but unparseable override file fails closed: the project is treated as disabled
/// rather than silently falling back to default-on processing.
pub fn resolve_policy(
    global: &GlobalPolicy,
    disabled_projects: &[String],
    project_identity: &str,
    project_dir: Option<&Path>,
) -> ResolvedPolicy {
    if disabled_projects.iter().any(|id| id == project_identity) {
        return ResolvedPolicy {
            enabled: false,
            disabled_reason: Some(DisabledReason::ExplicitlyDisabled),
            max_calls_per_hour: global.max_calls_per_hour,
            max_calls_per_day: global.max_calls_per_day,
            max_input_bytes: None,
            timeout_ms: None,
        };
    }
    let loaded = project_dir
        .and_then(find_project_override)
        .map(|path| load_override(&path));
    let invalid = matches!(loaded, Some(Err(_)));
    let override_project = loaded.and_then(Result::ok).unwrap_or_default();
    if invalid {
        return ResolvedPolicy {
            enabled: false,
            disabled_reason: Some(DisabledReason::InvalidProjectOverride),
            max_calls_per_hour: global.max_calls_per_hour,
            max_calls_per_day: global.max_calls_per_day,
            max_input_bytes: None,
            timeout_ms: None,
        };
    }
    let enabled = override_project.enabled.unwrap_or(true);
    ResolvedPolicy {
        enabled,
        disabled_reason: (!enabled).then_some(DisabledReason::ProjectOverride),
        max_calls_per_hour: override_project
            .max_calls_per_hour
            .unwrap_or(global.max_calls_per_hour),
        max_calls_per_day: override_project
            .max_calls_per_day
            .unwrap_or(global.max_calls_per_day),
        max_input_bytes: override_project.max_input_bytes,
        timeout_ms: override_project.timeout_ms,
    }
}
