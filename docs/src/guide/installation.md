# Installation

```console
$ cargo install --locked tocat
```

From a checkout, where the crate lives in `crates/tocat-cli` and is named for
the binary it produces:

```console
$ cargo install --path crates/tocat-cli
```

The workspace pins a stable toolchain in `rust-toolchain.toml`, and
`.cargo/config.toml` sets `--cfg tokio_unstable` for the whole build graph,
which `tokio-console` requires.

A Nix flake is provided. `nix develop` gives the toolchain, the cargo tooling,
`mdbook` and `tombi`, and a handful of things worth having next to a relay
(`socat`, `websocat`, `netcat`, `tcpdump`, `pv`, `hyperfine`, `tokio-console`).
`nix build` builds the binary.

## Choosing plugins

Plugins are cargo features of the `tocat` crate. The default feature
`all-plugins` turns on everything, and features are additive, so subtracting one
means turning the defaults off and naming what you want back.

```console
$ cargo install --locked tocat
# Only tee
$ cargo install --locked tocat --no-default-features --features tee
# tee and rate
$ cargo install --locked tocat --no-default-features --features tee,rate
# Everything except WASM, which is most of the build time
$ cargo install --locked tocat --no-default-features --features all-plugins-no-wasm
# No plugins at all
$ cargo install --locked tocat --no-default-features
```

| Feature               | Gives                                                                         |
| --------------------- | ----------------------------------------------------------------------------- |
| `all-plugins`         | Every plugin below. On by default                                             |
| `all-plugins-no-wasm` | Every plugin except `wasm`, which is what wasmtime costs to build             |
| `block`               | [`block`](plugins/block.md)                                                   |
| `compress`            | [`compress` and `decompress`](plugins/compress.md), and a zstd dependency     |
| `hash`                | [`hash`](plugins/hash.md), and the RustCrypto digest crates                   |
| `limit`               | [`limit`](plugins/limit.md)                                                   |
| `process`             | [`process`](plugins/process.md)                                               |
| `rate`                | [`rate`](plugins/rate.md)                                                     |
| `tee`                 | [`tee`](plugins/tee.md)                                                       |
| `throttle`            | [`throttle`](plugins/throttle.md)                                             |
| `timeout`             | [`timeout`](plugins/timeout.md)                                               |
| `wasm`                | [`wasm`](plugins/wasm.md), and a wasmtime dependency                          |
| `tokio-console`       | A `console-subscriber` layer, for inspecting the runtime with `tokio-console` |

Every plugin is a module of `tocat-plugins`, and a feature there enables both
the module and whatever optional dependencies it needs, so a build without
`compress` never compiles zstd and one without `wasm` never compiles wasmtime.
The `tocat` crate forwards its own features to that one and never names a
plugin.

A binary knows what it was built with, so `tocat --list-plugins` is the
authoritative answer for any given install: it prints each plugin and its one
line description. Naming a plugin the binary does not have is an error at
startup, with a suggestion drawn from the ones it does have.

## Building a guest

The [`wasm`](plugins/wasm.md) plugin loads modules rather than building them, so
nothing above produces one. `sdk/wasm/` is a CMake package that installs the
guest ABI header and a `tocat_add_wasm_guest()` function, and builds the example
guests with it. Installing it needs no compiler; building a guest needs a clang,
any clang, since a guest imports nothing and has no WASI to link against.

```console
$ cmake -S sdk/wasm -B build/wasm \
        -DCMAKE_TOOLCHAIN_FILE=$PWD/sdk/wasm/cmake/wasm32-toolchain.cmake
$ cmake --build build/wasm
```

## Building this book

The documentation is an [mdbook](https://rust-lang.github.io/mdBook/) in
`docs/`. `mdbook` is in the dev shell; otherwise `cargo install mdbook`.

```console
$ mdbook serve docs --open
$ mdbook build docs
```
