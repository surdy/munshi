//! Managed Munshi hook entries inside Claude Code's `~/.claude/settings.json`.
//!
//! Unlike Copilot's dedicated `hooks/munshi.json`, this file is owned by Claude Code and holds
//! unrelated user settings; Claude Code itself rewrites it during normal use (phase-0 finding).
//! Munshi therefore merges: it appends one matcher group per lifecycle event, recognizes only its
//! own strictly-shaped entries on update/removal, and preserves every other key, event, and entry
//! verbatim. `serde_json`'s `preserve_order` feature keeps the user's key order stable across
//! rewrites.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use serde_json::{Map, Value, json};
use tempfile::Builder;

use crate::registration::{
    RegistrationError, utf8, validate_existing_directory_if_present, validate_regular_owned_file,
};

/// Claude Code runs hook commands through the shell, so the fixed timeout rides in the entry and
/// the executable path must be quoted. Two seconds matches the Copilot managed contract; the
/// phase-0 probe confirmed the ingestion transaction fits comfortably.
const HOOK_TIMEOUT_SECONDS: u64 = 2;
/// Claude Code lifecycle events that map onto Munshi's agent-stop / session-end ingestion.
const MANAGED_EVENTS: [(&str, &str); 2] = [("Stop", "agent-stop"), ("SessionEnd", "session-end")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeHookStatus {
    /// Both managed entries are present and match the current executable.
    Installed,
    /// The settings file or the managed entries are absent.
    Missing,
    /// Managed entries exist but do not match the expected contract for this executable.
    Stale,
    /// The settings file cannot be interpreted as a mergeable settings object.
    Foreign,
}

pub(crate) fn munshi_hook_command(
    executable: &Path,
    event: &str,
) -> Result<String, RegistrationError> {
    let executable = utf8(executable)?;
    Ok(format!(
        "{} hook {event} --source claude-code",
        shell_quote(&executable)
    ))
}

/// Validates that the settings file (if present) is safe and mergeable, without writing anything.
/// Lets `register` reject a foreign or unsafe file before it commits configuration.
pub(crate) fn validate_claude_settings(settings_path: &Path) -> Result<(), RegistrationError> {
    load_settings(settings_path).map(|_| ())
}

/// Installs or refreshes Munshi's managed `Stop`/`SessionEnd` entries, preserving everything else
/// in the file. Creates a minimal settings file when none exists.
pub(crate) fn install_claude_hooks(
    settings_path: &Path,
    executable: &Path,
) -> Result<(), RegistrationError> {
    let (mut settings, mode) = load_settings(settings_path)?;
    let hooks = managed_hooks_object(&mut settings)?;
    for (event, hook_event) in MANAGED_EVENTS {
        let command = munshi_hook_command(executable, hook_event)?;
        let entries = managed_event_array(hooks, event)?;
        entries.retain(|entry| !is_munshi_hook_entry(entry));
        entries.push(json!({
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": HOOK_TIMEOUT_SECONDS,
            }]
        }));
    }
    write_settings(settings_path, &settings, mode)
}

/// Removes only Munshi's managed entries, pruning containers they leave empty. Never deletes the
/// file; absent files and files without managed entries are a no-op.
pub(crate) fn remove_claude_hooks(settings_path: &Path) -> Result<(), RegistrationError> {
    if !settings_path.exists() {
        if fs::symlink_metadata(settings_path).is_ok() {
            return Err(RegistrationError::UnsafePath(settings_path.to_path_buf()));
        }
        return Ok(());
    }
    let (mut settings, mode) = load_settings(settings_path)?;
    let mut changed = false;
    if let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) {
        for (event, _) in MANAGED_EVENTS {
            if let Some(entries) = hooks.get_mut(event).and_then(Value::as_array_mut) {
                let before = entries.len();
                entries.retain(|entry| !is_munshi_hook_entry(entry));
                changed |= entries.len() != before;
                if entries.is_empty() {
                    hooks.remove(event);
                }
            }
        }
        if hooks.is_empty() {
            settings.remove("hooks");
            changed = true;
        }
    }
    if changed {
        write_settings(settings_path, &settings, mode)?;
    }
    Ok(())
}

/// Reports the managed-entry state for doctor/configuration-check without modifying anything.
pub fn claude_hooks_status(settings_path: &Path, executable: &Path) -> ClaudeHookStatus {
    if !settings_path.is_file() {
        return ClaudeHookStatus::Missing;
    }
    let Ok(bytes) = fs::read(settings_path) else {
        return ClaudeHookStatus::Foreign;
    };
    let Ok(Value::Object(settings)) = serde_json::from_slice::<Value>(&bytes) else {
        return ClaudeHookStatus::Foreign;
    };
    let mut any = false;
    let mut all_current = true;
    for (event, hook_event) in MANAGED_EVENTS {
        let entries = settings
            .get("hooks")
            .and_then(Value::as_object)
            .and_then(|hooks| hooks.get(event))
            .and_then(Value::as_array);
        let owned: Vec<&Value> = entries
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| is_munshi_hook_entry(entry))
                    .collect()
            })
            .unwrap_or_default();
        if owned.is_empty() {
            all_current = false;
            continue;
        }
        any = true;
        let expected = munshi_hook_command(executable, hook_event).ok();
        let current = owned.len() == 1
            && expected.is_some_and(|command| {
                owned[0]
                    == &json!({
                        "hooks": [{
                            "type": "command",
                            "command": command,
                            "timeout": HOOK_TIMEOUT_SECONDS,
                        }]
                    })
            });
        all_current &= current;
    }
    match (any, all_current) {
        (false, _) => ClaudeHookStatus::Missing,
        (true, true) => ClaudeHookStatus::Installed,
        (true, false) => ClaudeHookStatus::Stale,
    }
}

