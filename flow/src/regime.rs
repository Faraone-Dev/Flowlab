/// Market regime classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Regime {
    Calm = 0,
    Volatile = 1,
    Aggressive = 2,
    Crisis = 3,
}

/// Input metrics for regime classification.
#[derive(Debug, Clone, Copy, Default)]
pub struct RegimeInput {
    pub spread_blowout_ratio: f64,
    pub book_imbalance: f64,
    pub vpin: f64,
    pub trade_velocity_ratio: f64,
    pub depth_depletion: f64,
    /// |Δmid|/mid in bps over the last 1s window. Lets the regime
    /// react to sharp directional price moves even when the book
    /// stays balanced (the classic toxic-flow signature).
    pub mid_drift_bps: f64,
    /// Events-per-second observed in the last 1s window. Used as a
    /// freshness dampener: when the tape is dead the topline of the
    /// book gets stale and spread/imbalance become artifacts, not
    /// real stress. Default 1000 (“fully fresh”) so callers that
    /// don't track eps still behave like the legacy classifier.
    pub events_per_sec: u64,
    /// True when bid >= ask at the time of measurement. On inferior
    /// venues like Nasdaq-BX the local top can sit below NBBO for
    /// long stretches; in that state `book_imbalance` and
    /// `spread_blowout_ratio` are computed against an inverted top
    /// and saturate. The classifier zeroes those two contributions
    /// when this flag is set, trusting only vpin / velocity / drift.
    pub book_crossed: bool,
}

/// Threshold-based regime classifier.
pub struct RegimeClassifier {
    pub volatile_threshold: f64,
    pub aggressive_threshold: f64,
    pub crisis_threshold: f64,
}

impl Default for RegimeClassifier {
    fn default() -> Self {
        // HFT-grade aggressive thresholds. Real-world VPIN rarely
        // exceeds 0.3-0.5 even on toxic minutes (Easley/Lopez de Prado),
        // so weighting VPIN at 5x and gating VOLATILE at score>=1.5
        // means we'd only flip out of CALM when vpin alone hits 0.3 —
        // a level at which a market maker has already pulled quotes.
        // Lower the bar so the regime tracks the live tape:
        //   - VOLATILE  at vpin~0.12 OR imbalance~0.30 OR spread 1.3x
        //   - AGGRESSIVE at vpin~0.30 OR strong imbalance + widening
        //   - CRISIS    at vpin~0.50 + spread blowout 2x
        Self {
            volatile_threshold: 0.6,
            aggressive_threshold: 1.5,
            crisis_threshold: 3.0,
        }
    }
}

impl RegimeClassifier {
    /// Classify based on composite stress score.
    pub fn classify(&self, input: &RegimeInput) -> Regime {
        let score = self.composite_score(input);

        if score >= self.crisis_threshold {
            Regime::Crisis
        } else if score >= self.aggressive_threshold {
            Regime::Aggressive
        } else if score >= self.volatile_threshold {
            Regime::Volatile
        } else {
            Regime::Calm
        }
    }

    /// Composite stress score: weighted sum of normalized metrics.
    /// Weights tuned so the regime *oscillates* across all four levels
    /// on real ITCH replays instead of being pinned by a single signal:
    ///   spread_blowout - 1   x 1.0  (1.5x widening = +0.5)
    ///   |imbalance|          x 3.0  (0.30 = +0.9)
    ///   vpin                 x 5.0  (0.25 baseline = +1.25 -> VOLATILE)
    ///   |vel_ratio - 1|      x 0.6  (2x or 0.5x burst = +0.6)
    ///   mid_drift_bps        x 0.30 (10 bps/s move alone = +3.0 -> CRISIS)
    ///   depth_depletion      x 3.0  (currently 0; reserved)
    fn composite_score(&self, input: &RegimeInput) -> f64 {
        // Crossed-book guard: spread/imbalance are nonsense when the
        // top is inverted; suppress them.
        let (spread_score, imbalance_score) = if input.book_crossed {
            (0.0, 0.0)
        } else {
            (
                (input.spread_blowout_ratio - 1.0).max(0.0),
                input.book_imbalance.abs() * 3.0,
            )
        };
        let vpin_score = input.vpin * 5.0;
        let velocity_score = (input.trade_velocity_ratio - 1.0).abs() * 0.6;
        let drift_score = input.mid_drift_bps * 0.30;
        let depth_score = input.depth_depletion * 3.0;
        let raw = spread_score + imbalance_score + vpin_score + velocity_score + drift_score + depth_score;
        // Freshness dampener: linearly fade as eps drops below 30/s.
        // Treat events_per_sec=0 as "unknown" -> full freshness so
        // upstream that doesn't track eps stays backwards-compatible.
        let freshness = if input.events_per_sec == 0 {
            1.0
        } else {
            ((input.events_per_sec as f64) / 30.0).min(1.0)
        };
        raw * freshness
    }
}
