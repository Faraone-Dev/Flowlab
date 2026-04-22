package server

import (
	"encoding/json"
	"log"
	"net/http"
	"time"

	"github.com/gorilla/websocket"
)

var upgrader = websocket.Upgrader{
	ReadBufferSize:  1024,
	WriteBufferSize: 4096,
	CheckOrigin:     func(r *http.Request) bool { return true }, // dashboard dev
}

// handleStream upgrades the connection to WebSocket and streams synthetic
// telemetry ticks at ~50 Hz. Each frame is a JSON-encoded Tick.
//
// Frame budget: ~20 ms per tick. We deliberately push a single tick per
// frame (not batches) so the dashboard can plot a smooth line and compute
// per-sample z-scores without reframing.
func (s *Server) handleStream(w http.ResponseWriter, r *http.Request) {
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		log.Printf("ws upgrade failed: %v", err)
		return
	}
	defer conn.Close()

	ticker := time.NewTicker(20 * time.Millisecond)
	defer ticker.Stop()

	// Drop the read goroutine: we want to notice client disconnect quickly.
	done := make(chan struct{})
	go func() {
		defer close(done)
		for {
			if _, _, err := conn.ReadMessage(); err != nil {
				return
			}
		}
	}()

	for {
		select {
		case <-done:
			return
		case <-ticker.C:
			tick := s.feed.Next()
			tick.Symbol, tick.InstrumentID = s.feed.Symbol()
			tick.Trades = s.feed.RecentTrades()
			if s.recorder != nil {
				s.recorder.LogTick(tick)
			}
			if err := conn.WriteJSON(tick); err != nil {
				return
			}
		}
	}
}

// handleReset clears the synthetic breaker latch (operator action).
func (s *Server) handleReset(w http.ResponseWriter, r *http.Request) {
	s.feed.Reset()
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]string{"status": "reset"})
}
