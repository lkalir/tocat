//! Write a tocat WebAssembly plugin in Rust.
//!
//! A guest is a type that implements [`Guest`], and [`export_guest!`] turns it
//! into a module: the arena, the outbox, the panic handler and every export
//! the host looks for. Nothing is written by hand, so nothing can be
//! misspelled, and the compiler checks the shape of the guest rather than the
//! host discovering it at load time.
//!
//! ```ignore
//! #![no_std]
//!
//! use tocat_wasm_sdk::{Boundaries, Context, Guest, export_guest};
//!
//! pub struct Upper {
//!     out: [u8; 256 * 1024],
//! }
//!
//! impl Guest for Upper {
//!     const INIT: Self = Self { out: [0; 256 * 1024] };
//!     const BOUNDARIES: Boundaries = Boundaries::Preserve;
//!
//!     fn on_bytes(&mut self, ctx: &mut Context, input: &[u8]) {
//!         for (out, byte) in self.out.iter_mut().zip(input) {
//!             *out = byte.to_ascii_uppercase();
//!         }
//!
//!         ctx.emit(&self.out[..input.len()]);
//!     }
//! }
//!
//! export_guest!(Upper, arena_bytes = 256 * 1024);
//! ```
//!
//! # What the compiler checks
//!
//! - `on_bytes` is required, and its signature is the trait's. A guest that
//!   does not have one is a trait error rather than a module the host loads and
//!   finds nothing in.
//! - [`Guest::INIT`] is a `const`, so the guest is built at compile time.
//!   Nothing calls `__wasm_call_ctors` in a module built with `--no-entry`, so
//!   a guest needing run-time construction would be silently uninitialised;
//!   requiring a const initialiser is how that is ruled out rather than
//!   documented.
//! - Every string handed to [`Context::halt`], [`Context::fail`] and
//!   [`Context::log`] is `&'static str`. The host reads guest memory after the
//!   call returns, so a message has to outlive the call, and in Rust that is a
//!   lifetime rather than a comment.
//! - [`export_guest!`] rejects a zero-sized arena, and the ABI crate asserts
//!   its own layout, so a mismatch is a build failure rather than a stage that
//!   silently drops the stream.
//!
//! # What it cannot check
//!
//! Bytes handed to [`Context::emit`] and offsets handed to [`Context::units`]
//! are read by the host *after* the call returns, so they must not move until
//! the next one begins. A guest emitting from its own fields, as above, is
//! fine; a guest that compacts a buffer at the end of the call that emitted
//! from it hands the sink bytes it has already overwritten. That one is on
//! you.

#![no_std]

use core::time::Duration;

pub use tocat_wasm_abi::{Boundaries, Emit, Level, LogRecord, Needs, Outbox, TOCAT_ABI_VERSION};
use tocat_wasm_abi::{TOCAT_FLAG_ERROR, TOCAT_FLAG_HALT, TOCAT_FLAG_PACE, TOCAT_FLAG_REARM};

/// How many log records one call may queue. Beyond this they are dropped:
/// the array lives in the guest, and a stage that wants to say twenty things
/// about one chunk has a different problem.
pub const MAX_LOGS: usize = 8;

/// A tocat stage.
///
/// Instances are per direction and per connection, so under `fork` every
/// accepted client gets its own module instance and its own state, and a
/// `direction = "both"` entry gets two. Nothing here is shared, which is why
/// none of it needs a lock.
pub trait Guest: Sized {
    /// The guest, built at compile time. See the note about
    /// `__wasm_call_ctors` above.
    const INIT: Self;

    /// What this stage does to the message boundaries passing through it.
    ///
    /// [`Boundaries::Fuse`], the default, claims nothing, which is the safe
    /// answer: the host warns about a stage that may not sit on a path whose
    /// destination is a datagram endpoint. A stage that emits one unit for
    /// every call it was given one may say [`Boundaries::Preserve`]; one that
    /// holds bytes across calls or reframes what it was handed may not, even
    /// when doing that is the point.
    const BOUNDARIES: Boundaries = Boundaries::Fuse;

