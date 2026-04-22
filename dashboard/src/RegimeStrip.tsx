import { useEffect, useRef } from 'react';

/**
 * RegimeStrip — timeline band colored by regime (0..3).
 *
 * One pixel column per sample. At 50 Hz × 600 samples the strip covers
 * ~12 s of history. Great for spotting when the market flipped into
 * CRITICAL and how long it stayed there — the whole point of a regime
 * classifier nobody can see on a single KPI.
 *
 *   0 CALM       → green
 *   1 VOLATILE   → blue
 *   2 AGGRESSIVE → yellow
 *   3 CRITICAL   → red
 */
export interface RegimeStripProps {
  data: Uint8Array | number[];
}

const COLORS: Record<number, string> = {
  0: '#2ca25f',
  1: '#4f8cff',
  2: '#f0c419',
  3: '#ff3b30',
};

export function RegimeStrip({ data }: RegimeStripProps) {
  const ref = useRef<HTMLCanvasElement | null>(null);
  const dataRef = useRef(data);
  dataRef.current = data;

  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    let w = 0;
    let h = 0;
    let raf = 0;

    const draw = () => {
      const arr = dataRef.current;
      const n = arr.length;
      ctx.clearRect(0, 0, w, h);
      if (w === 0 || h === 0 || n === 0) return;

      const bandH = Math.max(12, Math.floor(h * 0.55));
      const y0 = Math.floor((h - bandH) / 2);

      // 1 px per sample stretched into plot width
      const plotW = w - 8;
      const xStep = plotW / n;

      for (let i = 0; i < n; i++) {
        const r = arr[i] | 0;
        ctx.fillStyle = COLORS[r] ?? '#5a5a5a';
        ctx.fillRect(4 + Math.floor(i * xStep), y0, Math.ceil(xStep) + 1, bandH);
      }

      // frame
      ctx.strokeStyle = 'rgba(90,90,90,0.3)';
      ctx.lineWidth = 1;
      ctx.strokeRect(4 + 0.5, y0 + 0.5, plotW, bandH);

      // legend ticks
      ctx.fillStyle = '#8a8a8a';
      ctx.font = '9px ui-monospace, Consolas, monospace';
      ctx.textBaseline = 'top';
      ctx.fillText('CALM  VOL  AGG  CRIT', 4, y0 + bandH + 4);
    };

    const resize = () => {
      const rect = canvas.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      w = Math.max(1, Math.floor(rect.width));
      h = Math.max(1, Math.floor(rect.height));
      canvas.width = Math.floor(w * dpr);
      canvas.height = Math.floor(h * dpr);
      canvas.style.width = `${w}px`;
      canvas.style.height = `${h}px`;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      draw();
    };

    const ro = new ResizeObserver(() => resize());
    ro.observe(canvas);
    resize();

    const loop = () => {
      draw();
      raf = requestAnimationFrame(loop);
    };
    raf = requestAnimationFrame(loop);

    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, []);

  return (
    <canvas
      ref={ref}
      style={{ display: 'block', width: '100%', height: '100%' }}
    />
  );
}
