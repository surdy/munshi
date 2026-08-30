/**
 * Turning raw contracts into what the panels draw.
 *
 * Pure functions, JSON in and view models out, so the arithmetic is tested without a `munshi`
 * binary present. Navigation is deliberately lenient, matching the dashboard's aggregator: a row
 * missing a field is skipped or defaulted rather than failing the section, because these contracts
 * evolve independently of this app and a drifted field must degrade one panel, not the window.
 */

import type {
  AttemptListItem,
  AttemptsReport,
  CheckResult,
  SessionListItem,
  SessionsReport,
  SinkStatusReport,
  Snapshot,
  StatusReport,
} from "./contracts";
import { isInFlight, isPending, isUnhappy, lifecycleOf } from "./format";

/** Width of one bin in the attempt-outcome chart. */
export const BIN_MS = 30 * 60 * 1000;

/** How far back that chart reaches. */
export const CHART_SPAN_MS = 6 * 60 * 60 * 1000;

/** The outcomes the chart stacks, in a fixed order so colours never shuffle between renders. */
export const OUTCOMES = ["succeeded", "failed", "recovered", "superseded"] as const;
export type ChartedOutcome = (typeof OUTCOMES)[number];

export interface Tile {
  key: string;
  label: string;
  value: string;
  /** Secondary line, e.g. "of 412 sessions". */
  detail?: string;
  /** Drives the accent colour; `null` is neutral. */
  tone?: "good" | "warning" | "critical" | null;
}

/**
 * The headline tiles.
 *
 * Counts come from `sessions` rather than `status` where both could serve, so every tile and the
 * table below it are derived from the same listing and cannot disagree by a poll interval.
 */
export function tiles(snapshot: Snapshot): Tile[] {
  const items = sessionItems(snapshot.sessions);
  const total = snapshot.sessions?.total ?? items.length;

  let archived = 0;
  let queued = 0;
  let failed = 0;
  for (const item of items) {
    const state = lifecycleOf(item);
    if (state === "archived") archived += 1;
    if (isPending(state)) queued += 1;
    if (isUnhappy(state)) failed += 1;
  }

  const result: Tile[] = [
    {
      key: "archived",
      label: "Archived",
      value: String(archived),
      detail: total ? `of ${total} sessions` : "nothing captured yet",
      tone: null,
    },
    {
      key: "queued",
      label: "Queued",
      value: String(queued),
      detail: queued ? "waiting to be summarized" : "nothing waiting",
      tone: queued > 0 ? "warning" : "good",
    },
    {
      key: "failed",
      label: "Failed",
      value: String(failed),
      detail: failed ? "need attention" : "none",
      tone: failed > 0 ? "critical" : "good",
    },
  ];

  const uploads = sinkTile("uploads", "Uploads", snapshot.uploads);
  if (uploads) result.push(uploads);
  const deliveries = sinkTile("deliveries", "Deliveries", snapshot.deliveries);
  if (deliveries) result.push(deliveries);

  return result;
}

/**
 * A tile for one remote sink, or nothing at all when the sink is disabled.
 *
 * Both sinks are opt-in and off by default, so an unconfigured machine should show no tile rather
 * than a zero that reads like a failure.
 */
function sinkTile(key: string, label: string, report: SinkStatusReport | null): Tile | null {
  // `enabled` is nested under `settings`; a disabled sink reports it there rather than failing.
  if (report?.settings?.enabled !== true) return null;
  const pending = numberAt(report, "pending");
  // Dead-lettered work has exhausted its attempts, so it needs a person: count it as failed.
  const failed = numberAt(report, "failed") + numberAt(report, "dead_letter");
  const done = numberAt(report, key === "uploads" ? "uploaded" : "delivered");
  return {
    key,
    label,
    value: String(done),
    detail: pending || failed ? `${pending} pending · ${failed} failed` : "up to date",
    tone: failed > 0 ? "critical" : pending > 0 ? "warning" : "good",
  };
}

/** Sessions currently being worked, newest activity first. */
export function inFlight(report: SessionsReport | null): SessionListItem[] {
  return sessionItems(report)
    .filter((item) => isInFlight(lifecycleOf(item)))
    .sort((a, b) => (b.updated_at_ms ?? 0) - (a.updated_at_ms ?? 0));
}

export interface Bar {
  label: string;
  value: number;
}

/**
 * Remaining backlog by project, largest first.
 *
 * Ties break on the project name so the ordering is stable across polls and rows do not jump
 * around while someone is reading them.
 */
export function remainingByProject(report: SessionsReport | null, limit = 10): Bar[] {
  const counts = new Map<string, number>();
  for (const item of sessionItems(report)) {
    if (!isPending(lifecycleOf(item))) continue;
    const label = item.project?.trim() || "unattributed";
    counts.set(label, (counts.get(label) ?? 0) + 1);
  }
  return [...counts.entries()]
    .map(([label, value]) => ({ label, value }))
    .sort((a, b) => b.value - a.value || a.label.localeCompare(b.label))
    .slice(0, limit);
}

