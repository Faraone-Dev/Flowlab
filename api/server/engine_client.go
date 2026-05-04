// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

package server

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"sync"
	"sync/atomic"
	"time"
)

const (
	minSupportedWireVersion uint16 = 1
	maxSupportedWireVersion uint16 = 2
)

// Feed is the surface the WS handler depends on. Both SyntheticFeed and
// EngineClient satisfy it, so the server can be flipped between modes
// with a single CLI flag and the dashboard contract stays untouched.
type Feed interface {
	Next() Tick
	Reset()
	Symbol() (name string, instrumentID uint32)
	RecentTrades() []TradePrint
}

// ── Engine wire types ──────────────────────────────────────────────────
//
// One JSON object per frame, externally tagged like Rust's serde default:
//
//   { "Tick":   {...} }
//   { "Header": {...} }
//   { "Risk":   {...} }
//   { "Lat":    {...} }
//   { "Book":   {...} }
//   "Heartbeat"
//
// We only fully decode what the dashboard renders today: Tick. The rest
// is parsed loosely so we can surface counters without coupling Go to
// every Rust schema bump.

type engineHeader struct {
	Source       string  `json:"source"`
	IsLive       bool    `json:"is_live"`
	StartedAtNs  uint64  `json:"started_at_ns"`
	Symbol       *string `json:"symbol,omitempty"`
	InstrumentID *uint32 `json:"instrument_id,omitempty"`
}

type engineTick struct {
	Seq             uint64  `json:"seq"`
	EventTimeNs     uint64  `json:"event_time_ns"`
	ProcessTimeNs   uint64  `json:"process_time_ns"`
	MidTicks        int64   `json:"mid_ticks"`
	SpreadTicks     int64   `json:"spread_ticks"`
	MicropriceTicks int64   `json:"microprice_ticks"`
	SpreadBps       float64 `json:"spread_bps"`
	BidDepth        uint64  `json:"bid_depth"`
	AskDepth        uint64  `json:"ask_depth"`
	Imbalance       float64 `json:"imbalance"`
	Vpin            float64 `json:"vpin"`
	TradeVelocity   float64 `json:"trade_velocity"`
	Regime          uint8   `json:"regime"`
	EventsPerSec    uint64  `json:"events_per_sec"`
	DroppedTotal    uint64  `json:"dropped_total"`
}

type engineStageLat struct {
	P50Ns  uint64 `json:"p50_ns"`
	P99Ns  uint64 `json:"p99_ns"`
	P999Ns uint64 `json:"p999_ns"`
	MaxNs  uint64 `json:"max_ns"`
}

type engineLat struct {
	Parse     engineStageLat `json:"parse"`
	Apply     engineStageLat `json:"apply"`
	Analytics engineStageLat `json:"analytics"`
	Risk      engineStageLat `json:"risk"`
	WireOut   engineStageLat `json:"wire_out"`
}

type engineTrade struct {
	Seq        uint64 `json:"seq"`
	TsNs       uint64 `json:"ts_ns"`
	PriceTicks uint64 `json:"price_ticks"`
	Qty        uint64 `json:"qty"`
	Aggressor  int8   `json:"aggressor"`
}

type engineRisk struct {
	Halted       bool    `json:"halted"`
	Reason       *string `json:"reason"`
	GapsInWindow uint32  `json:"gaps_in_window"`
}

type engineLevel struct {
	PriceTicks uint64 `json:"price_ticks"`
	Qty        uint64 `json:"qty"`
	OrderCount uint32 `json:"order_count"`
}

type engineBook struct {
	Seq  uint64        `json:"seq"`
	Bids []engineLevel `json:"bids"`
	Asks []engineLevel `json:"asks"`
}

// EngineClient dials a flowlab-engine telemetry socket and continuously
// decodes frames into the latest published Tick. It survives the engine
// process restarting: after a disconnect we sleep with backoff and redial.
//
// Concurrency:
//   - One reader goroutine owns the TCP socket.
//   - Last decoded Tick is published via a single atomic pointer load/store.
//   - The WS handler calls Next() at its own cadence (50 Hz). If the engine
//     is faster, intermediate ticks are coalesced (latest wins). If the
//     engine is slower, the WS handler resends the last value — same as
//     a real desk seeing a stale book.
type EngineClient struct {
	addr string

	latest atomic.Pointer[Tick]
	book   atomic.Pointer[[2][]Level] // [0]=bids desc, [1]=asks asc

	mu          sync.Mutex
	header      engineHeader
	lastApplyNs uint64
	stages      StageLatencies
	gaps        uint32
	halted      bool
	reason      string
	dropped     uint64
	symbol      string
	instrument  uint32

	// Trade tape ring (last N prints).
	tradesMu sync.Mutex
	trades   []TradePrint // append-only cap tradeRingCap
	tradeSeq uint64

	startedAt time.Time
}

