# Feed design — ITCH replay vs. synthetic chaos

> Status: **frozen 2026-05-06**
> Scope: source layer of `flowlab-engine`, `--source synthetic | itch`
> Reference: [engine/src/main.rs](../engine/src/main.rs),
> [engine/src/sources/](../engine/src/sources/),
> [chaos/src/](../chaos/src/)

---

## TL;DR

Two source kinds coexist in the engine, by design:

| Source      | Determinism | Adversarial control | Use for                               |
| ----------- | ----------- | ------------------- | ------------------------------------- |
| `itch`      | replay-bound (file = state) | none — fixed history | parser correctness, replay determinism, hash agreement |
| `synthetic` | seed-bound + parameter-bound | full — 5 storm kinds × severity × duration | chaos engineering, cross-machine repro of stress, target-bot stress runs |

ITCH replay validates that **the pipeline is correct** on real exchange
data. The synthetic feed validates that **the system behaves under
controlled stress**. They answer different questions, so we ship both.

---

## Why ITCH 5.0 alone is not enough for chaos

The NASDAQ ITCH 5.0 specification is the public reference for
exchange-grade L3 market data. We support it (parser in
[feed-parser/src/itch.zig](../feed-parser/src/itch.zig), Rust mirror in
`replay`) and use it as the **ground-truth** input for the cross-impl
hash gate (`0xf54ce1b763823e87` over 5 000 events).

But it has structural limits as a chaos input.

### 1. History is fixed; chaos requires authored corruption

ITCH is a recording. Once the file is on disk, the sequence of book
events is immutable. You can replay it forwards bit-exact, but you
**cannot inject** a 4× cancellation storm or a flash crash without
modifying the file — which breaks the canonical hash and silently
turns it into a different feed.

A serious adversarial bench needs the **opposite property**: the
ability to author a stress condition (`PhantomLiquidity ±40 % depth at
1 Hz`, `FlashCrash linear mid-slide + spread blowout`, etc.) at a
precise severity level, on a fresh seed, against a baseline. ITCH
files cannot give you that.

### 2. Cadence is event-driven, not control-friendly

ITCH packets arrive when the market produces them. A quiet symbol
emits a few hundred messages per second; a busy one a few hundred
thousand. The chaos test loop, by contrast, needs **a fixed tick
cadence** (`--tick-hz 50` by default) so that a 30-second `FlashCrash`
storm produces the same number of corrupted ticks across machines and
runs.

We did not want to "rewrite" ITCH cadence by re-bucketing — that
would break the parser-truth claim. We kept ITCH as the
"what-the-exchange-actually-said" input and built the synthetic feed
on top of the canonical Event ABI for the "what-we-want-to-test"
input.

### 3. Bit-exact reproducibility across machines

ITCH replay is reproducible only if every reviewer has the same file.
NASDAQ does not redistribute pcaps publicly. A synthetic feed seeded
with `(seed, kind, severity, duration_ms, tick_hz)` reproduces
**byte-identically** on any host without shipping data. This matters
for the "5/5 cross-CPU" claim in
[`latency-cross-hw.md`](latency-cross-hw.md): if the input had been
ITCH, half the result would have been "you must email us for the file".

### 4. Storms must compose with the determinism core

The chaos injectors live in [api/server/feed.go](../api/server/feed.go)
(synthetic Go feed) and downstream effects are detected by
[chaos/src/](../chaos/src/) (Rust). Both sides operate on the
canonical `Event` ABI (40 B, `#[repr(C)]`, `extern struct`). A storm
modifies the **distribution** of the synthetic stream (depth shape,
trade velocity, mid drift, latency tail) while the wire format,
sequence rule, and hash protocol stay identical. The deterministic
core never knows whether a tick comes from "synthetic clean" or
"synthetic + FlashCrash 0.85 × 30 s" — it just applies events.

This composability is the property we wanted. ITCH-based chaos would
have required either (a) corrupting on the wire and breaking the
parser invariant, or (b) post-parser injection with a parallel
"corrupted Event" path — both leak adversarial logic into the
deterministic core.

---

## What ITCH replay is for

The ITCH source is **not deprecated**. It is the only layer in the
engine that proves "we can read real exchange data without losing
sequence, gap-handling, or schema invariants". It runs in CI through
[engine/tests/ich_real.rs](../engine/tests/ich_real.rs) and the
parser tests in [feed-parser/src/](../feed-parser/src/).

