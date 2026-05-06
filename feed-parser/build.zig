// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Ivan Piardi (Faraone-Dev)

const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    // Static library for Rust FFI (C ABI)
    const lib = b.addStaticLibrary(.{
        .name = "flowlab_feed_parser",
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
    });
    // Do NOT bundle compiler_rt: when the resulting static lib is linked
    // by MSVC `link.exe` (via Rust's cc-rs / msvc toolchain) the bundled
    // compiler_rt objects produce a "LNK1143: invalid COMDAT symbol"
    // because they were compiled with the Zig (LLVM) linker. The Rust
    // toolchain already ships its own compiler_rt, so we rely on that.
    lib.bundle_compiler_rt = false;

    b.installArtifact(lib);

    // Tests
    const tests = b.addTest(.{
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
    });

    const run_tests = b.addRunArtifact(tests);
    const test_step = b.step("test", "Run feed parser tests");
    test_step.dependOn(&run_tests.step);

    // Dedicated fuzz step — same compile unit as `test` (fuzz tests are
    // imported by `main.zig`) but exposed as a separate target so CI
    // can gate on it explicitly. Forces ReleaseFast so 200k+ iterations
    // per property complete in well under a second.
    const fuzz_tests = b.addTest(.{
        .root_source_file = b.path("src/fuzz.zig"),
        .target = target,
        .optimize = .ReleaseFast,
        .filter = "fuzz",
    });
    const run_fuzz = b.addRunArtifact(fuzz_tests);
    const fuzz_step = b.step("fuzz", "Run ITCH parser fuzz harness (property-based, seeded)");
    fuzz_step.dependOn(&run_fuzz.step);
}
