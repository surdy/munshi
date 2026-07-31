//! Assembly of the snapshot's `db` section from the `sessions`, `attempts` and `diagnostics`
//! JSON contracts.
//!
//! Everything here is pure — parsed contract values and the current wall-clock time in, one JSON
//! section out — so the histograms, the project ranking and the bin arithmetic are testable
//! without a `munshi` binary present. Navigation is deliberately lenient: a row missing a field is
//! skipped or defaulted rather than failing the section, because these contracts evolve
//! independently of this crate and a drifted field must degrade one panel, not the snapshot.
//!
//! State and source values are the CLI's kebab-case spellings (`summary-pending`, `claude-code`),
//! which are what the page matches on and prints.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// Lifecycle states counted as still queued, matching the page's "Queued" tile.
const QUEUED_STATES: [&str; 2] = ["interrupted", "observed"];

/// Lifecycle states counted as in flight. The page prints these verbatim as badges.
const PROCESSING_STATES: [&str; 2] = ["processing", "summary-pending"];

/// Attempt outcomes the outcome chart stacks, in the key order the spike emitted.
const OUTCOMES: [&str; 4] = ["succeeded", "failed", "recovered", "superseded"];

/// Width of one outcome bin.
const BIN_MS: i64 = 30 * 60 * 1000;

/// How far back the outcome chart reaches.
const WINDOW_MS: i64 = 6 * 60 * 60 * 1000;

/// Largest `processing_now` list; the page shows the first eight.
const PROCESSING_LIMIT: usize = 20;

/// Largest `recent_archived` and `recent_failures` lists.
const RECENT_LIMIT: usize = 10;

/// Largest `diagnostics_tail` list.
const DIAGNOSTICS_LIMIT: usize = 5;

/// Stand-in for a row whose project or source is absent or empty. The page prints it as a label.
const UNKNOWN: &str = "(unknown)";

/// Assembles the `db` section. A key is emitted only when the contract it derives from was
/// collected, so a section reads as "no rows" exactly where the page already renders an empty
/// state; when none of the three contracts is available the section itself is `null`.
pub(crate) fn assemble(
    now_ms: i64,
    sessions: Option<&Value>,
    attempts: Option<&Value>,
    diagnostics: Option<&Value>,
) -> Value {
    if sessions.is_none() && attempts.is_none() && diagnostics.is_none() {
        return Value::Null;
    }
    let mut db = Map::new();
    if sessions.is_some() {
        let rows = items(sessions);
        db.insert("by_state_source".to_owned(), by_state_source(rows));
        db.insert(
            "remaining_by_project".to_owned(),
            remaining_by_project(rows),
        );
        db.insert("archived_by_source".to_owned(), archived_by_source(rows));
        db.insert("processing_now".to_owned(), processing_now(rows));
        db.insert("recent_archived".to_owned(), recent_archived(rows));
    }
    if attempts.is_some() {
        let rows = items(attempts);
        db.insert("recent_failures".to_owned(), recent_failures(rows));
        db.insert("outcome_bins".to_owned(), outcome_bins(now_ms, rows));
    }
    if diagnostics.is_some() {
        db.insert(
            "diagnostics_tail".to_owned(),
            diagnostics_tail(items(diagnostics)),
        );
    }
    Value::Object(db)
}

/// Session counts grouped by lifecycle state and source, ordered by state then source so repeated
/// collections produce byte-identical arrays.
fn by_state_source(rows: &[Value]) -> Value {
    let mut counts: BTreeMap<(&str, &str), i64> = BTreeMap::new();
    for row in rows {
        let Some(state) = lifecycle(row) else {
            continue;
        };
        *counts.entry((state, source(row))).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|((state, source), n)| json!({ "state": state, "source": source, "n": n }))
        .collect()
}

/// Queued sessions grouped by project, ordered by descending count. Ties break on the project name
/// so the top-ten bar chart does not reshuffle between collections.
fn remaining_by_project(rows: &[Value]) -> Value {
    let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
    for row in rows {
        if lifecycle(row).is_some_and(|state| QUEUED_STATES.contains(&state)) {
            *counts.entry(project(row)).or_default() += 1;
        }
    }
    let mut ranked: Vec<(&str, i64)> = counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    ranked
        .into_iter()
        .map(|(project, n)| json!([project, n]))
        .collect()
}

