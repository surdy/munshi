/**
 * Presentation helpers. Pure, so the awkward cases (missing timestamps, clock skew, unknown
 * states) are tested directly rather than through the components.
 */

import type { SessionListItem } from "./contracts";

/** A compact relative age: "just now", "4m", "3h", "2d". */
export function relativeTime(ms: number | undefined, now: number = Date.now()): string {
  if (!ms || !Number.isFinite(ms)) return "—";
  const seconds = Math.round((now - ms) / 1000);
  // Clock skew, or a row written by a machine slightly ahead of this one: don't print "-3m".
  if (seconds < 45) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

/** An absolute local timestamp for tooltips, where the relative age is too coarse. */
export function absoluteTime(ms: number | undefined): string {
  if (!ms || !Number.isFinite(ms)) return "unknown";
  return new Date(ms).toLocaleString();
}

/**
 * The lifecycle state to display, preferring `lifecycle_state` and falling back to `state`.
 *
 * The same fallback the dashboard's aggregator uses: the two fields have coexisted since the
 * contract gained `lifecycle_state`, and an older CLI only fills the latter.
 */
export function lifecycleOf(item: SessionListItem): string {
  return item.lifecycle_state ?? item.state ?? "unknown";
}

/** Whether a state means Munshi still owes this session work. */
export function isPending(state: string): boolean {
  return (
    state === "summary-pending" ||
    state === "revision-pending" ||
    state === "interrupted" ||
    state === "observed"
  );
}

/** Whether a state means work is in flight right now. */
export function isInFlight(state: string): boolean {
  return state === "processing" || state === "summary-pending";
}

/** Whether a state is terminal-but-unhappy. */
export function isUnhappy(state: string): boolean {
  return state === "failed" || state === "transcript-lost";
}

/**
 * The verb for the retry action, which depends on what the session's state means.
 *
 * Munshi decides what retrying actually does; this only picks an honest label. ADR 0007 lists
 * "summarize now" and "retry" as distinct actions, and they are the same command underneath.
 */
export function retryLabel(state: string): string {
  if (state === "failed") return "Retry";
  if (state === "archived") return "Re-summarize";
  if (state === "revision-pending") return "Update summary";
  return "Summarize now";
}

/** A short, human name for a source. */
export function sourceLabel(source: string): string {
  if (source === "claude-code") return "Claude Code";
  if (source === "copilot") return "Copilot CLI";
  if (source === "codex") return "Codex CLI";
  return source;
}

/** Shortens a session id to something a table cell can hold, keeping it recognisable. */
export function shortId(sessionId: string): string {
  return sessionId.length <= 12 ? sessionId : `${sessionId.slice(0, 8)}…`;
}

/** Formats a byte count for the sink panels. */
export function bytes(value: number | undefined): string {
  if (!value || !Number.isFinite(value)) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let size = value;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) {
    size /= 1024;
    unit += 1;
  }
  return `${size < 10 && unit > 0 ? size.toFixed(1) : Math.round(size)} ${units[unit]}`;
}
