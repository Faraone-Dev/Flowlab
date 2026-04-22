import { useEffect, useState } from 'react';

type RunStatus = { active: boolean; id: string };

export function RunPanel() {
  const [st, setSt] = useState<RunStatus>({ active: false, id: '' });
  const [busy, setBusy] = useState(false);
  const [lastErr, setLastErr] = useState('');

  const refresh = async () => {
    try {
      const r = await fetch('/run/status');
      const j = (await r.json()) as RunStatus;
      setSt(j);
    } catch {
      /* ignore */
    }
  };

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 1500);
    return () => clearInterval(id);
  }, []);

  const start = async () => {
    setBusy(true);
    setLastErr('');
    try {
      const r = await fetch('/run/start', { method: 'POST' });
      if (!r.ok) {
        const t = await r.text();
        setLastErr(t.slice(0, 80));
      }
      await refresh();
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    setBusy(true);
    setLastErr('');
    try {
      const r = await fetch('/run/stop', { method: 'POST' });
      if (!r.ok) {
        const t = await r.text();
        setLastErr(t.slice(0, 80));
      }
      await refresh();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="run-panel">
      <div className="run-h">
        SCOREBOARD ·{' '}
        {st.active ? (
          <span className="run-badge rec">● REC {st.id}</span>
        ) : (
          <span className="run-badge idle">IDLE</span>
        )}
      </div>
      <div className="run-actions">
        {!st.active ? (
          <button className="run-btn run-start" disabled={busy} onClick={start}>
            START RUN
          </button>
        ) : (
          <button className="run-btn run-stop" disabled={busy} onClick={stop}>
            STOP &amp; SAVE
          </button>
        )}
      </div>
      <div className="run-hint">
        writes <code>data/runs/&lt;id&gt;/</code>{' '}
        events.jsonl · ticks.jsonl · run.yaml
      </div>
      {lastErr && <div className="run-err">{lastErr}</div>}
    </div>
  );
}
