// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

package server

import "testing"

func TestIsSupportedWireVersion(t *testing.T) {
	tests := []struct {
		name string
		ver  uint16
		want bool
	}{
		{name: "v1 compatibility", ver: 1, want: true},
		{name: "current v2", ver: 2, want: true},
		{name: "too old", ver: 0, want: false},
		{name: "future unsupported", ver: 3, want: false},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if got := isSupportedWireVersion(tc.ver); got != tc.want {
				t.Fatalf("isSupportedWireVersion(%d) = %v, want %v", tc.ver, got, tc.want)
			}
		})
	}
}
