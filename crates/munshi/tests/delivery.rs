//! Integration coverage for opt-in Notesmith delivery (issue #8).
//!
//! These tests drive the real `munshi` binary and its real minimal HTTP client against an
//! in-process fake Notesmith daemon, exercising create, replace, outage + retry, duplicate
//! prevention, backfill dry run/confirmation, and — crucially — that local archival is fully
//! independent of delivery success.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use munshi::{CompletionReason, SourceKind, StateStore, run_archive_worker_for_source};
use serde_json::{Value, json};
use tempfile::TempDir;

const VAULT: &str = "work";
const TOKEN_ENV: &str = "MUNSHI_TEST_DELIVERY_TOKEN";
const CLAUDE_SESSION: &str = "0c1a0de0-0000-4000-8000-000000000001";

/// A caller that only knows a project directory (e.g. Madari probing before the user has ever run
/// `munshi register` there) must get a valid, empty `schema_version: 1` contract from `delivery
/// status --json`, exactly like `sessions`/`status`/`show`/`retry` already do when unregistered —
/// never a bare, unparseable error string on stdout.
#[test]
fn delivery_status_json_on_an_unregistered_state_directory_degrades_to_empty_contract() {
    let harness = Harness::new();

    let status = harness.delivery_status_json();

    assert_eq!(status["schema_version"], 1);
    assert_eq!(status["command"], "delivery-status");
    assert_eq!(status["settings"]["enabled"], false);
    assert_eq!(status["settings"]["addressable"], false);
    assert_eq!(status["settings"]["endpoint"], Value::Null);
    assert_eq!(status["settings"]["versioned"], false);
    assert_eq!(status["settings"]["provision_history"], false);
    assert_eq!(status["total"], 0);
    assert_eq!(status["delivered"], 0);
    assert_eq!(status["pending"], 0);
    assert_eq!(status["failed"], 0);
    assert_eq!(status["dead_letter"], 0);
    assert_eq!(status["blocked"], 0);
    assert_eq!(status["items"], serde_json::json!([]));
}

#[test]
fn backfill_dry_run_reports_candidates_without_contacting_the_sink() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");

    harness.configure(&server.endpoint());
    harness.enable();

    let dry = harness.delivery_backfill_json(false);
    assert_eq!(dry["command"], "delivery-backfill");
    assert_eq!(dry["confirmed"], false);
    assert_eq!(dry["candidates"], 1);
    assert_eq!(dry["created"], 0);
    assert_eq!(
        server.request_count(),
        0,
        "a dry run must never contact the sink"
    );
}

#[test]
fn backfill_confirm_creates_then_replaces_the_same_note() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    let transcript = harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");

    harness.configure(&server.endpoint());
    harness.enable();

    let run = harness.delivery_backfill_json(true);
    assert_eq!(run["confirmed"], true);
    assert_eq!(run["candidates"], 1);
    assert_eq!(run["created"], 1);
    assert_eq!(
        server.note_count(),
        1,
        "first delivery creates exactly one note"
    );

    let status = harness.delivery_status_json();
    assert_eq!(status["delivered"], 1);
    let item = &status["items"][0];
    assert_eq!(item["state"], "delivered");
    assert_eq!(item["delivered_revision"], 1);
    let note_path = item["note_path"].as_str().unwrap().to_owned();
    let first_body = server.note_body(&note_path).expect("note stored");
    assert!(first_body.contains("Contract summary title"));

    // A newer summary revision replaces the persisted note in place (worker auto-delivers).
    harness.revise_session(SESSION_A, &transcript, "GOAL_TWO", "answer two");
    harness.wait_for_delivered_revision(SESSION_A, 2);

    assert_eq!(
        server.note_count(),
        1,
        "a later revision replaces the same note rather than creating a second"
    );
    let show = harness.show_json(SESSION_A);
    assert_eq!(show["session"]["delivery"]["state"], "delivered");
    assert_eq!(show["session"]["delivery"]["delivered_revision"], 2);
    assert_eq!(
        show["session"]["delivery"]["note_path"],
        Value::String(note_path)
    );
}

#[test]
fn replace_overwrites_remote_edits() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    let transcript = harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();
    assert_eq!(harness.delivery_backfill_json(true)["created"], 1);

    let note_path = harness.delivery_status_json()["items"][0]["note_path"]
        .as_str()
        .unwrap()
        .to_owned();
    // Simulate a remote edit of the Munshi-owned note.
    server.set_note_body(&note_path, "---\ntitle: hand edited\n---\nlocal edit");

    harness.revise_session(SESSION_A, &transcript, "GOAL_TWO", "answer two");
    harness.wait_for_delivered_revision(SESSION_A, 2);

    let body = server.note_body(&note_path).expect("note present");
    assert!(
        !body.contains("hand edited"),
        "Munshi owns delivered notes and overwrites remote edits"
    );
    assert!(body.contains("Contract summary title"));
}

#[test]
fn outage_never_rolls_back_local_archive_and_retry_recovers() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.configure(&server.endpoint());
    harness.enable();

    // Deliver during a total outage: the archive must still succeed locally.
    server.set_outage(true);
    let _ = harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.wait_for_delivery_failed(SESSION_A);

    let show = harness.show_json(SESSION_A);
    assert_eq!(
        show["session"]["state"], "archived",
        "a delivery outage never changes the archived lifecycle"
    );
    let archive_path = show["session"]["archive_path"].as_str().unwrap();
    assert!(
        harness.output.join(archive_path).exists(),
        "the local archive file exists regardless of delivery"
    );
    let delivery = harness.delivery_status_json();
    assert_eq!(delivery["delivered"], 0);
    assert_eq!(delivery["failed"], 1);
    assert_eq!(server.note_count(), 0);

    // Recover the sink and retry: delivery now succeeds without re-summarizing.
    server.set_outage(false);
    let retry = harness.delivery_retry_all_json(false);
    assert_eq!(retry["created"], 1);
    assert_eq!(server.note_count(), 1);
    assert_eq!(harness.delivery_status_json()["delivered"], 1);
}

#[test]
fn repeated_delivery_of_the_same_revision_is_idempotent() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();

    assert_eq!(harness.delivery_backfill_json(true)["created"], 1);
    let after_first = server.request_count();

    // A second confirmed backfill of an unchanged revision creates nothing new.
    let again = harness.delivery_backfill_json(true);
    assert_eq!(again["candidates"], 0);
    assert_eq!(again["created"], 0);
    assert_eq!(server.note_count(), 1);
    assert_eq!(
        server.request_count(),
        after_first,
        "an already-delivered revision does not contact the sink again"
    );
}

