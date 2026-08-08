# Configuration

tocat looks for a `tocat.toml`, then a `.tocat.toml`, in the working directory,
and merges what it finds with the command line, which wins. `--config PATH`
selects a file explicitly, `--no-config` skips the search entirely, and
`--dump-config` prints the merged result as TOML and exits.

Below is a configuration that accepts up to 64 connections on port 9000,
connects via TCP to another server, and keeps a raw log of the data coming back.

```toml
#:schema ./tocat.schema.json

log-level = "info"
buffer-size = "256KiB"

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

Endpoints can also be written as the string shorthand from the command line. The
two forms parse to the same thing, so an option means what it means in either.

```toml
#:schema ./tocat.schema.json

source = "tcp-listen:9000,fork"
sink = "tcp:localhost:8080"
```

## Keys

| Key           | Meaning                                                                |
| ------------- | ---------------------------------------------------------------------- |
| `source`      | The source endpoint, as a string or a table                            |
| `sink`        | The sink endpoint, as a string or a table                              |
| `[[plugin]]`  | Pipeline entries, in order. See [Plugins](plugins.md)                  |
| `buffer-size` | Bytes per copy, as a number or a string. See [Buffers](buffers.md)     |
| `progress`    | `never` (the default), `auto` or `always`. See [Progress](progress.md) |
| `log-level`   | `off`, `error`, `warn`, `info`, `debug` or `trace`                     |
| `[[log]]`     | Log sinks. See [Logging](logging.md)                                   |

Unknown top-level keys are an error rather than being ignored, as unknown
options are everywhere else.

## Merging

Command-line values replace file values field by field: an endpoint given as a
flag or a positional replaces the file's, `-b` replaces `buffer-size`, `-P`
replaces `progress`.

Plugins are the exception to "replace": they accumulate. The order is the file's
entries, then the inline positional pipeline, then every `-p`, so a standing
capture can live in `tocat.toml` while an ad-hoc stage is added for one run.

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

`--no-plugins` drops the file's list and keeps the `-p` entries, which is how
you run a standing configuration without its capture for one invocation. It
cannot be combined with `--no-config`, which has already dropped everything.

Note that `tocat.toml` and `.tocat.toml` are in the repository's `.gitignore`,
since they are how the project's own ad-hoc runs are configured.

## Schema

`tocat.schema.json` is a JSON schema for the configuration file format. If you
use [tombi](https://github.com/tombi-toml/tombi) (in the dev shell) you get
completion, hover documentation, and validation in your editor. Either add the
directive comment shown above, or register it in `tombi.toml`:

```toml
[[schemas]]
path = "tocat.schema.json"
include = ["tocat.toml", ".tocat.toml"]
```

If `tocat` was compiled with the `schema` feature (on by default), use the
`--dump-schema` flag to print the schema to stdout.

```console
$ tocat --dump-schema > tocat.schema.json
```

The schema describes every endpoint scheme and every plugin that ships with
tocat, one complete variant each rather than a shared base plus conditional
fragments, because editors only offer completions from the schema they resolve.
It also accepts entries for plugins it does not know about, which is necessary
since the plugin set depends on the features the binary was built with.

tocat itself is the real validator: it rejects unknown options, unknown plugins
and contradictory ones at startup, before either endpoint is opened. The schema
is a convenience on top of that, and needs updating alongside any option that is
added or renamed.
