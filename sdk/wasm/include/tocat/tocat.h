/*
 * tocat.h - the guest side of the tocat WebAssembly ABI, version 1.
 *
 * A guest imports nothing. It exports a handful of functions and a linear
 * memory, and everything it wants the host to do is written into a fixed
 * struct that the host reads after each call. <tocat/abi.h> is that struct and
 * its constants, generated from the Rust definition; this header is the
 * exports that never vary and the small amount of glue that makes writing the
 * rest readable.
 *
 * It is written for a single translation unit, which is what a guest usually
 * is: the outbox and the log buffer are file-scope statics, and including it
 * twice gives you two of each. See docs/src/api/wasm-abi.md for the contract
 * itself.
 *
 * Both C and C++ compile this, and <tocat/tocat.hpp> wraps it for C++ guests.
 */

#ifndef TOCAT_H
#define TOCAT_H

#include <stddef.h>
#include <stdint.h>

/*
 * The layout and the wire constants are generated from crates/tocat-abi, so
 * that the host that reads an outbox, a Rust guest that writes one, and this
 * header cannot disagree about what one is. Everything below is the part a
 * generator has no opinion about: the exports, the arena, and the helpers.
 */
#include <tocat/abi.h>

#ifndef TOCAT_MAX_LOGS
#define TOCAT_MAX_LOGS 8
#endif

/*
 * An exported function. lld exports nothing by default under `--no-entry`, so
 * the attribute is what puts a name in the export section, and `used` is what
 * stops the optimiser removing a function nobody calls. Every one of these is
 * called only by the host.
 */
#ifdef __cplusplus
#define TOCAT_EXPORT(name) extern "C" __attribute__((export_name(#name), used))
#else
#define TOCAT_EXPORT(name) __attribute__((export_name(#name), used))
#endif

/*
 * Every pointer crossing the ABI is an address in linear memory. In C that is
 * what a pointer already is, so this is a cast rather than a calculation, and
 * the base-offset mistake that catches guests written around a static array in
 * other languages does not arise here.
 */
#define TOCAT_ADDR(p) ((uint32_t)(uintptr_t)(p))

#ifdef __cplusplus
#define TOCAT_ASSERT(cond, message) static_assert(cond, message)
#else
#define TOCAT_ASSERT(cond, message) _Static_assert(cond, message)
#endif

/*
 * The generated struct is what the host reads, so check it against the length
 * the generator also emitted rather than trusting the two to agree. A
 * mismatch here is a build error; without it, it is a stage that quietly drops
 * everything.
 */
TOCAT_ASSERT(sizeof(tocat_outbox_t) == TOCAT_OUTBOX_LEN,
             "outbox must be TOCAT_OUTBOX_LEN bytes");
TOCAT_ASSERT(sizeof(tocat_log_record_t) == TOCAT_LOG_RECORD_LEN,
             "log record must be TOCAT_LOG_RECORD_LEN bytes");
TOCAT_ASSERT(offsetof(tocat_outbox_t, pace_ns) == 32, "pace_ns must be at 32");
TOCAT_ASSERT(offsetof(tocat_outbox_t, logs_ptr) == 40, "logs_ptr must be at 40");

static tocat_outbox_t tocat__ob;
static tocat_log_record_t tocat__logs[TOCAT_MAX_LOGS] __attribute__((unused));

TOCAT_EXPORT(tocat_abi_version) int32_t tocat_abi_version(void) {
    return TOCAT_ABI_VERSION;
}

TOCAT_EXPORT(tocat_outbox) int32_t tocat_outbox(void) {
    return (int32_t)TOCAT_ADDR(&tocat__ob);
}

/*
 * Clear the outbox. Call this first in every entrypoint: the struct persists
 * between calls, so a halt flag or a message pointer left over from an earlier
 * chunk is applied again.
 */
static inline void tocat_reset(void) {
    tocat__ob.emit = TOCAT_EMIT_PENDING;
    tocat__ob.bytes_ptr = 0;
    tocat__ob.bytes_len = 0;
    tocat__ob.bounds_ptr = 0;
    tocat__ob.bounds_len = 0;
    tocat__ob.flags = 0;
    tocat__ob.message_ptr = 0;
    tocat__ob.message_len = 0;
    tocat__ob.pace_ns = 0;
    tocat__ob.logs_ptr = 0;
    tocat__ob.logs_len = 0;
}

/* Forward the input unchanged. The host never reads guest memory for this. */
static inline void tocat_pass_through(void) {
    tocat__ob.emit = TOCAT_EMIT_PASSTHROUGH;
}

/* Swallow the chunk. The same as emitting nothing, said on purpose. */
static inline void tocat_drop(void) {
    tocat__ob.emit = TOCAT_EMIT_PENDING;
}

/*
 * Forward `len` bytes from `bytes`, which must stay put until the host has
 * read them, meaning until the next call begins. Nothing may be compacted or
 * reused inside the call that emitted it.
 */
