/// Market regime classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Regime {
    Calm = 0,
    Volatile = 1,
    Aggressive = 2,
    Crisis = 3,
}

/// Input metrics for regime classification.
#[derive(Debug, Clone, Copy)]
pub struct RegimeInput {
    pub spread_blowout_ratio: f64,
    pub book_imbalance: f64,
    pub vpin: f64,
    pub trade_velocity_ratio: f64,
    pub depth_depletion: f64,
}

/// Threshold-based regime classifier.
pub struct RegimeClassifier {
    pub volatile_threshold: f64,
    pub aggressive_threshold: f64,
    pub crisis_threshold: f64,
}

impl Default for RegimeClassifier {
    fn default() -> Self {
        Self {
            volatile_threshold: 1.5,
            aggressive_threshold: 3.0,
            crisis_threshold: 6.0,
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
    fn composite_score(&self, input: &RegimeInput) -> f64 {
        let spread_score = (input.spread_blowout_ratio - 1.0).max(0.0);
        let imbalance_score = input.book_imbalance.abs() * 2.0;
        let vpin_score = input.vpin * 5.0;
        let velocity_score = (input.trade_velocity_ratio - 1.0).max(0.0);
        let depth_score = input.depth_depletion * 3.0;

        spread_score + imbalance_score + vpin_score + velocity_score + depth_score
    }
}
