use flowlab_core::event::SequencedEvent;

use crate::generators::{
    CancellationStormGenerator, FlashCrashGenerator, LatencyArbProxyGenerator,
    MomentumIgnitionGenerator, PhantomLiquidityGenerator, StormGenerator,
};
use crate::ChaosKind;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StormSpec {
    pub kind: ChaosKind,
    pub severity: f32,
    pub duration_ns: u64,
    pub seed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StormMode {
    Idle,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StormSnapshot {
    pub mode: StormMode,
    pub kind: Option<ChaosKind>,
    pub severity: f32,
    pub seed: u64,
    pub started_at_ns: u64,
    pub expires_at_ns: u64,
    pub restarts: u32,
}

impl StormSnapshot {
    pub fn idle() -> Self {
        Self {
            mode: StormMode::Idle,
            kind: None,
            severity: 0.0,
            seed: 0,
            started_at_ns: 0,
            expires_at_ns: 0,
            restarts: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.mode == StormMode::Active
    }
}

pub struct StormOrchestrator {
    base_price: u64,
    active: Option<ActiveStorm>,
}

struct ActiveStorm {
    spec: StormSpec,
    started_at_ns: u64,
    expires_at_ns: u64,
    restarts: u32,
    generator: Box<dyn StormGenerator>,
}

impl StormOrchestrator {
    pub fn new(base_price: u64) -> Self {
        Self {
            base_price,
            active: None,
        }
    }

    pub fn start(&mut self, spec: StormSpec, now_ns: u64) -> StormSnapshot {
        let spec = StormSpec {
            severity: spec.severity.clamp(0.0, 1.0),
            duration_ns: spec.duration_ns.max(1),
            ..spec
        };
        let expires_at_ns = now_ns.saturating_add(spec.duration_ns);
        let generator = spawn_generator(spec, self.base_price, 0);
        self.active = Some(ActiveStorm {
            spec,
            started_at_ns: now_ns,
            expires_at_ns,
            restarts: 0,
            generator,
        });
        self.snapshot()
    }

    pub fn stop(&mut self) {
        self.active = None;
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub fn next_due_ns(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.generator.next_due_ns())
    }

    pub fn snapshot(&self) -> StormSnapshot {
        match &self.active {
            Some(active) => StormSnapshot {
                mode: StormMode::Active,
                kind: Some(active.spec.kind),
                severity: active.spec.severity,
                seed: active.spec.seed,
                started_at_ns: active.started_at_ns,
                expires_at_ns: active.expires_at_ns,
                restarts: active.restarts,
            },
            None => StormSnapshot::idle(),
        }
    }

    pub fn poll(&mut self, now_ns: u64, base_seq: u64) -> Vec<SequencedEvent> {
        if self.active.is_none() {
            return Vec::new();
        }

        if self.has_expired(now_ns) {
            self.stop();
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut next_seq = base_seq;
        let mut steps = 0;

        loop {
            let Some(active) = self.active.as_mut() else {
                break;
            };

            if active.generator.finished() {
                active.restarts = active.restarts.saturating_add(1);
                active.generator = spawn_generator(active.spec, self.base_price, active.restarts);
                continue;
            }

            if active.generator.next_due_ns() > now_ns {
                break;
            }

            let batch = active.generator.step(now_ns, next_seq);
            if let Some(max_seq) = batch.iter().map(|ev| ev.seq).max() {
                next_seq = max_seq.saturating_add(1);
            }
            out.extend(batch);
            steps += 1;

            if steps >= 32 {
                break;
            }
        }

        if self.has_expired(now_ns) {
            self.stop();
        }
        out
    }

    fn has_expired(&self, now_ns: u64) -> bool {
        self.active
            .as_ref()
            .map(|active| now_ns >= active.expires_at_ns)
            .unwrap_or(false)
    }
}

fn spawn_generator(spec: StormSpec, base_price: u64, restart: u32) -> Box<dyn StormGenerator> {
    let seed = spec.seed.wrapping_add(restart as u64);
    match spec.kind {
        ChaosKind::PhantomLiquidity => Box::new(PhantomLiquidityGenerator::new(
            seed,
            spec.severity,
            base_price,
            cycles_from_duration(spec.duration_ns, 5_000_000, 4),
        )),
        ChaosKind::CancellationStorm => {
            Box::new(CancellationStormGenerator::new(seed, spec.severity, base_price))
        }
        ChaosKind::MomentumIgnition => Box::new(MomentumIgnitionGenerator::new(
            seed,
            spec.severity,
            base_price,
            true,
        )),
        ChaosKind::FlashCrash => Box::new(FlashCrashGenerator::new(
            seed,
            spec.severity,
            base_price,
            true,
            cycles_from_duration(spec.duration_ns, 80_000_000, 1),
        )),
        ChaosKind::LatencyArbitrage => Box::new(LatencyArbProxyGenerator::new(
            seed,
            spec.severity,
            base_price,
            cycles_from_duration(spec.duration_ns, 40_000_000, 1),
        )),
        ChaosKind::QuoteStuff | ChaosKind::Spoof => Box::new(PhantomLiquidityGenerator::new(
            seed,
            spec.severity,
            base_price,
            cycles_from_duration(spec.duration_ns, 5_000_000, 4),
        )),
    }
}

fn cycles_from_duration(duration_ns: u64, period_ns: u64, minimum: u32) -> u32 {
    let cycles = duration_ns
        .saturating_add(period_ns.saturating_sub(1))
        / period_ns.max(1);
    minimum.max(cycles.min(u32::MAX as u64) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_orchestrator_emits_nothing() {
        let mut orch = StormOrchestrator::new(100_000);
        let out = orch.poll(1_000_000, 1);
        assert!(out.is_empty());
        assert_eq!(orch.snapshot(), StormSnapshot::idle());
    }

    #[test]
    fn start_and_poll_emit_monotone_sequences() {
        let mut orch = StormOrchestrator::new(100_000);
        orch.start(
            StormSpec {
                kind: ChaosKind::PhantomLiquidity,
                severity: 0.8,
                duration_ns: 50_000_000,
                seed: 0xDEAD_BEEF,
            },
            1_000_000,
        );

        let batch = orch.poll(1_000_000, 100);
        assert!(!batch.is_empty());
        assert!(orch.is_active());
        assert_eq!(batch.first().unwrap().seq, 100);
        for pair in batch.windows(2) {
            assert!(pair[0].seq < pair[1].seq);
        }
    }

    #[test]
    fn expiry_stops_active_storm() {
        let mut orch = StormOrchestrator::new(100_000);
        orch.start(
            StormSpec {
                kind: ChaosKind::FlashCrash,
                severity: 0.9,
                duration_ns: 10_000_000,
                seed: 7,
            },
            1_000_000,
        );
        let _ = orch.poll(1_000_000, 1);
        let after = orch.poll(20_000_000, 50);
        assert!(after.is_empty());
        assert!(!orch.is_active());
    }

    #[test]
    fn restart_keeps_storm_alive_until_ttl() {
        let mut orch = StormOrchestrator::new(100_000);
        orch.start(
            StormSpec {
                kind: ChaosKind::CancellationStorm,
                severity: 0.0,
                duration_ns: 400_000_000,
                seed: 42,
            },
            1_000_000,
        );

        let mut total = 0usize;
        for step in 0..70u64 {
            let now_ns = 1_000_000 + step * 5_000_000;
            total += orch.poll(now_ns, 1 + total as u64).len();
        }
        let snap = orch.snapshot();
        assert!(total > 0);
        assert!(snap.is_active());
        assert!(snap.restarts > 0);
    }
}