# Phase 0 Claude Code 2.1.205 findings

These findings combine official hook documentation, sanitized live hook fixtures, structural
inspection of a live transcript produced during the probe itself, and a fully synthetic
bookkeeping-record fixture. The probe session content was authored for the probe (no personal
session content was inspected or committed). The temporary hook configuration used for live
observation was removed and the user's `~/.claude/settings.json` was restored byte-for-byte.

## Environment and evidence

| Field | Observation |
| --- | --- |
| Observation date | 2026-07-18 |
| Platform | macOS |
| Claude Code | 2.1.205 |
| Adapter transcript pin | 2.1.44 (re-validated structurally at 2.1.205; envelope unchanged) |
| Committed evidence | Sanitized hook JSON under `fixtures/claude-code-2.1.205/hooks/`; synthetic bookkeeping transcript under `fixtures/claude-code-2.1.205/transcript/` |
| Raw payloads | Captured to a private scratch directory, never committed |
| Force-close attempts | One SIGKILL, one SIGINT |

## Validated contract findings

| Area | Finding | Evidence |
| --- | --- | --- |
| Hook configuration | Hooks live in `~/.claude/settings.json` under a top-level `hooks` object keyed by event name (`Stop`, `SessionEnd`), each an array of matcher groups whose `hooks` arrays hold `{"type": "command", "command": <shell string>, "timeout": <seconds>}`. No matcher is needed for these events. Configuration is read at session startup. | Live observation plus official hooks reference |
| Settings file volatility | Claude Code itself rewrites `~/.claude/settings.json` during normal use (observed key set changing between reads in one day). Any Munshi installer must merge and preserve unknown keys, and must expect concurrent rewrites. | Live observation |
| Payload transport | One JSON object on stdin per hook invocation. No timestamp field is provided on either event; ingestion must stamp receipt time. | Sanitized live fixtures |
| `Stop` payload | `session_id`, `transcript_path`, `cwd`, `prompt_id`, `permission_mode`, `hook_event_name: "Stop"`, `stop_hook_active`, `last_assistant_message`, `background_tasks`, `session_crons`. Fires once per completed assistant turn. `last_assistant_message` carries conversation content — hook handlers must never log or persist the raw payload. | Sanitized live fixtures |
| `SessionEnd` payload | `session_id`, `transcript_path`, `cwd`, `prompt_id`, `hook_event_name: "SessionEnd"`, `reason`. `transcript_path` was present on every observed `SessionEnd`, including interruption. | Sanitized live fixtures |
| Field-name convention | snake_case, unlike Copilot's camelCase. Payloads carry more fields than documented; tolerant deserialization (ignore unknown fields) is required — 2.1.205 already sends fields absent from older documentation (`prompt_id`, `background_tasks`, `session_crons`). | Sanitized live fixtures |
| `reason` values | Documented values are `clear`, `logout`, `prompt_input_exit`, and `other`. Observed: `other` for clean noninteractive (`claude -p`) completion, `other` for resumed noninteractive completion, and `other` for SIGINT interruption. The reason string alone therefore cannot distinguish a clean end from an interruption. | Sanitized live fixtures |
| Completion inference | In clean flows `Stop` preceded `SessionEnd` by ~40 ms. Under SIGINT mid-turn, no `Stop` fired and only `SessionEnd` (`reason: "other"`) arrived. A recorded `Stop` for the session is therefore the reliable completion signal; `reason` refines it (`clear`/`logout`/`prompt_input_exit` are affirmative user-driven ends). | Sanitized fixture timestamps |
| Resume identity | `claude -p --resume <id>` reused the same `session_id` and appended to the same transcript file. | Live observation |
| Force close | SIGKILL during an active turn emitted neither hook. Hook-only capture is insufficient; recovery needs a directory sweep. | Live observation |
| Transcript layout | `~/.claude/projects/<munged-cwd>/<session-uuid>.jsonl`, one regular file per session. The same directory also contains non-session entries (a `<uuid>/` subdirectory, a `memory/` directory). Discovery must accept only regular `*.jsonl` files whose stem is a valid session ID. | Live observation |
| Transcript envelope at 2.1.205 | `user`/`assistant` records match the 2.1.44 pin (`type`, `uuid`, `parentUuid`, `sessionId`, `timestamp`, `cwd`, `version`, `gitBranch`, `isSidechain`, `userType`, `message`), plus a new optional `entrypoint` key. New top-level record types appeared: `ai-title`, `attachment`, `last-prompt`, `mode`, `queue-operation`. All degrade to ignored metadata under the existing adapter. | Structural live inspection via `munshi-probe inspect-transcript` |
| End-to-end normalization | `munshi archive --source claude-code --events <live 2.1.205 transcript>` archived successfully with a stub summarizer: 19/19 records consumed, all bookkeeping types ignored, frontmatter `agent: "claude-code"`. | Live observation |
| Hook latency budget | The capture hook (spawn + read stdin + atomic write) completed within the window between hook fire and process exit in every run. Munshi's handler performs one short SQLite transaction; the Copilot-parity 2-second timeout is adequate. | Live observation |

