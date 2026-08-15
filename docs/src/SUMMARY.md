# Summary

[Introduction](introduction.md)

# User guide

- [Installation](guide/installation.md)
- [Invocation](guide/invocation.md)
- [Endpoints](guide/endpoints.md)
  - [exec and system](guide/endpoints/exec.md)
  - [file](guide/endpoints/file.md)
  - [pipe](guide/endpoints/pipe.md)
  - [pty and pty-exec](guide/endpoints/pty.md)
  - [stdio](guide/endpoints/stdio.md)
  - [tcp and tcp-listen](guide/endpoints/tcp.md)
  - [tty](guide/endpoints/tty.md)
  - [udp and udp-listen](guide/endpoints/udp.md)
  - [unix and unix-listen](guide/endpoints/unix.md)
- [Plugins](guide/plugins.md)
  - [base64 and unbase64](guide/plugins/base64.md)
  - [block](guide/plugins/block.md)
  - [compress and decompress](guide/plugins/compress.md)
  - [frame and unframe](guide/plugins/frame.md)
  - [hash](guide/plugins/hash.md)
  - [hexify](guide/plugins/hexify.md)
  - [limit](guide/plugins/limit.md)
  - [process](guide/plugins/process.md)
  - [rate](guide/plugins/rate.md)
  - [tee](guide/plugins/tee.md)
  - [throttle](guide/plugins/throttle.md)
  - [timeout](guide/plugins/timeout.md)
  - [wasm](guide/plugins/wasm.md)
- [Buffers](guide/buffers.md)
- [Progress](guide/progress.md)
- [Configuration](guide/configuration.md)
- [Logging](guide/logging.md)

# Plugin API

- [Overview](api/overview.md)
- [The Plugin trait](api/plugin-trait.md)
- [Options and building](api/building.md)
- [Units and boundaries](api/units.md)
- [Ticks and timers](api/ticks.md)
- [Effects and channels](api/effects.md)
- [Host plugins](api/host-plugins.md)
- [The guest ABI](api/wasm-abi.md)
- [Testing a stage](api/testing.md)

# Design

- [Architecture](design/architecture.md)
- [The data path](design/data-path.md)
- [Pipeline construction](design/pipeline.md)
- [The datagram model](design/datagrams.md)
- [Configuration resolution](design/configuration.md)
- [Lifecycle and shutdown](design/lifecycle.md)
