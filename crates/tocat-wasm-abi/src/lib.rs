//! The tocat WebAssembly guest ABI, version 2.
//!
//! One definition of the wire format, used by everything that touches it:
//!
//! - the host reads an [`Outbox`] out of guest memory after every call
//! - `tocat-wasm-sdk` writes one, on behalf of a Rust guest
//! - `sdk/wasm/include/tocat/abi.h` is generated from this crate, so a C or C++
//!   guest sees the same constants and the same struct rather than a
//!   hand-copied transcription of them
//!
//! Regenerate that header with:
//!
//! ```console
//! $ cargo run -p tocat-wasm-abi --example tocat-abi-header
//! ```
//!
//! and check it is current with `--check`, which is what CI should run.
//!
//! # What is here and what is not
//!
//! Layout, constants, and the conversions between them and Rust types.
//! Nothing else: no allocation, no error type, no I/O, no dependencies, and
//! `no_std`, because a guest compiled to wasm32 has none of those.
//!
//! Every wire value appears twice by design. `TOCAT_EMIT_BUFFERED` is the name
//! C sees, and [`Emit::Buffered`] is the name Rust sees; the first is what the
//! header generator emits, the second is what gets exhaustive matching. They
//! cannot disagree, because the enum discriminants are the constants.
//!
//! # The pointer rule
//!
//! Every pointer in an [`Outbox`] is an address in the guest's linear memory,
//! not an offset into whatever the guest uses as an arena. In Rust, as in C,
//! that is what a pointer already is, so this is a cast rather than a
//! calculation. Getting it wrong does not trap: both sides read memory that
//! exists, and the symptom is an outbox that decodes as all zeroes, which is
//! [`Emit::Pending`], which is a stage that silently swallows the stream.

#![no_std]

use core::mem::{offset_of, size_of};

/// Bumped for any change to what the host reads or to what a value means: the
/// struct below, the set of exports, or the interpretation of either. A guest
/// reporting a different version is refused when it loads rather than being
/// read as garbage.
///
/// The check is exact equality in both directions, which is the point. A host
/// that silently accepted a newer guest would honour the parts of its contract
/// it recognised and ignore the rest, and the one it ignored would be a
/// requirement the guest cannot work without.
pub const TOCAT_ABI_VERSION: u32 = 2;

/// Bytes the host reads at `tocat_outbox()`.
pub const TOCAT_OUTBOX_LEN: u32 = 48;

/// Bytes per record in the log array.
pub const TOCAT_LOG_RECORD_LEN: u32 = 12;

/// Forward nothing. Emitting nothing means the same thing; this exists so that
/// a filter can say it on purpose.
pub const TOCAT_EMIT_PENDING: u32 = 0;
/// Forward the input unchanged. The host does not read the guest's bytes at
/// all, and nothing is copied in either direction.
pub const TOCAT_EMIT_PASSTHROUGH: u32 = 1;
/// Forward `bytes`, framed by `bounds`.
pub const TOCAT_EMIT_BUFFERED: u32 = 2;

/// Restart this stage's tick schedule from now.
pub const TOCAT_FLAG_REARM: u32 = 1 << 0;
/// End the path: upstream end of stream arriving early, and a success.
pub const TOCAT_FLAG_HALT: u32 = 1 << 1;
/// Wait `pace_ns` before reading upstream again.
pub const TOCAT_FLAG_PACE: u32 = 1 << 2;
/// Fail the path, with `message` as the reason.
pub const TOCAT_FLAG_ERROR: u32 = 1 << 3;

/// Mask for the boundary effect in `tocat_boundaries`: bits 0 and 1.
pub const TOCAT_BOUNDARIES_MASK: u32 = 0b11;
/// The units this stage was given do not reach the stage below. Anything that
/// buffers across calls, splits, or coalesces.
pub const TOCAT_BOUNDARIES_FUSE: u32 = 0;
/// One unit in, one unit out.
pub const TOCAT_BOUNDARIES_PRESERVE: u32 = 1;
/// One unit in, one unit out, and the boundary is also written into the bytes,
/// so it survives a stage below that fuses. What `frame` does.
pub const TOCAT_BOUNDARIES_SEAL: u32 = 2;
/// The units below are read out of the bytes rather than inherited from above,
/// so the ones from above do not survive. What `unframe` does.
pub const TOCAT_BOUNDARIES_SPLIT: u32 = 3;

