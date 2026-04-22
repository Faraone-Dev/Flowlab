package feed

import (
	"log"
	"sync"
	"sync/atomic"
	"time"

	"github.com/Faraone-Dev/flowlab/ingest/mmap"
	"github.com/gorilla/websocket"
)

// WebSocketFeed connects to an exchange feed and writes raw data
// to the mmap ring buffer for zero-copy processing by Zig/Rust.
//
// Concurrency model:
//
//   - One producer goroutine runs `Run()`, which loops over
//     connect/readLoop until `Stop()` is called.
//   - `Stop()` is invoked from any other goroutine (typically the
//     signal handler in `cmd/ingest/main.go`).
//
// Synchronisation primitives:
//
//   - `stop` chan: closed once by `Stop()` to signal cancellation;
//     all internal selects observe it.
//   - `conn`: `atomic.Pointer[websocket.Conn]` so the read goroutine
//     can swap in a new connection on each (re)connect while `Stop()`
//     races to close the current one. The pointer write (publication
//     of a new conn) and the load+close in `Stop()` are atomic, so
//     `Stop()` never reads a torn pointer and never double-closes a
//     stale conn.
//   - `wg`: lets `Stop()` wait for `Run()` to actually return before
//     declaring the feed shut down.
type WebSocketFeed struct {
	url string

	ring *mmap.RingBuffer
	conn atomic.Pointer[websocket.Conn]

	stop     chan struct{}
	stopOnce sync.Once
	wg       sync.WaitGroup

	// Backpressure counter — bumped every time the ring buffer is full
	// and we have to drop the incoming WS message instead of blocking
	// the read side. Mirrors the drop-with-counter pattern used by
	// `flowlab-engine/backpressure`.
	dropped atomic.Uint64
}

func NewWebSocketFeed(url string, ring *mmap.RingBuffer) *WebSocketFeed {
	return &WebSocketFeed{
		url:  url,
		ring: ring,
		stop: make(chan struct{}),
	}
}

// Dropped returns the running count of WS messages dropped because
// the ring buffer was full at the time of arrival.
func (f *WebSocketFeed) Dropped() uint64 {
	return f.dropped.Load()
}

// Run blocks until Stop() is called. Intended to be launched as
// `go feed.Run()` by the caller; the WaitGroup add/done lives here so
// Stop() can wait for the loop to fully unwind.
func (f *WebSocketFeed) Run() {
	f.wg.Add(1)
	defer f.wg.Done()

	for {
		select {
		case <-f.stop:
			return
		default:
		}

		if err := f.connect(); err != nil {
			log.Printf("connection failed: %v, retrying in 1s", err)
			// Cancellable sleep: don't block Stop() for a full second.
			select {
			case <-f.stop:
				return
			case <-time.After(time.Second):
			}
			continue
		}

		f.readLoop()
	}
}

func (f *WebSocketFeed) connect() error {
	conn, _, err := websocket.DefaultDialer.Dial(f.url, nil)
	if err != nil {
		return err
	}
	// Publish atomically. If Stop() raced ahead and already closed an
	// older conn, this newer one will be closed when readLoop exits.
	f.conn.Store(conn)
	log.Printf("connected to %s", f.url)
	return nil
}

func (f *WebSocketFeed) readLoop() {
	// Snapshot the conn for the duration of this readLoop. The atomic
	// pointer may be cleared by Stop() at any time, but the local
	// reference keeps the conn alive for our own Close() below.
	conn := f.conn.Load()
	if conn == nil {
		return
	}
	defer func() {
		// Clear the atomic pointer first so a concurrent Stop() does
		// not race us on Close(). CompareAndSwap leaves a newer conn
		// (published by a future connect()) untouched.
		f.conn.CompareAndSwap(conn, nil)
		_ = conn.Close()
	}()

	for {
		select {
		case <-f.stop:
			return
		default:
		}

		_, message, err := conn.ReadMessage()
		if err != nil {
			// Either Stop() closed the conn under us, or the peer hung
			// up. Either way we exit and let Run() decide whether to
			// reconnect (it won't if stop is closed).
			log.Printf("read error: %v", err)
			return
		}

		// Backpressure: if the ring is full we drop and bump a counter
		// rather than sleeping. Sleeping inside the read side stalls
		// the kernel TCP buffer and ends up dropping further upstream
		// (with no visibility). Drop-with-counter keeps the loss
		// observable.
		if err := f.ring.Write(message); err != nil {
			f.dropped.Add(1)
		}
	}
}

// Stop signals Run() to exit and waits for it. Idempotent: safe to
// call multiple times from multiple goroutines.
func (f *WebSocketFeed) Stop() {
	f.stopOnce.Do(func() {
		close(f.stop)
		// Yank the active conn (if any) out of the atomic and close
		// it. This unblocks readLoop() which is parked in
		// ReadMessage(). Any subsequent connect() inside Run() would
		// publish a new pointer, but the next iteration observes the
		// closed stop chan and bails before calling readLoop().
		if c := f.conn.Swap(nil); c != nil {
			_ = c.Close()
		}
	})
	f.wg.Wait()
}
