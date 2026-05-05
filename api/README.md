# api

Go 1.22 control plane and **CHAOS injector**. Three responsibilities:

1. **Bridge** the Rust `flowlab-engine` TCP telemetry stream to the
   dashboard's WebSocket contract (`/stream`).
2. **Adversarial control plane**: inject 5 kinds of microstructure
   storms into the live feed (`/storm/*`), record audit-grade run
   artefacts (`/run/*`), and proxy to an external trading bot
   (`/bot/*`).
3. **Admin** surface for liveness, status and operator actions
   (`/health`, `/status`, `POST /reset`).

The control plane is read-only with respect to the deterministic core.
The Rust process owns truth; storms only modulate the synthetic feed
that lives in this Go service.

## Endpoints

| Method | Path                 | Purpose                                          |
| ------ | -------------------- | ------------------------------------------------ |
| `GET`  | `/health`            | `{"status":"ok"}`                                |
| `GET`  | `/status`            | `{version, service, mode, replaying}`            |
| `GET`  | `/stream`            | WebSocket, ~50 Hz `Tick` JSON                    |
| `POST` | `/reset`             | Clears synthetic-feed breaker latch              |
| `POST` | `/storm/start`       | Fire a storm `{kind, severity, duration_ms, seed}` |
| `POST` | `/storm/stop`        | Cancel the active storm                          |
| `GET`  | `/storm/status`      | Snapshot of the storm controller                 |
| `POST` | `/run/start`         | Begin a recorded run (creates `data/runs/<id>/`) |
| `POST` | `/run/stop`          | Finalize `run.yaml`, close JSONL writers         |
| `GET`  | `/run/status`        | Active run (if any)                              |
| `GET`  | `/run/list`          | All runs on disk, newest first                   |
| `GET`  | `/run/{id}/yaml`     | Raw `run.yaml` for a finished run                |
| `GET`  | `/bot/state`         | Reverse-proxy to TARGET's `/api/state`           |
| `GET`  | `/bot/health`        | Reverse-proxy to TARGET's `/api/health`          |
| `GET`  | `/`                  | Serves the built dashboard (`dashboard/dist/`)   |

## Feed modes

Selected at startup with `-feed`:

| Mode        | Backing data path                                                            |
| ----------- | ---------------------------------------------------------------------------- |
| `synthetic` | In-process Go feed (`server/feed.go`) with storm injectors. Default.         |
| `engine`    | Dials `flowlab-engine` over TCP and decodes its versioned telemetry stream.  |

In `engine` mode the bridge:

- dials `-engine` (default `127.0.0.1:9090`),
- decodes frames `[u32 len][u16 ver=1][JSON payload]` (see [`../engine/src/wire.rs`](../engine/src/wire.rs)),
- keeps the latest `TelemetryFrame::Tick` available via an atomic
  pointer so the WS handler never blocks,
- redials with exponential backoff on disconnect (the dashboard sees
  the last known tick in the meantime, like a stale book on a real desk).

The dashboard contract — a single JSON `Tick` per WS frame — is
**identical** in both modes. Storms apply only to the synthetic feed
(the Rust engine owns its own determinism and is not corruptible from
the control plane).

## Storm injection

Each storm kind leaves a **distinct, observable fingerprint** on the
microstructure tick stream. The exact magnitudes live in
[`server/feed.go`](server/feed.go).

| Kind                | What changes in `Tick`                                       |
| ------------------- | ------------------------------------------------------------ |
| `PhantomLiquidity`  | `bid_depth` / `ask_depth` oscillate ±40% at ~1Hz             |
| `CancellationStorm` | `events_per_sec` ×4, `vpin` ↑ 0.45, `trade_velocity` → ~0.05 |
| `MomentumIgnition`  | `mid_ticks` drifts in the sign of current `imbalance`        |
| `FlashCrash`        | `mid_ticks` slides linearly, `spread_ticks` blows out 4×     |
| `LatencyArbProxy`   | `lat_p99_ns` += sev × 50,000; `lat_p50_ns` unchanged         |

`severity ∈ [0,1]`, `duration_ms ∈ [1, ∞)`, optional `seed`. The
`StormController` is single-storm at a time; firing a new one
overrides the active one immediately.

## Run recorder

`POST /run/start` opens a directory `data/runs/<UTC-timestamp>/` with
three artefacts:

| File             | Content                                                                |
| ---------------- | ---------------------------------------------------------------------- |
| `run.yaml`       | Hand-rolled YAML summary, finalized on `/run/stop`. Includes verdict.  |
| `events.jsonl`   | Append-only audit trail: `run_start`, `storm_start`, `storm_stop`, `run_stop` (one JSON object per line). |
| `ticks.jsonl`    | Sampled microstructure tick (default 1 Hz) for offline analysis.       |

Verdict on the `target.delta` (EUR P&L change during the run):

| Range          | Verdict           |
| -------------- | ----------------- |
| `> -10`        | `TARGET_INTACT`   |
| `-100 .. -10`  | `TARGET_DAMAGED`  |
| `< -100`       | `TARGET_KILLED`   |

The recorder polls the bot via the configured `/bot/state` URL twice
per run (start + stop) and embeds the equity / trades / wins / losses
delta into `run.yaml`. Polling is best-effort and non-blocking; if the
bot is down the recorder still produces a valid file with zeroed
target fields.

## Contents

| Path                          | Role                                              |
| ----------------------------- | ------------------------------------------------- |
| `cmd/api/main.go`             | Entry point. Flags: `-addr`, `-feed`, `-engine`.  |
| `server/server.go`            | `http.ServeMux` router + CORS + dashboard mount.  |
| `server/stream.go`            | WS handler; ~20 ms tick cadence + recorder hook.  |
| `server/feed.go`              | Synthetic Go feed with per-kind storm injectors.  |
| `server/storm.go`             | `StormController` (start/stop/snapshot).          |
| `server/recorder.go`          | 3-file run artefact writer.                       |
| `server/bot_proxy.go`         | Reverse proxy to TARGET bot endpoints.            |
| `server/engine_client.go`     | TCP bridge to `flowlab-engine`.                   |

## Build & run

```bash
cd api
go vet ./...
go build -o ../bin/flowlab-api.exe ./cmd/api

# A) Synthetic feed (no Rust runtime needed)
../bin/flowlab-api.exe                                    # :8080

# B) Real Rust engine
../bin/flowlab-api.exe -feed engine -engine 127.0.0.1:9090
```

Or use the orchestrator from the repo root:

```powershell
.\run-desk.ps1 -Feed synthetic -NoBrowser
```

## Scope

Read-only observability with **operator-controlled adversarial
injection**. No trading logic, no strategy state, no replay. The
storms only reshape the synthetic feed; they cannot reach the
deterministic Rust core. Everything here is advisory and can be
killed/restarted at any time without affecting recorded runs already
on disk.
