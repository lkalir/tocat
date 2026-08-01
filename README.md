# tocat

A socat-inspired relay built on tokio. tocat connects two endpoints (sockets, files, subprocesses, stdio, etc.) and copies bytes between them in both directions. Unlike socat, connections can be described in a TOML config file with editor completion and validation, and the bytes in flight can be passed through a pipeline of plugins.

## Status

tocat is in early days and has a long way to go before reaching parity with socat. The currently supported sources and sinks are

- [ ] abstract
- [ ] abstract-listen
- [x] exec
- [ ] fifo
- [x] file
- [ ] pipe
- [ ] proxy
- [ ] pty
- [ ] socks
- [x] stdio
- [x] system
- [x] tcp
- [x] tcp-listen
- [ ] tls
- [ ] tls-listen
- [ ] udp
- [ ] udp-listen
- [x] unix
- [x] unix-listen
- [ ] unix-dgram
- [ ] unix-dgram-listen
- [ ] websocket
- [ ] websocket-listen

The plugins that ship with tocat are

- [ ] base64/unbase64 - encoding using base64
- [ ] cipher / uncipher - symmetric stream encryption/decryption
- [x] compress / decompress - zstd
- [ ] frame / unframe - apply or strip framing to and from streams
- [ ] hash - digest stream contents
- [ ] limit - terminate stream after N bytes
- [ ] pcap - save stream as pcap
- [x] process - delegate to subprocess using stdin/stdout
- [ ] redact - remove sensitive information from streams
- [x] tee - mirror a path's bytes to a file or stderr, verbatim or as a hex dump
- [ ] throttle - artificially constrict bandwidth
- [ ] WASM plugin support

Many configurations and options present in socat are also currently missing.

## Installation

```console
$ cargo install --path crates/tocat-cli
```

Plugins are cargo features. `tee` and `process` are on by default. Features are additive, so subtracting one means turning the defaults off and naming
what you want back.

```console
# Install with all plugins
$ cargo install --path crates/tocat-cli
# Defaults plus compression
$ cargo install --path crates/tocat-cli --features compress
# Only tee
$ cargo install --path crates/tocat-cli --no-default-features --features tee
# No plugins at all
$ cargo install --path crates/tocat-cli --no-default-features
```

## Usage

```
$ tocat --help
socat-inspired relay

Usage: tocat [OPTIONS] [SPEC]...

Arguments:
  [SPEC]...  SOURCE [PLUGIN ...] SINK. The outer specs are endpoints; anything between them is a pipeline entry. Slots already filled by --from/--to are skipped.

Options:
  -c, --config <PATH>      Configuration file to use.
      --no-config          Disable configuration file merging.
      --dump-config        Render the final configuration as TOML.
  -f, --from <ADDR>        Source endpoint. Fills the first positional slot.
  -t, --to <ADDR>          Sink endpoint. Fills the last positional slot.
  -p, --plugin <SPEC>      Pipeline entry: NAME[:DIRECTION][,key=value...]. Repeatable, applied in order.
      --no-plugins         Ignore plugins declared in the configuration file.
      --list-plugins       List the plugins compiled into this binary and exit.
  -v, --verbose...         Simple verbosity level.
      --log-level <LEVEL>  Explicit verbosity level. [possible values: off, error, warn, info, debug, trace]
  -h, --help               Print help
  -V, --version            Print version
```

tocat can be used similarly to socat with the source and sink endpoints specified via strings, you can also use the explicit `--from` and `--to` flags
or a configuration file (more on that below).

```console
$ tocat --from - --to tcp:localhost:9000
```

### Endpoints

The fundamental unit of tocat is the endpoint. An endpoint is a scheme, a target, and a set of options.

```
scheme:target,option,option=value
```

Bare options mean true, so `fork` and `fork=true` are equivalent. Schemes and options may have various aliases. If an option is specified multiple times,
the last instance takes precedence.

#### `tcp` - Connect to a TCP socket

Aliases: `tcp-connect`, `connect`

```console
$ tocat - tcp:example.com:80
$ tocat - tcp:[::1]:9000
```

####  `tcp-listen` - Accept inbound TCP connections

Aliases: `tcplisten`, `listen`. Defaults to `localhost:8000`

```console
$ tocat tcp-listen:9000 -
$ tocat tcp-listen:0.0.0.0:9000,fork tcp:localhost:8080
```

