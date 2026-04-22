import { useEffect, useMemo, useRef, useState } from 'react';
import { Spark } from './Spark';
import { RegimeStrip } from './RegimeStrip';
import { Ladder } from './Ladder';
import { TradeTape } from './TradeTape';
import { useStream, type StreamStatus } from './useStream';
import { REGIME_NAME, type Tick } from './types';

const CAP = 600; // ~12 s at 50 Hz

class Ring {
  readonly cap: number;
  readonly buf: Float64Array;
  write = 0;
  filled = false;
  constructor(cap: number) {
    this.cap = cap;
    this.buf = new Float64Array(cap);
  }
  push(v: number) {
    this.buf[this.write] = v;
    this.write = (this.write + 1) % this.cap;
    if (this.write === 0) this.filled = true;
  }
  ordered(): Float64Array {
    if (!this.filled) return this.buf.slice(0, this.write);
    const out = new Float64Array(this.cap);
    out.set(this.buf.subarray(this.write));
    out.set(this.buf.subarray(0, this.write), this.cap - this.write);
    return out;
  }
}

class RingU8 {
  readonly cap: number;
  readonly buf: Uint8Array;
  write = 0;
  filled = false;
  constructor(cap: number) {
    this.cap = cap;
    this.buf = new Uint8Array(cap);
  }
  push(v: number) {
    this.buf[this.write] = v;
    this.write = (this.write + 1) % this.cap;
    if (this.write === 0) this.filled = true;
  }
  ordered(): Uint8Array {
    if (!this.filled) return this.buf.slice(0, this.write);
    const out = new Uint8Array(this.cap);
    out.set(this.buf.subarray(this.write));
    out.set(this.buf.subarray(0, this.write), this.cap - this.write);
    return out;
  }
}