#[test]
fn create_conflict_is_adopted_as_a_replace() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();

    // A note already exists at the deterministic path (as after an operational-state rebuild):
    // creation returns 409 and Munshi adopts the note via a replace instead of duplicating it.
    let component = harness.project_component();
    let path = format!("Munshi/{component}/copilot-{SESSION_A}.md");
    server.set_note_body(&path, "---\ntitle: preexisting\n---\nold");

    harness.configure_with_folder(&server.endpoint(), "Munshi");
    let run = harness.delivery_backfill_json(true);
    let delivered = run["created"].as_u64().unwrap() + run["replaced"].as_u64().unwrap();
    assert_eq!(delivered, 1);
    assert_eq!(
        server.note_count(),
        1,
        "the conflicting note is adopted, not duplicated"
    );
    let body = server.note_body(&path).unwrap();
    assert!(body.contains("Contract summary title"));
}

#[test]
fn disabled_project_stops_future_delivery_but_retains_history() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();
    assert_eq!(harness.delivery_backfill_json(true)["created"], 1);

    harness.project_disable();

    // A new revision under a disabled project is not delivered, but existing history remains.
    let status = harness.delivery_status_json();
    assert_eq!(
        status["delivered"], 1,
        "existing delivery history is retained"
    );

    let retry = harness.delivery_backfill_json(true);
    assert_eq!(
        retry["candidates"], 0,
        "a disabled project offers no delivery candidates"
    );
    assert_eq!(server.note_count(), 1);
}

#[test]
fn bearer_token_is_required_and_never_leaked() {
    let token = "s3cr3t-delivery-token-value";
    let harness = Harness::new();
    let server = FakeNotesmith::start_requiring_token(token);
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure_with_credential(&server.endpoint());
    harness.enable();

    // A wrong token is rejected with an exact-match 401; nothing is delivered.
    harness.set_token(Some("wrong-token"));
    let bad = harness.delivery_backfill_json(true);
    assert_eq!(bad["created"], 0);
    assert_eq!(bad["failed"], 1);
    assert!(server.unauthorized_count() >= 1);
    assert_eq!(server.note_count(), 0);

    // The exact bearer token is accepted and the note is delivered.
    harness.set_token(Some(token));
    let good = harness.delivery_retry_all_json(false);
    assert_eq!(good["created"], 1);
    assert_eq!(server.note_count(), 1);
    assert_eq!(
        server.last_auth().as_deref(),
        Some(format!("Bearer {token}").as_str())
    );

    // The credential never appears in operational output or diagnostics.
    assert!(!harness.delivery_status_json().to_string().contains(token));
    assert!(!harness.show_json(SESSION_A).to_string().contains(token));
}

#[test]
fn create_then_replace_keeps_one_frontmatter_block_with_updated_identity() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    let transcript = harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();
    assert_eq!(harness.delivery_backfill_json(true)["created"], 1);

    let note_path = harness.delivery_status_json()["items"][0]["note_path"]
        .as_str()
        .unwrap()
        .to_owned();
    let created = server.note_body(&note_path).unwrap();
    assert_eq!(
        frontmatter_block_count(&created),
        1,
        "create must not stack a second frontmatter block:\n{created}"
    );
    assert!(created.contains("munshi_session: \"11111111"));
    assert!(created.contains("munshi_revision: 1"));
    assert!(created.contains("# Contract summary title"));
    // The archive's own frontmatter must not leak into the delivered note body.
    assert!(!created.contains("schema_version"));

    harness.revise_session(SESSION_A, &transcript, "GOAL_TWO", "answer two");
    harness.wait_for_delivered_revision(SESSION_A, 2);
    let replaced = server.note_body(&note_path).unwrap();
    assert_eq!(
        frontmatter_block_count(&replaced),
        1,
        "replace must write exactly one frontmatter block:\n{replaced}"
    );
    assert!(replaced.contains("munshi_session: \"11111111"));
    assert!(
        replaced.contains("munshi_revision: 2"),
        "munshi_* identity must persist and update on every revision:\n{replaced}"
    );
    assert!(replaced.contains("# Contract summary title"));
}

#[test]
fn backfill_is_idempotent_across_claude_and_copilot_sources() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.archive_claude_session(CLAUDE_SESSION);
    harness.configure(&server.endpoint());
    harness.enable();

    let run = harness.delivery_backfill_json(true);
    assert_eq!(run["candidates"], 2, "both sources are backfill candidates");
    assert_eq!(run["created"], 2);
    assert_eq!(server.note_count(), 2);

    // A second confirmed backfill must be idempotent for BOTH sources. Regression: a Copilot-scoped
    // delivery lookup previously hid Claude/Codex rows and re-delivered them every backfill.
    let again = harness.delivery_backfill_json(true);
    assert_eq!(
        again["candidates"], 0,
        "already-delivered sources are skipped"
    );
    assert_eq!(server.note_count(), 2);
    assert_eq!(harness.delivery_status_json()["delivered"], 2);
}

#[test]
fn retry_all_recovers_a_failed_claude_delivery() {
    let harness = Harness::new();
    let server = FakeNotesmith::start();
    harness.register();
    harness.configure(&server.endpoint());
    harness.enable();

    // The worker auto-delivers the Claude archive during an outage, leaving a failed delivery.
    server.set_outage(true);
    harness.archive_claude_session(CLAUDE_SESSION);
    server.set_outage(false);

    assert_eq!(harness.delivery_status_json()["failed"], 1);

    // Regression: a Copilot-scoped candidate lookup never selected non-Copilot failed rows, so
    // retry could never recover a Claude/Codex delivery.
    let retry = harness.delivery_retry_all_json(false);
    assert_eq!(retry["created"], 1);
    assert_eq!(server.note_count(), 1);
    assert_eq!(harness.delivery_status_json()["delivered"], 1);
}

#[test]
fn reset_delivery_for_retry_is_scoped_to_endpoint_and_vault() {
    let harness = Harness::new();
    harness.register();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");

    let endpoint = "http://127.0.0.1:1";
    let future = 4_000_000_000_000_i64;
    let mut store = harness.open_state();
    store
        .ensure_delivery_target(SESSION_A, endpoint, "vault-a")
        .unwrap();
    store
        .ensure_delivery_target(SESSION_A, endpoint, "vault-b")
        .unwrap();
    store
        .record_delivery_failure(
            SESSION_A,
            endpoint,
            "vault-a",
            "delivery-transport",
            5,
            future,
        )
        .unwrap();
    store
        .record_delivery_failure(
            SESSION_A,
            endpoint,
            "vault-b",
            "delivery-transport",
            5,
            future,
        )
        .unwrap();

    // Resetting one vault must not touch the same endpoint's other vault row.
    store
        .reset_delivery_for_retry(SESSION_A, endpoint, "vault-a", true)
        .unwrap();
    let vault_a = store
        .get_delivery(SESSION_A, endpoint, "vault-a")
        .unwrap()
        .unwrap();
    let vault_b = store
        .get_delivery(SESSION_A, endpoint, "vault-b")
        .unwrap()
        .unwrap();
    assert_eq!(vault_a.delivery_state, "pending");
    assert_eq!(
        vault_b.delivery_state, "failed",
        "the same endpoint's other vault row must be untouched"
    );
}