const tradeRingCap = 128

// NewEngineClient returns an unstarted client. Call Start() to begin
// reading frames in the background.
func NewEngineClient(addr string) *EngineClient {
	c := &EngineClient{addr: addr, startedAt: time.Now()}
	// Seed with an empty tick so Next() never returns a zero-value before
	// the first frame arrives — the dashboard can render "waiting for
	// engine" gracefully without a NPE.
	t := Tick{TsNs: time.Now().UnixNano(), BreakerReason: "waiting for engine"}
	c.latest.Store(&t)
	return c
}

// Start spawns the reader loop. Returns immediately.
func (c *EngineClient) Start() {
	go c.runForever()
}

// Next returns the most recently decoded tick. Never blocks.
func (c *EngineClient) Next() Tick {
	t := c.latest.Load()
	// Refresh timestamp so the dashboard's local time axis stays smooth
	// even when the engine is briefly stalled (last value resent).
	out := *t
	out.TsNs = time.Now().UnixNano()
	if bk := c.book.Load(); bk != nil {
		out.Bids = bk[0]
		out.Asks = bk[1]
	}
	c.mu.Lock()
	out.Stages = c.stages
	c.mu.Unlock()
	return out
}

// Symbol returns (name, instrument_id) from the last Header seen.
func (c *EngineClient) Symbol() (string, uint32) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.symbol, c.instrument
}

// RecentTrades returns a copy of the last N trade prints, oldest first.
func (c *EngineClient) RecentTrades() []TradePrint {
	c.tradesMu.Lock()
	defer c.tradesMu.Unlock()
	if len(c.trades) == 0 {
		return nil
	}
	out := make([]TradePrint, len(c.trades))
	copy(out, c.trades)
	return out
}

// Reset is a no-op when the breaker lives in the engine. Wired here so the
// HTTP /reset handler is uniform across feed implementations. To actually
// reset the Rust-side breaker we will add a control channel later.
func (c *EngineClient) Reset() {
	c.mu.Lock()
	c.halted = false
	c.reason = ""
	c.gaps = 0
	c.mu.Unlock()
}

func (c *EngineClient) runForever() {
	backoff := 100 * time.Millisecond
	const maxBackoff = 3 * time.Second
	for {
		if err := c.runOnce(); err != nil {
			log.Printf("engine client: %v (reconnecting in %s)", err, backoff)
		}
		time.Sleep(backoff)
		backoff *= 2
		if backoff > maxBackoff {
			backoff = maxBackoff
		}
	}
}

func (c *EngineClient) runOnce() error {
	conn, err := net.DialTimeout("tcp", c.addr, 2*time.Second)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer conn.Close()
	if tcp, ok := conn.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}
	log.Printf("engine client: connected to %s", c.addr)

	r := conn
	hdr := make([]byte, 4)

	for {
		// length prefix (u32 LE) — bytes that follow, INCLUDING the 2-byte version
		if _, err := io.ReadFull(r, hdr); err != nil {
			return fmt.Errorf("read len: %w", err)
		}
		n := binary.LittleEndian.Uint32(hdr)
		if n < 2 || n > 1<<20 {
			return fmt.Errorf("frame size out of range: %d", n)
		}
		buf := make([]byte, n)
		if _, err := io.ReadFull(r, buf); err != nil {
			return fmt.Errorf("read payload: %w", err)
		}
		// version (u16 LE)
		ver := binary.LittleEndian.Uint16(buf[:2])
		if !isSupportedWireVersion(ver) {
			return fmt.Errorf(
				"unsupported wire version: %d (supported %d..%d)",
				ver,
				minSupportedWireVersion,
				maxSupportedWireVersion,
			)
		}
		c.dispatchJSON(buf[2:])
	}
}

func isSupportedWireVersion(ver uint16) bool {
	return ver >= minSupportedWireVersion && ver <= maxSupportedWireVersion
}

