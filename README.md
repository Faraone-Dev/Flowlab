# FLOWLAB

**Deterministic Multi-Language HFT Replay & Microstructure Research System**

Rust core + Zig 0.13 ITCH parser + C++20 hot kernels + Go ingest &nbsp;|&nbsp;
83 tests (71 Rust + 12 Zig) &nbsp;|&nbsp; 40 B canonical Event ABI &nbsp;|&nbsp;
MoldUDP64 + WAL + SPSC mmap ring &nbsp;|&nbsp; Rust↔Zig↔C++ canonical L2 hash bit-identical &nbsp;|&nbsp;
6-guard fail-closed risk gate &nbsp;|&nbsp;
Live React/uPlot dashboard fed by a real Rust runtime over a versioned
TCP wire — see [`engine/`](engine/) and [`dashboard/`](dashboard/).

Multi-language pipeline for market-data replay, microstructure analytics,
HFT aggression detection, and strategy simulation under controlled stress.
Same input bytes produce identical state, byte-for-byte, across runs and
platforms.

> Research and simulation framework. Not a trading system.
>
> **Modeled:** deterministic order flow replay, microstructure analytics,
> chaos pattern detection, strategy harness against a replayed book.
> **Not modeled:** market impact, queue position, fill probability,
> exchange matching, latency arbitrage outcomes, real PnL.

---

## Posizionamento

**flowlab è il livello deterministico di dati e analytics su cui poggia
uno stack di ricerca HFT, non lo stack di trading completo.**

Quattro linguaggi, una sola verità: Rust possiede lo state machine,
Zig il parser, C++ i kernel caldi, Go l'I/O. Le invarianti
cross-implementation — canonical L2 hash bit-identical, Event ABI da
40 B, analytics replay-stable, WAL con halt deterministico sui gap —
sono validate in CI, non assunte.

Lo scope è una scelta precisa. Uno stack di strategia serio ha
bisogno di un substrato riproducibile **prima** di matching engine,
fill model e queue tracking. Quei layer sono volutamente fuori dal
perimetro: dipendono da assunzioni venue-specific (NASDAQ ITCH vs
CME MDP3 vs CBOE PITCH) e da informazioni proprietarie (queue
position, fill probability, market impact) che non devono contaminare
la base. flowlab fornisce le primitive su cui un layer di matching
coerente può essere costruito; non finge di essere quel layer.

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

The project is **Rust-core**: ~70% of the code (and the entire
deterministic state machine, replay, WAL, risk gate, analytics) is
Rust. The other languages are scoped specializations.

| Concern        | Language | Scope                                              | LOC |
| -------------- | -------- | -------------------------------------------------- | --- |
| Truth          | Rust     | Event ABI, state machine, replay, WAL, analytics   | ~7,400 |
| Specialization | Zig 0.13 | `comptime` ITCH 5.0 parser, zero-copy              | ~480 |
| Speed (opt-in) | C++20    | L2 book + Welford stats behind `--features native` | ~330 |
| I/O            | Go       | mmap ring writer + WebSocket ingest + control API  | ~430 |

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

## Live runtime + dashboard

The deterministic core can be driven by a runtime binary that exposes a
versioned telemetry stream over TCP. A Go bridge consumes that stream
and re-broadcasts it as JSON over WebSocket to a React/uPlot dashboard.

```
flowlab-engine (Rust, :9090)
    ├── Source         : synthetic | ich (WIP) | binance (WIP)
    ├── Pipeline       : HotOrderBook + VPIN + spread + imbalance
    ├── Risk gate      : CircuitBreaker (probed for latency)
    ├── Backpressure   : bounded mpsc(8192), drop counter exposed
    └── Wire           : [u32 len][u16 ver=1][bincode|json payload]
                                │
                                │ TCP
                                ▼
                      api/ (Go, :8080)
                     EngineClient + WS /stream
                                │
                                ▼
                  dashboard/ (React + Vite + uPlot)
                  6 streaming panels + ±2σ bands + KPI sidebar
```

Frame schema lives in [`engine/src/wire.rs`](engine/src/wire.rs)
(`TelemetryFrame::{Header, Tick, Book, Risk, Lat, Heartbeat}`). Every
`Tick` carries **two clocks** kept deliberately separate:

