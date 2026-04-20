package feed

import (
	"log"
	"sync"
	"time"

	"github.com/IvanPiardi/flowlab/ingest/mmap"
	"github.com/gorilla/websocket"
)

// WebSocketFeed connects to an exchange feed and writes raw data
// to the mmap ring buffer for zero-copy processing by Zig/Rust.
type WebSocketFeed struct {
	url  string
	ring *mmap.RingBuffer
	conn *websocket.Conn
	stop chan struct{}
	wg   sync.WaitGroup
}

func NewWebSocketFeed(url string, ring *mmap.RingBuffer) *WebSocketFeed {
	return &WebSocketFeed{
		url:  url,
		ring: ring,
		stop: make(chan struct{}),
	}
}

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
			time.Sleep(time.Second)
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
	f.conn = conn
	log.Printf("connected to %s", f.url)
	return nil
}

func (f *WebSocketFeed) readLoop() {
	defer f.conn.Close()

	for {
		select {
		case <-f.stop:
			return
		default:
		}

		_, message, err := f.conn.ReadMessage()
		if err != nil {
			log.Printf("read error: %v", err)
			return
		}

		// Write raw bytes to ring buffer — Zig parser handles the rest
		if err := f.ring.Write(message); err != nil {
			log.Printf("ring buffer write error: %v", err)
			// Backpressure: slow down reads
			time.Sleep(time.Microsecond * 100)
		}
	}
}

func (f *WebSocketFeed) Stop() {
	close(f.stop)
	if f.conn != nil {
		f.conn.Close()
	}
	f.wg.Wait()
}
