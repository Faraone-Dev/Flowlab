package server

import (
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"time"
)

type Server struct {
	mux     *http.ServeMux
	feed    Feed
	mode    string
	distDir string
}

// New keeps backward compatibility: synthetic feed by default.
func New() *Server {
	return NewWithFeed(NewSyntheticFeed(time.Now().UnixNano()), "synthetic")
}

// NewWithFeed wires an arbitrary Feed implementation. `mode` is a short
// label exposed via /status so the dashboard can show which source is live.
func NewWithFeed(f Feed, mode string) *Server {
	s := &Server{
		mux:  http.NewServeMux(),
		feed: f,
		mode: mode,
	}
	s.routes()
	return s
}

func (s *Server) Handler() http.Handler {
	return cors(s.mux)
}

func (s *Server) routes() {
	s.mux.HandleFunc("GET /health", s.handleHealth)
	s.mux.HandleFunc("GET /status", s.handleStatus)
	s.mux.HandleFunc("GET /stream", s.handleStream)
	s.mux.HandleFunc("POST /reset", s.handleReset)
	// Serve the built dashboard from the same origin as the API so the
	// browser does not have to cross ports (kills Firefox tracking-protection
	// / third-party cookie flakiness around dev proxies).
	s.distDir = resolveDashboardDist()
	if s.distDir != "" {
		fs := http.FileServer(http.Dir(s.distDir))
		s.mux.Handle("GET /", fs)
	}
}

// resolveDashboardDist walks a few well-known locations relative to the
// binary to find the Vite build output. Returns "" when no dashboard is
// shipped alongside the binary — the API keeps working headless.
func resolveDashboardDist() string {
	candidates := []string{
		"dashboard/dist",
		"../dashboard/dist",
		"../../dashboard/dist",
		"../flowlab/dashboard/dist",
	}
	if exe, err := os.Executable(); err == nil {
		exeDir := filepath.Dir(exe)
		for _, rel := range candidates {
			p := filepath.Join(exeDir, rel)
			if fi, err := os.Stat(p); err == nil && fi.IsDir() {
				return p
			}
		}
	}
	if cwd, err := os.Getwd(); err == nil {
		for _, rel := range candidates {
			p := filepath.Join(cwd, rel)
			if fi, err := os.Stat(p); err == nil && fi.IsDir() {
				return p
			}
		}
	}
	return ""
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
}

func (s *Server) handleStatus(w http.ResponseWriter, r *http.Request) {
	sym, inst := s.feed.Symbol()
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	json.NewEncoder(w).Encode(map[string]interface{}{
		"version":       "0.1.0",
		"service":       "flowlab-api",
		"mode":          s.mode,
		"symbol":        sym,
		"instrument_id": inst,
		"replaying":     false,
	})
}

// cors is a permissive CORS wrapper for dashboard development. The control
// plane is read-only and never participates in the deterministic data path,
// so allowing any origin during local dev is safe.
func cors(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		h.ServeHTTP(w, r)
	})
}
