# The guest ABI

A WebAssembly guest is a plugin like any other: the [`wasm`](../guide/plugins/wasm.md) stage implements [`Plugin`](plugin-trait.md) by forwarding each
call into the module and applying what comes back. This page is the guest half of that contract, ABI version 1.

The whole ABI is one call in and one struct out. **A guest imports nothing.** There are no host functions to link against, which is why a module built
against WASI is refused at load time. That is not a sandbox bolted on afterwards: the trait already says that a stage decides what to forward and
queues everything else for the host, so there is nothing left for a guest to import.

## Exports

| Export                   | Signature      | Required | Meaning                                         |
|--------------------------|----------------|----------|-------------------------------------------------|
| `memory`                 | memory         | yes      | The guest's linear memory                       |
| `tocat_abi_version`      | `() -> i32`    | yes      | Must be 1                                       |
| `tocat_outbox`           | `() -> i32`    | yes      | Pointer to the outbox                           |
| `tocat_alloc`            | `(i32) -> i32` | yes      | Somewhere the host may write `len` input bytes  |
| `tocat_on_bytes`         | `(i32, i32)`   | yes      | A chunk, as a pointer and a length              |
| `tocat_init`             | `(i32, i32)`   | no       | The entry's `config`, as JSON, once             |
| `tocat_on_eof`           | `()`           | no       | Upstream is finished. The last chance to emit   |
| `tocat_on_tick`          | `()`           | no       | The schedule came due                           |
| `tocat_tick_interval_ns` | `() -> i64`    | no       | Requested tick period, 0 for none. Read once    |
| `tocat_datagram_safe`    | `() -> i32`    | no       | Non-zero if boundaries are preserved. Read once |

`tocat_alloc` is an arena rather than a heap: the host never frees, writes exactly `len` bytes, and calls it again on the next chunk. One static
buffer, grown when a chunk does not fit, is a complete implementation. Returning 0 refuses the chunk, and the host fails that direction saying so.

Both `tocat_tick_interval_ns` and `tocat_datagram_safe` are read once, after `tocat_init`, because that is when the guest knows its options and because
the host reads them once too. A guest that asks for a tick without exporting `tocat_on_tick` gets no timer.

## Pointers are absolute

Every pointer crossing this ABI, in both directions, is a byte offset into the guest's linear memory as a whole. Not an offset into a buffer, and not
an offset into whatever `static` the guest is using as its arena.

This is the one mistake the ABI invites, because a guest written around a static array reads naturally with offsets into that array:

```rust,ignore
static mut ARENA: [u8; 131072] = [0; 131072];

pub extern "C" fn tocat_outbox() -> i32 {
    0                                   // wrong: 0 is the start of memory,
}                                       // not the start of ARENA
```

The linker decides where `ARENA` lands, usually somewhere above 1 KiB of data segments, so a guest doing that writes its outbox into `ARENA[0..48]`
while the host reads address 0, and reads its input from `ARENA[64..]` while the host wrote to address 64. Every pointer is wrong by the same constant,
and nothing traps: both sides are reading memory that exists.

The symptom is an outbox that decodes as all zeros, which is `emit = 0`, which is a stage that drops everything. If a guest that should be rewriting
the stream is silently dropping it, this is the first thing to check.

The fix is to add the arena's own address to every offset, and to treat the pointer the host hands to `tocat_on_bytes` as already absolute:

```rust,ignore
fn base() -> usize {
    &raw const ARENA as usize
}

pub extern "C" fn tocat_outbox() -> i32 {
    (base() + OUTBOX) as i32
}
```

A module written in WAT tends not to hit this, because a `(data (i32.const 0) ...)` segment really is at address 0. A module compiled from a language
with a data layout almost always does.

## The outbox

After every call the host reads a fixed-layout struct at `tocat_outbox()` and applies it. It is little-endian and 48 bytes long, so decoding is a
handful of loads and no allocation. Every pointer in it is an offset into the guest's own memory.

