# Summarizers

Munshi never calls a model API directly. Every summary is produced by an external
**summarizer executable** that you choose and configure with `munshi register --summarizer
...` (or `munshi archive --summarizer ...` for one-off runs). This document describes the
contract Munshi and your summarizer must agree on, gives two working examples, and covers
what to know before writing your own.

## 1. How Munshi invokes your summarizer

For each session Munshi decides to summarize, it:

1. Builds one JSON object (the *request*) describing the session.
2. Spawns your summarizer binary with any `--summarizer-arg` values as arguments, and writes
   the request JSON to its **stdin**, then closes stdin.
3. Waits up to `--timeout-ms` (default 300000) for the process to exit, then kills the whole
   process group if it's still running.
4. Reads stdout, capped at `--max-stdout-bytes` (default 262144), and stderr, capped at
   `--max-stderr-bytes` (default 65536).
5. Requires **exactly one JSON object** on stdout (the *response*) matching the schema in
   [section 3](#3-the-required-output), and nothing else — no leading/trailing text, no
   Markdown fences, no NDJSON stream.
6. If the process exits nonzero, stdout isn't valid JSON, the JSON doesn't validate, or the
   timeout fires, the summary attempt fails. Munshi does not retry immediately; the session is
   left pending and picked up again later via the normal `munshi retry` / worker cycle.

Munshi itself makes no network calls to summarize a session — whatever your summarizer does
(call an API, run a local model, shell out to another CLI) is entirely up to it. Session
content is **only ever sent on stdin**; it is never passed as a command-line argument, so it
never appears in `ps` output or shell history. `--summarizer-arg` is for fixed flags (model
name, output format, `--no-ask-user`, etc.), not content.

## 2. The input request

Munshi writes one JSON object like this to your summarizer's stdin:

```json
{
  "contract_version": 2,
  "phase": "complete",
  "instruction": "Summarize this coding session as exactly one JSON object matching required_schema. Return every required field. Capture goals, meaningful completed work, decisions, files changed, commands and validation, and open items. Do not quote prompts, raw tool output, secrets, or substantial code. Use concise strings and arrays of strings. Return JSON only, with no Markdown fence or commentary.",
  "required_schema": {
    "title": "non-empty string",
    "goal": "non-empty string",
    "work_completed": "array of strings",
    "decisions": "array of strings",
    "files_changed": "array of strings",
    "commands_and_validation": "array of strings",
    "open_items": "array of strings",
    "tags": "array of strings"
  },
  "session": {
    "id": "claude-code:a1b2c3d4-...",
    "source_agent": "claude-code",
    "session_id": "a1b2c3d4-...",
    "project_identity": "github.com/you/your-repo",
    "repository": "you/your-repo"
  },
  "events": [
    { "kind": "user", "content": "Add a retry command for failed deliveries." },
    { "kind": "assistant", "content": "I'll add a `munshi delivery retry` subcommand..." },
    { "kind": "tool", "content": "cargo test -p munshi delivery::retry ... ok" }
  ],
  "ignored_unknown_event_count": 0
}
```

Field reference:

| Field | Meaning |
|---|---|
| `contract_version` | Request-envelope version, currently `2` (issue #48). Version 2 added `contract_version`, `phase`, and the chunked-session fields below as a strictly additive change: a request from a pre-v2 Munshi simply lacks these fields, and a below-threshold session's v2 request differs from v1 only by the two marker fields. |
| `phase` | `"complete"` (the ordinary one-shot request — treat an absent `phase` from an older Munshi the same way), `"chunk"`, or `"reduce"`. The same value is exported as the `MUNSHI_SUMMARIZER_PHASE` environment variable on every invocation, so shell wrappers can branch without parsing the JSON. See [Chunked marathon sessions](#chunked-marathon-sessions-contract-v2) below. |
| `instruction` | Fixed natural-language instruction for the model. Wording differs per phase and for fresh vs. revision requests, but the field is always present. |
| `required_schema` | Output field names and expected shape, spelled out for the model. Mirrors `StructuredSummary` exactly — use it, don't hardcode a copy that could drift. |
| `session.id` | Munshi's globally unique ID, `<source>:<session_id>`. |
| `session.source_agent` | Harness that captured the session: `copilot-cli`, `claude-code`, or `codex-cli`. |
| `session.session_id` | The harness's own session ID. |
| `session.project_identity` | Canonical project identity Munshi resolved the session to: the normalized Git remote (for example `github.com/you/your-repo`) or `local:sha256:<digest>` for repositories without one. |
| `session.repository` | Best-effort repository name, or `null` if none was resolved. |
| `previous_summary` | Present only when revising an already-archived summary (a resumed/updated session): the last accepted `StructuredSummary`, same shape as the required output. Field is omitted entirely on a first-time summary. |
| `events` | Normalized transcript: an ordered array of `{ "kind": "user" \| "assistant" \| "tool", "content": string }`. This is the full material you have — there is no separate raw transcript to fetch. An event's `content` may be a *claim ticket* rather than the original text (see below). On a `"chunk"` request this holds only the current segment's events; on a `"reduce"` request it is empty. |
| `chunk` | Present only on `phase: "chunk"` requests: `{ "index": n, "count": m, "previous_chunk_summary": … }`. `index` is this segment's 1-based ordinal among `count` segments; `previous_chunk_summary` (present from the second segment on) is the accepted summary of the immediately preceding segment, carried for continuity. |
| `chunk_summaries` | Present only on `phase: "reduce"` requests: the per-segment summaries to synthesize, in segment order, each shaped exactly like the required output. |
| `ignored_unknown_event_count` | Count of transcript records Munshi couldn't normalize into an event. Usually 0; nonzero just means some records were dropped before reaching you. |

### Claim tickets (elided oversized events)

When a single event's content is very large (over Munshi's per-event extraction threshold,
default 128 KiB, configurable via `limits.max_event_text_bytes`), Munshi does **not** truncate it
and does not send you the raw bytes. Instead the event's `content` is replaced by a one-line
**claim ticket** marker, and the full content is preserved as its own content-addressed snapshot
artifact (ADR 0010). The marker has exactly this shape:

```text
[munshi claim-ticket sha256:<hex> bytes:<n> label:<label>]
```

- `<hex>` — the original content's lowercase-hex SHA-256 (unprefixed). This is the content address:
  it also names the `outputs/<hex>` snapshot artifact and appears in the rendered summary's
  frontmatter artifact index.
- `<n>` — the original content size in bytes.
- `<label>` — the event's kind (`user`, `assistant`, or `tool`), a hint at what was elided.

The marker is a single line, deterministic (a pure function of the content and its kind), and stable
across retries. **Reference these markers; never invent them.** Treat a claim ticket as "content
that exists but was too large to inline here": you may mention that a large tool output / message was
produced and, if useful, cite its `sha256`, but do not fabricate the elided text or guess at its
contents. There is no way to redeem a ticket mid-summary — the summarizer contract stays one-shot.
A holder of the finished summary can later fetch the exact original bytes with
`munshi retrieve <sha256>` once the snapshot is archived.

When `previous_summary` is set, the instruction asks for a **complete replacement** summary,
not a diff or an append: read the prior summary, read the new events, and return a full
`StructuredSummary` that still covers everything still true plus what changed. Do not assume
the caller will merge fields for you — nothing downstream stitches old and new together.

### Chunked marathon sessions (contract v2)

A session whose one-shot request would exceed `limits.chunk_threshold_bytes` (default 2.5 MiB,
token-calibrated against real backend rejections; issue #48) is summarized as a **map-reduce**
instead of one shot. Munshi splits the normalized event stream on event boundaries into segments
of roughly `limits.chunk_size_bytes` (default 1.5 MiB) and invokes your summarizer several
times:

1. One `phase: "chunk"` request per segment, in order. Each carries only that segment's
   `events`, a `chunk` object with the segment's `index`/`count`, and — from the second segment
   on — `chunk.previous_chunk_summary`, the summary you returned for the preceding segment, as
   continuity context. The instruction asks for a summary of **this segment only**.
2. One `phase: "reduce"` request over the collected segment summaries (`chunk_summaries`,
   with `events` empty), whose instruction asks for one summary of the **entire session**. In
   the rare case the reduce input itself exceeds the threshold, Munshi first condenses groups
   of segment summaries through intermediate `"reduce"` requests and reduces again over the
   results. When the session is a revision, `previous_summary` rides on the final reduce
   request and the instruction asks for a complete replacement, as usual.

Every invocation uses the **same response schema** ([section 3](#3-the-required-output)) and the
same timeout/stdout/stderr bounds, and each one is charged against the per-project call budget
individually. A summarizer that simply forwards the request and instruction to its model —
which is what both contrib wrappers do — needs no code changes for chunking to work.

Munshi also exports the phase as the `MUNSHI_SUMMARIZER_PHASE` environment variable
(`complete` | `chunk` | `reduce`) on every invocation, so a shell wrapper can branch — for
example to pick a cheaper model for chunk passes — without parsing the request. Both contrib
wrappers honor two optional environment variables, defaulting to their current behavior when
unset: `MUNSHI_CHUNK_MODEL` and `MUNSHI_REDUCE_MODEL` select the model for chunk / reduce
invocations (Claude Code via `--model` over `CLAUDE_MODEL`; Copilot CLI via its `--model` flag,
passed only when the override is set). Model policy stays outside Munshi itself.

Your summarizer's environment on each invocation is the inherited environment, plus any
operator-configured `summarizer.env` entries (repeatable `munshi register --summarizer-env
KEY=VALUE` — the way to set `MUNSHI_CHUNK_MODEL`/`MUNSHI_REDUCE_MODEL` from Munshi's own
configuration instead of the ambient environment), plus Munshi's own `MUNSHI_SUMMARIZER_PHASE`,
in that order — Munshi's own variables always win on conflict, and configured keys in the
reserved `MUNSHI_SUMMARIZER_*` namespace are rejected at registration. Munshi passes the
configured map through opaquely; it is this wrapper contract that gives the keys meaning.

**Backward compatibility:** the v2 envelope is strictly additive. Old wrappers keep working for
every below-threshold session (the request only gained the two marker fields), and requests from
a pre-v2 Munshi — no `contract_version`, no `phase`, no phase environment variable — should be
treated as `phase: "complete"`.

## 3. The required output

Your summarizer must print **exactly one JSON object** to stdout, matching `StructuredSummary`:

```json
{
  "title": "Add munshi delivery retry command",
  "goal": "Let failed Notesmith deliveries be retried individually or in bulk without a full re-summarize.",
  "work_completed": [
    "Implemented `munshi delivery retry [SESSION_ID] [--all] [--force]`.",
    "Wired retry into the existing bounded-attempt/dead-letter state machine."
  ],
  "decisions": [
    "Reused the delivery attempt counter rather than adding a separate retry counter."
  ],
  "files_changed": [
    "crates/munshi/src/cli/delivery.rs",
    "crates/munshi/src/delivery/retry.rs"
  ],
  "commands_and_validation": [
    "cargo test -p munshi delivery::retry -- passed"
  ],
  "open_items": [
    "Decide whether --force should also reset the dead-letter timestamp."
  ],
  "tags": ["delivery", "cli", "retry"]
}
```

Validation rules Munshi enforces on the parsed object (violating any of these is a summary
failure):

- **Exactly these eight fields**, nothing else — `deny_unknown_fields` is on, so extra keys
  reject the whole object.
- `title`: non-empty, single line, ≤200 characters.
- `goal`: non-empty, ≤4000 characters (newlines allowed, but not a line starting with `## `,
  which Munshi's Markdown renderer reserves).
- The six list fields (`work_completed`, `decisions`, `files_changed`,
  `commands_and_validation`, `open_items`, `tags`) must each be a JSON array of strings, with
  **at least one item** — if there's genuinely nothing to report, use a placeholder like
  `"none"` rather than an empty array. Each item is non-empty, single line, ≤4000 characters
  (≤100 for `tags`), and each list holds at most 200 items.
- No control characters other than `\n`/`\t` in any string.
- Output must be **valid JSON and only JSON** — a Markdown code fence around it, a leading
  sentence like "Here's the summary:", or trailing commentary all cause a parse failure.

Common rejection causes in practice: the model wraps its answer in a ` ```json ``` ` fence; the
model returns an empty array for a list it considers not applicable instead of a placeholder;
the model adds an extra field (like `summary` or `notes`) alongside the required ones; the
process writes progress/log lines to stdout instead of stderr, corrupting the single JSON
object Munshi expects; or the process exceeds `--timeout-ms` or `--max-stdout-bytes`.

## 4. Hazard: summarizers that are themselves session-recording harnesses

**Read this before pointing Munshi at a coding-agent CLI.** Copilot CLI and Claude Code — the
two summarizers Munshi ships wrappers for, and the obvious choices if you already have one
installed — record a session of their own for every invocation. That is a feedback loop waiting
to happen, and it has happened in the field.

**The loop.** Munshi asks the summarizer for a summary. The harness answering that request opens
a brand-new session in *its* home directory, whose first user message is Munshi's request
envelope and whose reply is the summary. If that home is the one you registered with Munshi,
that session is indistinguishable from real work: it has a user request and an assistant reply,
so it is archive-worthy. Munshi's `Stop`/`SessionEnd` hooks fire on it and the recovery sweep
discovers it, so archiving N sessions creates N new sessions to archive — each of which creates
another. Observed live during a backlog drive: Copilot's session state grew from 576 to 1,438
directories, and several hundred of those exhaust sessions were summarized, archived, and
uploaded before anyone noticed (issue #37). Every one of them cost a billed model call.

**The requirement.** Any summarizer that records sessions must be isolated so that (a) the
sessions it records land **outside** the harness home you registered with Munshi, and (b) the
hooks `munshi register` installed **do not fire** for summarizer runs. Doing only one is not
enough: hooks that still fire will observe sessions whose transcripts Munshi cannot resolve, and
relocated sessions that still trigger hooks re-enter the pipeline anyway. Both shipped wrappers
do this, by different means, because the two harnesses expose different levers:

- [`contrib/copilot-summarizer.sh`](../contrib/copilot-summarizer.sh) points `COPILOT_HOME` at
  `~/.copilot-summarizer`, seeded once with symlinks to the real home's config and auth, with its
  own `session-state` and — deliberately — no `hooks` directory.
- [`contrib/claude-summarizer.sh`](../contrib/claude-summarizer.sh) passes
  `--no-session-persistence` (no transcript is written at all) and `--setting-sources ''` (the
  settings file holding Munshi's hooks is never loaded), and additionally relocates
  `CLAUDE_CONFIG_DIR` to `~/.claude-summarizer` once that home can authenticate. See the script's
  header for why the third measure is conditional: with a custom `CLAUDE_CONFIG_DIR`, Claude Code
  reads its credential from `$CLAUDE_CONFIG_DIR/.credentials.json` and does *not* fall back to the
  macOS Keychain, so on a Keychain-backed install the isolated home needs a one-time
  `CLAUDE_CONFIG_DIR="$HOME/.claude-summarizer" claude /login` (or an exported
  `CLAUDE_CODE_OAUTH_TOKEN`) before it can be used.

If you write your own wrapper around a session-recording backend, do the equivalent — and verify
it: run one summary by hand, then confirm no new session appeared in the registered home.

**The built-in guard is a backstop, not the fix.** Munshi recognizes a session whose *first* user
message is one of its own summary-request envelopes and settles it as **not-archive-worthy** with
the `summarizer-exhaust` diagnostic (surfacing on the `last failure:` line of `munshi status` /
`munshi doctor`, with the sessions themselves under `munshi sessions --state
not-archive-worthy`). Such a session is never summarized, uploaded, or delivered, so a
misconfigured wrapper costs one wasted discovery and no billed call. But the exhaust sessions
still pile up on disk in the harness's own home, and the guard only recognizes what Munshi itself
emitted — a wrapper that rewrites or re-wraps the request before handing it to its harness
defeats it. Seeing `summarizer-exhaust` at all means **your wrapper's isolation is missing or
broken**; fix the wrapper.

## 5. Example: Copilot CLI as summarizer

Copilot CLI can act as a summarizer directly, since its noninteractive mode reads a prompt
and can be told to answer without a fence:

```bash
munshi register --accept-transcript-processing \
  --summarizer /absolute/path/to/copilot \
  --summarizer-arg=-s \
  --summarizer-arg=--no-ask-user \
  --output-dir /absolute/path/to/munshi-summaries
```

`-s` runs Copilot CLI in noninteractive/scripting mode; `--no-ask-user` prevents it from
pausing to ask clarifying questions (there's no user to answer). See
[`docs/automatic-archive.md`](automatic-archive.md) for the full registration walkthrough.

## 6. Example: Claude Code via `contrib/claude-summarizer.sh`

Bare `claude -p` is not directly usable as a summarizer: it tends to wrap its JSON answer in a
` ```json ``` ` Markdown fence, and for small/trivial sessions it sometimes returns empty
arrays for optional lists — both of which fail validation (fenced output isn't valid JSON on
its own, and Munshi rejects empty required lists).

[`contrib/claude-summarizer.sh`](../contrib/claude-summarizer.sh) is a verified wrapper that
fixes both problems: it runs `claude -p` with an appended system prompt asking for a bare JSON
object, then pipes the output through a small Python filter that strips any Markdown fence and
replaces any empty list with `["none"]` before re-emitting the JSON. It has been verified
end-to-end against real sessions.

Before using it:

- Edit the `CLAUDE_BIN` and `CLAUDE_MODEL` defaults at the top of the script (both also
  overridable via environment) to match your local install (`which claude`, and whichever
  Claude Code model you want to pay for summarization with — a small/cheap model is usually
  enough). The optional `MUNSHI_CHUNK_MODEL`/`MUNSHI_REDUCE_MODEL` variables select a different
  model per phase for chunked marathon sessions (contract v2 above); set them persistently with
  `munshi register --summarizer-env` rather than the ambient environment.
- Make it executable: `chmod +x contrib/claude-summarizer.sh`.
- Point `--summarizer` at its **absolute** path:

```bash
munshi register --accept-transcript-processing \
  --summarizer /absolute/path/to/munshi/contrib/claude-summarizer.sh \
  --output-dir /absolute/path/to/munshi-summaries
```

The script takes no `--summarizer-arg`s of its own; all Claude Code flags are baked in so the
wrapper's stdin contract (whole request in, whole `StructuredSummary` out) stays exact.

## 7. Writing your own

A summarizer can be written in any language, as a script or a compiled binary — Munshi only
cares that it is executable and speaks the stdin/stdout contract above. At minimum it must:

1. Read all of stdin (the full `SummaryRequest` JSON) before producing output.
2. Ask whatever backend you use to fill in `required_schema`, instructing it to return bare
   JSON (no fence, no commentary) — most stray-output failures come from skipping this.
3. Post-process the backend's answer defensively: strip any Markdown fence, and backfill any
   empty required list with a placeholder like `["none"]`, the way
   `contrib/claude-summarizer.sh` does — don't trust the model to always comply.
4. Write exactly one JSON object matching `StructuredSummary` to stdout, nothing else. Send
   logs/diagnostics to stderr instead.
5. Exit 0 on success; any nonzero exit is treated as a failed attempt regardless of what was
   printed.

Handle `previous_summary` explicitly for good revision behavior: when present, feed it back
into your prompt as prior context and ask the model to merge/update rather than start over from
just the new `events` — this keeps titles/goals stable across a session's resumes. Ignoring it
still works, but revisions may drift each time.

Test a summarizer standalone before registering it, by handing it a sample request the same way
Munshi would — save a request like the one in [section 2](#2-the-input-request) to
`sample-request.json`, then:

```bash
cat sample-request.json | /absolute/path/to/your-summarizer | python3 -m json.tool; echo "exit=$?"
```

If `python3 -m json.tool` prints a clean, re-formatted object with all eight fields and no
error, your summarizer is producing valid output. Also try a session with genuinely little to
report, to confirm your placeholder-backfill logic covers empty lists.

You can also exercise a registered summarizer against one real session without registering
automatic capture, using [`munshi archive`](manual-archive.md).

## 8. Cost/privacy notes

- Munshi sends the **full normalized transcript** (`events`) of a session to your summarizer on
  every invocation — and by extension to whatever backend your summarizer calls (a hosted model
  API, a local model, etc.). Treat your summarizer choice as the actual data-handling boundary:
  Munshi does not filter, sample, or truncate transcript content for privacy before handing it
  over — only for size, and there the operative bound is `--chunk-threshold-bytes` (default
  2621440), which splits an oversized session across invocations rather than dropping content.
  `--max-input-bytes` (default 8388608) is only a never-exceed backstop above it; see
  [`configuration.md`](configuration.md#the-size-knob-relation).
- Invocations are bounded by per-project budgets so a busy project can't cause unbounded spend:
  `--max-calls-per-hour` (default 10) and `--max-calls-per-day` (default 50), plus
  `--max-concurrency` (default 2) across all projects. Once a budget is hit, further summaries
  for that project defer until the window rolls over rather than being dropped. A chunked
  marathon session (contract v2 above) makes one budgeted call **per chunk plus the reduce
  pass(es)**, so a single 20 MiB session can consume around fifteen calls; size the budgets
  accordingly if you archive marathons routinely.
- **v1 does not redact secrets.** If a transcript contains credentials, tokens, or other
  sensitive text, that text is sent to your summarizer verbatim like everything else. Choose a
  summarizer backend you're comfortable sending session content to, and keep that in mind for
  any project where transcripts might contain secrets.

## 9. Cross-links

- [`docs/getting-started.md`](getting-started.md) — installing and registering Munshi for the
  first time.
- [`docs/troubleshooting.md`](troubleshooting.md) — diagnosing failed/pending sessions,
  including summarizer failures, with `munshi doctor` and `munshi sessions --state failed`.
- [`docs/automatic-archive.md`](automatic-archive.md) — full `munshi register` walkthrough.
- [`docs/manual-archive.md`](manual-archive.md) — running `munshi archive` against one session.
