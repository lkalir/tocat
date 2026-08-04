# `compress` and `decompress` - zstd

Behind the `compress` cargo feature. Compression is asymmetric, so pair the two rather than using `both`, which would compress both paths:

```console
$ tocat - compress:forward decompress:reverse tcp:relay.internal:9000
```

The far end of the link runs the mirror image, and the two relays form a compressed tunnel over an otherwise plaintext hop.

| Option     | Plugin       | Description                                                                              |
|------------|--------------|--------------------------------------------------------------------------------------------|
| `level=N`  | `compress`   | zstd level, 1 to 22. Higher is smaller and slower. Default is 3, which is zstd's own default |
| `flush`    | `compress`   | Flush after every chunk so bytes reach the peer immediately. On by default; costs ratio     |
| `report`   | both         | Log the compression or expansion ratio when the stream ends                                 |

A relay must not sit on bytes waiting for a better compression window, so `flush` is on by default: whatever arrived is on the wire before the call
returns. Turn it off for bulk transfers where throughput matters and nobody is waiting on a prompt, and output then appears only as zstd fills its own
buffer and at end of stream.

Both default to `detach`, since they are expensive enough per byte to be worth their own task. `detach=false` is accepted if you would rather have them
inline.

`compress` writes a zstd epilogue at end of stream; without it a peer's decoder rejects the stream as truncated. Two paths never reach end of stream, a
datagram source and a held [`pipe`](../endpoints/pipe.md), so neither the epilogue nor `report` happens there. Neither stage preserves message
boundaries, so a datagram destination on the same path draws a warning.
