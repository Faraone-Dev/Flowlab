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
}