/// Recognizes an entry Munshi installed: a single command hook whose shell command ends with one
/// of Munshi's managed `hook <event> --source claude-code` invocations. Mirrors the strictness of
/// the Copilot hook-file recognition so foreign entries are never touched.
fn is_munshi_hook_entry(entry: &Value) -> bool {
    let Some(entry) = entry.as_object() else {
        return false;
    };
    let Some(hooks) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    if hooks.len() != 1 {
        return false;
    }
    let Some(hook) = hooks[0].as_object() else {
        return false;
    };
    if hook.get("type").and_then(Value::as_str) != Some("command") {
        return false;
    }
    let Some(command) = hook.get("command").and_then(Value::as_str) else {
        return false;
    };
    MANAGED_EVENTS.iter().any(|(_, hook_event)| {
        command.ends_with(&format!(" hook {hook_event} --source claude-code"))
    })
}

fn load_settings(settings_path: &Path) -> Result<(Map<String, Value>, u32), RegistrationError> {
    let parent = settings_path
        .parent()
        .ok_or_else(|| RegistrationError::UnsafePath(settings_path.to_path_buf()))?;
    validate_existing_directory_if_present(parent)?;
    if !settings_path.exists() {
        if fs::symlink_metadata(settings_path).is_ok() {
            return Err(RegistrationError::UnsafePath(settings_path.to_path_buf()));
        }
        return Ok((Map::new(), 0o600));
    }
    validate_regular_owned_file(settings_path)?;
    let mode = fs::symlink_metadata(settings_path)
        .map_err(RegistrationError::Io)?
        .mode()
        & 0o777;
    let bytes = fs::read(settings_path).map_err(RegistrationError::Io)?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(settings)) => Ok((settings, mode)),
        // This file is Claude Code's, not Munshi's; refusing to touch an uninterpretable file is a
        // foreign-file error, not a malformed-owned-file one.
        _ => Err(RegistrationError::ForeignSettingsUnrecognized(
            settings_path.to_path_buf(),
        )),
    }
}

fn managed_hooks_object(
    settings: &mut Map<String, Value>,
) -> Result<&mut Map<String, Value>, RegistrationError> {
    if !settings.contains_key("hooks") {
        settings.insert("hooks".to_owned(), Value::Object(Map::new()));
    }
    match settings.get_mut("hooks") {
        Some(Value::Object(hooks)) => Ok(hooks),
        _ => Err(RegistrationError::MalformedOwnedFile),
    }
}

fn managed_event_array<'a>(
    hooks: &'a mut Map<String, Value>,
    event: &str,
) -> Result<&'a mut Vec<Value>, RegistrationError> {
    if !hooks.contains_key(event) {
        hooks.insert(event.to_owned(), Value::Array(Vec::new()));
    }
    match hooks.get_mut(event) {
        Some(Value::Array(entries)) => Ok(entries),
        _ => Err(RegistrationError::MalformedOwnedFile),
    }
}

/// Atomic replace that preserves the settings file's existing mode instead of forcing 0600 —
/// the file belongs to Claude Code and Munshi must not tighten or loosen it.
fn write_settings(
    settings_path: &Path,
    settings: &Map<String, Value>,
    mode: u32,
) -> Result<(), RegistrationError> {
    let parent = settings_path
        .parent()
        .ok_or_else(|| RegistrationError::UnsafePath(settings_path.to_path_buf()))?;
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(settings.clone()))
        .map_err(RegistrationError::Json)?;
    bytes.push(b'\n');
    let mut temporary = Builder::new()
        .prefix(".munshi-")
        .tempfile_in(parent)
        .map_err(RegistrationError::Io)?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(RegistrationError::Io)?;
    temporary.write_all(&bytes).map_err(RegistrationError::Io)?;
    temporary.flush().map_err(RegistrationError::Io)?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(RegistrationError::Io)?;
    let file = temporary
        .persist(settings_path)
        .map_err(|error| RegistrationError::Io(error.error))?;
    file.sync_all().map_err(RegistrationError::Io)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(RegistrationError::Io)
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting_covers_plain_and_hostile_paths() {
        assert_eq!(
            shell_quote("/usr/local/bin/munshi"),
            "/usr/local/bin/munshi"
        );
        assert_eq!(shell_quote("/tmp/my dir/munshi"), "'/tmp/my dir/munshi'");
        assert_eq!(shell_quote("/tmp/a'b/munshi"), "'/tmp/a'\\''b/munshi'");
    }

    #[test]
    fn recognition_requires_the_managed_suffix_and_single_command_hook() {
        let ours = json!({"hooks": [{"type": "command", "command": "/bin/munshi hook agent-stop --source claude-code", "timeout": 2}]});
        assert!(is_munshi_hook_entry(&ours));
        let foreign =
            json!({"hooks": [{"type": "command", "command": "/bin/other --flag", "timeout": 2}]});
        assert!(!is_munshi_hook_entry(&foreign));
        let multi = json!({"hooks": [
            {"type": "command", "command": "/bin/munshi hook agent-stop --source claude-code"},
            {"type": "command", "command": "/bin/other"}
        ]});
        assert!(!is_munshi_hook_entry(&multi));
    }
}