// ---------------------------------------------------------------------------
// Issue #9: versioned delivery with correlated remote revision history
// ---------------------------------------------------------------------------

#[test]
fn local_git_history_is_independent_when_delivery_is_disabled() {
    // Acceptance: local Git history keeps working when Notesmith is absent / delivery is disabled.
    let harness = Harness::new();
    harness.register_versioned();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");

    let subjects = harness.archive_git_log();
    assert!(
        subjects
            .iter()
            .any(|subject| subject.contains(&format!("copilot:{SESSION_A} revision 1"))),
        "local archive must commit the revision independently; log={subjects:?}"
    );
    // No sink was configured, so no delivery row exists.
    assert_eq!(harness.delivery_status_json()["total"], 0);
}

#[test]
fn versioned_delivery_blocks_when_remote_history_is_unavailable() {
    // Acceptance: versioned delivery blocks (never degrades to latest-only) when the remote cannot
    // preserve correlated history, and the local archive is untouched.
    let harness = Harness::new();
    let server = FakeNotesmith::start(); // git.enabled = false
    harness.register_versioned();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");

    harness.configure(&server.endpoint());
    harness.enable();

    let run = harness.delivery_backfill_json(true);
    assert_eq!(run["blocked"], 1, "run={run}");
    assert_eq!(run["created"], 0);
    assert_eq!(
        server.note_count(),
        0,
        "a blocked versioned delivery must never write a latest-only note"
    );
    assert!(server.commits().is_empty());

    let status = harness.delivery_status_json();
    assert_eq!(status["blocked"], 1);
    let item = &status["items"][0];
    assert_eq!(item["state"], "blocked");
    assert_eq!(item["last_error_category"], "remote-history-unavailable");

    // The local archive committed the revision regardless of the delivery block.
    assert!(
        harness
            .archive_git_log()
            .iter()
            .any(|subject| subject.contains(&format!("copilot:{SESSION_A} revision 1")))
    );

    // `delivery history` reports the capability as unavailable with a non-zero exit.
    let (report, ok) = harness.delivery_history(false);
    assert_eq!(report["status"], "unavailable");
    assert_eq!(report["required"], true);
    assert!(
        !ok,
        "history verify must fail when the capability is absent"
    );
}

#[test]
fn versioned_delivery_commits_correlated_revision_when_capability_present() {
    // Acceptance: delivered revisions carry stable identifiers that correlate local and remote
    // histories, and each revision is preserved as its own remote commit.
    let harness = Harness::new();
    let server = FakeNotesmith::start_with_history(); // git.enabled = true
    harness.register_versioned();
    harness.configure(&server.endpoint());
    harness.enable();
    let transcript = harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.wait_for_delivered_revision(SESSION_A, 1);

    assert_eq!(server.note_count(), 1);
    let commits = server.commits();
    assert_eq!(commits.len(), 1, "one correlated commit per revision");
    assert!(
        commits[0].contains(&format!("copilot:{SESSION_A} revision 1")),
        "remote commit must correlate to the local session/revision; commit={:?}",
        commits[0]
    );

    // The delivery row records the correlated remote history commit.
    let status = harness.delivery_status_json();
    let item = &status["items"][0];
    assert_eq!(item["state"], "delivered");
    assert!(item["history_commit"].is_string());
    // The correlated commit touched only the delivered note (clean-tree preflight held).
    assert_eq!(
        server.commit_files_changed(&format!("munshi: copilot:{SESSION_A} revision 1")),
        Some(1)
    );

    // A second revision preserves a second correlated commit.
    harness.revise_session(SESSION_A, &transcript, "GOAL_TWO", "answer two");
    harness.wait_for_delivered_revision(SESSION_A, 2);
    let commits = server.commits();
    assert_eq!(commits.len(), 2);
    assert!(commits[1].contains(&format!("copilot:{SESSION_A} revision 2")));

    // Local and remote histories share the same source-scoped session/revision identity.
    let local = harness.archive_git_log();
    assert!(
        local
            .iter()
            .any(|subject| subject.contains(&format!("copilot:{SESSION_A} revision 2")))
    );
}

#[test]
fn configuring_remote_history_unblocks_versioned_delivery() {
    // Acceptance: Munshi can explicitly configure the capability; a blocked delivery recovers once
    // the capability is present. Exercises the disabled -> enabled configuration transition.
    let harness = Harness::new();
    let server = FakeNotesmith::start(); // git.enabled = false
    harness.register_versioned();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.configure(&server.endpoint());
    harness.enable();

    // First backfill blocks because remote history is unavailable.
    assert_eq!(harness.delivery_backfill_json(true)["blocked"], 1);
    assert!(!server.git_enabled());

    // Explicitly configure the capability via Munshi; the vault git history is now enabled.
    let (report, ok) = harness.delivery_history(true);
    assert!(ok, "configure should succeed: {report}");
    assert_eq!(report["status"], "configured");
    assert!(server.git_enabled(), "Munshi must have enabled vault git");

    // Retrying the blocked delivery now succeeds and preserves a correlated commit.
    let retry = harness.delivery_retry_all_json(false);
    let created = retry["created"].as_u64().unwrap_or(0);
    let replaced = retry["replaced"].as_u64().unwrap_or(0);
    assert_eq!(created + replaced, 1, "retry={retry}");
    assert_eq!(server.note_count(), 1);
    assert!(
        server.commits()[0].contains(&format!("copilot:{SESSION_A} revision 1")),
        "commits={:?}",
        server.commits()
    );
    let status = harness.delivery_status_json();
    assert_eq!(status["blocked"], 0);
    assert_eq!(status["delivered"], 1);
}

#[test]
fn provisioning_auto_configures_capability_during_delivery() {
    // Acceptance: with provisioning enabled, versioned delivery configures the missing capability
    // itself rather than blocking. Exercises the capability-success path end to end.
    let harness = Harness::new();
    let server = FakeNotesmith::start(); // git.enabled = false initially
    harness.register_versioned();
    harness.configure_with_provision(&server.endpoint());
    harness.enable();
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.wait_for_delivered_revision(SESSION_A, 1);

    assert!(
        server.git_enabled(),
        "provisioning must enable the vault git history"
    );
    assert_eq!(server.note_count(), 1);
    assert!(server.commits()[0].contains(&format!("copilot:{SESSION_A} revision 1")));
}

