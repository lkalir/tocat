# Configuration resolution

There is one merged description of a run, and everything downstream reads only
that. The command line and the config file are two ways of writing the same
thing rather than two parallel mechanisms.

## Order of operations

Startup is ordered around two constraints that are easy to break by accident.

A `tracing` subscriber can only be installed once, but configuration loading can
fail and needs to report it. So a scoped bootstrap subscriber is installed
first, at whatever level `-v` asked for, and is dropped before the real sinks
are built from the merged config.

`--list-plugins` and `--dump-config` answer questions about the configuration,
so they exit before anything opens a socket or a file. `--list-plugins` goes
first of all, before the config file is even read, since the answer cannot
depend on it.

## Precedence

1. A config file is located: `--config PATH`, or `tocat.toml` then `.tocat.toml`
   in the working directory, unless `--no-config` was given.
2. Command-line values replace file values field by field. An endpoint given as
   a flag or a positional replaces the file's, `-b` replaces `buffer-size`, `-P`
   replaces `progress`.
3. Plugin entries are the exception to replace: they accumulate, in the order
   file, then inline positionals, then `-p`. `--no-plugins` drops the file's
   list and keeps the rest.
4. The log level is the explicit `--log-level` if given, otherwise the higher of
   `-v` and the file's `log-level`. It is written back into the config, so the
   dump shows what will actually be used.
5. `RUST_LOG` is applied on top of the default stderr sink when the filter is
   built.

`--dump-config` renders the merged result as TOML, and is exactly what the rest
of the program sees. It is worth keeping it that way rather than letting it
drift into a pretty printer: it is the answer to "what is tocat actually about
to do".

## Two spellings, one meaning

An endpoint can be written as a string (`source = "tcp-listen:9000,fork"`) or as
a table (`[source]` with `type`, `host`, `port`, `fork`). Both resolve to the
same `EndpointSpec`, and the string form in the file goes through the same
parser as the string form on the command line, so adding an option means adding
it once, next to the field it sets, and both spellings and the error messages
follow.

The serde representation is arranged to keep those two forms honest: the
endpoint enum is internally tagged on `type`, and each variant is a newtype over
the transport's own struct, which flattens into the same table it would have
produced inline.

Plugin entries work the same way. `[[plugin]]` carries `name`, `direction`, `as`
and `detach`, with the plugin's own options flattened alongside them, and the
compact command-line grammar produces exactly that structure: a bare key is
`true`, a value that parses as an integer becomes one, `true` and `false` become
booleans, and anything else stays a string. The plugin's config type decides the
real shape from there, which is why `ByteSize` and `Interval` accept both a
number and a string.

Keys are matched leniently in one direction only: case is ignored and dashes and
underscores are removed for identifiers, while values stay untouched. A
normalized string is never stored, forwarded or displayed. That keeps
`max-connections` and `max_connections` the same option without ever
second-guessing a path, a command or a label. Where a candidate matches no
declared spelling it is passed through untouched rather than guessed at, which
is what keeps a plugin's own serde aliases working.

## Validation

tocat is the validator, and everything it can check it checks before opening an
endpoint: unknown top-level keys, unknown schemes, options belonging to another
scheme, unknown plugins, options no plugin declares, contradictory ones, and
channel targets that cannot be opened.

`tocat.schema.json` is a convenience on top of that, not a second source of
truth. It gives completion, hover documentation and validation in an editor that
speaks JSON Schema. It is written as one complete variant per known plugin
rather than a shared base plus conditional fragments, because editors only offer
completions from the schema they resolve and anything behind `allOf` or
`if`/`then` is invisible to them. It deliberately accepts plugin entries it does
not know about, since the plugin set depends on the cargo features the binary
was built with.

When an option is added or renamed, the schema is updated alongside it. The
schema being wrong is a documentation bug rather than a behaviour change.
