/// Strategy stress test report.
#[derive(Debug, Clone)]
pub struct StressReport {
    pub strategy_name: String,
    pub events_processed: u64,
    pub final_pnl: f64,
    pub max_drawdown: f64,
    pub final_inventory: i64,
    pub fills: u64,
}

impl std::fmt::Display for StressReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "=== {} ===", self.strategy_name)?;
        writeln!(f, "Events:    {}", self.events_processed)?;
        writeln!(f, "PnL:       {:.2}", self.final_pnl)?;
        writeln!(f, "Drawdown:  {:.2}", self.max_drawdown)?;
        writeln!(f, "Inventory: {}", self.final_inventory)?;
        writeln!(f, "Fills:     {}", self.fills)
    }
}
