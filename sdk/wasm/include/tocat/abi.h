/*
 * The tocat WebAssembly guest ABI, version 1.
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
 Bumped for any change to the layout below. A guest reporting a different
 version is refused when it loads rather than being read as garbage.
 */
#define TOCAT_ABI_VERSION 1

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
