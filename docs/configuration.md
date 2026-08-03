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
| `summarizer.env` | Environment variables set on every summarizer invocation, on top of the inherited environment. Opaque to Munshi — it defines no keys itself; the wrapper contract ([`summarizers.md`](summarizers.md)) gives them meaning, for example `MUNSHI_CHUNK_MODEL`/`MUNSHI_REDUCE_MODEL` for the contrib wrappers' per-phase models. Merged before Munshi's own per-invocation variables (`MUNSHI_SUMMARIZER_PHASE`), which win on conflict; keys in the reserved `MUNSHI_SUMMARIZER_*` namespace are rejected at register. Additive with a serde default (no version bump). Repeatable `--summarizer-env KEY=VALUE`. |
| `output_directory` | Absolute root of the Munshi-owned Markdown archives. `--output-dir`. |
| `state_directory` | Absolute state home this registration owns; must match the `--state-dir`/`$MUNSHI_HOME` commands run against. |
| `archive_git_history` | `true` when the output directory is a dedicated Git repository with one commit per summary revision. `--archive-git-history` (default `false`). When enabled alongside summary delivery, delivery must be versioned (issue #9). |
| `local_archival_enabled` | Always `true` for an active registration; part of the recognition contract. |
| `transcript_processing_accepted` | Records the accepted transcript-processing disclosure; always `true` for an active registration. |
| `project_origin` | Project-identity source; always `agent_stop_cwd` in v1/v2 of Munshi. |

## `limits` — summarizer run bounds

Defaults in parentheses; set at registration time by the matching `--…` flag. Sessions that fail
deterministically on a size cap park permanently; `munshi doctor` surfaces them as the
`size-cap-parked` check, naming the limit flag to raise (issue #41). The two size knobs that
constrain each other, `max_input_bytes` and `chunk_threshold_bytes`, are covered together under
[the size-knob relation](#the-size-knob-relation) below.

| Setting | Meaning |
| --- | --- |
| `timeout_ms` | Summarizer wall-clock timeout per invocation (`300000`). |
| `max_source_bytes` | Largest raw transcript Munshi will read (`67108864`, 64 MiB). Sized for real agentic sessions (issue #41): long sessions routinely produce 10–60 MiB transcripts, and ~9% of the first full backlog exceeded the old 8 MiB default and parked as deterministic `source-failed`. |
| `max_input_bytes` | Absolute never-exceed backstop on the normalized input piped to the summarizer (`8388608`, 8 MiB; issue #41 raised it from 1 MiB, keeping the ~8× raw:normalized ratio with `max_source_bytes`). Must be **at least** `chunk_threshold_bytes` — see [the size-knob relation](#the-size-knob-relation) below. `--max-input-bytes`. |
| `max_stdout_bytes` | Cap on summarizer stdout (`262144`). |
| `max_stderr_bytes` | Cap on captured summarizer stderr (`65536`). |
| `max_event_text_bytes` | Per-event extraction threshold (`131072`): content larger than this is extracted as an `outputs/<sha256>` snapshot artifact and elided from summarizer input (ADR 0010). Manual archival uses the same threshold. |
| `chunk_threshold_bytes` | Chunked map-reduce trigger (`2621440`, issue #48): a session whose measured one-shot request exceeds this is summarized in per-segment chunks plus a reduce pass instead of one shot. Also the hard cap on any single chunk/reduce request. The default is token-calibrated (issue #48 live calibration): the backend boundary is ~922k tokens, and at observed byte/token ratios of ~3.2–4.5 the earlier 6 MiB byte-calibrated default still admitted one-shot rejections; 2.5 MiB stays under the token limit at the densest observed ratio. Additive with a serde default — configurations written before issue #48 load unchanged, no version bump. `--chunk-threshold-bytes`. |
| `chunk_size_bytes` | Approximate serialized-events payload each chunk request targets on the chunked path (`1572864`, issue #48; sized against the token-calibrated threshold above). Chunks split only on event boundaries, so individual chunks may run over or under. Additive with a serde default, like `chunk_threshold_bytes`. `--chunk-size-bytes`. |

### The size-knob relation

`chunk_threshold_bytes` and `max_input_bytes` are not peers, and the invariant between them is
`max_input_bytes >= chunk_threshold_bytes`.

- **`chunk_threshold_bytes` is the operative bound.** It routes every session — over it the
  request is chunked, at or under it the request runs one-shot — and it is simultaneously the
  ceiling on each individual chunk and reduce request. It is the knob to tune against what your
  summarizer backend actually accepts.
- **`max_input_bytes` is an absolute never-exceed backstop.** Since chunking landed (issue #48) it
  can only bind on the one-shot path, which the threshold already bounds, so on a correctly
  configured Munshi it never fires: it exists to stop pathological input from reaching the
  summarizer at all, not to size normal work.

Setting the cap *below* the threshold is therefore never useful and is actively harmful: it
recreates the pre-issue-#48 band in which every request between the two values fails
deterministically under `summary-input-limit` and floors to a placeholder summary instead of being
chunked or summarized. `munshi register` and `munshi archive` reject the inverted relation at the
flag, before writing configuration or invoking anything (issue #52), and `munshi doctor` re-checks
a hand-edited `config.json` as the `input-cap-relation` warning. Keep the same relation in mind for
a `.munshi.toml` project override of `max_input_bytes`, which narrows the cap for one project
without changing the global threshold.

A future config version may fold the backstop away entirely and leave a single size knob; until
then, raise the threshold — not the cap — when large sessions need more room.

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

## `memory_sync` — opt-in mirroring of harness auto-memory (issue #59)

Owned by the `munshi memory-sync` commands (`configure`, `enable`, `disable`, `status`, `run`).
Mirrors each registered harness's auto-memory directories (`<claude_home>/projects/<slug>/memory/`)
into a Notesmith *document* vault — never the fact-memory vault — with the vault's per-vault Git
history as the snapshot mechanism (the issue #9 machinery; history is required, so an absent
capability blocks rather than degrades). Files are mirrored verbatim; identity and correlation ride
in a per-directory `<machine>/<slug>.manifest.md` note and the correlated commit message
`munshi memory <machine>:<slug> revision <n>`. Collection is read-only, change detection is a
per-file sha256 manifest (an unchanged directory never contacts the sink), and the whole path is
strictly downstream of archival (ADR 0006): triggered after each archive and drained by
`munshi tick`, with bounded retries into a dead letter.

| Setting | Meaning |
| --- | --- |
| `enabled` | Whether memory directories are mirrored after archival and from the tick. Opt-in; `false` by default. Enabling requires endpoint + vault + machine label. |
| `endpoint` | Base URL of the Notesmith daemon, e.g. `http://127.0.0.1:27183`. |
| `vault` | Target Notesmith vault name (a document vault). |
| `folder` | Optional vault-relative folder the per-machine memory trees are filed under; routes are `[folder/]<machine>/<slug>/<file>`. |
| `credential` | Where the bearer credential is read from at sync time; same shape as `summary_delivery.credential`. Never the secret itself. |
| `max_attempts` | Bounded sync attempts before a memory directory is parked as a dead letter (`5`). Revive with `memory-sync run --force`. |
| `machine_label` | The one canonical machine label mirrored paths are routed under. Chosen at configure time (`--machine`, else the sanitized hostname) and persisted — never re-derived per run, so one physical machine can never appear as two. |
| `machine_id` | The `archive_upload.client_id` captured at configure time when that section is configured, carried in the manifest so the memory mirror and the Patwari archive correlate without name matching. |
| `provision_history` | `true` lets Munshi explicitly configure the vault's revision-history capability when absent instead of only verifying it (`false` by default). |

## `harnesses` — registered hook installations

Recorded by `munshi register` so `unregister`, `doctor`, and recovery know which harness homes this
registration manages (ADR 0008). Each key is absent when that harness is not registered.

| Setting | Meaning |
| --- | --- |
| `copilot_home` | Copilot home whose `hooks/munshi.json` this registration owns. |
| `claude_home` | Claude Code home whose `settings.json` carries Munshi's managed `Stop` and `SessionEnd` entries. |

## `summarizer_exhaust` — retention for the isolated summarizer home

Written by `munshi register` from its flags, like the top-level settings: re-running `register`
without `--summarizer-exhaust-home` turns retention off again. Absent — the default — means
`munshi tick` prunes nothing, which is how Munshi behaved before issue #60. Additive with a serde
default (no version bump), so configurations written before it load unchanged.

Munshi has no other record of the isolated home: the isolation lives inside the summarizer wrapper
(`contrib/copilot-summarizer.sh` sets `COPILOT_HOME`), which Munshi treats as an opaque executable.
Naming it here is what makes retention possible — and why `home` is validated against every home
Munshi captures from. See [`user-guide.md`](user-guide.md) for the guards and the rationale.

| Setting | Meaning |
| --- | --- |
| `home` | Absolute path of the isolated summarizer home whose `session-state/` entries and session store `munshi tick` prunes. Refused at `register`, and reported as a `doctor` error, when it equals, contains, or is contained by a registered harness home or `~/.copilot`. `--summarizer-exhaust-home`. |
| `retention_days` | Age in whole days above which a `session-state/` entry is deleted. `0` keeps everything, as does an absent `home` (`7`). `--summarizer-exhaust-retention-days`. |

## Related files

- `munshi.db` — rebuildable SQLite operational state; never configuration.
- `locks/` — the registration lock and per-session worker locks.
- `.munshi.toml` — optional per-project policy override, discovered upward from a session's origin
  directory (see [`user-guide.md`](user-guide.md)).