    /// What this stage needs of the path it was placed on.
    ///
    /// Unlike [`BOUNDARIES`](Guest::BOUNDARIES), which the host only warns
    /// about, an unmet requirement is a configuration error. Say
    /// [`Needs::Upstream`] when every call has to carry one whole message, and
    /// [`Needs::Downstream`] when the units emitted have to reach the far end
    /// intact. Both are met by a datagram endpoint, or by an `unframe` above
    /// and a `frame` below.
    const NEEDS: Needs = Needs::Nothing;

    /// A tick period fixed at compile time. See [`Guest::tick_interval`] for
    /// one that comes from options.
    const TICK_INTERVAL: Option<Duration> = None;

    /// The entry's `config`, as JSON, once, before any bytes.
    ///
    /// Rejecting an option here with [`Context::fail`] is a startup error
    /// carrying that message, which is where a bad option should be caught.
    fn init(&mut self, ctx: &mut Context, config: &[u8]) {
        let _ = (ctx, config);
    }

    /// A chunk from upstream. The only required method.
    fn on_bytes(&mut self, ctx: &mut Context, input: &[u8]);

    /// Upstream has finished: the last chance to emit. Anything emitted
    /// continues down through the stages below, which then see their own end
    /// of stream.
    ///
    /// Some paths never reach it. A datagram source has no end of stream, and
    /// neither does a held FIFO, so a stage whose only output happens here
    /// produces nothing at all on one.
    fn on_eof(&mut self, ctx: &mut Context) {
        let _ = ctx;
    }

    /// The tick schedule came due. Only ever called when an interval was
    /// asked for.
    fn on_tick(&mut self, ctx: &mut Context) {
        let _ = ctx;
    }

    /// The tick period, read once, after `init`, so it can depend on options.
    ///
    /// The host owns the timer, so what a stage gets is a cadence rather than
    /// a delay: a tick that came due while bytes were flowing fires at the
    /// next opportunity. A stage that means "an interval after I started
    /// waiting" says so with [`Context::rearm`].
    fn tick_interval(&self) -> Option<Duration> {
        Self::TICK_INTERVAL
    }
}

/// The effect queue for one call, and the only way to say anything to the
/// host.
///
/// A stage decides what to forward and asks for everything else; the host
/// applies it after the call returns. That is what lets a guest exist at all:
/// there is nothing here that needs a host function, so a guest imports
/// nothing and cannot reach a clock, a file or a socket.
pub struct Context {
    outbox: Outbox,
    logs: [LogRecord; MAX_LOGS],
}

impl Context {
    pub const fn new() -> Self {
        Self {
            outbox: Outbox::new(),
            logs: [LogRecord {
                level: 0,
                ptr: 0,
                len: 0,
            }; MAX_LOGS],
        }
    }

    /// The address the host reads. An address in linear memory, which in Rust
    /// is what a pointer already is.
    pub fn outbox_addr(&self) -> i32 {
        (&raw const self.outbox) as usize as i32
    }

    /// Clear the queue. The generated entrypoints call this before every hook,
    /// since the outbox persists between calls and a stale flag would be
    /// applied twice.
    pub fn reset(&mut self) {
        self.outbox.reset();
    }

    /// Forward the input unchanged. The host does not read the guest's bytes
    /// at all, so this costs nothing in either direction.
    pub fn pass_through(&mut self) {
        self.outbox.set_emit(Emit::Passthrough);
    }

    /// Swallow the chunk. Emitting nothing does the same thing; this says it
    /// on purpose.
    pub fn drop_chunk(&mut self) {
        self.outbox.set_emit(Emit::Pending);
    }

    /// Forward these bytes.
    ///
    /// They must not move until the next call: the host reads them after this
    /// one returns.
    pub fn emit(&mut self, bytes: &[u8]) {
        self.outbox.set_emit(Emit::Buffered);
        self.outbox.bytes_ptr = bytes.as_ptr() as usize as u32;
        self.outbox.bytes_len = bytes.len() as u32;
    }

