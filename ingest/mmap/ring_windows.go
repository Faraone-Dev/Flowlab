// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

//go:build windows

// Windows implementation of mmap-backed RingBuffer using Win32
// CreateFileMapping / MapViewOfFile. Layout MUST match ring.go
// byte-for-byte so the Zig/Rust readers see identical headers,
// indices, and payloads.
//
// Concurrency: single-writer / single-reader. Index updates use
// sync/atomic on raw *uint64 pointers into the mapped region; on
// Windows AMD64/ARM64 these are sequentially-consistent and act as
// release/acquire fences for the surrounding payload writes.

package mmap

import (
	"encoding/binary"
	"fmt"
	"os"
	"reflect"
	"sync/atomic"
	"unsafe"

	"golang.org/x/sys/windows"
)

// Layout must match ring.go (Linux/Darwin):
//
//	[0..64)    Header       (magic + capacity)
//	[64..128)  WriteIndex   (own cache line)
//	[128..192) ReadIndex    (own cache line, no false sharing)
//	[192..)    Page payload
const (
	headerSize     = 64
	writeIdxOffset = 64
	readIdxOffset  = 128
	dataOff        = 192
)

type RingBuffer struct {
	file    *os.File
	mapping windows.Handle
	addr    uintptr
	data    []byte
	size    int
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

	hi := uint32(uint64(totalSize) >> 32)
	lo := uint32(uint64(totalSize) & 0xFFFFFFFF)
	mapping, err := windows.CreateFileMapping(
		windows.Handle(f.Fd()),
		nil,
		windows.PAGE_READWRITE,
		hi, lo,
		nil,
	)
	if err != nil {
		f.Close()
		return nil, fmt.Errorf("CreateFileMapping: %w", err)
	}

	addr, err := windows.MapViewOfFile(
		mapping,
		windows.FILE_MAP_READ|windows.FILE_MAP_WRITE,
		0, 0,
		uintptr(totalSize),
	)
	if err != nil {
		windows.CloseHandle(mapping)
		f.Close()
		return nil, fmt.Errorf("MapViewOfFile: %w", err)
	}

	// Build a []byte over the mapped region without copying.
	var data []byte
	sh := (*reflect.SliceHeader)(unsafe.Pointer(&data))
	sh.Data = addr
	sh.Len = totalSize
	sh.Cap = totalSize

	copy(data[0:8], []byte("FLOWRING"))
	binary.LittleEndian.PutUint64(data[8:16], uint64(size))

	return &RingBuffer{
		file:    f,
		mapping: mapping,
		addr:    addr,
		data:    data,
		size:    size,
	}, nil
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

	atomic.StoreUint64(r.writeIndex(), wi+uint64(len(batch)))
	return nil
}

func (r *RingBuffer) Close() error {
	if r.addr != 0 {
		_ = windows.UnmapViewOfFile(r.addr)
		r.addr = 0
		r.data = nil
	}
	if r.mapping != 0 {
		_ = windows.CloseHandle(r.mapping)
		r.mapping = 0
	}
	if r.file != nil {
		return r.file.Close()
	}
	return nil
}
