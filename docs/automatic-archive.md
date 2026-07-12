# Automatic clean-session archival

Register with absolute paths and explicitly accept the disclosure:

```bash
munshi register --accept-disclosure \
  --summarizer /absolute/path/to/copilot \
  --summarizer-arg=-s \
  --summarizer-arg=--no-ask-user \
  --output-dir /absolute/path/to/munshi-summaries
```

Without `--accept-disclosure`, a terminal prompts for the exact text `I ACCEPT`; noninteractive
registration fails. The disclosure states that transcript summarization becomes default-on, v1
does not redact secrets or provide granular filtering, transcript content is sent to the configured
summarizer, output is local Markdown, and remote delivery remains disabled.

The default locations are:

- hooks: `$COPILOT_HOME/hooks/munshi.json`, or `~/.copilot/hooks/munshi.json`
- state/config: `$COPILOT_HOME/munshi`, or `~/.copilot/munshi`

`--state-dir` and `--copilot-home` override them. Registration writes only Munshi's dedicated hook
and config files, rejects symlinked or wrongly owned managed paths, and atomically replaces valid
existing Munshi files. `munshi unregister` removes only those two files; archives and pending
diagnostic state remain.

The hooks read exactly one bounded JSON object and emit nothing. `agentStop` atomically records only
the session ID, transcript path, and origin working directory. `sessionEnd` starts a detached
worker, closes inherited standard streams, and returns quickly. The worker invokes the same archive
path as `munshi archive`. Hook validation and local archival failures always return success to
Copilot; a content-free error category is stored at `failures/last.json`. Tests can use the hidden
`munshi hook wait` command to await a worker deterministically.

## Temporary file-state limitations

This issue intentionally does not add SQLite. A create-new marker suppresses concurrent duplicate
workers, but a crash can leave a stale marker. There is no crash recovery, interrupted-session
scan, resumed delta, revision increment, transcript rewrite recovery, or guaranteed retry schedule.
Those operational guarantees belong to issue #4. Remote delivery remains disabled.
