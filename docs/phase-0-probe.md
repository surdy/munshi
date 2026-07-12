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
  --output fixtures/agent-stop.json \
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
  --input fixtures/transcript.jsonl \
  --discriminator-key type \
  --discriminator-key event
```

Metrics include bytes read, logical line count, JSON-valid line count, top-level key frequencies,
and selected top-level discriminator counts. Discriminator labels are JSON scalar literals. Select
only keys whose values are known to be non-sensitive.

## Probe structured summary invocation

The executable is required; there is no default Copilot command. Input comes from `--input` or
standard input. Each `--arg` is forwarded unchanged.

```bash
munshi-probe summarize \
  --binary ./path/to/fake-copilot \
  --arg --structured-mode \
  --input fixtures/transcript.jsonl \
  --timeout-ms 30000 \
  --max-stdout-bytes 1048576 \
  --max-stderr-bytes 65536
```

The child receives the input on standard input in a new process group. Timeout or output overflow
kills that group. Stderr is bounded and is never echoed by probe errors. Success requires stdout to
be one JSON value with non-empty `title` (at most 200 characters) and `summary` (at most 2,000
characters):

```json
{
  "title": "Compatibility probe",
  "summary": "Validated structured JSON invocation."
}
```

Authentication, quota, model flags, and Copilot-specific arguments remain live-observation
questions. Tests use shell fake executables and consume no AI credits.
