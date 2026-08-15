/*
 * toupper - upper-case the stream.
 *
 * The smallest useful guest: one transform per chunk, no state, no options.
 * Build it, then drop it into a relay:
 *
 *     tocat - 'wasm,module=toupper.wasm' tcp:localhost:9000
 */

#include <tocat/tocat.h>

/*
 * The most a chunk may be. tocat's copy buffer is 256 KiB by default, and a
 * chunk is never larger than that, so a guest sized below it has to be run
 * with a matching `-b`. Refusing is better than truncating: the host turns a
 * refused chunk into a failed direction saying so.
 */
#define CAPACITY (256 * 1024)

TOCAT_ARENA(CAPACITY)

static uint8_t output[CAPACITY];

TOCAT_EXPORT(tocat_on_bytes) void tocat_on_bytes(int32_t ptr, int32_t len) {
    tocat_reset();

    /* The host's pointer is an address in our memory, so it is just a pointer. */
    const uint8_t *input = (const uint8_t *)(uintptr_t)ptr;
    uint32_t n = (uint32_t)len;

    for (uint32_t i = 0; i < n; i++) {
        uint8_t byte = input[i];
        output[i] = (byte >= 'a' && byte <= 'z') ? (uint8_t)(byte - 32) : byte;
    }

    /*
     * One emission, so one unit: one write at a byte sink, one datagram at a
     * datagram sink. No boundaries are needed to say that.
     */
    tocat_emit(output, n);
}

/*
 * One message in, one message out, and nothing is held across calls, so this
 * preserves boundaries and says so. A guest that says nothing claims nothing,
 * which is the right default and the wrong answer here. Bits 2 and 3 are left
 * clear: this stage works on any path.
 */
TOCAT_EXPORT(tocat_boundaries) int32_t tocat_boundaries(void) {
    return TOCAT_BOUNDARIES_PRESERVE | TOCAT_NEEDS_NOTHING;
}
