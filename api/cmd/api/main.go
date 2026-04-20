package main

import (
	"flag"
	"log"
	"net/http"

	"github.com/IvanPiardi/flowlab/api/server"
)

func main() {
	addr := flag.String("addr", ":8080", "API server listen address")
	flag.Parse()

	srv := server.New()

	log.Printf("flowlab control plane listening on %s", *addr)
	if err := http.ListenAndServe(*addr, srv.Handler()); err != nil {
		log.Fatalf("server error: %v", err)
	}
}