| Option            | Description                                                                                             |
|-------------------|---------------------------------------------------------------------------------------------------------|
| fork              | Create a task for each client, without this option tocat serves a single connection and then terminates |
| max-connections=N | Concurrent connection ceiling. Default is 1024                                                          |


#### `unix` and `unix-listen` - Unix domain sockets

```console
$ tocat - unix:/run/app/app.sock
$ tocat unix-listen:/tmp/tocat.sock,fork,unlink,mode=660 tcp:localhost:8080
```

| Option                  | Description                                                                                                   |
|-------------------------|---------------------------------------------------------------------------------------------------------------|
| fork, max-connections=N | As `tcp-listen`                                                                                               |
| unlink                  | Remove a stale socket before binding. tocat probes the file and will refuse to unlink a socket that is in-use |
| mode=NNN                | Permissions to apply after binding                                                                            |

#### `file` - read and write files

Files are unidirectional. They are read when they are the source and written to when they are the sink. Opening a FIFO blocks until a peer appears.

```console
$ tocat file:/tmp/payload tcp:localhost:9000
$ tocat tcp-listen:9000 file:/tmp/capture,truncate
```

| Option   | Description                                      |
|----------|--------------------------------------------------|
| append   | Append instead of overwrite file                 |
| create   | Create if missing. On by default                 |
| truncate | Truncate file on open, ignored if append is true |

#### `exec` - subprocesses

Runs a program with its stdin and stdout wired to the relay. Arguments are split on whitespace and passed directly to the program. This is not a shell,
so no globbing, quoting, or metacharacters. The child's stderr is inherited, so its diagnostics go to your terminal rather than into the relayed data.
The child is killed when tocat drops the connection.

```console
$ tocat tcp-listen:9000,fork "exec:/usr/bin/env cat"
```

#### `system` - shell commands
Runs the given string through a shell, so pipes, redirection, globbing, and variable expansion all work. Anything the string contains runs with tocat's
privileges — don't use system with a command built from untrusted input, or in a config file others can write.

```console
$ tocat tcp-listen:9000,fork "system:grep -v DEBUG | sort -u"
```

#### `stdio` - standard input and output

Also spelled `-`. Note that tocat's stdout carries relayed data when stdio is the sink. Logs and dumps go to stderr for this reason, and tocat refuses
to dump to stdout.

```console
$ dd if=/dev/zero | tocat - tcp:localhost:9000
```

## Plugins

Bytes can be passed through a pipeline of plugins on their way between the endpoints. A plugin can watch the stream, rewrite it, or drop parts of it.

Plugins are written like endpoints — a name, an optional direction, and a set of options — and go between the source and the sink on the command line.

```
name[:direction],option,option=value
```

```console
$ tocat tcp-listen:8080,fork tee,format=hex tcp:example.com:80
```

The same entries can be given with `-p`, which is the only way to add one when both endpoints come from flags or a config file. Repeat it as needed;
entries apply in the order written, after any in the config file.

```console
$ tocat -f tcp-listen:8080,fork -t tcp:example.com:80 -p 'tee,format=hex'
```

Roles are decided by position, never by looking at the text — an endpoint and a plugin are spelled the same way. The outer arguments fill whichever
endpoint slots `--from` and `--to` left open, and whatever remains in the middle is the pipeline.

```console
$ tocat SRC SINK                  # no plugins
$ tocat SRC tee compress SINK     # two entries, in that order
$ tocat -f SRC tee SINK           # one entry; SINK fills the open slot
$ tocat -f SRC -t SINK tee        # one entry; both slots already filled
```

Run `tocat --list-plugins` to see what your build has, and `--no-plugins` to ignore the ones in a config file.

### Direction

An entry applies to one path or both. Omitting the direction means both.

| Direction        | Aliases                          | Meaning                              |
|------------------|----------------------------------|--------------------------------------|
| `source-to-sink` | `forward`, `src-to-sink`, `out`  | Bytes read from the source           |
| `sink-to-source` | `reverse`, `sink-to-src`, `in`   | Bytes read from the sink             |
| `both`           | `bidi`, `duplex`, `all`          | Both paths, as two separate instances|

`both` builds two independent instances, one per path, so per-direction state — byte offsets, codec state — never leaks across paths.

The command line is a picture of the wire, read left to right, and the reverse path reads it right to left:

