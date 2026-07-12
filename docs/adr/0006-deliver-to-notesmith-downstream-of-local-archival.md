# Deliver to Notesmith downstream of local archival

Notesmith delivery is opt-in, disabled by default, and strictly downstream of local archival. A
Notesmith outage, an unresolved credential, or an exhausted delivery attempt never rolls back,
invalidates, or blocks a local Markdown archive; delivery is attempted only after a summary
revision has been durably archived, and any failure is recorded as bounded operational state.

Delivered notes are Munshi-owned copies. A note is routed under a stable, origin-project-identity
folder with a stable `<source>-<session-id>` filename, and Munshi persists the returned note path,
delivered revision, and summary hash in a rebuildable SQLite `deliveries` table (schema 3). The
first successful revision creates the note; later revisions replace it in place without
`expected_hash`, so Munshi overwrites remote edits. Retries are idempotent — an unchanged
revision+hash is a no-op that never contacts the sink — and a create that conflicts with an
existing note is adopted as a replace, keeping the note identifier idempotent even across an
operational-database rebuild. Failures retry with bounded exponential backoff and are parked as a
dead letter once `max_attempts` is exhausted. Enabling delivery reports the pending backfill as a
dry run and requires explicit confirmation before publishing existing summaries. Disabling
delivery, or disabling a project, stops future delivery while retaining delivery history.

Credentials are never stored in configuration: the Munshi-owned `config.json` records only the
sink endpoint, vault, folder, and the *source* of the credential (an environment variable name or
an OS credential-store entry), which is resolved to a bearer token only at delivery time.

The wire protocol is isolated behind a `NotesmithSink` trait spoken over a minimal blocking
HTTP/1.1 client (Notesmith is a localhost daemon, so no async or TLS dependency is added). This
release implements only latest-revision create/replace. The mandatory versioned,
revision-history-preserving delivery is deferred to issue #9; delivered notes already carry stable
`munshi_session`/`munshi_revision` frontmatter that a future versioned sink can correlate against.