    /// Frame what was emitted into units, at these offsets into it.
    ///
    /// One unit is one write at a byte sink, one datagram at a datagram sink,
    /// and one call to every stage below, so ask only when the splits are the
    /// point. The trailing unit closes itself and needs no boundary.
    pub fn units(&mut self, bounds: &[u32]) {
        self.outbox.bounds_ptr = bounds.as_ptr() as usize as u32;
        self.outbox.bounds_len = bounds.len() as u32;
    }

    /// Queue a log record, which the host emits tagged with this stage's name.
    ///
    /// `&'static str` because the host reads the text after the call returns.
    pub fn log(&mut self, level: Level, message: &'static str) {
        let index = self.outbox.logs_len as usize;

        if index >= MAX_LOGS {
            return;
        }

        self.logs[index] = LogRecord {
            level: level.as_u32(),
            ptr: message.as_ptr() as usize as u32,
            len: message.len() as u32,
        };

        self.outbox.logs_ptr = self.logs.as_ptr() as usize as u32;
        self.outbox.logs_len += 1;
    }

    /// End the path: upstream end of stream arriving early rather than a
    /// failure. What is already emitted is written, the stages below are
    /// drained, and tocat exits successfully.
    pub fn halt(&mut self, reason: &'static str) {
        self.outbox.set(TOCAT_FLAG_HALT);
        self.set_message(reason);
    }

    /// Fail the path. From [`Guest::init`] this is how an option is rejected,
    /// and becomes a startup error carrying this message.
    pub fn fail(&mut self, message: &'static str) {
        self.outbox.set(TOCAT_FLAG_ERROR);
        self.set_message(message);
    }

    /// Ask the host to wait before reading upstream again.
    ///
    /// Nothing is buffered: the read simply does not happen, which on a socket
    /// closes the receive window and slows the peer at source. Where several
    /// stages ask on one chunk, the longest request wins rather than the sum.
    pub fn pace(&mut self, wait: Duration) {
        self.outbox.set(TOCAT_FLAG_PACE);
        self.outbox.pace_ns = wait.as_nanos().min(u64::MAX as u128) as u64;
    }

    /// Restart this stage's tick schedule from now, which is how a deadline
    /// gets measured from the last byte rather than from wherever the host's
    /// cadence had reached.
    pub fn rearm(&mut self) {
        self.outbox.set(TOCAT_FLAG_REARM);
    }

