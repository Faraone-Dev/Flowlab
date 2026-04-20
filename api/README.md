# api

Go 1.22 control plane. HTTP admin surface and Prometheus metrics
exporter. Does not participate in the deterministic data path.

## Contents

| Path           | Role                                             |
| -------------- | ------------------------------------------------ |
| `cmd/`         | Main binaries                                    |
| `server/`      | HTTP router, health checks, metrics endpoint     |

## Endpoints

| Method | Path          | Purpose                                 |
| ------ | ------------- | --------------------------------------- |
| GET    | `/healthz`    | Liveness                                |
| GET    | `/readyz`     | Readiness (ring attached, producer up)  |
| GET    | `/metrics`    | Prometheus scrape endpoint              |
| POST   | `/ingest/*`   | Ingest control (start / stop / status)  |

## Metrics (partial)

| Metric                         | Type      | Labels            |
| ------------------------------ | --------- | ----------------- |
| `flowlab_ring_write_total`     | counter   | `producer`        |
| `flowlab_ring_backpressure_ns` | histogram | `producer`        |
| `flowlab_feed_reconnect_total` | counter   | `feed`, `reason`  |
| `flowlab_feed_msg_total`       | counter   | `feed`, `kind`    |

## Scope

Read-only observability and low-frequency control. No trading logic,
no strategy state, no replay. Everything here is advisory.
