# tocat

A socat-inspired relay built on tokio. tocat connects two endpoints (sockets,
files, subprocesses, stdio, etc.) and copies bytes between them in both
directions. Unlike socat, connections can be described in a TOML config file
with editor completion and validation, and the bytes in flight can be passed
through a pipeline of plugins.

```console
$ cargo install --locked tocat
```

```console
$ tocat - tcp:localhost:9000
$ tocat tcp-listen:9000,fork tcp:localhost:8080
$ tocat tcp-listen:8080,fork tee,format=hex tcp:example.com:80
$ tocat -f tcp-listen:8080,fork -t tcp:example.com:80 -p 'throttle,rate=1MiB'
```

A run is a source, a sink, and an optional pipeline of plugins between them. The
outer positional arguments are the endpoints and anything between them is a
pipeline entry; `--from` and `--to` fill the same two slots, and `-p` adds an
entry wherever the endpoints came from.

The same run can live in a `tocat.toml`, which tocat finds and merges with the
command line, the command line winning:

```toml
source = "tcp-listen:9000,fork"
sink = "tcp:localhost:8080"

[[plugin]]
name = "tee"
format = "hex"
```

## Plugins

Plugins are cargo features and all of them are on by default, so subtracting one
means turning the defaults off and naming what you want back:

```console
$ cargo install --locked tocat --no-default-features --features tee,rate
```

`all-plugins-no-wasm` is the one worth knowing: it gives everything except the
WebAssembly host, which is most of the build time.

`tocat --list-plugins` prints what your binary was actually built with.

Bytes can also be passed through a WebAssembly module, written in Rust with
[`tocat-wasm-sdk`](https://crates.io/crates/tocat-wasm-sdk) or in C or C++ with
the CMake SDK in the repository. A guest imports nothing at all: no clock, no
files, no sockets.

## Documentation

The user guide, the plugin API and the design notes are an mdbook under `docs/`
in the [repository](https://github.com/lkalir/tocat).

## License

MIT or Apache-2.0, at your option.
