# Automatic clean-session archival

Register with absolute paths and explicitly accept the disclosure:

```bash
munshi register --accept-transcript-processing \
  --summarizer /absolute/path/to/copilot \
  --summarizer-arg=-s \
  --summarizer-arg=--no-ask-user \
  --output-dir /absolute/path/to/munshi-summaries
```

Without `--accept-transcript-processing`, a terminal prompts for the exact text `I ACCEPT`;
noninteractive registration fails. `--dry-run` prints the managed paths without writing. The
disclosure states that transcript summarization becomes default-on for all projects, the full
transcript is sent again to the configured summarizer and may consume credits, v1 does not redact
secrets or provide granular filtering, output is local Markdown, and remote delivery remains
disabled.

The default locations are:

- hooks: `$COPILOT_HOME/hooks/munshi.json`, or `~/.copilot/hooks/munshi.json`
- state/config: `$COPILOT_HOME/munshi`, or `~/.copilot/munshi`

`--copilot-home` overrides the root. Registration writes only Munshi's dedicated hook and config
files, rejects symlinked or wrongly owned managed paths, and does not rewrite equivalent files.
`munshi unregister` removes only those two positively recognized files; archives and pending
diagnostic state remain.

Register, and unregister when an existing hooks directory contains managed removal work, serialize
changes with a persistent owner-only `hooks/.munshi-registration.lock` file and a nonblocking OS
advisory lock held for the complete operation. The file is intentionally not unlinked; an unheld
file is immediately reusable, and the kernel releases an active lock if the process exits or
crashes. Unregister does not create a missing hooks directory or lock; it may remove a positively
recognized config directly when no hooks directory exists.

The hooks use direct `exec`/`args` fields with a two-second timeout, read exactly one bounded JSON
object, and emit nothing. `agentStop` atomically records only the required session metadata,
preserving the first recorded working directory as the origin project. A clean `sessionEnd` writes
a durable pending job and starts a detached worker with its working directory and inherited
standard streams closed. The worker invokes the same archive path as `munshi archive`. Hook
validation and local archival failures always return success to Copilot; a content-free error
category is stored at `failures/last.json`. Tests can use the hidden `munshi hook wait` command to
await a worker deterministically.

That hook shape is intentionally guarded as a version-pinned Copilot CLI 1.0.70 compatibility
contract from Phase 0. Generic web documentation or a different locally installed Copilot version
is not treated as evidence that the schema changed.

## Temporary file-state limitations

This issue intentionally does not add SQLite. Unlike the crash-safe registration advisory lock,
the per-session create-new worker marker is temporary operational state and a crash can leave it
stale. There is no worker-marker recovery, interrupted-session scan, resumed delta, revision
increment, transcript rewrite recovery, or guaranteed retry schedule. Those operational guarantees
belong to issue #4. Remote delivery remains disabled.

A worker that reaches a graceful archive failure records a redacted failed result and durably
removes both its pending job and worker marker; issue #3 does not retain attempted jobs for retry.
Only a process crash can leave those temporary files stale for issue #4 recovery.
