# The tocat WebAssembly guest SDK.
#
# Installs the guest ABI header and a CMake package that gives a downstream
# project one function, tocat_add_wasm_guest(), and a toolchain file for
# targeting wasm32. The guests under examples/ are built with the same
# function a consumer would use, so the example and the supported path cannot
# drift apart.
#
#   cmake -S sdk/wasm -B build/wasm \
#         -DCMAKE_TOOLCHAIN_FILE=$PWD/sdk/wasm/cmake/wasm32-toolchain.cmake
#   cmake --build build/wasm
#
# Configuring without that toolchain file is supported and installs the
# headers and the package: a consumer cross-compiling their own guests does
# not need a wasm compiler present when the SDK is installed.

cmake_minimum_required(VERSION 3.21)

# The version comes from the workspace, so the SDK and the binary that loads
# its guests cannot claim different ones. Falls back when the SDK has been
# copied out of the repository on its own.
set(TOCAT_WASM_VERSION 0.0.0)

if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/../../Cargo.toml")
  file(READ "${CMAKE_CURRENT_SOURCE_DIR}/../../Cargo.toml" tocat_cargo_toml)

  if(tocat_cargo_toml MATCHES "version = \"([0-9]+\\.[0-9]+\\.[0-9]+)\"")
    set(TOCAT_WASM_VERSION "${CMAKE_MATCH_1}")
  endif()
endif()

# LANGUAGES NONE so that installing the SDK needs no compiler at all. The
# examples enable C and C++ below, once they are known to be wanted.
project(
  TocatWasm
  VERSION ${TOCAT_WASM_VERSION}
  DESCRIPTION "Guest SDK for tocat WebAssembly plugins"
  HOMEPAGE_URL "https://github.com/lkalir/tocat"
  LANGUAGES NONE)

# Everything installed here is architecture independent (one header and a few
# CMake scripts), so it belongs in lib rather than lib64. Saying so before
# GNUInstallDirs also spares it guessing from a language this project does not
# enable.
set(CMAKE_INSTALL_LIBDIR
    lib
    CACHE PATH "Object code libraries")

include(GNUInstallDirs)
include(CMakePackageConfigHelpers)
include(cmake/TocatWasmGuest.cmake)

# The ABI version is declared by the generated header, since the wire format
# is generated from crates/tocat-abi rather than written here. Reading it back
# out means the package can expose it without a second place to update.
set(TOCAT_WASM_ABI_HEADER "${CMAKE_CURRENT_SOURCE_DIR}/include/tocat/abi.h")

if(NOT EXISTS "${TOCAT_WASM_ABI_HEADER}")
  message(
    FATAL_ERROR
      "${TOCAT_WASM_ABI_HEADER} is missing. It is generated from "
      "crates/tocat-abi and committed; regenerate it with\n"
      "  cargo run -p tocat-abi --features generate --bin tocat-abi-header")
endif()

file(READ "${TOCAT_WASM_ABI_HEADER}" tocat_abi_header)

if(NOT tocat_abi_header MATCHES "#define TOCAT_ABI_VERSION ([0-9]+)")
  message(FATAL_ERROR "${TOCAT_WASM_ABI_HEADER} declares no TOCAT_ABI_VERSION")
endif()

set(TOCAT_WASM_ABI_VERSION "${CMAKE_MATCH_1}")

add_library(tocat_wasm INTERFACE)
add_library(Tocat::wasm ALIAS tocat_wasm)

# The alias above only exists in this build. EXPORT_NAME is what makes the
# installed package offer the same spelling, since the export namespace is
# prepended to this rather than to the target's own name.
set_target_properties(tocat_wasm PROPERTIES EXPORT_NAME wasm)

target_include_directories(
  tocat_wasm INTERFACE $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
                       $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)

# A guest is one translation unit that imports nothing, so the SDK is a header
# and a set of flags. There is deliberately no library to link: a runtime is
# the thing a guest must not have.

if(CMAKE_SYSTEM_PROCESSOR STREQUAL "wasm32")
  set(tocat_examples_default ON)
else()
  set(tocat_examples_default OFF)
endif()

option(TOCAT_WASM_BUILD_EXAMPLES "Build the example guests"
       ${tocat_examples_default})
option(TOCAT_WASM_INSTALL "Install the SDK" ON)

if(TOCAT_WASM_BUILD_EXAMPLES)
  enable_language(C)
  enable_language(CXX)
  add_subdirectory(examples)
endif()

if(NOT TOCAT_WASM_INSTALL)
  return()
endif()

install(TARGETS tocat_wasm EXPORT TocatWasmTargets)
install(DIRECTORY include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})

set(TOCAT_WASM_CMAKE_DIR ${CMAKE_INSTALL_LIBDIR}/cmake/TocatWasm)

install(
  EXPORT TocatWasmTargets
  FILE TocatWasmTargets.cmake
  NAMESPACE Tocat::
  DESTINATION ${TOCAT_WASM_CMAKE_DIR})

# The function and the toolchain file are part of the package rather than of
# this build: a consumer needs both to compile a guest at all.
install(FILES cmake/TocatWasmGuest.cmake cmake/wasm32-toolchain.cmake
        DESTINATION ${TOCAT_WASM_CMAKE_DIR})

configure_package_config_file(
  cmake/TocatWasmConfig.cmake.in
  ${CMAKE_CURRENT_BINARY_DIR}/TocatWasmConfig.cmake
  INSTALL_DESTINATION ${TOCAT_WASM_CMAKE_DIR})

# The ABI is versioned separately from the crate, and a guest built against
# ABI 1 keeps working, so compatibility here is about the header's helpers
# rather than about the wire format.
write_basic_package_version_file(
  ${CMAKE_CURRENT_BINARY_DIR}/TocatWasmConfigVersion.cmake
  COMPATIBILITY SameMinorVersion)

install(FILES ${CMAKE_CURRENT_BINARY_DIR}/TocatWasmConfig.cmake
              ${CMAKE_CURRENT_BINARY_DIR}/TocatWasmConfigVersion.cmake
        DESTINATION ${TOCAT_WASM_CMAKE_DIR})
