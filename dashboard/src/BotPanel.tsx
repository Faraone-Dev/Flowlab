import { useEffect, useState } from 'react';

type BotState = {
  online?: boolean;
  bot_name?: string;
  mode?: string;
  status_line?: string;
  quotes_connected?: boolean;
  trade_connected?: boolean;
  open_positions?: number;
  total_trades?: number;
  wins?: number;
  losses?: number;
  win_rate?: number;
  realized_pnl?: number;
  unrealized_pnl?: number;
  equity?: number;
  account_balance?: number;
  account_currency?: string;
  margin_level?: number;
  daily_high_pnl?: number;
  daily_low_pnl?: number;
  last_signal?: { pair?: string; direction?: string; strength?: number; at_ms?: number } | null;
  last_trade?: { pair?: string; side?: string; price?: number; pnl?: number; at_ms?: number } | null;
  pairs?: string[];
  spreads?: Record<string, number>;
};

function fmtMoney(v: number | undefined, cur: string | undefined) {
  if (v === undefined) return '—';
  const sign = v >= 0 ? '+' : '';
  return `${sign}${v.toFixed(2)} ${cur ?? ''}`;
}

function pnlClass(v: number | undefined) {
  if (v === undefined || v === 0) return 'pnl pnl-flat';
  return v > 0 ? 'pnl pnl-up' : 'pnl pnl-dn';
}

export function BotPanel() {
  const [s, setS] = useState<BotState | null>(null);
  const [err, setErr] = useState(false);

  useEffect(() => {
    let stopped = false;
    const tick = async () => {
      try {
        const r = await fetch('/bot/state');
        const j = (await r.json()) as BotState;
        if (!stopped) {
          setS(j);
          setErr(false);
        }
      } catch {
        if (!stopped) setErr(true);
      }
    };
    tick();
    const id = setInterval(tick, 1000);
    return () => {
      stopped = true;
      clearInterval(id);
    };
  }, []);

  const offline = !s || s.online === false;

  return (
    <div className="bot-panel">
      <div className="bot-h">
        TARGET · ZEUS-HFT{' '}
        {offline ? (
          <span className="bot-badge offline">OFFLINE</span>
        ) : (
          <span className="bot-badge online">{s?.mode ?? 'LIVE'}</span>
        )}
      </div>

      {offline && (
        <div className="bot-empty">
          waiting for zeus on :3001 …
          {err && <div className="bot-err">last fetch failed</div>}
        </div>
      )}

      {!offline && (
        <>
          <div className="bot-grid">
            <div className="bot-cell">
              <div className="bot-k">EQUITY</div>
              <div className={pnlClass((s?.equity ?? 0) - (s?.account_balance ?? 0))}>
                {fmtMoney(s?.equity, s?.account_currency)}
              </div>
            </div>
            <div className="bot-cell">
              <div className="bot-k">REALIZED</div>
              <div className={pnlClass(s?.realized_pnl)}>
                {fmtMoney(s?.realized_pnl, s?.account_currency)}
              </div>
            </div>
            <div className="bot-cell">
              <div className="bot-k">UNREAL</div>
              <div className={pnlClass(s?.unrealized_pnl)}>
                {fmtMoney(s?.unrealized_pnl, s?.account_currency)}
              </div>
            </div>
            <div className="bot-cell">
              <div className="bot-k">DAILY LO</div>
              <div className={pnlClass(s?.daily_low_pnl)}>
                {fmtMoney(s?.daily_low_pnl, s?.account_currency)}
              </div>
            </div>
            <div className="bot-cell">
              <div className="bot-k">DAILY HI</div>
              <div className={pnlClass(s?.daily_high_pnl)}>
                {fmtMoney(s?.daily_high_pnl, s?.account_currency)}
              </div>
            </div>
            <div className="bot-cell">
              <div className="bot-k">OPEN POS</div>
              <div className="bot-v">{s?.open_positions ?? 0}</div>
            </div>
            <div className="bot-cell">
              <div className="bot-k">TRADES</div>
              <div className="bot-v">{s?.total_trades ?? 0}</div>
            </div>
            <div className="bot-cell">
              <div className="bot-k">W / L</div>
              <div className="bot-v">
                {s?.wins ?? 0} / {s?.losses ?? 0}
              </div>
            </div>
            <div className="bot-cell">
              <div className="bot-k">WIN%</div>
              <div className="bot-v">{((s?.win_rate ?? 0) * 100).toFixed(1)}%</div>
            </div>
          </div>

          {s?.last_signal && (
            <div className="bot-line">
              <span className="bot-k">SIG</span>{' '}
              <span className="bot-v">
                {s.last_signal.pair} {s.last_signal.direction}{' '}
                str {(s.last_signal.strength ?? 0).toFixed(2)}
              </span>
            </div>
          )}
          {s?.last_trade && (
            <div className="bot-line">
              <span className="bot-k">TRD</span>{' '}
              <span className="bot-v">
                {s.last_trade.pair} {s.last_trade.side} @ {s.last_trade.price}{' '}
                <span className={pnlClass(s.last_trade.pnl)}>
                  {fmtMoney(s.last_trade.pnl, s?.account_currency)}
                </span>
              </span>
            </div>
          )}
          <div className="bot-conn">
            <span className={s?.quotes_connected ? 'dot dot-on' : 'dot dot-off'} /> quotes{' '}
            <span className={s?.trade_connected ? 'dot dot-on' : 'dot dot-off'} /> trade
          </div>
        </>
      )}
    </div>
  );
}
