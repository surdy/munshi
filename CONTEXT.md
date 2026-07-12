# Munshi

Munshi is the local-first record-keeping context for coding-agent sessions and their durable
summaries.

## Language

**Archived session**:
A session whose current summary has been atomically written to local Markdown. Remote delivery is
tracked separately and does not determine whether the session is archived.
_Avoid_: Delivered session, backed-up session

**Delivery**:
An attempt to publish an archived session summary to a configured remote sink such as Notesmith.
_Avoid_: Archive, backup

**Revision pending**:
A previously archived session with newer source activity that is not yet represented in its local
Markdown summary. Its last successful archived revision remains readable.
_Avoid_: Archived, delivery pending

**Logical session**:
One coding-agent conversation identified by the source harness's stable session ID, even when its
activity spans multiple repositories or working directories.
_Avoid_: Project session, transcript

**Origin project**:
The first repository detected for a logical session. It is the session's stable project association
for routing; later repositories are related activity.
_Avoid_: Current project, latest project

**Project identity**:
The normalized canonical Git remote shared by clones and worktrees, or a locally assigned identity
when no remote exists. Filesystem paths and directory names are project metadata, not identity.
_Avoid_: Repository path, project slug

**Interrupted session**:
A logical session with recorded activity but no clean source-harness end event. It remains pending
until Munshi recovers and archives it with the interrupted completion reason.
_Avoid_: Failed session, discarded session

**Archive-worthy session**:
A logical session containing at least one user request and agent-produced content or tool activity.
Observed sessions below this threshold do not produce Markdown summaries.
_Avoid_: Empty session, observed session

**Archive file**:
Munshi-owned Markdown containing the current successful summary revision for a logical session.
Munshi may replace it atomically; it is not a user-editable note.
_Avoid_: Working note, user document

**Delivered note**:
A Munshi-owned remote copy of an archive file. Munshi may overwrite remote edits when publishing a
new summary revision.
_Avoid_: Curated note, user-owned note

**Local archival**:
The default processing of archive-worthy sessions through the configured summarization model into
local archive files after Munshi is registered. Projects may opt out; remote delivery remains
separately opt-in.
_Avoid_: Delivery, backup

**Registration**:
The explicit setup action that installs Munshi's hooks and enables default-on transcript
summarization after prominently disclosing that v1 performs no secret redaction.
_Avoid_: Project opt-in, remote delivery consent

**Summary pending**:
An archive-worthy session waiting for a successful summary, including sessions deferred by an
exhausted summarization budget. Eligible work is retried opportunistically by later hooks and
user-invoked Munshi commands.
_Avoid_: Archived session, skipped session

**Disabled project**:
A project excluded from future transcript processing and delivery. Disabling does not delete its
existing archive files or delivered notes.
_Avoid_: Deleted project, paused project

**Summary title**:
The human-readable title of the current summary revision. It may change as a session evolves and
never determines archive identity or routing.
_Avoid_: Session identity, archive key

**Delivery backfill**:
The confirmed first publication of existing current archive files when a remote sink is enabled for
a scope. Subsequent successful revisions are delivered normally.
_Avoid_: Migration, backup

**Archive deletion**:
An explicit removal operation scoped to local content or to both local and delivered copies.
Local-only removal is the default.
_Avoid_: Project disablement, retention

**Git history**:
Optional revision history for archive files. When enabled, Munshi and Notesmith maintain separate
Git histories correlated by stable session and summary revision identifiers. It may be enabled
locally without a remote sink.
_Avoid_: Shared archive repository, database revision store

**Archive commit**:
One automatic Git commit containing one successful summary revision for one archive file, identified
by the stable session ID and summary revision number.
_Avoid_: Batch commit, user commit

**Archive repository**:
The dedicated Git repository rooted at Munshi's configured archive output directory. Munshi never
creates archive commits in source-code repositories.
_Avoid_: Source repository, project repository

**Versioned delivery**:
Remote publication while Git history is enabled. It requires the Notesmith sink to preserve its own
correlated revision history; delivery is blocked if that capability is unavailable.
_Avoid_: Best-effort delivery, latest-only delivery

**Archive record**:
The durable Markdown representation of an archived session, with optional Git history. SQLite holds
rebuildable operational state and is not required for an existing archive record to remain valid.
_Avoid_: Database row, export

**Summary failure**:
An unsuccessful attempt to summarize an archive-worthy session. It leaves the session retryable and
does not create a metadata-only or raw-transcript archive file.
_Avoid_: Archived session, placeholder archive

**Summary**:
A structured work record of a session's goals, decisions, meaningful changes, validation, and open
items. It excludes verbatim prompts, raw tool output, secrets, and substantial code excerpts.
_Avoid_: Transcript, conversation log

**Current summary**:
The latest successful summary revision. When Git history is disabled, it is the only summary text
Munshi retains.
_Avoid_: Summary history, transcript
