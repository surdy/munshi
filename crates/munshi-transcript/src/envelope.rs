//! Structural, privacy-safe envelope predicates (issue #27): the pure format knowledge
//! behind `munshi`'s transcript validation and Claude Code origin recovery, moved here so
//! transcript-record format knowledge lives in exactly one crate (ADR 0011). Each
//! predicate inspects only version-pinned discriminator keys of an already-parsed record
//! object, never record content; the bounded-I/O wrappers (symlink checks, line caps,
//! bounded reads) remain in `munshi`.

use std::path::Path;

use serde_json::{Map, Value};

use crate::Source;

/// Structural envelope recognition for the first meaningful transcript record.
///
/// Each check is intentionally shallow and privacy-safe: it inspects only the
/// version-pinned discriminator keys, never record content, so a different
/// harness's transcript is rejected before any normalization occurs.
pub fn envelope_matches(source: Source, object: &Map<String, Value>) -> bool {
    match source {
        Source::Copilot => {
            object.get("id").is_some_and(Value::is_string)
                && object.contains_key("timestamp")
                && object.contains_key("parentId")
                && object.get("type").is_some_and(Value::is_string)
                && object.get("data").is_some_and(Value::is_object)
        }
        Source::ClaudeCode => {
            let has_type = object.get("type").is_some_and(Value::is_string);
            let claude_shaped = object.get("message").is_some_and(Value::is_object)
                || object.contains_key("leafUuid")
                || object.contains_key("sessionId")
                || object.contains_key("uuid");
            has_type && claude_shaped && !object.contains_key("payload")
        }
        Source::Codex => {
            object.get("type").is_some_and(Value::is_string)
                && object.contains_key("timestamp")
                && object.contains_key("payload")
        }
    }
}

/// The origin project directory a Claude Code transcript record declares: its top-level
/// `cwd` value, when present and an absolute path. Only the pinned `cwd` key is
/// inspected — mirroring the envelope-recognition read discipline — and record content is
/// never read.
pub fn claude_origin_cwd(object: &Map<String, Value>) -> Option<&str> {
    let cwd = object.get("cwd")?.as_str()?;
    Path::new(cwd).is_absolute().then_some(cwd)
}

/// The git branch a Claude Code transcript record declares: its top-level `gitBranch`
/// value, when present and non-empty. Same read discipline as [`claude_origin_cwd`] —
/// only the pinned key is inspected, record content is never read. Recorded alongside
/// `cwd` on every turn record, it is the branch evidence the recorded-origin fallback
/// (issue #40) carries into provenance when the origin directory no longer exists.
pub fn claude_git_branch(object: &Map<String, Value>) -> Option<&str> {
    let branch = object.get("gitBranch")?.as_str()?;
    (!branch.is_empty()).then_some(branch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(json: &str) -> Map<String, Value> {
        serde_json::from_str::<Value>(json)
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn envelopes_are_mutually_exclusive_on_representative_first_records() {
        let copilot = object(
            r#"{"id":"e1","timestamp":"2026-07-25T00:00:00Z","parentId":null,
                "type":"session.start","data":{}}"#,
        );
        let claude = object(r#"{"type":"summary","summary":"s","leafUuid":"u"}"#);
        let codex = object(r#"{"type":"session_meta","timestamp":"t","payload":{}}"#);

        assert!(envelope_matches(Source::Copilot, &copilot));
        assert!(!envelope_matches(Source::Copilot, &claude));
        assert!(!envelope_matches(Source::Copilot, &codex));

        assert!(envelope_matches(Source::ClaudeCode, &claude));
        assert!(!envelope_matches(Source::ClaudeCode, &codex));

        assert!(envelope_matches(Source::Codex, &codex));
        assert!(!envelope_matches(Source::Codex, &claude));
    }

    #[test]
    fn claude_origin_requires_an_absolute_cwd_string() {
        assert_eq!(
            claude_origin_cwd(&object(r#"{"cwd":"/home/user/project"}"#)),
            Some("/home/user/project")
        );
        assert_eq!(
            claude_origin_cwd(&object(r#"{"cwd":"relative/path"}"#)),
            None
        );
        assert_eq!(claude_origin_cwd(&object(r#"{"cwd":7}"#)), None);
        assert_eq!(claude_origin_cwd(&object(r#"{"type":"user"}"#)), None);
    }

    #[test]
    fn claude_git_branch_requires_a_non_empty_string() {
        assert_eq!(
            claude_git_branch(&object(r#"{"gitBranch":"main"}"#)),
            Some("main")
        );
        assert_eq!(claude_git_branch(&object(r#"{"gitBranch":""}"#)), None);
        assert_eq!(claude_git_branch(&object(r#"{"gitBranch":7}"#)), None);
        assert_eq!(claude_git_branch(&object(r#"{"type":"user"}"#)), None);
    }
}
