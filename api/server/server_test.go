// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

package server

import (
	"net/http/httptest"
	"testing"
)

func TestIsAllowedOrigin(t *testing.T) {
	req := httptest.NewRequest("GET", "http://api.example.test/status", nil)
	req.Host = "api.example.test"

	tests := []struct {
		name   string
		origin string
		want   bool
	}{
		{name: "same origin", origin: "http://api.example.test", want: true},
		{name: "loopback localhost", origin: "http://localhost:5173", want: true},
		{name: "loopback ip", origin: "http://127.0.0.1:3000", want: true},
		{name: "foreign origin", origin: "https://evil.example", want: false},
		{name: "bad origin", origin: "://bad", want: false},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			if got := isAllowedOrigin(req, tc.origin); got != tc.want {
				t.Fatalf("isAllowedOrigin(%q) = %v, want %v", tc.origin, got, tc.want)
			}
		})
	}
}

func TestIsSafeRunID(t *testing.T) {
	tests := []struct {
		id   string
		want bool
	}{
		{id: "2026-04-23T10-11-12Z", want: true},
		{id: "../secret", want: false},
		{id: "2026-04-23T10:11:12Z", want: false},
		{id: "2026-04-23T10-11-12Z\\..\\x", want: false},
	}

	for _, tc := range tests {
		if got := isSafeRunID(tc.id); got != tc.want {
			t.Fatalf("isSafeRunID(%q) = %v, want %v", tc.id, got, tc.want)
		}
	}
}
