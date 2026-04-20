//! Build script for the `native` feature.
//!
//! Builds the C++ hot path in-tree via the `cc` crate (which auto-detects
//! MSVC / clang / gcc / zig-cc) and links the pre-built Zig static library
//! produced by `zig build -Doptimize=ReleaseFast`.
//!
//! No external CMake step is required.

use std::env;
use std::path::PathBuf;

fn main() {
    if env::var_os("CARGO_FEATURE_NATIVE").is_none() {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest.parent().unwrap().to_path_buf();

    // ─── C++ hotpath — compile in-tree via cc-rs ─────────────────────
    let hotpath = workspace.join("hotpath");
    let src = hotpath.join("src");
    let inc = hotpath.join("include");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .include(&inc)
        .file(src.join("orderbook.cpp"))
        .file(src.join("hasher.cpp"))
        .file(src.join("stats.cpp"))
        .file(src.join("ffi.cpp"));

    let compiler = build.get_compiler();
    if compiler.is_like_msvc() {
        build
            .flag("/O2")
            .flag("/Oi")
            .flag("/EHs-c-")
            .flag("/GR-")
            .flag("/permissive-")
            .define("NDEBUG", None);
        build.flag_if_supported("/arch:AVX2");
    } else {
        build
            .flag("-O3")
            .flag("-fno-exceptions")
            .flag("-fno-rtti")
            .define("NDEBUG", None);
        build.flag_if_supported("-march=native");
    }

    build.compile("flowlab_hotpath_ffi");

    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-lib=dylib=stdc++");
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-lib=dylib=c++");

    for entry in std::fs::read_dir(&src).expect("read hotpath/src") {
        let p = entry.unwrap().path();
        println!("cargo:rerun-if-changed={}", p.display());
    }
    for entry in walk(&inc) {
        println!("cargo:rerun-if-changed={}", entry.display());
    }

    // ─── Zig feed-parser — link pre-built static lib ─────────────────
    let zig_lib_dir = workspace.join("feed-parser").join("zig-out").join("lib");
    let zig_lib = zig_lib_dir.join(static_lib_name("flowlab_feed_parser"));

    if !zig_lib.exists() {
        panic!(
            "native feature enabled but Zig feed-parser lib not found.\n\
             Expected: {}\n\
             Run: cd feed-parser && zig build -Doptimize=ReleaseFast",
            zig_lib.display()
        );
    }

    println!("cargo:rustc-link-search=native={}", zig_lib_dir.display());
    println!("cargo:rustc-link-lib=static=flowlab_feed_parser");
    println!("cargo:rerun-if-changed={}", zig_lib.display());

    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-lib=dylib=ntdll");
        println!("cargo:rustc-link-lib=dylib=kernel32");
    }
}

#[cfg(target_os = "windows")]
fn static_lib_name(name: &str) -> String {
    format!("{name}.lib")
}

#[cfg(not(target_os = "windows"))]
fn static_lib_name(name: &str) -> String {
    format!("lib{name}.a")
}

fn walk(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}
