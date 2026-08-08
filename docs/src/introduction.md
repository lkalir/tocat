# Introduction

tocat is a socat-inspired relay built on tokio. It connects two endpoints
(sockets, files, subprocesses, stdio, etc.) and copies bytes between them in
both directions. Unlike socat, connections can be described in a TOML config
file with editor completion and validation, and the bytes in flight can be
passed through a pipeline of plugins.

```console
$ tocat tcp-listen:8080,fork tee,format=hex tcp:example.com:80
```

Everything tocat does is built out of four ideas:

- an **endpoint** is a scheme, a target, and a set of options, and a run has
  exactly two of them, a source and a sink
- a **path** is one direction of the relay, source to sink or sink to source,
  and each path is copied independently
- a **stage** is one instance of a plugin, on one path of one connection, with
  its own options and its own state
- a **unit** is what a stage emits between boundaries, and is what becomes one
  write, one datagram, or one call to the stage below

## How this book is organised

- The **user guide** is the reference for running tocat: how a run is described,
  every endpoint scheme, every plugin that ships with it, and the cross-cutting
  subjects of buffers, progress reporting, configuration files and logging.
- The **plugin API** section describes the contract in the `tocat-api` crate,
  and is what you want if you are writing a plugin.
- The **design** section describes how tocat is put together and why, and is
  what you want if you are changing tocat itself.

For what is implemented and what is still missing, see the status section in the
repository README.

## Conventions

Command-line examples are shown as shell sessions:

```console
$ tocat --from - --to tcp:localhost:9000
```

Options are given in tables. An option written `key=VALUE` takes a value, and
one written on its own is a flag, which means true when present.

Identifiers are matched leniently everywhere: scheme names, endpoint option
keys, plugin names, plugin option keys and enum values all ignore case and treat
dashes and underscores as noise, so `max-connections`, `max_connections` and
`MaxConnections` are one option. Values (paths, commands, labels, aliases) are
never touched.

Sizes take binary suffixes (`b`, `k`/`kb`/`kib`, `m`/`mb`/`mib`,
`g`/`gb`/`gib`), so `64k` is 65536 and `1M` is a mebibyte. A bare number is
bytes. In a config file a size may be written as a number or as a string.

Durations take `ns`, `us`, `ms`, `s`, `m`, `h`, `d`, `w`, and may be compounded
(`1m30s`). A bare number is seconds. The `rate` plugin's `interval` option has
its own smaller grammar, described on [its page](guide/plugins/rate.md).