- `event_time_ns`   \u2014 source-provided wall clock (ITCH ts48, Binance E)
- `process_time_ns` \u2014 engine `CLOCK_MONOTONIC` at apply
- `latency_ns = process - event` is computed only where the two clocks
  are comparable (replay yes, crypto WAN no).

### Run the desk locally

```bash
# 1. start the Rust runtime
cargo run -p flowlab-engine --release -- \
  --source synthetic --wire json --listen 127.0.0.1:9090 --tick-hz 50

# 2. start the Go bridge
cd api && go run ./cmd/api -addr :8080 -feed engine -engine 127.0.0.1:9090

# 3. start the dashboard
cd ../dashboard && npm install && npm run dev   # http://localhost:5173
```

`-feed=synthetic` keeps the legacy in-process Go feed for offline
dashboard hacking. The dashboard contract is unchanged across modes.

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

### Clock model

The engine carries **two clocks** and never conflates them.

| Clock              | Source                                  | Used for                          |
| ------------------ | --------------------------------------- | --------------------------------- |
| `event_time_ns`    | feed payload (ITCH ts48, Binance `E`)   | display, microstructure analytics |
| `process_time_ns`  | engine `CLOCK_MONOTONIC` at `apply()`   | latency probes, replay diagnostics |

Neither clock affects ordering. Replay is driven by sequence ID alone,
so `process_time_ns` is reproducible across runs only in pure replay
mode; in live ingest it diverges by design. `latency_ns = process -
event` is reported only where the two clocks are actually comparable
(replay yes, crypto WAN no).

### Backpressure semantics

Every boundary that crosses runtime domains has an explicit
back-pressure rule. Silent drop is forbidden.

| Boundary                          | Rule                                              |
| --------------------------------- | ------------------------------------------------- |
| Go ingest → Rust core (mmap ring) | SPSC, producer **blocks** on full; reader never sees partial batch |
| MoldUDP64 ingress (forward buf)   | Bounded; overflow → deterministic gap halt        |
| Engine → telemetry bridge (mpsc)  | Bounded `mpsc(8192)`; drop counter exposed in `TelemetryFrame::Risk` |
| Bridge → WebSocket clients        | Per-client buffer; slow client is dropped, not the engine |

The deterministic core is never starved by a slow consumer and never
silently loses data; failures surface as halts or counters.

### Cross-language verification

Every stage emits an `xxh3_64` digest with domain tag `FLOWLAB-L2-v1`.

```
Rust digest  ══╗
                ╠═══  MUST BE IDENTICAL  (validated)
Zig digest   ══╝

C++ digest   ══════  WIP — see hotpath/README.md
```

- **Rust ↔ Zig:** validated. The Zig parser asserts at `comptime`
  that `@sizeOf(Event) == 40` and exports `flowlab_event_size()`,
  which Rust checks on every `parse_itch` call. Same input bytes
  produce the same canonical events.
- **Rust ↔ C++:** validated. The canonical L2 hash uses
  `XXH3_64bits` on both sides (single-header
  [`xxhash.h`](hotpath/include/xxhash.h) v0.8.3 in C++,
  `xxhash-rust` in Rust) with the same scheme: domain seed
  `XXH3_64bits("FLOWLAB-L2-v1", 13)`, 16 B per level
  `(price_le, total_qty_le)`, `'|'` side separator, XOR-fold per level.
  Cross-FFI bench in [`bench/benches/pipeline.rs`](bench/benches/pipeline.rs)
  asserts byte-for-byte digest equality over 50 000 events:
  Rust `HotOrderBook` and C++ `OrderBook` both produce
  `0xf54ce1b763823e87`.

Any digest mismatch within a verified pair is a hard failure: replay
aborts.

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

Current passing counts (verified by `grep -c '^\s*#\[test\]'` and
`grep -c '^test '`):

| Surface                                       | Tests  |
| --------------------------------------------- | ------ |
| `flowlab-replay` (unit + integration)         | 31 + 2 |
| `flowlab-flow` (circuit breaker, etc.)        | 7      |
| `flowlab-core` (`hot_book`, event, state)     | 9      |
| `flowlab-bench` (cross-impl hash agreement)   | 2      |
| `flowlab-e2e` (chaos + e2e + fuzz harnesses)  | ~20    |
| Zig `feed-parser` (`itch.zig` + `main.zig`)   | 12     |
| **Total**                                     | **83** |

