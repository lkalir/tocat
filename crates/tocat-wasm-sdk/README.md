# tocat-wasm-sdk

Write a [tocat](https://crates.io/crates/tocat) plugin in Rust, compiled to
WebAssembly and loaded by the relay at run time.

```console
$ cargo add tocat-wasm-sdk
$ rustup target add wasm32-unknown-unknown
```

A guest is a type implementing `Guest`, and `export_guest!` turns it into a
module: the arena, the outbox, the panic handler, and every export the host
looks for.

```rust,ignore
#![no_std]

use tocat_wasm_sdk::{Context, Guest, export_guest};

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

```console
$ cargo build --release --target wasm32-unknown-unknown
$ tocat - 'wasm,module=upper.wasm' tcp:localhost:9000
```

`init`, `on_eof`, `on_tick` and `tick_interval` have defaults, so a guest writes
only the hooks it needs.

## What the compiler checks

- `on_bytes` is required, so a guest missing one is a trait error rather than a
  module the host loads and finds nothing in.
- `INIT` is a `const`. Nothing calls `__wasm_call_ctors` in a module built with
  `--no-entry`, so a guest built at run time would never be built at all;
  requiring a const initialiser rules that out rather than documenting it.
- Messages handed to `halt`, `fail` and `log` are `&'static str`. The host reads
  guest memory after the call returns, so a message has to outlive the call, and
  here that is a lifetime rather than a comment.

## What a guest cannot do

Import anything. No clock, no files, no sockets, no host functions at all: a
module with a single import is refused when the relay loads it. It does not need
any, because a stage decides what to forward and queues everything else for the
host, so there is nothing left to call.

## Documentation

The guest ABI and the cost model are in the plugin API section of the book,
under `docs/` in the [repository](https://github.com/lkalir/tocat). The same ABI
for C and C++ is the CMake SDK under `sdk/wasm`.

## License

MIT or Apache-2.0, at your option.
