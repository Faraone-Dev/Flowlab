# api

Go 1.22 control plane. Two responsibilities:

1. **Bridge** the Rust `flowlab-engine` TCP telemetry stream to the
   dashboard's existing WebSocket contract (`/stream`).
2. **Admin** surface for liveness, status, and operator actions
   (`/health`, `/status`, `POST /reset`).

The control plane is read-only with respect to the deterministic core
and never participates in the data path. The Rust process owns truth.

## Status

| Endpoint              | Status                                                         |
| --------------------- | -------------------------------------------------------------- |
| `GET /health`         | Implemented — JSON `{"status":"ok"}`                           |
| `GET /status`         | Implemented — JSON `{version, service, mode, replaying}`       |
| `GET /stream`         | Implemented — WebSocket, ~50 Hz `Tick` JSON                    |
| `POST /reset`         | Implemented — clears synthetic-feed breaker latch              |
| `GET /metrics`        | TODO — Prometheus handler                                      |
| `POST /ingest/*`      | TODO — control-plane ingest commands                           |

## Feed modes

Selected at startup with `-feed`:

| Mode        | Backing data path                                                            |
| ----------- | ---------------------------------------------------------------------------- |
| `synthetic` | In-process Go feed (`server/feed.go`). UI dev only.                          |
| `engine`    | Dials `flowlab-engine` over TCP and decodes its versioned telemetry stream.  |

In `engine` mode the bridge:

- dials `-engine` (default `127.0.0.1:9090`),
- decodes frames `[u32 len][u16 ver=1][JSON payload]` (see [`../engine/src/wire.rs`](../engine/src/wire.rs)),
- keeps the latest `TelemetryFrame::Tick` available via an atomic
  pointer so the WS handler never blocks,
- redials with exponential backoff on disconnect (the dashboard sees
  the last known tick in the meantime, like a stale book on a real desk).

The dashboard contract — a single JSON `Tick` per WS frame — is
**identical** in both modes. Swapping feeds is a one-flag change.

## Contents

| Path                          | Role                                              |
| ----------------------------- | ------------------------------------------------- |
| `cmd/api/main.go`             | Entry point. Flags: `-addr`, `-feed`, `-engine`.  |
| `server/server.go`            | `http.ServeMux` router + CORS.                    |
| `server/stream.go`            | WS handler; ~20 ms tick cadence.                  |
| `server/feed.go`              | Synthetic Go feed (correlated stress bursts).     |
| `server/engine_client.go`     | TCP bridge to `flowlab-engine`.                   |

## Build & run

```bash
cd api
go vet ./...
go build ./...

# A) Synthetic feed (no Rust runtime needed)
go run ./cmd/api -addr :8080

# B) Real Rust engine
go run ./cmd/api -addr :8080 -feed engine -engine 127.0.0.1:9090
```

## Scope

Read-only observability and low-frequency control. No trading logic,
no strategy state, no replay. Everything here is advisory and can be
killed/restarted at any time without affecting the deterministic core.