```
tocat SRC  a  b  c  SINK

source -> sink:   SRC -> a -> b -> c -> SINK
sink -> source:   SRC <- a <- b <- c <- SINK
```

So a stage written earlier sits nearer the source, and bytes coming back from the sink reach the later stages first. Write the stages in the order the
forward path would see them, and the reverse path nests correctly — which is what you want for anything that wraps the payload.

The `source`/`sink` aliases are accepted but read badly: `tee:sink` means sink-to-source, not "tee at the sink". Prefer `forward` and `reverse`.

### Common options

Two options are handled by tocat rather than the plugin, and work on any entry.

| Option      | Description                                                                                                                    |
|-------------|--------------------------------------------------------------------------------------------------------------------------------|
| `as=NAME`   | Name this instance. Appears in logs and in `tee` headers. Without it a stage is named after its plugin, with `#1`, `#2` for repeats |
| `detach`    | Run this stage on its own task. Costs a copy and a wakeup per chunk, so it is only worth it for stages that are expensive per byte |

`process` always runs on its own task, so `detach = false` on one is rejected rather than ignored.

If nothing is declared on a path, that direction is copied straight through with no plugin machinery in the way.

### `tee` - mirror the stream

Writes a copy of everything on its path to a file or to stderr, without touching the payload.

```console
$ tocat tcp-listen:8080,fork tee,format=hex tcp:example.com:80
$ tocat tcp-listen:8080,fork 'tee:forward,file=req.bin' 'tee:reverse,file=resp.bin' tcp:example.com:80
```

| Option        | Description                                                                        |
|---------------|--------------------------------------------------------------------------------------|
| `file=PATH`   | File to write to. `-`, `stderr` or omitted for stderr                              |
| `format=hex`  | Offset, hex, and ASCII columns behind a `[source -> sink \| stage]` header          |
| `format=raw`  | Payload bytes verbatim. Also `binary`, `raw-binary`. The default                    |
| `append`      | Append to an existing file rather than truncating. On by default                    |
| `width=N`     | Bytes per row in hex mode. Default is 16                                           |
| `label=TEXT`  | Override the hex header label. Prefer `as`, which names the instance everywhere     |

Entries naming the same file share one writer, so their output interleaves at chunk granularity rather than racing. Hex entries stay separable because
each carries the connection and the stage name in its header; raw entries do not, so two raw tees on one file will produce something you cannot untangle.

tocat refuses to write a dump to stdout, which may be carrying relayed payload.

### `compress` and `decompress` - zstd

Behind the `compress` cargo feature. Compression is asymmetric, so pair the two rather than using `both`:

```console
$ tocat - compress:forward decompress:reverse tcp:relay.internal:9000
```

The far end of the link runs the mirror image, and the two relays form a compressed tunnel over an otherwise plaintext hop.

| Option      | Plugin       | Description                                                                                     |
|-------------|--------------|-------------------------------------------------------------------------------------------------|
| `level=N`   | `compress`   | zstd level, 1–22. Higher is smaller and slower. Default is 3                                    |
| `flush`     | `compress`   | Flush after every chunk so bytes reach the peer immediately. On by default; costs ratio          |
| `report`    | both         | Log the compression ratio when the stream ends                                                  |

Both default to `detach`, since they are expensive enough per byte to be worth their own task.

### `process` - pipe through a subprocess

Hands this path's bytes to a program's stdin and takes its stdout back, so any filter that reads stdin and writes stdout becomes a stage.

```console
$ tocat tcp-listen:9000,fork 'process:forward,command=gzip -c' tcp:backend:8080
```

```toml
[[plugin]]
name = "process"
direction = "source-to-sink"
argv = ["gzip", "-c"]
stderr = "log"
```

| Option        | Description                                                                                              |
|---------------|------------------------------------------------------------------------------------------------------------|
| `argv=[...]`  | Program and arguments, passed directly. No shell, no globbing, no metacharacters. Config file only        |
| `command=STR` | A shell command line, as `system:`. Runs with tocat's privileges                                          |
| `stderr=log`  | Capture the child's stderr and re-emit each line as a warning tagged with the stage name. The default     |
| `stderr=inherit` | Let it go to tocat's stderr, where it interleaves with logs and dumps                                  |
| `stderr=null` | Discard it                                                                                               |