export function App() {
  const rings = useRef({
    mid: new Ring(CAP),
    spread: new Ring(CAP),
    imb: new Ring(CAP),
    vpin: new Ring(CAP),
    p50: new Ring(CAP),
    p99: new Ring(CAP),
    eps: new Ring(CAP),
    bidDepth: new Ring(CAP),
    askDepth: new Ring(CAP),
    tradeVel: new Ring(CAP),
    regime: new RingU8(CAP),
  }).current;

  const framesSeen = useRef(0);
  const lastTickRef = useRef<Tick | null>(null);
  const [feedLabel, setFeedLabel] = useState('…');
  const [, setFrame] = useState(0);
  const [latest, setLatest] = useState<Tick | null>(null);

  useEffect(() => {
    let cancelled = false;
    let attempt = 0;
    const probe = () => {
      if (cancelled) return;
      fetch('/status')
        .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
        .then((s: { mode?: string }) => {
          if (cancelled) return;
          const m = (s.mode ?? '').toLowerCase();
          if (m.startsWith('engine')) setFeedLabel('ENGINE · NASDAQ BX ITCH');
          else if (m.startsWith('synthetic')) setFeedLabel('SYNTHETIC');
          else setFeedLabel(s.mode ?? 'UNKNOWN');
        })
        .catch(() => {
          attempt += 1;
          if (!cancelled && attempt < 10) setTimeout(probe, 500);
          else if (!cancelled) setFeedLabel('UNKNOWN');
        });
    };
    probe();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let raf = 0;
    let last = performance.now();
    const loop = (t: number) => {
      if (t - last >= 100) {
        last = t;
        setLatest(lastTickRef.current);
        setFrame((n) => n + 1);
      }
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(raf);
  }, []);

  const url = useMemo(() => {
    const proto = window.location.protocol === 'https:' ? 'wss' : 'ws';
    return `${proto}://${window.location.host}/stream`;
  }, []);

  const status: StreamStatus = useStream(
    url,
    (t: Tick) => {
      rings.mid.push(t.mid_ticks / 10_000);
      rings.spread.push(t.spread_ticks / 10_000);
      rings.imb.push(t.imbalance);
      rings.vpin.push(t.vpin);
      rings.p50.push(t.lat_p50_ns);
      rings.p99.push(t.lat_p99_ns);
      rings.eps.push(t.events_per_sec);
      rings.bidDepth.push(t.bid_depth);
      rings.askDepth.push(t.ask_depth);
      rings.tradeVel.push(t.trade_velocity);
      rings.regime.push(t.regime);
      lastTickRef.current = t;
    },
    () => {
      framesSeen.current++;
    },
  );

  const imb = rings.imb.ordered();
  const p50 = rings.p50.ordered();
  const p99 = rings.p99.ordered();
  const bidDepth = rings.bidDepth.ordered();
  const askDepth = rings.askDepth.ordered();
  const regime = rings.regime.ordered();
  const vpin = rings.vpin.ordered();

  // Plot total visible liquidity, not bid-ask delta. The delta is almost
  // the same signal as imbalance: imbalance ~= (bid - ask) / (bid + ask),
  // so with a slowly-moving denominator the two charts look identical.
  // Total depth is the distinct question we care about here: "is the book
  // getting thicker or thinner overall?"
  const totalDepth = useMemo(() => {
    const n = Math.min(bidDepth.length, askDepth.length);
    const out = new Float64Array(n);
    for (let i = 0; i < n; i++) out[i] = bidDepth[i] + askDepth[i];
    return out;
  }, [bidDepth, askDepth]);

  // UTC clock, updated 4 Hz (desk convention).
  const [nowUtc, setNowUtc] = useState(() => new Date());
  useEffect(() => {
    const id = setInterval(() => setNowUtc(new Date()), 250);
    return () => clearInterval(id);
  }, []);
  const utcStr = nowUtc.toISOString().slice(11, 23) + 'Z';

  const symbol = latest?.symbol && latest.symbol.length > 0 ? latest.symbol : null;
  const instrument = latest?.instrument_id ?? 0;
  const headLeft = symbol
    ? `${symbol} · NASDAQ`
    : instrument
      ? `LOCATE ${instrument} · NASDAQ`
      : 'WARMING UP · NASDAQ';

  const midPx = latest ? latest.mid_ticks / 10_000 : 0;
  const microPx = latest && latest.microprice_ticks ? latest.microprice_ticks / 10_000 : midPx;
  const sprBps = latest?.spread_bps ?? 0;

  return (
    <div className="page">
      <header className="deskhead">
        <div className="deskhead-left">
          <div className="deskhead-symbol">{headLeft}</div>
          <div className="deskhead-sub">
            FLOWLAB · {feedLabel} · <Dot s={status} />
            {status.toUpperCase()}
          </div>
        </div>
        <div className="deskhead-clock">{utcStr}</div>
        <div className="deskhead-right">
          <div className="px-trio">
            <span className="px-label">MID</span>
            <span className="px-val">{fmt(midPx, 4)}</span>
            <span className="px-sep">|</span>
            <span className="px-label">MICRO</span>
            <span className="px-val">{fmt(microPx, 4)}</span>
            <span className="px-sep">|</span>
            <span className="px-label">SPR</span>
            <span className="px-val">{sprBps.toFixed(1)} bps</span>
          </div>
          <div className="deskhead-sub">
            FRAMES {framesSeen.current.toLocaleString()} · SEQ{' '}
            {latest?.seq.toLocaleString() ?? '—'} · REGIME{' '}
            <span className={`regime r${latest?.regime ?? 0}`}>
              {REGIME_NAME[latest?.regime ?? 0]}
            </span>{' '}
            ·{' '}
            {latest?.breaker_halted ? (
              <span className="halt">HALT · {latest.breaker_reason}</span>
            ) : (
              <span className="ok">ARMED</span>
            )}
          </div>
        </div>
      </header>

      <div className="board">
        <div className="charts">
          <Card title="VPIN · TOXICITY" value={fmt(latest?.vpin ?? 0, 3)}>
            {/* VPIN (volume-synchronised PIN) is the headline order-flow
                toxicity metric. The mid is fixed by construction on the
                synthetic feed, so plotting it is a flat line; VPIN moves
                with every burst phase and is what the regime classifier
                actually keys off. */}
            <Spark data={vpin} color="#f5a623" positiveOnly />
          </Card>
          <Card title="BOOK IMBALANCE" value={fmt(latest?.imbalance ?? 0, 3)}>
            {/* Autoscale: real-world imbalance lives in ±0.05 most of the
                time; clamping to ±1 made the line look frozen at zero. */}
            <Spark data={imb} color="#f5a623" />
          </Card>
          <Card
            title="DEPTH · TOTAL LIQ"
            value={`${fmt((latest?.bid_depth ?? 0) + (latest?.ask_depth ?? 0), 0)}`}
          >
            <Spark data={totalDepth} color="#f5a623" positiveOnly />
          </Card>
          <Card
            title="LATENCY · P50 / P99 · ns"
            value={`${fmt(latest?.lat_p50_ns ?? 0, 0)} / ${fmt(latest?.lat_p99_ns ?? 0, 0)}`}
          >
            <Spark data={p50} color="#4f8cff" data2={p99} color2="#ff3b30" positiveOnly />
          </Card>
        </div>

        <section className="ladder-wrap">
          <div className="ladder-h">ORDER BOOK · TOP 10</div>
          <Ladder bids={latest?.bids ?? []} asks={latest?.asks ?? []} />
          <StageLatStrip stages={latest?.stages} />
        </section>

        <section className="tape-wrap">
          <div className="tape-h">TRADE TAPE</div>
          <div className="tape-head">
            <span>TIME</span><span>PRICE</span><span>QTY</span><span>·</span>
          </div>
          <TradeTape trades={latest?.trades ?? []} />
        </section>

        <aside className="kpi">
          <div className="kpi-h">RISK · OPS</div>
          <Kpi k="MID" v={fmt(latest ? latest.mid_ticks / 10_000 : 0, 4)} />
          <Kpi k="IMBALANCE" v={fmt(latest?.imbalance ?? 0, 3)} />
          <Kpi k="BID DEPTH" v={fmt(latest?.bid_depth ?? 0, 0)} />
          <Kpi k="ASK DEPTH" v={fmt(latest?.ask_depth ?? 0, 0)} />
          <Kpi k="VPIN" v={fmt(latest?.vpin ?? 0, 3)} />
          <Kpi k="EVT/S" v={fmt(latest?.events_per_sec ?? 0, 0)} />
          <Kpi k="LAT P50" v={`${fmt(latest?.lat_p50_ns ?? 0, 0)} ns`} />
          <Kpi k="LAT P99" v={`${fmt(latest?.lat_p99_ns ?? 0, 0)} ns`} />
          <Kpi k="GAPS/MIN" v={fmt(latest?.gaps_last_minute ?? 0, 0)} />
          <Kpi k="REGIME" v={REGIME_NAME[latest?.regime ?? 0]} />
          <div className="kpi-strip">
            <div className="kpi-strip-h">REGIME · 12s</div>
            <div className="kpi-strip-b">
              <RegimeStrip data={regime} />
            </div>
          </div>
          <div className="kpi-actions">
            <button onClick={() => fetch('/reset', { method: 'POST' })}>RESET BREAKER</button>
          </div>
        </aside>
      </div>

      <footer className="bar small">
        <span><b>SRC</b> {feedLabel}</span>
        <span><b>WS</b> {status}</span>
        <span className="grow" />
        <span>© FLOWLAB</span>
      </footer>
    </div>
  );
}

function Card({ title, value, children }: { title: string; value: string; children: React.ReactNode }) {
  return (
    <div className="card">
      <div className="card-h">
        <span className="card-t">{title}</span>
        <span className="card-v">{value}</span>
      </div>
      <div className="card-b">{children}</div>
    </div>
  );
}

function Kpi({ k, v }: { k: string; v: string }) {
  return (
    <div className="kpi-row">
      <span className="kpi-k">{k}</span>
      <span className="kpi-v">{v}</span>
    </div>
  );
}

function Dot({ s }: { s: StreamStatus }) {
  const cls = s === 'live' ? 'dot ok' : s === 'connecting' ? 'dot warn' : 'dot idle';
  return <span className={cls} />;
}

function StageLatStrip({ stages }: { stages?: import('./types').StageLatencies }) {
  // Per-stage p50 / p99 mini bar. Shows up only when the engine has emitted
  // a Lat frame; synthetic feed leaves every stage at 0.
  if (!stages) return null;
  const rows: Array<[string, number, number]> = [
    ['PARSE',     stages.parse.p50_ns,     stages.parse.p99_ns],
    ['APPLY',     stages.apply.p50_ns,     stages.apply.p99_ns],
    ['ANALYTICS', stages.analytics.p50_ns, stages.analytics.p99_ns],
    ['RISK',      stages.risk.p50_ns,      stages.risk.p99_ns],
    ['WIRE',      stages.wire.p50_ns,      stages.wire.p99_ns],
  ];
  const max = Math.max(1, ...rows.map(([, , p99]) => p99));
  return (
    <div className="stage-lat">
      <div className="stage-lat-h">PIPELINE · P50 / P99 · ns</div>
      <div className="stage-lat-rows">
        {rows.map(([name, p50, p99]) => (
          <div className="stage-lat-row" key={name}>
            <span className="stage-lat-name">{name}</span>
            <span className="stage-lat-bar">
              <span className="stage-lat-fill-p99" style={{ width: `${(p99 / max) * 100}%` }} />
              <span className="stage-lat-fill-p50" style={{ width: `${(p50 / max) * 100}%` }} />
            </span>
            <span className="stage-lat-num">
              {p50.toLocaleString()} / {p99.toLocaleString()}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

function fmt(n: number, d: number): string {
  if (!isFinite(n)) return '—';
  return n.toLocaleString(undefined, { minimumFractionDigits: d, maximumFractionDigits: d });
}
