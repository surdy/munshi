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