## Lifecycle matrix

| Scenario | Hook events and order | Identity/path | Sanitized fixture | Status |
| --- | --- | --- | --- | --- |
| Noninteractive clean end (`claude -p`) | `Stop` then `SessionEnd` (~45 ms apart); reason `other` | Both events supplied the same `transcript_path` | `noninteractive/stop.json`, `noninteractive/session-end.json` | Observed |
| Resumed noninteractive end (`claude -p --resume`) | `Stop` then `SessionEnd`; reason `other` | Same `session_id` and transcript file as the original run | `resumed/stop.json`, `resumed/session-end.json` | Observed |
| SIGINT before completed turn | No `Stop`; `SessionEnd` with reason `other` | `SessionEnd` still supplied `transcript_path` | `interrupted/session-end.json` | Observed |
| SIGKILL during active turn | Neither hook fired | No hook-provided identity/path | None | Observed |
| Interactive `/clear`, `/logout`, prompt-input exit | Expected reasons `clear`, `logout`, `prompt_input_exit` per official documentation | Not automatable headlessly | None | Documented, not observed |

## Transcript structural matrix

`fixtures/claude-code-2.1.205/transcript/0c1a0de0-0000-4000-8000-000000000205.jsonl` is synthetic
content shaped from the aggregate structural observations below; it commits one example of each new
bookkeeping record type interleaved with pinned-shape `user`/`assistant` records.

| Scenario | Bytes | Valid lines | `type` counts | Status |
| --- | ---: | ---: | --- | --- |
| Probe session after resume | 21,025 | 19 | `assistant` 4; `user` 2; `attachment` 5; `queue-operation` 4; `last-prompt` 2; `ai-title` 1; `mode` 1 | Observed |

## Ingestion design implications

1. Map `Stop` to the agent-stop ingestion path and `SessionEnd` to session-end, keyed by
   `hook_event_name`, with tolerant snake_case deserialization (unknown fields ignored).
2. Persist the hook-provided `transcript_path` on both events; no session-ID-only transcript
   resolution is ever needed for Claude Code, preserving the existing adapter rule.
3. Stamp ingestion time locally; payloads carry no timestamp.
4. Completion mapping: treat `clear`/`logout`/`prompt_input_exit` as affirmative completion;
   treat `other` and unrecognized values as unknown, relying on a previously recorded agent stop
   to distinguish completed sessions from interrupted ones. Never drop a session for an
   unrecognized reason.
5. Recovery must sweep `~/.claude/projects/*/` for stale regular `*.jsonl` files because SIGKILL
   provides no notification; the sweep yields explicit transcript paths.
6. Never log or persist raw hook payloads (`last_assistant_message` contains conversation text).

## Remaining unknowns

- Exact behavior of interactive-only end reasons (`clear`, `logout`, `prompt_input_exit`) — taken
  from official documentation, not observed headlessly.
- Whether `Stop` hooks can observe queued-command flows (`background_tasks`, `session_crons`
  semantics) beyond the empty arrays seen here.
- Hook behavior of future Claude Code versions; this contract is pinned at 2.1.205.

Authoritative references:

- [Claude Code hooks reference](https://docs.anthropic.com/en/docs/claude-code/hooks)
- [Claude Code settings](https://docs.anthropic.com/en/docs/claude-code/settings)
