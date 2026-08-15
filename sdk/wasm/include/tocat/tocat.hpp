/*
 * tocat.hpp - the guest ABI for C++.
 *
 * A wrapper over <tocat/tocat.h> rather than a second implementation of it.
 * The layout, the constants and the exports that never vary stay in the C
 * header, which is the one place they are declared; everything here is types
 * and compile-time checks over the top. Two headers describing one ABI would
 * drift, and the one that drifted would be the one nobody tested.
 *
 * What it buys over the C header:
 *
 *   - a guest is a type with methods, not a set of free functions writing
 *     into a global, and TOCAT_GUEST generates every export from it
 *   - the entrypoints cannot be misspelled, and the outbox cannot be left
 *     unreset, because neither is written by hand
 *   - `emit`, `halt`, `pace` and the rest take types rather than pairs of
 *     integers, so a length and a pointer cannot be swapped
 *   - optional hooks are detected rather than declared, so a guest writes the
 *     three lines it needs and nothing else
 *
 * What it deliberately does not do is reach for the standard library. A guest
 * has no allocator, no exceptions, no RTTI and no libc++ built for wasm32, so
 * <string>, <vector> and <expected> are all unavailable and <span> and
 * <type_traits> are a gamble on how the cross toolchain resolves headers. This
 * file includes nothing but the C header, and uses compiler builtins where a
 * trait is needed. Idiomatic here means C++20 concepts, scoped enums and
 * `constexpr`, not the parts of the language that assume a runtime.
 *
 * Requires C++20. See docs/src/api/wasm-abi.md for the ABI itself.
 */

#ifndef TOCAT_HPP
#define TOCAT_HPP

#include <tocat/tocat.h>

namespace tocat {

/* A chunk. Non-owning, and only valid for the call that was handed it: the
 * host may write the next chunk over this one. */
class bytes {
  public:
    constexpr bytes() = default;

    constexpr bytes(const uint8_t *data, uint32_t size) : data_(data), size_(size) {}

    /* From the host's pointer, which is an address in linear memory. */
    bytes(int32_t ptr, int32_t len)
        : data_((const uint8_t *)(uintptr_t)ptr), size_((uint32_t)len) {}

    constexpr const uint8_t *data() const { return data_; }
    constexpr uint32_t size() const { return size_; }
    constexpr bool empty() const { return size_ == 0; }

    constexpr const uint8_t *begin() const { return data_; }
    constexpr const uint8_t *end() const { return data_ + size_; }

    constexpr uint8_t operator[](uint32_t at) const { return data_[at]; }

    /* The same bytes as characters, for a guest whose input is text. Not
     * `constexpr`: the cast is not one. */
    const char *chars() const { return (const char *)data_; }

  private:
    const uint8_t *data_ = nullptr;
    uint32_t size_ = 0;
};

/* A duration, so that a number cannot arrive in the wrong unit. */
struct nanos {
    uint64_t count = 0;
};

inline namespace literals {

consteval nanos operator""_ns(unsigned long long n) { return nanos{(uint64_t)n}; }
consteval nanos operator""_us(unsigned long long n) { return nanos{(uint64_t)n * 1000ull}; }
consteval nanos operator""_ms(unsigned long long n) { return nanos{(uint64_t)n * 1000000ull}; }
consteval nanos operator""_s(unsigned long long n) { return nanos{(uint64_t)n * 1000000000ull}; }

} // namespace literals

enum class level : uint32_t {
    trace = TOCAT_TRACE,
    debug = TOCAT_DEBUG,
    info = TOCAT_INFO,
    warn = TOCAT_WARN,
    error = TOCAT_ERROR,
};

/*
 * Text the host will read after the call returns, so it has to outlive the
 * call. Constructing from a literal is `consteval`, which is what makes that
 * true by construction; anything else goes through `borrowed` and is the
 * guest's promise to keep.
 */
class message {
  public:
    template <uint32_t N>
    consteval message(const char (&literal)[N]) : text_(literal), size_(N - 1) {}

    static constexpr message borrowed(const char *text, uint32_t size) {
        return message(text, size);
    }

    constexpr const char *text() const { return text_; }
    constexpr uint32_t size() const { return size_; }

  private:
    constexpr message(const char *text, uint32_t size) : text_(text), size_(size) {}

