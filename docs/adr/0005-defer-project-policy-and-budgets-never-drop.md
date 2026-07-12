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
