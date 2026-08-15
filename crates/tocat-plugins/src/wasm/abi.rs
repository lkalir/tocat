//! The guest ABI, version 2.
//!
//! The whole ABI is one call in and one struct out. A guest exports plain
//! functions and a linear memory, and imports nothing at all: no clock, no
//! files, no sockets, no host functions of any kind. That is not a restriction
//! bolted on for safety, it is what the [`Plugin`](tocat_api::Plugin) contract
//! already says. A stage decides what to forward and queues everything else
//! for the host to perform, so there is nothing for a guest to import.
//!
//! Refusing imports outright is therefore both the capability boundary and a
//! useful error message: a module built against WASI is rejected at startup,
//! naming the import, rather than trapping on the first chunk.
//!
//! # Exports
//!
//! | Export                   | Signature      | Required | Meaning                                          |
//! |--------------------------|----------------|----------|--------------------------------------------------|
//! | `memory`                 | memory         | yes      | The guest's linear memory                        |
//! | `tocat_abi_version`      | `() -> i32`    | yes      | Must be [`ABI_VERSION`]                          |
//! | `tocat_outbox`           | `() -> i32`    | yes      | Pointer to the guest's [outbox](Outbox)          |
//! | `tocat_alloc`            | `(i32) -> i32` | yes      | Somewhere the host may write `len` input bytes   |
//! | `tocat_on_bytes`         | `(i32, i32)`   | yes      | A chunk at that pointer and length               |
//! | `tocat_init`             | `(i32, i32)`   | no       | The entry's `config`, as JSON, once              |
//! | `tocat_on_eof`           | `()`           | no       | Upstream is finished                             |
//! | `tocat_on_tick`          | `()`           | no       | The schedule came due                            |
//! | `tocat_tick_interval_ns` | `() -> i64`    | no       | Requested tick period, 0 for none. Read once     |
//! | `tocat_boundaries`       | `() -> i32`    | no       | Boundary effect and requirement. Read once       |
//!
//! `tocat_boundaries` packs two answers into one word: `TOCAT_BOUNDARIES_*` in
//! bits 0 and 1 for what the stage does to message boundaries, and
//! `TOCAT_NEEDS_*` in bits 2 and 3 for what it needs of the path. Zero, which
//! is what a guest not exporting it is taken to mean, claims nothing and asks
//! for nothing. Any other bit is a guest built against a later ABI and is
//! refused at load.
//!
//! `tocat_alloc` is an arena, not a heap: the host never frees, writes exactly
//! `len` bytes, and is free to call it again on the next chunk. Returning one
//! static buffer, grown when a chunk does not fit, is a complete
//! implementation. Returning 0 refuses the chunk and fails the direction.
//!
//! # Pointers are absolute
//!
//! Every pointer crossing this ABI, in both directions, is a byte offset into
//! the guest's linear memory as a whole, not into whatever the guest is using
//! as an arena. A guest built around a static array has to add that array's
//! own address to every offset it hands over, since the linker rather than the
//! guest decides where it lands.
//!
//! Getting that wrong does not trap, because both sides then read memory that
//! exists: the guest writes its outbox into its array while the host reads the
//! address the guest named. The symptom is an outbox that decodes as all
//! zeros, which is `EMIT_DROP`, which is a stage that silently swallows the
//! stream.
//!
//!
//! # The outbox
//!
//! After every call the host reads the struct at `tocat_outbox()` and applies
//! it. It is fixed-layout, little-endian, and [`OUTBOX_LEN`] bytes long, so
//! decoding is a handful of loads and no allocation. Every pointer in it is an
//! offset into the guest's own memory, and the host copies what it needs
//! before handing control back.
//!
//! | Offset | Type  | Field         | Meaning                                                        |
//! |--------|-------|---------------|----------------------------------------------------------------|
//! | 0      | `u32` | `emit`        | [`EMIT_DROP`], [`EMIT_PASSTHROUGH`] or [`EMIT_BUFFERED`]        |
//! | 4      | `u32` | `bytes_ptr`   | Emitted bytes, when buffered                                   |
//! | 8      | `u32` | `bytes_len`   |                                                                |
//! | 12     | `u32` | `bounds_ptr`  | `u32` offsets into those bytes, one per unit boundary          |
//! | 16     | `u32` | `bounds_len`  | Count, not bytes. Zero means one unit covering everything      |
//! | 20     | `u32` | `flags`       | [`FLAG_REARM`], [`FLAG_HALT`], [`FLAG_PACE`], [`FLAG_ERROR`]   |
//! | 24     | `u32` | `message_ptr` | UTF-8, the halt reason or the error                            |
//! | 28     | `u32` | `message_len` |                                                                |
//! | 32     | `u64` | `pace_ns`     | How long to wait before reading again                          |
//! | 40     | `u32` | `logs_ptr`    | Records of `(level: u32, ptr: u32, len: u32)`                  |
//! | 44     | `u32` | `logs_len`    | Count of records                                               |
//!
//! A guest that forgets to reset the outbox between calls repeats itself
//! rather than corrupting anything, which is the failure mode worth having.
//!
//! # Not here yet
//!
//! Side channels. [`open_channel`](tocat_api::BuildCtx::open_channel) happens
//! at build time and hands back an id, so it needs a round trip the ABI does
//! not have: the guest would declare its targets during `tocat_init`, the host
//! would open them, and the ids would have to be written back before the first
//! chunk. Until then a guest that wants to record something logs it.

