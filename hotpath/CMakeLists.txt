cmake_minimum_required(VERSION 3.20)
project(flowlab-hotpath LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# Release flags: aggressive optimization for hot path.
# MSVC uses a different flag syntax than GCC/Clang; pick the right set.
if(MSVC)
    # /O2        : full speed optimization
    # /Oi        : enable intrinsics
    # /GL        : whole-program optimization (link-time codegen)
    # /EHs-c-    : disable C++ exception handling (equivalent to -fno-exceptions)
    # /GR-       : disable RTTI (equivalent to -fno-rtti)
    # /DNDEBUG   : release define
    # /arch:AVX2 : portable SIMD target (no /arch:native on MSVC)
    set(CMAKE_CXX_FLAGS_RELEASE "/O2 /Oi /GL /EHs-c- /GR- /DNDEBUG /arch:AVX2")
    add_compile_options(/W3 /permissive-)
else()
    set(CMAKE_CXX_FLAGS_RELEASE "-O3 -march=native -DNDEBUG -fno-exceptions -fno-rtti")
endif()

add_library(flowlab_hotpath STATIC
    src/orderbook.cpp
    src/hasher.cpp
    src/stats.cpp
)

target_include_directories(flowlab_hotpath PUBLIC include)

# C ABI export for Rust FFI
add_library(flowlab_hotpath_ffi STATIC
    src/ffi.cpp
)
target_link_libraries(flowlab_hotpath_ffi PRIVATE flowlab_hotpath)
target_include_directories(flowlab_hotpath_ffi PUBLIC include)
