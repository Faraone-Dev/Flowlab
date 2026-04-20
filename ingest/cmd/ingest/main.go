package main

import (
	"flag"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/IvanPiardi/flowlab/ingest/feed"
	"github.com/IvanPiardi/flowlab/ingest/mmap"
)

func main() {
	bufferPath := flag.String("buffer", "/tmp/flowlab-ring", "path to mmap ring buffer file")
	bufferSize := flag.Int("size", 64*1024*1024, "ring buffer size in bytes")
	feedURL := flag.String("feed", "", "WebSocket feed URL")
	flag.Parse()

	if *feedURL == "" {
		log.Fatal("--feed URL required")
	}

	// Create mmap ring buffer for zero-copy transfer to Zig/Rust
	ring, err := mmap.NewRingBuffer(*bufferPath, *bufferSize)
	if err != nil {
		log.Fatalf("failed to create ring buffer: %v", err)
	}
	defer ring.Close()

	// Start feed connection
	feeder := feed.NewWebSocketFeed(*feedURL, ring)
	go feeder.Run()

	// Graceful shutdown
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
	<-sig

	log.Println("shutting down...")
	feeder.Stop()
}
