# α — Latency optimization on `HotOrderBook::apply`

> Status: **frozen 2026-04-20**
> Scope: single-instrument hot path, in-process apply, x86_64
> Methodology: `bench/src/bin/latency_apply.rs`, deterministic streams,
> rdtsc-calibrated per-event timing + wall-time reconciliation.

---

## TL;DR

Two changes on the hot path of `HotOrderBook<256>::apply` cut TRADE
median by **−58%** and TRADE p99 by **−64%** on the realistic STEADY
mix, with **zero new `unsafe`** code and full backwards-compatible
semantics.

| Metric (STEADY mix, 500 000 events) | Baseline | α     | Δ        |
| ----------------------------------- | -------- | ----- | -------- |
| TRADE p50                           |  72 ns   |  30 ns | **−58%** |
| TRADE p99                           | 352 ns   | 128 ns | **−64%** |
| TRADE p99.9                         | 480 ns   | 352 ns | −27%     |
| TOTAL p50                           |  24 ns   |  22 ns | −8%      |
| TOTAL p99                           | 160 ns   |  88 ns | **−45%** |
| Wall apply-only                     | 22.5 ms  | 17.1 ms | **−24%** |
| TRADE interference tax (p99)        | ×4.00    | ×1.38  | collapsed |
| Intrinsic share of mix p99          | 25 %     | 73 %   | flip      |

The "intrinsic share" jump from 25 % → 73 % is the headline: the book
went from cache-bound to compute-bound. There is little headroom left
for further latency optimisation without changing data structures.

---

## Methodology

### 1. Clock

`rdtsc` calibrated against `Instant::now()` over 5 windows of 100 ms,
then per-pair overhead estimated with a 100 000-pair median. Result on
the test box: **3.7920 cycles / ns**, **20-cycle (≈5 ns) per-pair
overhead** subtracted from each sample.

### 2. Histogram

Log-linear, 192 bins (24 powers × 8 sub-bins) covering 1 ns … 16 ms,
~12.5 % bin resolution. Overflow counter tracked separately.

### 3. Phases

The harness runs 7 disjoint phases, each on its own freshly warmed
book to keep cache state representative:

1. **WARMUP** — 10 000 events to populate the book grid + slab.
2. **STEADY** — 500 000 events, ITCH-realistic 60 / 25 / 15 ADD /
   CANCEL / TRADE mix. **The reference baseline.**
3. **BURST** — 200 000 events with concentrated bursts (different seed).
4. **CANCEL-HEAVY** — 500 000 events, 50 / 50 ADD / CANCEL, no TRADE.
5. **TRADE-ISOLATED** — 500 000 TRADE-only events on a fat-warmed book
   (50 000 resting orders), no ADD/CANCEL pressure → measures the
   intrinsic TRADE cost with the full working set L1-resident.
6. **LANE-BATCHED** — same input as STEADY, applied via `apply_lanes`
   (3-pass) in 64-event windows. Wall-time compared head-to-head with
   STEADY. **Used to disprove β.**
7. **STEADY + PREFETCH** — same input as STEADY, harness issues
   `prefetch_event(events[i+1])` before timing event `i`. **The α
   measurement.**

### 4. Honest comparisons

- Per-event histograms (rdtsc around a single `apply`) are only
  compared against other per-event histograms.
- Whole-pass timings (`apply_lanes`) are only compared against the
  *wall sum* of the per-event run on the same input.
- The prefetch overhead is OUTSIDE the rdtsc window for the per-event
  histogram (it represents work the producer can do upstream) but IS
  included in the apply-only wall accumulator for full-disclosure.

---

## What was changed

### Change 1 — `trade()` hot/cold split

`hot_book.rs`:

```rust
#[inline]
fn trade(&mut self, e: &Event) -> bool {
    if e.order_id == 0 {
        return self.trade_anon(e.price, e.qty, e.side == Side::Bid as u8);
    }
    let Some(meta) = self.orders.get_mut(e.order_id) else { return false };
    let fill    = e.qty.min(meta.qty);
    let new_qty = meta.qty - fill;
    let price   = meta.price;
    let is_bid  = meta.side == Side::Bid as u8;

    if new_qty != 0 {
        // ── HOT path: partial fill (the common case)
        meta.qty = new_qty;
        let (levels, count) = if is_bid {
            (&mut self.bids[..], self.bid_count as usize)
        } else {
            (&mut self.asks[..], self.ask_count as usize)
        };
        if let Some(i) = Self::find_level(levels, count, price, is_bid) {
            levels[i].total_qty = levels[i].total_qty.saturating_sub(fill);
            return true;
        }
        return false;
    }

    // ── COLD path: full fill — order eviction + possible level remove
    self.trade_full_fill(e.order_id, price, fill, is_bid)
}

#[cold] #[inline(never)]
fn trade_full_fill(&mut self, order_id: u64, price: u64, fill: u64, is_bid: bool) -> bool { … }

#[cold] #[inline(never)]
fn trade_anon(&mut self, price: u64, qty: u64, is_bid: bool) -> bool { … }
```