// The wire format lives in `tocat-wasm-abi`, which is also what the C header is
// generated from and what a Rust guest writes through `tocat-wasm-sdk`. The
// host reading its own copy of these numbers is how the three ends of one ABI
// drift apart, so it reads theirs.
use tocat_api::{PluginError, Result};
pub use tocat_wasm_abi::{
    TOCAT_EMIT_BUFFERED as EMIT_BUFFERED, TOCAT_EMIT_PASSTHROUGH as EMIT_PASSTHROUGH,
    TOCAT_EMIT_PENDING as EMIT_DROP, TOCAT_FLAG_ERROR as FLAG_ERROR, TOCAT_FLAG_HALT as FLAG_HALT,
    TOCAT_FLAG_PACE as FLAG_PACE, TOCAT_FLAG_REARM as FLAG_REARM, exports::BOUNDARIES,
    unpack_boundaries,
};

use super::NAME;

/// Bumped for any change to the layout below. A guest reporting a different
/// version is rejected at startup rather than being read as garbage.
pub const ABI_VERSION: i32 = tocat_wasm_abi::TOCAT_ABI_VERSION as i32;

pub const OUTBOX_LEN: usize = tocat_wasm_abi::TOCAT_OUTBOX_LEN as usize;

/// Bytes per record in the guest's log array.
pub const LOG_RECORD_LEN: u32 = tocat_wasm_abi::TOCAT_LOG_RECORD_LEN;

/// One decoded outbox. Every field is an offset and a length in the guest's
/// memory; nothing is copied until the host applies it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Outbox {
    pub emit: u32,
    pub bytes: Span,
    pub bounds: Span,
    pub flags: u32,
    pub message: Span,
    pub pace_ns: u64,
    pub logs: Span,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Span {
    pub ptr: u32,
    pub len: u32,
}

impl Outbox {
    /// Decode the struct at `at`, which is whatever `tocat_outbox()` returned.
    ///
    /// Bounds are checked here rather than at each use, so everything after
    /// this is a slice of memory that is known to exist.
    pub fn read(memory: &[u8], at: u32) -> Result<Self> {
        let head = slice(memory, at, OUTBOX_LEN as u32)?;

        let u32_at = |offset: usize| -> u32 {
            u32::from_le_bytes([
                head[offset],
                head[offset + 1],
                head[offset + 2],
                head[offset + 3],
            ])
        };

        let pace_ns = {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&head[32..40]);
            u64::from_le_bytes(bytes)
        };

        Ok(Self {
            emit: u32_at(0),
            bytes: Span {
                ptr: u32_at(4),
                len: u32_at(8),
            },
            bounds: Span {
                ptr: u32_at(12),
                len: u32_at(16),
            },
            flags: u32_at(20),
            message: Span {
                ptr: u32_at(24),
                len: u32_at(28),
            },
            pace_ns,
            logs: Span {
                ptr: u32_at(40),
                len: u32_at(44),
            },
        })
    }

    pub fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }
}

/// A guest span as a slice of guest memory, or an error naming what was out of
/// range. A guest handing back a wild pointer is a bug in the guest, not a
/// vulnerability in the host: linear memory is the whole of what it can reach.
pub fn slice(memory: &[u8], ptr: u32, len: u32) -> Result<&[u8]> {
    let start = ptr as usize;
    let end = start.checked_add(len as usize);

    match end {
        Some(end) if end <= memory.len() => Ok(&memory[start..end]),
        _ => Err(PluginError::runtime(
            NAME,
            format!("guest returned a span outside its memory: {len} bytes at {ptr:#x}"),
        )),
    }
}

/// The bounds array as offsets. Read as `u32`s rather than borrowed, since a
/// guest is under no obligation to align them.
pub fn bounds(memory: &[u8], span: Span, limit: u32) -> Result<Vec<usize>> {
    let raw = slice(memory, span.ptr, span.len.saturating_mul(4))?;

    raw.chunks_exact(4)
        .map(|bytes| {
            let offset = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);

            if offset > limit {
                return Err(PluginError::runtime(
                    NAME,
                    format!("guest emitted a boundary at {offset} of {limit} bytes"),
                ));
            }

            Ok(offset as usize)
        })
        .collect()
}

/// Guest log levels. Anything unrecognised, including a guest that never set
/// the field, is info: a record it bothered to queue is still worth seeing.
pub fn log_level(raw: u32) -> tocat_api::LogLevel {
    tocat_wasm_abi::Level::from_u32(raw)
}
