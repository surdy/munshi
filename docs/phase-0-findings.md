# Phase 0 Copilot CLI 1.0.70 findings

These findings combine official documentation, privacy-safe static inspection of installed Copilot
CLI 1.0.70, sanitized hook fixtures, a fully synthetic transcript-envelope fixture, and reported
aggregate live observations from macOS. The final integration passes did not inspect raw personal
session content or invoke Copilot. The temporary hook configuration used for live observation was
removed.

## Environment and evidence

| Field | Observation |
| --- | --- |
| Observation date | 2026-07-11 |
| Platform | macOS |
| Copilot CLI | 1.0.70 |
| Committed evidence | Sanitized hook JSON plus synthetic JSONL envelope fixture under `fixtures/copilot-1.0.70/` |
| Raw transcripts | Not committed or inspected during integration |
| Structured-summary input | Synthetic only |
| Force-close attempts | Two |

## Validated contract findings

| Area | Finding | Evidence |
| --- | --- | --- |
| User hook path | `~/.copilot/hooks/munshi-phase0.json` fired with direct `exec`/`args` command hooks. `~/.copilot/config/hooks/munshi-phase0.json` did not fire. Config is loaded at CLI startup. | Live observation plus public hook documentation |
| `agentStop` payload | Contains `timestamp`, `cwd`, `sessionId`, `transcriptPath`, and `stopReason: "end_turn"`. | Sanitized live fixtures |
| `sessionEnd` payload | Contains `sessionId`, `timestamp`, `cwd`, and `reason`, with no `transcriptPath`; installed source permits optional `error`. | Sanitized live fixtures plus installed 1.0.70 source |
| Transcript envelope | Observed JSONL records use `id`, `timestamp`, `parentId`, `type`, and `data`. Installed schemas also permit optional `agentId`. | Structural live inspection plus installed schemas |
| Normal hook order | In all three clean observed flows, `agentStop` preceded `sessionEnd` by 30–69 ms. This records ordering, not whether hook work blocks shutdown. | Sanitized fixture timestamps |
| Resume identity | The same session identity and session-state path were reused after resume. The identifier itself is not committed. | Live observation |
| Early interruption | Ctrl-C before an agent turn completed exited status 1, emitted no `agentStop`, and emitted `sessionEnd` with `reason: "user_exit"`. | Live observation and sanitized fixture |
| Force close | Two SIGKILL attempts emitted neither `agentStop` nor `sessionEnd`. Hook-only recovery is insufficient. | Live observation |
| Structured summary | Synthetic stdin through `munshi-probe summarize --binary copilot --arg=-s --arg=--no-ask-user` produced valid `title`/`summary` JSON twice. | Live observation |
| Unauthenticated CLI | Empty temporary `COPILOT_HOME` exited 0 with 223 stdout bytes, 0 stderr bytes, and non-JSON setup guidance. The probe classified malformed JSON. | Live observation |
| Quota | Changelog confirms quota guidance exists, but exact strings and exit behavior were intentionally not tested. | Official changelog only |
| Stop-hook failure behavior | Hook failure and timeout generally fail open for stop hooks; `sessionEnd` output is ignored. | Authoritative documentation/runtime behavior |
| Version drift note | `preToolUse` exit-code-2 behavior differs between documentation and the installed 1.0.70 changelog. This is unrelated to stop hooks but confirms the need for versioned verification. | Documentation/changelog comparison |

Official docs still do not define native structured-summary output. Installed 1.0.70 has an
undocumented `--output-format json` that emits JSONL event objects; it is not the validated
`title`/`summary` contract. No documented child timeout exists, so the wrapper owns timeout and
process-group cancellation.

## Lifecycle matrix

| Scenario | Hook events and order | Identity/path | Timing and result | Sanitized fixture | Status |
| --- | --- | --- | --- | --- | --- |
| Noninteractive clean end | `agentStop` then `sessionEnd`; end reason redacted and not inferred | `agentStop` supplied transcript path | ~46 ms between hooks; structured probe confirmation took 6.48 s | `noninteractive/agent-stop.json`, `noninteractive/session-end.json` | Observed |
| Resumed clean end | `agentStop` then `sessionEnd`; reason `complete` | Session identity and session-state path remained stable | ~30 ms between hooks | `resumed/agent-stop.json`, `resumed/session-end.json` | Observed |
| Interactive clean exit | `agentStop` then `sessionEnd`; end reason redacted and not inferred | `agentStop` supplied transcript path | ~69 ms between hooks; CLI reported 5.94 AI Credits and ~4 s model time | `interactive/agent-stop.json`, `interactive/session-end.json` | Observed |
| Ctrl-C before completed turn | No `agentStop`; `sessionEnd` reason `user_exit` | No hook-provided transcript path | Ctrl-C at ~2.5 s; process exit 1 | `interrupted/session-end.json` | Observed |
| SIGKILL during active session | Neither lifecycle hook fired in two attempts | No hook-provided identity/path | No hook notification | None | Observed |

