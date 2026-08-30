import { describe, expect, it } from "vitest";
import { bytes, isPending, lifecycleOf, relativeTime, retryLabel, shortId } from "../format";

describe("relativeTime", () => {
  const now = 1_700_000_000_000;

  it("formats recent, minute, hour and day scales", () => {
    expect(relativeTime(now - 1000, now)).toBe("just now");
    expect(relativeTime(now - 5 * 60_000, now)).toBe("5m");
    expect(relativeTime(now - 3 * 3_600_000, now)).toBe("3h");
    expect(relativeTime(now - 2 * 86_400_000, now)).toBe("2d");
  });

  it("does not print a negative age for a clock slightly ahead", () => {
    expect(relativeTime(now + 10_000, now)).toBe("just now");
  });

  it("renders a dash for a missing timestamp", () => {
    expect(relativeTime(undefined, now)).toBe("—");
    expect(relativeTime(Number.NaN, now)).toBe("—");
  });
});

describe("lifecycleOf", () => {
  it("prefers lifecycle_state, falls back to state, then unknown", () => {
    expect(lifecycleOf({ source: "c", session_id: "a", state: "x", lifecycle_state: "y" })).toBe(
      "y",
    );
    expect(lifecycleOf({ source: "c", session_id: "a", state: "x" })).toBe("x");
    expect(lifecycleOf({ source: "c", session_id: "a" } as never)).toBe("unknown");
  });
});

describe("retryLabel", () => {
  it("names the action honestly for each state", () => {
    expect(retryLabel("failed")).toBe("Retry");
    expect(retryLabel("archived")).toBe("Re-summarize");
    expect(retryLabel("revision-pending")).toBe("Update summary");
    expect(retryLabel("summary-pending")).toBe("Summarize now");
  });
});

describe("isPending", () => {
  it("covers every state that still owes work", () => {
    for (const state of ["summary-pending", "revision-pending", "interrupted", "observed"]) {
      expect(isPending(state)).toBe(true);
    }
    expect(isPending("archived")).toBe(false);
  });
});

describe("shortId", () => {
  it("leaves short ids alone and truncates long ones", () => {
    expect(shortId("abc")).toBe("abc");
    expect(shortId("e0820fcc-1111-2222-3333-444444444444")).toBe("e0820fcc…");
  });
});

describe("bytes", () => {
  it("scales to a readable unit", () => {
    expect(bytes(0)).toBe("0 B");
    expect(bytes(512)).toBe("512 B");
    expect(bytes(2048)).toBe("2.0 KiB");
    expect(bytes(5 * 1024 * 1024)).toBe("5.0 MiB");
  });
});
