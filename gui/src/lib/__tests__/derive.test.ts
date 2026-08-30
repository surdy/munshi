import { describe, expect, it } from "vitest";
import type { AttemptsReport, SessionsReport, SinkStatusReport, Snapshot } from "../contracts";
import {
  archivedBySource,
  BIN_MS,
  failingChecks,
  inFlight,
  isRegistered,
  outcomeBins,
  remainingByProject,
  tiles,
  unexpectedSchemaVersions,
} from "../derive";

function sessions(items: unknown[]): SessionsReport {
  return {
    schema_version: 1,
    command: "sessions",
    filter: null,
    total: items.length,
    returned: items.length,
    items: items as SessionsReport["items"],
  };
}

function emptySnapshot(overrides: Partial<Snapshot> = {}): Snapshot {
  return {
    generated_at_ms: 0,
    cli: {
      path: "/usr/bin/munshi",
      origin: "path",
      version: "0.1.0",
      bundled_path: null,
      bundled_version: null,
      install_target: null,
      installed: false,
      update_available: false,
      install_dir_on_path: false,
    },
    errors: [],
    status: null,
    sessions: null,
    attempts: null,
    diagnostics: null,
    uploads: null,
    deliveries: null,
    ...overrides,
  };
}

describe("tiles", () => {
  it("counts archived, queued and failed from one listing", () => {
    const snapshot = emptySnapshot({
      sessions: sessions([
        { source: "copilot", session_id: "a", state: "archived", lifecycle_state: "archived" },
        { source: "copilot", session_id: "b", state: "summary-pending" },
        { source: "copilot", session_id: "c", state: "interrupted" },
        { source: "copilot", session_id: "d", state: "failed" },
        { source: "copilot", session_id: "e", state: "transcript-lost" },
      ]),
    });
    const result = tiles(snapshot);
    expect(result.find((tile) => tile.key === "archived")?.value).toBe("1");
    expect(result.find((tile) => tile.key === "queued")?.value).toBe("2");
    // transcript-lost is unhappy-terminal too, so it counts with failed.
    expect(result.find((tile) => tile.key === "failed")?.value).toBe("2");
  });

  it("prefers lifecycle_state over the older state field", () => {
    const snapshot = emptySnapshot({
      sessions: sessions([
        { source: "copilot", session_id: "a", state: "observed", lifecycle_state: "archived" },
      ]),
    });
    expect(tiles(snapshot).find((tile) => tile.key === "archived")?.value).toBe("1");
  });

  it("shows no sink tile when the sink is disabled", () => {
    const disabled: SinkStatusReport = {
      schema_version: 1,
      command: "archive-upload-status",
      settings: { enabled: false },
    };
    const snapshot = emptySnapshot({ sessions: sessions([]), uploads: disabled });
    expect(tiles(snapshot).some((tile) => tile.key === "uploads")).toBe(false);
  });

  it("reads enabled from settings, not the top level", () => {
    // Guards the real contract shape: `enabled` is nested, while the counts are top level.
    const enabled: SinkStatusReport = {
      schema_version: 1,
      command: "archive-upload-status",
      settings: { enabled: true, endpoint: "https://patwari.example" },
      total: 43,
      uploaded: 40,
      pending: 2,
      failed: 0,
      dead_letter: 1,
    };
    const snapshot = emptySnapshot({ sessions: sessions([]), uploads: enabled });
    const tile = tiles(snapshot).find((entry) => entry.key === "uploads");
    expect(tile?.value).toBe("40");
    // dead_letter has exhausted its attempts, so it counts as failed.
    expect(tile?.detail).toBe("2 pending · 1 failed");
    expect(tile?.tone).toBe("critical");
  });

  it("calls an enabled sink with nothing outstanding up to date", () => {
    const enabled: SinkStatusReport = {
      schema_version: 1,
      command: "summary-delivery-status",
      settings: { enabled: true },
      delivered: 735,
      pending: 0,
      failed: 0,
      dead_letter: 0,
    };
    const snapshot = emptySnapshot({ sessions: sessions([]), deliveries: enabled });
    const tile = tiles(snapshot).find((entry) => entry.key === "deliveries");
    expect(tile?.detail).toBe("up to date");
    expect(tile?.tone).toBe("good");
  });

  it("survives a null sessions section", () => {
    expect(() => tiles(emptySnapshot())).not.toThrow();
    expect(tiles(emptySnapshot())[0].value).toBe("0");
  });
});

describe("remainingByProject", () => {
  it("counts only pending sessions and labels missing projects", () => {
    const report = sessions([
      { source: "copilot", session_id: "a", state: "summary-pending", project: "alpha" },
      { source: "copilot", session_id: "b", state: "interrupted", project: "alpha" },
      { source: "copilot", session_id: "c", state: "summary-pending" },
      { source: "copilot", session_id: "d", state: "archived", project: "alpha" },
    ]);
    expect(remainingByProject(report)).toEqual([
      { label: "alpha", value: 2 },
      { label: "unattributed", value: 1 },
    ]);
  });

  it("breaks ties on name so the order does not shuffle between polls", () => {
    const report = sessions([
      { source: "copilot", session_id: "a", state: "summary-pending", project: "zeta" },
      { source: "copilot", session_id: "b", state: "summary-pending", project: "alpha" },
    ]);
    expect(remainingByProject(report).map((bar) => bar.label)).toEqual(["alpha", "zeta"]);
  });
});

describe("archivedBySource", () => {
  it("counts archived sessions per harness", () => {
    const report = sessions([
      { source: "claude-code", session_id: "a", state: "archived" },
      { source: "claude-code", session_id: "b", state: "archived" },
      { source: "copilot", session_id: "c", state: "archived" },
      { source: "copilot", session_id: "d", state: "failed" },
    ]);
    expect(archivedBySource(report)).toEqual([
      { label: "claude-code", value: 2 },
      { label: "copilot", value: 1 },
    ]);
  });
});