Go `ingest/` and `api/` are infra-only (mmap ring producer + control
plane) and currently have no unit tests; they are exercised end-to-end
by the Rust replay + cross-impl hash harness.

---

## Benchmarks

```bash
cargo bench -p flowlab-bench                        # pipeline, replay
cargo bench -p flowlab-bench --features native      # with C++ + Zig
```

All benches use pre-allocated buffers and seeded synthetic streams.
No I/O inside measured regions.

### Reference numbers (best observed stable run)

| Bench                | Time           | Throughput        |
| -------------------- | -------------- | ----------------- |
| `itch_parse/10000`   | ~120-130 µs    | ~2.3-2.6 GiB/s    |
| `itch_parse/100000`  | ~1.22-1.35 ms  | ~2.3 GiB/s        |
| `full_hot/10000`     | —              | ~600-650 MiB/s    |

**Methodology** (required for any number above to be replicable):

- Reported as **best observed stable run under idle system**, not the
  mean across noisy runs. Multi-run variance is reported separately
  for diagnostics.
- Hardware: consumer x86-64 laptop, hybrid P/E-core CPU, AVX2.
- Power scheme: Windows **Balanced**, processor boost mode
  **AGGRESSIVE** (`PERFBOOSTMODE = 2`). No CPU pinning, no priority
  boost (the OS scheduler outperformed manual pinning on this rig).
- Thermal: passive idle, no sustained load before measurement.
- Harness: Criterion 0.5.1, 100 samples, 3 s warm-up + 5 s measurement.
- Compilers: `rustc --release` with `target-cpu=native`, MSVC 14.44
  `/O2 /Oi /arch:AVX2`, Zig 0.13 `ReleaseFast`.

Numbers are not portable across machines or thermal states. Reproduce
them on your own hardware before quoting.

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

## What is real, what is WIP

FLOWLAB documents both. Reviewers are expected to read source, not
banners.

**Implemented and tested:**

- Canonical 40 B `Event` ABI, frozen, `#[repr(C)]` + Zig `extern
  struct` + C++ POD layout
- `BTreeMap` reference orderbook + `HotOrderBook<256>` with slab-backed
  order index (Vec dense + aHash sparse, fixed seed)
- WAL: 64 MiB segments, CRC-32 per record, torn-tail recovery,
  bit-exact replay over 5000 events
- MoldUDP64 frame parser + `GapTracker` with bounded forward buffer
- SPSC lock-free mmap ring (Go writer ↔ Rust reader) with explicit
  Acquire/Release fences
- ITCH 5.0 parser in both Rust and Zig with cross-impl event
  agreement
- Microstructure analytics: imbalance, rolling spread, VPIN, price
  impact, threshold-based regime classifier (Calm/Volatile/
  Aggressive/Crisis)
- Circuit breaker: 6 guards, fail-closed latch, 7 unit tests
- Chaos infrastructure: clustering and stress-window extractor
- Three chaos integration tests: 10 M-event drift, corruption
  injection, multi-thread burst desync
- C++ `OrderBook<MaxLevels>` (flat-array L2) and Welford
  `RollingStats` header, callable through the FFI

**WIP — not production-grade today:**

| Item | Status | Reference |
| ---- | ------ | --------- |
| C++ SIMD batch stats | Header-only `RollingStats` is real; SIMD batch update is a TODO | [hotpath/src/stats.cpp](hotpath/src/stats.cpp) |
| Snapshot serialize/deserialize | Struct only; no on-disk format yet | [replay/src/snapshot.rs](replay/src/snapshot.rs) |
| Lab matching engine | `Strategy` trait + `StrategyExecutor` exist; orders are emitted but not matched against the book yet | [lab/src/executor.rs](lab/src/executor.rs) |
| Chaos pattern detectors | Quote-stuffing and spoofing implemented; the other 5 `ChaosKind` variants (PhantomLiquidity, CancellationStorm, MomentumIgnition, FlashCrash, LatencyArbitrage) are reserved enum members consumed by clustering/window but have no detector yet | [chaos/src/detection.rs](chaos/src/detection.rs) |
| Windows mmap ring writer | Linux/macOS/FreeBSD only; Windows path returns an explicit error and recommends WSL | [ingest/mmap/ring_windows.go](ingest/mmap/ring_windows.go) |
| Control API | `/health` and `/status` only; `/metrics` and `/ingest/*` are TODO | [api/server/server.go](api/server/server.go) |

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
