import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Bars } from "./components/Bars";
import { ConfigPanel } from "./components/ConfigPanel";
import { DiagnosticsPanel } from "./components/DiagnosticsPanel";
import { ErrorBanner } from "./components/ErrorBanner";
import { OutcomeChart } from "./components/OutcomeChart";
import { SessionDetail } from "./components/SessionDetail";
import { SessionsTable } from "./components/SessionsTable";
import { SetupBanner } from "./components/SetupBanner";
import { Tiles } from "./components/Tiles";
import { snapshot as fetchSnapshot, retryAll, runTick } from "./lib/api";
import { EXPECTED_SCHEMA_VERSION, type SessionListItem, type Snapshot } from "./lib/contracts";
import {
  archivedBySource,
  inFlight,
  outcomeBins,
  remainingByProject,
  tiles,
  unexpectedSchemaVersions,
} from "./lib/derive";
import { relativeTime, sourceLabel } from "./lib/format";

/**
 * How often a fresh snapshot is collected.
 *
 * Each round is six `munshi` invocations, so this is a real cost on the machine rather than a
 * cheap HTTP poll. 20s keeps an in-flight session visibly moving without spawning processes
 * continuously; the window also refreshes immediately after any action and whenever it regains
 * focus, which is what actually makes it feel live.
 */
const POLL_MS = 20_000;

export default function App() {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [selected, setSelected] = useState<SessionListItem | null>(null);
  const [toast, setToast] = useState<{ text: string; error: boolean } | null>(null);
  const [busy, setBusy] = useState(false);

  // Guards against a slow round landing after a newer one and rewinding the view.
  const generation = useRef(0);

  const refresh = useCallback(async () => {
    const round = ++generation.current;
    try {
      const next = await fetchSnapshot();
      if (round === generation.current) setSnapshot(next);
    } catch (error) {
      if (round === generation.current) {
        setToast({ text: String(error), error: true });
      }
    } finally {
      if (round === generation.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), POLL_MS);
    // Coming back to the window should show current state, not whatever the last tick left.
    const onFocus = () => void refresh();
    window.addEventListener("focus", onFocus);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener("focus", onFocus);
    };
  }, [refresh]);

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(() => setToast(null), toast.error ? 9000 : 4000);
    return () => window.clearTimeout(timer);
  }, [toast]);

  const notify = useCallback((text: string) => setToast({ text, error: false }), []);
  const fail = useCallback((text: string) => setToast({ text, error: true }), []);

  const acted = useCallback(
    (message: string) => {
      notify(message);
      void refresh();
    },
    [notify, refresh],
  );

  async function runAction(label: string, action: () => Promise<unknown>) {
    setBusy(true);
    try {
      await action();
      acted(`${label} finished.`);
    } catch (error) {
      fail(String(error));
    } finally {
      setBusy(false);
    }
  }

  const sessions = useMemo(
    () => (Array.isArray(snapshot?.sessions?.items) ? snapshot.sessions.items : []),
    [snapshot],
  );
  const working = useMemo(() => inFlight(snapshot?.sessions ?? null), [snapshot]);
  const projects = useMemo(() => remainingByProject(snapshot?.sessions ?? null), [snapshot]);
  const sources = useMemo(() => archivedBySource(snapshot?.sessions ?? null), [snapshot]);
  const bins = useMemo(() => outcomeBins(snapshot?.attempts ?? null), [snapshot]);
  const schemaWarnings = useMemo(
    () => (snapshot ? unexpectedSchemaVersions(snapshot, EXPECTED_SCHEMA_VERSION) : []),
    [snapshot],
  );

  // Keep the open drawer showing the freshest row for its session across polls.
  const selectedLive = useMemo(() => {
    if (!selected) return null;
    return (
      sessions.find(
        (item) => item.session_id === selected.session_id && item.source === selected.source,
      ) ?? selected
    );
  }, [selected, sessions]);

  if (loading && !snapshot) {
    return (
      <div className="app">
        <div className="titlebar" data-tauri-drag-region>
          Munshi
        </div>
        <div className="scroll">
          <div className="wrap">
            <div className="empty">Reading Munshi's state…</div>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="app">
      <div className="titlebar" data-tauri-drag-region>
        Munshi
      </div>

      <div className="scroll">
        <div className="wrap">
          <header className="page">
            <h1>Archiving backlog</h1>
            {snapshot?.cli.path ? (
              <span className="pill" title={snapshot.cli.path}>
                <i className={`dot ${snapshot.errors.length ? "warning" : "good"}`} />
                munshi {snapshot.cli.version ?? "?"}
              </span>
            ) : null}
            <div className="spacer" />
            <span className="hdr-meta">updated {relativeTime(snapshot?.generated_at_ms)}</span>
            <button type="button" disabled={busy} onClick={() => void refresh()}>
              Refresh
            </button>
            {/* The same idempotent sweep the launchd job runs — drains retries on demand. */}
            <button type="button" disabled={busy} onClick={() => runAction("Sweep", runTick)}>
              Run sweep
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => runAction("Retry all", () => retryAll(false))}
            >
              Retry all
            </button>
          </header>

          {snapshot ? (
            <SetupBanner
              cli={snapshot.cli}
              status={snapshot.status}
              onInstalled={(path) => acted(`Installed ${path}.`)}
              onError={fail}
            />
          ) : null}

          {snapshot ? (
            <ErrorBanner errors={snapshot.errors} schemaWarnings={schemaWarnings} />
          ) : null}

          {snapshot ? <Tiles tiles={tiles(snapshot)} /> : null}

          <div className="grid">
            <div className="card">
              <h2>
                Right now <span className="count">{working.length}</span>
              </h2>
              {working.length === 0 ? (
                <div className="empty">Nothing being summarized.</div>
              ) : (
                <div className="bars">
                  {working.map((item) => (
                    <div className="bar-row" key={`${item.source}:${item.session_id}`}>
                      <div className="bar-label" title={item.summary_title ?? item.session_id}>
                        {item.summary_title || item.session_id}
                        <span className="mono"> · {sourceLabel(item.source)}</span>
                      </div>
                      <div className="bar-value">{relativeTime(item.updated_at_ms)}</div>
                    </div>
                  ))}
                </div>
              )}
            </div>

            <div className="card">
              <h2>Remaining by project</h2>
              <Bars bars={projects} empty="Nothing waiting." />
            </div>

            <div className="card">
              <h2>Archived by source</h2>
              <Bars bars={sources} empty="Nothing archived yet." />
            </div>

            <div className="card">
              <h2>Attempt outcomes · last 6h</h2>
              <OutcomeChart bins={bins} />
            </div>

            <ConfigPanel
              status={snapshot?.status ?? null}
              uploads={snapshot?.uploads ?? null}
              deliveries={snapshot?.deliveries ?? null}
            />
          </div>

          <SessionsTable items={sessions} selected={selectedLive} onSelect={setSelected} />

          <div style={{ height: 12 }} />
          <DiagnosticsPanel report={snapshot?.diagnostics ?? null} />
        </div>
      </div>

      {selectedLive ? (
        <SessionDetail
          item={selectedLive}
          onClose={() => setSelected(null)}
          onActed={acted}
          onError={fail}
        />
      ) : null}

      {toast ? (
        <div className={`toast ${toast.error ? "error" : ""}`} role="status">
          {toast.text}
        </div>
      ) : null}
    </div>
  );
}
