# Reference vs hot — three implementations, one canonical hash

> Status: **frozen 2026-05-06**
> Scope: correctness strategy for the order book hot path
> Reference: [bench/src/lib.rs](../bench/src/lib.rs) (test
> `cross_impl_l2_hash_agreement`),
> [core/src/orderbook.rs](../core/src/orderbook.rs),
> [core/src/hot_book.rs](../core/src/hot_book.rs),
> [core/src/ffi.rs](../core/src/ffi.rs)

---

## TL;DR

Flowlab carries **three independent order book implementations**. They
exist on purpose. Every push to `main` runs them on the same 50 000
synthetic ITCH events (seed `0xC0FFEE`) and asserts they all produce
the **same canonical L2 hash**. Any mismatch fails CI fail-fast, no
retry.

| # | Implementation        | Language | Layout                  | Role                                |
| - | --------------------- | -------- | ----------------------- | ----------------------------------- |
| 1 | `OrderBook`           | Rust     | `BTreeMap<Price, Level>` + `HashMap<OrderId, Order>` | reference oracle — boring, obvious |
| 2 | `HotOrderBook<256>`   | Rust     | fixed-array bids/asks + slab + sparse index | production hot path |
| 3 | `CppOrderBook`        | C++20    | template `OrderBook<MaxLevels>` (mirror of #2) | cross-language witness |

The interesting property is not "Flowlab has an order book". It is
**"Flowlab has three, and the build refuses to compile if they
disagree by one bit."**

---

## The problem an optimised hot path always has

A naive in-process order book is easy to make correct: walk a sorted
map, insert in order, sum quantities. Slow, but obviously right —
the data structure does the ordering work for you.

The hot path is the opposite. To hit nanosecond budgets it must be
ugly:

- bids/asks as **fixed `[Level; MAX_LEVELS]` arrays** (no `Vec`,
  no heap) — see [core/src/hot_book.rs](../core/src/hot_book.rs)
  lines 64–88.
- `align(64)` struct with an explicit **`_gap: [u8; 64]`** between
  bid and ask grids to prevent false sharing on SIMD loads
  ([core/src/hot_book.rs](../core/src/hot_book.rs) lines 73–82).
- order index split into a **dense `Vec<OrderMeta>` slab + sparse
  `HashMap<u64, u32>`** (12 B/entry instead of 32 B → ~5× more
  entries per cache line on lookup) —
  [core/src/hot_book.rs](../core/src/hot_book.rs) lines 113–123.
- `_mm_prefetch` one event ahead, hot/cold split on `trade()`,
  `ptr::copy` instead of bubble swaps for insertion.

Every one of those decisions is a knife. Each can silently produce
the wrong state and you would not notice until a downstream invariant
broke at 3 AM.

The standard answer in HFT firms is: **keep a slow reference
implementation alive forever, and validate the fast one against it on
every build.** Jane Street validates OCaml against C, Optiver
validates Python research against C++ tactical, HRT validates Python
against C++. The reference is not legacy code waiting to be deleted —
it is the oracle.

Flowlab does the same thing.

---

## How the gate works in this repo

The contract is one test, in
[bench/src/lib.rs](../bench/src/lib.rs) at `cross_impl_l2_hash_agreement`:

```rust
let raw    = synthetic_itch_stream(50_000, 0xC0FFEE);
let mut parsed = Vec::with_capacity(50_000);
parse_buffer(&raw, &mut parsed).unwrap();

let mut hot = HotOrderBook::<256>::new(1);   // fast tactical
let mut bt  = OrderBook::new(1);              // slow oracle
let mut cpp = CppOrderBook::new();            // C++ witness

for ev in &parsed {
    if matches!(ev.event_type,
        EventType::OrderAdd | EventType::OrderCancel | EventType::Trade)
    {
        hot.apply(ev);
        bt.apply(ev);
        cpp.apply(ev);
    }
}

let h_hot = hot.canonical_l2_hash();
let h_bt  = bt.canonical_l2_hash();
let h_cpp = cpp.state_hash();

assert_eq!(h_hot, h_bt,  "HotOrderBook / OrderBook (BTreeMap) mismatch");
assert_eq!(h_hot, h_cpp, "Rust / C++ L2 hash mismatch");
```

Same input. Same canonical L2 hash protocol (xxh3-64 with domain seed
`XXH3_64bits("FLOWLAB-L2-v1", 13)`, 16 B per level, side separator,
XOR-fold). Three implementations, one number. The expected agreement
value is `0xf54ce1b763823e87` — pinned in
[`docs/feed-design.md`](feed-design.md).

The test runs in CI as the dedicated job
**`Cross-language L2 hash agreement gate`** in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml). If any of
the three diverges by one bit, the build is red.