#[test]
fn later_revision_blocks_without_overwriting_the_delivered_note() {
    // When the remote capability regresses, a new revision is blocked and the previously delivered
    // note is never overwritten with a latest-only copy.
    let harness = Harness::new();
    let server = FakeNotesmith::start_with_history();
    harness.register_versioned();
    harness.configure(&server.endpoint());
    harness.enable();
    let transcript = harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.wait_for_delivered_revision(SESSION_A, 1);
    assert_eq!(server.commits().len(), 1);
    let note_path = harness.delivery_status_json()["items"][0]["note_path"]
        .as_str()
        .unwrap()
        .to_owned();
    let delivered_body = server.note_body(&note_path).unwrap();

    // Regress the capability, then archive a second revision.
    server.set_git_enabled(false);
    harness.revise_session(SESSION_A, &transcript, "GOAL_TWO", "answer two");
    harness.wait_for_delivery_blocked(SESSION_A);

    let item = harness.delivery_status_json()["items"][0].clone();
    assert_eq!(item["state"], "blocked");
    assert_eq!(item["last_error_category"], "remote-history-unavailable");
    assert_eq!(
        item["delivered_revision"], 1,
        "a blocked revision must not advance the delivered revision"
    );
    assert_eq!(
        server.note_body(&note_path).as_deref(),
        Some(delivered_body.as_str()),
        "the delivered note must not be overwritten with a latest-only copy"
    );
    assert_eq!(server.commits().len(), 1, "no new commit while blocked");
}

#[test]
fn versioned_delivery_blocks_when_the_vault_has_unrelated_dirty_changes() {
    // Because Notesmith commits stage the whole working tree, Munshi must not write/commit a
    // revision while unrelated changes are present (they would be bundled into the correlated
    // commit). Delivery blocks with an actionable status; the local archive/Git remain valid.
    let harness = Harness::new();
    let server = FakeNotesmith::start_with_history();
    harness.register_versioned();
    harness.configure(&server.endpoint());
    harness.enable();
    server.add_unrelated_dirty("Journal/unrelated.md");
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.wait_for_delivery_blocked(SESSION_A);

    let item = harness.delivery_status_json()["items"][0].clone();
    assert_eq!(item["state"], "blocked");
    assert_eq!(item["last_error_category"], "remote-history-dirty");
    assert_eq!(
        server.note_count(),
        0,
        "no note may be written while the vault has unrelated dirty changes"
    );
    assert!(
        server.commits().is_empty(),
        "no commit may bundle unrelated changes"
    );
    // The local archive committed the revision regardless of the remote block.
    assert!(
        harness
            .archive_git_log()
            .iter()
            .any(|subject| subject.contains(&format!("copilot:{SESSION_A} revision 1")))
    );

    // Once the operator resolves the unrelated change, a retry delivers and commits cleanly.
    server.clear_unrelated_dirty();
    let retry = harness.delivery_retry_all_json(false);
    let created = retry["created"].as_u64().unwrap_or(0);
    let replaced = retry["replaced"].as_u64().unwrap_or(0);
    assert_eq!(created + replaced, 1, "retry={retry}");
    assert_eq!(server.note_count(), 1);
    assert_eq!(
        server.commit_files_changed(&format!("munshi: copilot:{SESSION_A} revision 1")),
        Some(1),
        "the recovered commit touches only the delivered note"
    );
}

#[test]
fn dropped_commit_response_recovers_the_correlated_commit() {
    // The remote commit lands but its response is lost. Munshi recovers the exact commit by its
    // deterministic correlation message, so exactly one commit exists and its SHA is persisted.
    let harness = Harness::new();
    let server = FakeNotesmith::start_with_history();
    harness.register_versioned();
    harness.configure(&server.endpoint());
    harness.enable();
    // Drop only the commit response; the recovery log lookup still succeeds.
    server.drop_next_commit(0);
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.wait_for_delivered_revision(SESSION_A, 1);

    assert_eq!(
        server.commits().len(),
        1,
        "the dropped response must not produce a duplicate commit"
    );
    let item = harness.delivery_status_json()["items"][0].clone();
    assert_eq!(item["state"], "delivered");
    assert_eq!(
        item["history_commit"], "commit0001",
        "the recovered commit SHA must be persisted"
    );
}

#[test]
fn crash_after_commit_before_db_recovers_sha_on_retry() {
    // Simulates a crash after the remote commit lands but before the operational database records
    // success (the commit response and the recovery lookup are both lost on the first attempt). On
    // retry, the clean idempotent replace yields committed=false and Munshi recovers the existing
    // commit's SHA by exact message — proving one remote commit and a persisted SHA.
    let harness = Harness::new();
    let server = FakeNotesmith::start_with_history();
    harness.register_versioned();
    harness.configure(&server.endpoint());
    harness.enable();
    // Drop the commit response and the immediate recovery lookup, forcing a recorded failure while
    // the commit has actually landed server-side.
    server.drop_next_commit(1);
    harness.archive_session(SESSION_A, "GOAL_ONE", "answer one");
    harness.wait_for_delivery_failed(SESSION_A);
    assert_eq!(
        server.commits().len(),
        1,
        "the commit landed server-side despite the lost response"
    );

    // Retry: the note already matches, so git/commit is a no-op and the SHA is recovered by lookup.
    let retry = harness.delivery_retry_all_json(false);
    let created = retry["created"].as_u64().unwrap_or(0);
    let replaced = retry["replaced"].as_u64().unwrap_or(0);
    assert_eq!(created + replaced, 1, "retry={retry}");
    harness.wait_for_delivered_revision(SESSION_A, 1);

    assert_eq!(
        server.commits().len(),
        1,
        "recovery must not create a second remote commit"
    );
    let item = harness.delivery_status_json()["items"][0].clone();
    assert_eq!(item["state"], "delivered");
    assert_eq!(item["history_commit"], "commit0001");
}

/// Counts the number of complete YAML frontmatter blocks in a note document.
fn frontmatter_block_count(document: &str) -> usize {
    document
        .lines()
        .filter(|line| line.trim_end() == "---")
        .count()
        / 2
}

// ---------------------------------------------------------------------------
// Fake Notesmith daemon
// ---------------------------------------------------------------------------

struct FakeCommit {
    message: String,
    sha: String,
    files_changed: usize,
}

