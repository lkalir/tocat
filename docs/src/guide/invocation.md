# Invocation

```
$ tocat -h
socat-inspired relay

Usage: tocat [OPTIONS] [SPEC]...

Arguments:
  [SPEC]...  SOURCE [PLUGIN ...] SINK. The outer specs are endpoints; anything between them is a pipeline entry. Slots already filled by --from/--to are skipped.

Options:
  -c, --config <PATH>       Configuration file to use.
      --no-config           Disable configuration file merging.
      --dump-config         Render the final configuration as TOML.
  -b, --buffer-size <SIZE>  Bytes per copy, e.g. 65536 or 256KiB. One buffer per direction per connection.
  -f, --from <ADDR>         Source endpoint. Fills the first positional slot.
  -t, --to <ADDR>           Sink endpoint. Fills the last positional slot.
  -p, --plugin <SPEC>       Pipeline entry: NAME[:DIRECTION][,key=value...]. Repeatable, applied in order.
      --no-plugins          Ignore plugins declared in the configuration file.
      --list-plugins        List the plugins compiled into this binary and exit.
  -P, --progress[=<WHEN>]   Draw a progress line on stderr. Bare, or 'auto', draws only when stderr is a terminal; 'always' draws regardless. [possible values: never, auto, always]
  -v, --verbose...          Simple verbosity level.
      --log-level <LEVEL>   Explicit verbosity level. [possible values: off, error, warn, info, debug, trace]
  -h, --help                Print help (see more with '--help')
  -V, --version             Print version
```

tocat can be used similarly to socat, with the source and sink endpoints given
as strings. You can also use the explicit `--from` and `--to` flags, or a
[configuration file](configuration.md).

```console
$ tocat --from - --to tcp:localhost:9000
$ tocat - tcp:localhost:9000
```

## Positional slots

There are exactly two endpoint slots, and roles are decided by position, never
by looking at the text. `--from` and `--to` fill the first and last slots; the
outer positional arguments fill whichever of those slots are still open;
whatever remains in the middle is the [pipeline](plugins.md).

```console
$ tocat SRC SINK                  # no plugins
$ tocat SRC tee compress SINK     # two entries, in that order
$ tocat -f SRC tee SINK           # one entry; SINK fills the open slot
$ tocat -f SRC -t SINK tee        # one entry; both slots already filled
```

A lone positional with both slots open fills the source, which is what the older
two-positional form did. When both endpoints come from flags or from a config
file, `-p` is the way to add a stage: a bare positional would be read as an
endpoint.

```console
$ tocat -f tcp-listen:8080,fork -t tcp:example.com:80 -p 'tee,format=hex'
```

Quoting matters at the shell, not to tocat: an entry containing spaces or shell
metacharacters (`'process,command=gzip -c'`, `"system:grep -v DEBUG | sort -u"`)
has to reach tocat as one argument.

## Flags that answer questions

These exit before any endpoint is opened.

| Flag             | Prints                                                                                                       |
| ---------------- | ------------------------------------------------------------------------------------------------------------ |
| `--list-plugins` | Each plugin compiled into this binary, with its description. Exits before the config file is even read       |
| `--dump-config`  | The merged configuration, after the config file and the command line, as TOML. Exactly what the run will use |

`-v` (repeatable: once for debug, twice or more for trace) and `--log-level`
conflict with each other; `--config` and `--no-plugins` conflict with
`--no-config`. Running with `-v` logs the resolved chains for both directions,
which is the quickest way to confirm a long command line came out in the order
you meant.

## Stopping

The first SIGINT or SIGTERM stops accepting new connections and drains what is
in flight; a second exits immediately with status 130. A relay that finishes
normally exits 0, and one that fails to start or fails mid-transfer logs the
error and exits 1.

Draining includes the pipeline. The signal reaches a plugin as end of stream, so
a stage holding buffered bytes still emits them and a stage that writes an
epilogue still writes it: interrupting `hash` prints its digest, interrupting
`compress` closes the frame, and a `block` stage flushes its partial tail. Wait
for the exit rather than sending a second signal if you want that output.

One case cannot be interrupted promptly: the synchronous copy path (chosen when
there are no plugins and both endpoints are file, pipe or stdio) checks for
shutdown between chunks, so a read that never returns, such as a FIFO with no
writer, is left behind after a short grace period rather than waited on forever.
