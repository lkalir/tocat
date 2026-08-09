# Plugins

Bytes can be passed through a pipeline of plugins on their way between the
endpoints. A plugin can watch the stream, rewrite it, or drop parts of it.

Plugins are written like endpoints (a name, an optional direction, and a set of
options) and go between the source and the sink on the command line.

```
name[:direction],option,option=value
```

```console
$ tocat tcp-listen:8080,fork tee,format=hex tcp:example.com:80
```

The same entries can be given with `-p`, which is the only way to add one when
both endpoints come from flags or a config file. Repeat it as needed; entries
apply in the order written, after any in the config file.

```console
$ tocat -f tcp-listen:8080,fork -t tcp:example.com:80 -p 'tee,format=hex'
```

Run `tocat --list-plugins` to see what your build has, and `--no-plugins` to
ignore the ones in a config file.

## What ships with tocat

| Plugin                            | Does                                  | Payload   | Runs        | Datagram paths                    |
| --------------------------------- | ------------------------------------- | --------- | ----------- | --------------------------------- |
| [`tee`](plugins/tee.md)           | Mirror the bytes to a file or stderr  | untouched | inline      | safe                              |
| [`hash`](plugins/hash.md)         | Digest what crosses this point        | untouched | inline      | safe                              |
| [`rate`](plugins/rate.md)         | Report throughput at this point       | untouched | inline      | safe                              |
| [`throttle`](plugins/throttle.md) | Hold the path to a bandwidth ceiling  | untouched | inline      | safe                              |
| [`limit`](plugins/limit.md)       | End the transfer after N bytes        | truncated | inline      | safe unless `at-limit=exact`      |
| [`block`](plugins/block.md)       | Cut the path into fixed-size records  | reframed  | inline      | warns: boundaries are its own     |
| [`compress`](plugins/compress.md) | zstd compress or decompress           | rewritten | detached    | warns                             |
| [`process`](plugins/process.md)   | Pipe the path through a child process | rewritten | own process | warns                             |
| [`timeout`](plugins/timeout.md)   | End the path once it has gone quiet   | untouched | inline      | safe                              |
| [`wasm`](plugins/wasm.md)         | Run a WebAssembly guest as a stage    | guest's   | detached    | the guest declares, default warns |

"Datagram paths" is what each stage reports about itself, and it is what tocat
checks when the destination on that path is a datagram endpoint. The default for
a stage that says nothing, including any plugin from outside the binary, is that
it is not safe.

## Writing an entry

On the command line an entry is comma-separated, as an endpoint is. A bare key
means true (`tee,append`), a value that parses as an integer becomes one, `true`
and `false` become booleans, and everything else stays a string. Sizes and
durations are therefore written as they are elsewhere (`bytes=10MiB`,
`flush=250ms`), and a value containing a comma has to go in a config file.

Option keys are matched leniently: case, dashes and underscores are noise, so
`at-limit`, `at_limit` and `atLimit` are the same option. An option the plugin
does not declare is an error rather than being ignored.

In a config file, an entry is a `[[plugin]]` table with the plugin's own options
alongside `name`, `direction`, `as` and `detach`:

```toml
[[plugin]]
name = "tee"
direction = "sink-to-source"
file = "/var/log/tocat-capture.bin"
format = "raw-binary"
```

## Direction

An entry applies to one path or both. Omitting the direction means the forward
path, from the source to the sink.

| Direction        | Aliases                                          | Meaning                               |
| ---------------- | ------------------------------------------------ | ------------------------------------- |
| `source-to-sink` | `forward`, `fwd`, `src-to-sink`, `out`, `source` | Bytes read from the source            |
| `sink-to-source` | `reverse`, `rev`, `sink-to-src`, `in`, `sink`    | Bytes read from the sink              |
| `both`           | `bidi`, `bidirectional`, `duplex`, `all`         | Both paths, as two separate instances |

`both` builds two independent instances, one per path, so per-direction state
(byte offsets, codec state) never leaks across paths. That also means two of
whatever the stage counts: `limit,bytes=1MiB,direction=both` lets a megabyte
past in each direction rather than a megabyte between them, and a ticking stage
declared that way builds a timer per path per connection. Asking for both is
therefore worth doing on purpose, which is why it is no longer what you get by
saying nothing.

An asymmetric stage is not its own inverse, so `both` is wrong for it and the
pair is written out:

```console
$ tocat - compress decompress:reverse tcp:relay.internal:9000
```

The command line is a picture of the wire, read left to right, and the reverse
path reads it right to left:

```
tocat SRC  a  b  c  SINK

source -> sink:   SRC -> a -> b -> c -> SINK
sink -> source:   SRC <- a <- b <- c <- SINK
```

So a stage written earlier sits nearer the source, and bytes coming back from
the sink reach the later stages first. Write the stages in the order the forward
path would see them, and the reverse path nests correctly, which is what you
want for anything that wraps the payload.

The `source`/`sink` aliases are accepted but read badly: `tee:sink` means
sink-to-source, not "tee at the sink". Prefer `forward` and `reverse`.

## Common options

Two options are handled by tocat rather than the plugin, and work on any entry.

| Option    | Description                                                                                                                         |
| --------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `as=NAME` | Name this instance. Appears in logs and in `tee` headers. Without it a stage is named after its plugin, with `#1`, `#2` for repeats |
| `detach`  | Run this stage on its own task. Costs a copy and a wakeup per chunk, so it is only worth it for stages that are expensive per byte  |

The `#n` suffixes are only added where a name would otherwise appear twice on
the same path, so a lone `tee` stays `tee`.

`compress` and `decompress` default to detached, and `detach=false` on one is
honoured. `process` runs as a child process and always has its own task, so
`detach=false` on one is rejected rather than ignored.

If nothing is declared on a path, that direction is copied straight through with
no plugin machinery in the way.

## Datagrams

On a byte stream a chunk is an arbitrary slice, and a stage may buffer, split or
coalesce it freely. On a datagram path the chunk *is* the message: one call per
datagram, and each unit the stage emits is sent as exactly one datagram. A stage
that holds bytes across calls, or emits two messages' worth from one, will
produce well-formed datagrams containing nonsense.

Every stage declares whether it may sit on such a path. When one that may not is
on a path whose *destination* is a datagram endpoint, tocat names it, warns, and
relays anyway:

```
stage may not preserve message boundaries; datagrams send to this endpoint may be split, merged, or malformed
```

That is a warning rather than a refusal because rewriting the message stream is
sometimes the point: `block,size=1400` on a UDP path emits one datagram per
block, which is a reasonable thing to ask for and still not a preservation of
what the peer sent.

The check is against the destination, not the source. Sending datagrams *into* a
stream sink is unremarkable and draws no warning. So
`udp-listen:9000 compress:forward tcp:collector:9000` is fine, while
`udp-listen:9000 compress udp:peer:9000` will warn. A `process` stage always
warns: its stdin and stdout are byte streams, so boundaries are gone the moment
bytes cross the pipe.

One more thing a datagram sink does silently: an empty emission is not sent. At
end of stream a pipeline is drained once more, and on a datagram sink that would
otherwise put a spurious zero-length message on the wire, which many peers read
as a close. The cost is that a genuine zero-length datagram is dropped rather
than forwarded.