    const char *text_;
    uint32_t size_;
};

/*
 * The effect queue for one call, and the only way to say anything to the host.
 * Constructing it resets the outbox, which is why the generated exports make
 * one first: a flag or a message left over from an earlier chunk would
 * otherwise be applied again.
 *
 * Stateless and non-copyable: there is one outbox, so there is one of these
 * per call, and it exists to give the operations a home rather than to hold
 * anything.
 */
class ctx {
  public:
    ctx() { tocat_reset(); }

    ctx(const ctx &) = delete;
    ctx &operator=(const ctx &) = delete;

    /* Forward the input unchanged. Nothing is copied in either direction. */
    void pass_through() { tocat_pass_through(); }

    /* Swallow the chunk. The same as emitting nothing, said on purpose. */
    void drop() { tocat_drop(); }

    /*
     * Forward these bytes, which must stay put until the next call: the host
     * reads them after this one returns. Compacting a buffer at the end of the
     * call that emitted from it hands the sink bytes already overwritten.
     */
    void emit(const void *data, uint32_t size) { tocat_emit(data, size); }
    void emit(bytes out) { tocat_emit(out.data(), out.size()); }

    /*
     * Frame what was emitted into units at these offsets into it. One unit is
     * one write at a byte sink, one datagram at a datagram sink, and one call
     * to every stage below, so ask only when the splits are the point. The
     * trailing unit closes itself.
     */
    void units(const uint32_t *bounds, uint32_t count) { tocat_units(bounds, count); }

    template <uint32_t N> void units(const uint32_t (&bounds)[N]) { tocat_units(bounds, N); }

    void log(level at, message text) {
        tocat_log((uint32_t)at, text.text(), text.size());
    }

    /*
     * End the path: upstream end of stream arriving early rather than a
     * failure. What is already emitted is written, the stages below are
     * drained, and tocat exits successfully.
     */
    void halt(message reason) { tocat_halt(reason.text(), reason.size()); }

    /*
     * Fail the path. From `init` this is how an option is rejected, and
     * becomes a startup error carrying this message.
     */
    void fail(message reason) { tocat_fail(reason.text(), reason.size()); }

    /* Wait this long before reading upstream again. Nothing is buffered. */
    void pace(nanos wait) { tocat_pace(wait.count); }

    /*
     * Restart this stage's tick schedule from now, which is how a deadline is
     * measured from the last byte rather than from wherever the host's cadence
     * had reached.
     */
    void rearm() { tocat_rearm(); }
};

/* A guest is anything that can be handed a chunk. */
template <typename G>
concept guest = requires(G &g, ctx &c, bytes input) { g.on_bytes(c, input); };

namespace detail {

template <typename G>
concept has_init = requires(G &g, ctx &c, bytes config) { g.init(c, config); };

template <typename G>
concept has_eof = requires(G &g, ctx &c) { g.on_eof(c); };

template <typename G>
concept has_tick = requires(G &g, ctx &c) { g.on_tick(c); };

/*
 * An interval decided at run time, from options. Constraining the return type
 * would want <concepts>, which a guest cannot rely on having, so this matches
 * on the call and the assignment to `nanos` below is what rejects a guest that
 * answers with something else.
 */
template <typename G>
concept has_tick_interval_member = requires(const G &g) { g.tick_interval(); };

/* An interval fixed at compile time. */
template <typename G>
concept has_tick_constant = requires { G::tick_interval; };

template <typename G>
concept declares_boundaries = requires { G::boundaries; };

template <typename G>
concept declares_needs = requires { G::needs; };

template <guest G> void init(G &g, bytes config) {
    ctx c;

    if constexpr (has_init<G>) {
        g.init(c, config);
    }
}

template <guest G> void on_bytes(G &g, bytes input) {
    ctx c;
    g.on_bytes(c, input);
}

template <guest G> void on_eof(G &g) {
    ctx c;

    if constexpr (has_eof<G>) {
        g.on_eof(c);
    }
}

template <guest G> void on_tick(G &g) {
    ctx c;

    if constexpr (has_tick<G>) {
        g.on_tick(c);
    }
}

/*
 * Read once, after init, so a guest whose interval comes from its options
 * answers with a member function and one with a fixed cadence with a constant.
 * A guest that says nothing gets no timer at all, which is what costs nothing.
 */
template <guest G> int64_t tick_interval(const G &g) {
    if constexpr (!has_tick<G>) {
        return 0;
    } else if constexpr (has_tick_interval_member<G>) {
        const nanos interval = g.tick_interval();
        return (int64_t)interval.count;
    } else if constexpr (has_tick_constant<G>) {
        const nanos interval = G::tick_interval;
        return (int64_t)interval.count;
    } else {
        return 0;
    }
}

/*
 * The two boundary answers packed into the one word the host reads: what the
 * stage does to message boundaries in bits 0 and 1, what it needs of the path
 * in bits 2 and 3.
 *
 * A guest that declares neither answers zero, which claims nothing and asks
 * for nothing. That is the safe reading, and it is also what the host assumes
 * of a guest that does not export the function at all.
 */
template <guest G> constexpr int32_t boundaries() {
    uint32_t packed = TOCAT_BOUNDARIES_FUSE | TOCAT_NEEDS_NOTHING;

    if constexpr (declares_boundaries<G>) {
        packed = (packed & ~(uint32_t)TOCAT_BOUNDARIES_MASK) |
                 ((uint32_t)G::boundaries & (uint32_t)TOCAT_BOUNDARIES_MASK);
    }

    if constexpr (declares_needs<G>) {
        packed = (packed & ~(uint32_t)TOCAT_NEEDS_MASK) |
                 ((uint32_t)G::needs & (uint32_t)TOCAT_NEEDS_MASK);
    }

    return (int32_t)packed;
}

} // namespace detail
} // namespace tocat