Give one of `argv` or `command`. Only `command` works on the command line, since there is no way to write an array there — and a command containing a
comma has to go in a config file, because commas separate options.

Filters that emit nothing until their input closes work correctly: tocat feeds the child and drains it concurrently, and closes its stdin at end of
stream so it flushes. `sort`, `tac` and `gzip` all behave.

A non-zero exit fails that direction rather than being logged and ignored, since it means the bytes the child produced were incomplete or wrong.

This is the most expensive stage tocat has. Bytes cross a pipe twice per chunk in each direction, and every connection spawns its own children — under
`fork` with `direction = "both"`, sixty-four clients means one hundred and twenty-eight processes. It is the right price for reaching any tool on the
system, but reach for a compiled plugin for anything hot.

### Writing a plugin

Plugins implement the `Plugin` trait from the `tocat-api` crate. A plugin is a synchronous byte transformer: it is handed a chunk and decides what to
forward, and anything that touches the outside world — writing a dump file, emitting a log line — is queued for tocat to perform rather than done in
place. That keeps plugins testable and off the async runtime, and it is the shape a WASM guest has to take, so the same trait will cover WASM plugins
when they land.

A few stages cannot satisfy that contract — `process` decides nothing per chunk and may emit output belonging to chunks it was handed long ago. Those
describe themselves to tocat and let it do the running, rather than bending the trait to fit. Spawning is a capability of the host by construction,
which is what keeps it out of reach of a WASM guest.

Plugins that need no dependencies live as modules in `tocat-plugins`; ones that bring their own dependency tree get their own crate and are registered
from there. Either way they are reached through one registry, so tocat itself cannot tell them apart.

## Configuration

tocat will automatically look for a `.tocat.toml` or `tocat.toml` file and merge its contents with your cli arguments (cli taking precedence). Use the
`--config` flag to select a config and the `--no-config` flag to disable the search. You can use the `--dump-config` flag to display the final merged
result.

Below is an example configuration that accepts up to 64 connections on port 9000, and connects via TCP to another server, with a raw log of data
coming back from the connection.

```toml
#:schema ./tocat.schema.json

log-level = "info"

[source]
type = "tcp-listen"
host = "0.0.0.0"
port = 9000
fork = true
max-connections = 64

[sink]
type = "tcp"
addr = "backend.internal:8080"

[[plugin]]
name = "tee"
direction = "sink-to-source"
file = "/var/log/tocat-capture.bin"
format = "raw-binary"
```

Endpoints can also be specified using the string shorthands like in the cli

```toml
#:schema ./tocat.schema.json

source = "tcp-listen:9000,fork"
sink = "tcp:localhost:8080"
```

Plugin entries are a list, applied in order, and command-line entries are appended to them. So a standing capture can live in the config file while an
ad-hoc stage is added for one run.

```toml
[[plugin]]
name = "tee"
as = "wire"
file = "session.hex"
format = "hex"

[[plugin]]
name = "compress"
direction = "forward"
level = 9
```

### Schema

`tocat.schema.json` is a JSON-schema for the configuration file format. If you use [tombi](https://github.com/tombi-toml/tombi) you get completion,
hover documentation, and validation in your editor. Either add the directive comment shown above, or register it in `tombi.toml`:

```toml
[[schemas]]
path = "tocat.schema.json"
include = ["tocat.toml", ".tocat.toml"]
```

The schema describes the plugins that ship with tocat, and accepts entries for ones it does not know about — which is necessary, since the plugin set
depends on the features the binary was built with. tocat itself is the real validator and rejects unknown options at startup.

### Logging

Logs go to stderr by default. Verbosity can be set by repeatedly using `-v`, explicitly via `--log-level`, or by setting `log-level` in the config file.
`RUST_LOG` overrides all of these options and can perform per-target levels.

```console
$ RUST_LOG=tocat=trace,tokio=warn tocat tcp-listen:9000 -
```

Run with `-v` to see the resolved pipeline for each direction, which is the quickest way to confirm a long chain came out in the order you meant.

Additional sinks can be configured using the config file.

```toml
[[log]]
type = "stderr"
format = "compact"

[[log]]
type = "file"
path = "/var/log/tocat.log"
format = "json"
level = "debug"
rotation = "daily"
max_files = 7
```

Traffic dumps are not logs: they are written by the `tee` plugin, go wherever that entry points, and are unaffected by the log level.
