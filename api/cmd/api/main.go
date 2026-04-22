package main

import (
	"flag"
	"log"
	"net/http"
	"time"

	"github.com/IvanPiardi/flowlab/api/server"
)

func main() {
	addr := flag.String("addr", ":8080", "API server listen address")
	feedKind := flag.String("feed", "synthetic",
		"feed source: synthetic (in-process Go) | engine (TCP from flowlab-engine)")
	engineAddr := flag.String("engine", "127.0.0.1:9090",
		"flowlab-engine telemetry address (only used when -feed=engine)")
	flag.Parse()

	var srv *server.Server
	switch *feedKind {
	case "synthetic":
		srv = server.New()
		log.Printf("flowlab control plane: feed=synthetic")
	case "engine":
		c := server.NewEngineClient(*engineAddr)
		c.Start()
		srv = server.NewWithFeed(c, "engine:"+*engineAddr)
		log.Printf("flowlab control plane: feed=engine %s", *engineAddr)
	default:
		log.Fatalf("unknown -feed value: %q (want synthetic|engine)", *feedKind)
	}

	httpSrv := &http.Server{
		Addr:              *addr,
		Handler:           srv.Handler(),
		ReadHeaderTimeout: 5 * time.Second,
	}
	log.Printf("flowlab control plane listening on %s", *addr)
	if err := httpSrv.ListenAndServe(); err != nil {
		log.Fatalf("server error: %v", err)
	}
}
