import { useEffect, useRef } from 'react';

/**
 * Spark — dead-simple canvas sparkline.
 *
 * Zero dependencies, zero lifecycle tricks. We own a <canvas> directly,
 * paint a polyline + a subtle baseline + the latest value, and resize the
 * backing store via ResizeObserver. Works identically in Firefox, Chrome,
 * Edge, and VS Code's Simple Browser.
 */
export interface SparkProps {
  data: Float64Array | number[];
  color?: string;
  /** Optional second overlaid series (shares the same Y scale). */
  data2?: Float64Array | number[];
  color2?: string;
  /** Clamp Y axis to [0, max]. Useful for latencies, depths, vpin. */
  positiveOnly?: boolean;
  /** Optional fixed vertical range. If provided, data is scaled into it. */
  min?: number;
  max?: number;
}

export function Spark(props: SparkProps) {
  const ref = useRef<HTMLCanvasElement | null>(null);
  const propsRef = useRef(props);
  propsRef.current = props;

  useEffect(() => {
    const canvas = ref.current;
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    let w = 0;
    let h = 0;
    let raf = 0;

    const draw = () => {
      const p = propsRef.current;
      const data = p.data;
      const data2 = p.data2;
      const n = data.length;
      ctx.clearRect(0, 0, w, h);
      if (n < 2 || w === 0 || h === 0) return;

      // find data range across both series when data2 is present
      let dmin = p.min ?? Infinity;
      let dmax = p.max ?? -Infinity;
      if (p.min === undefined || p.max === undefined) {
        for (let i = 0; i < n; i++) {
          const v = data[i];
          if (p.min === undefined && v < dmin) dmin = v;
          if (p.max === undefined && v > dmax) dmax = v;
        }
        if (data2) {
          const m = data2.length;
          for (let i = 0; i < m; i++) {
            const v = data2[i];
            if (p.min === undefined && v < dmin) dmin = v;
            if (p.max === undefined && v > dmax) dmax = v;
          }
        }
      }
      if (p.positiveOnly && dmin > 0) dmin = 0;
      if (dmin === dmax) {
        dmin -= 1;
        dmax += 1;
      }
      const pad = (dmax - dmin) * 0.08;
      dmin -= pad;
      dmax += pad;

      const padL = 4;
      const padR = 4;
      const padT = 6;
      const padB = 6;
      const plotW = w - padL - padR;
      const plotH = h - padT - padB;

      const xFor = (i: number, len: number) =>
        padL + (len === 1 ? plotW / 2 : (i / (len - 1)) * plotW);
      const yFor = (v: number) =>
        padT + plotH - ((v - dmin) / (dmax - dmin)) * plotH;

      // faint midline
      ctx.strokeStyle = 'rgba(90,90,90,0.18)';
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(padL, padT + plotH / 2);
      ctx.lineTo(w - padR, padT + plotH / 2);
      ctx.stroke();

      const drawSeries = (arr: Float64Array | number[], color: string, fill: boolean) => {
        const len = arr.length;
        if (len < 2) return;
        if (fill) {
          ctx.fillStyle = hexToRgba(color, 0.08);
          ctx.beginPath();
          ctx.moveTo(xFor(0, len), padT + plotH);
          for (let i = 0; i < len; i++) ctx.lineTo(xFor(i, len), yFor(arr[i]));
          ctx.lineTo(xFor(len - 1, len), padT + plotH);
          ctx.closePath();
          ctx.fill();
        }
        ctx.strokeStyle = color;
        ctx.lineWidth = 1.4;
        ctx.lineJoin = 'round';
        ctx.beginPath();
        for (let i = 0; i < len; i++) {
          const x = xFor(i, len);
          const y = yFor(arr[i]);
          if (i === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        }
        ctx.stroke();
        // last-value dot
        ctx.fillStyle = color;
        ctx.beginPath();
        ctx.arc(xFor(len - 1, len), yFor(arr[len - 1]), 2, 0, Math.PI * 2);
        ctx.fill();
      };

      const color = p.color ?? '#f5a623';
      // draw primary last so its dot sits on top
      if (data2) drawSeries(data2, p.color2 ?? '#d6493a', false);
      drawSeries(data, color, !data2);
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

    // redraw loop — cheap and bulletproof. Canvas clear + polyline of 600
    // points is sub-ms even on budget laptops. No scale trickery, no
    // library lifecycle, no "it stopped after 2 seconds" mystery.
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

function hexToRgba(hex: string, a: number): string {
  const m = hex.replace('#', '');
  const n = parseInt(
    m.length === 3 ? m.split('').map((c) => c + c).join('') : m,
    16,
  );
  const r = (n >> 16) & 0xff;
  const g = (n >> 8) & 0xff;
  const b = n & 0xff;
  return `rgba(${r},${g},${b},${a})`;
}