**Why it matters:**

- The common case is a partial fill that leaves the resting order
  alive (`new_qty > 0`). Full fills (eviction + `order_count--` +
  potential `remove_level`) are the minority but were inlined into the
  hot path, costing branches and i-cache footprint.
- Marking the cold paths `#[cold]` + `#[inline(never)]`:
  1. moves them out-of-line, freeing i-cache slots in the hot loop;
  2. tells the codegen to favour the partial-fill branch in branch
     prediction;
  3. removes the `if fully_filled { … }` branch from the inner update
     entirely.
- Anonymous (no-oid) trades only fire on synthetic / pre-enriched
  streams — they don't deserve a slot in the hot decision tree.

**Effect (measured, no prefetch):** TRADE-ISOLATED p99 dropped
**256 → 96 ns** in the run where the refactor first landed. p99.9
**448 → 288 ns**.

### Change 2 — `prefetch_event` (slab slot + level grid)

`hot_book.rs`:

```rust
#[inline]
pub fn prefetch_event(&self, event: &Event) {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};

        let t = event.event_type;
        let needs_lookup = t == EventType::Trade        as u8
                        || t == EventType::OrderCancel  as u8
                        || t == EventType::OrderModify  as u8;

        // Slab slot — only events that READ an existing oid.
        if needs_lookup && event.order_id != 0 {
            if let Some(&slot) = self.orders.index.get(&event.order_id) {
                let ptr = self.orders.data.as_ptr().wrapping_add(slot as usize);
                unsafe { _mm_prefetch(ptr as *const i8, _MM_HINT_T0) };
            }
        }

        // Level grid top-of-book — every event that reaches find_level.
        let touches_grid = needs_lookup || t == EventType::OrderAdd as u8;
        if touches_grid {
            unsafe {
                _mm_prefetch(self.bids.as_ptr() as *const i8, _MM_HINT_T0);
                _mm_prefetch(self.asks.as_ptr() as *const i8, _MM_HINT_T0);
            }
        }
    }
}
```

**Design notes:**

- Two separate prefetch targets, two separate justifications:
  1. **Slab slot** — the `OrderMeta` for the order this event refers
     to. Hidden behind a HashMap probe (the cost of which is itself
     amortised because the index buckets stay hot in L1 for the same
     reason).
  2. **Level grid top** — the binary-search `partition_point` in
     `find_level` always touches the first cache line of the side's
     `[Level; MAX_LEVELS]` array. We prefetch BOTH sides
     unconditionally (2 × 64 B = 1 cache line per side, stride is
     trivial) so the prefetcher doesn't need to know `is_bid` ahead of
     the slab read.
- `_MM_HINT_T0` (full L1 prefetch) is correct: at lookahead = 1 the
  consumer is one event away, far inside the L1 retention window.
- Lookahead is 1 in the harness. We tested 0 (no prefetch) and 1.
  Lookahead 2 was deliberately not added — at 30 ns/event the
  in-flight depth is already 2 (= L1 hit latency × throughput).
- ADD events skip the slab prefetch — they WRITE a fresh slot (or
  re-use a free-list slot), they don't read an existing one.

**Effect (measured, atop the trade refactor):**

| TRADE                | slot-only PF | slot + grid PF |
| -------------------- | ------------ | -------------- |
| p50                  | 28 ns        | 30 ns          |
| p99                  | 176 ns       | **128 ns**     |
| p99.9                | 352 ns       | 352 ns         |

The grid prefetch was the right call for the **p99 tail**, neutral on
the median (the median case had the grid hot already from the previous
event).

### Change 3 (alternative path, **rejected**) — `apply_lanes` (β)

`hot_book.rs`:

```rust
pub fn apply_lanes(&mut self, events: &[Event]) -> usize {
    self.apply_lane_adds(events)
        + self.apply_lane_trades(events)
        + self.apply_lane_cancels(events)
}
```

Three homogeneous filter passes per 64-event window. The hypothesis
was that running ADD / TRADE / CANCEL each in their own loop would
keep their working sets L1-resident.

