import { useMemo, useState } from "react";
import type { SessionListItem } from "../lib/contracts";
import {
  isPending,
  isUnhappy,
  lifecycleOf,
  relativeTime,
  shortId,
  sourceLabel,
} from "../lib/format";

/** The filter buckets across the top of the table. */
const SCOPES = [
  { key: "all", label: "All" },
  { key: "queued", label: "Queued" },
  { key: "failed", label: "Failed" },
  { key: "archived", label: "Archived" },
] as const;

type Scope = (typeof SCOPES)[number]["key"];

function badgeClass(state: string): string {
  if (state === "archived") return "badge archived";
  if (state === "processing") return "badge processing";
  if (isUnhappy(state)) return "badge failed";
  if (isPending(state)) return "badge pending";
  return "badge";
}

function matchesScope(state: string, scope: Scope): boolean {
  if (scope === "all") return true;
  if (scope === "queued") return isPending(state);
  if (scope === "failed") return isUnhappy(state);
  return state === "archived";
}

/**
 * The session list: the app's main surface, and where every per-session action starts.
 *
 * Filtering and search happen here over the already-collected listing rather than by re-running
 * `munshi sessions --state …`. One collection round then serves every view, so switching filters
 * is instant and does not multiply the number of processes spawned.
 */
export function SessionsTable({
  items,
  selected,
  onSelect,
}: {
  items: SessionListItem[];
  selected: SessionListItem | null;
  onSelect: (item: SessionListItem) => void;
}) {
  const [scope, setScope] = useState<Scope>("all");
  const [query, setQuery] = useState("");

  const rows = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return items
      .filter((item) => matchesScope(lifecycleOf(item), scope))
      .filter((item) => {
        if (!needle) return true;
        return [item.summary_title, item.project, item.session_id, item.source]
          .filter((field): field is string => typeof field === "string")
          .some((field) => field.toLowerCase().includes(needle));
      })
      .sort((a, b) => (b.updated_at_ms ?? 0) - (a.updated_at_ms ?? 0));
  }, [items, scope, query]);

  const counts = useMemo(() => {
    const result: Record<Scope, number> = { all: 0, queued: 0, failed: 0, archived: 0 };
    for (const item of items) {
      const state = lifecycleOf(item);
      result.all += 1;
      if (isPending(state)) result.queued += 1;
      if (isUnhappy(state)) result.failed += 1;
      if (state === "archived") result.archived += 1;
    }
    return result;
  }, [items]);

  return (
    <div className="card">
      <h2>
        Sessions <span className="count">{rows.length} shown</span>
      </h2>

      <div className="filters">
        <div className="seg">
          {SCOPES.map((entry) => (
            <button
              type="button"
              key={entry.key}
              aria-pressed={scope === entry.key}
              onClick={() => setScope(entry.key)}
            >
              {entry.label} ({counts[entry.key]})
            </button>
          ))}
        </div>
        <input
          type="search"
          placeholder="Filter by title, project, or session id"
          value={query}
          aria-label="Filter sessions"
          onChange={(event) => setQuery(event.target.value)}
        />
      </div>

      {rows.length === 0 ? (
        <div className="empty">
          {items.length === 0
            ? "No sessions captured yet. Finish a Copilot CLI or Claude Code session and it will appear here."
            : "No sessions match this filter."}
        </div>
      ) : (
        <div className="table-scroll">
          <table>
            <thead>
              <tr>
                <th>Title</th>
                <th>Project</th>
                <th>Source</th>
                <th>State</th>
                <th>Updated</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((item) => {
                const state = lifecycleOf(item);
                const key = `${item.source}:${item.session_id}`;
                const isSelected =
                  selected?.session_id === item.session_id && selected?.source === item.source;
                return (
                  <tr
                    key={key}
                    aria-selected={isSelected}
                    tabIndex={0}
                    onClick={() => onSelect(item)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        onSelect(item);
                      }
                    }}
                  >
                    <td className="title" title={item.summary_title ?? undefined}>
                      {item.summary_title || (
                        <span className="mono">{shortId(item.session_id)}</span>
                      )}
                    </td>
                    <td className="title" title={item.project ?? undefined}>
                      {item.project || <span className="mono">—</span>}
                    </td>
                    <td className="num">{sourceLabel(item.source)}</td>
                    <td>
                      <span className={badgeClass(state)}>{state}</span>
                    </td>
                    <td className="num">{relativeTime(item.updated_at_ms)}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
