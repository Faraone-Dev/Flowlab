# ingest

Go 1.22 ingest layer. The only component in FLOWLAB that touches the
network. Writes raw feed batches to the shared mmap ring where the
Rust replay layer picks them up zero-copy.

## Status

| Component                          | Status                                                       |
| ---------------------------------- | ------------------------------------------------------------ |
| `mmap/ring.go` (Linux/macOS/FreeBSD) | Implemented — POSIX `mmap`, atomic Acquire/Release on indices |
| `mmap/ring_windows.go`             | Implemented — Win32 `CreateFileMapping` / `MapViewOfFile`, byte-identical layout to POSIX side, atomic release on `writeIdx` |
| `feed/websocket.go`                | Implemented — `gorilla/websocket` client with reconnect + backpressure-aware writes |
| `cmd/ingest/`                      | Daemon entry point                                           |
| Unit tests                         | None yet                                                     |

## Contents

| Path                   | Role                                                  |
| ---------------------- | ----------------------------------------------------- |
| `cmd/ingest/main.go`   | Daemon entry point                                    |
| `feed/websocket.go`    | WebSocket client with reconnect + write-side backpressure |
| `mmap/ring.go`         | POSIX `mmap` ring writer (Linux / macOS / FreeBSD)    |
| `mmap/ring_windows.go` | Win32 `CreateFileMapping` / `MapViewOfFile` ring writer (Windows) |

## Ring layout (shared with `flowlab-replay::ring_reader`)

```
0        "FLOWRING"     magic (8 B)
8        capacity       u64 LE  — power of two
64       writeIdx       u64 atomic   (own cache line)
128      readIdx        u64 atomic   (own cache line)
192      payload        capacity bytes
```

- Single producer (this process) / single consumer (Rust).
- Release store on `writeIdx` after payload; Acquire load on reader.
- Cache-line padding between writer and reader indices avoids false
  sharing.
- Backpressure: when the ring is full, `Write` returns an error and
  the WebSocket loop sleeps briefly. There are **no silent drops**.
- Batches are written contiguously (or wrap once) so the reader never
  observes a partial batch.

## Boundaries

Go is the **non-deterministic** layer. It handles:

- WebSocket / HTTP / file ingestion
- Reconnect, retry, write-side backpressure

Go **never** participates in replay logic. Once bytes are in the
ring, Go's job is done.

## Build & run

```bash
cd ingest
go vet ./...
go build ./...
go test -race -count=1 ./...   # currently a no-op: no test files
```

## Windows

`ring_windows.go` uses
`golang.org/x/sys/windows.{CreateFileMapping, MapViewOfFile}` and
produces the exact same on-disk layout as the POSIX path: 8-byte
`FLOWRING` magic, `u64 LE` capacity, write/read indices on their own
64-byte cache lines, payload starting at offset 192. Index updates use
`sync/atomic` on raw `*uint64` pointers into the mapped region; on
AMD64/ARM64 these act as release/acquire fences for the surrounding
payload writes. The Rust ring reader (`memmap2`-backed) consumes the
same file unchanged.