static inline void tocat_emit(const void *bytes, uint32_t len) {
    tocat__ob.emit = TOCAT_EMIT_BUFFERED;
    tocat__ob.bytes_ptr = TOCAT_ADDR(bytes);
    tocat__ob.bytes_len = len;
}

/*
 * Frame what was emitted into units, at these offsets into it. One unit
 * becomes one write at a byte sink, one datagram at a datagram sink, and one
 * call to every stage below, so ask only when the splits are the point. The
 * trailing unit needs no boundary of its own.
 */
static inline void tocat_units(const uint32_t *bounds, uint32_t count) {
    tocat__ob.bounds_ptr = TOCAT_ADDR(bounds);
    tocat__ob.bounds_len = count;
}

/*
 * Queue a log record. The message has to outlive the call, so a string literal
 * or a static buffer, never a local.
 */
static inline void tocat_log(uint32_t level, const char *message, uint32_t len) {
    if (tocat__ob.logs_len >= (uint32_t)TOCAT_MAX_LOGS) {
        return;
    }

    tocat_log_record_t *record = &tocat__logs[tocat__ob.logs_len];
    record->level = level;
    record->ptr = TOCAT_ADDR(message);
    record->len = len;

    tocat__ob.logs_ptr = TOCAT_ADDR(tocat__logs);
    tocat__ob.logs_len += 1;
}

/*
 * End the path. This is upstream end of stream arriving early rather than a
 * failure: what is already emitted is written, the stages below are drained,
 * and tocat exits successfully.
 */
static inline void tocat_halt(const char *reason, uint32_t len) {
    tocat__ob.flags |= TOCAT_FLAG_HALT;
    tocat__ob.message_ptr = TOCAT_ADDR(reason);
    tocat__ob.message_len = len;
}

/*
 * Fail the path. Use this when the bytes cannot be processed, and from
 * `tocat_init` to reject an option, where it becomes a startup error carrying
 * this message.
 */
static inline void tocat_fail(const char *message, uint32_t len) {
    tocat__ob.flags |= TOCAT_FLAG_ERROR;
    tocat__ob.message_ptr = TOCAT_ADDR(message);
    tocat__ob.message_len = len;
}

/* Ask the host to wait this long before reading upstream again. */
static inline void tocat_pace(uint64_t nanos) {
    tocat__ob.flags |= TOCAT_FLAG_PACE;
    tocat__ob.pace_ns = nanos;
}

/*
 * Restart this stage's tick schedule from now. A tick is a cadence rather than
 * a delay, so this is how a deadline gets measured from the last byte instead
 * of from wherever the cadence had reached.
 */
static inline void tocat_rearm(void) {
    tocat__ob.flags |= TOCAT_FLAG_REARM;
}

/*
 * The arena the host writes chunks into, and the export that hands out its
 * address. It is an arena rather than a heap: the host never frees, writes
 * exactly the length it asked for, and asks again on the next chunk. Returning
 * 0 refuses the chunk and fails the direction.
 */
#define TOCAT_ARENA(size)                                                     \
    static uint8_t tocat__arena[size];                                        \
                                                                              \
    TOCAT_EXPORT(tocat_alloc) int32_t tocat_alloc(int32_t len) {              \
        if (len < 0 || (uint32_t)len > (uint32_t)sizeof tocat__arena) {       \
            return 0;                                                         \
        }                                                                     \
                                                                              \
        return (int32_t)TOCAT_ADDR(tocat__arena);                             \
    }

/* Length of a string literal, without pulling in a libc for strlen. */
#define TOCAT_STR(literal) (literal), (uint32_t)(sizeof(literal) - 1)

/*
 * A guest has no libc, but the compiler still lowers loops and struct copies
 * into calls to these, so they have to exist somewhere. Define
 * TOCAT_NO_MEM_BUILTINS if you are linking something that already provides
 * them.
 *
 * They are the naive versions on purpose: this is a fallback for what the
 * compiler generates, not something a guest should be calling in a hot loop.
 */
#ifndef TOCAT_NO_MEM_BUILTINS
#ifdef __cplusplus
extern "C" {
#endif

void *memcpy(void *to, const void *from, size_t n) {
    uint8_t *d = (uint8_t *)to;
    const uint8_t *s = (const uint8_t *)from;

    for (size_t i = 0; i < n; i++) {
        d[i] = s[i];
    }

    return to;
}

void *memmove(void *to, const void *from, size_t n) {
    uint8_t *d = (uint8_t *)to;
    const uint8_t *s = (const uint8_t *)from;

    if (d < s) {
        for (size_t i = 0; i < n; i++) {
            d[i] = s[i];
        }
    } else {
        for (size_t i = n; i > 0; i--) {
            d[i - 1] = s[i - 1];
        }
    }

    return to;
}

void *memset(void *to, int byte, size_t n) {
    uint8_t *d = (uint8_t *)to;

    for (size_t i = 0; i < n; i++) {
        d[i] = (uint8_t)byte;
    }

    return to;
}

#ifdef __cplusplus
}
#endif
#endif /* TOCAT_NO_MEM_BUILTINS */

#endif /* TOCAT_H */
