// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//! Smoke-test the IchSource against a real BinaryFILE-framed dump.
//!
//! Skipped automatically when `FLOWLAB_ITCH_FILE` is not set so CI on
//! machines without the multi-GB file stays green.

use flowlab_engine::ich::{Framing, IchSource, Pace};
use flowlab_engine::source::Source;
use std::time::{Duration, Instant};

#[test]
fn binary_file_drains_real_events() {
    let path = match std::env::var("FLOWLAB_ITCH_FILE") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!("skip: FLOWLAB_ITCH_FILE not set");
            return;
        }
    };

    let mut src = IchSource::open(&path, Pace::AsFastAsPossible, Framing::BinaryFile)
        .expect("open ITCH file");

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut events: u64 = 0;
    let mut last_ts: u64 = 0;
    let mut monotonic = true;

    while Instant::now() < deadline {
        match src.next() {
            Some(ev) => {
                if ev.ts < last_ts {
                    monotonic = false;
                }
                last_ts = ev.ts;
                events += 1;
            }
            None => break,
        }
    }

    let bytes_total = src.bytes_total();
    let bytes_done = src.bytes_consumed();
    let unknown = src.skipped_unknown();
    let ignored = src.skipped_ignored();
    let pct = (bytes_done as f64 / bytes_total as f64) * 100.0;

    eprintln!(
        "ich smoke: events={events} ignored={ignored} unknown={unknown} \
         bytes={bytes_done}/{bytes_total} ({pct:.2}%) last_ts_ns={last_ts} monotonic={monotonic}"
    );

    assert!(events > 1_000, "expected >1k real events in 3s, got {events}");
    assert!(unknown < events / 100, "too many unknown msgs: {unknown}");
    assert!(monotonic, "ITCH ts48 timestamps must be monotonic");
}
