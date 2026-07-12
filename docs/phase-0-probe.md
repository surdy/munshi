# Phase 0 compatibility probe

`munshi-probe` is deliberately smaller than the planned Munshi product. It captures fixtures,
reports transcript structure, and tests a caller-selected structured-summary command. It does not
install hooks, discover sessions, invoke Copilot by default, archive sessions, or write SQLite.

Build it with:

```bash
cargo build -p munshi-probe
```

## Capture a hook payload

Pipe one JSON payload to `capture-hook`. Sanitization replaces every string value except values
explicitly allowlisted by the caller. Object keys, array shape, numbers, booleans, and nulls remain
unchanged.

```bash
munshi-probe capture-hook \
  --output path/to/sanitized-fixtures/agent-stop.json \
  --sanitize \
  --preserve-value agentStop \
  --preserve-value copilot
```

Sanitized JSON is fully produced in memory before any fixture file is created. The destination is
created with a same-directory temporary file and a no-clobber atomic persist; an existing fixture is
never replaced. Invalid JSON or sanitization failure leaves no destination file. Omitting
`--sanitize` intentionally stores the validated raw JSON and should be used only for non-sensitive
synthetic input.

## Inspect transcript structure

The inspector treats the input as JSON Lines and emits JSON metrics, never line content:

```bash
munshi-probe inspect-transcript \
  --input path/to/transcript.jsonl \
  --discriminator-key type \
  --discriminator-key event
```

Metrics include bytes read, logical line count, JSON-valid line count, top-level key frequencies,
and selected top-level discriminator counts. Discriminator labels are JSON scalar literals. Select
only keys whose values are known to be non-sensitive.

## Probe structured summary invocation

The executable is required; there is no default Copilot command. Input comes from `--input` or
standard input. Each `--arg` is forwarded unchanged. The documented Copilot noninteractive forms
accept piped input or `-p`, with `-s`, `--no-ask-user`, `--model`, and tool allow/deny flags. No
documented Copilot child timeout was found, so this wrapper owns timeout and process-group
cancellation.

```bash
munshi-probe summarize \
  --binary ./path/to/fake-copilot \
  --arg --structured-mode \
  --input path/to/synthetic-input \
  --timeout-ms 30000 \
  --max-stdout-bytes 1048576 \
  --max-stderr-bytes 65536
```

The child receives the input on standard input in a new process group. Timeout or output overflow
kills that group. Stderr is bounded and is never echoed by probe errors. Success requires stdout to
be one JSON value with non-empty `title` (at most 200 characters) and `summary` (at most 2,000
characters). Both fields are trimmed before validation and before being returned:

```json
{
  "title": "Compatibility probe",
  "summary": "Validated structured JSON invocation."
}
```

Official documentation does not define native structured-summary output. Installed Copilot CLI
1.0.70 has an undocumented `--output-format json`, but that mode emits JSONL event objects and is
not the `title`/`summary` contract above. Plain/silent model output prompted to emit exactly one JSON
object was validated twice with synthetic stdin on macOS and CLI 1.0.70, taking 7.48 seconds and
6.48 seconds. This proves the invocation shape, not reliability for private or large transcripts.

For a future explicitly authorized live run, keep the prompt and transcript on standard input, not
in process arguments:

```bash
{
  printf '%s\n' \
    'Return exactly one JSON object with non-empty string fields "title" and "summary".'
  cat path/to/synthetic-or-explicitly-approved-input
} | munshi-probe summarize \
  --binary copilot \
  --arg -s \
  --arg --no-ask-user \
  --arg --model \
  --arg "$MODEL"
```

Add version-appropriate tool-deny flags before a live run. Do not use the undocumented JSONL event
mode as proof of the summary contract. With an empty temporary `COPILOT_HOME`, CLI 1.0.70 exited
zero with 223 stdout bytes, no stderr, and non-JSON setup guidance; the probe correctly classified
that output as malformed JSON. Future authentication classification needs a setup preflight and/or
version-aware guidance detection, not exit status alone. Exact quota strings and exit behavior were
not live-tested. Tests use shell fake executables and consume no AI credits.

## Copilot boundaries

Authoritative documentation, privacy-safe static inspection of installed Copilot CLI 1.0.70, and
the recorded live observations establish these boundaries:

- User hooks are loaded from `~/.copilot/hooks` or `$COPILOT_HOME/hooks`; configuration is read at
  CLI startup. Live testing confirmed this path and found that `~/.copilot/config/hooks` did not
  fire.
- `agentStop` includes `timestamp`, `cwd`, `sessionId`, `transcriptPath`, and
  `stopReason: "end_turn"`.
- `sessionEnd` includes `sessionId`, `timestamp`, `cwd`, and `reason`; installed source permits an
  optional `error` and does not include `transcriptPath`.
- Installed transcripts use `~/.copilot/session-state/<UUID>/events.jsonl`. Event envelopes contain
  `id`, `timestamp`, `parentId`, `type`, and `data`, plus optional `agentId`. Runtime-package schemas
  exist, but are evidence for the installed version rather than a stable private contract.
- Stop-hook failures and timeouts generally fail open; `sessionEnd` output is ignored.
- An early interrupted session can emit only `sessionEnd`; SIGKILL can emit no hook notification.
  The source adapter must prefer hook-provided `transcriptPath`, then use only a version-pinned,
  existence-validated `$COPILOT_HOME/session-state/<sessionId>/events.jsonl` fallback. This installed
  path is private, not a public compatibility promise.

Committed fixtures under `fixtures/copilot-1.0.70/` contain sanitized hook payloads only. No raw
transcript or personal prompt content is committed. Exact quota behavior and broader model-output
reliability remain unobserved.

Authoritative references:

- [Running Copilot CLI programmatically](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/use-copilot-cli)
- [GitHub Copilot hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)