/// Mask for the requirement in `tocat_boundaries`: bits 2 and 3.
pub const TOCAT_NEEDS_MASK: u32 = 0b1100;
/// The stage works on any path.
pub const TOCAT_NEEDS_NOTHING: u32 = 0;
/// Every call must carry one whole message, so boundaries have to reach this
/// stage from the endpoint above or from a `TOCAT_BOUNDARIES_SPLIT` stage.
pub const TOCAT_NEEDS_UPSTREAM: u32 = 1 << 2;
/// The units this stage emits must reach the endpoint below or a
/// `TOCAT_BOUNDARIES_SEAL` stage, or what it emitted cannot be read back.
pub const TOCAT_NEEDS_DOWNSTREAM: u32 = 1 << 3;
/// Both of the above.
pub const TOCAT_NEEDS_BOTH: u32 = TOCAT_NEEDS_UPSTREAM | TOCAT_NEEDS_DOWNSTREAM;

pub const TOCAT_TRACE: u32 = 0;
pub const TOCAT_DEBUG: u32 = 1;
pub const TOCAT_INFO: u32 = 2;
pub const TOCAT_WARN: u32 = 3;
pub const TOCAT_ERROR: u32 = 4;

/// One queued log record: a level, and a string in the guest's memory.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogRecord {
    pub level: u32,
    pub ptr: u32,
    pub len: u32,
}

/// What a call left behind for the host.
///
/// Fixed layout, little-endian, [`TOCAT_OUTBOX_LEN`] bytes. `repr(C)` rather
/// than `packed`: wasm32 puts the `u64` on an eight-byte boundary, which is
/// where offset 32 already is, so there is no padding to remove and no
/// unaligned field to read. The assertions below are what keep that true on
/// every target this crate is built for, including the 64-bit host that reads
/// the struct back out of guest memory.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outbox {
    pub emit: u32,
    pub bytes_ptr: u32,
    pub bytes_len: u32,
    pub bounds_ptr: u32,
    pub bounds_len: u32,
    pub flags: u32,
    pub message_ptr: u32,
    pub message_len: u32,
    pub pace_ns: u64,
    pub logs_ptr: u32,
    pub logs_len: u32,
}

const _: () = {
    assert!(size_of::<Outbox>() == TOCAT_OUTBOX_LEN as usize);
    assert!(offset_of!(Outbox, emit) == 0);
    assert!(offset_of!(Outbox, bytes_ptr) == 4);
    assert!(offset_of!(Outbox, bytes_len) == 8);
    assert!(offset_of!(Outbox, bounds_ptr) == 12);
    assert!(offset_of!(Outbox, bounds_len) == 16);
    assert!(offset_of!(Outbox, flags) == 20);
    assert!(offset_of!(Outbox, message_ptr) == 24);
    assert!(offset_of!(Outbox, message_len) == 28);
    assert!(offset_of!(Outbox, pace_ns) == 32);
    assert!(offset_of!(Outbox, logs_ptr) == 40);
    assert!(offset_of!(Outbox, logs_len) == 44);

    assert!(size_of::<LogRecord>() == TOCAT_LOG_RECORD_LEN as usize);
};

impl Outbox {
    pub const fn new() -> Self {
        Self {
            emit: TOCAT_EMIT_PENDING,
            bytes_ptr: 0,
            bytes_len: 0,
            bounds_ptr: 0,
            bounds_len: 0,
            flags: 0,
            message_ptr: 0,
            message_len: 0,
            pace_ns: 0,
            logs_ptr: 0,
            logs_len: 0,
        }
    }

    /// Clear it. The struct persists between calls, so a halt flag or a
    /// message pointer left over from an earlier chunk would be applied again.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub const fn emit(&self) -> Option<Emit> {
        Emit::from_u32(self.emit)
    }

    pub const fn set_emit(&mut self, emit: Emit) {
        self.emit = emit.as_u32();
    }

    pub const fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }

    pub const fn set(&mut self, flag: u32) {
        self.flags |= flag;
    }
}

/// What a stage decided to do with the chunk it was given.
#[repr(u32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Emit {
    /// Nothing emitted; the chunk stops here.
    #[default]
    Pending = TOCAT_EMIT_PENDING,
    /// Input forwarded verbatim. The host reuses the input slice, copying
    /// nothing.
    Passthrough = TOCAT_EMIT_PASSTHROUGH,
    /// The stage wrote its own bytes into the output buffer, and any framing
    /// it declared along with them.
    Buffered = TOCAT_EMIT_BUFFERED,
}

impl Emit {
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            TOCAT_EMIT_PENDING => Some(Self::Pending),
            TOCAT_EMIT_PASSTHROUGH => Some(Self::Passthrough),
            TOCAT_EMIT_BUFFERED => Some(Self::Buffered),
            _ => None,
        }
    }

    pub const fn as_u32(self) -> u32 {
        self as u32
    }
}