struct FakeState {
    notes: HashMap<String, String>,
    requests: usize,
    unauthorized: usize,
    required_token: Option<String>,
    last_auth: Option<String>,
    /// Whether the vault's per-vault Git revision history is enabled (issue #9 capability).
    git_enabled: bool,
    /// Commits recorded by `git/commit`, newest last — the vault's correlated history.
    commits: Vec<FakeCommit>,
    /// The last committed content per note path, so a note counts as "dirty" until committed.
    committed_notes: HashMap<String, String>,
    /// Injected unrelated dirty working-tree paths (files Munshi does not own).
    extra_dirty: Vec<String>,
    /// When set, the next `git/commit` records its commit but drops the HTTP response (simulating a
    /// lost commit response or a crash between the remote commit and the local database write).
    drop_commit_response_once: bool,
    /// Number of subsequent `git/log` responses to drop (simulating an unreachable lookup).
    drop_log_responses: u32,
    /// Bumped on every config write so the ETag changes, mirroring Notesmith's hash-based ETag.
    config_generation: u64,
}

struct FakeNotesmith {
    port: u16,
    state: Arc<Mutex<FakeState>>,
    outage: Arc<AtomicBool>,
}

impl FakeNotesmith {
    fn start() -> Self {
        Self::start_inner(None, false)
    }

    fn start_requiring_token(token: &str) -> Self {
        Self::start_inner(Some(token.to_owned()), false)
    }

    /// Starts a fake with the vault's revision-history capability already enabled.
    fn start_with_history() -> Self {
        Self::start_inner(None, true)
    }

    fn start_inner(required_token: Option<String>, git_enabled: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(FakeState {
            notes: HashMap::new(),
            requests: 0,
            unauthorized: 0,
            required_token,
            last_auth: None,
            git_enabled,
            commits: Vec::new(),
            committed_notes: HashMap::new(),
            extra_dirty: Vec::new(),
            drop_commit_response_once: false,
            drop_log_responses: 0,
            config_generation: 0,
        }));
        let outage = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_outage = Arc::clone(&outage);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                handle_connection(stream, &thread_state, &thread_outage);
            }
        });
        Self {
            port,
            state,
            outage,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn set_outage(&self, outage: bool) {
        self.outage.store(outage, Ordering::SeqCst);
    }

    fn note_count(&self) -> usize {
        self.state.lock().unwrap().notes.len()
    }

    fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests
    }

    fn unauthorized_count(&self) -> usize {
        self.state.lock().unwrap().unauthorized
    }

    fn last_auth(&self) -> Option<String> {
        self.state.lock().unwrap().last_auth.clone()
    }

    fn note_body(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().notes.get(path).cloned()
    }

    fn set_note_body(&self, path: &str, body: &str) {
        self.state
            .lock()
            .unwrap()
            .notes
            .insert(path.to_owned(), body.to_owned());
    }

    fn git_enabled(&self) -> bool {
        self.state.lock().unwrap().git_enabled
    }

    fn set_git_enabled(&self, enabled: bool) {
        self.state.lock().unwrap().git_enabled = enabled;
    }

    /// Injects an unrelated dirty working-tree path (a file Munshi does not own).
    fn add_unrelated_dirty(&self, path: &str) {
        self.state.lock().unwrap().extra_dirty.push(path.to_owned());
    }

    /// Clears injected unrelated dirty paths, simulating an operator committing or discarding them.
    fn clear_unrelated_dirty(&self) {
        self.state.lock().unwrap().extra_dirty.clear();
    }

    /// Arranges for the next `git/commit` to record its commit but drop the HTTP response, and for
    /// the next `drop_logs` `git/log` lookups to be dropped too — simulating a crash/lost response
    /// after the remote commit lands but before Munshi records success.
    fn drop_next_commit(&self, drop_logs: u32) {
        let mut guard = self.state.lock().unwrap();
        guard.drop_commit_response_once = true;
        guard.drop_log_responses = drop_logs;
    }

    /// The commit messages recorded by `git/commit`, in order — the vault's correlated history.
    fn commits(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .commits
            .iter()
            .map(|commit| commit.message.clone())
            .collect()
    }

    /// The number of files changed by the commit carrying `message`, if any.
    fn commit_files_changed(&self, message: &str) -> Option<usize> {
        self.state
            .lock()
            .unwrap()
            .commits
            .iter()
            .find(|commit| commit.message == message)
            .map(|commit| commit.files_changed)
    }
}

fn handle_connection(
    mut stream: TcpStream,
    state: &Arc<Mutex<FakeState>>,
    outage: &Arc<AtomicBool>,
) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    if outage.load(Ordering::SeqCst) {
        // Simulate an unavailable daemon.
        let _ = stream.write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    let mut guard = state.lock().unwrap();
    guard.last_auth = request.auth.clone();
    // Enforce exact bearer auth when a token is required (reverse-proxy trust model).
    if let Some(required) = guard.required_token.clone() {
        let expected = format!("Bearer {required}");
        if request.auth.as_deref() != Some(expected.as_str()) {
            guard.unauthorized += 1;
            drop(guard);
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        }
    }
    guard.requests += 1;
    let response = route(
        &request.method,
        &request.target,
        &request.body,
        request.if_match.as_deref(),
        &mut guard,
    );
    drop(guard);
    let _ = stream.write_all(response.as_bytes());
}

/// Reproduces Notesmith's create note assembly: a single YAML frontmatter block built from the
/// request's `frontmatter` map, followed by the request `content` (body only).
fn build_note_document(frontmatter: &Value, content: &str) -> String {
    let mut yaml = String::new();
    if let Some(map) = frontmatter.as_object() {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            match &map[key] {
                Value::Number(number) => yaml.push_str(&format!("{key}: {number}\n")),
                value => yaml.push_str(&format!("{key}: \"{}\"\n", value.as_str().unwrap_or(""))),
            }
        }
    }
    format!("---\n{yaml}---\n{content}\n")
}

