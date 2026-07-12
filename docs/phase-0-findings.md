# Phase 0 research findings and live test matrix

This file separates authoritative documentation and privacy-safe static package inspection from
live behavior. No hook was installed, no personal session or transcript content was inspected, and
Copilot was not executed for these findings. Do not mark a live row observed without recording the
Copilot CLI version, platform, exact command shape, timestamp, fixture reference, and whether AI
credits could have been consumed.

## Research environment

| Field | Observation |
| --- | --- |
| Research recorded | 2026-07-11 |
| Evidence classes | Official GitHub documentation; privacy-safe static inspection of installed runtime package |
| Copilot CLI version | 1.0.70, static inspection only |
| Authentication state | Not inspected |
| Live model invocation | Not performed |
| Personal session content | Not inspected |

## Documentation and static-inspection findings

| Area | Finding | Evidence |
| --- | --- | --- |
| User hooks | Hook configuration is loaded from `~/.copilot/hooks` or `$COPILOT_HOME/hooks` when the CLI starts. Restart the CLI after configuration changes. | Authoritative documentation |
| `agentStop` payload | Fields are `timestamp`, `cwd`, `sessionId`, `transcriptPath`, and `stopReason`; documented stop reason is `end_turn`. | Authoritative hook documentation |
| `sessionEnd` payload | Fields are `sessionId`, `timestamp`, `cwd`, and `reason`. Installed source permits optional `error`; there is no `transcriptPath`. | Documentation plus installed 1.0.70 source/schema |
| Transcript location | Installed runtime writes `~/.copilot/session-state/<UUID>/events.jsonl`. | Privacy-safe installed-package inspection |
| Transcript envelope | Top-level event keys are `id`, `timestamp`, `parentId`, `type`, and `data`, with optional `agentId`. Schemas are present in the runtime package. | Privacy-safe installed-package inspection |
| Noninteractive input | Documented forms include piped standard input or `-p`, plus `-s`, `--no-ask-user`, `--model`, and tool allow/deny flags. Transcript content should remain on stdin. | Authoritative documentation |
| Timeout ownership | No documented child-process timeout was found. Munshi's wrapper must enforce timeout and process-group cancellation. | Documentation finding and probe design |
| Structured output | Official docs do not define native structured-summary output. Installed 1.0.70 has undocumented `--output-format json`, which emits JSONL event objects rather than one title/summary result. | Documentation plus privacy-safe installed-package inspection |
| Current summary strategy | Use plain/silent output with a prompt requiring exactly one JSON object, then validate `title` and `summary`. Do not treat JSONL event mode as this contract. | Probe decision pending live validation |
| Stop-hook failure behavior | Hook failure and timeout generally fail open for stop hooks; `sessionEnd` output is ignored. | Authoritative documentation/runtime behavior |
| Version drift note | `preToolUse` exit-code-2 behavior differs between documentation and the installed 1.0.70 changelog. This is unrelated to stop hooks but shows that versioned verification is necessary. | Documentation/changelog comparison |

Authoritative references:

- [Running Copilot CLI programmatically](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/use-copilot-cli)
- [GitHub Copilot hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)

## Live lifecycle matrix

| Scenario | Hook events and order | Session ID behavior | Transcript reference behavior | Timing/blocking | Sanitized fixture | Status |
| --- | --- | --- | --- | --- | --- | --- |
| Interactive normal end | Unknown | Unknown | Unknown | Unknown | None | Not observed |
| Noninteractive normal end | Unknown | Unknown | Unknown | Unknown | None | Not observed |
| Resumed session | Unknown | Unknown | Unknown | Unknown | None | Not observed |
| Interrupted session | Unknown | Unknown | Unknown | Unknown | None | Not observed |
| Force-closed session | Unknown | Unknown | Unknown | Unknown | None | Not observed |

Payload fields and installed transcript structure are known above. Exact live firing order,
resume/interruption behavior, and path behavior across lifecycle variants remain unobserved.

## Live transcript matrix

Record only structural output from `inspect-transcript` plus sanitized fixture paths.

| Scenario | Bytes | Lines | JSON-valid lines | Top-level keys | Selected discriminators | Status |
| --- | ---: | ---: | ---: | --- | --- | --- |
| Interactive normal end | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Noninteractive normal end | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Resumed session | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Interrupted/force-closed | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |

## Live structured-summary matrix

| Scenario | Exit/signal | Error classification | Stdout bytes | Stderr bytes | Latency | Valid Phase 0 JSON | Status |
| --- | --- | --- | ---: | ---: | --- | --- | --- |
| Successful invocation | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Unauthenticated | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Quota exhausted | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Timeout | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Malformed model output | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Oversized stdout/stderr | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |

Exact authentication and quota exit forms, model-output reliability, and latency remain
unobserved. The undocumented JSONL event output is not a successful Phase 0 summary result.

## Observation notes

For each run, record:

1. Exact command with secrets and private paths removed.
2. Whether the run was interactive, noninteractive, resumed, interrupted, or force-closed.
3. Sanitized fixture filenames and their capture allowlists.
4. Structural transcript report.
5. Exit status, timeout, byte limits, elapsed time, and validation result.
6. Any behavior that changes a README assumption or resolves a Phase 0 open question.