    fn set_message(&mut self, message: &'static str) {
        self.outbox.message_ptr = message.as_ptr() as usize as u32;
        self.outbox.message_len = message.len() as u32;
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the macro needs that a guest should not have to name.
#[doc(hidden)]
pub mod private {
    use tocat_wasm_abi::pack_boundaries;

    use super::{Context, Guest};

    /// The host's pointer is an address in our own memory, so this is a cast
    /// rather than a calculation.
    ///
    /// # Safety
    ///
    /// Called only from the generated exports, with the pointer and length the
    /// host just wrote.
    pub unsafe fn slice<'a>(ptr: i32, len: i32) -> &'a [u8] {
        if ptr == 0 || len <= 0 {
            return &[];
        }

        unsafe { core::slice::from_raw_parts(ptr as usize as *const u8, len as usize) }
    }

    /// The two boundary answers in the one word the host reads. Both are
    /// associated constants, so this folds away entirely.
    pub fn boundaries<G: Guest>() -> i32 {
        pack_boundaries(G::BOUNDARIES, G::NEEDS) as i32
    }

    pub fn tick_interval_ns<G: Guest>(guest: &G) -> i64 {
        match guest.tick_interval() {
            Some(interval) => interval.as_nanos().min(i64::MAX as u128) as i64,
            None => 0,
        }
    }

    pub fn init<G: Guest>(guest: &mut G, ctx: &mut Context, config: &[u8]) {
        ctx.reset();
        guest.init(ctx, config);
    }

    pub fn on_bytes<G: Guest>(guest: &mut G, ctx: &mut Context, input: &[u8]) {
        ctx.reset();
        guest.on_bytes(ctx, input);
    }

    pub fn on_eof<G: Guest>(guest: &mut G, ctx: &mut Context) {
        ctx.reset();
        guest.on_eof(ctx);
    }

    pub fn on_tick<G: Guest>(guest: &mut G, ctx: &mut Context) {
        ctx.reset();
        guest.on_tick(ctx);
    }
}

/// Turn a [`Guest`] into a module.
///
/// ```ignore
/// export_guest!(Upper, arena_bytes = 256 * 1024);
/// ```
///
/// Defines the arena the host writes chunks into, the outbox it reads back,
/// the panic handler, and all nine exports. Every export is generated whether
/// or not the guest implements the corresponding hook, which changes nothing
/// the host does: it asks for the tick period, and a guest without one answers
/// zero, so no timer is built.
///
/// The arena is a static array rather than a heap: the host never frees,
/// writes exactly the length it asked for, and asks again on the next chunk.
/// A chunk larger than the arena is refused, which fails that direction with a
/// message saying so, so size it at or above the relay's copy buffer (256 KiB
/// by default) or run the relay with a matching `-b`.
#[macro_export]
macro_rules! export_guest {
    ($guest:ty,arena_bytes = $arena_bytes:expr) => {
        const _: () = {
            assert!(
                $arena_bytes > 0,
                "a tocat guest needs an arena for the host to write chunks into"
            );
        };

        static mut TOCAT_ARENA: [u8; $arena_bytes] = [0; $arena_bytes];
        static mut TOCAT_GUEST: $guest = <$guest as $crate::Guest>::INIT;
        static mut TOCAT_CONTEXT: $crate::Context = $crate::Context::new();

        /// Both statics are reached only from the exports below, which the
        /// host calls one at a time on one thread: a wasm module has no
        /// threads to race with.
        fn tocat_state() -> (&'static mut $guest, &'static mut $crate::Context) {
            unsafe { (&mut *(&raw mut TOCAT_GUEST), &mut *(&raw mut TOCAT_CONTEXT)) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tocat_abi_version() -> i32 {
            $crate::TOCAT_ABI_VERSION as i32
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tocat_outbox() -> i32 {
            let (_, ctx) = tocat_state();
            ctx.outbox_addr()
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tocat_alloc(len: i32) -> i32 {
            if len < 0 || len as usize > $arena_bytes {
                // Refusing the chunk fails the direction, which is better than
                // truncating it and better than writing past the arena.
                return 0;
            }

            (&raw mut TOCAT_ARENA) as usize as i32
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tocat_init(ptr: i32, len: i32) {
            let (guest, ctx) = tocat_state();
            let config = unsafe { $crate::private::slice(ptr, len) };

            $crate::private::init(guest, ctx, config);
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tocat_on_bytes(ptr: i32, len: i32) {
            let (guest, ctx) = tocat_state();
            let input = unsafe { $crate::private::slice(ptr, len) };

            $crate::private::on_bytes(guest, ctx, input);
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tocat_on_eof() {
            let (guest, ctx) = tocat_state();
            $crate::private::on_eof(guest, ctx);
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tocat_on_tick() {
            let (guest, ctx) = tocat_state();
            $crate::private::on_tick(guest, ctx);
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tocat_tick_interval_ns() -> i64 {
            let (guest, _) = tocat_state();
            $crate::private::tick_interval_ns(guest)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tocat_boundaries() -> i32 {
            $crate::private::boundaries::<$guest>()
        }

        /// A trap fails the direction, which is the right answer for a stage
        /// that cannot process the bytes it was given. Skipped for tests,
        /// which link against std and bring their own.
        #[cfg(all(target_arch = "wasm32", not(test)))]
        #[panic_handler]
        fn tocat_panic(_: &core::panic::PanicInfo) -> ! {
            core::arch::wasm32::unreachable()
        }
    };
}