| Offset | Type  | Field         | Meaning                                                   |
|--------|-------|---------------|-----------------------------------------------------------|
| 0      | `u32` | `emit`        | 0 drop, 1 passthrough, 2 buffered                         |
| 4      | `u32` | `bytes_ptr`   | Emitted bytes, when buffered                              |
| 8      | `u32` | `bytes_len`   |                                                           |
| 12     | `u32` | `bounds_ptr`  | `u32` offsets into those bytes, one per unit boundary     |
| 16     | `u32` | `bounds_len`  | Count, not bytes. Zero means one unit covering everything |
| 20     | `u32` | `flags`       | 1 rearm, 2 halt, 4 pace, 8 error                          |
| 24     | `u32` | `message_ptr` | UTF-8: the halt reason, or the error                      |
| 28     | `u32` | `message_len` |                                                           |
| 32     | `u64` | `pace_ns`     | How long the host should wait before reading again        |
| 40     | `u32` | `logs_ptr`    | Records of `(level: u32, ptr: u32, len: u32)`             |
| 44     | `u32` | `logs_len`    | Count of records                                          |

Log levels are 0 trace, 1 debug, 2 info, 3 warn, 4 error.

Passthrough is the cheap answer and the host does not read the guest's bytes at all: nothing is copied in either direction, exactly as for a native
stage. Boundaries mean what they mean everywhere else, so read [Units and boundaries](units.md) before emitting more than one.

Two flags end the path and differ only in whether that is a success. `halt` is [`Ctx::halt`](effects.md): upstream end of stream arriving early, so
stages below are drained, sinks are flushed, and tocat exits successfully. `error` fails the direction, and is also how `tocat_init` rejects an option,
which is what makes a bad guest option a startup error carrying the guest's own message.

A guest that forgets to reset the outbox between calls repeats itself rather than corrupting anything, which is the failure mode worth having. A guest
that hands back a pointer outside its memory gets a runtime error naming the span: linear memory is the whole of what it can reach.

## Where the ABI is defined

`crates/tocat-wasm-abi` is the one definition of the wire format, and everything that touches it reads that crate rather than its own copy: the host decodes
an outbox with it, `tocat-wasm-sdk` writes one with it, and `sdk/wasm/include/tocat/abi.h` is generated from it.

```console
$ cargo run -p tocat-wasm-abi --features generate --bin tocat-abi-header
$ cargo run -p tocat-wasm-abi --features generate --bin tocat-abi-header -- --check
```

The header is committed so that a C guest builds without a Rust toolchain, and `--check` is what stops a committed generated file drifting: it writes
nothing and exits non-zero when the two disagree. Everything a generator has no opinion about, which is the exports, the arena and the helpers, stays
hand-written in `tocat.h`.

The crate asserts its own layout, so a change to the struct that moves a field fails to compile rather than becoming a stage that reads nonsense. Every
wire value has two spellings on purpose: `TOCAT_EMIT_BUFFERED` is what C sees and `Emit::Buffered` is what Rust sees, and they cannot disagree because
the enum's discriminants are the constants.

## Guests in Rust

`crates/tocat-wasm-sdk` is the Rust equivalent of `tocat.hpp`: a guest is a type implementing `Guest`, and `export_guest!` generates the arena, the outbox,
the panic handler and all nine exports.

```rust,ignore
pub struct Upper {
    out: [u8; CAPACITY],
}

impl Guest for Upper {
    const INIT: Self = Self { out: [0; CAPACITY] };
    const DATAGRAM_SAFE: bool = true;

    fn on_bytes(&mut self, ctx: &mut Context, input: &[u8]) {
        for (out, byte) in self.out.iter_mut().zip(input) {
            *out = byte.to_ascii_uppercase();
        }

        ctx.emit(&self.out[..input.len()]);
    }
}

export_guest!(Upper, arena_bytes = CAPACITY);
```

