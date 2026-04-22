import { useState } from 'react';

type StormKind =
  | 'PhantomLiquidity'
  | 'CancellationStorm'
  | 'MomentumIgnition'
  | 'FlashCrash'
  | 'LatencyArbProxy';

type StormSnapshot = {
  mode: 'Idle' | 'Active';
  kind?: StormKind;
  severity: number;
  seed: number;
  started_at_ms?: number;
  expires_at_ms?: number;
};

const KINDS: { kind: StormKind; label: string; color: string }[] = [
  { kind: 'PhantomLiquidity', label: 'PHANTOM',    color: '#9b59ff' },
  { kind: 'CancellationStorm', label: 'CANCEL',    color: '#ffae42' },
  { kind: 'MomentumIgnition', label: 'IGNITION',   color: '#ff5e3a' },
  { kind: 'FlashCrash',       label: 'CRASH',      color: '#ff3b30' },
  { kind: 'LatencyArbProxy',  label: 'LAT-ARB',    color: '#4f8cff' },
];

export function StormPanel({ snap, onChange }: { snap: StormSnapshot | null; onChange: () => void }) {
  const [severity, setSeverity] = useState(0.6);
  const [durSec, setDurSec] = useState(20);
  const [busy, setBusy] = useState(false);

  const fire = async (kind: StormKind) => {
    setBusy(true);
    try {
      await fetch('/storm/start', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          kind,
          severity,
          duration_ms: Math.max(1, Math.round(durSec * 1000)),
          seed: Math.floor(Math.random() * 0xffffffff),
        }),
      });
      onChange();
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    setBusy(true);
    try {
      await fetch('/storm/stop', { method: 'POST' });
      onChange();
    } finally {
      setBusy(false);
    }
  };

  const active = snap?.mode === 'Active';
  const remainMs =
    active && snap?.expires_at_ms ? Math.max(0, snap.expires_at_ms - Date.now()) : 0;

  return (
    <div className="storm-panel">
      <div className="storm-h">
        CHAOS ·{' '}
        {active ? (
          <span className="storm-badge active">{snap?.kind} · sev {snap?.severity.toFixed(2)}</span>
        ) : (
          <span className="storm-badge idle">IDLE</span>
        )}
      </div>

      <div className="storm-row">
        <label>SEVERITY</label>
        <input
          type="range"
          min={0}
          max={1}
          step={0.05}
          value={severity}
          onChange={(e) => setSeverity(parseFloat(e.target.value))}
        />
        <span className="storm-num">{severity.toFixed(2)}</span>
      </div>

      <div className="storm-row">
        <label>DURATION</label>
        <input
          type="range"
          min={5}
          max={120}
          step={5}
          value={durSec}
          onChange={(e) => setDurSec(parseInt(e.target.value, 10))}
        />
        <span className="storm-num">{durSec}s</span>
      </div>

      <div className="storm-grid">
        {KINDS.map((k) => (
          <button
            key={k.kind}
            className="storm-btn"
            disabled={busy}
            onClick={() => fire(k.kind)}
            style={{ borderColor: k.color, color: k.color }}
            title={`Fire ${k.kind} for ${durSec}s @ severity ${severity.toFixed(2)}`}
          >
            {k.label}
          </button>
        ))}
      </div>

      <div className="storm-row" style={{ marginTop: 4 }}>
        <button className="storm-stop" disabled={!active || busy} onClick={stop}>
          STOP STORM
        </button>
        {active && remainMs > 0 && (
          <span className="storm-rem">{(remainMs / 1000).toFixed(1)}s left</span>
        )}
      </div>
    </div>
  );
}

export type { StormSnapshot, StormKind };
