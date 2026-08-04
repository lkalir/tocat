# tocat

A socat-inspired relay built on tokio. tocat connects two endpoints (sockets, files, subprocesses, stdio, etc.) and copies bytes between them in both
directions. Unlike socat, connections can be described in a TOML config file with editor completion and validation, and the bytes in flight can be
passed through a pipeline of plugins.

## Status

tocat is in early days and has a long way to go before reaching parity with socat. The currently supported sources and sinks are

- [ ] abstract
- [ ] abstract-listen
- [x] exec
- [x] file
- [x] pipe / fifo
- [ ] proxy
- [ ] pty
- [ ] socks
- [x] stdio
- [x] system
- [x] tcp
- [x] tcp-listen
- [ ] tls
- [ ] tls-listen
- [x] udp
- [x] udp-listen
- [x] unix
- [x] unix-listen
- [ ] unix-dgram
- [ ] unix-dgram-listen
- [ ] websocket
- [ ] websocket-listen

The plugins that ship with tocat are

- [ ] base64/unbase64 - encoding using base64
- [x] block - accumulate and emit data in fixed sizes
- [x] compress / decompress - zstd
- [ ] encrypt / decrypt - symmetric stream encryption/decryption
- [ ] frame / unframe - apply or strip framing to and from streams
- [ ] hash - digest stream contents
- [x] limit - terminate stream after N bytes
- [ ] pcap - save stream as pcap
- [x] process - delegate to subprocess using stdin/stdout
- [x] rate - measure and report throughput
- [ ] redact - remove sensitive information from streams
- [x] tee - mirror a path's bytes to a file or stderr, verbatim or as a hex dump
- [x] throttle - artificially constrict bandwidth
- [ ] WASM plugin support

Many configurations and options present in socat are also currently missing.

## Installation

```console
$ cargo install --path crates/tocat-cli
```

Plugins are cargo features and all of them are on by default. Features are additive, so subtracting one means turning the defaults off and naming what
you want back.

```console
# Only tee
$ cargo install --path crates/tocat-cli --no-default-features --features tee
# tee and rate
$ cargo install --path crates/tocat-cli --no-default-features --features tee,rate
# No plugins at all
$ cargo install --path crates/tocat-cli --no-default-features
```

`nix develop` gives the toolchain and the tools this repo expects (`mdbook`, `tombi`, `tokio-console`, `socat`, `pv` and friends); `nix build` builds
the binary.

## Usage

A run is a source, a sink, and an optional pipeline of plugins between them. The outer positional arguments are the endpoints and anything between them
is a pipeline entry; `--from` and `--to` fill the same two slots, and `-p` adds a pipeline entry wherever the endpoints came from.

```console
$ tocat - tcp:localhost:9000
$ tocat tcp-listen:9000,fork tcp:localhost:8080
$ tocat tcp-listen:8080,fork tee,format=hex tcp:example.com:80
$ tocat -f tcp-listen:8080,fork -t tcp:example.com:80 -p 'throttle,rate=1MiB'
```

Endpoints and pipeline entries are both a name, an optional target or direction, and comma-separated options.

```
scheme:target,option,option=value
name[:direction],option,option=value
```

The same run can live in a `tocat.toml`, which tocat finds and merges with the command line, the command line winning.

```toml
source = "tcp-listen:9000,fork"
sink = "tcp:localhost:8080"

[[plugin]]
name = "tee"
format = "hex"
```

Run `tocat -h` for the full list of flags, `tocat --list-plugins` for the plugins your binary was built with, and `tocat --dump-config` to see the
merged configuration a run will use.

## Documentation

The documentation is an mdbook in [`docs/`](docs/).

```console
$ mdbook serve docs --open
```

| Part                                       | Contents                                                                                            |
|--------------------------------------------|-----------------------------------------------------------------------------------------------------|
| [User guide](docs/src/guide/invocation.md) | Invocation, every endpoint scheme, every plugin, buffers, progress, configuration files and logging |
| [Plugin API](docs/src/api/overview.md)     | The `Plugin` trait, options and building, units, ticks, effects, host plugins, and testing a stage  |
| [Design](docs/src/design/architecture.md)  | Architecture, the data path, pipeline construction, the datagram model, configuration, lifecycle    |

## License

Dual licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
