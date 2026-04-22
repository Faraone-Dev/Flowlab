package server

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"
)

// Recorder writes a desk-grade artefact set for one run:
//
//	data/runs/<run_id>/
//	  events.jsonl     — append-only JSONL: storm_start, storm_stop,
//	                     breaker_trip, target_signal, target_trade.
//	  ticks.jsonl      — sampled tick metrics (1 row per second by default).
//	  run.yaml         — manifest + post-run summary (the scoreboard).
//
// The format mirrors what a desk would expect from a serious bench:
// append-only JSONL is grep/jq-friendly during a live session; YAML
// summary is human-scannable post-run; both can be reloaded by any
// downstream tool (DuckDB, polars, kdb+/q via CSV bridge, etc.).
type Recorder struct {
	mu         sync.Mutex
	rootDir    string
	active     *runState
	storm      *StormController
	botStateFn func() ([]byte, bool)
}

type runState struct {
	id          string
	dir         string
	startedAt   time.Time
	eventsFile  *os.File
	ticksFile   *os.File
	tickEvery   time.Duration
	lastTickAt  time.Time
	tickCount   uint64
	storms      []stormRecord
	startEquity float64
	startCcy    string
	endEquity   float64
	startSnap   map[string]any
	endSnap     map[string]any
}

type stormRecord struct {
	Kind           string  `yaml:"kind"`
	StartedAtMs    int64   `yaml:"started_at_ms"`
	Severity       float64 `yaml:"severity"`
	DurationMs     uint64  `yaml:"duration_ms"`
	EquityDeltaEUR float64 `yaml:"target_pnl_delta_eur"`
	startEquity    float64
}

// NewRecorder constructs a recorder rooted at <rootDir>/data/runs.
// botStateFn is a callback that returns the current target bot snapshot
// (zeus's /api/state body) so we can compute equity deltas around storms.
func NewRecorder(rootDir string, storm *StormController, botStateFn func() ([]byte, bool)) *Recorder {
	return &Recorder{
		rootDir:    rootDir,
		storm:      storm,
		botStateFn: botStateFn,
	}
}

// Active returns true when a run is being recorded.
func (r *Recorder) Active() bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.active != nil
}

// CurrentID returns the active run id (or "").
func (r *Recorder) CurrentID() string {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.active == nil {
		return ""
	}
	return r.active.id
}

// Start opens a new run directory and begins recording.
func (r *Recorder) Start() (string, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.active != nil {
		return r.active.id, fmt.Errorf("run already active: %s", r.active.id)
	}
	id := time.Now().UTC().Format("2006-01-02T15-04-05Z")
	dir := filepath.Join(r.rootDir, "data", "runs", id)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", err
	}
	evf, err := os.OpenFile(filepath.Join(dir, "events.jsonl"), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		return "", err
	}
	tkf, err := os.OpenFile(filepath.Join(dir, "ticks.jsonl"), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		evf.Close()
		return "", err
	}
	st := &runState{
		id:         id,
		dir:        dir,
		startedAt:  time.Now().UTC(),
		eventsFile: evf,
		ticksFile:  tkf,
		tickEvery:  1 * time.Second,
	}
	if r.botStateFn != nil {
		if body, ok := r.botStateFn(); ok {
			var m map[string]any
			if json.Unmarshal(body, &m) == nil {
				st.startSnap = m
				if eq, ok := m["equity"].(float64); ok {
					st.startEquity = eq
				}
				if ccy, ok := m["account_currency"].(string); ok {
					st.startCcy = ccy
				}
			}
		}
	}
	r.active = st
	r.writeEventLocked(map[string]any{
		"ts_ms":               time.Now().UnixMilli(),
		"type":                "run_start",
		"run":                 id,
		"target_start_equity": st.startEquity,
		"target_currency":     st.startCcy,
	})
	return id, nil
}

// Stop finalizes the current run, writes run.yaml and closes files.
func (r *Recorder) Stop() (string, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.active == nil {
		return "", fmt.Errorf("no active run")
	}
	st := r.active
	if r.botStateFn != nil {
		if body, ok := r.botStateFn(); ok {
			var m map[string]any
			if json.Unmarshal(body, &m) == nil {
				st.endSnap = m
				if eq, ok := m["equity"].(float64); ok {
					st.endEquity = eq
				}
			}
		}
	}
	r.writeEventLocked(map[string]any{
		"ts_ms":             time.Now().UnixMilli(),
		"type":              "run_stop",
		"run":               st.id,
		"target_end_equity": st.endEquity,
		"target_delta":      st.endEquity - st.startEquity,
	})
	if err := r.writeManifestLocked(st); err != nil {
		// best effort, still close
		_ = err
	}
	st.eventsFile.Close()
	st.ticksFile.Close()
	id := st.id
	r.active = nil
	return id, nil
}