/// Archived session counts keyed by source. The page reads the CLI's kebab-case source strings
/// (`claude-code`, `copilot-cli`) directly out of this object.
fn archived_by_source(rows: &[Value]) -> Value {
    let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
    for row in rows {
        if lifecycle(row) == Some("archived") {
            *counts.entry(source(row)).or_default() += 1;
        }
    }
    Value::Object(
        counts
            .into_iter()
            .map(|(source, n)| (source.to_owned(), json!(n)))
            .collect(),
    )
}

/// Sessions being summarized right now, most recently updated first. Rows without a session ID are
/// dropped because the page slices the ID to build its short form.
fn processing_now(rows: &[Value]) -> Value {
    let mut selected = select(rows, |row| {
        has_session_id(row)
            && lifecycle(row).is_some_and(|state| PROCESSING_STATES.contains(&state))
    });
    sort_newest_first(&mut selected, "updated_at_ms");
    selected
        .into_iter()
        .take(PROCESSING_LIMIT)
        .map(|row| {
            json!({
                "sid": text(row, "session_id"),
                "source": text(row, "source"),
                "state": lifecycle(row),
                "project": project(row),
            })
        })
        .collect()
}

/// The most recently archived sessions, newest first.
fn recent_archived(rows: &[Value]) -> Value {
    let mut selected = select(rows, |row| {
        has_session_id(row) && lifecycle(row) == Some("archived")
    });
    sort_newest_first(&mut selected, "updated_at_ms");
    selected
        .into_iter()
        .take(RECENT_LIMIT)
        .map(|row| {
            json!({
                "sid": text(row, "session_id"),
                "source": text(row, "source"),
                "project": project(row),
                "title": text(row, "summary_title"),
                "at_ms": millis(row, "updated_at_ms"),
            })
        })
        .collect()
}

/// The most recent failed processing attempts, newest finish first.
fn recent_failures(rows: &[Value]) -> Value {
    let mut selected = select(rows, |row| {
        has_session_id(row) && text(row, "outcome") == Some("failed")
    });
    sort_newest_first(&mut selected, "finished_at_ms");
    selected
        .into_iter()
        .take(RECENT_LIMIT)
        .map(|row| {
            json!({
                "sid": text(row, "session_id"),
                "source": text(row, "source"),
                "project": project(row),
                "error_category": text(row, "error_category"),
                "at_ms": millis(row, "finished_at_ms"),
            })
        })
        .collect()
}

/// Attempt outcomes bucketed into [`BIN_MS`] bins spanning the last [`WINDOW_MS`], empty bins
/// included so the chart keeps a fixed time axis. Bins run from the one containing
/// `now_ms - WINDOW_MS` through the one containing `now_ms`; an attempt that finished before the
/// window, or in the future beyond the current bin, is not represented.
fn outcome_bins(now_ms: i64, rows: &[Value]) -> Value {
    let start = (now_ms - WINDOW_MS).div_euclid(BIN_MS) * BIN_MS;
    let end = (now_ms.div_euclid(BIN_MS) + 1) * BIN_MS;
    let mut counts: BTreeMap<i64, [i64; 4]> = BTreeMap::new();
    for row in rows {
        let Some(finished) = millis(row, "finished_at_ms") else {
            continue;
        };
        if finished < start {
            continue;
        }
        let Some(outcome) = text(row, "outcome")
            .and_then(|outcome| OUTCOMES.iter().position(|known| *known == outcome))
        else {
            continue;
        };
        counts
            .entry(finished.div_euclid(BIN_MS) * BIN_MS)
            .or_default()[outcome] += 1;
    }
    let mut bins = Vec::new();
    let mut bin_start = start;
    while bin_start < end {
        let tally = counts.get(&bin_start).copied().unwrap_or_default();
        let mut bin = Map::new();
        bin.insert("bin_start_ms".to_owned(), json!(bin_start));
        for (outcome, count) in OUTCOMES.iter().zip(tally) {
            bin.insert((*outcome).to_owned(), json!(count));
        }
        bins.push(Value::Object(bin));
        bin_start += BIN_MS;
    }
    Value::Array(bins)
}

/// The most recently recorded diagnostics, newest first.
fn diagnostics_tail(rows: &[Value]) -> Value {
    let mut selected = select(rows, |_| true);
    sort_newest_first(&mut selected, "recorded_at_ms");
    selected
        .into_iter()
        .take(DIAGNOSTICS_LIMIT)
        .map(|row| {
            json!({
                "operation": text(row, "operation"),
                "category": text(row, "category"),
                "recorded_at_ms": millis(row, "recorded_at_ms"),
            })
        })
        .collect()
}

