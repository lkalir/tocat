# tocat

A socat-inspired relay built on tokio. tocat connects two endpoints (sockets, files, subprocesses, stdio, etc.) and copies bytes between them in both directions. Unlike socat, connections can be described in a TOML config file with editor completion and validation.

## Status

tocat is in early days and has a long way to go before reaching parity with socat. The currently supported sources and sinks are

- [ ] abstract
- [ ] abstract-listen
- [x] exec
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

Many configurations and options present in socat are also currently missing.

## Installation

```console
$ cargo install --path .
```

## Usage

```
$ tocat --help
socat-inspired relay

Usage: tocat [OPTIONS] [SOURCE] [SINK]

Arguments:
  [SOURCE]  Source endpoint.
  [SINK]    Sink endpoint.

Options:
  -c, --config <PATH>      Configuration file to use.
      --no-config          Disable configuration file merging.
      --dump-config        Render the final configuration as TOML.
  -f, --from <ADDR>        Source endpoint.
  -t, --to <ADDR>          Sink endpoint.
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

### Dumping traffic

Any endpoint can dump the data passing through it using a common set of dump-related options.


| Option       | Description                                   | 
|--------------|-----------------------------------------------|
| `dump=PATH`  | File to write to. `-` or `stderr` for stderr. |
| `format=hex` | Offset, hex, and ASCII columns.               | 
| `format=raw` | Payload bytes verbatim. Also `binary`.        |

Providing the same dump path to both the source and the sink will interleave them. If a format is specified without a dump path then the endpoint will
dump to stderr.

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

[sink.dump]
file = "/var/log/tocat-capture.bin"
format = "raw-binary"
```

Endpoints can also be specified using the string shorthands like in the cli

```toml
#:schema ./tocat.schema.json

source = "tcp-listen:9000,fork"
sink = "tcp:localhost:8080"
```

### Schema

`tocat.schema.json` is a JSON-schema for the configuration file format. If you use [tombi](https://github.com/tombi-toml/tombi) you get completion,
hover documentation, and validation in your editor. Either add the directive comment shown above, or register it in `tombi.toml`:

```toml
[[schemas]]
path = "tocat.schema.json"
include = ["tocat.toml", ".tocat.toml"]
```

### Logging

Logs go to stderr by default. Verbosity can be set by repeatedly using `-v`, explicitly via `--log-level`, or by setting `log-level` in the config file.
`RUST_LOG` overrides all of these options and can perform per-target levels.

```console
$ RUST_LOG=tocat=trace,tokio=warn tocat tcp-listen:9000 -
```

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
