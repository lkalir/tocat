# Logging

Logs go to stderr by default, in a compact format. Verbosity can be set by repeating `-v` (once for debug, twice for trace), explicitly with
`--log-level`, or with `log-level` in the config file. An explicit `--log-level` wins; otherwise the higher of `-v` and the config file's level is used.

```console
$ RUST_LOG=tocat=trace,tokio=warn tocat tcp-listen:9000 -
```

`RUST_LOG` is applied on top of the default stderr sink, and can select per target. Two targets are worth knowing: `tocat` for the relay itself and
`plugin` for records emitted by stages, each of which carries a `stage` field naming the instance. So `RUST_LOG=plugin=info` gives you `rate` reports
and `process` diagnostics without the rest.

```console
$ RUST_LOG=plugin=info tocat -f file:big.iso -t tcp:host:9000 -p 'rate,interval=1s'
```

Additional sinks can be configured in the config file. Any number of `[[log]]` entries are composed into one subscriber, each with its own format and
level.

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

| Key         | Applies to | Values                                                                           |
|-------------|------------|-----------------------------------------------------------------------------------|
| `type`      | both       | `stderr` or `file`                                                               |
| `format`    | both       | `compact` (the default), `pretty` or `json`                                      |
| `level`     | both       | This sink's level. Defaults to the resolved global level                         |
| `path`      | file       | Where to write. Required                                                         |
| `rotation`  | file       | `never` (the default), `minutely`, `hourly` or `daily`                           |
| `max_files` | file       | How many rotated files to keep                                                   |
| `truncate`  | file       | Truncate on open rather than appending. Only applies when `rotation = "never"`   |

Declaring any `[[log]]` entry replaces the default stderr sink rather than adding to it, so include one of `type = "stderr"` if you want both. Note
that `RUST_LOG` is not applied to sinks declared this way: each takes its own `level` (or the global one) and nothing else.

Under rotation the `path` is split into a directory and a filename prefix, and file writing is non-blocking, with the worker flushed at exit.

Two other things share the stderr stream. Log records erase the [`--progress`](progress.md) line before printing and let the next tick redraw it, under
the same lock. Traffic dumps are not logs at all: they are written by the [`tee`](plugins/tee.md) plugin, go wherever that entry points, and are
unaffected by the log level.
