# ingest

Go 1.22 ingest layer. The only component in FLOWLAB that touches the
network. Writes raw feed batches to the shared mmap ring where Zig
picks them up zero-copy.

## Contents

| Path                | Role                                                   |
| ------------------- | ------------------------------------------------------ |
| `cmd/`              | Binaries: ingest daemons and fixtures                  |
| `feed/websocket.go` | WebSocket client with reconnect + exponential backoff  |
| `mmap/ring.go`      | Portable ring writer (POSIX mmap)                      |
| `mmap/ring_windows.go` | Windows `CreateFileMapping` implementation         |

## Ring layout (shared with `flowlab-replay::ring_reader`)

```
0        "FLOWRING"     magic (8 B)
8        capacity       u64 LE  — power of two
64       writeIdx       u64 atomic
128      readIdx        u64 atomic
192      payload        capacity bytes
```

- Single producer (this process) / single consumer (Rust).
- Release store on `writeIdx` after payload; Acquire load on reader.
- Backpressure by blocking the producer when the ring is full. No
  silent drops, ever.
- Batches are atomic: the reader never observes a partial batch.

## Boundaries

Go is the **non-deterministic** layer. It handles:

- WebSocket / HTTP / file ingestion
- Decompression, feed multiplexing
- Reconnect, retry, metrics

Go **never** participates in replay logic. Once bytes are in the
ring, Go's job is done.

## Build & run

```bash
cd ingest
go vet ./...
go build ./...
go test -race -count=1 ./...
```
