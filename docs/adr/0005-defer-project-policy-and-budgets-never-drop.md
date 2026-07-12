# Defer project policy and cost budgets, never drop

Munshi enforces project enable/disable, hourly/daily summarizer-call budgets, and worker
concurrency as deferrals, not failures or deletions. A disabled project, an exhausted budget, or
saturated concurrency leaves a session in its current pending lifecycle state with only a safe
diagnostic category recorded; later hooks and `munshi hook recover` retry it opportunistically once
the project is re-enabled, the budget window rolls over, or concurrency frees up.

The explicit disabled-projects list lives in Munshi's own `config.json`, alongside global budget
defaults, rather than in the rebuildable SQLite operational state: an explicit `munshi project
disable` must survive a database rebuild from Markdown (ADR 0004), and re-registration preserves it
rather than silently re-enabling every project. A project may also carry a nearest-parent
`.munshi.toml` override for its own budgets and enable state; a present but unparsable or unsafe
override fails closed (treated as disabled) rather than silently falling back to default-on
processing, consistent with registration's explicit disclosure that summarization is default-on.

Concurrency and budget capacity are each checked and reserved inside one `BEGIN IMMEDIATE` SQLite
transaction rather than as a separate read followed by a separate write: `claim_session` checks
live processing leases and claims a session in the same transaction, and
`reserve_summarizer_call` checks a project's rolling hourly/daily call count and records the call
in the same transaction, called only once a real summarizer invocation is imminent. SQLite
serializes writers across processes on the same database file, so this is the only way two
independent `munshi hook-worker` processes racing for the same slot or the same project's budget
cannot both observe capacity and both proceed. A non-atomic "check, then separately act" pair is
not an acceptable substitute for this guarantee, even if each half is individually correct.
