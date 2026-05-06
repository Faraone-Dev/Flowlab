# Stack rationale — 4 + 1 languages, one source of truth

> Status: **frozen 2026-05-06**
> Scope: language choice per layer, FFI contract, why not single-language
> Reference: [README.md § Language responsibilities](../README.md),
> [core/src/event.rs](../core/src/event.rs),
> [feed-parser/src/itch.zig](../feed-parser/src/itch.zig),
> [hotpath/include/flowlab/](../hotpath/include/flowlab/),
> [api/server/](../api/server/),
> [dashboard/src/](../dashboard/src/)

---

## TL;DR

Flowlab uses Rust + Zig 0.13 + C++20 + Go + TS/React. This is **not
five languages for show**. Each layer is the language whose primitives
map to the problem one-to-one. The cross-implementation L2 hash gate
(Rust ↔ Zig ↔ C++, `0xf54ce1b763823e87`, enforced in CI) is what makes
the choice provable rather than aesthetic — if any of the three drift
on the same input, the build fails.

| Layer            | Language    | What the language gives me              | Why nothing else fits as well                                  |
| ---------------- | ----------- | --------------------------------------- | -------------------------------------------------------------- |
| Truth            | Rust        | ownership-bound determinism, no GC, `#[repr(C)]` ABI control | Go = GC pauses in hot path; C++ = no borrow checker for the state machine; Zig = ecosystem too small for replay/WAL/analytics breadth |
| Specialization   | Zig 0.13    | `comptime` size/layout proofs, zero-copy slicing, `extern struct` | Rust `const fn` is less ergonomic for binary parser shape; C = no type safety; C++ = parser would be 3× the code |
| Speed (opt-in)   | C++20       | template specialization, intrinsics, bit-identical XXH3 reference (`xxhash.h`) | Rust = no upstream XXH3 single-header reference, hashing risks drift; Zig = same ecosystem issue as above |
| I/O + control    | Go          | first-class WS/HTTP stdlib, goroutines, GC fine *outside* the deterministic core | Rust async = overkill for a control plane; Python = GIL + serialisation cost; Node = no native concurrency primitives |
| UI               | TS + React  | uPlot is a JS library, browsers are HTML/JS | Rust → WASM possible but adds a build step nobody asked for; Python Plotly is server-side only |

The total is **5 specialised tools** behind **one Event ABI**. The
core never changes language; only the surface around it does.

---

## The contract that holds it together

The 40 B canonical `Event` is the single binary contract every
language is forced to speak. It is defined once in
[core/src/event.rs](../core/src/event.rs) as `#[repr(C)]`, mirrored as
`extern struct` in [feed-parser/src/itch.zig](../feed-parser/src/itch.zig)
with a `comptime` assertion that `@sizeOf(Event) == 40`, and as a POD
struct in [hotpath/include/flowlab/](../hotpath/include/flowlab/) with
the same field order. Any layout drift in any language is caught at
build time before any runtime can mismatch.

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

The L2 hash protocol (xxh3_64 with domain seed `XXH3_64bits("FLOWLAB-L2-v1", 13)`,
16 B per level `(price_le, total_qty_le)`, `'|'` side separator,
XOR-fold) lives in three places that must agree byte-for-byte:

```
Rust  ──┐
Zig   ──┼── digest agreement enforced in CI on every push
C++   ──┘
```

If two of the three diverge on the same 5 000-event seeded stream, the
`Cross-language L2 hash agreement gate` job in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) fails the
build. This is the answer to the question "how do you know the
multi-language story is real and not decorative?". The answer is:
because if it weren't, CI would be red.

---

## Per-language: what it does, what it is forbidden from doing

### Rust — Truth

**Owns:** Event ABI definition, state machine (`HotOrderBook`), WAL,
replay engine, microstructure analytics, risk gate, snapshot codec,
chaos detection (`ChaosChain`), engine main loop, telemetry wire.

**Forbidden from:** spawning the I/O loop that talks to the network,
opening the TCP listener directly (the runtime binary does, but the
core crates do not), allocating in the hot path of `apply()`,
panicking on input it does not own (input validation lives at the
boundary, not in the state machine).

The deterministic core has no runtime dependency on a GC or on
syscalls beyond `read` / `mmap`. This is why Rust — not Go, not C++ —
is the only viable choice here. Borrow checker enforces the
zero-allocation discipline at compile time; ownership semantics
enforce the single-writer state-machine model without locks.

### Zig 0.13 — Specialization

**Owns:** ITCH 5.0 parser as `comptime`-checked binary slicer
(~490 LOC).

**Forbidden from:** networking, allocation, any operation that
touches state outside the parser.

`comptime` is the killer feature here: the size of every ITCH message
type is proven at compile time, and `@sizeOf(Event) == 40` is a build
error if it isn't. A Rust parser would have to do these checks with
`const fn` + `static_assertions!` — possible, but uglier and slower
to write. A C parser would have no type safety on the wire layout.
A C++ parser would need 3× the code with `static_assert` chains.

Zig 0.13 was chosen specifically because the language's design rules
("no hidden control flow, no hidden allocation, no hidden anything")
match the constraints of a binary parser one-to-one.

### C++20 — Speed (opt-in)

**Owns:** flat-array L2 `OrderBook<MaxLevels>`, Welford `RollingStats`
header, XXH3 hasher (vendored upstream `xxhash.h` v0.8.3 from Yann
Collet), FFI shim. ~530 LOC of hand-written code; the 6 620-line
`xxhash.h` is upstream reference and is **not counted as ours**.

