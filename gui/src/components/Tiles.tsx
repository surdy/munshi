import type { Tile } from "../lib/derive";

/** The headline row: archived, queued, failed, and a tile per enabled remote sink. */
export function Tiles({ tiles }: { tiles: Tile[] }) {
  return (
    <div className="tiles">
      {tiles.map((tile) => (
        <div key={tile.key} className={`tile ${tile.tone ?? ""}`}>
          <div className="label">{tile.label}</div>
          <div className="value">{tile.value}</div>
          {tile.detail ? <div className="detail">{tile.detail}</div> : null}
        </div>
      ))}
    </div>
  );
}