// LogStormStart records the moment a storm fires + captures equity baseline.
func (r *Recorder) LogStormStart(spec StormSpec) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.active == nil {
		return
	}
	rec := stormRecord{
		Kind:        string(spec.Kind),
		StartedAtMs: time.Now().UnixMilli(),
		Severity:    spec.Severity,
		DurationMs:  spec.DurationMs,
	}
	if r.botStateFn != nil {
		if body, ok := r.botStateFn(); ok {
			var m map[string]any
			if json.Unmarshal(body, &m) == nil {
				if eq, ok := m["equity"].(float64); ok {
					rec.startEquity = eq
				}
			}
		}
	}
	r.active.storms = append(r.active.storms, rec)
	r.writeEventLocked(map[string]any{
		"ts_ms":                  rec.StartedAtMs,
		"type":                   "storm_start",
		"kind":                   rec.Kind,
		"severity":               rec.Severity,
		"duration_ms":            rec.DurationMs,
		"seed":                   spec.Seed,
		"target_equity_at_start": rec.startEquity,
	})
}

// LogStormStop closes the most recent storm record with equity delta.
func (r *Recorder) LogStormStop(kind string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.active == nil || len(r.active.storms) == 0 {
		return
	}
	st := &r.active.storms[len(r.active.storms)-1]
	endEq := st.startEquity
	if r.botStateFn != nil {
		if body, ok := r.botStateFn(); ok {
			var m map[string]any
			if json.Unmarshal(body, &m) == nil {
				if eq, ok := m["equity"].(float64); ok {
					endEq = eq
				}
			}
		}
	}
	st.EquityDeltaEUR = endEq - st.startEquity
	r.writeEventLocked(map[string]any{
		"ts_ms":                time.Now().UnixMilli(),
		"type":                 "storm_stop",
		"kind":                 kind,
		"target_equity_at_end": endEq,
		"target_delta_eur":     st.EquityDeltaEUR,
	})
}

// LogTick samples a tick at most once per `tickEvery` interval.
func (r *Recorder) LogTick(t Tick) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.active == nil {
		return
	}
	now := time.Now()
	if !r.active.lastTickAt.IsZero() && now.Sub(r.active.lastTickAt) < r.active.tickEvery {
		return
	}
	r.active.lastTickAt = now
	r.active.tickCount++
	row := map[string]any{
		"ts_ns":          t.TsNs,
		"seq":            t.Seq,
		"mid":            float64(t.MidTicks) / 10000.0,
		"spread_bps":     t.SpreadBps,
		"vpin":           t.Vpin,
		"imbalance":      t.Imbalance,
		"bid_depth":      t.BidDepth,
		"ask_depth":      t.AskDepth,
		"regime":         t.Regime,
		"events_per_sec": t.EventsPerSec,
		"lat_p50_ns":     t.LatP50Ns,
		"lat_p99_ns":     t.LatP99Ns,
		"storm_active":   t.StormActive,
		"storm_kind":     t.StormKind,
	}
	b, _ := json.Marshal(row)
	r.active.ticksFile.Write(b)
	r.active.ticksFile.Write([]byte("\n"))
}

func (r *Recorder) writeEventLocked(ev map[string]any) {
	if r.active == nil {
		return
	}
	b, _ := json.Marshal(ev)
	r.active.eventsFile.Write(b)
	r.active.eventsFile.Write([]byte("\n"))
}

