package feed

import (
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/Faraone-Dev/flowlab/ingest/mmap"
	"github.com/gorilla/websocket"
)

// ─── helpers ─────────────────────────────────────────────────────────

// newRing creates a fresh mmap ring buffer in the test's temp dir.
func newRing(tb testing.TB, size int) *mmap.RingBuffer {
	tb.Helper()
	path := filepath.Join(tb.TempDir(), "ring")
	r, err := mmap.NewRingBuffer(path, size)
	if err != nil {
		tb.Fatalf("ring: %v", err)
	}
	tb.Cleanup(func() { _ = r.Close() })
	return r
}

// echoServer spins up an httptest WS server. The handler runs `fn` once
// the upgrade succeeds; pass a function that pumps messages at the
// cadence and shape your test wants. The connection stays open until
// `fn` returns or the client disconnects.
func echoServer(tb testing.TB, fn func(*websocket.Conn)) (url string, stop func()) {
	tb.Helper()
	up := websocket.Upgrader{
		CheckOrigin: func(*http.Request) bool { return true },
	}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := up.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		defer c.Close()
		fn(c)
	}))
	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")
	return wsURL, srv.Close
}

// ─── correctness tests ───────────────────────────────────────────────

// TestStopIsIdempotentAndRaceFree exercises the exact pattern that
// used to data-race on f.conn: Stop() called concurrently with an
// active readLoop, multiple times, from multiple goroutines. Run with
// `go test -race` to get the verdict from the runtime checker.
func TestStopIsIdempotentAndRaceFree(t *testing.T) {
	url, srvStop := echoServer(t, func(c *websocket.Conn) {
		// Stream a tiny message every 100µs forever.
		for {
			if err := c.WriteMessage(websocket.BinaryMessage, []byte("x")); err != nil {
				return
			}
			time.Sleep(100 * time.Microsecond)
		}
	})
	defer srvStop()

	ring := newRing(t, 1<<20) // 1 MiB
	f := NewWebSocketFeed(url, ring)

	go f.Run()

	// Let the feed connect and pump a few messages.
	time.Sleep(50 * time.Millisecond)

	// Hammer Stop() from many goroutines at once. The first call wins
	// via stopOnce; the rest must be no-ops, never panic, never close
	// the same conn twice.
	const callers = 32
	var wg sync.WaitGroup
	wg.Add(callers)
	for i := 0; i < callers; i++ {
		go func() {
			defer wg.Done()
			f.Stop()
		}()
	}
	wg.Wait()

	// A second call after wg.Wait() must also be safe.
	f.Stop()
}

// TestStopUnblocksReadLoop verifies that Stop() unblocks a readLoop
// parked in ReadMessage(). If the conn-swap-on-Stop logic regresses,
// this test will hang for the test timeout instead of returning.
func TestStopUnblocksReadLoop(t *testing.T) {
	// Server that NEVER writes. The client is parked in ReadMessage()
	// until Stop() yanks the conn from under it.
	url, srvStop := echoServer(t, func(c *websocket.Conn) {
		// Block on the client's read so the upgrade stays alive.
		_, _, _ = c.ReadMessage()
	})
	defer srvStop()

	ring := newRing(t, 1<<16)
	f := NewWebSocketFeed(url, ring)

	done := make(chan struct{})
	go func() {
		f.Run()
		close(done)
	}()

	// Let the feed connect and park in ReadMessage.
	time.Sleep(50 * time.Millisecond)

	// Stop() must return promptly even though readLoop is blocked.
	stopReturned := make(chan struct{})
	go func() {
		f.Stop()
		close(stopReturned)
	}()

	select {
	case <-stopReturned:
	case <-time.After(2 * time.Second):
		t.Fatal("Stop() did not return within 2s — readLoop probably leaked")
	}

	select {
	case <-done:
	case <-time.After(1 * time.Second):
		t.Fatal("Run() did not return after Stop()")
	}
}

