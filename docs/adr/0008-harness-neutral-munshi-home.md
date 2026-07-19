# Store state and configuration in a harness-neutral Munshi home

## Status

Accepted. Amends [ADR 0004](0004-use-rebuildable-sqlite-operational-state.md), which located the
operational database at `$COPILOT_HOME/munshi/munshi.db` and rooted registration inside the Copilot
home.

## Context

Munshi v1 targeted Copilot CLI only, so state, configuration, and the registration lock all lived
under `$COPILOT_HOME/munshi`, and registration enforced `state_directory == copilot_home/munshi`.
Supporting Claude Code (and later Codex) as capture harnesses breaks that assumption two ways:

- A user running only Claude Code should not need a Copilot installation — or a fabricated
  `~/.copilot` directory — for Munshi to register and operate.
- The SQLite store is already multi-source (`UNIQUE(source_kind, source_session_id)`, source-scoped
  opens), so per-harness state directories would fragment one logical archive without benefit.

## Decision

- Munshi's state directory is a harness-neutral home resolved as: explicit `--state-dir` flag,
  then `$MUNSHI_HOME`, then `~/.munshi`. It holds `config.json`, `munshi.db`, and `locks/`,
  exactly what `$COPILOT_HOME/munshi` held before.
- The registration lock moves from `$COPILOT_HOME/hooks/.munshi-registration.lock` to
  `<state>/locks/.munshi-registration.lock` (dot-prefixed so it can never collide with a
  `locks/<session_id>.lock` file). Lock semantics are unchanged: persistent owner-only 0600 file,
  nonblocking OS advisory lock held for the whole operation.
- `config.json` gains a `harnesses` section recording each harness home this registration manages
  (`copilot_home` now; `claude_home` next). Hook installation and the Copilot session-ID transcript
  fallback resolve through these recorded homes instead of deriving the Copilot home from the state
  directory's parent. When no Copilot home is recorded, Copilot-specific fallback and discovery are
  skipped rather than guessed.
- Registration installs hooks per selected harness target; Copilot's `hooks/munshi.json` format is
  unchanged. `unregister`, `project`, and `delivery` commands scope by state directory; unregister
  removes the hook installations the configuration records.

## No migration

No migration code is shipped. Munshi has never been registered on any machine this repository
serves (verified: no `~/.copilot/munshi`, no managed hook files); there is no legacy state to
carry forward. ADR 0004's rebuild path (`hook recover --rebuild-state`, archives as authority)
would cover any hypothetical straggler anyway.

## Consequences

- Copilot behavior is unchanged apart from paths: hooks still live in `$COPILOT_HOME/hooks`, but
  state and the lock live in the Munshi home, and `COPILOT_HOME` no longer influences state
  resolution — `MUNSHI_HOME` does.
- Hook subcommands resolve their state directory the same way (`$MUNSHI_HOME` → `~/.munshi`), so
  installed hook entries keep working if the user relocates the home via the environment variable.
- Claude Code registration (hooks in `~/.claude/settings.json`) can proceed without any Copilot
  installation, sharing the same store and archive tree.
