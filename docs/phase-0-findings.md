# Phase 0 findings template

This file is a recording template, not a claim about observed Copilot CLI behavior. Do not mark a
row observed without recording the Copilot CLI version, platform, exact command shape, timestamp,
fixture reference, and whether AI credits could have been consumed.

## Environment

| Field | Observation |
| --- | --- |
| Date/time | Not observed |
| Platform and version | Not observed |
| Copilot CLI version | Not observed |
| Authentication state | Not observed |
| Model and command flags | Not observed |
| Probe commit | Not observed |

## Lifecycle matrix

| Scenario | Hook events and order | Session ID behavior | Transcript reference behavior | Timing/blocking | Sanitized fixture | Status |
| --- | --- | --- | --- | --- | --- | --- |
| Interactive normal end | Unknown | Unknown | Unknown | Unknown | None | Not observed |
| Noninteractive normal end | Unknown | Unknown | Unknown | Unknown | None | Not observed |
| Resumed session | Unknown | Unknown | Unknown | Unknown | None | Not observed |
| Interrupted session | Unknown | Unknown | Unknown | Unknown | None | Not observed |
| Force-closed session | Unknown | Unknown | Unknown | Unknown | None | Not observed |

## Transcript structure

Record only structural output from `inspect-transcript` plus sanitized fixture paths.

| Scenario | Bytes | Lines | JSON-valid lines | Top-level keys | Selected discriminators | Status |
| --- | ---: | ---: | ---: | --- | --- | --- |
| Interactive normal end | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Noninteractive normal end | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Resumed session | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Interrupted/force-closed | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |

## Structured-summary matrix

| Scenario | Exit/signal | Error classification | Stdout bytes | Stderr bytes | Latency | Valid Phase 0 JSON | Status |
| --- | --- | --- | ---: | ---: | --- | --- | --- |
| Successful invocation | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Unauthenticated | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Quota exhausted | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Timeout | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Malformed model output | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |
| Oversized stdout/stderr | Unknown | Unknown | Unknown | Unknown | Unknown | Unknown | Not observed |

## Observation notes

For each run, record:

1. Exact command with secrets and private paths removed.
2. Whether the run was interactive, noninteractive, resumed, interrupted, or force-closed.
3. Sanitized fixture filenames and their capture allowlists.
4. Structural transcript report.
5. Exit status, timeout, byte limits, elapsed time, and validation result.
6. Any behavior that changes a README assumption or resolves a Phase 0 open question.
