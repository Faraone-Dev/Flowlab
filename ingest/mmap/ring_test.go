// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

package mmap

import (
	"crypto/sha256"
	"encoding/binary"
	"hash"
	"path/filepath"
	"sync/atomic"
	"testing"
)

// readBatch is the consumer-side counterpart of RingBuffer.Write,
// used only by the integration tests in this file. Production
// consumers live in Zig/Rust and read the same layout directly off
// the mmap, so this helper deliberately mirrors that path:
//  1. acquire-load the writer index
//  2. compute available bytes
//  3. copy out (handling wrap)
//  4. release-store the reader index
//
// Returning an empty slice means "no new data" — not an error.
func (r *RingBuffer) readBatch(max int) []byte {
	wi := atomic.LoadUint64(r.writeIndex())
	ri := atomic.LoadUint64(r.readIndex())

	avail := int(wi - ri)
	if avail <= 0 {
		return nil
	}
	if avail > max {
		avail = max
	}

	out := make([]byte, avail)
	off := dataOff + int(ri%uint64(r.size))
	first := r.size - int(ri%uint64(r.size))
	if first >= avail {
		copy(out, r.data[off:off+avail])
	} else {
		copy(out[:first], r.data[off:off+first])
		copy(out[first:], r.data[dataOff:dataOff+(avail-first)])
	}

	atomic.StoreUint64(r.readIndex(), ri+uint64(avail))
	return out
}

// TestRingProduceConsumeHash is the missing IPC integration test.
// It pushes N deterministic frames through the ring, hashes the
// producer-side bytes inline, drains them via readBatch in
// arbitrarily sized reads, and asserts the consumer-side hash
// matches. If the layout, atomic ordering, or wrap math regresses,
// the hashes diverge and this test fails loud instead of silently
// corrupting data on the wire.
func TestRingProduceConsumeHash(t *testing.T) {
	path := filepath.Join(t.TempDir(), "ring")
	const ringSize = 4096 // small on purpose: forces wrap-around
	const frames = 5_000  // >> ring capacity, must wrap many times
	const frameSize = 37  // odd size to dodge alignment luck

	r, err := NewRingBuffer(path, ringSize)
	if err != nil {
		t.Fatalf("NewRingBuffer: %v", err)
	}
	t.Cleanup(func() { _ = r.Close() })

	prodHash := sha256.New()
	consHash := sha256.New()

	produced := 0
	consumed := 0
	frame := make([]byte, frameSize)

	for produced < frames {
		// Build a deterministic payload: little-endian frame index
		// followed by filler bytes (i*7 mod 256). Anything content-
		// addressable works — the point is hash equality.
		binary.LittleEndian.PutUint64(frame[0:8], uint64(produced))
		for i := 8; i < frameSize; i++ {
			frame[i] = byte((produced*7 + i) & 0xff)
		}

		if err := r.Write(frame); err != nil {
			// Ring full → drain some, then retry. Models the real
			// SPSC backpressure pattern.
			drainOne(t, r, consHash, &consumed)
			continue
		}
		prodHash.Write(frame)
		produced++

		// Periodically drain so the writer doesn't permanently stall
		// the producer loop. Picked to force many wraps.
		if produced%16 == 0 {
			drainOne(t, r, consHash, &consumed)
		}
	}

	// Final drain.
	for consumed < produced*frameSize {
		drainOne(t, r, consHash, &consumed)
	}

	got := consHash.Sum(nil)
	want := prodHash.Sum(nil)
	if string(got) != string(want) {
		t.Fatalf("hash mismatch: producer=%x consumer=%x", want, got)
	}
	if consumed != produced*frameSize {
		t.Fatalf("byte count mismatch: produced=%d consumed=%d",
			produced*frameSize, consumed)
	}
}

func drainOne(t *testing.T, r *RingBuffer, h hash.Hash, consumed *int) {
	t.Helper()
	// Read at most half the ring to exercise multiple drains per
	// fill cycle and the wrap path on both sides.
	chunk := r.readBatch(2048)
	if len(chunk) == 0 {
		return
	}
	h.Write(chunk)
	*consumed += len(chunk)
}

// TestRingRejectsOversizedWrite locks in the documented "ring full"
// behaviour: writes larger than free capacity must error, never
// silently truncate or overwrite unread bytes.
func TestRingRejectsOversizedWrite(t *testing.T) {
	path := filepath.Join(t.TempDir(), "ring")
	r, err := NewRingBuffer(path, 256)
	if err != nil {
		t.Fatalf("NewRingBuffer: %v", err)
	}
	t.Cleanup(func() { _ = r.Close() })

	if err := r.Write(make([]byte, 257)); err == nil {
		t.Fatalf("expected error writing 257 bytes into 256-byte ring")
	}
}
