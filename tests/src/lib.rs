//! Shared helpers for end-to-end and fuzz harnesses.
//!
//! Kept intentionally tiny: the signal layer must read like the
//! README, not like a framework.

use std::path::PathBuf;

/// Unique tmp file path scoped to this test process.
pub fn tmp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "flowlab-e2e-{}-{}-{}.bin",
        tag,
        std::process::id(),
        next_id()
    ));
    p
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// Cheap deterministic PRNG (xorshift64*). Fuzz harnesses use this
/// to derive every input from a single seed, so a failure can be
/// reproduced bit-for-bit by replaying the seed.
#[derive(Debug, Clone, Copy)]
pub struct XorShift64(pub u64);

impl XorShift64 {
    pub fn new(seed: u64) -> Self {
        // Avoid the trivial fixed point at 0.
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn next_in(&mut self, lo: u64, hi: u64) -> u64 {
        debug_assert!(hi > lo);
        lo + (self.next_u64() % (hi - lo))
    }
}
