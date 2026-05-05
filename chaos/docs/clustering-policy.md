# Chaos clusterer — merge policy & dual-window design

> Status: **frozen 2026-05-05**
> Scope: `flowlab_chaos::ChaosClusterer`, single-symbol episode clustering
> Bench: `bench/benches/clustering_throughput.rs`

---

## TL;DR

The previous clusterer merged any two `ChaosEvent`s whose seq distance
was below a single `max_gap`. That treated unrelated detector flags
from different actors as one episode, and refused to merge a
`FlashCrash` with the `MomentumIgnition` that triggered it (the two
detectors are explicitly anti-overlap'd at the detector level — once
both fire, they describe one event, not two).

The current policy decides on **kind + initiator + dual seq/time
window** and emits **three** severity scalars per cluster instead of
one. The change is additive: `peak_severity`, `new(max_gap)` and the
single-window behaviour are preserved bit-for-bit.

| Aspect              | Before                  | After                              |
| ------------------- | ----------------------- | ---------------------------------- |
| Merge condition     | `seq_close`             | `(seq_close OR time_close)` AND policy |
| Cross-actor merging | yes (silent collision)  | no (`Some(x) == Some(y)` only)     |
| `None` initiator    | merged with anything    | never merged (unknown ≠ shared)    |
| `Crash ↔ Ignition`  | only if same initiator  | always (shared price substrate)    |
| Severity output     | `peak_severity`         | `peak`, `mean`, `composite`        |
| Output ordering     | by `start_seq`          | by `start_seq` (unchanged)         |
| BC constructor      | `new(max_gap)`          | `new(max_gap)` — identical         |

---

## Merge policy

`should_merge(current_cluster, incoming_event)` is a 4-rule table.
Both windows must be checked first; if neither says "close", the merge
is rejected outright regardless of kind/initiator.

```text
                                seq_close = max_gap > 0
                                            && event.start_seq
                                                <= current.end_seq + max_gap
                                time_close = max_time_gap_ns > 0
                                            && event.start_ts_ns
                                                <= current.end_ts_ns + max_time_gap_ns

                                if !(seq_close || time_close) -> SPLIT
```

Then:

| # | Current kind   | Incoming kind   | Initiator pair        | Decision |
| - | -------------- | --------------- | --------------------- | -------- |
| 1 | K              | K               | `Some(x), Some(x)`    | **MERGE** |
| 2 | K              | K               | `Some(x), Some(y)` x≠y| SPLIT    |
| 3 | K              | K               | `None, *` or `*,None` | SPLIT    |
| 4 | FlashCrash     | MomentumIgnition| any                   | **MERGE** |
| 4'| MomentumIgnition| FlashCrash     | any                   | **MERGE** |
| 5 | K1             | K2 (else)       | any                   | SPLIT    |

The asymmetric `None` rule is deliberate. `None` in `ChaosEvent.initiator`
means *the detector could not identify a single actor for this flag*
(rate-window detectors like `QuoteStuff`, `CancellationStorm`). Two
`None`s do **not** count as "same actor" — they count as "two unknowns",
which is the safer assumption for an episode aggregator that feeds a
human dashboard.

The only cross-kind merge is `FlashCrash ↔ MomentumIgnition`. Those
two detectors share the same triggering price move by construction —
`MomentumIgnition` fires on the run-up, `FlashCrash` on the snap-back
of the same displacement. The `chaos/storm.rs` orchestrator already
prevents them from racing in the storm scenarios; treating them as one
cluster is the natural extension at the report layer.

---

## Dual seq + time window

A single `max_gap` over sequence numbers does not survive bursty
input. Two events 200 seq apart can be 50 µs apart on a quiet feed
and 5 ms apart in the middle of a 100 k-msg burst — the same gap
means very different things. The `PhantomLiquidity` detector already
runs a dual seq+time window internally for exactly this reason; the
clusterer now mirrors that policy:

```text
seq_close  iff max_gap          > 0  AND  Δseq  ≤ max_gap
time_close iff max_time_gap_ns  > 0  AND  Δts   ≤ max_time_gap_ns

merge candidate iff seq_close OR time_close
```

Either window can be disabled by passing `0`. The default
`new(max_gap)` constructor disables the time window and reproduces
the pre-dual-window behaviour bit-for-bit (same merge decisions →
same cluster sequence → same `peak_severity` per cluster).

---

## Composite severity

A cluster exposes three severity scalars because each one answers a
different question:

* `peak_severity` — the worst single flag in the episode. Triage
  metric: *how bad did it get at the worst instant?*
* `mean_severity` — average per-event severity. Useful when an
  episode is long: *did it stay bad or just spike once?*
* `composite_severity = peak * (1 + ln(count))` — combines worst
  instant with persistence. **This is the field the dashboard sorts
  by when ranking episodes.**

The `1 + ln(count)` factor was chosen against three alternatives:

| Formula                | count=1 | count=10 | count=100 | Rejected because |
| ---------------------- | ------- | -------- | --------- | ---------------- |
| `peak * count`         | 1×      | 10×      | 100×      | a 100-event mild episode dominates a 5-event severe one |
| `peak * sqrt(count)`   | 1×      | 3.16×    | 10×       | rewards volume too aggressively at the high end |
| **`peak * (1+ln(n))`** | **1×**  | **3.30×**| **5.61×** | **chosen — sub-linear, singleton == peak (`ln(1)=0`)** |
| `peak`                 | 1×      | 1×       | 1×        | persistence ignored; long episodes lost in the noise |

