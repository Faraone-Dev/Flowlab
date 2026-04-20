# FLOWLAB

**Deterministic Multi-Language HFT Replay & Microstructure Research System**

Rust + C++20 + Zig + Go &nbsp;|&nbsp; 81 Tests Passing (incl. 10 M-event drift, multi-thread chaos, ITCH fuzz) &nbsp;|&nbsp; 40 B Canonical Event ABI &nbsp;|&nbsp;
MoldUDP64 + WAL + SPSC mmap ring &nbsp;|&nbsp; Cross-Impl Hash Verified &nbsp;|&nbsp;
6-Guard Fail-Closed Risk Gate

Multi-language pipeline for market-data replay, microstructure analytics,
HFT aggression detection, and strategy simulation under controlled stress.
Same input bytes produce identical state, byte-for-byte, across runs,
platforms, and languages.

> Research and simulation framework. Not a trading system.

---

## Core principles

- **Determinism first.** Sequence-driven execution. Wall-clock time is
  informational; it is never an input to ordering.
- **Event sourcing.** All state derives from an immutable, append-only
  event log. Snapshots are cached projections, never sources of truth.
- **Separation of I/O and computation.** Go handles the world; Rust
  owns the truth; C++ owns speed; Zig owns specialization.
- **Zero-copy across the boundary.** No serialization between ingest
  and core. Mmap ring → Zig parser → Rust normalizer.
- **Performance discipline.** Pre-allocated buffers, `#[repr(C)]`
  layouts, no hidden allocation in the hot path. Correctness is
  proved by cross-impl hash agreement *before* any micro-opt lands.

---

## Language responsibilities

Each language is used only where it is strictly superior.

| Concern        | Language | Principle                                          |
| -------------- | -------- | -------------------------------------------------- |
| I/O            | Go       | Network, reconnect, backpressure                   |
| Truth          | Rust     | Correctness, ownership, event log, state machine   |
| Speed          | C++20    | Hot-path kernels: book, matching, stats, hasher    |
| Specialization | Zig 0.13 | `comptime` ITCH parser, zero-branch dispatch       |

Go **never** participates in replay. C++ and Zig never touch the
network. The deterministic core has no runtime dependency on either
GC or syscalls beyond `read` / `mmap`.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        NON-DETERMINISTIC                          │
│  Go ingest  ── WS / HTTP / file ── reconnect ── backpressure     │
└──────────────────┬───────────────────────────────────────────────┘
                   │  mmap ring (SPSC, lock-free, atomic indices)
                   ▼
