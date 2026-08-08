# Cross-compile to wasm32 with clang.
#
#   cmake -S sdk/wasm -B build/wasm \
#         -DCMAKE_TOOLCHAIN_FILE=$PWD/sdk/wasm/cmake/wasm32-toolchain.cmake
#
# Any clang can target wasm32; no wasi-sdk is needed, because a tocat guest
# imports nothing and so has no WASI to link against.

set(CMAKE_SYSTEM_NAME Generic)
set(CMAKE_SYSTEM_PROCESSOR wasm32)

find_program(TOCAT_WASM_CC NAMES clang)
find_program(TOCAT_WASM_CXX NAMES clang++)

if(NOT TOCAT_WASM_CC)
  message(FATAL_ERROR "clang is required to build WebAssembly guests")
endif()

set(CMAKE_C_COMPILER "${TOCAT_WASM_CC}")
set(CMAKE_C_COMPILER_TARGET wasm32)

if(TOCAT_WASM_CXX)
  set(CMAKE_CXX_COMPILER "${TOCAT_WASM_CXX}")
  set(CMAKE_CXX_COMPILER_TARGET wasm32)
endif()

# CMake proves a compiler works by linking a small executable, which for this
# target needs -Wl,--no-entry and a freestanding source. Rather than teach the
# check about that, skip it: a compiler that cannot produce a guest fails
# loudly on the first real target.
set(CMAKE_C_COMPILER_WORKS TRUE)
set(CMAKE_CXX_COMPILER_WORKS TRUE)

set(CMAKE_EXECUTABLE_SUFFIX ".wasm")
set(CMAKE_EXECUTABLE_SUFFIX_C ".wasm")
set(CMAKE_EXECUTABLE_SUFFIX_CXX ".wasm")

# Nothing on the host is linkable into a guest, so look for programs there and
# for everything else nowhere.
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
