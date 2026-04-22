import { useEffect, useRef, useState } from 'react';
import type { Tick } from './types';

/**
 * useStream — single-WebSocket subscription with auto-reconnect.
 *
 * Returns the latest Tick plus a connection status. Each tick is delivered
 * via a stable callback (`onTick`) instead of forcing a React re-render
 * per frame; that way we hit 50 Hz without thrashing the reconciler.
 */
export type StreamStatus = 'idle' | 'connecting' | 'live' | 'closed';

export function useStream(
  url: string,
  onTick: (t: Tick) => void,
  onFrameReceived?: () => void,
): StreamStatus {
  const [status, setStatus] = useState<StreamStatus>('idle');
  const onTickRef = useRef(onTick);
  onTickRef.current = onTick;
  const onFrameRef = useRef(onFrameReceived);
  onFrameRef.current = onFrameReceived;

  useEffect(() => {
    let ws: WebSocket | null = null;
    let stopped = false;
    let reconnectDelay = 250;

    const connect = () => {
      if (stopped) return;
      setStatus('connecting');
      ws = new WebSocket(url);
      ws.binaryType = 'arraybuffer';

      ws.onopen = () => {
        setStatus('live');
        reconnectDelay = 250;
      };

      ws.onmessage = (ev) => {
        // Dashboard contract: server sends TEXT JSON frames. Handle the
        // occasional proxy-wrapped binary frame defensively so a misrouted
        // byte blob never silently freezes the UI.
        onFrameRef.current?.();
        try {
          let text: string;
          if (typeof ev.data === 'string') {
            text = ev.data;
          } else if (ev.data instanceof ArrayBuffer) {
            text = new TextDecoder().decode(ev.data);
          } else {
            return;
          }
          const t = JSON.parse(text) as Tick;
          onTickRef.current(t);
        } catch {
          /* drop malformed frame, keep stream alive */
        }
      };

      ws.onclose = () => {
        setStatus('closed');
        if (stopped) return;
        setTimeout(connect, reconnectDelay);
        reconnectDelay = Math.min(reconnectDelay * 2, 4000);
      };

      ws.onerror = () => ws?.close();
    };

    connect();

    return () => {
      stopped = true;
      ws?.close();
    };
  }, [url]);

  return status;
}
