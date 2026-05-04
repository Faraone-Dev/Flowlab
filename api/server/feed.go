// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

package server

import (
	"math"
	"math/rand"
	"sync"
	"time"
)

type Level struct {
	PriceTicks uint64 `json:"price_ticks"`
	Qty        uint64 `json:"qty"`
	OrderCount uint32 `json:"order_count"`
}

// StageLat is a pair (p50, p99) for one pipeline stage, nanoseconds.
type StageLat struct {
	P50Ns uint64 `json:"p50_ns"`
	P99Ns uint64 `json:"p99_ns"`
}

// TradePrint is one executed trade surfaced to the dashboard tape.
// Aggressor: 1 = buyer-initiated (hit ask), -1 = seller-initiated (hit bid),
// 0 = inside spread / undetermined.
type TradePrint struct {
	TsNs       uint64 `json:"ts_ns"`
	PriceTicks uint64 `json:"price_ticks"`
	Qty        uint64 `json:"qty"`
	Aggressor  int8   `json:"aggressor"`
}

// StageLatencies breaks down the engine pipeline. Zero values when the
// synthetic feed is driving (it has no real stages to measure).
type StageLatencies struct {
	Parse     StageLat `json:"parse"`
	Apply     StageLat `json:"apply"`
	Analytics StageLat `json:"analytics"`
	Risk      StageLat `json:"risk"`
	Wire      StageLat `json:"wire"`
}

// Tick is one snapshot of the synthetic market microstructure state.
// Mirrors the shape of a real flowlab telemetry packet: every field is a
// scalar that the dashboard plots as a line + deviation band.
//
// All prices are integer ticks (1 tick = 1/10000 USD) — same convention as
// the deterministic core. f64 fields are bounded ratios.
type Tick struct {
	TsNs            int64          `json:"ts_ns"`
	Seq             uint64         `json:"seq"`
	MidTicks        int64          `json:"mid_ticks"`
	SpreadTicks     int64          `json:"spread_ticks"`
	MicropriceTicks int64          `json:"microprice_ticks"`
	SpreadBps       float64        `json:"spread_bps"`
	BidDepth        uint64         `json:"bid_depth"`
	AskDepth        uint64         `json:"ask_depth"`
	Imbalance       float64        `json:"imbalance"`      // [-1, 1]
	Vpin            float64        `json:"vpin"`           // [0, 1]
	TradeVel        float64        `json:"trade_velocity"` // ratio vs mean
	Regime          uint8          `json:"regime"`         // 0..3
	LatP50Ns        uint64         `json:"lat_p50_ns"`
	LatP99Ns        uint64         `json:"lat_p99_ns"`
	LatP999Ns       uint64         `json:"lat_p999_ns"`
	LatMaxNs        uint64         `json:"lat_max_ns"`
	LatJitterNs     uint64         `json:"lat_jitter_ns"` // p99 - p50
	DroppedTotal    uint64         `json:"dropped_total"`
	UptimeMs        uint64         `json:"uptime_ms"`
	EventsPerSec    uint64         `json:"events_per_sec"`
	BreakerHalted   bool           `json:"breaker_halted"`
	BreakerReason   string         `json:"breaker_reason"`
	GapsLastMinute  uint32         `json:"gaps_last_minute"`
	StormActive     bool           `json:"storm_active"`
	StormKind       string         `json:"storm_kind,omitempty"`
	Bids            []Level        `json:"bids"`
	Asks            []Level        `json:"asks"`
	Stages          StageLatencies `json:"stages"`
	Symbol          string         `json:"symbol"`
	InstrumentID    uint32         `json:"instrument_id"`
	Trades          []TradePrint   `json:"trades,omitempty"`
}

// SyntheticFeed produces ticks with realistic correlation between metrics.
// The intent is NOT trading realism — it is dashboard realism: numbers that
// move together the way a real desk would see them.
type SyntheticFeed struct {
	mu          sync.Mutex
	rng         *rand.Rand
	seq         uint64
	mid         float64
	spread      float64
	vpin        float64
	tradeVel    float64
	bidDepth    float64
	askDepth    float64
	startedAt   time.Time
	stress      float64 // 0..1 — drives correlated spikes
	stressDecay float64
	halted      bool
	reason      string
	gaps        uint32
	storm       *StormController // nil if no control plane wired
}

func NewSyntheticFeed(seed int64) *SyntheticFeed {
	return &SyntheticFeed{
		rng:         rand.New(rand.NewSource(seed)),
		mid:         100_000_000, // 10000.0000 USD
		spread:      20,          // 0.0020 USD
		bidDepth:    50_000,
		askDepth:    50_000,
		startedAt:   time.Now(),
		stressDecay: 0.985,
	}
}

