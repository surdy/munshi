//! Integration coverage for claim tickets and the snapshot artifact index (issue #21, ADR 0010).
//!
//! These tests drive the real normalization, summary-input, and render/parse paths end to end to
//! prove three properties: an elided event's claim ticket in summarizer input, the frontmatter
//! artifact index, and the `outputs/<sha256>` set `assemble_artifact_sources` uploads all agree on
//! the same content addresses; and a ticketed summary renders with every index value derived from
//! local extraction alone, so it is complete and deliverable with Patwari unreachable.

use munshi::{
    ArchiveMetadata, ProjectIdentity, SessionReference, SourceKind, StructuredSummary,
    assemble_artifact_sources, build_summary_input, extract_outputs, load_session_update,
    parse_archive_markdown, render_revision_markdown, resolve_session_reference,
    snapshot_artifact_index,
};
use tempfile::TempDir;

const THRESHOLD: usize = 64;

fn copilot_user(text: &str) -> String {
    serde_json::json!({
        "id": "user-record",
        "timestamp": "2026-07-25T00:00:00Z",
        "parentId": "root",
        "type": "user.message",
        "data": { "content": text },
    })
    .to_string()
}

fn copilot_tool_complete(call_id: &str, output: &str) -> String {
    serde_json::json!({
        "id": call_id,
        "timestamp": "2026-07-25T00:00:00Z",
        "parentId": "root",
        "type": "tool.execution_complete",
        "data": { "toolCallId": call_id, "success": true, "result": { "content": output } },
    })
    .to_string()
}

fn project() -> ProjectIdentity {
    ProjectIdentity {
        identity: "github.com/surdy/munshi".to_owned(),
        component: "munshi".to_owned(),
        project: "munshi".to_owned(),
        repository: Some("surdy/munshi".to_owned()),
        branch: None,
        origin: munshi::ProjectOrigin::Live,
    }
}

fn summary() -> StructuredSummary {
    StructuredSummary {
        title: "Ran the build".to_owned(),
        goal: "Build the project.".to_owned(),
        work_completed: vec!["Referenced the elided tool output via its claim ticket.".to_owned()],
        decisions: Vec::new(),
        files_changed: Vec::new(),
        commands_and_validation: Vec::new(),
        open_items: Vec::new(),
        tags: vec!["build".to_owned()],
    }
}

/// Writes a Copilot transcript containing one oversized tool output and loads it with a small
/// extraction threshold so exactly one event is elided into a claim ticket.
fn load_ticketed_session(dir: &TempDir) -> (munshi::NormalizedSession, Vec<u8>) {
    let session_id = "sess-ticket";
    let session_dir = dir.path().join(session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let events_path = session_dir.join("events.jsonl");
    let transcript = format!(
        "{}\n{}\n",
        copilot_user("run the build"),
        copilot_tool_complete("call-1", &"x".repeat(500)),
    )
    .into_bytes();
    std::fs::write(&events_path, &transcript).unwrap();

    let resolved = resolve_session_reference(&SessionReference {
        source: SourceKind::Copilot,
        session_id: Some(session_id.to_owned()),
        events_path: Some(events_path),
        copilot_home: None,
    })
    .unwrap();
    let update = load_session_update(&resolved, 1 << 20, None, THRESHOLD).unwrap();
    (update.session, transcript)
}

#[test]
fn ticket_frontmatter_and_artifact_set_agree_on_the_same_addresses() {
    let dir = TempDir::new().unwrap();
    let (session, transcript) = load_ticketed_session(&dir);

    assert_eq!(
        session.artifact_index.extracted_outputs.len(),
        1,
        "exactly one oversized event is extracted"
    );
    let entry = session.artifact_index.extracted_outputs[0].clone();

    // 1. The summarizer input carries the claim ticket for the elided event.
    let input = build_summary_input(&session, &project(), 1 << 20).unwrap();
    let input_text = String::from_utf8(input).unwrap();
    assert!(
        input_text.contains(&format!(
            "[munshi claim-ticket sha256:{} bytes:{} label:{}]",
            entry.sha256, entry.bytes, entry.label
        )),
        "the summarizer sees the claim ticket in place of the elided content"
    );

    // 2. The rendered frontmatter indexes artifact_set_version, the transcript hash, and the output.
    let metadata = ArchiveMetadata {
        session: &session,
        project: &project(),
    };
    let markdown = render_revision_markdown(&metadata, &summary(), 1, "complete", None);
    let parsed = parse_archive_markdown(&markdown).unwrap();
    assert_eq!(parsed.artifact_set_version, Some(1));
    assert_eq!(
        parsed.transcript_sha256.as_deref(),
        Some(session.source_hash.as_str()),
        "the transcript hash in the index is the normalization read's source hash"
    );
    assert_eq!(parsed.extracted_outputs, vec![entry.clone()]);

    // 3. The uploaded artifact set resolves the same output under outputs/<sha256>.
    let sources = assemble_artifact_sources(
        Some(markdown.into_bytes()),
        Some(transcript),
        SourceKind::Copilot,
        THRESHOLD,
    );
    let output_stem = sources
        .iter()
        .find_map(|source| source.logical_path.strip_prefix("outputs/"))
        .expect("the artifact set carries the extracted output");
    assert_eq!(
        output_stem, entry.sha256,
        "ticket, frontmatter, and artifact set share the one content address"
    );
}

#[test]
fn ticketed_summary_renders_with_patwari_unreachable() {
    let dir = TempDir::new().unwrap();
    let (session, transcript) = load_ticketed_session(&dir);

    // The ordering guarantee: every index value is derived from local extraction of the summarized
    // bytes, so rendering never blocks on — nor even constructs — a Patwari upload. Recomputing the
    // index straight from the transcript bytes yields exactly what the renderer recorded.
    let metadata = ArchiveMetadata {
        session: &session,
        project: &project(),
    };
    let markdown = render_revision_markdown(&metadata, &summary(), 1, "complete", None);
    let parsed = parse_archive_markdown(&markdown).unwrap();

    let local = snapshot_artifact_index(&transcript, SourceKind::Copilot, THRESHOLD);
    assert_eq!(parsed.extracted_outputs, local.extracted_outputs);
    assert_eq!(
        parsed.transcript_sha256.as_deref(),
        Some(session.source_hash.as_str())
    );
    // The extracted-output addresses match a pure local hash of the content, no upload involved.
    let outputs = extract_outputs(&transcript, SourceKind::Copilot, THRESHOLD);
    assert_eq!(outputs.len(), parsed.extracted_outputs.len());
    for (output, entry) in outputs.iter().zip(&parsed.extracted_outputs) {
        assert_eq!(output.sha256, entry.sha256);
    }
}
