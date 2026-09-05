//! The committed summarizer-contract fixtures (issue #91).
//!
//! `docs/summarizers.md` §7 and `docs/troubleshooting.md` tell a reader to pipe a sample request
//! into their summarizer. These tests keep the three copies of that request honest — the envelope
//! Munshi actually serializes, the fixture the docs point at, and the JSON block §2 prints — so a
//! change to the request struct cannot leave the doc telling readers to test against a shape
//! Munshi no longer sends.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use munshi::{
    NormalizedEvent, NormalizedSession, ProjectIdentity, ProjectOrigin, SourceKind,
    StructuredSummary, build_summary_input, validate_structured_summary,
};

const SESSION_ID: &str = "a1b2c3d4-0000-4000-8000-000000000001";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture_path(name: &str) -> PathBuf {
    repo_root().join("fixtures/summarizer").join(name)
}

/// The session and project the committed fixture describes, so the fixture can be checked against
/// a real [`build_summary_input`] call rather than eyeballed.
fn fixture_session() -> (NormalizedSession, ProjectIdentity) {
    let session = NormalizedSession {
        source: SourceKind::ClaudeCode,
        session_id: SESSION_ID.to_owned(),
        events: vec![
            NormalizedEvent {
                kind: "user",
                content: "Add a retry command for failed deliveries.".to_owned(),
            },
            NormalizedEvent {
                kind: "assistant",
                content: "I'll add a `munshi delivery retry` subcommand...".to_owned(),
            },
            NormalizedEvent {
                kind: "tool",
                content: "cargo test -p munshi delivery::retry ... ok".to_owned(),
            },
        ],
        user_requests: 1,
        assistant_messages: 1,
        tool_activities: 1,
        ignored_events: 0,
        source_cursor: 3,
        source_byte_cursor: 0,
        source_prefix_hash: "sha256:0".to_owned(),
        source_hash: "sha256:0".to_owned(),
        source_bytes: 0,
        started_at: None,
        updated_at: None,
        artifact_index: Default::default(),
        opening_summary_request: false,
    };
    let project = ProjectIdentity {
        identity: "github.com/you/your-repo".to_owned(),
        component: "your-repo".to_owned(),
        project: "your-repo".to_owned(),
        repository: Some("you/your-repo".to_owned()),
        branch: Some("main".to_owned()),
        origin: ProjectOrigin::Live,
    };
    (session, project)
}

/// The fenced JSON block of `docs/summarizers.md` §2 ("The input request"), which is the first
/// ```json fence in the document.
fn documented_request_block() -> String {
    let doc = fs::read_to_string(repo_root().join("docs/summarizers.md")).unwrap();
    let after_open = doc
        .split_once("```json\n")
        .expect("docs/summarizers.md has a ```json fence")
        .1;
    let block = after_open
        .split_once("\n```")
        .expect("the §2 json fence is closed")
        .0;
    block.to_owned()
}

#[test]
fn sample_request_fixture_is_the_envelope_munshi_serializes() {
    let (session, project) = fixture_session();
    let built = build_summary_input(&session, &project, usize::MAX).unwrap();

    let built: serde_json::Value = serde_json::from_slice(&built).unwrap();
    let fixture: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture_path("sample-request.json")).unwrap())
            .unwrap();

    assert_eq!(
        fixture, built,
        "fixtures/summarizer/sample-request.json has drifted from the request Munshi builds"
    );
}

#[test]
fn summarizers_doc_shows_the_committed_fixture_verbatim() {
    let fixture = fs::read_to_string(fixture_path("sample-request.json")).unwrap();
    assert_eq!(
        documented_request_block(),
        fixture.trim_end(),
        "docs/summarizers.md §2 and fixtures/summarizer/sample-request.json must be the same text"
    );
}

#[test]
fn no_op_reference_summarizer_answers_the_sample_request() {
    let request = fs::read(fixture_path("sample-request.json")).unwrap();
    let mut child = Command::new(fixture_path("no-op.sh"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&request).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "no-op.sh exited {}", output.status);
    let summary: StructuredSummary = serde_json::from_slice(&output.stdout)
        .expect("no-op.sh must print exactly one StructuredSummary object");
    validate_structured_summary(summary).expect("no-op.sh output must pass Munshi's validation");
}