// Next advances the simulation by one step and returns the new tick.
func (f *SyntheticFeed) Next() Tick {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.seq++

	// ── stress regime: 1% chance per tick to inject a burst of stress.
	if f.rng.Float64() < 0.01 {
		f.stress = math.Min(1.0, f.stress+0.4+f.rng.Float64()*0.3)
	}
	f.stress *= f.stressDecay
	// ── storm injection: force stress floor from active StormController.
	var stormSeverity float64
	var stormKind string
	if f.storm != nil {
		stormSeverity = f.storm.ActiveSeverity()
		stormKind = f.storm.ActiveKind()
		if stormSeverity > 0 {
			floor := stormSeverity*0.7 + f.rng.Float64()*stormSeverity*0.3
			if f.stress < floor {
				f.stress = floor
			}
		}
	}

	// ── mid: random walk with drift proportional to imbalance.
	imb := (f.bidDepth - f.askDepth) / (f.bidDepth + f.askDepth)
	drift := imb * 0.3
	noise := f.rng.NormFloat64() * (1.0 + f.stress*8.0)
	f.mid += drift + noise
	if f.mid < 1 {
		f.mid = 1
	}

	// ── spread: baseline 20 ticks, blows out under stress.
	baseSpread := 20.0
	f.spread = baseSpread*(1.0+f.stress*4.0) + math.Abs(f.rng.NormFloat64())*2.0

	// ── depths: drift around 50k, asymmetric under stress.
	f.bidDepth = clampPositive(f.bidDepth + f.rng.NormFloat64()*500 - f.stress*1500*f.rng.Float64())
	f.askDepth = clampPositive(f.askDepth + f.rng.NormFloat64()*500 - f.stress*1500*f.rng.Float64())

	// ── VPIN: ramps with stress; mean-reverts slowly.
	target := 0.15 + f.stress*0.6
	f.vpin += (target-f.vpin)*0.05 + f.rng.NormFloat64()*0.005
	f.vpin = clamp01(f.vpin)

	// ── trade velocity ratio: 1.0 baseline; stress doubles it.
	f.tradeVel = 1.0 + f.stress*1.8 + f.rng.NormFloat64()*0.05

	// ── latency p50/p99: hot path stays tight unless stress drives queueing.
	p50 := 30.0 + f.stress*20.0 + math.Abs(f.rng.NormFloat64())*4.0
	p99 := 90.0 + f.stress*180.0 + math.Abs(f.rng.NormFloat64())*15.0

	// ── events/sec: realistic 50k-200k.
	eps := 80_000.0 + f.stress*120_000.0 + f.rng.NormFloat64()*4_000.0
	if eps < 1000 {
		eps = 1000
	}

	// ── kind-specific overrides: each storm leaves a distinct fingerprint
	//    on the chart that matches the chaos crate's generator semantics.
	//    Magnitudes are scaled to be VISIBLE on the dashboard given the
	//    current baselines (depth ~1e9, mid ~1e8 ticks, p99 ~600ns).
	if stormSeverity > 0 {
		switch stormKind {
		case "PhantomLiquidity":
			// Depth oscillates ±40% of current size at ~1Hz cycle.
			cycle := math.Sin(float64(f.seq) * 0.08)
			oscBid := cycle * stormSeverity * 0.4 * f.bidDepth
			oscAsk := cycle * stormSeverity * 0.4 * f.askDepth
			f.bidDepth = clampPositive(f.bidDepth + oscBid)
			f.askDepth = clampPositive(f.askDepth - oscAsk)
		case "CancellationStorm":
			// Cancel-to-trade spike: events/sec doubles+, trade velocity collapses.
			eps *= 1.0 + stormSeverity*3.0
			f.tradeVel = math.Max(0.05, f.tradeVel-stormSeverity*0.85)
			f.vpin = clamp01(f.vpin + stormSeverity*0.45)
		case "MomentumIgnition":
			// Strong directional drift, same sign as current imbalance.
			// 200 ticks/step at sev=1 → ~4000 ticks (0.40 USD = 40bps) over 20 steps.
			dir := 1.0
			if imb < 0 {
				dir = -1.0
			}
			f.mid += dir * stormSeverity * 200.0
			f.spread *= 1.0 + stormSeverity*1.5
		case "FlashCrash":
			// Linear downward slide: sev=1 → 400 ticks/step, ~80bps in 20 ticks.
			f.mid -= stormSeverity * 400.0
			f.spread *= 1.0 + stormSeverity*4.0
		case "LatencyArbProxy":
			// p99 tail explodes to 5-50µs range; p50 stays cold.
			p99 += stormSeverity * 50_000
		}
	}

	// ── regime classification — MUST stay in lockstep with the Rust
	//    canonical classifier in `flow/src/regime.rs`. The synthetic
	//    feed has no mid-drift / crossed-book / depth-depletion
	//    inputs, so they are passed as zeros (the Rust classifier
	//    handles those defaults identically). Parity is locked in
	//    by `regime_parity_test.go`; if that test fails, fix THIS
	//    function — never the test vectors — to match Rust.
	regime := classifyRegime(regimeInput{
		spreadBlowoutRatio: f.spread / baseSpread,
		bookImbalance:      imb,
		vpin:               f.vpin,
		tradeVelocityRatio: f.tradeVel,
		eventsPerSec:       uint64(eps),
	})

	// ── gaps: very rare spike under stress.
	if f.rng.Float64() < f.stress*0.02 {
		f.gaps++
	}

	// ── breaker: trip on extreme regime + gap pressure.
	if !f.halted && (regime == 3 && f.gaps > 8) {
		f.halted = true
		f.reason = "FeedGap"
	}

	// ── synthesize top-10 book levels around the current mid.
	//    Each level steps by spread/2 ticks; sizes decay outward and are
	//    proportional to the side's total depth. Phantom storm makes the
	//    far levels balloon (visible in the ladder).
	halfSpread := f.spread * 0.5
	if halfSpread < 1 {
		halfSpread = 1
	}
	bidPerLevel := f.bidDepth / 10.0
	askPerLevel := f.askDepth / 10.0
	bids := make([]Level, 10)
	asks := make([]Level, 10)
	for i := 0; i < 10; i++ {
		// price step: 1 tick from inside, then widening by halfSpread per level.
		offset := halfSpread + float64(i)*halfSpread*0.5
		// size decay: top of book heaviest; geometric falloff.
		decay := math.Pow(0.85, float64(i))
		bidQty := bidPerLevel * decay * (0.6 + 0.8*f.rng.Float64())
		askQty := askPerLevel * decay * (0.6 + 0.8*f.rng.Float64())
		bids[i] = Level{
			PriceTicks: uint64(f.mid - offset),
			Qty:        uint64(bidQty),
			OrderCount: uint32(3 + i),
		}
		asks[i] = Level{
			PriceTicks: uint64(f.mid + offset),
			Qty:        uint64(askQty),
			OrderCount: uint32(3 + i),
		}
	}

	// ── synthesize trade prints for the tape.
	//    Per-tick trade probability scales with tradeVel (baseline ~1.0).
	//    Up to 6 prints per tick under stress; aggressor side biased by imbalance.
	var trades []TradePrint
	tradeP := math.Min(0.95, f.tradeVel*0.6)
	for i := 0; i < 6 && f.rng.Float64() < tradeP; i++ {
		var aggr int8
		var px float64
		// Bias: positive imbalance → buyer-aggressive (hits ask).
		if f.rng.Float64() < 0.5+imb*0.5 {
			aggr = 1
			px = f.mid + halfSpread
		} else {
			aggr = -1
			px = f.mid - halfSpread
		}
		// Lot size: most prints small, occasional larger.
		qty := uint64(50 + f.rng.Intn(950))
		if f.rng.Float64() < 0.05 {
			qty *= 10
		}
		trades = append(trades, TradePrint{
			TsNs:       uint64(time.Now().UnixNano()),
			PriceTicks: uint64(px),
			Qty:        qty,
			Aggressor:  aggr,
		})
	}

	return Tick{
		TsNs:           time.Now().UnixNano(),
		Seq:            f.seq,
		MidTicks:       int64(f.mid),
		SpreadTicks:    int64(f.spread),
		BidDepth:       uint64(f.bidDepth),
		AskDepth:       uint64(f.askDepth),
		Imbalance:      imb,
		Vpin:           f.vpin,
		TradeVel:       f.tradeVel,
		Regime:         regime,
		LatP50Ns:       uint64(p50),
		LatP99Ns:       uint64(p99),
		EventsPerSec:   uint64(eps),
		BreakerHalted:  f.halted,
		BreakerReason:  f.reason,
		GapsLastMinute: f.gaps,
		StormActive:    stormSeverity > 0,
		StormKind:      stormKind,
		Bids:           bids,
		Asks:           asks,
		Trades:         trades,
		Stages: StageLatencies{
			// Per-stage budget (rough split): parse 15%, apply 25%, analytics 30%, risk 10%, wire 20%.
			Parse:     StageLat{P50Ns: uint64(p50 * 0.15), P99Ns: uint64(p99 * 0.15)},
			Apply:     StageLat{P50Ns: uint64(p50 * 0.25), P99Ns: uint64(p99 * 0.25)},
			Analytics: StageLat{P50Ns: uint64(p50 * 0.30), P99Ns: uint64(p99 * 0.30)},
			Risk:      StageLat{P50Ns: uint64(p50 * 0.10), P99Ns: uint64(p99 * 0.10)},
			Wire:      StageLat{P50Ns: uint64(p50 * 0.20), P99Ns: uint64(p99 * 0.20)},
		},
	}
}

// Reset clears breaker latch + gap counter (operator action).
func (f *SyntheticFeed) Reset() {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.halted = false
	f.reason = ""
	f.gaps = 0
	f.stress = 0
}

// Symbol identifies the synthetic ticker. Purely cosmetic — it lets the
// dashboard label the header the same way it does for a real feed.
func (f *SyntheticFeed) Symbol() (string, uint32) { return "SYNTH", 0 }

// RecentTrades is a no-op for the synthetic feed — no trade prints.
func (f *SyntheticFeed) RecentTrades() []TradePrint { return nil }

func clamp01(x float64) float64 {
	if x < 0 {
		return 0
	}
	if x > 1 {
		return 1
	}
	return x
}

func clampPositive(x float64) float64 {
	if x < 1000 {
		return 1000
	}
	return x
}
