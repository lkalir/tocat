# Overview

Plugins implement the `Plugin` trait from the `tocat-api` crate. A plugin is a
synchronous byte transformer: it is handed a chunk that arrived from upstream
and decides what to forward downstream. Anything that touches the outside world
(writing a dump file, emitting a log line, waiting) is not performed by the
plugin. It is queued as an effect and applied by the host after the call
returns.

Three properties follow from that split, and they are the reason for it:

- **Testable.** A stage is a function from a chunk and its state to bytes out
  plus a list of requested effects. A test drives it with chunks and asserts on
  both, with no runtime, no sockets and no temporary files. See
  [Testing a stage](testing.md).
- **Off the async runtime.** Nothing in a stage awaits, so no stage can stall a
  reactor thread by accident, and all I/O stays on the host's runtime where it
  can be batched and overlapped with the downstream write.
- **Portable to a guest.** It is the shape a WASM guest has to take, and the
  [`wasm`](../guide/plugins/wasm.md) stage is that: it implements `Plugin` by
  forwarding each call into a module and applying what comes back, and nothing
  in the relay knows the difference. Because effects are queued rather than
  performed, a guest needs no host imports at all, which is what makes "imports
  nothing" an enforceable rule rather than a wish. See
  [The guest ABI](wasm-abi.md).

The same split covers time. A stage cannot await and cannot read a clock, so one
that needs time rather than traffic to drive it declares a period and is called
back on it. See [Ticks and timers](ticks.md).

A few stages cannot satisfy that contract at all. A subprocess decides nothing
synchronously and may emit bytes belonging to chunks it was given long ago, so
it is *described* to the host and *run* by it. See
[Host plugins](host-plugins.md).

## Vocabulary

| Term    | Meaning                                                                                             |
| ------- | --------------------------------------------------------------------------------------------------- |
| plugin  | The implementation, registered by name, named on the command line and in the config file            |
| factory | `PluginFactory`, which validates one entry's options and builds one instance                        |
| stage   | One instance, on one path of one connection, with its own options and its own state                 |
| path    | One direction of the relay: source to sink, or sink to source                                       |
| chunk   | The bytes handed to a stage in one call                                                             |
| unit    | What a stage emits between [boundaries](units.md), and what becomes one write, message or call      |
| segment | A run of inline stages the host drives on one task. A `detach` cuts one segment into two            |
| effect  | Something the stage wants the host to do: write a dump, log, wait, stop reading, rearm its schedule |

## The crate

`tocat-api` depends only on `serde` and `serde_json`. It has no tokio, no I/O
and no knowledge of the relay, which is what lets a plugin be built and tested
on its own.

| Module      | Holds                                                                                   |
| ----------- | --------------------------------------------------------------------------------------- |
| `plugin`    | `Plugin`, `PluginFactory`, `Stage`, `Ctx`, `BuildCtx`, `Emission`, `Emit`, `EffectSink` |
| `pipeline`  | `Pipeline`, `Chain`, `Segment`, `Registry`, `Emitted`                                   |
| `channel`   | `ChannelId`, `ChannelTarget`, `HostBuilder`                                             |
| `error`     | `PluginError` and the crate's `Result`                                                  |
| `forgiving` | The deserializer wrapper that makes option keys and enum values lenient                 |
| `normalize` | The one spelling rule every identifier in tocat is matched by                           |
| `size`      | `ByteSize`: one grammar for every byte count                                            |
| `interval`  | `Interval`: one grammar for every duration                                              |

## Where to go next

- [The Plugin trait](plugin-trait.md) for the calls a stage receives and what it
  may do in them.
- [Options and building](building.md) for the factory, config deserialization
  and registration.
- [Units and boundaries](units.md) for controlling how what a stage emits is
  delivered.
- [Ticks and timers](ticks.md) for stages driven by time rather than by traffic.
- [Effects and channels](effects.md) for reaching the outside world.
- [Host plugins](host-plugins.md) for stages the trait cannot express.
- [Testing a stage](testing.md) for driving one without a relay.
