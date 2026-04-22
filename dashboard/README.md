# dashboard

Bloomberg-style live desk for FLOWLAB telemetry. React 18 + Vite 5 + TS 5
(strict), uPlot 1.6 for the streaming line/band charts. Render is
decoupled from the WebSocket cadence: ticks land in ring buffers at
~50 Hz, a 10 Hz `requestAnimationFrame` loop redraws the canvas.

## What it shows today

Six streaming line panels with ±2σ deviation bands computed online from
a 600-sample (~12 s) Welford rolling window:

| Panel              | Source field           | Notes                                |
| ------------------ | ---------------------- | ------------------------------------ |
| Mid price          | `mid_ticks`            | Integer ticks rendered as USD        |
| Spread             | `spread_ticks`         | Blows out under stress regime        |
| Book imbalance     | `imbalance`            | [-1, 1], bid-positive                |
| VPIN               | `vpin`                 | Toxic flow proxy                     |
| Latency p99        | `lat_p99_ns`           | Hot-path nanoseconds (real engine)   |
| Events / sec       | `events_per_sec`       | Throughput                           |

A right-column KPI sidebar tracks bid depth, ask depth, latency p50/p99,
throughput and breaker state with live z-scores. The top bar carries the
current regime classification (CALM / VOLATILE / AGGRESSIVE / CRISIS), a
feed-mode tag (`SYNTHETIC FEED` vs the real engine) and the
circuit-breaker latch.

z-scores are highlighted at |z| ≥ 2 (warn) and |z| ≥ 3 (halt). That is
how a desk reads a tape: numbers always matter, but unusual numbers are
the ones that drive action.

### Coming next (engine-side data already on the wire)

| Panel               | Source frame    | Status                                |
| ------------------- | --------------- | ------------------------------------- |
| Order book ladder   | `Book`          | Decoded but not yet rendered          |
| Latency per stage   | `Lat` (5 stages)| Only `apply.p99` surfaced today       |
| Risk-gate counters  | `Risk`          | Halted/reason wired; per-guard TODO   |

## Architecture

```
flowlab-engine (Rust, :9090)         api/ (Go, :8080)            Browser
────────────────────────────         ────────────────            ───────
[u32 len][u16 ver=1][JSON]  ── TCP ► EngineClient                useStream
  TelemetryFrame::Tick               + WS /stream  ─── WS ────►  (callbacks,
  + Header/Book/Risk/Lat                                          no React render)
                                                                       │
                                                                       ▼
                                                              Series ring buffer
                                                              + RollingStats (Welford)
                                                                       │
                                                                       ▼  10 Hz rAF
                                                                StreamChart (uPlot)
```

The WS pushes one JSON `Tick` per frame at ~50 Hz. Each tick goes into
per-metric ring buffers (no React render). A 10 Hz `requestAnimationFrame`
loop triggers a single re-render that snapshots all buffers and redraws
the charts. This keeps React work bounded regardless of feed rate.

The dashboard knows nothing about the engine wire format. The Go bridge
translates `TelemetryFrame::Tick` → the existing JSON `Tick` shape. Swap
the Rust source (`synthetic` → `ich` → `binance`) and the dashboard
keeps working unchanged.

## Run

Two modes. Both expose the same `Tick` JSON over `ws://localhost:8080/stream`,
so the dashboard is identical in either path.

### A) Real Rust engine (default for live work)

```bash
# terminal 1 — Rust runtime
cargo run -p flowlab-engine --release -- \
  --source synthetic --wire json --listen 127.0.0.1:9090 --tick-hz 50

# terminal 2 — Go bridge
cd ../api && go run ./cmd/api -addr :8080 -feed engine -engine 127.0.0.1:9090

# terminal 3 — dashboard
cd ../dashboard && npm install && npm run dev   # http://localhost:5173
```

Numbers in the Latency p99 panel are **real measurements** of
`HotOrderBook::apply` taken inside the Rust runtime, not Go-side
synthetic noise.

### B) Pure Go synthetic feed (offline dashboard hacking)

```bash
cd ../api && go run ./cmd/api -addr :8080            # default -feed=synthetic
cd ../dashboard && npm run dev
```

The synthetic feed in `api/server/feed.go` produces correlated
microstructure with stress bursts. Useful when iterating on the UI
without running the Rust runtime.

Vite proxies `/stream`, `/health`, `/status`, `/reset` to `localhost:8080`,
so there is no CORS dance in dev.

## Wiring to the real core

Swap the Source flag on `flowlab-engine` to feed real data:

| `--source`   | Status                                   | Boundary                              |
| ------------ | ---------------------------------------- | ------------------------------------- |
| `synthetic`  | Live today                               | Bringup / pipeline validation         |
| `ich`        | WIP — IchSource adapter on flowlab-replay| Nanosecond claims allowed (in-process)|
| `binance`    | WIP                                      | Engine-internal latency only — WAN is declared separately, never blurred |

The wire contract — `TelemetryFrame` in [`../engine/src/wire.rs`](../engine/src/wire.rs) — is what the dashboard depends on, not the source kind.