fn route(
    method: &str,
    target: &str,
    body: &str,
    if_match: Option<&str>,
    state: &mut FakeState,
) -> String {
    // Vault config: exposes and updates the per-vault `git.enabled` revision-history capability.
    let config_target = format!("/api/v/{VAULT}/config");
    if target == config_target {
        let config_value =
            |state: &FakeState| json!({ "name": VAULT, "git": { "enabled": state.git_enabled } });
        if method == "GET" {
            let hash = format!("cfg-{}", state.config_generation);
            return json_response(
                200,
                &json!({
                    "config": config_value(state),
                    "hash": hash,
                    "path": ".notesmith/vault.toml",
                    "warnings": {},
                }),
            );
        }
        if method == "PUT" {
            let current = format!("cfg-{}", state.config_generation);
            match if_match {
                Some(value) if value == current => {}
                Some(_) | None => {
                    return json_response(
                        if if_match.is_none() { 428 } else { 409 },
                        &json!({ "error": "conflict" }),
                    );
                }
            }
            // Notesmith's PUT takes the full config object as the body (not wrapped).
            let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let enabled = payload["git"]["enabled"].as_bool().unwrap_or(false);
            // Enabling git auto-initializes the repository (notes-method routes/config.rs).
            let did_init = enabled && !state.git_enabled;
            state.git_enabled = enabled;
            state.config_generation += 1;
            let hash = format!("cfg-{}", state.config_generation);
            let git_init = if did_init {
                json!({ "initialized": true, "alreadyRepo": false, "sha": "init0000" })
            } else {
                Value::Null
            };
            return json_response(
                200,
                &json!({
                    "config": config_value(state),
                    "hash": hash,
                    "path": ".notesmith/vault.toml",
                    "warnings": {},
                    "gitInit": git_init,
                }),
            );
        }
    }

    // Git working-tree status: notes differing from their last committed content, plus any injected
    // unrelated dirty files. Mirrors notes-method `git/status`.
    if method == "GET" && target == format!("/api/v/{VAULT}/git/status") {
        if !state.git_enabled {
            return json_response(400, &json!({ "error": "vault is not a git repository" }));
        }
        let dirty = fake_dirty_paths(state);
        return json_response(
            200,
            &json!({
                "changed": dirty,
                "staged": [],
                "untracked": [],
                "clean": dirty.is_empty(),
            }),
        );
    }

    // Git log: the vault's commit history, newest first, with per-commit subject and file count.
    if method == "GET" && target.starts_with(&format!("/api/v/{VAULT}/git/log")) {
        if !state.git_enabled {
            return json_response(400, &json!({ "error": "vault is not a git repository" }));
        }
        if state.drop_log_responses > 0 {
            state.drop_log_responses -= 1;
            return String::new();
        }
        let entries: Vec<Value> = state
            .commits
            .iter()
            .rev()
            .map(|commit| {
                json!({
                    "sha": commit.sha,
                    "shortSha": commit.sha,
                    "author": "munshi",
                    "authorEmail": "munshi@localhost",
                    "timestampSecs": 0,
                    "subject": commit.message,
                    "filesChanged": commit.files_changed,
                    "insertions": 0,
                    "deletions": 0,
                })
            })
            .collect();
        return json_response_array(200, &entries);
    }

    // Git commit: stages the *whole* working tree (dirty notes + unrelated files), mirroring
    // notes-method `commit_all`. Requires git.enabled. A no-op returns committed=false/sha=null.
    if method == "POST" && target == format!("/api/v/{VAULT}/git/commit") {
        if !state.git_enabled {
            return json_response(
                400,
                &json!({ "error": "git integration is not enabled for this vault" }),
            );
        }
        let dirty = fake_dirty_paths(state);
        if dirty.is_empty() {
            return json_response(
                200,
                &json!({ "committed": false, "sha": Value::Null, "files": [] }),
            );
        }
        let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let message = payload["message"].as_str().unwrap_or("").to_owned();
        let sha = format!("commit{:04}", state.commits.len() + 1);
        state.commits.push(FakeCommit {
            message,
            sha: sha.clone(),
            files_changed: dirty.len(),
        });
        // The whole working tree is now committed: notes match their content and unrelated files
        // are recorded as committed.
        state.committed_notes = state.notes.clone();
        state.extra_dirty.clear();
        if state.drop_commit_response_once {
            state.drop_commit_response_once = false;
            return String::new();
        }
        return json_response(
            200,
            &json!({ "committed": true, "sha": sha, "files": dirty }),
        );
    }

    // target: /api/v/{vault}/notes  or  /api/v/{vault}/notes/{path...}
    let prefix = format!("/api/v/{VAULT}/notes");
    if method == "POST" && target == prefix {
        let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let folder = payload["folder"].as_str().unwrap_or("");
        let title = payload["title"].as_str().unwrap_or("note");
        // Notesmith assembles the stored document from the body plus a separate frontmatter map,
        // and its save pipeline canonicalizes the note. Munshi's replace document already
        // canonicalizes the body with `trim_end`, so mirror that here: create and replace of the
        // same revision converge to identical bytes (a re-delivery is then a no-op).
        let document = build_note_document(
            &payload["frontmatter"],
            payload["content"].as_str().unwrap_or("").trim_end(),
        );
        let path = if folder.is_empty() {
            format!("{title}.md")
        } else {
            format!("{}/{title}.md", folder.trim_matches('/'))
        };
        if state.notes.contains_key(&path) {
            return json_response(409, &json!({ "error": "exists" }));
        }
        let hash = simple_hash(&document);
        state.notes.insert(path.clone(), document);
        return json_response(201, &json!({ "path": path, "hash": hash }));
    }
    if method == "PUT" && target.starts_with(&format!("{prefix}/")) {
        let path = decode(&target[prefix.len() + 1..]);
        let payload: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        // PUT writes the complete document Munshi sends, verbatim.
        let content = payload["content"].as_str().unwrap_or("").to_owned();
        if !state.notes.contains_key(&path) {
            return json_response(404, &json!({ "error": "not found" }));
        }
        let hash = simple_hash(&content);
        state.notes.insert(path.clone(), content);
        return json_response(200, &json!({ "path": path, "hash": hash }));
    }
    json_response(404, &json!({ "error": "unhandled" }))
}

fn json_response(status: u16, value: &Value) -> String {
    let body = value.to_string();
    let reason = match status {
        200 => "OK",
        201 => "Created",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        428 => "Precondition Required",
        _ => "Status",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn json_response_array(status: u16, entries: &[Value]) -> String {
    json_response(status, &Value::Array(entries.to_vec()))
}

/// The vault's dirty working-tree paths: notes whose content differs from their last committed
/// content, plus any injected unrelated dirty files.
fn fake_dirty_paths(state: &FakeState) -> Vec<String> {
    let mut dirty: Vec<String> = state
        .notes
        .iter()
        .filter(|(path, content)| state.committed_notes.get(*path) != Some(*content))
        .map(|(path, _)| path.clone())
        .collect();
    dirty.extend(state.extra_dirty.iter().cloned());
    dirty.sort();
    dirty.dedup();
    dirty
}

struct FakeRequest {
    method: String,
    target: String,
    body: String,
    auth: Option<String>,
    if_match: Option<String>,
}

fn read_request(stream: &mut TcpStream) -> Option<FakeRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_subsequence(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > 8 * 1024 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut content_length = 0usize;
    let mut auth = None;
    let mut if_match = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        } else if lower.starts_with("authorization:") {
            auth = line
                .split_once(':')
                .map(|(_, value)| value.trim().to_owned());
        } else if lower.starts_with("if-match:") {
            if_match = line
                .split_once(':')
                .map(|(_, value)| value.trim().trim_matches('"').to_owned());
        }
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Some(FakeRequest {
        method,
        target,
        body: String::from_utf8_lossy(&body).into_owned(),
        auth,
        if_match,
    })
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode(value: &str) -> String {
    let mut output = String::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                output.push(byte as char);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index] as char);
        index += 1;
    }
    output
}

