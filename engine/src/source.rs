//! Source — pluggable event producer.
//!
//! Sync MVP. The trait deliberately has zero async dependencies so it can be
//! swapped under test (replaying a fixed Vec<Event>) and so the engine can be
//! single-threaded by default. Async wrapping (tokio::Stream) is a future
//! adapter, NOT a refactor.
//!
//! Contract:
//!   - `next` returns `None` only at terminal end-of-stream.
//!   - `next` MUST NOT block indefinitely. Live sources poll/spin internally
//!     and return `None` from `try_next`-style implementations only when truly
//!     exhausted; a temporary "no event yet" state is signalled by returning
//!     `None` *and* `is_live() == true`.

use flowlab_core::event::Event;

pub trait Source: Send {
    /// Human-readable name (used in logs and the telemetry header frame).
    fn name(&self) -> &str;

    /// Pull the next event. `None` means either:
    ///   - end of stream (replay sources), if `is_live() == false`
    ///   - no event currently available (live sources), if `is_live() == true`
    fn next(&mut self) -> Option<Event>;

    /// Whether the source represents a live feed (true) or finite replay (false).
    fn is_live(&self) -> bool {
        false
    }

    /// Optional lookup: resolve an instrument_id to its human-readable
    /// ticker (e.g. stock_locate 8251 → "AAPL"). Default `None` for sources
    /// that don't carry a symbol directory.
    fn symbol_of(&self, _instrument_id: u32) -> Option<&str> {
        None
    }
}