┌──────────────────────────────────────────────────────────────────┐
│                         DETERMINISTIC CORE                        │
│                                                                   │
│  Zig feed-parser  ── ITCH 5.0 / FIX / OUCH ── zero-copy          │
│        │                                                          │
│        │  extern "C"  (40 B Event, #[repr(C)], align 8)          │
│        ▼                                                          │
│  Rust normalizer ── canonical event log ── WAL ── snapshots      │
│        │                                                          │
│        ├── replay engine (flowlab-replay)                         │
│        ├── microstructure analytics + risk gate (flowlab-flow)   │
│        ├── HFT aggression detection (flowlab-chaos)               │
│        ├── strategy sandbox (flowlab-lab)                         │
│        └── state verifier (flowlab-verify)                        │
│                                                                   │
│  C++ hot path (hotpath/)  ←  FFI  ──  book, hasher, stats        │
└──────────────────────────────────────────────────────────────────┘
```

---

## Canonical event (frozen ABI)

```
offset  size  field
 0       8    ts              u64 LE, nanoseconds (informational)
 8       8    price           u64 LE, integer ticks
16       8    qty             u64 LE
24       8    order_id        u64 LE
32       4    instrument_id   u32 LE
36       1    event_type      u8
37       1    side            u8
38       2    _pad            [u8; 2]
                              ────────
                              40 B, align(8), #[repr(C)]
```

Properties:

- Little-endian on all supported targets.
- `bytemuck::Pod + Zeroable` on the Rust side; POD on the C++ side.
- Bit-identical across Rust / C++ / Zig; no padding ambiguity.
- Any change bumps the canonical L2 hash in `flowlab-verify`.

---

## Inter-language data path

No serialization layer exists between ingest and core.

```
Go (ingest)  ──[ mmap ring, SPSC, atomic indices ]──►  Zig (parser)
                                                          │
                         extern "C"  (40 B Event)         │
                                                          ▼
                                                   Rust (normalizer)
```

### Ring-buffer layout

```
0        "FLOWRING"        magic (8 B)
8        capacity          u64 LE (power of two)
64       writeIdx          u64 atomic
128      readIdx           u64 atomic
192      payload           capacity bytes
```

Single producer / single consumer. Release-store on `writeIdx` after
payload; acquire-load on the reader. Producer blocks on full; reader
never observes a partial batch.

### Transport (production)

- **MoldUDP64** (`session[10] | seq BE | count BE | [u16 BE len | msg]×count`)
- UDP multicast ingress with `GapTracker` and bounded forward buffer.
- `count = 0` → heartbeat; `count = 0xFFFF` → end-of-session.

---

## Durability & recovery

| Component                     | Guarantee                                                |
| ----------------------------- | -------------------------------------------------------- |
| WAL (`replay::wal`)           | 64 MiB segments, `len | CRC32 | payload`, torn-tail safe |
| Event log                     | Append-only, length-prefixed, CRC'd                      |
| Snapshots                     | Content-addressed; replay resumes from nearest seq       |
| Ring IPC                      | Backpressure, never silent drop                          |
| MoldUDP64 gap handling        | Bounded forward buffer; deterministic gap halt           |

Bit-exact replay is a test invariant: the WAL reproduces the canonical
L2 hash `0xf54ce1b763823e87` over 5000 events.

---

## Risk gate

[flow/src/circuit_breaker.rs](flow/src/circuit_breaker.rs) is the last
line of defence before any outbound order. Every submission path MUST
call `CircuitBreaker::check(&Intent)` and respect the `Decision`.

| Guard             | Trip condition                                          |
| ----------------- | ------------------------------------------------------- |
| Rate limit        | Token bucket (orders / sec)                             |
| Position cap      | `abs(net_pos + order_qty) > max_position`               |
| Daily-loss floor  | `cash_flow_ticks < -max_daily_loss_ticks`               |
| OTR ceiling       | `orders / max(1, trades) > max_otr` (post-warmup)       |
| Feed gap          | `gaps_within(window) >= gap_threshold`                  |
| Manual kill       | Operator latch                                          |

Tripping latches. Recovery is explicit (`reset`, `start_of_day`).
Fail-closed, always.

---

## Determinism model

### Sequencing

- Monotonic global sequence ID per event log.
- Compound key `(channel_id, seq)` for multi-feed scenarios.
- Timestamps are informational only; ordering never depends on them.
- Sequence gaps halt the engine deterministically — no silent skip.

### Cross-language verification

Every stage emits an `xxh3_64` digest with domain tag `FLOWLAB-L2-v1`.
Rust, C++, and Zig digests must match bit-for-bit:

```
Rust digest  ══╗
                ╠═══  MUST BE IDENTICAL
C++ digest   ══╝
```

Any mismatch is a hard failure. Replay aborts.

### Failure modes

| Failure                          | Behaviour                                   |
| -------------------------------- | ------------------------------------------- |
| Corrupted event (bad magic / CRC)| Rejected at normalization, never logged     |
| Sequence gap                     | Deterministic halt + gap report             |
| Parser error                     | `-1` return; caller handles; no partial log |
| Cross-language hash mismatch     | Replay aborted; state diff emitted          |
| Ring full                        | Producer blocks (backpressure, no drop)     |

No partial state mutation is ever allowed. Atomic all-or-nothing.

---

## Performance discipline

Rules enforced in code review and CI.

1. **Pre-allocate everything in the hot path.** `HashMap::with_capacity`,
   `Vec::with_capacity` for book levels, order lookup, event buffers,
   replay buffers. Zero reallocations during steady state.
2. **`#[repr(C)]` for every struct crossing the FFI.** `repr(align(n))`
   where required. Layout is part of the ABI; changing it bumps the
   canonical hash.
3. **No unaligned raw-pointer access.** Parser readers use
   `read_unaligned` only where the wire format demands it, and never
   for shared-memory structs.
4. **Swap hashers only with numbers.** The stdlib hasher is DoS-safe
   and the default. A faster hasher ships only with a bench proving
   the win on our workload.
5. **Correctness before speed, always.** A change that breaks the
   canonical L2 hash does not land, period.

These rules are aligned with the FFI and ABI contracts above: stable
layout plus pre-allocated capacity is what eliminates both layout
mismatches and memory churn.

---

## Project layout

```
flowlab/
├── core/                Rust: event, state machine, FFI to C++
├── replay/              Rust: engine, WAL, ring, ITCH, MoldUDP64, UDP
├── flow/                Rust: microstructure analytics + risk gate
├── chaos/               Rust (+C++): HFT aggression detection
├── lab/                 Rust: strategy sandbox
├── verify/              Rust: cross-language state hashing
├── hotpath/             C++20: book, matching sim, rolling stats
├── feed-parser/         Zig 0.13: comptime ITCH parsers, zero-copy
├── ingest/              Go: WS / HTTP / file ingest, mmap ring producer
├── api/                 Go: control plane + Prometheus
├── bench/               Rust: criterion benchmarks
├── data/                Binary event logs (raw + normalized)
├── .github/workflows/   CI matrix (Linux + Windows, ±native)
├── Cargo.toml           Rust workspace
├── go.work              Go workspace
├── Makefile             Unified build orchestration
└── README.md
```

Every top-level folder has its own `README.md` with the detailed
contract.

---

## Build

```bash
# full build (portable Rust + Zig + C++ + Go)
make all

# Rust workspace, portable only
cargo build --release

# Rust + native FFI (C++ hot path + Zig static lib)
cargo build -p flowlab-core --features native

# Zig feed parser
cd feed-parser && zig build -Doptimize=ReleaseFast

# Go services
cd ingest && go build ./...
cd api    && go build ./...
```

Prerequisites for `--features native`:

- Rust ≥ 1.83 (edition 2024)
- Zig 0.13.0 on PATH
- A C++20 toolchain: MSVC 2022 Build Tools on Windows, clang++ ≥ 16
  or g++ ≥ 12 elsewhere

---

## Test

```bash
cargo test --workspace                              # pure Rust
cargo test --workspace --features native            # + FFI
cd feed-parser && zig build test --summary all      # Zig unit tests
cd ingest      && go test -race -count=1 ./...      # Go
```

Current passing counts:

| Surface                                 | Tests  |
| --------------------------------------- | ------ |
| `flowlab-replay` (unit + integration)   | 31 + 2 |
| `flowlab-flow` (circuit breaker, etc.)  | 8      |
| `flowlab-core` / `bench` / others       | rest   |
| Zig `feed-parser`                       | 12     |

---

## Benchmarks

```bash
cargo bench -p flowlab-bench                        # pipeline, replay
cargo bench -p flowlab-bench --features native      # with C++ + Zig
```

All benches use pre-allocated buffers and seeded synthetic streams.
No I/O inside measured regions.

---

## Continuous integration

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs on every
push and pull request:

| Job            | OS matrix          | What it validates                    |
| -------------- | ------------------ | ------------------------------------ |
| `rust`         | Ubuntu + Windows   | `fmt`, `clippy -D warnings`, tests   |
| `rust-native`  | Ubuntu + Windows   | Zig + C++ FFI build, native tests    |
| `go`           | Ubuntu + Windows   | `go vet`, build, `go test -race`     |

CI is the source of truth for cross-platform determinism.

---

## Out of scope

- Live trading
- Production exchange connectivity
- Real-time risk or position management
- Arbitrage or mempool tooling

FLOWLAB is a research and simulation framework. All execution paths
operate on replayed data.

---

## License

Proprietary — see [LICENSE](LICENSE). All rights reserved.

For commercial licensing or usage permissions, contact
`Faraone-Dev@users.noreply.github.com`.
