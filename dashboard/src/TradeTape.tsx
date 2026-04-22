import type { TradePrint } from './types';
import { useEffect, useRef } from 'react';

// Newest at the BOTTOM (desk convention). Auto-scroll pinned to bottom.
export function TradeTape({ trades }: { trades: TradePrint[] }) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [trades.length]);

  if (!trades.length) {
    return (
      <div className="tape-empty">waiting for trades…</div>
    );
  }

  return (
    <div className="tape-body" ref={ref}>
      {trades.map((t, i) => {
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
