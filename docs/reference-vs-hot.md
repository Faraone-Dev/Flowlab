# Three order books, one number — how Flowlab knows the fast one is right

> Status: **frozen 2026-05-06**
> Code: [bench/src/lib.rs](../bench/src/lib.rs) · test
> `cross_impl_l2_hash_agreement`
> Books: [core/src/orderbook.rs](../core/src/orderbook.rs) (slow) ·
> [core/src/hot_book.rs](../core/src/hot_book.rs) (fast) ·
> [core/src/ffi.rs](../core/src/ffi.rs) (C++ via FFI)

---

## The short version

Flowlab carries **three order books**. They look different inside, but
they must produce the **same number** when fed the same events. CI
runs the comparison on every push. If the numbers diverge by one bit,
the build fails.

| #  | Name                | Language | What it looks like inside        | Why it exists                |
| -- | ------------------- | -------- | -------------------------------- | ---------------------------- |
| 1  | `OrderBook`         | Rust     | sorted map (`BTreeMap`)          | the slow, obviously-correct one |
| 2  | `HotOrderBook<256>` | Rust     | fixed arrays + slab + cache tricks | the fast one used in production |
| 3  | `CppOrderBook`      | C++20    | template version of #2           | the cross-language check     |

The three implementations never talk to each other. They each take the
same events, build their own state, and at the end they each spit out
one number — a hash of the book. **All three numbers must be equal.**

That's it. That's the whole idea.

---

## Why even bother

Imagine you write a fast order book. Arrays, no allocations, prefetch,
cache-line tricks, the whole thing. It runs in 30 ns per trade.

Now answer: how do you know it's right?

You can write 200 unit tests. They will catch the bugs you thought
about. They will not catch the ones you didn't. The whole reason you
went fast is that you skipped the boring code — and the boring code is
where correctness comes for free.

The trick used in real HFT shops: **keep the boring version alive
forever.** Use it as a judge. The fast one is right if and only if it
agrees with the boring one on every input.

That's what `OrderBook` (the `BTreeMap` one) does in this repo. It is
not legacy. It is not "the old version". It is the **judge**.

---

## What the fast one does that's risky

[core/src/hot_book.rs](../core/src/hot_book.rs) makes choices that are
fast but easy to get wrong:

- bids and asks are **fixed arrays** of 256 levels — no `Vec`, no
  heap allocation per event
- the struct is **64-byte aligned** with a deliberate **64-byte gap**
  between bids and asks, so the CPU never accidentally loads bid data
  while reading asks
- orders are stored in a **slab** (one big array, reused via free
  list) with a small `HashMap` pointing into it — 12 bytes per entry
  instead of 32, so 5× more order lookups fit in one cache line
- it tells the CPU to **prefetch** the next event one step ahead
- on a trade, the rare case (full fill) is moved out of the hot path
  with `#[cold]`, leaving the common case as a straight line

Every one of these can silently produce the wrong state. You wouldn't
know until a downstream check broke at 3 AM.

The slow `BTreeMap` book has none of these tricks. It walks a sorted
map and sums quantities. **It cannot lose a level by forgetting to
shift an array.** The structure does the work.

---

## What the comparison looks like

One test, in [bench/src/lib.rs](../bench/src/lib.rs):

```rust
let raw    = synthetic_itch_stream(50_000, 0xC0FFEE);
let parsed = parse(raw);

let mut hot = HotOrderBook::<256>::new(1);   // fast
let mut bt  = OrderBook::new(1);              // slow judge
let mut cpp = CppOrderBook::new();            // C++ check

for ev in &parsed {
    hot.apply(ev);
    bt.apply(ev);
    cpp.apply(ev);
}

assert_eq!(hot.canonical_l2_hash(), bt.canonical_l2_hash());
assert_eq!(hot.canonical_l2_hash(), cpp.state_hash());
```

Same input. Three books. One number each. They must all match.

The expected number is `0xf54ce1b763823e87`. It's pinned. If the next
PR touches the hot path and that number changes, CI catches it before
merge.