---

## Why three, not two

Two implementations would already be the textbook reference-vs-hot
pattern. Flowlab carries a third — the C++ `OrderBook<MaxLevels>`
template behind `--features native` — for a specific reason.

| If divergence is between… | Diagnosis                                       |
| ------------------------- | ----------------------------------------------- |
| Hot vs reference (Rust)   | bug in the optimised Rust path                  |
| Hot+ref vs C++            | bug in the cross-language ABI or in C++ kernel  |
| Reference vs C++          | bug in the canonical hash protocol (rare; both differ from hot in the same way) |

Two witnesses can disagree without telling you who is wrong. **Three
witnesses triangulate.** Two-against-one is a real signal, not a
coin flip.

This also enforces the 40 B `Event` ABI in code, not just in docs:
the C++ `OrderBook::apply` reads the same `Event` bytes as the Rust
ones; if the layout drifts on any side, the hashes diverge before the
next push.

---

## What the slow one is good for besides being right

The `OrderBook` (BTreeMap) is *not* benchmark fodder. It exists to:

1. **Be the oracle** for `cross_impl_l2_hash_agreement`. Its
   correctness is structural — `BTreeMap` cannot lose a level by
   forgetting to memmove an array. If the slow one says "best bid is
   X", that is true by construction.
2. **Document the spec in code.** Anyone reading `apply` in
   [core/src/orderbook.rs](../core/src/orderbook.rs) understands what
   an `OrderAdd` is *supposed* to do, in 30 lines, with no tricks.
   The hot path is then "do exactly this, but faster".
3. **Survive refactors of the hot path.** Any future rewrite of
   `HotOrderBook` (different cache strategy, SIMD apply, different
   slab eviction) is gated by the same hash agreement: the new fast
   path must reproduce what the slow path computes, byte for byte.

Deleting the BTreeMap implementation to "clean up" would silently
remove the oracle and convert the cross-impl test into "two
implementations of the fast pattern check each other" — which proves
nothing, because both could share the same bug.

---

## What this gate does *not* prove

Honest scope:

- It proves **state convergence**, not **performance**. The
  reference is allowed to be 100× slower; that is the deal.
- It proves **agreement on the canonical hash**, not full L3 order
  identity. Two books with the same aggregate qty per level but
  different per-order metadata would still hash equal. That is by
  design — `canonical_l2_hash` is the L2 invariant, and L3 details
  are validated by other tests (e.g. `cancel_heavy.rs`,
  `corruption_injection.rs` in [tests/](../tests/)).
- It runs on a synthetic ITCH stream of 50 000 events with a fixed
  seed. Real-world feed shapes (heavy cancel storms, corrupted
  packets, gap recovery) are validated separately by the chaos and
  fuzz suites.

---

## Why this matters in writing

A team that asks *"how do you trust your hot path?"* in interview is
not asking for "I wrote tests". They want to hear:

> Three independent implementations — boring Rust BTreeMap as
> oracle, optimised Rust array+slab as production, C++ template as
> cross-language witness — fed the same seeded event stream and
> required to produce the same canonical L2 hash. Gated in CI on
> every push, fail-fast, no retry.

That answer is one sentence and it is the difference between
"engineer who writes tests" and "engineer who designed a correctness
strategy". Flowlab's value is not the order book — it is the gate
around it.

---

## Limits we do not hide

- The triangulation only covers **L2 state**. L3 order-tracking
  drift requires the dedicated chaos tests (cancellation storm,
  corruption injection).
- 50 000 events at fixed seed is enough to catch **structural
  divergence** but not enough to catch a one-in-a-billion edge case;
  the fuzz harness ([feed-parser/src/fuzz.zig](../feed-parser/src/fuzz.zig))
  covers parser-level safety, but no fuzz currently runs the
  three-way book triangulation at random seeds.
- The C++ implementation lives behind `--features native`. Pure-Rust
  builds run only the two-way Rust gate (`HotOrderBook` vs
  `OrderBook`). The full triple-witness gate requires a C++ toolchain.