| Concern                                 | Validated by                          |
| --------------------------------------- | ------------------------------------- |
| Parser correctness on real bytes        | `feed-parser/src/itch.zig` test block |
| Cross-impl event agreement (Rust ↔ Zig) | `flowlab_event_size()` + parse loop   |
| Canonical L2 hash bit-identical         | `bench/benches/pipeline.rs`           |
| WAL replay determinism                  | 5 000-event bit-exact replay test     |
| Sequence + gap handling                 | `replay/src/moldudp64.rs` + tests     |

If you remove the ITCH source, you lose the "this thing reads real
exchange feeds" claim. If you remove the synthetic source, you lose
the entire adversarial bench. Both are load-bearing.

---

## Why the synthetic feed lives in Go, not Rust

The synthetic feed is in [api/server/feed.go](../api/server/feed.go),
not in the deterministic Rust core. This is intentional and follows
the same rule as ITCH ingest: **non-deterministic sources never live
in the core**.

- The Rust engine reads bytes from a TCP frame stream
  ([engine/src/wire.rs](../engine/src/wire.rs)) and trusts the
  upstream to produce well-formed Events.
- The Go bridge owns wall-clock cadence, storm scheduling, recorder
  I/O, and reverse-proxy to the TARGET bot. Putting any of these in
  Rust would force the deterministic state machine to know about
  `time.Tick`, HTTP, and operator intent — exactly the contamination
  the source/computation split exists to prevent.

The price is one extra hop (Go → TCP → Rust). The gain is that the
Rust replay can read **the same Events** from a file or from the
synthetic Go bridge, with no code path change in the core. Replaying
a recorded `events.jsonl` from `data/runs/<id>/` produces the same
state as the live storm that generated it. This is the "replay-from
checkpoint ≡ replay-from-scratch" guarantee, extended to chaos runs.

---

## Storm parameter discipline

Each storm is parameterised by exactly four operator inputs:

| Field       | Type    | Range            | Purpose                              |
| ----------- | ------- | ---------------- | ------------------------------------ |
| `kind`      | enum    | one of 5         | which injector to engage             |
| `severity`  | f32     | `[0.0, 1.0]`     | intensity multiplier inside injector |
| `duration_ms` | u32   | `[5_000, 120_000]` | hard upper bound, no run-forever     |
| `seed`      | u64     | any              | makes the storm bit-exact replayable |

Severity 0 is a no-op (canary path). Severity 1 is the documented
worst-case for that kind (e.g. `FlashCrash 1.0` = full mid-slide +
maximum spread blowout). Out-of-range parameters are rejected at the
control plane (`POST /storm/start`) with `400` and never reach the
feed. This keeps `run.yaml` reproducible: the same five fields
recreate the same run on any host.

---

## What we did NOT do

We considered three alternatives during design and rejected each.

| Alternative                                       | Why rejected |
| ------------------------------------------------- | ------------ |
| Wire-level corruption of replayed ITCH packets    | Breaks parser invariants, makes the canonical hash meaningless, contaminates the "we can read real exchange data" claim with unrelated chaos logic |
| Post-parser injection of fake Events into Rust    | Forces adversarial logic into the deterministic core; replay would have to know which Events were "real" vs "injected" — defeats event sourcing |
| Two parallel `Event` ABIs (clean + corrupted)     | Doubles the FFI surface, doubles the cross-impl hash gate, no upside vs a single ABI fed by either source |

The chosen split — ITCH for parser truth, synthetic for chaos truth,
both speaking the same 40 B Event ABI — keeps every invariant in one
place: the canonical Event.

---

## Limits we do not hide

- The synthetic feed is **not a market simulator**. It does not model
  market impact, queue position, fill probability, or counterparty
  behaviour. It only reshapes the *observable* microstructure (depth,
  trade velocity, mid path, latency tail).
- ITCH replay is single-symbol per run by default. Multi-symbol
  replay works but is not part of the day-to-day stress harness.
- The synthetic feed targets `--tick-hz 50` for stable measurement;
  cranking it up changes the cadence-vs-cardinality tradeoff and
  invalidates comparison against any number recorded at 50 Hz.
- Neither source is a substitute for venue connectivity. Live trading
  is explicitly out of scope (see README "What is real, what is WIP").