Two contracts the test suite pins:

* `singleton.composite_severity == singleton.peak_severity` exactly
  (because `ln(1) = 0`). A 1-event cluster ranks identically under
  both old and new ordering, so adding the field cannot demote any
  existing alert.
* For the same `peak`, persistence beats volume **sub-linearly**: a
  10-event episode ranks `(1+ln(10))/(1+ln(2)) ≈ 1.95×` above a
  2-event one, not 5×. A long mild storm cannot push a short severe
  spike off the dashboard.

`mean_severity` is incrementally maintained via a private
`sum_severity` field so absorbing the N-th event is O(1), not O(N).
`composite_severity` is recomputed once per absorb (one `ln` call,
one multiply).

---

## Performance

Smoke run, Win11 / i7, `cargo bench --bench clustering_throughput -- --quick`:

| Group                          | n      | Throughput     | per-event |
| ------------------------------ | ------ | -------------- | --------- |
| `clustering/seq_only`          | 1 000  | 10.5 Melem/s   |  ~95 ns   |
| `clustering/seq_only`          | 10 000 | 10.0 Melem/s   | ~100 ns   |
| `clustering/seq_only`          | 50 000 |  7.3 Melem/s   | ~136 ns   |
| `clustering/dual_window`       | 1 000  | 12.0 Melem/s   |  ~83 ns   |
| `clustering/dual_window`       | 10 000 |  9.9 Melem/s   | ~101 ns   |
| `clustering/dual_window`       | 50 000 |  6.9 Melem/s   | ~145 ns   |
| `clustering/shape/all_merge`   | 10 000 | 18.0 Melem/s   |  ~56 ns   |
| `clustering/shape/all_split`   | 10 000 |  4.6 Melem/s   | ~217 ns   |

Reads:

* **Dual window costs essentially nothing** on realistic input. The
  10k delta is `+1 ns/event` (within Win11 noise of ±15 ns); the 1k
  case is faster, which is also noise. The second branch is a single
  `u64` compare with one short-circuit on `max_time_gap_ns == 0`.
* **`all_split` is ~4× slower than `all_merge`** because every
  comparison spawns a new `ChaosCluster { events: vec![e.clone()] }`
  and pushes it into the output `Vec<ChaosCluster>`. The cost here is
  per-cluster setup, not the merge decision itself.
* **50k slower per-event than 10k** is the output `Vec<ChaosCluster>`
  realloc rounds (realistic input produces ~hundreds of clusters at
  50k). A future patch could pre-size with `Vec::with_capacity(n / 8)`
  if this group becomes a hot path; today it is not.

---

## What we did NOT do

* **No clustering across symbols.** The clusterer takes a flat
  `&[ChaosEvent]` and is symbol-agnostic. Cross-symbol episode
  detection is a different semantic problem (correlated venues,
  cross-listing arb) and belongs in a layer above this one.
* **No machine learning.** Every merge/split decision is a 4-line
  table lookup. ML would add training data, model versioning, and
  non-determinism — all three are explicitly outside FLOWLAB's
  scope (the project is a deterministic replay bench, not an
  online classifier).
* **No time-decay severity.** A flag's severity is taken as the
  detector emitted it. Time decay would either need a per-detector
  decay constant (parameter explosion) or a global one (semantically
  wrong — a 200 ms `FlashCrash` doesn't decay the same way as a
  5 s `CancellationStorm`).
* **No cross-kind merging beyond `FlashCrash ↔ MomentumIgnition`.**
  The other 5 detectors fire on independent substrates
  (rate / order-book shape / latency proxy) and merging them across
  kinds would lose information at the dashboard layer.
* **No best-cluster selection / overlap resolution.** Output is in
  `start_seq` order and may contain back-to-back clusters; consumers
  that want a single "headline episode per minute" should layer that
  on top.
* **No initiator inference for `None`.** If the detector reports
  `None` we keep `None`. We do not heuristically attribute `None`
  flags to nearby `Some(x)` flags — that would smear initiators
  across actors and make the field useless for downstream attribution.

---

## Tests pinning these contracts

`chaos/src/clustering.rs` test module covers:

* `same_kind_same_initiator_merges` — rule 1
* `same_kind_different_initiator_splits` — rule 2
* `unknown_initiators_do_not_merge` — rule 3 (the surprising one)
* `crash_and_ignition_merge_regardless_of_initiator` — rule 4
* `unrelated_kinds_never_merge` — rule 5
* `distant_events_split_even_when_eligible` — window arithmetic
* `empty_input_yields_no_clusters` — degenerate input
* `time_window_merges_when_seq_window_would_split` — dual-window
  rescue path (seq says split, time says merge)
* `seq_window_merges_when_time_window_would_split` — symmetric rescue
* `both_windows_disabled_never_merges` — both-zero degenerate
* `singleton_composite_equals_peak` — additivity contract
  (`ln(1) = 0`)
* `composite_grows_with_persistence_not_volume` — the
  `(1 + ln(count))` choice

12 tests, deterministic, no I/O.