/// The `items` array of a `--json` contract, or an empty slice when the contract is absent or does
/// not carry one.
fn items(contract: Option<&Value>) -> &[Value] {
    contract
        .and_then(|contract| contract.get("items"))
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The rows satisfying `keep`, borrowed rather than cloned so the caller can sort cheaply.
fn select(rows: &[Value], keep: impl Fn(&Value) -> bool) -> Vec<&Value> {
    rows.iter().filter(|row| keep(row)).collect()
}

/// Whether a row can appear in a session-keyed list. The page slices the session ID to build the
/// short form it prints, so a row without one has nothing to render.
fn has_session_id(row: &Value) -> bool {
    text(row, "session_id").is_some()
}

/// Orders rows newest first on a millisecond field. A row without that timestamp sorts last rather
/// than displacing a dated one, and equal timestamps keep the contract's own order.
fn sort_newest_first(rows: &mut [&Value], key: &str) {
    rows.sort_by_key(|row| Reverse(millis(row, key).unwrap_or(i64::MIN)));
}

/// A string field, or `None` when it is absent, null, empty, or not a string.
fn text<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// A millisecond timestamp field, or `None` when it is absent or not an integer.
fn millis(row: &Value, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)
}

/// A row's lifecycle state, falling back to the operational `state` field for contracts that only
/// carry the latter.
fn lifecycle(row: &Value) -> Option<&str> {
    text(row, "lifecycle_state").or_else(|| text(row, "state"))
}

/// A row's capturing harness.
fn source(row: &Value) -> &str {
    text(row, "source").unwrap_or(UNKNOWN)
}

