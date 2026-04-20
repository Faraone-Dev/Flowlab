//go:build windows

// Windows mmap support — TODO: implement using
// golang.org/x/sys/windows.{CreateFileMapping, MapViewOfFile}.
// For now, this stub fails fast so callers see a clear error
// instead of cryptic syscall.Mmap link errors.

package mmap

import (
	"errors"
	"os"
)

type RingBuffer struct{}

func NewRingBuffer(path string, size int) (*RingBuffer, error) {
	return nil, errors.New("flowlab: mmap RingBuffer not yet implemented on Windows; use WSL or Linux")
}

func (r *RingBuffer) Write(batch []byte) error { return errors.New("unsupported") }
func (r *RingBuffer) Close() error             { return nil }

var _ = os.Stdout
