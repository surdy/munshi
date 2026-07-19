# Use rebuildable SQLite operational state

> Amended by [ADR 0008](0008-harness-neutral-munshi-home.md): the state directory moved from
> `$COPILOT_HOME/munshi` to the harness-neutral Munshi home (`$MUNSHI_HOME`, default `~/.munshi`).
> Everything else in this ADR stands.

Munshi stores synchronous multi-process hook state in `$COPILOT_HOME/munshi/munshi.db` through
forward `schema_migrations`. Short SQLite transactions own observation deduplication, lifecycle
transitions, worker claims, current cursor/revision metadata, the current structured-summary cache,
attempts, and safe diagnostics. Per-session OS advisory locks serialize source processing and
Markdown replacement without blocking unrelated sessions.

Markdown remains the durable archive authority. SQLite stores no raw transcript and no historical
summary body. A missing or corrupt database can be rebuilt from validated current Munshi-owned
Markdown; schema 1 archives force a complete reread because they lack prefix evidence. Recognized
stale issue #3 metadata/jobs are imported transactionally and removed only after import, while
fresh, malformed, or unsafe artifacts are deferred or left untouched.
