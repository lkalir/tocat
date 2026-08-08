# tocat_add_wasm_guest(<target> SOURCES <src>... [OUTPUT_NAME <name>])
#
# Builds one guest module. The result is <name>.wasm in the build directory,
# ready to hand to `tocat -p 'wasm,module=...'`.
#
#   tocat_add_wasm_guest(redact SOURCES redact.c)
#
# Shipped as part of the installed package, so a project consuming the SDK
# calls exactly what the examples call.

function(tocat_add_wasm_guest target)
  cmake_parse_arguments(GUEST "" "OUTPUT_NAME" "SOURCES" ${ARGN})

  if(NOT GUEST_SOURCES)
    message(FATAL_ERROR "tocat_add_wasm_guest(${target}): SOURCES is required")
  endif()

  if(NOT CMAKE_SYSTEM_PROCESSOR STREQUAL "wasm32")
    message(
      FATAL_ERROR
        "tocat_add_wasm_guest(${target}) needs a wasm32 toolchain. Configure "
        "with -DCMAKE_TOOLCHAIN_FILE=<sdk>/cmake/wasm32-toolchain.cmake, or "
        "with \${TOCAT_WASM_TOOLCHAIN_FILE} from the installed package.")
  endif()

  # C++20 because <tocat/tocat.hpp> is built on concepts and consteval. C
  # guests are unaffected.
  add_executable(${target} ${GUEST_SOURCES})
  target_link_libraries(${target} PRIVATE Tocat::wasm)

  set_target_properties(
    ${target}
    PROPERTIES SUFFIX ".wasm"
               C_STANDARD 11
               C_STANDARD_REQUIRED ON
               CXX_STANDARD 20
               CXX_STANDARD_REQUIRED ON)

  if(GUEST_OUTPUT_NAME)
    set_target_properties(${target} PROPERTIES OUTPUT_NAME ${GUEST_OUTPUT_NAME})
  endif()

  # -ffreestanding because there is no hosted environment here, and
  # -fno-builtin because the compiler will otherwise turn a copy loop into a
  # call to a memcpy that does not exist. tocat.h defines the ones it emits
  # anyway, since -fno-builtin is not a guarantee.
  #
  # C++ gives up exceptions and RTTI: both want a runtime, and a guest has
  # none. A throw would trap rather than unwind.
  target_compile_options(
    ${target}
    PRIVATE -O2
            -ffreestanding
            -fno-builtin
            -Wall
            -Wextra
            $<$<COMPILE_LANGUAGE:CXX>:-fno-exceptions>
            $<$<COMPILE_LANGUAGE:CXX>:-fno-rtti>)

  # --no-entry because a guest has no main: the host calls its exports
  # directly. --stack-first puts the stack below the data segments, so
  # overflowing it traps instead of quietly overwriting the outbox. -nostdlib
  # is the point of the whole exercise: a guest that links nothing imports
  # nothing, and a module with imports is refused when tocat loads it.
  target_link_options(
    ${target}
    PRIVATE
    -nostdlib
    -Wl,--no-entry
    -Wl,--stack-first
    -Wl,--strip-all)
endfunction()