---

## Why three books, not two

Two books would already be the standard "slow judge vs fast worker"
trick. The third (C++) adds a separate check.

| What disagrees                | What that tells you                            |
| ----------------------------- | ---------------------------------------------- |
| fast vs slow (both Rust)      | bug in the fast Rust path                      |
| Rust pair vs C++              | bug in the C++ kernel, or in the FFI layout    |
| slow vs C++ (rare)            | bug in the canonical hash protocol itself     |

Two witnesses can disagree without telling you which one lies. Three
witnesses **triangulate**: two-against-one is a real signal.

The C++ check also forces the `Event` layout (40 bytes, fixed) to stay
identical between Rust and C++. If anyone shifts a field by accident,
the hashes diverge before the next push.

---

## What the comparison checks — and what it doesn't

The comparison is on the **L2 hash**. That word matters, so here's the
plain version.

**L1** = top of book. One bid, one ask. The two best prices.

**L2** = full price ladder, aggregated. *"At price 100 there are 1500
shares total across 3 orders."* You see the totals per price level, not
who's behind them.

**L3** = every single order. *"At price 100: order #7831 = 500 shares,
order #9012 = 700 shares, order #9415 = 300 shares."* You see the
individual orders.

Flowlab tracks **L3 in the data**: every order has its own ID, slot,
price, qty, side. Cancels and trades resolve by order ID. Both books
do this. So L3 is implemented.

But the **comparison hash is L2**. Three reasons:

1. **Trading decisions look at L2, not L3.**
   A market-making strategy decides on *"how much is at the best
   bid?"*. It doesn't care which trader is behind that number. If the
   three books agree on the L2 totals, then any strategy running on
   top makes the **same decisions** regardless of which book it uses.
   That is the property worth protecting.

2. **L3 can disagree without anyone being wrong.**
   The Rust `HashMap` iterates orders in one order. A different hash
   table — or a future replacement — iterates them in another. Same
   orders, same totals, different iteration order. An L3 hash would
   flag this as a bug. It isn't. It's just a layout detail.

   Forcing L3 hashes to match would mean **freezing** every
   implementation to use the same hash table with the same seed. You
   lose the freedom to swap structures for zero gain in real
   correctness.

3. **The market itself works this way.**
   NASDAQ sends L3 data — every order is visible. But when two
   implementations need to agree on *"what does the book look like"*,
   the industry compares at L2. L3 ordering is a fact about your
   local code, not a fact about the market.

So the gate is at the right level. Order-by-order behaviour is checked
by separate tests like
[tests/e2e/cancel_heavy.rs](../tests/e2e/cancel_heavy.rs) and
[tests/chaos/corruption_injection.rs](../tests/chaos/corruption_injection.rs),
which catch L3 bugs through behaviour (does the right qty get
cancelled?) rather than through hash equality.

---

## What this is not

- **Not a performance gate.** The slow book is allowed to be 100×
  slower. Speed is checked elsewhere, in
  [docs/latency-alpha.md](latency-alpha.md) and
  [docs/latency-cross-hw.md](latency-cross-hw.md).
- **Not exhaustive.** 50k events at one seed catches structural
  bugs, not edge cases that hit one in a billion. The fuzz harness
  ([feed-parser/src/fuzz.zig](../feed-parser/src/fuzz.zig)) covers
  parser-level random input separately.
- **Not free for pure-Rust builds.** The C++ side needs
  `--features native` and a C++ toolchain. Without it, only the
  Rust pair (fast vs slow) runs. The full three-way check requires
  the full toolchain.

---

## Why this matters when talking about it

When someone asks *"how do you know your hot path is correct?"*, the
answer is one sentence:

> Three independent books — a slow `BTreeMap` one as the judge, a
> fast array-and-slab one for production, and a C++ template version
> for cross-language check — get fed the same events and must produce
> the same hash. Gated in CI, fail-fast.

That's the whole story. The value is not the order book. The value is
the comparison around it.