**Forbidden from:** networking, replay state, any I/O.

C++ is opt-in behind `--features native`. The pure-Rust build of
Flowlab works on its own; C++ exists only to host two things that are
either (a) cleaner in C++ — flat-array book with template
specialization on `MaxLevels` — or (b) externally authoritative — the
XXH3 reference implementation. Vendoring `xxhash.h` rather than
linking `libxxhash` keeps the cross-impl hash bit-identical without a
system dep, which is the whole point of the gate.

A batched AVX2 kernel for `RollingStats` was prototyped and rejected
at engine tick cadence (~50 Hz, batch size 1, SIMD prologue dominates
over scalar Welford). Rationale preserved in
[`hotpath/src/stats.cpp`](../hotpath/src/stats.cpp).

### Go — I/O + control plane

**Owns:** mmap ring writer (Windows + POSIX), WebSocket ingest,
control plane HTTP server (`/storm/*`, `/run/*`, `/bot/*`, `/stream`),
synthetic chaos feed, recorder (`run.yaml` + `events.jsonl` +
`ticks.jsonl`), reverse-proxy to TARGET bot. ~2 800 LOC.

**Forbidden from:** participating in replay, owning any piece of
deterministic state, deciding what the canonical L2 hash is.

Go's standard library handles WebSocket, HTTP, JSON, and goroutines
without external deps and without ceremony. Rust async is technically
capable but is overkill for a control plane that mostly waits on
sockets and writes JSONL files. The GC is fine here precisely because
the deterministic core does not run inside the GC's domain — Go talks
to Rust through a TCP frame stream
([engine/src/wire.rs](../engine/src/wire.rs)) or through the SPSC
mmap ring. The boundary is sharp and one-way.

### TS + React — UI

**Owns:** the CHAOS desk dashboard (~1 200 LOC TS/TSX), uPlot panels,
WebSocket client, severity sliders, BotPanel.

**Forbidden from:** holding state that survives a page reload (the
recorder owns persistence), assuming reorderable streams (events
arrive in sequence order).

In production the React bundle is served as static files by the Go
process on `:8080`; there is no Node runtime running. The dashboard
talks only to Go endpoints — never directly to the Rust engine.

---

## What we did NOT do

| Alternative                                         | Why rejected |
| --------------------------------------------------- | ------------ |
| All-Rust (replace Zig parser + C++ hot path + Go control plane) | Possible but loses three properties: (a) no `comptime` size proofs in the parser, (b) no upstream XXH3 reference for hash agreement, (c) Rust async control plane adds complexity for no measured win |
| Rust + Python (instead of Go)                       | Python's GIL + serialisation cost on the WS hot path is ~10× the Go equivalent on the same workload; rejected on cadence grounds |
| Rust + C (instead of Rust + Zig + C++)              | Loses parser type safety; loses template-specialised hot kernels; gains nothing |
| C++ everywhere                                      | Loses Rust's borrow-check guarantees on the state machine; the entire WAL + replay determinism story becomes "trust me" instead of "compiler enforces it" |
| Rewrite `xxhash.h` in pure Rust                     | The reference implementation is the *definition* of XXH3; any rewrite risks subtle drift, defeats the gate. Vendoring upstream is the safe choice |

---

## Cost of the multi-language stack (honest)

The split is not free.

| Cost                          | Mitigation in this repo                                |
| ----------------------------- | ------------------------------------------------------ |
| Build complexity              | `make all` builds everything; `cargo build --release` is enough for Rust-only review; C++/Zig hidden behind `--features native` |
| CI matrix                     | 3 jobs × 2 OSes (Ubuntu + Windows) — kept narrow on purpose |
| Contributor onboarding        | Each top-level folder has its own README with the local contract |
| Risk of layout drift          | The L2 hash gate makes drift a build failure, not a runtime bug |
| Build prerequisites           | Zig 0.13 + C++20 + Rust 1.83 + Go listed explicitly in the README "Build" section |

The trade was explicit: pay one-time stack complexity to gain (a) the
right tool per layer, (b) provable cross-language determinism, (c) no
runtime dependency on a GC inside the deterministic core. For a
deterministic substrate that hosts live trading bots and whose core
promise is reproducibility under stress, this is the right trade.

---

## Reference points

For context, multi-language stacks are the norm — not the exception —
in the firms whose engineering style this repo aims at:

- Optiver: C++ + Python (research) + occasional Rust
- Jane Street: OCaml + C + Python
- IMC Trading: Java + C++ + Python
- Hudson River Trading: C++ + Python + Java
- Jump Trading: C++ + Python + Java + KDB
- Citadel Securities: C++ + Python + KDB

In every case the rule is the same: the *deterministic execution
core* is in the language with the strongest static guarantees and the
weakest hidden runtime; *I/O, research, and tooling* live in
languages chosen for ergonomics. Flowlab follows the same pattern at
hobby scale: Rust for the core, Zig/C++ for specialised hot kernels,
Go for I/O and control, TS for the UI.

---

## Limits we do not hide

- Cross-impl validation covers Rust ↔ Zig (event size + parse
  equivalence) and Rust ↔ C++ (canonical L2 hash). It does not cover
  C++ ↔ Zig directly; transitive equivalence holds only because both
  agree with Rust on the same bytes. A direct Zig ↔ C++ gate would be
  redundant given the Rust pivot.
- Go is excluded from cross-impl hashing by design. Adding Go would
  require a fourth XXH3 implementation behind a fourth FFI; the
  return is zero because Go never participates in replay.
- TS/React UI is not part of any determinism claim. It is a viewer
  for the deterministic stream.