// dispatchJSON peeks at the externally-tagged variant name and decodes
// the payload only for variants the dashboard cares about.
func (c *EngineClient) dispatchJSON(payload []byte) {
	// Heartbeat is the bare string "Heartbeat".
	if len(payload) > 0 && payload[0] == '"' {
		return
	}
	var raw map[string]json.RawMessage
	if err := json.Unmarshal(payload, &raw); err != nil {
		return
	}
	for k, v := range raw {
		switch k {
		case "Header":
			var h engineHeader
			if json.Unmarshal(v, &h) == nil {
				c.mu.Lock()
				c.header = h
				if h.Symbol != nil {
					c.symbol = *h.Symbol
				}
				if h.InstrumentID != nil {
					c.instrument = *h.InstrumentID
				}
				c.mu.Unlock()
			}
		case "Tick":
			var et engineTick
			if json.Unmarshal(v, &et) != nil {
				continue
			}
			c.mu.Lock()
			c.dropped = et.DroppedTotal
			gaps := c.gaps
			halted := c.halted
			reason := c.reason
			lat := c.lastApplyNs
			c.mu.Unlock()

			t := Tick{
				TsNs:            time.Now().UnixNano(),
				Seq:             et.Seq,
				MidTicks:        et.MidTicks,
				SpreadTicks:     et.SpreadTicks,
				MicropriceTicks: et.MicropriceTicks,
				SpreadBps:       et.SpreadBps,
				BidDepth:        et.BidDepth,
				AskDepth:        et.AskDepth,
				Imbalance:       et.Imbalance,
				Vpin:            et.Vpin,
				TradeVel:        et.TradeVelocity,
				Regime:          et.Regime,
				LatP50Ns:        lat, // placeholder until Lat decoded
				LatP99Ns:        lat, // ditto
				DroppedTotal:    et.DroppedTotal,
				EventsPerSec:    et.EventsPerSec,
				BreakerHalted:   halted,
				BreakerReason:   reason,
				GapsLastMinute:  gaps,
			}
			c.latest.Store(&t)
		case "Lat":
			var el engineLat
			if json.Unmarshal(v, &el) == nil {
				stages := StageLatencies{
					Parse:     StageLat{P50Ns: el.Parse.P50Ns, P99Ns: el.Parse.P99Ns},
					Apply:     StageLat{P50Ns: el.Apply.P50Ns, P99Ns: el.Apply.P99Ns},
					Analytics: StageLat{P50Ns: el.Analytics.P50Ns, P99Ns: el.Analytics.P99Ns},
					Risk:      StageLat{P50Ns: el.Risk.P50Ns, P99Ns: el.Risk.P99Ns},
					Wire:      StageLat{P50Ns: el.WireOut.P50Ns, P99Ns: el.WireOut.P99Ns},
				}
				c.mu.Lock()
				c.stages = stages
				c.lastApplyNs = el.Apply.P99Ns
				c.mu.Unlock()
				// also patch the latest tick so the next Next() carries it
				if last := c.latest.Load(); last != nil {
					patched := *last
					patched.LatP50Ns = el.Apply.P50Ns
					patched.LatP99Ns = el.Apply.P99Ns
					patched.LatP999Ns = el.Apply.P999Ns
					patched.LatMaxNs = el.Apply.MaxNs
					if el.Apply.P99Ns > el.Apply.P50Ns {
						patched.LatJitterNs = el.Apply.P99Ns - el.Apply.P50Ns
					}
					patched.Stages = stages
					c.latest.Store(&patched)
				}
			}
		case "Risk":
			var er engineRisk
			if json.Unmarshal(v, &er) == nil {
				c.mu.Lock()
				c.gaps = er.GapsInWindow
				c.halted = er.Halted
				if er.Reason != nil {
					c.reason = *er.Reason
				} else {
					c.reason = ""
				}
				c.mu.Unlock()
			}
		case "Book":
			var eb engineBook
			if json.Unmarshal(v, &eb) != nil {
				continue
			}
			bids := make([]Level, len(eb.Bids))
			for i, l := range eb.Bids {
				bids[i] = Level{PriceTicks: l.PriceTicks, Qty: l.Qty, OrderCount: l.OrderCount}
			}
			asks := make([]Level, len(eb.Asks))
			for i, l := range eb.Asks {
				asks[i] = Level{PriceTicks: l.PriceTicks, Qty: l.Qty, OrderCount: l.OrderCount}
			}
			pair := [2][]Level{bids, asks}
			c.book.Store(&pair)
		case "Trade":
			var et engineTrade
			if json.Unmarshal(v, &et) != nil {
				continue
			}
			tp := TradePrint{
				TsNs:       et.TsNs,
				PriceTicks: et.PriceTicks,
				Qty:        et.Qty,
				Aggressor:  et.Aggressor,
			}
			c.tradesMu.Lock()
			if len(c.trades) >= tradeRingCap {
				// shift out the oldest entry
				c.trades = append(c.trades[:0], c.trades[1:]...)
			}
			c.trades = append(c.trades, tp)
			c.tradesMu.Unlock()
		}
	}
}