// writeManifestLocked emits run.yaml — minimal hand-rolled YAML to avoid
// pulling a YAML dep. The schema is stable: run_id, timings, storms[],
// target { start_equity, end_equity, delta }, verdict.
func (r *Recorder) writeManifestLocked(st *runState) error {
	endedAt := time.Now().UTC()
	delta := st.endEquity - st.startEquity
	verdict := "TARGET_INTACT"
	if delta < -10 {
		verdict = "TARGET_DAMAGED"
	}
	if delta < -100 {
		verdict = "TARGET_KILLED"
	}
	f, err := os.Create(filepath.Join(st.dir, "run.yaml"))
	if err != nil {
		return err
	}
	defer f.Close()
	w := f
	fmt.Fprintf(w, "run_id: %s\n", st.id)
	fmt.Fprintf(w, "started_at: %s\n", st.startedAt.Format(time.RFC3339))
	fmt.Fprintf(w, "ended_at:   %s\n", endedAt.Format(time.RFC3339))
	fmt.Fprintf(w, "duration_s: %d\n", int(endedAt.Sub(st.startedAt).Seconds()))
	fmt.Fprintf(w, "tick_samples: %d\n", st.tickCount)
	fmt.Fprintln(w, "storms_fired:")
	if len(st.storms) == 0 {
		fmt.Fprintln(w, "  []")
	}
	for _, s := range st.storms {
		fmt.Fprintf(w, "  - kind: %s\n", s.Kind)
		fmt.Fprintf(w, "    started_at_ms: %d\n", s.StartedAtMs)
		fmt.Fprintf(w, "    severity: %.3f\n", s.Severity)
		fmt.Fprintf(w, "    duration_ms: %d\n", s.DurationMs)
		fmt.Fprintf(w, "    target_pnl_delta_eur: %.2f\n", s.EquityDeltaEUR)
	}
	fmt.Fprintln(w, "target:")
	fmt.Fprintf(w, "  bot: zeus-hft\n")
	fmt.Fprintf(w, "  currency: %s\n", safeStr(st.startCcy, "EUR"))
	fmt.Fprintf(w, "  start_equity: %.2f\n", st.startEquity)
	fmt.Fprintf(w, "  end_equity:   %.2f\n", st.endEquity)
	fmt.Fprintf(w, "  delta:        %.2f\n", delta)
	if st.endSnap != nil {
		if v, ok := st.endSnap["total_trades"].(float64); ok {
			fmt.Fprintf(w, "  total_trades: %d\n", int(v))
		}
		if v, ok := st.endSnap["wins"].(float64); ok {
			fmt.Fprintf(w, "  wins: %d\n", int(v))
		}
		if v, ok := st.endSnap["losses"].(float64); ok {
			fmt.Fprintf(w, "  losses: %d\n", int(v))
		}
		if v, ok := st.endSnap["win_rate"].(float64); ok {
			fmt.Fprintf(w, "  win_rate: %.3f\n", v)
		}
	}
	fmt.Fprintf(w, "verdict: %s\n", verdict)
	return nil
}

func safeStr(s, dflt string) string {
	if s == "" {
		return dflt
	}
	return s
}

// ── HTTP ─────────────────────────────────────────────────────────────

// RunListEntry is a single past run summary for /run/list.
type RunListEntry struct {
	ID        string `json:"id"`
	StartedAt string `json:"started_at,omitempty"`
	HasYAML   bool   `json:"has_yaml"`
}

func (s *Server) handleRunStart(w http.ResponseWriter, r *http.Request) {
	id, err := s.recorder.Start()
	w.Header().Set("Content-Type", "application/json")
	if err != nil {
		w.WriteHeader(http.StatusConflict)
		json.NewEncoder(w).Encode(map[string]string{"error": err.Error(), "id": id})
		return
	}
	json.NewEncoder(w).Encode(map[string]any{"id": id, "active": true})
}

func (s *Server) handleRunStop(w http.ResponseWriter, r *http.Request) {
	id, err := s.recorder.Stop()
	w.Header().Set("Content-Type", "application/json")
	if err != nil {
		w.WriteHeader(http.StatusConflict)
		json.NewEncoder(w).Encode(map[string]string{"error": err.Error()})
		return
	}
	json.NewEncoder(w).Encode(map[string]any{"id": id, "active": false})
}

func (s *Server) handleRunStatus(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]any{
		"active": s.recorder.Active(),
		"id":     s.recorder.CurrentID(),
	})
}

func (s *Server) handleRunList(w http.ResponseWriter, r *http.Request) {
	dir := filepath.Join(s.recorder.rootDir, "data", "runs")
	w.Header().Set("Content-Type", "application/json")
	entries, err := os.ReadDir(dir)
	if err != nil {
		json.NewEncoder(w).Encode([]RunListEntry{})
		return
	}
	out := make([]RunListEntry, 0, len(entries))
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		_, err := os.Stat(filepath.Join(dir, e.Name(), "run.yaml"))
		out = append(out, RunListEntry{
			ID:      e.Name(),
			HasYAML: err == nil,
		})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].ID > out[j].ID })
	json.NewEncoder(w).Encode(out)
}

// handleRunYAML returns the run.yaml content for a given id (text/plain).
func (s *Server) handleRunYAML(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	if id == "" {
		http.Error(w, "missing id", http.StatusBadRequest)
		return
	}
	p := filepath.Join(s.recorder.rootDir, "data", "runs", id, "run.yaml")
	f, err := os.Open(p)
	if err != nil {
		http.Error(w, "not found", http.StatusNotFound)
		return
	}
	defer f.Close()
	w.Header().Set("Content-Type", "text/plain; charset=utf-8")
	io.Copy(w, f)
}
