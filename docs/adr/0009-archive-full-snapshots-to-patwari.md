# Archive full session snapshots to Patwari

Munshi uploads each successful summary revision to Patwari as one immutable snapshot containing the
complete verbatim transcript, the rendered summary revision, and every extracted output
(ADR 0010), with roles conveyed by reserved logical paths under a versioned artifact set. Each
revision uploads full artifacts rather than deltas: Patwari's per-owner blob deduplication absorbs
unchanged artifacts, and the source cursors already recorded in capture provenance keep segmented
delta upload open as a future optimization. Artifacts are zstd-compressed but not encrypted; Patwari
verifies original content hashes on its trusted network, and encryption would blind that
verification.

Archive upload runs strictly downstream of local archival and in parallel with Notesmith delivery:
a Patwari failure never blocks Markdown creation or delivery, and vice versa. Attempts are tracked
in the rebuildable SQLite operational state and retried with backoff, mirroring ADR 0006. When
uploads lag behind archival, Munshi converges on the latest successful revision rather than
queueing intermediates: a superseded revision whose upload never completed is skipped, which is
safe because every snapshot is full and self-contained, so the newest snapshot's transcript
carries the skipped revision's session content. Munshi
registers a persistent client UUID with Patwari, generates a fresh capture ID per distinct snapshot
attempt, and reuses that capture ID on retries so interrupted uploads resume rather than duplicate.

Harness sidecar state (Copilot workspace and checkpoint files, Claude Code todos) is excluded from
the initial artifact set. Snapshots are immutable and artifact sets additive, so each adapter can
add its sidecar files later under a bumped artifact-set version without migrating existing
snapshots; consumers must tolerate absent artifact kinds.

**Update (2026-08-03, issue #23):** artifact set v2 adds optional `sidecar/<relative-path>`
artifacts. Copilot contributes an allowlisted set of small textual session-state files
(`workspace.yaml`, `plan.md`, `vscode.metadata.json`, `checkpoints/*.md`, the rewind-snapshot
indexes) under per-file/total/count caps; Claude Code and Codex contribute none — their sidecar
state (todos, shell snapshots) was evaluated and judged not worth archiving. The set is captured
and **staged into the archive output directory beside the summary Markdown at archive time**, and
uploads assemble from the staged copies: capture identity is reused across retries of one
revision and Patwari rejects a reused capture ID whose canonical manifest changed, so live
session-state files — which mutate under a running session — can never feed the manifest
directly. Sidecars are optional and non-fatal end to end: they are outside the required logical
paths (so backfill never re-uploads historical snapshots), a file that mutates during capture is
skipped, and transcript interpretation is unchanged from v1 (readers accept versions 1..=2).
