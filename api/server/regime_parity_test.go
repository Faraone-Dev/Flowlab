// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

package server

import (
	"math"
	"testing"
)

// regimeParityVectors are the SHARED test vectors used to lock the
// Go classifier in step with `flow/src/regime.rs`. The expected
// scores were derived by hand from the documented Rust formula
// (see comments in regime.go). If you change a weight or threshold
// in either side WITHOUT updating both files + this table, this
// test will fail — which is exactly the point.
//
// Mirror these vectors into a Rust unit test if you ever want a
// bit-identical cross-language assertion; today they are duplicated
// in spirit, not in code.
//
// Convention: every "neutral" case sets spreadBlowoutRatio=1.0 and
// tradeVelocityRatio=1.0 — those are the values that make the
// corresponding score terms cleanly zero. A zero default would add
// 0.6 from the velocity term and obscure what we're actually testing.
func neutralInput() regimeInput {
	return regimeInput{
		spreadBlowoutRatio: 1.0,
		tradeVelocityRatio: 1.0,
		eventsPerSec:       1000,
	}
}

func withVpin(in regimeInput, v float64) regimeInput      { in.vpin = v; return in }
func withImbalance(in regimeInput, v float64) regimeInput { in.bookImbalance = v; return in }
func withDrift(in regimeInput, v float64) regimeInput     { in.midDriftBps = v; return in }
func withVel(in regimeInput, v float64) regimeInput       { in.tradeVelocityRatio = v; return in }
func withEps(in regimeInput, v uint64) regimeInput        { in.eventsPerSec = v; return in }
func withSpread(in regimeInput, v float64) regimeInput    { in.spreadBlowoutRatio = v; return in }
func withCrossed(in regimeInput, v bool) regimeInput      { in.bookCrossed = v; return in }

var regimeParityVectors = []struct {
	name       string
	in         regimeInput
	wantScore  float64
	wantRegime uint8
}{
	{
		name:       "calm_baseline",
		in:         neutralInput(),
		wantScore:  0.0,
		wantRegime: 0,
	},
	{
		// vpin 0.12 alone → 0.6 → exactly the volatile threshold
		name:       "vpin_volatile_edge",
		in:         withVpin(neutralInput(), 0.12),
		wantScore:  0.6,
		wantRegime: 1,
	},
	{
		// vpin 0.30 → 1.5 → exactly aggressive threshold
		name:       "vpin_aggressive_edge",
		in:         withVpin(neutralInput(), 0.30),
		wantScore:  1.5,
		wantRegime: 2,
	},
	{
		// vpin 0.60 → 3.0 → exactly crisis threshold
		name:       "vpin_crisis_edge",
		in:         withVpin(neutralInput(), 0.60),
		wantScore:  3.0,
		wantRegime: 3,
	},
	{
		// imbalance 0.30 → 0.9 → volatile
		name:       "imbalance_volatile",
		in:         withImbalance(neutralInput(), 0.30),
		wantScore:  0.9,
		wantRegime: 1,
	},
	{
		// crossed-book zeros spread + imbalance contributions
		name: "crossed_book_suppresses_spread_and_imb",
		in: withCrossed(
			withImbalance(withSpread(withVpin(neutralInput(), 0.10), 5.0), 0.50),
			true,
		),
		wantScore:  0.5, // only vpin*5 = 0.5 survives
		wantRegime: 0,
	},
	{
		// freshness damper: same raw but eps=15/s → score halved
		name:       "freshness_damper_halves_score",
		in:         withEps(withVpin(neutralInput(), 0.30), 15),
		wantScore:  0.75,
		wantRegime: 1, // 0.75 ≥ 0.6 → volatile
	},
	{
		// eps=0 ⇒ freshness=1 (backwards compat for callers not
		// tracking eps), so a full crisis score still classifies
		name:       "eps_zero_means_full_freshness",
		in:         withEps(withVpin(neutralInput(), 0.60), 0),
		wantScore:  3.0,
		wantRegime: 3,
	},
	{
		// drift alone at 10 bps/s → 3.0 → crisis (matches Rust comment)
		name:       "mid_drift_alone_crisis",
		in:         withDrift(neutralInput(), 10.0),
		wantScore:  3.0,
		wantRegime: 3,
	},
	{
		// |vel - 1| = 1 (e.g. 2x burst) → 0.6 → volatile edge
		name:       "velocity_burst_volatile_edge",
		in:         withVel(neutralInput(), 2.0),
		wantScore:  0.6,
		wantRegime: 1,
	},
}

func TestRegimeParityWithRust(t *testing.T) {
	const eps = 1e-9
	for _, v := range regimeParityVectors {
		t.Run(v.name, func(t *testing.T) {
			gotScore := compositeScore(v.in)
			if math.Abs(gotScore-v.wantScore) > eps {
				t.Errorf("score mismatch: got %.6f want %.6f", gotScore, v.wantScore)
			}
			gotRegime := classifyRegime(v.in)
			if gotRegime != v.wantRegime {
				t.Errorf("regime mismatch: got %d want %d (score=%.6f)",
					gotRegime, v.wantRegime, gotScore)
			}
		})
	}
}
