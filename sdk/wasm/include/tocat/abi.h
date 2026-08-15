/*
 * The tocat WebAssembly guest ABI, version 2.
 *
 * Generated from crates/tocat-wasm-abi by cbindgen. Do not edit: run
 *
 *     cargo run -p tocat-wasm-abi --features generate --bin tocat-abi-header
 *
 * Include <tocat/tocat.h> rather than this file. That one adds the exports,
 * the arena and the helpers; this one is only the shape of the wire.
 */

#ifndef TOCAT_ABI_H
#define TOCAT_ABI_H

#include <stdint.h>

/*
 Bumped for any change to what the host reads or to what a value means: the
 struct below, the set of exports, or the interpretation of either. A guest
 reporting a different version is refused when it loads rather than being
 read as garbage.

 The check is exact equality in both directions, which is the point. A host
 that silently accepted a newer guest would honour the parts of its contract
 it recognised and ignore the rest, and the one it ignored would be a
 requirement the guest cannot work without.
 */
#define TOCAT_ABI_VERSION 2

/*
 Bytes the host reads at `tocat_outbox()`.
 */
#define TOCAT_OUTBOX_LEN 48

/*
 Bytes per record in the log array.
 */
#define TOCAT_LOG_RECORD_LEN 12

/*
 Forward nothing. Emitting nothing means the same thing; this exists so that
 a filter can say it on purpose.
 */
#define TOCAT_EMIT_PENDING 0

/*
 Forward the input unchanged. The host does not read the guest's bytes at
 all, and nothing is copied in either direction.
 */
#define TOCAT_EMIT_PASSTHROUGH 1

/*
 Forward `bytes`, framed by `bounds`.
 */
#define TOCAT_EMIT_BUFFERED 2

/*
 Restart this stage's tick schedule from now.
 */
#define TOCAT_FLAG_REARM (1 << 0)

/*
 End the path: upstream end of stream arriving early, and a success.
 */
#define TOCAT_FLAG_HALT (1 << 1)

/*
 Wait `pace_ns` before reading upstream again.
 */
#define TOCAT_FLAG_PACE (1 << 2)

/*
 Fail the path, with `message` as the reason.
 */
#define TOCAT_FLAG_ERROR (1 << 3)

/*
 Mask for the boundary effect in `tocat_boundaries`: bits 0 and 1.
 */
#define TOCAT_BOUNDARIES_MASK 3

/*
 The units this stage was given do not reach the stage below. Anything that
 buffers across calls, splits, or coalesces.
 */
#define TOCAT_BOUNDARIES_FUSE 0

/*
 One unit in, one unit out.
 */
#define TOCAT_BOUNDARIES_PRESERVE 1

/*
 One unit in, one unit out, and the boundary is also written into the bytes,
 so it survives a stage below that fuses. What `frame` does.
 */
#define TOCAT_BOUNDARIES_SEAL 2

/*
 The units below are read out of the bytes rather than inherited from above,
 so the ones from above do not survive. What `unframe` does.
 */
#define TOCAT_BOUNDARIES_SPLIT 3

/*
 Mask for the requirement in `tocat_boundaries`: bits 2 and 3.
 */
#define TOCAT_NEEDS_MASK 12

/*
 The stage works on any path.
 */
#define TOCAT_NEEDS_NOTHING 0

/*
 Every call must carry one whole message, so boundaries have to reach this
 stage from the endpoint above or from a `TOCAT_BOUNDARIES_SPLIT` stage.
 */
#define TOCAT_NEEDS_UPSTREAM (1 << 2)

/*
 The units this stage emits must reach the endpoint below or a
 `TOCAT_BOUNDARIES_SEAL` stage, or what it emitted cannot be read back.
 */
#define TOCAT_NEEDS_DOWNSTREAM (1 << 3)

/*
 Both of the above.
 */
#define TOCAT_NEEDS_BOTH (TOCAT_NEEDS_UPSTREAM | TOCAT_NEEDS_DOWNSTREAM)

#define TOCAT_TRACE 0

#define TOCAT_DEBUG 1

#define TOCAT_INFO 2

#define TOCAT_WARN 3

#define TOCAT_ERROR 4

/*
 What a call left behind for the host.

 Fixed layout, little-endian, [`TOCAT_OUTBOX_LEN`] bytes. `repr(C)` rather
 than `packed`: wasm32 puts the `u64` on an eight-byte boundary, which is
 where offset 32 already is, so there is no padding to remove and no
 unaligned field to read. The assertions below are what keep that true on
 every target this crate is built for, including the 64-bit host that reads
 the struct back out of guest memory.
 */
typedef struct {
    uint32_t emit;
    uint32_t bytes_ptr;
    uint32_t bytes_len;
    uint32_t bounds_ptr;
    uint32_t bounds_len;
    uint32_t flags;
    uint32_t message_ptr;
    uint32_t message_len;
    uint64_t pace_ns;
    uint32_t logs_ptr;
    uint32_t logs_len;
} tocat_outbox_t;

/*
 One queued log record: a level, and a string in the guest's memory.
 */
typedef struct {
    uint32_t level;
    uint32_t ptr;
    uint32_t len;
} tocat_log_record_t;

#endif  /* TOCAT_ABI_H */
