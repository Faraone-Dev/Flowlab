// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//go:build linux || darwin || freebsd

package mmap

import (
	"encoding/binary"
	"fmt"
	"os"
	"sync/atomic"
	"syscall"
	"unsafe"
)

// Layout (cache-line aware):
//   [0..64)    Header        (magic + capacity)
//   [64..128)  WriteIndex    (own cache line — writer side)
//   [128..192) ReadIndex     (own cache line — reader side)
//   [192..)    Page payload  (4KB-aligned)
const (
	headerSize     = 64
	writeIdxOffset = 64  // own cache line
	readIdxOffset  = 128 // own cache line, no false sharing with writer
	dataOff        = 192
)

// RingBuffer is a lock-free single-writer single-reader ring buffer
// backed by a memory-mapped file for zero-copy IPC with Zig/Rust.
type RingBuffer struct {
	file *os.File
	data []byte
	size int
}

func NewRingBuffer(path string, size int) (*RingBuffer, error) {
	f, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0644)
	if err != nil {
		return nil, fmt.Errorf("open: %w", err)
	}

	totalSize := dataOff + size
	if err := f.Truncate(int64(totalSize)); err != nil {
		f.Close()
		return nil, fmt.Errorf("truncate: %w", err)
	}

	data, err := syscall.Mmap(
		int(f.Fd()),
		0,
		totalSize,
		syscall.PROT_READ|syscall.PROT_WRITE,
		syscall.MAP_SHARED,
	)
	if err != nil {
		f.Close()
		return nil, fmt.Errorf("mmap: %w", err)
	}

	copy(data[0:8], []byte("FLOWRING"))
	binary.LittleEndian.PutUint64(data[8:16], uint64(size))

	return &RingBuffer{file: f, data: data, size: size}, nil
}

func (r *RingBuffer) writeIndex() *uint64 {
	return (*uint64)(unsafe.Pointer(&r.data[writeIdxOffset]))
}

func (r *RingBuffer) readIndex() *uint64 {
	return (*uint64)(unsafe.Pointer(&r.data[readIdxOffset]))
}

// Write writes a batch of bytes to the ring buffer.
// Memory-order: payload writes happen-before the writeIndex publication
// thanks to atomic.StoreUint64 acting as a release fence on AMD64/ARM64.
func (r *RingBuffer) Write(batch []byte) error {
	wi := atomic.LoadUint64(r.writeIndex())
	ri := atomic.LoadUint64(r.readIndex())

	available := uint64(r.size) - (wi - ri)
	if uint64(len(batch)) > available {
		return fmt.Errorf("ring buffer full: need %d, available %d", len(batch), available)
	}

	offset := dataOff + int(wi%uint64(r.size))
	firstChunk := r.size - int(wi%uint64(r.size))
	if firstChunk >= len(batch) {
		copy(r.data[offset:], batch)
	} else {
		copy(r.data[offset:], batch[:firstChunk])
		copy(r.data[dataOff:], batch[firstChunk:])
	}

	// Release: payload visible before index advance
	atomic.StoreUint64(r.writeIndex(), wi+uint64(len(batch)))
	return nil
}

func (r *RingBuffer) Close() error {
	if r.data != nil {
		syscall.Munmap(r.data)
	}
	if r.file != nil {
		return r.file.Close()
	}
	return nil
}