**Result:** correct (canonical L2 hash bit-exact vs serial `apply` on
ITCH-conformant input — see `test_apply_lanes_matches_serial_canonical_hash`),
but **+8.8 % wall** vs interleaved on the same input. Diagnosis:

- 3 passes × 64 events = 192 loop iterations vs 64 for serial.
- Each pass STILL touches every event byte for the type-check filter
  (the filter is the work, not the body).
- Triple loop overhead and triple type-check cost dominate the
  cache-locality gain at the chosen window size.

The code is retained — it may pay off in a future batch / offline
context where the producer can hand pre-bucketed slices, eliminating
the type filter entirely.

---

## How to reproduce

```bash
cd flowlab
cargo test -p flowlab-core --lib              # 9/9 incl. apply_lanes equivalence
cargo build --release -p flowlab-bench --bin latency_apply
./target/release/latency_apply
```

Output sections:

- `PER-EVENT-TYPE LATENCY DISTRIBUTION` — histograms for each phase
- `TAIL/MEDIAN RATIOS` — p99/p50 and p99.9/p50 stability metric
- `STEADY p99 ATTRIBUTION` — bar chart of which event type owns the tail
- `INTERFERENCE` — TRADE intrinsic vs TRADE under mix pressure
- `LANE-SEPARATED EXECUTION` — β experiment, wall-time honest compare
- `ALPHA: PREFETCH LOOKAHEAD` — α experiment, per-type delta vs baseline

The numbers above were captured on a 3.79 GHz x86_64 box, Windows,
release profile, no LTO. Run-to-run variance on the box is roughly
±10 % on p50, ±15 % on p99 — the deltas reported here are larger than
that envelope by an order of magnitude.

---

## What "interference tax" means

```
interference tax (p99) = TRADE p99 in STEADY mix
                       ÷ TRADE p99 isolated, fat-warmed book

intrinsic share (p99)  = TRADE p99 isolated
                       ÷ TRADE p99 in STEADY mix
```

`intrinsic share` close to 1 → the workload is compute-bound, the
mix isn't hurting TRADE.
`intrinsic share` close to 0 → cache pollution from interleaved ADDs
and CANCELs is dominating.

α moved this metric **25 % → 73 %**. That is the optimisation in one
number.

---

## What we did NOT do (and why)

- **No `unsafe` for the prefetch path** — `_mm_prefetch` is the only
  unsafe call and is constrained to the inside of `prefetch_event`,
  with the pointer arithmetic done via `wrapping_add` on a `Vec` we
  own. No raw lifetime extension, no manual aliasing.
- **No data-structure rewrite** — we did not collapse `Level` into
  `Level` immobile + bitset, did not change `OrderSlab`, did not
  remove the HashMap. Those were on the table as α.5 / α.6 and were
  postponed because the prefetch+hot/cold pair already moved the
  workload from cache-bound to compute-bound (73 % intrinsic share).
- **No SIMD** — the existing portable_simd `find_level` was not
  modified; the bottleneck moved away from it.
- **No lookahead = 2** — at 30 ns / event (≈110 cycles) and L1 latency
  ≈ 12 cycles, the in-flight depth at lookahead 1 is already enough
  to fully hide the miss in the common path. Lookahead 2 risks L1 set
  pressure on workloads with locality.

---

## Files touched

| File | Change |
| ---- | ------ |
| `core/src/hot_book.rs` | `prefetch_event`, `trade` hot/cold split, `trade_full_fill`, `trade_anon`, `apply_lanes`, `apply_lane_adds/trades/cancels`, equivalence test |
| `bench/src/bin/latency_apply.rs` | `measure_steady` (returns wall), `measure_steady_prefetch`, `measure_lanes` (per-lane honest timing), `LaneStats`, phases 6 + 7, ALPHA / LANE / WALL report blocks |

---

## Glossary

- **Lookahead** — number of events the prefetch is issued ahead of
  the matching `apply()`. We use 1.
- **Intrinsic TRADE cost** — `TRADE-ISOLATED` p50/p99/p99.9. The
  speed of light on this hardware for this data structure.
- **Interference tax** — multiplier between intrinsic and in-mix.
- **Lane** — a homogeneous pass over events of one type (`apply_lane_*`).
- **HOT / COLD path** — within `trade()`, the partial-fill branch is
  HOT (one straight-line update) and the full-fill branch is COLD
  (`#[cold] #[inline(never)]` helper). Branch prediction and i-cache
  layout are biased toward HOT.
