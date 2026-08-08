# Architecture

tocat is a small amount of machinery arranged so that the common case stays
cheap. This section describes how the pieces fit and why, and is written for
people changing tocat itself.

## Crates

```
crates/
  tocat-api        the plugin contract: traits, pipeline, registry, shared grammars
  tocat-plugins    every native plugin, one module each, behind cargo features
  tocat-cli        the `tocat` binary: endpoints, config, relay, pumps, host services
  tocat-wasm-abi   the guest wire format, shared by the host, the SDK and the C header
  tocat-wasm-sdk   writing a guest in Rust
  tocat-wasm-shell a REPL for poking at a guest without a relay
```

The direction of the dependencies is the design. `tocat-api` depends on serde
and nothing else: no tokio, no I/O, no relay. A plugin therefore depends on the
contract rather than on the program, which is what lets it be built and tested
alone, and is what a WASM guest needs, since a guest cannot link the relay at
all. `tocat-wasm-abi` goes one further and depends on nothing, because it is
also compiled into guests and translated into C.

The binary depends on `tocat-plugins` as a whole and never on one plugin, and
the facade exposes nothing but [`register_native`], so a plugin can move, change
its dependencies or disappear behind a feature without anything above noticing.

## Modules in the binary

| Module     | Owns                                                                                        |
| ---------- | ------------------------------------------------------------------------------------------- |
| `cli`      | The argument surface and the positional layout rules                                        |
| `config`   | The config file, the merge with the CLI, and the compact plugin-entry grammar               |
| `endpoint` | One module per transport, plus the shared option grammar, stream shapes and system plumbing |
| `relay`    | Connection lifecycle: validation, listening, forking, and which copy path a run takes       |
| `pump`     | One direction: segments, links, ticks, and the three per-direction fast paths               |
| `host`     | The host half of the plugin API: channel plan, effect staging, channel writers              |
| `buffer`   | Page-aligned copy buffers                                                                   |
| `progress` | The meter, the read-half counter, and the painter                                           |
| `logging`  | Declarative log sinks composed into one subscriber                                          |
| `shutdown` | Signal handling and the drain-then-exit contract                                            |
| `child`    | Spawning, shared by the `exec:`/`system:` endpoints and the `process` plugin                |

`endpoint` is deliberately one file per transport. Adding a scheme is a new
file, one variant on `EndpointSpec`, and one line in the scheme table in
`endpoint::parse`; each module owns its own fields, its parse, its label and its
connect.

## The shape of a run

1. **Parse and merge.** The config file (if any) and the command line become one
   `Config`, with the command line winning. `--dump-config` prints exactly this
   and exits. See [Configuration resolution](configuration.md).
2. **Resolve.** Endpoint specs become `EndpointSpec`s and the merged plugin list
   stays as declarations. Anything wrong with an endpoint is an error here.
3. **Validate.** `Relay::new` builds both chains once, purely to discover which
   side channels they want, opens those channels, and freezes the plan. A
   misspelled plugin, an option no plugin declares, a `detach = false` on a
   subprocess stage or an unwritable dump file all fail here, before either
   endpoint is touched. The instances built for this pass are dropped. See
   [Pipeline construction](pipeline.md).
4. **Warn.** Three things are checked once, at this point, and warned about
   rather than refused: a stage that may not preserve message boundaries on a
   path whose destination is a datagram endpoint, a dump pointed at stderr while
   `--progress` is drawing there, and a buffer size and connection ceiling that
   together allow more than a gibibyte of copy buffers.
5. **Connect.** The endpoints are opened, or one of them listens and accepts,
   once or repeatedly under `fork`. Each connection builds its own chain pair;
   only the channel handles are shared.
6. **Relay.** The run takes the cheapest path that fits: see
   [The data path](data-path.md).
7. **Finish.** End of stream on a path cascades through its stages, flushes and
   closes the sink. Channels are flushed on the way out. See
   [Lifecycle and shutdown](lifecycle.md).

## Design commitments

These hold across the codebase and are the reason for most of the local
decisions.

- **Errors are loud and early.** Everything decidable from the configuration is
  decided before a byte moves. Unknown options are rejected rather than ignored,
  because a silently ignored option is a wrong relay that looks right.
- **Roles come from position, never from text.** The endpoint slots are filled
  positionally, and a pipeline entry is never promoted to an endpoint because it
  happens to look like one. Guessing would make the meaning of a command line
  depend on which plugins the binary was built with.
- **One spelling rule.** Schemes, endpoint options, plugin names, plugin option
  keys and enum values are all matched with case, dashes and underscores
  ignored, and a normalized string is never stored, forwarded or displayed.
  Free-form values never come near it.
- **Paths are independent.** Every stage instance belongs to one path of one
  connection. There is no shared mutable state between directions, which is what
  makes `both` safe for stateful stages and keeps `fork` off locks on the data
  path.
- **The host owns the outside world.** Stages do not touch files, sockets,
  clocks or tasks. Everything that does is requested and performed by the host,
  which keeps stages testable and portable and keeps I/O policy in one place.
- **Pay for what you ask for.** A run with no plugins builds no pipeline
  machinery, a segment with nothing ticking builds no timer, a stage that
  forwards untouched copies nothing, and an unframed emission allocates no
  boundary list. Anything that costs per byte or per connection is opt-in and
  documented as such.