/** Archived counts by harness. */
export function archivedBySource(report: SessionsReport | null): Bar[] {
  const counts = new Map<string, number>();
  for (const item of sessionItems(report)) {
    if (lifecycleOf(item) !== "archived") continue;
    const label = item.source || "unknown";
    counts.set(label, (counts.get(label) ?? 0) + 1);
  }
  return [...counts.entries()]
    .map(([label, value]) => ({ label, value }))
    .sort((a, b) => b.value - a.value || a.label.localeCompare(b.label));
}

export interface OutcomeBin {
  /** Start of the bin, epoch ms. */
  startMs: number;
  counts: Record<ChartedOutcome, number>;
}

/**
 * Attempt outcomes binned over the last six hours.
 *
 * Bins are anchored to `now` rather than to the newest attempt, so an idle machine shows an empty
 * recent stretch instead of silently rescaling the axis to old activity.
 */
export function outcomeBins(report: AttemptsReport | null, now: number = Date.now()): OutcomeBin[] {
  const binCount = Math.ceil(CHART_SPAN_MS / BIN_MS);
  const newest = Math.ceil(now / BIN_MS) * BIN_MS;
  const oldest = newest - binCount * BIN_MS;

  const bins: OutcomeBin[] = [];
  for (let index = 0; index < binCount; index += 1) {
    bins.push({
      startMs: oldest + index * BIN_MS,
      counts: { succeeded: 0, failed: 0, recovered: 0, superseded: 0 },
    });
  }

  for (const attempt of attemptItems(report)) {
    // An attempt still in flight has no finish time; place it by when it started.
    const at = attempt.finished_at_ms ?? attempt.started_at_ms;
    if (!at || at < oldest || at >= newest) continue;
    const outcome = attempt.outcome;
    if (!outcome || !isCharted(outcome)) continue;
    const index = Math.floor((at - oldest) / BIN_MS);
    const bin = bins[index];
    if (bin) bin.counts[outcome] += 1;
  }

  return bins;
}

function isCharted(outcome: string): outcome is ChartedOutcome {
  return (OUTCOMES as readonly string[]).includes(outcome);
}

/** Recently archived sessions, newest first. */
export function recentlyArchived(report: SessionsReport | null, limit = 12): SessionListItem[] {
  return sessionItems(report)
    .filter((item) => lifecycleOf(item) === "archived")
    .sort((a, b) => (b.updated_at_ms ?? 0) - (a.updated_at_ms ?? 0))
    .slice(0, limit);
}

/** Sessions in an unhappy terminal state, newest first. */
export function recentFailures(report: SessionsReport | null, limit = 12): SessionListItem[] {
  return sessionItems(report)
    .filter((item) => isUnhappy(lifecycleOf(item)))
    .sort((a, b) => (b.updated_at_ms ?? 0) - (a.updated_at_ms ?? 0))
    .slice(0, limit);
}

/**
 * Whether the machine looks registered.
 *
 * Every read-only contract is valid but empty before `munshi register` has ever run, so "no
 * sessions" alone is not evidence of a problem — the configuration assessment is.
 *
 * The signal is the `config-file` check rather than the rolled-up `configuration.status` or
 * `runtime_compatible`: both of those also go bad for a machine that *is* registered but whose
 * config the current workers cannot fully drive, and telling such a user to run `register` would
 * be wrong advice. A loaded config file is exactly the question "has this been set up" asks.
 */
export function isRegistered(status: StatusReport | null): boolean {
  const checks = status?.configuration?.checks;
  if (Array.isArray(checks)) {
    const configFile = checks.find((check) => check.code === "config-file");
    if (configFile) return configFile.status === "ok";
  }
  // Older CLIs may not carry `checks`; fall back to the compatibility flag.
  return status?.configuration?.runtime_compatible === true;
}

/** Checks that are not `ok`, worst first, for the configuration panel. */
export function failingChecks(status: StatusReport | null): CheckResult[] {
  const checks = status?.configuration?.checks;
  if (!Array.isArray(checks)) return [];
  const rank: Record<string, number> = { error: 0, warning: 1, unknown: 2, ok: 3 };
  return checks
    .filter((check) => check.status !== "ok")
    .sort((a, b) => (rank[a.status ?? "unknown"] ?? 2) - (rank[b.status ?? "unknown"] ?? 2));
}

/** A schema version this UI was not written against, if the CLI reported one. */
export function unexpectedSchemaVersions(snapshot: Snapshot, expected: number): number[] {
  const seen = new Set<number>();
  for (const report of [
    snapshot.status,
    snapshot.sessions,
    snapshot.attempts,
    snapshot.diagnostics,
    snapshot.uploads,
    snapshot.deliveries,
  ]) {
    const version = report?.schema_version;
    if (typeof version === "number" && version !== expected) seen.add(version);
  }
  return [...seen].sort((a, b) => a - b);
}

/** Defensive readers: a malformed or absent section becomes an empty list, never a throw. */
function sessionItems(report: SessionsReport | null): SessionListItem[] {
  return Array.isArray(report?.items) ? report.items : [];
}

function attemptItems(report: AttemptsReport | null): AttemptListItem[] {
  return Array.isArray(report?.items) ? report.items : [];
}

function numberAt(report: SinkStatusReport, key: string): number {
  const value = report[key];
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}
