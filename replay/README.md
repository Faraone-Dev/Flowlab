# flowlab-replay

Deterministic replay drivers: file, ring IPC, WAL, UDP multicast.
Every source emits the exact same `&[Event]` stream regardless of origin.

## Modules

| Module           | Purpose                                                           |
| ---------------- | ----------------------------------------------------------------- |
| `engine.rs`      | Driver loop: source → normalizer → state machine → digest         |
| `log.rs`         | Binary event log (append-only, length-prefixed, CRC'd)            |
| `snapshot.rs`    | Snapshot / restore for fast-forward replay                        |
| `itch.rs`        | NASDAQ ITCH 5.0 parser (framed + unframed) + synthetic stream     |
| `file_source.rs` | Offline ITCH file replay with `Pace::{AsFastAsPossible,RealTime}` |
| `ring_reader.rs` | Rust consumer of the Go mmap ring (magic `FLOWRING`)              |
| `wal.rs`         | Segmented 64 MiB WAL with CRC-32, torn-tail recovery, bit-exact   |
| `moldudp.rs`     | MoldUDP64 frame parser + `GapTracker` (bounded forward buffer)    |
| `udp_source.rs`  | UDP multicast ingress bound to MoldUDP64                          |

## Ring buffer layout (mmap, shared with Go `ingest/mmap/ring.go`)

```
0        "FLOWRING"                 magic (8 B)
8        capacity                   u64 LE (power of two)
64       writeIdx                   u64 atomic
128      readIdx                    u64 atomic
192      payload                    capacity bytes
```

Release/Acquire fences on both ends. Single producer (Go) / single
consumer (Rust) lock-free. Backpressure is mandatory: the writer
blocks; the reader never sees partial batches.

## WAL record format

```
u32 len_le | u32 crc32_le | payload[len]
```

Segmented into 64 MiB files `wal-{020}.log`. Torn tails at EOF are
skipped on recovery. Bit-exact replay is tested against the canonical
L2 hash `0xf54ce1b763823e87` over 5000 events.

## MoldUDP64

```
session[10] | seq u64 BE | count u16 BE | [u16 BE len | msg]×count
```

`count = 0` → heartbeat. `count = 0xFFFF` → end-of-session.
`GapTracker` maintains a bounded forward buffer (`BTreeMap<seq, msg>`)
and emits `GapStats { in_order_delivered, reordered_delivered,
gaps_opened/closed, dropped_overflow, duplicates_ignored }`.

## Determinism guarantees

- Same event log → same state, bit for bit.
- Sequence gaps halt the engine deterministically — no silent skip.
- No wall-clock dependencies: ordering is seq-driven only.

## Tests

`cargo test -p flowlab-replay`: 31 unit + 2 integration + 1 doctest.
