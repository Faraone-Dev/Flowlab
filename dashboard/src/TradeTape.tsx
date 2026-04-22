import type { TradePrint } from './types';
import { useEffect, useRef } from 'react';

const MAX_TAPE = 200;

// Newest at the BOTTOM (desk convention). Auto-scroll pinned to bottom.
// Accumulates prints across ticks — the upstream feed only ships the
// trades from the latest tick (0..6), so we keep a rolling window here.
export function TradeTape({ trades }: { trades: TradePrint[] }) {
  const ref = useRef<HTMLDivElement>(null);
  const tape = useRef<TradePrint[]>([]);
  const lastTs = useRef<number>(0);

  // Append only NEW trades (dedup by ts_ns + price + qty).
  for (const t of trades) {
    if (t.ts_ns > lastTs.current) {
      tape.current.push(t);
      lastTs.current = t.ts_ns;
    }
  }
  if (tape.current.length > MAX_TAPE) {
    tape.current = tape.current.slice(-MAX_TAPE);
  }

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [tape.current.length]);

  if (!tape.current.length) {
    return (
      <div className="tape-empty">waiting for trades…</div>
    );
  }

  return (
    <div className="tape-body" ref={ref}>
      {tape.current.map((t, i) => {
        const px = t.price_ticks / 10_000;
        const cls =
          t.aggressor > 0 ? 'tape-row tape-buy'
          : t.aggressor < 0 ? 'tape-row tape-sell'
          : 'tape-row tape-mid';
        // keep time local to avoid passing big tz logic; ts_ns is source clock
        const ts = tsToHms(t.ts_ns);
        return (
          <div className={cls} key={`${i}-${t.ts_ns}-${t.price_ticks}`}>
            <span className="tape-ts">{ts}</span>
            <span className="tape-px">{px.toFixed(4)}</span>
            <span className="tape-qty">{t.qty.toLocaleString()}</span>
            <span className="tape-side">{t.aggressor > 0 ? '▲' : t.aggressor < 0 ? '▼' : '·'}</span>
          </div>
        );
      })}
    </div>
  );
}

function tsToHms(ns: number): string {
  // ITCH timestamps are ns since midnight. Convert straight to HH:MM:SS.mmm.
  const totalMs = Math.floor(ns / 1_000_000);
  const ms = totalMs % 1000;
  const totalS = Math.floor(totalMs / 1000);
  const s = totalS % 60;
  const m = Math.floor(totalS / 60) % 60;
  const h = Math.floor(totalS / 3600) % 24;
  const pad = (n: number, w = 2) => n.toString().padStart(w, '0');
  return `${pad(h)}:${pad(m)}:${pad(s)}.${pad(ms, 3)}`;
}
