# Buffers

tocat copies through one buffer per direction per connection, 256 KiB by
default. `-b` on the command line or `buffer-size` in the config file changes
it, as a byte count or with a binary suffix.

```console
$ tocat -b 1MiB file:/big.iso tcp:host:9000
```

```toml
buffer-size = "64k"
```

Buffers are page-aligned and allocated whole pages at a time, though the extra
is never exposed: asking for 1000 bytes gives a 1000-byte slice.

The buffer size is the largest chunk a stage can be handed, so it also sets the
granularity of anything acting between reads. That is why
[`throttle`](plugins/throttle.md) paces in units of chunks, and why a `block`
size far below the buffer runs the rest of the pipeline many times per read. On
a datagram path it is also the message ceiling: one receive is one datagram, and
anything longer is truncated.

Under `fork` this multiplies, since every connection has its own buffer each
way. tocat warns at startup when the buffer size and the connection ceiling
together allow more than a gibibyte of copy buffers, which `-b 1MiB` against the
default ceiling of 1024 already does.

## Kernel pipe buffers

Kernel pipe buffers are a separate resource, and they cap throughput
independently: a pipe holds 64 KiB by default, so a writer stalls once that much
is unread however large tocat's own buffer is. Where the pipe belongs to tocat
it is enlarged to match the copy buffer automatically:

- stdin and stdout, whenever they turn out to be pipes, which is what
  `tocat … | pv` makes of fd 1
- the stdin and stdout of `exec:`, `system:` and [`process`](plugins/process.md)
  children, which always are

Named FIFOs are left alone unless you ask with `size=`, because their buffer is
shared with whoever else has the path open.

Pipe resizing is Linux-only and best-effort, and it is done by asking rather
than by checking first. The kernel rounds up to a power of two; a descriptor
that is not a pipe, or one with more buffered than the new size would hold, is
left alone and noted at debug. The one case reported louder is refusal to exceed
`/proc/sys/fs/pipe-max-size` (1 MiB by default) without `CAP_SYS_RESOURCE`,
which warns and carries on at the existing size.
