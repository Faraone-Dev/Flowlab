//! Foreign Function Interface — bindings to C++ hotpath and Zig feed-parser.
//!
//! These declarations match the C ABI exports from:
//!   - `hotpath/include/flowlab/ffi.h`   (C++ SIMD order book + hasher)
//!   - `feed-parser/src/main.zig`        (comptime ITCH parser)
//!
//! Linking is handled by build.rs which finds the static libraries
//! produced by CMake (hotpath) and Zig (feed-parser).

use crate::event::Event;

// ─── C++ Hotpath ──────────────────────────────────────────────────────

unsafe extern "C" {
    /// Create a new C++ order book. Returns opaque handle.
    pub fn flowlab_orderbook_new() -> *mut core::ffi::c_void;

    /// Destroy a C++ order book.
    pub fn flowlab_orderbook_free(book: *mut core::ffi::c_void);

    /// Apply a raw 40-byte event to the C++ book.
    pub fn flowlab_orderbook_apply(book: *mut core::ffi::c_void, event_data: *const u8);

    /// Get best bid price from C++ book.
    pub fn flowlab_orderbook_best_bid(book: *const core::ffi::c_void) -> u64;

    /// Get best ask price from C++ book.
    pub fn flowlab_orderbook_best_ask(book: *const core::ffi::c_void) -> u64;

    /// Get spread from C++ book.
    pub fn flowlab_orderbook_spread(book: *const core::ffi::c_void) -> u64;

    /// Compute state hash of current C++ book.
    pub fn flowlab_orderbook_hash(book: *const core::ffi::c_void) -> u64;

    /// Create a new C++ state hasher.
    pub fn flowlab_hasher_new() -> *mut core::ffi::c_void;

    /// Free a C++ state hasher.
    pub fn flowlab_hasher_free(hasher: *mut core::ffi::c_void);

    /// Update C++ hasher with data.
    pub fn flowlab_hasher_update(hasher: *mut core::ffi::c_void, data: *const u8, len: u64);

    /// Get current hash digest from C++ hasher.
    pub fn flowlab_hasher_digest(hasher: *const core::ffi::c_void) -> u64;
}

// ─── Zig Feed Parser ─────────────────────────────────────────────────

unsafe extern "C" {
    /// Parse a raw ITCH 5.0 buffer into canonical events.
    /// Returns number of events written, or -1 on error.
    pub fn flowlab_parse_itch(
        raw: *const u8,
        raw_len: usize,
        out: *mut Event,
        out_cap: usize,
    ) -> i64;

    /// Parse a length-framed ITCH 5.0 stream. Each message is prefixed
    /// by a u16 big-endian length. Returns number of events written, or -1 on error.
    pub fn flowlab_parse_itch_framed(
        raw: *const u8,
        raw_len: usize,
        out: *mut Event,
        out_cap: usize,
    ) -> i64;

    /// Get the size of the canonical Event struct (cross-language validation).
    pub fn flowlab_event_size() -> usize;
}

// ─── Safe Wrappers ───────────────────────────────────────────────────

/// Safe wrapper around the C++ SIMD order book.
pub struct CppOrderBook {
    handle: *mut core::ffi::c_void,
}

// SAFETY: The C++ order book is single-threaded by design.
// The caller must ensure no concurrent access.
unsafe impl Send for CppOrderBook {}

impl CppOrderBook {
    /// Create a new C++ order book instance.
    pub fn new() -> Self {
        let handle = unsafe { flowlab_orderbook_new() };
        assert!(!handle.is_null(), "C++ orderbook allocation failed");
        Self { handle }
    }

    /// Apply a canonical event to the C++ book.
    pub fn apply(&mut self, event: &Event) {
        let ptr = event as *const Event as *const u8;
        unsafe { flowlab_orderbook_apply(self.handle, ptr) };
    }

    pub fn best_bid(&self) -> u64 {
        unsafe { flowlab_orderbook_best_bid(self.handle) }
    }

    pub fn best_ask(&self) -> u64 {
        unsafe { flowlab_orderbook_best_ask(self.handle) }
    }

    pub fn spread(&self) -> u64 {
        unsafe { flowlab_orderbook_spread(self.handle) }
    }

    /// State hash — used for cross-language verification.
    pub fn state_hash(&self) -> u64 {
        unsafe { flowlab_orderbook_hash(self.handle) }
    }
}

impl Drop for CppOrderBook {
    fn drop(&mut self) {
        unsafe { flowlab_orderbook_free(self.handle) };
    }
}

/// Parse raw ITCH 5.0 bytes using the Zig comptime parser.
/// Returns a Vec of canonical events.
pub fn parse_itch(raw: &[u8]) -> Result<Vec<Event>, &'static str> {
    // Validate struct size agreement across languages
    let zig_size = unsafe { flowlab_event_size() };
    assert_eq!(
        zig_size,
        core::mem::size_of::<Event>(),
        "Event size mismatch: Zig={zig_size} Rust={}",
        core::mem::size_of::<Event>()
    );

    // Pre-allocate conservative upper bound
    let max_events = raw.len() / 20; // smallest ITCH message ~20 bytes
    let mut out = vec![Event::zeroed(); max_events];

    let n = unsafe {
        flowlab_parse_itch(raw.as_ptr(), raw.len(), out.as_mut_ptr(), out.len())
    };

    if n < 0 {
        return Err("Zig ITCH parser returned error");
    }

    out.truncate(n as usize);
    Ok(out)
}

impl Event {
    /// Create a zeroed event (for output buffer initialization).
    fn zeroed() -> Self {
        unsafe { core::mem::zeroed() }
    }
}
