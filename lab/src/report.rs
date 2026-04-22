use flowlab_chaos::{ChaosEvent, ChaosKind};

/// Strategy stress test report.
///
/// `chaos_events` and `chaos_counts` are populated only when the
/// executor was built with a `ChaosChain`. They are otherwise empty
/// / zero, so consumers that ignore chaos still see the original
/// shape of the report.
#[derive(Debug, Clone)]
pub struct StressReport {
    pub strategy_name: String,
    pub events_processed: u64,
    pub final_pnl: f64,
    pub max_drawdown: f64,
    pub final_inventory: i64,
    pub fills: u64,

    /// All chaos flags emitted during the run, in stream order. Empty
    /// when no chain was attached.
    pub chaos_events: Vec<ChaosEvent>,
    /// Per-kind cumulative flag counts. Order matches the chain's
    /// fixed kind order so reports across runs are diff-able.
    pub chaos_counts: Vec<(ChaosKind, u64)>,
}

impl std::fmt::Display for StressReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== {} ===", self.strategy_name)?;
        writeln!(f, "Events:    {}", self.events_processed)?;
        writeln!(f, "PnL:       {:.2}", self.final_pnl)?;
        writeln!(f, "Drawdown:  {:.2}", self.max_drawdown)?;
        writeln!(f, "Inventory: {}", self.final_inventory)?;
        writeln!(f, "Fills:     {}", self.fills)?;
        if !self.chaos_counts.is_empty() {
            let total: u64 = self.chaos_counts.iter().map(|(_, c)| c).sum();
            writeln!(f, "Chaos:     {} flags", total)?;
            for (kind, c) in &self.chaos_counts {
                if *c > 0 {
                    writeln!(f, "  {:>20?}: {}", kind, c)?;
                }
            }
        }
        Ok(())
    }
}
