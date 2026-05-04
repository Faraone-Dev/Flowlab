// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

package server

import "math"

// =============================================================
// Regime classifier — Go mirror of `flow/src/regime.rs`.
//
// SYNC CONTRACT:
//   This file is the dashboard-side mirror of the canonical Rust
//   classifier. The Rust version is the source of truth. Any change
//   to weights or thresholds in `flow/src/regime.rs` MUST be applied
//   here in the same commit, and the parity vectors in
//   `regime_parity_test.go` must be updated to match.
//
//   Do NOT diverge "just for the dashboard". The whole point of
//   showing a regime number to the operator is that it equals what
//   the engine sees.
//
// Layout intentionally mirrors the Rust struct field-by-field so a
// diff of the two files is trivial to read.
// =============================================================

const (
	regimeVolatileThreshold   = 0.6
	regimeAggressiveThreshold = 1.5
	regimeCrisisThreshold     = 3.0
)

type regimeInput struct {
	spreadBlowoutRatio float64
	bookImbalance      float64
	vpin               float64
	tradeVelocityRatio float64
	depthDepletion     float64
	midDriftBps        float64
	eventsPerSec       uint64
	bookCrossed        bool
}

// compositeScore mirrors RegimeClassifier::composite_score in Rust.
//
// Weights (locked to flow/src/regime.rs):
//   spread_blowout - 1   x 1.0
//   |imbalance|          x 3.0
//   vpin                 x 5.0
//   |vel_ratio - 1|      x 0.6
//   mid_drift_bps        x 0.30
//   depth_depletion      x 3.0
//
// Crossed-book guard zeros spread+imbalance contributions.
// Freshness damper linearly fades the score as eps drops below 30/s;
// eps==0 is treated as "unknown" → full freshness for backwards
// compatibility with callers that don't track eps.
func compositeScore(in regimeInput) float64 {
	var spreadScore, imbalanceScore float64
	if !in.bookCrossed {
		spreadScore = math.Max(0, in.spreadBlowoutRatio-1.0)
		imbalanceScore = math.Abs(in.bookImbalance) * 3.0
	}
	vpinScore := in.vpin * 5.0
	velocityScore := math.Abs(in.tradeVelocityRatio-1.0) * 0.6
	driftScore := in.midDriftBps * 0.30
	depthScore := in.depthDepletion * 3.0
	raw := spreadScore + imbalanceScore + vpinScore + velocityScore + driftScore + depthScore

	freshness := 1.0
	if in.eventsPerSec != 0 {
		freshness = math.Min(1.0, float64(in.eventsPerSec)/30.0)
	}
	return raw * freshness
}

// classifyRegime returns 0=Calm, 1=Volatile, 2=Aggressive, 3=Crisis.
// Mirrors RegimeClassifier::classify in Rust.
func classifyRegime(in regimeInput) uint8 {
	s := compositeScore(in)
	switch {
	case s >= regimeCrisisThreshold:
		return 3
	case s >= regimeAggressiveThreshold:
		return 2
	case s >= regimeVolatileThreshold:
		return 1
	default:
		return 0
	}
}