/*
 * TOCAT_GUEST(<type>, <arena bytes>)
 *
 * Defines the arena, the instance, and every export, at namespace scope:
 *
 *     struct Upper {
 *         static constexpr uint32_t boundaries = TOCAT_BOUNDARIES_PRESERVE;
 *
 *         void on_bytes(tocat::ctx &c, tocat::bytes input) { ... }
 *     };
 *
 *     TOCAT_GUEST(Upper, 256 * 1024);
 *
 * `boundaries` and `needs` are optional; a guest declaring neither claims
 * nothing and asks for nothing, which is the safe reading.
 *
 * `init`, `on_eof`, `on_tick` and `tick_interval` are optional and detected.
 * The exports for them are generated either way, which changes nothing the
 * host does: it asks for the tick period, and a guest without one answers
 * zero, so no timer is built.
 */
#define TOCAT_GUEST(GuestType, ArenaBytes)                                                  \
    TOCAT_ARENA(ArenaBytes)                                                                 \
                                                                                            \
    static_assert(::tocat::guest<GuestType>,                                                \
                  "a tocat guest needs on_bytes(tocat::ctx&, tocat::bytes)");               \
                                                                                            \
    /* Nothing calls __wasm_call_ctors under --no-entry, so a guest needing a               \
     * constructor would be silently uninitialised. Catching that here beats                \
     * debugging a stage that reads zeroes. */                                              \
    static_assert(__is_trivially_constructible(GuestType),                                  \
                  "a tocat guest must be trivially constructible: global constructors do "  \
                  "not run in a module built with --no-entry");                             \
                                                                                            \
    static GuestType tocat__guest;                                                          \
                                                                                            \
    TOCAT_EXPORT(tocat_init) void tocat_init(int32_t ptr, int32_t len) {                    \
        ::tocat::detail::init(tocat__guest, ::tocat::bytes(ptr, len));                      \
    }                                                                                       \
                                                                                            \
    TOCAT_EXPORT(tocat_on_bytes) void tocat_on_bytes(int32_t ptr, int32_t len) {            \
        ::tocat::detail::on_bytes(tocat__guest, ::tocat::bytes(ptr, len));                  \
    }                                                                                       \
                                                                                            \
    TOCAT_EXPORT(tocat_on_eof) void tocat_on_eof(void) {                                    \
        ::tocat::detail::on_eof(tocat__guest);                                              \
    }                                                                                       \
                                                                                            \
    TOCAT_EXPORT(tocat_on_tick) void tocat_on_tick(void) {                                  \
        ::tocat::detail::on_tick(tocat__guest);                                             \
    }                                                                                       \
                                                                                            \
    TOCAT_EXPORT(tocat_tick_interval_ns) int64_t tocat_tick_interval_ns(void) {             \
        return ::tocat::detail::tick_interval(tocat__guest);                                \
    }                                                                                       \
                                                                                            \
    TOCAT_EXPORT(tocat_boundaries) int32_t tocat_boundaries(void) {                         \
        return ::tocat::detail::boundaries<GuestType>();                                    \
    }

#endif /* TOCAT_HPP */
