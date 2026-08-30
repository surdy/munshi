import { OUTCOMES, type OutcomeBin } from "../lib/derive";

const LEGEND: Record<(typeof OUTCOMES)[number], string> = {
  succeeded: "Succeeded",
  failed: "Failed",
  recovered: "Recovered",
  superseded: "Superseded",
};

/**
 * Attempt outcomes over the last six hours, as stacked columns.
 *
 * Built from divs rather than SVG: the shapes are plain stacked rectangles, and a flex column per
 * bin reflows with the window for free.
 */
export function OutcomeChart({ bins }: { bins: OutcomeBin[] }) {
  const totals = bins.map((bin) => OUTCOMES.reduce((sum, key) => sum + bin.counts[key], 0));
  const max = Math.max(...totals, 1);
  if (totals.every((total) => total === 0)) {
    return <div className="empty">No processing attempts in the last six hours.</div>;
  }

  return (
    <>
      <div className="chart">
        {bins.map((bin, index) => (
          <div
            className="chart-col"
            key={bin.startMs}
            title={`${new Date(bin.startMs).toLocaleTimeString()} — ${totals[index]} attempt(s)`}
          >
            {OUTCOMES.map((outcome) => {
              const count = bin.counts[outcome];
              if (count === 0) return null;
              return (
                <div
                  key={outcome}
                  className={`chart-seg ${outcome}`}
                  style={{ height: `${(count / max) * 100}%` }}
                />
              );
            })}
          </div>
        ))}
      </div>
      <div className="chart-axis">
        <span>6h ago</span>
        <span>now</span>
      </div>
      <div className="legend">
        {OUTCOMES.map((outcome) => (
          <span key={outcome}>
            <i className={`swatch chart-seg ${outcome}`} />
            {LEGEND[outcome]}
          </span>
        ))}
      </div>
    </>
  );
}
