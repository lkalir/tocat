# `wasm` - run a WebAssembly guest

Runs a `.wasm` module as a stage. The guest sits where it was written, takes a
direction, and forwards or rewrites what it is handed like any other stage.

```console
$ tocat tcp-listen:8080,fork 'wasm,module=/usr/lib/tocat/redact.wasm' tcp:example.com:80
```

```toml
[[plugin]]
name = "wasm"
module = "/usr/lib/tocat/redact.wasm"
direction = "source-to-sink"
config = { patterns = ["authorization", "cookie"] }
```

| Option            | Description                                                                                     |
| ----------------- | ----------------------------------------------------------------------------------------------- |
| `module=PATH`     | The module to load. Required. Aliases: `path`, `file`                                           |
| `config=JSON`     | Handed to the guest verbatim, once. tocat does not look inside it                               |
| `fuel=N`          | Instructions one call may cost before the guest is trapped. Default 100000000. `0` is unmetered |
| `memory-max=SIZE` | Ceiling on the guest's linear memory. Default 64MiB, per instance                               |

`config` is the guest's own options, and the guest validates them and writes its
own error messages. In a config file it is a table, which is the readable form.
On the command line it is a JSON string, and since commas separate options
there, anything with more than one key has to go in a config file.

## What a guest cannot do

A guest imports nothing at all. No clock, no files, no sockets, no host
functions of any kind, and no way to spawn anything. That is not a promise the
documentation makes, it is a property the loader enforces: a module that imports
so much as WASI is refused when it is loaded, naming the import.

It does not need any of them. A stage decides what to forward and queues
everything else for the host to perform, so the things a guest would reach for
are already messages rather than calls: it can forward bytes, frame them into
units, log, ask to wait, ask to stop, and ask for its tick schedule to be
restarted. The one capability the ABI does not carry yet is side channels, so a
guest that wants to record something logs it. See
[The guest ABI](../../api/wasm-abi.md) for how to write one,
`crates/tocat-wasm-sdk` for writing one in Rust, and `sdk/wasm/` for a C and C++
SDK. Working guests for all three are in the repository.

Two consequences worth stating plainly. A guest cannot exfiltrate the payload,
because it has nowhere to put it. And a guest cannot be a
[host plugin](../../api/host-plugins.md): spawning is a host capability by
construction, so a subprocess stage will always be native.

## Cost

A guest is expensive enough per byte that it defaults to `detach`, running on
its own task. `detach=false` still works for one cheap enough not to want its
own.

The module is compiled once per process, at startup, however many stages and
connections name it. What is per stage is an instantiation and a linear memory,
and a stage is per direction per connection: under `fork` with the default
`direction=both`, a hundred clients is two hundred instances. The memory ceiling
is the number that multiplies, so `memory-max=64MiB` against a thousand
connections is a promise you may not want to make.

Fuel is per call, not per connection, so the budget is "how much work may one
chunk cost". A guest that runs out is trapped, which fails that direction: the
bytes it was handed have gone nowhere, so there is nothing else honest to do.
The default is generous enough for any straightforward per-byte transform of a
full buffer and tight enough that a runaway loop is caught in milliseconds.
`fuel=0` turns metering off, and with it the guarantee that a guest cannot hang
the relay: `on_bytes` runs on the copy task, and nothing else on that task moves
while a guest is inside it.

## When a guest does nothing

A guest that loads, runs, and silently drops the stream is almost always handing
the host offsets into its own arena rather than addresses in its linear memory.
Nothing traps, because both sides read memory that exists, and the outbox the
host reads is whatever happens to be at the address the guest named, which is
usually zeros, which decodes as "drop everything". See
[Pointers are absolute](../../api/wasm-abi.md#pointers-are-absolute).

Run the stage at `-v` and the resolved chain will confirm it is where you think
it is. Beyond that, a guest is easiest to debug outside the relay: load the
module, call the exports by hand, and decode the outbox after each call.

## Datagrams

A guest declares for itself whether it preserves message boundaries, and one
that says nothing is assumed not to, which is the same default every native
stage has. A guest that emits one unit per call it was given one is safe; one
that buffers across calls or reframes what it was handed is not, and gets the
usual warning when the destination on its path is a datagram endpoint.