/// What a stage does to the message boundaries passing through it.
///
/// Read once, after `tocat_init`, out of the low two bits of
/// `tocat_boundaries`. The host folds these along the chain to answer one
/// question per requiring stage: do that stage's units survive as far as they
/// have to. Nothing here is consulted on the per-chunk path.
///
/// [`Fuse`](Self::Fuse) is the default and the safe answer, because it claims
/// nothing: a stage that has not thought about boundaries cannot be relied on
/// to keep them.
#[repr(u32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Boundaries {
    #[default]
    Fuse = TOCAT_BOUNDARIES_FUSE,
    Preserve = TOCAT_BOUNDARIES_PRESERVE,
    Seal = TOCAT_BOUNDARIES_SEAL,
    Split = TOCAT_BOUNDARIES_SPLIT,
}

impl Boundaries {
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            TOCAT_BOUNDARIES_FUSE => Some(Self::Fuse),
            TOCAT_BOUNDARIES_PRESERVE => Some(Self::Preserve),
            TOCAT_BOUNDARIES_SEAL => Some(Self::Seal),
            TOCAT_BOUNDARIES_SPLIT => Some(Self::Split),
            _ => None,
        }
    }

    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Whether one message arriving still means one message leaving.
    ///
    /// True for [`Preserve`](Self::Preserve) and [`Seal`](Self::Seal): sealing
    /// writes framing into the payload, which changes the bytes of a datagram
    /// without changing how many there are. This is what the host warns about
    /// on a path whose destination is a datagram endpoint.
    pub const fn preserves_messages(self) -> bool {
        matches!(self, Self::Preserve | Self::Seal)
    }

    /// Whether a requirement scanning downwards passes this stage without
    /// being settled either way.
    ///
    /// Only [`Preserve`](Self::Preserve) does. [`Seal`](Self::Seal) settles it
    /// in favour, the other two against, which is why the scan stops at all
    /// three and asks [`satisfies_downstream`](Self::satisfies_downstream)
    /// which it was.
    pub const fn passes_downstream(self) -> bool {
        matches!(self, Self::Preserve)
    }

    /// Whether a requirement scanning upwards passes this stage without being
    /// settled either way.
    ///
    /// [`Seal`](Self::Seal) does, because it emits one unit for every unit it
    /// was given; sealing only settles a scan going the other way.
    pub const fn passes_upstream(self) -> bool {
        matches!(self, Self::Preserve | Self::Seal)
    }

    /// Whether a downstream requirement that reached this stage is met by it,
    /// so that nothing below can invalidate it.
    pub const fn satisfies_downstream(self) -> bool {
        matches!(self, Self::Seal)
    }

    /// Whether an upstream requirement that reached this stage is met by it.
    pub const fn satisfies_upstream(self) -> bool {
        matches!(self, Self::Split)
    }
}

/// What a stage needs of the path it is placed on.
///
/// Read once, out of bits 2 and 3 of `tocat_boundaries`. Unlike
/// [`Boundaries`], which the host only warns about, an unmet requirement is a
/// configuration error: a stage saying this cannot do its job at all.
///
/// The two sides are separate because the stages that want them want opposite
/// ones. A stage that seals a message and appends a tag makes its own
/// boundaries and needs them to survive downwards; the stage that verifies and
/// strips that tag needs whole messages from above and does not care what
/// happens below it.
#[repr(u32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Needs {
    #[default]
    Nothing = TOCAT_NEEDS_NOTHING,
    Upstream = TOCAT_NEEDS_UPSTREAM,
    Downstream = TOCAT_NEEDS_DOWNSTREAM,
    Both = TOCAT_NEEDS_BOTH,
}

impl Needs {
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            TOCAT_NEEDS_NOTHING => Some(Self::Nothing),
            TOCAT_NEEDS_UPSTREAM => Some(Self::Upstream),
            TOCAT_NEEDS_DOWNSTREAM => Some(Self::Downstream),
            TOCAT_NEEDS_BOTH => Some(Self::Both),
            _ => None,
        }
    }

    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    pub const fn upstream(self) -> bool {
        matches!(self, Self::Upstream | Self::Both)
    }

    pub const fn downstream(self) -> bool {
        matches!(self, Self::Downstream | Self::Both)
    }
}

/// Pack what `tocat_boundaries` returns.
pub const fn pack_boundaries(boundaries: Boundaries, needs: Needs) -> u32 {
    boundaries.as_u32() | needs.as_u32()
}