describe("inFlight", () => {
  it("returns processing and summary-pending sessions, newest first", () => {
    const report = sessions([
      { source: "copilot", session_id: "old", state: "processing", updated_at_ms: 1 },
      { source: "copilot", session_id: "new", state: "summary-pending", updated_at_ms: 9 },
      { source: "copilot", session_id: "done", state: "archived", updated_at_ms: 5 },
    ]);
    expect(inFlight(report).map((item) => item.session_id)).toEqual(["new", "old"]);
  });
});

describe("outcomeBins", () => {
  const now = 1_700_000_000_000;

  it("bins attempts by finish time within the six-hour window", () => {
    const attempts: AttemptsReport = {
      schema_version: 1,
      command: "attempts",
      returned: 3,
      items: [
        { outcome: "succeeded", finished_at_ms: now - 1000 },
        { outcome: "succeeded", finished_at_ms: now - 2000 },
        { outcome: "failed", finished_at_ms: now - BIN_MS * 3 },
      ],
    };
    const bins = outcomeBins(attempts, now);
    const totals = bins.reduce((sum, bin) => sum + bin.counts.succeeded + bin.counts.failed, 0);
    expect(totals).toBe(3);
    expect(bins.at(-1)?.counts.succeeded).toBe(2);
  });

  it("ignores attempts outside the window and unknown outcomes", () => {
    const attempts: AttemptsReport = {
      schema_version: 1,
      command: "attempts",
      returned: 3,
      items: [
        { outcome: "succeeded", finished_at_ms: now - 24 * 60 * 60 * 1000 },
        { outcome: "some-future-outcome", finished_at_ms: now - 1000 },
        { outcome: "processing", finished_at_ms: now - 1000 },
      ],
    };
    const bins = outcomeBins(attempts, now);
    const total = bins.reduce(
      (sum, bin) =>
        sum +
        bin.counts.succeeded +
        bin.counts.failed +
        bin.counts.recovered +
        bin.counts.superseded,
      0,
    );
    expect(total).toBe(0);
  });

  it("falls back to the start time for an attempt that never finished", () => {
    const attempts: AttemptsReport = {
      schema_version: 1,
      command: "attempts",
      returned: 1,
      items: [{ outcome: "recovered", started_at_ms: now - 1000, finished_at_ms: null }],
    };
    expect(outcomeBins(attempts, now).at(-1)?.counts.recovered).toBe(1);
  });

  it("returns empty bins rather than throwing for a null section", () => {
    const bins = outcomeBins(null, now);
    expect(bins).toHaveLength(12);
    expect(bins.every((bin) => bin.counts.succeeded === 0)).toBe(true);
  });
});

function status(configuration: Record<string, unknown>) {
  return {
    schema_version: 1,
    command: "status",
    state_directory: "/home/x/.munshi",
    configuration,
    sessions: {},
  } as const;
}

describe("isRegistered", () => {
  it("is false when the config file is missing", () => {
    expect(isRegistered(null)).toBe(false);
    expect(
      isRegistered(
        status({
          status: "error",
          runtime_compatible: false,
          checks: [{ code: "config-file", status: "error", message: "missing config.json" }],
        }),
      ),
    ).toBe(false);
  });

  it("is true when the config file loaded", () => {
    expect(
      isRegistered(
        status({
          status: "ok",
          runtime_compatible: true,
          checks: [{ code: "config-file", status: "ok", message: "loaded config.json" }],
        }),
      ),
    ).toBe(true);
  });

  it("stays true for a registered machine whose config the workers cannot fully drive", () => {
    // The regression this guards: rolling up `configuration.status` or trusting
    // `runtime_compatible` would tell an already-registered user to run `register` again.
    expect(
      isRegistered(
        status({
          status: "warning",
          runtime_compatible: false,
          checks: [
            { code: "config-file", status: "ok", message: "loaded config.json" },
            { code: "runtime-compatible", status: "warning", message: "not fully compatible" },
          ],
        }),
      ),
    ).toBe(true);
  });

  it("falls back to runtime_compatible when an older CLI sends no checks", () => {
    expect(isRegistered(status({ runtime_compatible: true }))).toBe(true);
    expect(isRegistered(status({ runtime_compatible: false }))).toBe(false);
  });
});

describe("failingChecks", () => {
  it("returns only non-ok checks, errors before warnings", () => {
    const result = failingChecks(
      status({
        checks: [
          { code: "config-file", status: "ok" },
          { code: "runtime-compatible", status: "warning" },
          { code: "hook-file", status: "error" },
        ],
      }),
    );
    expect(result.map((check) => check.code)).toEqual(["hook-file", "runtime-compatible"]);
  });

  it("is empty when everything is ok or checks are absent", () => {
    expect(failingChecks(status({ checks: [{ code: "config-file", status: "ok" }] }))).toEqual([]);
    expect(failingChecks(status({}))).toEqual([]);
    expect(failingChecks(null)).toEqual([]);
  });
});

describe("unexpectedSchemaVersions", () => {
  it("reports a version this UI was not written against", () => {
    const snapshot = emptySnapshot({
      sessions: { ...sessions([]), schema_version: 2 },
    });
    expect(unexpectedSchemaVersions(snapshot, 1)).toEqual([2]);
  });

  it("is silent on the expected version", () => {
    expect(unexpectedSchemaVersions(emptySnapshot({ sessions: sessions([]) }), 1)).toEqual([]);
  });
});
