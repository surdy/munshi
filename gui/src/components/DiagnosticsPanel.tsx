import type { DiagnosticsReport } from "../lib/contracts";
import { relativeTime } from "../lib/format";

/** The diagnostics tail: Munshi's own content-free record of what went wrong. */
export function DiagnosticsPanel({ report }: { report: DiagnosticsReport | null }) {
  const items = Array.isArray(report?.items) ? report.items : [];
  return (
    <div className="card">
      <h2>
        Diagnostics <span className="count">{items.length}</span>
      </h2>
      {items.length === 0 ? (
        <div className="empty">Nothing recorded.</div>
      ) : (
        <div className="table-scroll">
          <table>
            <thead>
              <tr>
                <th>Operation</th>
                <th>Category</th>
                <th>Cause</th>
                <th>When</th>
              </tr>
            </thead>
            <tbody>
              {items.map((item, index) => (
                // Diagnostics carry no id of their own; the tail is append-only and re-rendered
                // wholesale each poll, so position is a stable enough key here.
                // biome-ignore lint/suspicious/noArrayIndexKey: no stable id in the contract
                <tr key={`${item.recorded_at_ms}-${index}`} style={{ cursor: "default" }}>
                  <td className="mono">{item.operation ?? "—"}</td>
                  <td>{item.category ?? "—"}</td>
                  <td className="mono">{item.cause_category ?? "—"}</td>
                  <td className="num">{relativeTime(item.recorded_at_ms)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
