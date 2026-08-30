import type { Bar } from "../lib/derive";

/**
 * A horizontal magnitude chart.
 *
 * Bars are scaled against the largest value rather than the total, so a single dominant project
 * does not flatten every other row into invisibility.
 */
export function Bars({ bars, empty }: { bars: Bar[]; empty: string }) {
  if (bars.length === 0) return <div className="empty">{empty}</div>;
  const max = Math.max(...bars.map((bar) => bar.value), 1);
  return (
    <div className="bars">
      {bars.map((bar) => (
        <div className="bar-row" key={bar.label}>
          <div>
            <div className="bar-label" title={bar.label}>
              {bar.label}
            </div>
            <div className="bar-track">
              <div className="bar-fill" style={{ width: `${(bar.value / max) * 100}%` }} />
            </div>
          </div>
          <div className="bar-value">{bar.value}</div>
        </div>
      ))}
    </div>
  );
}
