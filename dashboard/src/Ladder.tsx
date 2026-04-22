import type { Level } from './types';

// Top-N order book ladder. Reads Level[] already ordered by engine
// (bids desc, asks asc). Heat bars are scaled to the max qty in view so
// walls pop out visually.
export function Ladder({ bids, asks, levels = 10 }: { bids: Level[]; asks: Level[]; levels?: number }) {
  const b = bids.slice(0, levels);
  const a = asks.slice(0, levels);
  const maxQty = Math.max(
    1,
    ...b.map((l) => l.qty),
    ...a.map((l) => l.qty),
  );
  // Top rows = asks (ascending price reversed so best ask sits right above mid).
  const asksRev = [...a].reverse();

  const fmtPx = (p: number) => (p / 10_000).toFixed(4);
  const fmtQty = (q: number) => q.toLocaleString();

  return (
    <div className="ladder">
      <div className="ladder-head">
        <span>BID SIZE</span>
        <span>PRICE</span>
        <span>ASK SIZE</span>
      </div>
      {asksRev.map((l, i) => (
        <div className="ladder-row" key={`a${i}`}>
          <span className="ladder-bid" />
          <span className="ladder-px ask">{fmtPx(l.price_ticks)}</span>
          <span className="ladder-ask">
            <span
              className="ladder-bar ask-bar"
              style={{ width: `${(l.qty / maxQty) * 100}%` }}
            />
            <span className="ladder-num">{fmtQty(l.qty)}</span>
          </span>
        </div>
      ))}
      <div className="ladder-sep" />
      {b.map((l, i) => (
        <div className="ladder-row" key={`b${i}`}>
          <span className="ladder-bid">
            <span
              className="ladder-bar bid-bar"
              style={{ width: `${(l.qty / maxQty) * 100}%` }}
            />
            <span className="ladder-num">{fmtQty(l.qty)}</span>
          </span>
          <span className="ladder-px bid">{fmtPx(l.price_ticks)}</span>
          <span className="ladder-ask" />
        </div>
      ))}
    </div>
  );
}
