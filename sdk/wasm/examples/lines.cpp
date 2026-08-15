/*
 * lines - cut the path into one unit per line.
 *
 * The C++ example, written against <tocat/tocat.hpp>: a guest is a type with
 * methods, and TOCAT_GUEST generates the exports from it. Compare toupper.c,
 * which does a simpler job against the C header, writing the outbox by hand.
 *
 *     [[plugin]]
 *     name = "wasm"
 *     module = "lines.wasm"
 *     config = { max-line = 8192, flush-ms = 250 }
 *
 * A unit is one write at a byte sink and one datagram at a datagram sink, so
 * this turns a stream into one message per line. That is also why it does not
 * declare itself boundary preserving: the boundaries it emits are its own.
 */

#include <tocat/tocat.hpp>

using namespace tocat::literals;

/* Everything held between calls lives here, so this is the longest line. */
static constexpr uint32_t CAPACITY = 64 * 1024;

/*
 * How many units one call may emit. A chunk with more lines than this keeps
 * the rest held for the next call, which costs a little latency and keeps the
 * bound on memory honest.
 */
static constexpr uint32_t MAX_UNITS = 1024;

struct Lines {
    /*
     * Trivially constructible on purpose: nothing calls __wasm_call_ctors in a
     * module built with --no-entry, so a constructor would never run. The
     * static_assert in TOCAT_GUEST enforces it rather than leaving it to be
     * discovered as a stage that reads zeroes.
     */
    uint8_t held[CAPACITY];
    uint32_t held_len;

    /*
     * Bytes at the front of `held` that the last call emitted. The host reads
     * guest memory *after* the call returns, so nothing may move until the
     * next call begins: compacting eagerly would hand the sink bytes that had
     * already been overwritten.
     */
    uint32_t consumed;

    uint32_t bounds[MAX_UNITS];

    uint32_t max_line;
    uint64_t flush_ns;

    /*
     * Read once, after init, so it can depend on the options. Zero means no
     * ticks and no timer, which is what an unset `flush-ms` should cost.
     */
    tocat::nanos tick_interval() const { return tocat::nanos{flush_ns}; }

    /*
     * Options arrive once, before any bytes. Rejecting one here is a startup
     * error carrying this message, which is where a bad option should be
     * caught.
     */
    void init(tocat::ctx &c, tocat::bytes config) {
        max_line = 8192;

        uint32_t value = 0;

        if (find_uint(config, "max-line", 8, &value)) {
            max_line = value;
        }

        if (max_line == 0 || max_line > CAPACITY) {
            c.fail("max-line does not fit in this guest's buffer");
            return;
        }

        if (find_uint(config, "flush-ms", 8, &value)) {
            flush_ns = (uint64_t)value * (1_ms).count;
        }

        c.log(tocat::level::info, "splitting on newlines");
    }

    void on_bytes(tocat::ctx &c, tocat::bytes input) {
        compact();

        if (held_len + input.size() > CAPACITY) {
            c.fail("a line exceeded max-line");
            return;
        }

        const bool was_idle = held_len == 0;
        const uint32_t scan_from = held_len;

        memcpy(held + held_len, input.data(), input.size());
        held_len += input.size();

        /*
         * Whatever was held is a partial line, so it contains no newline and
         * only the bytes that just arrived are worth scanning.
         */
        uint32_t units = 0;
        for (uint32_t i = scan_from; i < held_len && units < MAX_UNITS; i++) {
            if (held[i] == '\n') {
                bounds[units++] = i + 1;
            }
        }

        if (units == 0) {
            if (held_len > max_line) {
                c.fail("a line exceeded max-line");
                return;
            }

            /*
             * Nothing complete yet. Restart the flush window from the byte
             * that started this line, so `flush-ms` bounds how long that byte
             * waits rather than being a position in a cadence it never sees.
             */
            c.drop();

            if (was_idle && flush_ns != 0) {
                c.rearm();
            }

            return;
        }

        /*
         * Every complete line becomes its own unit. The last boundary is where
         * the emission ends rather than a split inside it, so it is left off:
         * the trailing unit closes itself.
         */
        const uint32_t end = bounds[units - 1];
        c.emit(held, end);

        if (units > 1) {
            c.units(bounds, units - 1);
        }

        consumed = end;

        if (held_len > end && flush_ns != 0) {
            c.rearm();
        }
    }

    /*
     * The last chance to emit. A tail without a newline is still a line, and
     * dropping it because the stream ended untidily would lose data.
     */
    void on_eof(tocat::ctx &c) {
        compact();

        if (held_len == 0) {
            return;
        }

        c.emit(held, held_len);
        consumed = held_len;
    }

    /*
     * The flush window expired. Emitting a partial line splits it, which is
     * the trade `flush-ms` asks for: latency bounded, at the cost of a line
     * arriving as two units.
     */
    void on_tick(tocat::ctx &c) {
        compact();

        if (held_len == 0) {
            return;
        }

        c.emit(held, held_len);
        consumed = held_len;
        c.rearm();
    }

  private:
    void compact() {
        if (consumed == 0) {
            return;
        }

        memmove(held, held + consumed, held_len - consumed);
        held_len -= consumed;
        consumed = 0;
    }

    /*
     * The guest owns its options, so it owns parsing them. This reads one
     * unsigned value out of flat JSON and is deliberately about as much as
     * that: a guest with real options should bring a parser, which is why the
     * host hands the config over as bytes rather than pretending to know the
     * schema.
     */
    static bool find_uint(tocat::bytes config, const char *key, uint32_t key_len,
                          uint32_t *out) {
        const char *json = config.chars();
        const uint32_t len = config.size();

        for (uint32_t i = 0; i + key_len <= len; i++) {
            bool match = true;

            for (uint32_t k = 0; k < key_len; k++) {
                if (json[i + k] != key[k]) {
                    match = false;
                    break;
                }
            }

            if (!match) {
                continue;
            }

            uint32_t at = i + key_len;
            while (at < len && json[at] != ':' && json[at] != ',') {
                at++;
            }

            if (at >= len || json[at] != ':') {
                continue;
            }

            at++;
            while (at < len && (json[at] == ' ' || json[at] == '"')) {
                at++;
            }

            if (at >= len || json[at] < '0' || json[at] > '9') {
                continue;
            }

            uint32_t value = 0;
            while (at < len && json[at] >= '0' && json[at] <= '9') {
                value = value * 10 + (uint32_t)(json[at] - '0');
                at++;
            }

            *out = value;
            return true;
        }

        return false;
    }
};

/*
 * `boundaries` is deliberately not declared. A guest that says nothing is
 * assumed unsafe, and this one is: it holds bytes across calls and the
 * boundaries it emits are its own rather than the ones a peer sent. tocat
 * warns and relays anyway, since one datagram per line is a reasonable thing
 * to ask for.
 */
TOCAT_GUEST(Lines, 256 * 1024)
