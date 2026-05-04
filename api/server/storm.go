// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

package server

import (
	"sync"
	"time"
)

// StormKind mirrors chaos::ChaosKind in the Rust chaos crate.
type StormKind string

const (
	StormKindPhantomLiquidity  StormKind = "PhantomLiquidity"
	StormKindCancellationStorm StormKind = "CancellationStorm"
	StormKindMomentumIgnition  StormKind = "MomentumIgnition"
	StormKindFlashCrash        StormKind = "FlashCrash"
	StormKindLatencyArbProxy   StormKind = "LatencyArbProxy"
)

// StormSpec is the JSON body for POST /storm/start.
type StormSpec struct {
	Kind       StormKind `json:"kind"`
	Severity   float64   `json:"severity"`    // [0.0, 1.0]
	DurationMs uint64    `json:"duration_ms"` // 0 = infinite until explicit stop
	Seed       uint64    `json:"seed"`
}

// StormMode mirrors Rust StormMode.
type StormMode string

const (
	StormModeIdle   StormMode = "Idle"
	StormModeActive StormMode = "Active"
)

// StormSnapshot is the JSON response body for GET /storm/status.
type StormSnapshot struct {
	Mode        StormMode  `json:"mode"`
	Kind        *StormKind `json:"kind,omitempty"`
	Severity    float64    `json:"severity"`
	Seed        uint64     `json:"seed"`
	StartedAtMs *int64     `json:"started_at_ms,omitempty"`
	ExpiresAtMs *int64     `json:"expires_at_ms,omitempty"` // null = runs until stop
}

// StormController manages storm lifecycle for the HTTP control-plane endpoints.
// Thread-safe; all state protected by mu.
type StormController struct {
	mu      sync.Mutex
	active  bool
	spec    StormSpec
	startMs int64
	expMs   int64 // 0 = no expiry
}

func newStormController() *StormController { return &StormController{} }

// Start activates a storm. Returns the resulting snapshot.
func (c *StormController) Start(spec StormSpec) StormSnapshot {
	if spec.Severity < 0 {
		spec.Severity = 0
	}
	if spec.Severity > 1 {
		spec.Severity = 1
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	c.active = true
	c.spec = spec
	c.startMs = time.Now().UnixMilli()
	c.expMs = 0
	if spec.DurationMs > 0 {
		c.expMs = c.startMs + int64(spec.DurationMs)
	}
	return c.snapshotLocked()
}

// Stop deactivates the current storm. Returns the resulting idle snapshot.
func (c *StormController) Stop() StormSnapshot {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.active = false
	return c.snapshotLocked()
}

// Snapshot returns a read-only view. Checks expiry automatically.
func (c *StormController) Snapshot() StormSnapshot {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.checkExpiryLocked()
	return c.snapshotLocked()
}

// ActiveSeverity returns severity if a storm is running, 0 otherwise.
// Called from the hot tick path — uses its own lock (no nesting with feed.mu).
func (c *StormController) ActiveSeverity() float64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.checkExpiryLocked()
	if !c.active {
		return 0
	}
	return c.spec.Severity
}

// ActiveKind returns the kind label if a storm is running, "" otherwise.
func (c *StormController) ActiveKind() string {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.checkExpiryLocked()
	if !c.active {
		return ""
	}
	return string(c.spec.Kind)
}

func (c *StormController) checkExpiryLocked() {
	if c.active && c.expMs > 0 && time.Now().UnixMilli() >= c.expMs {
		c.active = false
	}
}

func (c *StormController) snapshotLocked() StormSnapshot {
	if !c.active {
		return StormSnapshot{Mode: StormModeIdle}
	}
	k := c.spec.Kind
	snap := StormSnapshot{
		Mode:        StormModeActive,
		Kind:        &k,
		Severity:    c.spec.Severity,
		Seed:        c.spec.Seed,
		StartedAtMs: &c.startMs,
	}
	if c.expMs > 0 {
		snap.ExpiresAtMs = &c.expMs
	}
	return snap
}
