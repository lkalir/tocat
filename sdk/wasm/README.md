# tocat WebAssembly guest SDK

The guest side of the [`wasm`](../../docs/src/guide/plugins/wasm.md) plugin: one
header, one CMake function, and a toolchain file. The example guests under
`examples/` are built with the same function a consumer would use, so the
supported path and the demonstrated path cannot drift apart.

```console
$ cmake -S sdk/wasm -B build/wasm \
        -DCMAKE_TOOLCHAIN_FILE=$PWD/sdk/wasm/cmake/wasm32-toolchain.cmake
$ cmake --build build/wasm
$ tocat - 'wasm,module=build/wasm/examples/toupper.wasm' tcp:localhost:9000
```

`clang` is the only requirement, and any clang can target wasm32. Nothing here
needs a wasi-sdk, because a guest imports nothing and so has no WASI to link
against.

| Path                           | Is                                                                                     |
| ------------------------------ | -------------------------------------------------------------------------------------- |
| `include/tocat/abi.h`          | Generated from `crates/tocat-wasm-abi`: the outbox struct and the wire constants       |
| `include/tocat/tocat.h`        | The exports that never vary, the arena, and helpers for the rest                       |
| `include/tocat/tocat.hpp`      | The same ABI for C++: a guest is a type, and one macro generates its exports           |
| `cmake/TocatWasmGuest.cmake`   | `tocat_add_wasm_guest()`, which is the whole build interface                           |
| `cmake/wasm32-toolchain.cmake` | Targets wasm32 with clang                                                              |
| `examples/toupper.c`           | The smallest useful guest: one transform per chunk, no state, no options               |
| `examples/lines.cpp`           | Holding bytes across calls, emitting several units, options, end of stream, and a tick |

## Installing it

```console
$ cmake -S sdk/wasm -B build/sdk -DCMAKE_INSTALL_PREFIX=/usr/local
$ cmake --install build/sdk
```

Installing needs no compiler at all: the project declares `LANGUAGES NONE` and
only enables C and C++ when the examples are asked for. That matters for
packaging, where the machine building the package and the machine
cross-compiling guests are rarely the same one.

## Using it from another project

```cmake
find_package(TocatWasm REQUIRED)

tocat_add_wasm_guest(redact SOURCES redact.c)
```

```console
$ cmake -S . -B build \
        -DCMAKE_TOOLCHAIN_FILE=/usr/local/lib/cmake/TocatWasm/wasm32-toolchain.cmake
```

The toolchain file has to be given on the command line rather than by the
package, because CMake resolves it before the first `project()` call and long
before `find_package()` runs. The package exposes its own copy as
`TOCAT_WASM_TOOLCHAIN_FILE`, which is the path to pass to a nested configure
when a project builds guests as part of a larger host build.

`find_package` also sets `TOCAT_WASM_ABI_VERSION`, read out of the header at
configure time, so a consumer can check which ABI it is about to compile against
rather than waiting for tocat to refuse the module.

`tocat_add_wasm_guest(<target> SOURCES <src>... [OUTPUT_NAME <name>])` produces
`<name>.wasm` and applies the flags a guest needs. It refuses to run without a
wasm32 toolchain rather than producing a native binary nobody asked for.

## Where the ABI comes from

`include/tocat/abi.h` is generated from `crates/tocat-wasm-abi`, which is also
what the relay decodes an outbox with and what a Rust guest writes one through.
It is committed so that a C guest needs no Rust toolchain to build, and
regenerated with:

```console
$ ./scripts/regen-abi.sh
$ cargo run -p tocat-wasm-abi --example tocat-abi-header -- --check
```

The generator is an example rather than a binary so that cbindgen stays a
dev-dependency and never appears in what a consumer of the crate resolves.

`--check` writes nothing and fails when the committed header is stale, which is
the thing worth running in CI: a generated file that nobody checks is a
hand-written file with a misleading comment at the top.

## Things that will catch you

**Exports.** lld exports nothing under `--no-entry`, so a plain non-static
function is invisible to the host. `TOCAT_EXPORT` attaches
`__attribute__((export_name(...)))`, which is what puts the name in the export
section, and `used`, which stops the optimiser deleting a function that nothing
in the module calls.

**Pointers are absolute**, meaning addresses in linear memory. In C that is what
a pointer already is, so `TOCAT_ADDR` is a cast and the mistake that catches
guests written around a static array in other languages cannot happen here.

**No libc, but the compiler still calls it.** A copy loop can be lowered into a
call to `memcpy`, and a struct assignment into `memmove`, whether or not you
wrote either. `tocat.h` defines both, plus `memset`; `-fno-builtin` reduces how
often they are reached for. Define `TOCAT_NO_MEM_BUILTINS` if something else in
your link provides them.

**Global constructors do not run** in C++. With `--no-entry` there is no
entrypoint to call `__wasm_call_ctors`, so anything needing a non-trivial
constructor is left uninitialised, silently. Keep global state trivially
constructible, as `lines.cpp` does, or export the initialiser and call it
yourself:

```cmake
target_link_options(redact PRIVATE -Wl,--export=__wasm_call_ctors)
```

```cpp
extern "C" void __wasm_call_ctors();

TOCAT_EXPORT(tocat_init) void tocat_init(int32_t ptr, int32_t len) {
    __wasm_call_ctors();
    ...
}
```

**Emitted bytes must not move until the next call.** The host reads guest memory
after the call returns, so a buffer compacted at the end of the call that
emitted from it hands the sink bytes that have already been overwritten.
`lines.cpp` defers compaction to the start of the following call, which is what
`consumed` is for.

**Sizes are a contract with the relay.** `tocat_alloc` returning 0 refuses the
chunk and fails the direction, which is the honest answer but not a useful one.
A guest whose arena is smaller than tocat's copy buffer (256 KiB by default)
needs the relay run with a matching `-b`.

## Trying one without a relay

A guest is easiest to debug outside tocat: load the module, call the exports by
hand, and decode the outbox after each call. Anything that can instantiate a
module will do, since there are no imports to satisfy.