The first structured-summary run took 7.48 seconds while the hook was in the non-working
`~/.copilot/config/hooks` location. Its valid synthetic summary result is retained as invocation
evidence; its absent hooks are retained as path evidence.

## Transcript structural matrix

Only aggregate structural results are recorded. No raw JSONL is committed.

`fixtures/copilot-1.0.70/transcript/synthetic-envelope.jsonl` is sanitized synthetic content, not a
copy or transformation of a personal transcript. Its envelope and event categories are shaped only
from the aggregate observations above and the installed 1.0.70 schema. It contains one record for
each of `session.start`, `session.model_change`, `system.message`, `user.message`,
`assistant.turn_start`, `assistant.message`, `assistant.turn_end`, `hook.start`, `hook.end`,
`session.resume`, and `session.shutdown`, plus one optional `agentId` example. Minimal `data`
objects intentionally make no claim about private semantic schemas.

| Scenario | Bytes | Valid lines | Top-level keys | `type` counts | Status |
| --- | ---: | ---: | --- | --- | --- |
| Noninteractive initial | 34,236 | 9 | `id`, `timestamp`, `parentId`, `type`, `data` | `assistant.message` 1; `assistant.turn_end` 1; `assistant.turn_start` 1; `session.model_change` 1; `session.shutdown` 1; `session.start` 1; `system.message` 1; `user.message` 2 | Observed |
| After resume | 70,742 | 26 | `id`, `timestamp`, `parentId`, `type`, `data` | `assistant.message` 2; `assistant.turn_end` 2; `assistant.turn_start` 2; `hook.start` 4; `hook.end` 4; `session.model_change` 2; `session.resume` 1; `session.shutdown` 2; `session.start` 1; `system.message` 2; `user.message` 4 | Observed |
| Interactive clean exit | 35,008 | 13 | `id`, `timestamp`, `parentId`, `type`, `data` | `assistant.message` 1; `assistant.turn_end` 1; `assistant.turn_start` 1; `hook.start` 2; `hook.end` 2; `session.model_change` 1; `session.shutdown` 1; `session.start` 1; `system.message` 1; `user.message` 2 | Observed |
| Interrupted | Unknown | Unknown | Unknown | Unknown | Not structurally inspected |
| Force-closed | Unknown | Unknown | Unknown | Unknown | Not structurally inspected |

## Structured-summary and failure matrix

| Scenario | Exit/result | Stdout | Stderr | Latency | Probe classification | Status |
| --- | --- | ---: | ---: | --- | --- | --- |
| Synthetic summary, first run | Valid `title`/`summary` | Size not recorded | Size not recorded | 7.48 s | Success | Observed |
| Synthetic summary, documented hook path | Valid `title`/`summary` | Size not recorded | Size not recorded | 6.48 s | Success | Observed |
| Empty-home unauthenticated setup | Exit 0, non-JSON guidance | 223 bytes | 0 bytes | Not recorded | Malformed JSON | Observed |
| Quota exhausted | Not attempted | Unknown | Unknown | Unknown | Unknown | Not live-tested |
| Real Copilot timeout | Not attempted | Unknown | Unknown | Unknown | Unknown | Not live-tested |
| Oversized real output | Not attempted | Unknown | Unknown | Unknown | Unknown | Not live-tested |

Authentication classification cannot rely on process exit status alone. Phase 1 needs a setup
preflight and/or version-aware recognition of known authentication guidance. Quota guidance and
exit behavior remain version-sensitive and unobserved.

## Recovery design implications

1. Prefer and persist hook-provided `transcriptPath`.
2. For interrupted sessions without `agentStop`, Copilot CLI 1.0.70 can be handled by a
   version-pinned source adapter that derives
   `$COPILOT_HOME/session-state/<sessionId>/events.jsonl`, requires it to exist, and validates the
   expected envelope.
3. Treat that location as private implementation evidence, not a stable public contract. No
   documented CLI resolver exists; the installed experimental RPC is not a production dependency.
4. Leave recovery pending if the version-pinned fallback cannot be resolved safely.
5. Scan source session state opportunistically after later hooks or explicit recovery commands,
   because force-kill can provide no lifecycle notification.

## Remaining unknowns

- Exact quota guidance strings and exit behavior.
- Hook behavior for additional failure modes and future Copilot versions.
- Structured-summary reliability for large inputs and different models.
- Whether hook execution is synchronous enough to require detached finalization.
- A stable semantic cursor over evolving transcript event variants.

Authoritative references:

- [Running Copilot CLI programmatically](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/use-copilot-cli)
- [GitHub Copilot hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference)
