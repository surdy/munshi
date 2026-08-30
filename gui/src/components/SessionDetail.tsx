import { useEffect, useState } from "react";
import { openTarget, readArchive, retrySession, showSession } from "../lib/api";
import type { SessionListItem, ShowReport } from "../lib/contracts";
import { absoluteTime, lifecycleOf, retryLabel, sourceLabel } from "../lib/format";

/**
 * One session in full, with the actions ADR 0007 blesses: view the summary, open the Notesmith
 * deep link, and summarize/retry.
 *
 * The list row is shown immediately and `munshi show --json` fills in behind it, so opening a
 * session never waits on a subprocess before showing anything.
 */
export function SessionDetail({
  item,
  onClose,
  onActed,
  onError,
}: {
  item: SessionListItem;
  onClose: () => void;
  onActed: (message: string) => void;
  onError: (message: string) => void;
}) {
  const [detail, setDetail] = useState<ShowReport | null>(null);
  const [markdown, setMarkdown] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Depend on the primitives the fetch actually uses, so the effect re-runs when the selected
  // session changes but not when the parent re-renders a new object around the same session.
  const source = item.source;
  const sessionId = item.session_id;
  const archivePathHint = item.archive_path ?? null;

  // Close on Escape. The listener goes on the document rather than the drawer element: the
  // drawer is not focused when it opens, so a handler bound to it would never fire.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  useEffect(() => {
    let cancelled = false;
    setDetail(null);
    setMarkdown(null);

    showSession(source, sessionId)
      .then((report) => {
        if (cancelled) return;
        setDetail(report);
        const path = report.session?.archive_path ?? archivePathHint;
        if (!path) return;
        // A missing or unreadable archive file is not an error worth a banner: the summary panel
        // simply stays empty, and the rest of the drawer is still useful.
        readArchive(path)
          .then((text) => {
            if (!cancelled) setMarkdown(text);
          })
          .catch(() => undefined);
      })
      .catch((error) => {
        if (!cancelled) onError(String(error));
      });

    return () => {
      cancelled = true;
    };
  }, [source, sessionId, archivePathHint, onError]);

  const session = detail?.session ?? null;
  const state = session?.lifecycle_state ?? session?.state ?? lifecycleOf(item);
  const archivePath = session?.archive_path ?? item.archive_path ?? null;
  const noteLink = session?.delivery?.note_link ?? null;

  async function act(force: boolean) {
    setBusy(true);
    try {
      await retrySession(item.source, item.session_id, force);
      onActed(`${retryLabel(state)} queued for ${item.summary_title || item.session_id}.`);
    } catch (error) {
      onError(String(error));
    } finally {
      setBusy(false);
    }
  }

  async function open(target: string) {
    try {
      await openTarget(target);
    } catch (error) {
      onError(String(error));
    }
  }

  return (
    <div className="drawer-scrim">
      {/* A real button rather than a click handler on the backdrop: click-away is then reachable
          by keyboard and announced, instead of being a mouse-only affordance. */}
      <button
        type="button"
        className="scrim-close"
        aria-label="Close session detail"
        onClick={onClose}
      />
      <aside className="drawer" role="dialog" aria-modal="true" aria-label="Session detail">
        <div className="drawer-head">
          <h2>{item.summary_title || item.session_id}</h2>
          <div className="drawer-meta">
            <span className="pill">{state}</span>
            <span className="pill">{sourceLabel(item.source)}</span>
            {item.project ? <span className="pill">{item.project}</span> : null}
          </div>
        </div>

        <div className="drawer-body">
          <dl>
            <div className="field">
              <dt>Session id</dt>
              <dd className="mono">{item.session_id}</dd>
            </div>
            {session?.revision !== undefined || item.revision !== undefined ? (
              <div className="field">
                <dt>Revision</dt>
                <dd>{session?.revision ?? item.revision}</dd>
              </div>
            ) : null}
            {item.completion_reason ? (
              <div className="field">
                <dt>Completion</dt>
                <dd>{item.completion_reason}</dd>
              </div>
            ) : null}
            {item.last_error_code ? (
              <div className="field">
                <dt>Last error</dt>
                <dd className="mono">{item.last_error_code}</dd>
              </div>
            ) : null}
            {session?.delivery?.state ? (
              <div className="field">
                <dt>Delivery</dt>
                <dd>{session.delivery.state}</dd>
              </div>
            ) : null}
            {item.patwari_session_id ? (
              <div className="field">
                <dt>Archive id</dt>
                <dd className="mono">{item.patwari_session_id}</dd>
              </div>
            ) : null}
            <div className="field">
              <dt>Updated</dt>
              <dd>{absoluteTime(item.updated_at_ms)}</dd>
            </div>
            {archivePath ? (
              <div className="field">
                <dt>Archive file</dt>
                <dd className="mono">{archivePath}</dd>
              </div>
            ) : null}
          </dl>

          <div className="summary">
            {markdown ? (
              <pre>{markdown}</pre>
            ) : (
              <div className="empty">
                {detail === null
                  ? "Loading…"
                  : archivePath
                    ? "The archive file could not be read."
                    : "No summary has been written for this session yet."}
              </div>
            )}
          </div>
        </div>

        <div className="drawer-actions">
          <button type="button" className="primary" disabled={busy} onClick={() => act(false)}>
            {retryLabel(state)}
          </button>
          {/* Forcing bypasses the retry backoff, which is the only way to move a parked session. */}
          <button type="button" disabled={busy} onClick={() => act(true)}>
            Force
          </button>
          {archivePath ? (
            <button type="button" onClick={() => open(archivePath)}>
              Open summary
            </button>
          ) : null}
          {noteLink ? (
            <button type="button" onClick={() => open(noteLink)}>
              Open in Notesmith
            </button>
          ) : null}
          <div className="spacer" />
          <button type="button" onClick={onClose}>
            Close
          </button>
        </div>
      </aside>
    </div>
  );
}
