# Deliver to Notesmith downstream of local archival

Notesmith delivery is opt-in, disabled by default, and strictly downstream of local archival. A
Notesmith outage, an unresolved credential, or an exhausted delivery attempt never rolls back,
invalidates, or blocks a local Markdown archive; delivery is attempted only after a summary
revision has been durably archived, and any failure is recorded as bounded operational state.

Delivered notes are Munshi-owned copies. A note is routed under a stable, origin-project-identity
folder with a stable `<source>-<session-id>` filename, and Munshi persists the returned note path,
delivered revision, and summary hash in a rebuildable SQLite `deliveries` table (schema 3). The
first successful revision creates the note; later revisions replace it in place without
`expected_hash`, so Munshi overwrites remote edits. Because Notesmith's create route assembles the
note from a body plus a separate frontmatter map while its replace route writes the request body
verbatim, Munshi sends the summary **body only** with a Munshi-owned identity frontmatter map on
create, and a **complete single-frontmatter-block document** on replace; either way the delivered
note carries exactly one frontmatter block whose `munshi_session`/`munshi_source`/`munshi_project`/
`munshi_revision` fields are present and updated on every revision. Retries are idempotent — an
unchanged revision+hash is a no-op that never contacts the sink — and a create that conflicts with
an existing note is adopted as a replace, keeping the note identifier idempotent even across an
operational-database rebuild. Failures retry with bounded exponential backoff and are parked as a
dead letter once `max_attempts` is exhausted. Enabling delivery reports the pending backfill as a
dry run and requires explicit confirmation before publishing existing summaries. Disabling
delivery, or disabling a project, stops future delivery while retaining delivery history. Backfill
and retry resolve each session's delivery through its own source scope, so Claude and Codex
deliveries are counted and retried correctly alongside Copilot.

Credentials are never stored in configuration: the Munshi-owned `config.json` records only the
sink endpoint, vault, folder, and the *source* of the credential (an environment variable name or
an OS credential-store entry), which is resolved to a bearer token only at delivery time. Notesmith
itself is unauthenticated and defers authentication to a reverse proxy (notes-method ADR 0010), so
Munshi sends `Authorization: Bearer <token>` when a credential source is configured; the token is
never logged or echoed in any diagnostic.

The wire protocol is isolated behind a `NotesmithSink` trait spoken over a minimal blocking
HTTP/1.1 client (Notesmith is a localhost daemon, so no async or TLS dependency is added). This
release implements only latest-revision create/replace. The mandatory versioned,
revision-history-preserving delivery is deferred to issue #9; delivered notes already carry stable
`munshi_session`/`munshi_revision` frontmatter that a future versioned sink can correlate against.

**Update (issue #9):** versioned delivery is now implemented on the same `NotesmithSink` trait. When
local archive Git history is enabled alongside delivery, Munshi verifies (or, when configured to
provision, explicitly enables) the Notesmith vault's per-vault Git revision-history capability
(`config.git.enabled`) and commits each delivered revision into the vault's own history with a
message correlated to the local archive commit by source-scoped session identity and revision. If
the capability is absent, versioned delivery is blocked with an actionable `remote-history-unavailable`
status instead of degrading to latest-only storage, and the block never affects the already-durable
local archive (consistent with ADR 0003).
