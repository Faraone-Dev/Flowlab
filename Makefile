.PHONY: all rust cpp zig go clean test bench

# ─── FLOWLAB unified build ─────────────────────────────────────────

all: rust cpp zig go

# ─── Rust ──────────────────────────────────────────────────────────

rust:
	cargo build --release

rust-debug:
	cargo build

test-rust:
	cargo test --workspace

# ─── C++ Hotpath ───────────────────────────────────────────────────

cpp:
	cmake -S hotpath -B hotpath/build -DCMAKE_BUILD_TYPE=Release
	cmake --build hotpath/build

cpp-debug:
	cmake -S hotpath -B hotpath/build -DCMAKE_BUILD_TYPE=Debug
	cmake --build hotpath/build

# ─── Zig Feed Parser ──────────────────────────────────────────────

zig:
	cd feed-parser && zig build -Doptimize=ReleaseFast

zig-debug:
	cd feed-parser && zig build

test-zig:
	cd feed-parser && zig build test

# ─── Go Services ──────────────────────────────────────────────────

go:
	cd ingest && go build -o ../target/flowlab-ingest ./cmd/ingest
	cd api && go build -o ../target/flowlab-api ./cmd/api

test-go:
	cd ingest && go test ./...
	cd api && go test ./...

# ─── Test All ─────────────────────────────────────────────────────

test: test-rust test-zig test-go

# ─── Benchmarks ───────────────────────────────────────────────────

bench:
	cargo bench

bench-replay:
	cargo bench --bench replay

# ─── Clean ────────────────────────────────────────────────────────

ifeq ($(OS),Windows_NT)
RM_RF = powershell -NoProfile -Command "Remove-Item -Recurse -Force"
RM_F  = powershell -NoProfile -Command "Remove-Item -Force -ErrorAction SilentlyContinue"
else
RM_RF = rm -rf
RM_F  = rm -f
endif

clean:
	cargo clean
	-$(RM_RF) hotpath/build
	-$(RM_RF) feed-parser/zig-out
	-$(RM_RF) feed-parser/zig-cache
	-$(RM_F) target/flowlab-ingest target/flowlab-api