// TestDroppedCounterUnderBackpressure forces the ring to overflow and
// asserts the dropped counter rises monotonically rather than the
// readLoop sleeping (which would have stalled the kernel TCP buffer).
func TestDroppedCounterUnderBackpressure(t *testing.T) {
	const msgSize = 4 * 1024
	payload := make([]byte, msgSize)
	for i := range payload {
		payload[i] = byte(i)
	}

	url, srvStop := echoServer(t, func(c *websocket.Conn) {
		// Spam fixed-size frames as fast as possible.
		for i := 0; i < 2_000; i++ {
			if err := c.WriteMessage(websocket.BinaryMessage, payload); err != nil {
				return
			}
		}
		// Hold the conn open so the client's readLoop doesn't see EOF
		// before the test asserts.
		time.Sleep(500 * time.Millisecond)
	})
	defer srvStop()

	// Ring much smaller than the total bytes the server is going to send
	// → guaranteed overflow → dropped counter must move.
	ring := newRing(t, 64*1024)
	f := NewWebSocketFeed(url, ring)
	go f.Run()

	// Give the server time to finish its 2000 frames.
	time.Sleep(400 * time.Millisecond)
	f.Stop()

	got := f.Dropped()
	if got == 0 {
		t.Fatalf("expected backpressure drops but Dropped()==0; ring is %d B, server sent ~%d B",
			64*1024, 2_000*msgSize)
	}
	t.Logf("dropped=%d under backpressure (ring=64KiB, sent~%dKiB)",
		got, 2_000*msgSize/1024)
}

// TestRunReconnectsAfterRemoteClose verifies that closing the conn
// from the server side triggers a redial inside Run() instead of
// leaking the goroutine or terminating it.
func TestRunReconnectsAfterRemoteClose(t *testing.T) {
	var serverHits atomic.Uint64
	url, srvStop := echoServer(t, func(c *websocket.Conn) {
		serverHits.Add(1)
		// Send one byte then hang up. Forces the client into a
		// reconnect loop.
		_ = c.WriteMessage(websocket.BinaryMessage, []byte{0x42})
		_ = c.Close()
	})
	defer srvStop()

	ring := newRing(t, 1<<16)
	f := NewWebSocketFeed(url, ring)
	go f.Run()

	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if serverHits.Load() >= 3 {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	f.Stop()

	if hits := serverHits.Load(); hits < 3 {
		t.Fatalf("expected at least 3 server connects (reconnect loop) — got %d", hits)
	}
}

// ─── benchmark ───────────────────────────────────────────────────────

// BenchmarkFeedThroughput measures how many WS messages the feed can
// pull from the server and stuff into the ring per second. The
// server sends a fixed-size payload as fast as possible; the bench
// counts how many we drained in `b.N` iterations of a steady-state
// loop.
//
// Use:
//
//	go test -bench=. -benchmem ./feed/...
//	go test -bench=. -race ./feed/...   # paranoia run
func BenchmarkFeedThroughput(b *testing.B) {
	const msgSize = 256
	payload := make([]byte, msgSize)

	// Server fires payload to the client in a tight loop. b.N is set
	// per-iteration by the benchmark runner; we close after writing
	// b.N * 1.1 to make sure the client always has data to drain.
	target := uint64(b.N)
	if target < 1024 {
		target = 1024
	}

	var sent atomic.Uint64
	url, srvStop := echoServer(b, func(c *websocket.Conn) {
		for sent.Load() < target+target/10 {
			if err := c.WriteMessage(websocket.BinaryMessage, payload); err != nil {
				return
			}
			sent.Add(1)
		}
		// Linger so Stop() can race against an active conn realistically.
		time.Sleep(50 * time.Millisecond)
	})
	defer srvStop()

	// Generous ring so backpressure isn't the bottleneck of this
	// benchmark — we want to measure Run()/readLoop()/ring.Write
	// throughput, not drop accounting.
	ring := newRing(b, 64*1024*1024)
	f := NewWebSocketFeed(url, ring)

	b.ResetTimer()
	b.SetBytes(int64(msgSize))

	go f.Run()
	// Spin until we've drained b.N messages or 5s, whichever first.
	deadline := time.Now().Add(5 * time.Second)
	for {
		if sent.Load() >= target && f.Dropped() == 0 {
			// Sender's done and we kept up — exit hot loop.
			if ring != nil {
				break
			}
		}
		if time.Now().After(deadline) {
			break
		}
		time.Sleep(time.Millisecond)
	}
	f.Stop()

	b.StopTimer()
	b.ReportMetric(float64(sent.Load()), "msgs_sent")
	b.ReportMetric(float64(f.Dropped()), "msgs_dropped")
}