/// A row's origin project. The CLI resolves the project name, component and working directory into
/// this one field; rows predating it group under a single visible bucket.
fn project(row: &Value) -> &str {
    text(row, "project").unwrap_or(UNKNOWN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two harnesses, several states, one row missing its project.
    fn sessions_contract() -> Value {
        json!({
            "schema_version": 1,
            "command": "sessions",
            "total": 7,
            "items": [
                {"source": "claude-code", "session_id": "a1", "state": "archived",
                 "lifecycle_state": "archived", "summary_title": "Wire up the dashboard",
                 "project": "munshi", "updated_at_ms": 3_000},
                {"source": "copilot-cli", "session_id": "b2", "state": "archived",
                 "lifecycle_state": "archived", "summary_title": null,
                 "project": "patwari", "updated_at_ms": 5_000},
                {"source": "claude-code", "session_id": "c3", "state": "interrupted",
                 "lifecycle_state": "interrupted", "project": "munshi", "updated_at_ms": 1_000},
                {"source": "claude-code", "session_id": "d4", "state": "observed",
                 "lifecycle_state": "observed", "project": "munshi", "updated_at_ms": 2_000},
                {"source": "copilot-cli", "session_id": "e5", "state": "observed",
                 "lifecycle_state": "observed", "updated_at_ms": 2_500},
                {"source": "claude-code", "session_id": "f6", "state": "processing",
                 "lifecycle_state": "processing", "project": "notesmith", "updated_at_ms": 9_000},
                {"source": "copilot-cli", "session_id": "g7", "state": "summary-pending",
                 "lifecycle_state": "summary-pending", "project": "patwari",
                 "updated_at_ms": 8_000}
            ]
        })
    }

    #[test]
    fn groups_sessions_by_lifecycle_state_and_source() {
        let sessions = sessions_contract();
        assert_eq!(
            by_state_source(items(Some(&sessions))),
            json!([
                {"state": "archived", "source": "claude-code", "n": 1},
                {"state": "archived", "source": "copilot-cli", "n": 1},
                {"state": "interrupted", "source": "claude-code", "n": 1},
                {"state": "observed", "source": "claude-code", "n": 1},
                {"state": "observed", "source": "copilot-cli", "n": 1},
                {"state": "processing", "source": "claude-code", "n": 1},
                {"state": "summary-pending", "source": "copilot-cli", "n": 1},
            ])
        );
    }

    #[test]
    fn ranks_queued_projects_by_count_and_buckets_missing_names() {
        let sessions = sessions_contract();
        assert_eq!(
            remaining_by_project(items(Some(&sessions))),
            json!([["munshi", 2], ["(unknown)", 1]])
        );
    }

    #[test]
    fn ranks_equal_project_counts_by_name() {
        let sessions = json!({"items": [
            {"lifecycle_state": "observed", "project": "zeta"},
            {"lifecycle_state": "observed", "project": "alpha"},
        ]});
        assert_eq!(
            remaining_by_project(items(Some(&sessions))),
            json!([["alpha", 1], ["zeta", 1]])
        );
    }

    #[test]
    fn counts_archived_sessions_by_source() {
        let sessions = sessions_contract();
        assert_eq!(
            archived_by_source(items(Some(&sessions))),
            json!({"claude-code": 1, "copilot-cli": 1})
        );
    }

    #[test]
    fn lists_in_flight_sessions_newest_update_first() {
        let sessions = sessions_contract();
        assert_eq!(
            processing_now(items(Some(&sessions))),
            json!([
                {"sid": "f6", "source": "claude-code", "state": "processing",
                 "project": "notesmith"},
                {"sid": "g7", "source": "copilot-cli", "state": "summary-pending",
                 "project": "patwari"},
            ])
        );
    }

    #[test]
    fn lists_recent_archives_newest_first_with_titles() {
        let sessions = sessions_contract();
        assert_eq!(
            recent_archived(items(Some(&sessions))),
            json!([
                {"sid": "b2", "source": "copilot-cli", "project": "patwari", "title": null,
                 "at_ms": 5_000},
                {"sid": "a1", "source": "claude-code", "project": "munshi",
                 "title": "Wire up the dashboard", "at_ms": 3_000},
            ])
        );
    }

    #[test]
    fn lists_only_failed_attempts_newest_finish_first() {
        let attempts = json!({"items": [
            {"source": "claude-code", "session_id": "a1", "project": "munshi",
             "outcome": "succeeded", "error_category": null, "finished_at_ms": 900},
            {"source": "claude-code", "session_id": "b2", "project": "munshi",
             "outcome": "failed", "error_category": "summarizer-timeout", "finished_at_ms": 500},
            {"source": "copilot-cli", "session_id": "c3", "project": "patwari",
             "outcome": "failed", "error_category": "transcript-missing", "finished_at_ms": 800},
        ]});
        assert_eq!(
            recent_failures(items(Some(&attempts))),
            json!([
                {"sid": "c3", "source": "copilot-cli", "project": "patwari",
                 "error_category": "transcript-missing", "at_ms": 800},
                {"sid": "b2", "source": "claude-code", "project": "munshi",
                 "error_category": "summarizer-timeout", "at_ms": 500},
            ])
        );
    }

    #[test]
    fn bins_span_thirteen_half_hours_ending_with_the_current_bin() {
        let now_ms: i64 = 20_000_000_000;
        let bins = outcome_bins(now_ms, &[]);
        let bins = bins.as_array().expect("bins are an array");
        assert_eq!(bins.len(), 13);
        let first = bins[0]["bin_start_ms"]
            .as_i64()
            .expect("bin start is an int");
        let last = bins[12]["bin_start_ms"]
            .as_i64()
            .expect("bin start is an int");
        assert_eq!(first, (now_ms - WINDOW_MS).div_euclid(BIN_MS) * BIN_MS);
        assert_eq!(last, now_ms.div_euclid(BIN_MS) * BIN_MS);
        assert_eq!(last - first, WINDOW_MS);
        assert_eq!(
            bins[0],
            json!({"bin_start_ms": first, "succeeded": 0, "failed": 0, "recovered": 0,
                   "superseded": 0})
        );
    }

    #[test]
    fn bins_tally_outcomes_and_keep_empty_bins_between_them() {
        let now_ms: i64 = 20_000_000_000;
        let current = now_ms.div_euclid(BIN_MS) * BIN_MS;
        let attempts = json!({"items": [
            {"outcome": "succeeded", "finished_at_ms": current + 1},
            {"outcome": "succeeded", "finished_at_ms": current + 2},
            {"outcome": "recovered", "finished_at_ms": current - BIN_MS},
            {"outcome": "superseded", "finished_at_ms": current - 3 * BIN_MS},
            {"outcome": "failed", "finished_at_ms": current - 3 * BIN_MS},
        ]});
        let bins = outcome_bins(now_ms, items(Some(&attempts)));
        let bins = bins.as_array().expect("bins are an array");
        assert_eq!(
            bins[12],
            json!({"bin_start_ms": current, "succeeded": 2, "failed": 0, "recovered": 0,
                   "superseded": 0})
        );
        assert_eq!(
            bins[11],
            json!({"bin_start_ms": current - BIN_MS, "succeeded": 0, "failed": 0, "recovered": 1,
                   "superseded": 0})
        );
        assert_eq!(
            bins[10],
            json!({"bin_start_ms": current - 2 * BIN_MS, "succeeded": 0, "failed": 0,
                   "recovered": 0, "superseded": 0})
        );
        assert_eq!(
            bins[9],
            json!({"bin_start_ms": current - 3 * BIN_MS, "succeeded": 0, "failed": 1,
                   "recovered": 0, "superseded": 1}),
            "one bin tallies each outcome independently"
        );
    }

    #[test]
    fn bins_exclude_attempts_outside_the_window() {
        let now_ms: i64 = 20_000_000_000;
        let start = (now_ms - WINDOW_MS).div_euclid(BIN_MS) * BIN_MS;
        let attempts = json!({"items": [
            {"outcome": "failed", "finished_at_ms": start - 1},
            {"outcome": "failed", "finished_at_ms": start},
            {"outcome": "failed", "finished_at_ms": now_ms + WINDOW_MS},
            {"outcome": "failed", "finished_at_ms": null},
            {"outcome": "unheard-of", "finished_at_ms": start},
        ]});
        let bins = outcome_bins(now_ms, items(Some(&attempts)));
        let bins = bins.as_array().expect("bins are an array");
        let failed: i64 = bins
            .iter()
            .map(|bin| bin["failed"].as_i64().unwrap_or_default())
            .sum();
        assert_eq!(failed, 1);
        assert_eq!(bins[0]["failed"], json!(1));
    }

    #[test]
    fn diagnostics_tail_keeps_the_five_newest() {
        let diagnostics = json!({"items": (0..8)
            .map(|index| json!({"operation": format!("op-{index}"), "category": "retry",
                                "recorded_at_ms": index * 10}))
            .collect::<Vec<_>>()});
        let tail = diagnostics_tail(items(Some(&diagnostics)));
        let tail = tail.as_array().expect("tail is an array");
        assert_eq!(tail.len(), DIAGNOSTICS_LIMIT);
        assert_eq!(
            tail[0],
            json!({"operation": "op-7", "category": "retry", "recorded_at_ms": 70})
        );
        assert_eq!(tail[4]["operation"], json!("op-3"));
    }

    #[test]
    fn assembles_only_the_sections_whose_contract_was_collected() {
        let sessions = sessions_contract();
        let assembled = assemble(0, Some(&sessions), None, None);
        let keys: Vec<&str> = assembled
            .as_object()
            .expect("db is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "by_state_source",
                "remaining_by_project",
                "archived_by_source",
                "processing_now",
                "recent_archived",
            ]
        );
    }

    #[test]
    fn assembles_every_section_when_all_three_contracts_are_present() {
        let sessions = sessions_contract();
        let attempts = json!({"items": []});
        let diagnostics = json!({"items": []});
        let assembled = assemble(0, Some(&sessions), Some(&attempts), Some(&diagnostics));
        let keys: Vec<&str> = assembled
            .as_object()
            .expect("db is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "by_state_source",
                "remaining_by_project",
                "archived_by_source",
                "processing_now",
                "recent_archived",
                "recent_failures",
                "outcome_bins",
                "diagnostics_tail",
            ]
        );
    }

    #[test]
    fn assembles_null_when_no_contract_was_collected() {
        assert_eq!(assemble(0, None, None, None), Value::Null);
    }

    #[test]
    fn tolerates_contracts_without_an_items_array() {
        let malformed = json!({"schema_version": 2, "command": "sessions"});
        let assembled = assemble(0, Some(&malformed), Some(&malformed), Some(&malformed));
        assert_eq!(assembled["by_state_source"], json!([]));
        assert_eq!(assembled["archived_by_source"], json!({}));
        assert_eq!(assembled["recent_failures"], json!([]));
        assert_eq!(assembled["diagnostics_tail"], json!([]));
    }

    #[test]
    fn tolerates_rows_missing_the_fields_the_page_keys_on() {
        let sessions = json!({"items": [
            {"lifecycle_state": "archived"},
            {"session_id": "a1"},
            {"source": "claude-code", "session_id": "b2", "state": "archived",
             "updated_at_ms": 1},
        ]});
        let assembled = assemble(0, Some(&sessions), None, None);
        // The ID-less archived row is counted but never listed; the state-less row is neither.
        assert_eq!(
            assembled["archived_by_source"],
            json!({"(unknown)": 1, "claude-code": 1})
        );
        assert_eq!(
            assembled["recent_archived"],
            json!([{"sid": "b2", "source": "claude-code", "project": "(unknown)", "title": null,
                    "at_ms": 1}])
        );
    }
}