/// Read what `tocat_boundaries` returned.
///
/// `None` for any bit outside the two masks, which is a guest built against a
/// later ABI than this host speaks. Refusing it is the point: reading an
/// unknown value as [`Boundaries::Fuse`] would run a stage whose requirement
/// the host cannot see, and the symptom would be a corrupt stream rather than
/// an error.
///
/// Zero is a fixed point: it is what a guest that does not export the function
/// at all is taken to have answered, so [`Boundaries::Fuse`] and
/// [`Needs::Nothing`] have to stay at 0. Both are the reading that claims
/// nothing and asks for nothing, which is the only safe thing to assume of a
/// stage that did not say.
pub const fn unpack_boundaries(value: u32) -> Option<(Boundaries, Needs)> {
    if value & !(TOCAT_BOUNDARIES_MASK | TOCAT_NEEDS_MASK) != 0 {
        return None;
    }

    match (
        Boundaries::from_u32(value & TOCAT_BOUNDARIES_MASK),
        Needs::from_u32(value & TOCAT_NEEDS_MASK),
    ) {
        (Some(boundaries), Some(needs)) => Some((boundaries, needs)),
        _ => None,
    }
}

/// Severity of a queued log record, in the order every logging library writes
/// them.
#[repr(u32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace = TOCAT_TRACE,
    Debug = TOCAT_DEBUG,
    /// What a record with an unrecognised level is read as: a guest that
    /// bothered to queue one should still be heard.
    #[default]
    Info = TOCAT_INFO,
    Warn = TOCAT_WARN,
    Error = TOCAT_ERROR,
}

impl Level {
    pub const fn from_u32(value: u32) -> Self {
        match value {
            TOCAT_TRACE => Self::Trace,
            TOCAT_DEBUG => Self::Debug,
            TOCAT_WARN => Self::Warn,
            TOCAT_ERROR => Self::Error,
            _ => Self::Info,
        }
    }

    pub const fn as_u32(self) -> u32 {
        self as u32
    }
}

/// The names a guest exports, so that a host looks them up from the same place
/// a guest is documented against.
pub mod exports {
    pub const MEMORY: &str = "memory";
    pub const ABI_VERSION: &str = "tocat_abi_version";
    pub const OUTBOX: &str = "tocat_outbox";
    pub const ALLOC: &str = "tocat_alloc";
    pub const INIT: &str = "tocat_init";
    pub const ON_BYTES: &str = "tocat_on_bytes";
    pub const ON_EOF: &str = "tocat_on_eof";
    pub const ON_TICK: &str = "tocat_on_tick";
    pub const TICK_INTERVAL_NS: &str = "tocat_tick_interval_ns";
    pub const BOUNDARIES: &str = "tocat_boundaries";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_enums_are_the_constants() {
        assert_eq!(Emit::Buffered.as_u32(), TOCAT_EMIT_BUFFERED);
        assert_eq!(Level::Warn.as_u32(), TOCAT_WARN);
        assert_eq!(
            Emit::from_u32(TOCAT_EMIT_PASSTHROUGH),
            Some(Emit::Passthrough)
        );
        assert_eq!(Emit::from_u32(3), None);
        assert_eq!(Level::from_u32(99), Level::Info);
        assert_eq!(Boundaries::Seal.as_u32(), TOCAT_BOUNDARIES_SEAL);
        assert_eq!(Needs::Downstream.as_u32(), TOCAT_NEEDS_DOWNSTREAM);
    }

    #[test]
    fn boundaries_round_trip_through_one_word() {
        for boundaries in [
            Boundaries::Fuse,
            Boundaries::Preserve,
            Boundaries::Seal,
            Boundaries::Split,
        ] {
            for needs in [
                Needs::Nothing,
                Needs::Upstream,
                Needs::Downstream,
                Needs::Both,
            ] {
                let packed = pack_boundaries(boundaries, needs);
                assert_eq!(unpack_boundaries(packed), Some((boundaries, needs)));
            }
        }
    }

    /// A guest that does not export the function is read as having answered
    /// zero, so zero has to keep meaning the claim that asks for nothing.
    #[test]
    fn zero_claims_nothing_and_asks_for_nothing() {
        assert_eq!(
            unpack_boundaries(0),
            Some((Boundaries::Fuse, Needs::Nothing))
        );
    }

    /// A guest built against a later ABI is refused rather than read as a
    /// stage that claims nothing.
    #[test]
    fn an_unknown_bit_is_refused() {
        assert_eq!(unpack_boundaries(1 << 4), None);
        assert_eq!(unpack_boundaries(u32::MAX), None);
    }

    #[test]
    fn an_outbox_starts_and_resets_empty() {
        let mut outbox = Outbox::new();
        assert_eq!(outbox.emit(), Some(Emit::Pending));

        outbox.set(TOCAT_FLAG_HALT);
        outbox.set_emit(Emit::Buffered);
        assert!(outbox.has(TOCAT_FLAG_HALT));

        outbox.reset();
        assert_eq!(outbox, Outbox::new());
        assert!(!outbox.has(TOCAT_FLAG_HALT));
    }
}