fn simple_hash(content: &str) -> String {
    let mut hash: u64 = 1469598103934665603;
    for byte in content.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Test harness (drives the real munshi binary)
// ---------------------------------------------------------------------------

const SESSION_A: &str = "11111111-1111-4111-8111-111111111111";

struct Harness {
    #[allow(dead_code)]
    directory: TempDir,
    copilot_home: PathBuf,
    state: PathBuf,
    output: PathBuf,
    project: PathBuf,
    token: RefCell<Option<String>>,
}

impl Harness {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/munshi-delivery-test-artifacts");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("delivery-case-")
            .tempdir_in(root)
            .unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .arg(&project)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(["remote", "add", "origin", "git@github.com:surdy/munshi.git"])
                .status()
                .unwrap()
                .success()
        );
        let copilot_home = directory.path().join("copilot-home");
        Self {
            state: copilot_home.join("munshi"),
            output: directory.path().join("archives"),
            copilot_home,
            project,
            directory,
            token: RefCell::new(None),
        }
    }

    /// Sets the bearer token the CLI should present, delivered through the credential env var so it
    /// is never written to configuration.
    fn set_token(&self, token: Option<&str>) {
        *self.token.borrow_mut() = token.map(ToOwned::to_owned);
    }

    fn munshi(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_munshi"));
        match self.token.borrow().as_deref() {
            Some(token) => {
                command.env(TOKEN_ENV, token);
            }
            None => {
                command.env_remove(TOKEN_ENV);
            }
        }
        command
    }

    fn register(&self) {
        let output = self
            .munshi()
            .arg("register")
            .arg("--accept-transcript-processing")
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .arg("--output-dir")
            .arg(&self.output)
            .arg("--summarizer")
            .arg(fake("status-contract.sh"))
            .arg("--timeout-ms")
            .arg("5000")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_success(&output);
    }

    /// Registers with local archive Git history enabled, so delivery must be versioned (issue #9).
    fn register_versioned(&self) {
        let output = self
            .munshi()
            .arg("register")
            .arg("--accept-transcript-processing")
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .arg("--output-dir")
            .arg(&self.output)
            .arg("--archive-git-history")
            .arg("--summarizer")
            .arg(fake("status-contract.sh"))
            .arg("--timeout-ms")
            .arg("5000")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn configure(&self, endpoint: &str) {
        self.configure_with_folder(endpoint, "Munshi");
    }

    fn configure_with_folder(&self, endpoint: &str, folder: &str) {
        let output = self
            .munshi()
            .args(["delivery", "configure", "--endpoint"])
            .arg(endpoint)
            .args(["--vault", VAULT, "--folder", folder])
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn configure_with_credential(&self, endpoint: &str) {
        let output = self
            .munshi()
            .args(["delivery", "configure", "--endpoint"])
            .arg(endpoint)
            .args([
                "--vault",
                VAULT,
                "--folder",
                "Munshi",
                "--credential-env",
                TOKEN_ENV,
            ])
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .output()
            .unwrap();
        assert_success(&output);
    }

    /// Configures the sink and opts into Munshi explicitly configuring the remote history
    /// capability when it is absent (issue #9 provisioning).
    fn configure_with_provision(&self, endpoint: &str) {
        let output = self
            .munshi()
            .args(["delivery", "configure", "--endpoint"])
            .arg(endpoint)
            .args([
                "--vault",
                VAULT,
                "--folder",
                "Munshi",
                "--provision-history",
            ])
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .output()
            .unwrap();
        assert_success(&output);
    }

    /// Runs `delivery history`, optionally configuring the capability, returning the JSON contract
    /// and whether the command reported success (exit code zero).
    fn delivery_history(&self, configure: bool) -> (Value, bool) {
        let mut command = self.munshi();
        command.args(["delivery", "history", "--state-dir"]);
        command.arg(self.state_str());
        command.arg("--json");
        if configure {
            command.arg("--configure");
        }
        let output = command.output().unwrap();
        let value = serde_json::from_slice(&output.stdout).expect("valid JSON");
        (value, output.status.success())
    }

    /// Reads the local archive repository's commit subjects (newest first).
    fn archive_git_log(&self) -> Vec<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.output)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    fn enable(&self) {
        let output = self
            .munshi()
            .args(["delivery", "enable"])
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn project_disable(&self) {
        let output = self
            .munshi()
            .args(["project", "disable"])
            .arg(&self.project)
            .arg("--copilot-home")
            .arg(&self.copilot_home)
            .output()
            .unwrap();
        assert_success(&output);
    }

    fn project_component(&self) -> String {
        let show = self.show_json(SESSION_A);
        show["session"]["project"]["component"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn delivery_status_json(&self) -> Value {
        self.json([
            "delivery",
            "status",
            "--state-dir",
            self.state_str(),
            "--json",
        ])
    }

    fn delivery_backfill_json(&self, confirm: bool) -> Value {
        let mut args = vec![
            "delivery".to_owned(),
            "backfill".to_owned(),
            "--state-dir".to_owned(),
            self.state.display().to_string(),
            "--json".to_owned(),
        ];
        if confirm {
            args.push("--confirm".to_owned());
        }
        self.json(args)
    }

    fn delivery_retry_all_json(&self, force: bool) -> Value {
        let mut args = vec![
            "delivery".to_owned(),
            "retry".to_owned(),
            "--all".to_owned(),
            "--state-dir".to_owned(),
            self.state.display().to_string(),
            "--json".to_owned(),
        ];
        if force {
            args.push("--force".to_owned());
        }
        self.json(args)
    }

    fn show_json(&self, session_id: &str) -> Value {
        self.json([
            "show",
            session_id,
            "--state-dir",
            self.state_str(),
            "--json",
        ])
    }

    fn state_str(&self) -> &str {
        self.state.to_str().unwrap()
    }

    fn json<I, S>(&self, args: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = self.munshi().args(args).output().unwrap();
        assert!(
            !output.stdout.is_empty(),
            "empty stdout; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("valid JSON")
    }

    /// Registers, writes a transcript, and drives one full archive lifecycle. Returns the
    /// transcript path so callers can append further turns.
    fn archive_session(&self, session_id: &str, request: &str, answer: &str) -> PathBuf {
        let transcript = self.write_transcript(session_id, request, answer);
        self.agent_stop(session_id, &transcript, 10_000);
        assert_success(&self.hook(
            "session-end",
            &json!({
                "sessionId": session_id,
                "timestamp": 10_001,
                "cwd": self.project,
                "reason": "complete",
            }),
        ));
        assert_success(&self.wait(session_id));
        transcript
    }

    fn agent_stop(&self, session_id: &str, transcript: &Path, timestamp: u64) {
        assert_success(&self.hook(
            "agent-stop",
            &json!({
                "sessionId": session_id,
                "timestamp": timestamp,
                "cwd": self.project,
                "transcriptPath": transcript,
                "stopReason": "end_turn",
            }),
        ));
    }

    fn session_end(&self, session_id: &str, timestamp: u64) {
        assert_success(&self.hook(
            "session-end",
            &json!({
                "sessionId": session_id,
                "timestamp": timestamp,
                "cwd": self.project,
                "reason": "complete",
            }),
        ));
    }

    /// Appends a turn and drives a second full archive lifecycle for an existing session.
    fn revise_session(&self, session_id: &str, transcript: &Path, request: &str, answer: &str) {
        self.append_turn(transcript, request, answer);
        self.agent_stop(session_id, transcript, 20_000);
        self.session_end(session_id, 20_001);
        assert_success(&self.wait(session_id));
    }

    /// Archives a Claude Code session through the source-neutral library pipeline (ingest +
    /// worker), so delivery can be exercised across more than one source. Uses the shared
    /// `run_archive_worker_for_source` path the CLI drives for non-Copilot sources.
    fn archive_claude_session(&self, session_id: &str) {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/claude-code-2.1.44/normal")
            .join(format!("{session_id}.jsonl"));
        let transcript_dir = self.state.parent().unwrap().join("claude-transcripts");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript = transcript_dir.join(format!("{session_id}.jsonl"));
        std::fs::copy(&fixture, &transcript).unwrap();

        {
            let mut store =
                StateStore::open_for_source(&self.state, SourceKind::ClaudeCode).unwrap();
            store
                .ingest_agent_stop(session_id, 30_000, &self.project, &transcript)
                .unwrap();
            store
                .ingest_session_end(
                    session_id,
                    30_001,
                    &self.project,
                    "complete",
                    CompletionReason::Complete,
                    None,
                )
                .unwrap();
        }
        run_archive_worker_for_source(&self.state, SourceKind::ClaudeCode, session_id).unwrap();
        // Confirm the Claude session archived locally regardless of any delivery outcome.
        let store = StateStore::open_for_source(&self.state, SourceKind::ClaudeCode).unwrap();
        let record = store.get_session(session_id).unwrap().unwrap();
        assert_eq!(record.lifecycle_state, "archived");
    }

    fn open_state(&self) -> StateStore {
        StateStore::open(&self.state)
            .or_else(|_| StateStore::open_for_source(&self.state, SourceKind::Copilot))
            .unwrap()
    }

    /// Delivery is best-effort and runs *after* a session is marked archived, so a `hook wait`
    /// can return before delivery settles. These waiters let assertions observe the settled
    /// delivery outcome deterministically.
    fn wait_for_delivery_failed(&self, session_id: &str) {
        self.wait_for_delivery(session_id, |item| item["state"] == "failed");
    }

    fn wait_for_delivery_blocked(&self, session_id: &str) {
        self.wait_for_delivery(session_id, |item| item["state"] == "blocked");
    }

    fn wait_for_delivered_revision(&self, session_id: &str, revision: u64) {
        self.wait_for_delivery(session_id, |item| {
            item["state"] == "delivered" && item["delivered_revision"] == revision
        });
    }

    fn wait_for_delivery(&self, session_id: &str, predicate: impl Fn(&Value) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = self.delivery_status_json();
            if let Some(items) = status["items"].as_array()
                && items
                    .iter()
                    .any(|item| item["session_id"] == session_id && predicate(item))
            {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for delivery of {session_id}; status={}",
                    status
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn hook(&self, event: &str, payload: &Value) -> Output {
        let mut child = self
            .munshi()
            .arg("hook")
            .arg(event)
            .env("COPILOT_HOME", &self.copilot_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn wait(&self, session_id: &str) -> Output {
        self.munshi()
            .arg("hook")
            .arg("wait")
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--session-id")
            .arg(session_id)
            .arg("--timeout-ms")
            .arg("10000")
            .output()
            .unwrap()
    }

    fn write_transcript(&self, session_id: &str, request: &str, answer: &str) -> PathBuf {
        let path = self
            .copilot_home
            .join("session-state")
            .join(session_id)
            .join("events.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, transcript(session_id, request, answer)).unwrap();
        path.canonicalize().unwrap()
    }

    fn append_turn(&self, transcript: &Path, request: &str, answer: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(transcript)
            .unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "id": format!("{request}-user"),
                "timestamp": "2026-07-12T00:01:00.000Z",
                "parentId": "initial-assistant",
                "type": "user.message",
                "data": {"content": request},
            })
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "id": format!("{request}-assistant"),
                "timestamp": "2026-07-12T00:01:01.000Z",
                "parentId": format!("{request}-user"),
                "type": "assistant.message",
                "data": {"content": answer, "messageId": format!("{request}-message")},
            })
        )
        .unwrap();
    }
}

fn transcript(session_id: &str, request: &str, answer: &str) -> String {
    [
        json!({
            "id": "initial-start",
            "timestamp": "2026-07-12T00:00:00.000Z",
            "parentId": null,
            "type": "session.start",
            "data": {"sessionId": session_id},
        }),
        json!({
            "id": "initial-user",
            "timestamp": "2026-07-12T00:00:01.000Z",
            "parentId": "initial-start",
            "type": "user.message",
            "data": {"content": request},
        }),
        json!({
            "id": "initial-assistant",
            "timestamp": "2026-07-12T00:00:02.000Z",
            "parentId": "initial-user",
            "type": "assistant.message",
            "data": {"content": answer, "messageId": "initial-message"},
        }),
    ]
    .into_iter()
    .map(|value| value.to_string())
    .collect::<Vec<_>>()
    .join("\n")
        + "\n"
}

fn fake(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/manual/fake-summarizer")
        .join(name);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.canonicalize().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