`init`, `on_eof`, `on_tick` and `tick_interval` have defaults, so a guest writes the ones it needs. Two of the checks are sharper here than in C++.
`INIT` is a `const`, which is how "global constructors do not run under `--no-entry`" becomes a compile error rather than a guest that silently reads
zeroes. And every message handed to `halt`, `fail` and `log` is a `&'static str`, so the requirement that it outlive the call is a lifetime the
compiler enforces rather than a comment.

`examples/wasm/` is a separate cargo workspace, since guests are built for `wasm32-unknown-unknown` and pulling that into the main one would mean
building the relay for it too.

```console
$ cargo build --release --manifest-path examples/wasm/Cargo.toml
```

## Guests in C and C++

`sdk/wasm/` is the same contract in C and C++: `tocat/abi.h` is generated from the crate above, `tocat/tocat.h` adds the exports and the helpers, and a
CMake package builds guests with both.

```console
$ cmake -S sdk/wasm -B build/wasm \
        -DCMAKE_TOOLCHAIN_FILE=$PWD/sdk/wasm/cmake/wasm32-toolchain.cmake
$ cmake --build build/wasm
```

```cmake
find_package(TocatWasm REQUIRED)

tocat_add_wasm_guest(redact SOURCES redact.c)
```

| File                       | Is                                                                                     |
|----------------------------|----------------------------------------------------------------------------------------|
| `include/tocat/abi.h`      | Generated: the outbox as a struct, and the wire constants                              |
| `include/tocat/tocat.h`    | The fixed exports, the arena, and helpers for the rest                                 |
| `include/tocat/tocat.hpp`  | The same ABI for C++: a guest is a type, and one macro generates its exports           |
| `examples/toupper.c`       | The smallest useful guest: one transform per chunk, no state, no options               |
| `examples/lines.cpp`       | Holding bytes across calls, emitting several units, options, end of stream, and a tick |

In C++ the exports are generated rather than written:

```cpp
struct Upper {
    static constexpr bool datagram_safe = true;

    void on_bytes(tocat::ctx &c, tocat::bytes input) { ... }
};

TOCAT_GUEST(Upper, 256 * 1024)
```

`init`, `on_eof`, `on_tick` and `tick_interval` are optional and detected. `ctx` resets the outbox on construction, so the stale-flag mistake below
cannot be made, and a halt reason is `consteval` from a string literal, so one that would not outlive the call does not compile. The header wraps the C
one rather than restating it: two headers describing one ABI would drift, and the one that drifted would be the one nobody tested.

Three things differ from the Rust guest above and are worth knowing before writing one.

Pointers stop being a hazard: in C a pointer already is an address in linear memory, so `TOCAT_ADDR` is a cast rather than a calculation.

The layout can be checked at compile time. `tocat.h` declares the outbox as a struct and asserts its size and two of its offsets, which turns a
mismatch into a build error rather than a stage that silently drops the stream. wasm32 puts the `u64` at offset 32 on an eight-byte boundary, so there
is no padding anywhere and the struct is exactly 48 bytes.

The compiler still expects a libc that is not there. A copy loop can be lowered into a call to `memcpy` whether or not you wrote one, so the header
defines `memcpy`, `memmove` and `memset`. In C++ there is a sharper version of the same problem: with `--no-entry` nothing calls `__wasm_call_ctors`,
so global constructors never run and anything needing one is silently uninitialised. Keep guest state trivially constructible, or export the
initialiser and call it from `tocat_init`. The SDK's README covers both.

## Not here yet

Side channels. [`open_channel`](effects.md#side-channels) happens at build time and hands back an id, so it needs a round trip the ABI does not have:
the guest would declare its targets during `tocat_init`, the host would open them, and the ids would have to be written back before the first chunk.
Until then, a guest that wants to record something logs it, and the log sinks decide where that goes.
