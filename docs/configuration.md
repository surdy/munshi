# Configuration reference

Every setting in Munshi's owned configuration file, in one place. The file lives at
`$MUNSHI_HOME/config.json` (default `~/.munshi/config.json`), is written by `munshi register` and
the section-owning commands below, and is read on every hook, worker, and CLI invocation. Munshi
owns this file: it refuses to touch a config it does not positively recognize, writes it atomically
with `0600` permissions, and serializes every write behind the registration lock
(`locks/.munshi-registration.lock`). Prefer the commands over hand-editing; an edit that leaves the
file unrecognizable blocks processing until repaired (see
[`troubleshooting.md`](troubleshooting.md)).

## Schema version and migration

| Setting | Value |
| --- | --- |
| `version` | Current schema version: **2**. |

Version 2 (issue #36) replaced version 1's scattered pair — a top-level `remote_delivery` bool plus
a `delivery` section — with the single self-contained `summary_delivery` section documented below.
A version-1 file still loads: `remote_delivery` maps to `summary_delivery.enabled` and every
`delivery.*` field carries over losslessly, and the migrated file is persisted as version 2 on the
next configuration load (or write), under the registration lock like every other config write.
`munshi doctor` reports the recorded version as the `config-version` check.

The migration is one-way and triggered by any configuration read: the first invocation of a v2
binary rewrites the file, after which a v1 binary rejects it (`MalformedOwnedFile`) and — because
hooks fail open — capture silently stops for any harness whose hook files still point at the old
executable. After upgrading, re-run `munshi register` if any hook path changed; downgrading
requires re-registering with the old binary.

## Top-level settings

Written by `munshi register` from its flags; re-run `register` to change them.

| Setting | Meaning |
| --- | --- |
| `summarizer.executable` | Absolute path of the summary executable transcripts are piped to. `--summarizer`. |
| `summarizer.args` | Arguments forwarded to the summarizer. Transcript content is only ever sent on stdin. Repeatable `--summarizer-arg`. |
| `output_directory` | Absolute root of the Munshi-owned Markdown archives. `--output-dir`. |
| `state_directory` | Absolute state home this registration owns; must match the `--state-dir`/`$MUNSHI_HOME` commands run against. |
| `archive_git_history` | `true` when the output directory is a dedicated Git repository with one commit per summary revision. `--archive-git-history` (default `false`). When enabled alongside summary delivery, delivery must be versioned (issue #9). |
| `local_archival_enabled` | Always `true` for an active registration; part of the recognition contract. |
| `transcript_processing_accepted` | Records the accepted transcript-processing disclosure; always `true` for an active registration. |
| `project_origin` | Project-identity source; always `agent_stop_cwd` in v1/v2 of Munshi. |

## `limits` — summarizer run bounds

Defaults in parentheses; set at registration time by the matching `--…` flag.

| Setting | Meaning |
| --- | --- |
| `timeout_ms` | Summarizer wall-clock timeout per invocation (`300000`). |
| `max_source_bytes` | Largest raw transcript Munshi will read (`8388608`). |
| `max_input_bytes` | Largest normalized input piped to the summarizer (`1048576`). |
| `max_stdout_bytes` | Cap on summarizer stdout (`262144`). |
| `max_stderr_bytes` | Cap on captured summarizer stderr (`65536`). |
| `max_event_text_bytes` | Per-event extraction threshold (`131072`): content larger than this is extracted as an `outputs/<sha256>` snapshot artifact and elided from summarizer input (ADR 0010). Manual archival uses the same threshold. |
| `chunk_threshold_bytes` | Chunked map-reduce trigger (`6291456`, issue #48): a session whose measured one-shot request exceeds this is summarized in per-segment chunks plus a reduce pass instead of one shot (or the input-limit placeholder floor). Also the hard cap on any single chunk/reduce request. Additive with a serde default — configurations written before issue #48 load unchanged, no version bump. `--chunk-threshold-bytes`. |
| `chunk_size_bytes` | Approximate serialized-events payload each chunk request targets on the chunked path (`2097152`, issue #48). Chunks split only on event boundaries, so individual chunks may run over or under. Additive with a serde default, like `chunk_threshold_bytes`. `--chunk-size-bytes`. |

## `policy` — cost and scope controls

Budgets come from `register` flags; `disabled_projects` is owned by `munshi project
disable`/`enable` and survives re-registration (issue #31).

| Setting | Meaning |
| --- | --- |
| `max_calls_per_hour` | Summarizer invocations allowed per project per rolling hour (`10`). |
| `max_calls_per_day` | Summarizer invocations allowed per project per rolling day (`50`). |
| `max_concurrency` | Sessions summarized concurrently across all projects (`2`, minimum 1). |
| `disabled_projects` | Canonical project identities excluded from future processing and delivery. Existing archives are untouched. |

Per-project `.munshi.toml` overrides layer on top of this section at resolution time; they are
never written into `config.json` (see [`user-guide.md`](user-guide.md)).

## `summary_delivery` — opt-in Notesmith delivery of current summaries

Owned by the `munshi summary-delivery` commands (`configure`, `enable`, `disable`; `delivery`
remains a deprecated alias). Publishes the *current* summary revision of each archived session to a
Notesmith vault, strictly downstream of local archival (ADR 0006). The section is self-contained —
enablement lives inside it, mirroring `archive_upload`.

| Setting | Meaning |
| --- | --- |
| `enabled` | Whether summary delivery runs after a successful local archive. Opt-in; `false` by default. Enabling requires an addressable sink (endpoint + vault). |
| `endpoint` | Base URL of the Notesmith daemon, e.g. `http://127.0.0.1:27183`. |
| `vault` | Target Notesmith vault name. |
| `folder` | Optional vault-relative folder Munshi-owned session notes are filed under. |
| `credential` | Where the bearer credential is read from at delivery time, or absent for an unauthenticated local daemon. Either `{"source": "env", "var": …}` or `{"source": "keychain", "service": …, "account": …}`. The secret itself is never stored. |
| `max_attempts` | Bounded delivery attempts before a session's delivery is parked as a dead letter (`5`). |
| `provision_history` | For versioned delivery (issue #9): `true` lets Munshi explicitly configure the vault's revision-history capability when absent instead of only verifying it (`false` by default). |

## `archive_upload` — opt-in Patwari upload of full session snapshots

Owned by the `munshi archive-upload` commands. Uploads each revision's full snapshot — rendered
summary, verbatim transcript, extracted outputs — to a Patwari archive server, in parallel with and
independent of summary delivery (ADR 0009).

| Setting | Meaning |
| --- | --- |
| `enabled` | Whether archive upload runs after a successful local archive. Opt-in; `false` by default. Enabling requires a configured endpoint. |
| `endpoint` | Base URL of the Patwari archive server, e.g. `http://127.0.0.1:8080`. |
| `client_id` | The persistent client UUID Munshi registers and uploads under. Minted once by `archive-upload configure`, reused verbatim, and durable across operational-database rebuilds (ADR 0004) — which is why it lives here rather than in SQLite. |
| `max_attempts` | Bounded upload attempts before a session's upload is parked as a dead letter (`5`). |

## `harnesses` — registered hook installations

Recorded by `munshi register` so `unregister`, `doctor`, and recovery know which harness homes this
registration manages (ADR 0008). Each key is absent when that harness is not registered.

| Setting | Meaning |
| --- | --- |
| `copilot_home` | Copilot home whose `hooks/munshi.json` this registration owns. |
| `claude_home` | Claude Code home whose `settings.json` carries Munshi's managed `Stop` and `SessionEnd` entries. |

## Related files

- `munshi.db` — rebuildable SQLite operational state; never configuration.
- `locks/` — the registration lock and per-session worker locks.
- `.munshi.toml` — optional per-project policy override, discovered upward from a session's origin
  directory (see [`user-guide.md`](user-guide.md)).
