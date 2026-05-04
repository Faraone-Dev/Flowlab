// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

package server

import (
	"encoding/json"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"
)

type Server struct {
	mux          *http.ServeMux
	feed         Feed
	mode         string
	distDir      string
	storm        *StormController
	botHealthURL string
	recorder     *Recorder
}

// New keeps backward compatibility: synthetic feed by default.
func New() *Server {
	return NewWithFeed(NewSyntheticFeed(time.Now().UnixNano()), "synthetic")
}

// NewWithFeed wires an arbitrary Feed implementation. `mode` is a short
// label exposed via /status so the dashboard can show which source is live.
func NewWithFeed(f Feed, mode string) *Server {
	sc := newStormController()
	s := &Server{
		mux:          http.NewServeMux(),
		feed:         f,
		mode:         mode,
		storm:        sc,
		botHealthURL: "http://127.0.0.1:3001",
	}
	// Wire storm severity into the synthetic feed when present.
	if sf, ok := f.(*SyntheticFeed); ok {
		sf.storm = sc
	}
	// Recorder: writes events.jsonl + ticks.jsonl + run.yaml under
	// <repo-root>/data/runs/<id>/. Resolve repo root by walking up from CWD.
	root := resolveRepoRoot()
	botFn := func() ([]byte, bool) {
		client := &http.Client{Timeout: 500 * time.Millisecond}
		resp, err := client.Get(s.botHealthURL + "/api/state")
		if err != nil || resp.StatusCode != 200 {
			if resp != nil {
				resp.Body.Close()
			}
			return nil, false
		}
		defer resp.Body.Close()
		buf := make([]byte, 0, 4096)
		tmp := make([]byte, 4096)
		for {
			n, rerr := resp.Body.Read(tmp)
			if n > 0 {
				buf = append(buf, tmp[:n]...)
			}
			if rerr != nil {
				break
			}
		}
		return buf, true
	}
	s.recorder = NewRecorder(root, sc, botFn)
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
	// Storm control plane
	s.mux.HandleFunc("POST /storm/start", s.handleStormStart)
	s.mux.HandleFunc("POST /storm/stop", s.handleStormStop)
	s.mux.HandleFunc("GET /storm/status", s.handleStormStatus)
	// Zeus bot health proxy
	s.mux.HandleFunc("GET /bot/health", s.handleBotHealth)
	s.mux.HandleFunc("GET /bot/state", s.handleBotState)
	// Run recorder
	s.mux.HandleFunc("POST /run/start", s.handleRunStart)
	s.mux.HandleFunc("POST /run/stop", s.handleRunStop)
	s.mux.HandleFunc("GET /run/status", s.handleRunStatus)
	s.mux.HandleFunc("GET /run/list", s.handleRunList)
	s.mux.HandleFunc("GET /run/{id}/yaml", s.handleRunYAML)
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

// resolveRepoRoot walks up from CWD looking for a directory that contains
// "go.work" or "Cargo.toml" — that's the flowlab repo root, where data/runs
// will live. Falls back to CWD.
func resolveRepoRoot() string {
	cwd, err := os.Getwd()
	if err != nil {
		return "."
	}
	cur := cwd
	for i := 0; i < 6; i++ {
		if _, err := os.Stat(filepath.Join(cur, "go.work")); err == nil {
			return cur
		}
		parent := filepath.Dir(cur)
		if parent == cur {
			break
		}
		cur = parent
	}
	return cwd
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
}

// ── Storm control plane ────────────────────────────────────────────────

func (s *Server) handleStormStart(w http.ResponseWriter, r *http.Request) {
	var spec StormSpec
	if err := json.NewDecoder(r.Body).Decode(&spec); err != nil {
		w.Header().Set("Content-Type", "application/json")
		http.Error(w, `{"error":"invalid json body"}`, http.StatusBadRequest)
		return
	}
	snap := s.storm.Start(spec)
	if s.recorder != nil {
		s.recorder.LogStormStart(spec)
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(snap)
}

func (s *Server) handleStormStop(w http.ResponseWriter, r *http.Request) {
	kind := s.storm.ActiveKind()
	snap := s.storm.Stop()
	if s.recorder != nil {
		s.recorder.LogStormStop(kind)
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(snap)
}

func (s *Server) handleStormStatus(w http.ResponseWriter, r *http.Request) {
	snap := s.storm.Snapshot()
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(snap)
}

// ── Bot health proxy ───────────────────────────────────────────────────

func (s *Server) handleBotHealth(w http.ResponseWriter, r *http.Request) {
	client := &http.Client{Timeout: 1 * time.Second}
	resp, err := client.Get(s.botHealthURL)
	status := "online"
	code := http.StatusOK
	if err != nil {
		status = "offline"
		code = http.StatusServiceUnavailable
	} else {
		resp.Body.Close()
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(map[string]string{
		"bot":    "zeus-hft",
		"mode":   "dry-run",
		"status": status,
		"url":    s.botHealthURL,
	})
}

// handleBotState proxies zeus's /api/state JSON (P&L, win/loss, equity,
// last signal, trade history). Falls back to {"online":false} when zeus
// is unreachable so the dashboard always renders something.
func (s *Server) handleBotState(w http.ResponseWriter, r *http.Request) {
	client := &http.Client{Timeout: 800 * time.Millisecond}
	resp, err := client.Get(s.botHealthURL + "/api/state")
	w.Header().Set("Content-Type", "application/json")
	if err != nil || resp.StatusCode != http.StatusOK {
		if resp != nil {
			resp.Body.Close()
		}
		w.WriteHeader(http.StatusOK)
		json.NewEncoder(w).Encode(map[string]interface{}{
			"online": false,
			"url":    s.botHealthURL + "/api/state",
		})
		return
	}
	defer resp.Body.Close()
	w.WriteHeader(http.StatusOK)
	// Stream raw bytes through (no decode) — keeps zeus's schema intact.
	buf := make([]byte, 64*1024)
	for {
		n, rerr := resp.Body.Read(buf)
		if n > 0 {
			w.Write(buf[:n])
		}
		if rerr != nil {
			break
		}
	}
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

// cors allows same-origin requests and loopback dashboard dev origins.
// Rejecting foreign origins closes the control-plane's browser trust boundary
// without affecting the local desk workflow.
func cors(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		origin := r.Header.Get("Origin")
		if origin != "" {
			if !isAllowedOrigin(r, origin) {
				http.Error(w, "forbidden origin", http.StatusForbidden)
				return
			}
			w.Header().Set("Access-Control-Allow-Origin", origin)
			w.Header().Add("Vary", "Origin")
			w.Header().Set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
			w.Header().Set("Access-Control-Allow-Headers", "Content-Type")
		}
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		h.ServeHTTP(w, r)
	})
}

func isAllowedOrigin(r *http.Request, origin string) bool {
	if origin == "" {
		return true
	}
	u, err := url.Parse(origin)
	if err != nil || u.Host == "" {
		return false
	}
	if u.Scheme != "http" && u.Scheme != "https" {
		return false
	}
	host := strings.ToLower(u.Hostname())
	if host == "localhost" || host == "127.0.0.1" || host == "::1" {
		return true
	}
	return strings.EqualFold(normalizeHostPort(u.Host), normalizeHostPort(r.Host))
}

func normalizeHostPort(hostport string) string {
	if hostport == "" {
		return ""
	}
	if host, port, err := net.SplitHostPort(hostport); err == nil {
		return strings.ToLower(net.JoinHostPort(host, port))
	}
	return strings.ToLower(hostport)
}
