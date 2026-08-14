//! `frame` / `unframe`: put message boundaries on a byte stream, and take
//! them off again.
//!
//! # What they are for
//!
//! On a datagram path a chunk is a message and there is nothing to do. On a
//! byte stream a chunk is an arbitrary slice, so a stage that needs whole
//! messages (`unbase64`, a decrypt stage, anything with a per-message header)
//! has no way to find them. `unframe` is where that knowledge lives: it
//! accumulates until it has a whole message, then emits it as one unit, which
//! the pipeline delivers as one [`Plugin::on_bytes`] call to every stage
//! below.
//!
//! `frame` is the inverse, and it takes the boundaries it is given: one unit
//! in, one framed unit out. It is only meaningful where those boundaries mean
//! something, which is a datagram path or the output of a stage that declared
//! them.
//!
//! # Modes
//!
//! Five, in two families. The terminator family scans for a byte string and
//! pays for a payload that could contain it; the counted family reads a header
//! and pays nothing for the payload at all.
//!
//! | Mode | Framing | Overhead | Payload |
//! | ---- | ------- | -------- | ------- |
//! | `delimiter` | a byte string, newline by default | delimiter | must not contain the delimiter |
//! | `cobs` | zero byte, payload stuffed | 1 + 1 per 254 bytes | any |
//! | `slip` | `0xc0`, payload escaped | 1 + 1 per `0xc0` or `0xdb` | any |
//! | `length` | fixed-width big-endian prefix | 1, 2, 4 or 8 | any, up to what the width holds |
//! | `netstring` | `LEN:payload,` | 3 or so | any |
//!
//! `delimiter` is the one to reach for over text: cheap, greppable, and a
//! stage like `base64` guarantees the payload cannot contain a newline.
//!
//! `cobs` ([consistent overhead byte stuffing]) and `slip` ([RFC 1055]) both
//! escape the payload so the terminator cannot appear in it. COBS is the
//! better of the two on every axis (bounded overhead, one byte per frame in
//! the common case, resynchronises at the next zero byte); SLIP is here
//! because serial hardware and embedded stacks already speak it. Reach for
//! SLIP to talk to something that requires it, and COBS otherwise.
//!
//! `length` is exact and costs nothing per payload byte, and it is the only
//! mode that knows how big a message is before reading it, so `max-message`
//! rejects an oversized one from the header rather than after buffering it.
//! It cannot resynchronise: a receiver that joins mid-stream, or a sender that
//! gets one length wrong, is lost until the connection is remade.
//!
//! `netstring` ([the djb format]) is `length:payload,` with a decimal length.
//! It has the same properties as `length` with a self-describing, greppable
//! header, and the trailing comma catches a desynchronised stream on the very
//! next message instead of never.
//!
//! [consistent overhead byte stuffing]: https://doi.org/10.1109/90.769765
//! [RFC 1055]: https://www.rfc-editor.org/rfc/rfc1055
//! [the djb format]: https://cr.yp.to/proto/netstrings.txt
//!
//! # Pairing
//!
//! The pair nests like any other wrapping stage, so the entries read in the
//! order the forward path sees them:
//!
//! ```toml
//! [[plugin]]
//! name = "base64"
//! direction = "source-to-sink"
//!
//! [[plugin]]
//! name = "unbase64"
//! direction = "sink-to-source"
//!
//! [[plugin]]
//! name = "frame"
//! direction = "source-to-sink"
//!
//! [[plugin]]
//! name = "unframe"
//! direction = "sink-to-source"
//! ```
//!
//! Encode then frame going out; unframe then decode coming back, because the
//! reverse path walks the list backwards.
//!
//! # Bounded memory
//!
//! `unframe` holds a partial message, so a peer whose framing does not match
//! is a peer that asks the relay to buffer without limit. `max-message` caps
//! it and defaults to 1 MiB. A stream that exceeds it is a protocol error, not
//! a large message to be accommodated.

use serde::{Deserialize, Serialize};
use tocat_api::{BuildCtx, ByteSize, Ctx, Plugin, PluginError, PluginFactory, Result, Stage};

pub const FRAME: &str = "frame";
pub const UNFRAME: &str = "unframe";

/// Terminates a COBS frame and cannot appear inside one, which is the whole
/// point of the encoding.
const COBS_DELIMITER: u8 = 0;

/// A COBS block holds at most this many payload bytes before a new code byte
/// is needed. Fixed by the encoding, not a tuning knob.
const COBS_BLOCK: usize = 254;

/// RFC 1055's three special bytes: the terminator, the escape, and the two
/// bytes that stand for them once escaped.
const SLIP_END: u8 = 0xc0;
const SLIP_ESC: u8 = 0xdb;
const SLIP_ESC_END: u8 = 0xdc;
const SLIP_ESC_ESC: u8 = 0xdd;

/// A `usize` is at most 20 decimal digits, so a netstring header longer than
/// this is not a length however long the message turns out to be.
const NETSTRING_DIGITS: usize = 20;

const DEFAULT_LENGTH_BYTES: usize = 4;
const DEFAULT_MAX_MESSAGE: usize = 1024 * 1024;

/// Names the fix, since the alternative is a message the far end silently
/// reads as two.
const AMBIGUOUS_MESSAGE: &str = concat!(
    "message would frame as two: it contains the delimiter, or ends with a ",
    "prefix of one. Use mode = \"cobs\", or check = false if the peer's ",
    "parser tolerates it",
);

/// How messages are marked on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FrameMode {
    /// Terminate each message with `delimiter`.
    #[default]
    Delimiter,
    /// Stuff each message so it contains no zero byte, then terminate it with
    /// one.
    Cobs,
    /// Escape `0xc0` and `0xdb` in each message, then terminate it with
    /// `0xc0`.
    Slip,
    /// Prefix each message with its length, in a fixed number of bytes.
    Length,
    /// Wrap each message as `length:payload,` with a decimal length.
    Netstring,
}

impl FrameMode {
    /// The byte string that ends a frame, or none for a counted mode.
    ///
    /// Only the terminator modes scan, and only they can be confused by a
    /// payload, so this is what the two families are told apart by.
    fn terminator(self, configured: Option<&Delimiter>) -> Vec<u8> {
        match self {
            Self::Delimiter => configured.cloned().unwrap_or_default().0,
            Self::Cobs => vec![COBS_DELIMITER],
            Self::Slip => vec![SLIP_END],
            Self::Length | Self::Netstring => Vec::new(),
        }
    }

    /// What to call this mode in a message.
    fn name(self) -> &'static str {
        match self {
            Self::Delimiter => "delimiter",
            Self::Cobs => "cobs",
            Self::Slip => "slip",
            Self::Length => "length",
            Self::Netstring => "netstring",
        }
    }

    /// Whether a header describes the payload's length, so an oversized
    /// message can be refused before it is read.
    fn counted(self) -> bool {
        matches!(self, Self::Length | Self::Netstring)
    }

    /// Whether the payload reaches the wire as it was handed over.
    ///
    /// The counted modes and `delimiter` forward the caller's bytes; `cobs`
    /// and `slip` rewrite them, and so need a buffer of their own.
    fn escapes(self) -> bool {
        matches!(self, Self::Cobs | Self::Slip)
    }
}

/// Byte order of the `length` mode's header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Endian {
    /// Network order, and what a peer means unless it says otherwise.
    #[default]
    #[serde(alias = "network", alias = "be")]
    Big,
    /// Host order on every machine tocat runs on, and a common mistake to have
    /// to interoperate with.
    #[serde(alias = "le")]
    Little,
}

/// The width and byte order of a `length` header.
#[derive(Debug, Clone, Copy)]
struct LengthFormat {
    bytes: usize,
    endian: Endian,
}

impl LengthFormat {
    /// The largest message this header can describe.
    fn ceiling(self) -> u64 {
        match self.bytes {
            8 => u64::MAX,
            n => (1u64 << (8 * n)) - 1,
        }
    }

    fn encode(self, len: usize, out: &mut Vec<u8>) {
        let bytes = (len as u64).to_be_bytes();
        let start = bytes.len() - self.bytes;

        match self.endian {
            Endian::Big => out.extend_from_slice(&bytes[start..]),
            Endian::Little => out.extend(bytes[start..].iter().rev()),
        }
    }

    fn decode(self, header: &[u8]) -> u64 {
        let shift = |acc: u64, byte: u8| (acc << 8) | u64::from(byte);

        match self.endian {
            Endian::Big => header.iter().copied().fold(0, shift),
            Endian::Little => header.iter().rev().copied().fold(0, shift),
        }
    }
}

/// What to do with bytes left over when the stream ends part way through a
/// message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AtEof {
    /// Emit them as a final message.
    Emit,
    /// Fail the path.
    Error,
    /// Discard them.
    Drop,
}

impl AtEof {
    /// A text stream whose last line has no newline is routine, so
    /// `delimiter` mode emits it. Everywhere else a partial frame is a message
    /// that was cut off in transit rather than one the sender finished.
    fn default_for(mode: FrameMode) -> Self {
        match mode {
            FrameMode::Delimiter => Self::Emit,
            _ => Self::Error,
        }
    }
}

/// A message terminator, written as a string with the usual escapes.
///
/// `"\n"`, `"\r\n"`, `"\0"` and `"\x1e"` all work, on the command line as well
/// as in a config file, because the escapes are decoded here rather than
/// relying on a shell or a TOML parser to have done it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delimiter(Vec<u8>);

impl Default for Delimiter {
    fn default() -> Self {
        Self(vec![b'\n'])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDelimiterError(String);

impl std::fmt::Display for ParseDelimiterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseDelimiterError {}

impl std::str::FromStr for Delimiter {
    type Err = ParseDelimiterError;

    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let mut out = Vec::with_capacity(raw.len());
        let mut chars = raw.chars();

        while let Some(c) = chars.next() {
            if c != '\\' {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                continue;
            }

            match chars.next() {
                Some('n') => out.push(b'\n'),
                Some('r') => out.push(b'\r'),
                Some('t') => out.push(b'\t'),
                Some('0') => out.push(0),
                Some('\\') => out.push(b'\\'),
                Some('x') => {
                    let hex: String = chars.by_ref().take(2).collect();
                    let byte = u8::from_str_radix(&hex, 16).map_err(|_| {
                        ParseDelimiterError(format!("{hex:?} is not two hex digits"))
                    })?;
                    out.push(byte);
                }
                Some(other) => {
                    return Err(ParseDelimiterError(format!(
                        "unknown escape \\{other}; use \\n, \\r, \\t, \\0, \\\\ or \\xNN"
                    )));
                }
                None => return Err(ParseDelimiterError("trailing backslash".into())),
            }
        }

        if out.is_empty() {
            return Err(ParseDelimiterError(
                "an empty delimiter marks nothing".into(),
            ));
        }

        Ok(Self(out))
    }
}

impl std::fmt::Display for Delimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for &byte in &self.0 {
            match byte {
                b'\n' => f.write_str("\\n")?,
                b'\r' => f.write_str("\\r")?,
                b'\t' => f.write_str("\\t")?,
                0 => f.write_str("\\0")?,
                b'\\' => f.write_str("\\\\")?,
                0x20..=0x7e => write!(f, "{}", byte as char)?,
                other => write!(f, "\\x{other:02x}")?,
            }
        }

        Ok(())
    }
}

impl Serialize for Delimiter {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Delimiter {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;

        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct FrameConfig {
    /// How messages are marked on the wire.
    pub mode: FrameMode,

    /// The terminator, in `delimiter` mode.
    pub delimiter: Option<Delimiter>,

    /// Width of the header, in `length` mode: 1, 2, 4 or 8.
    pub length_bytes: Option<usize>,

    /// Byte order of the header, in `length` mode.
    pub endian: Option<Endian>,

    /// Reject a message that would frame as two, in `delimiter` mode.
    ///
    /// Costs one scan of each message. Turn it off only for a peer whose
    /// parser is known to tolerate it, or use a mode where the question cannot
    /// arise.
    pub check: Option<bool>,
}

impl Default for FrameConfig {
    fn default() -> Self {
        Self {
            mode: FrameMode::default(),
            delimiter: None,
            length_bytes: None,
            endian: None,
            check: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct UnframeConfig {
    /// How messages are marked on the wire. Must match the sender.
    pub mode: FrameMode,

    /// The terminator, in `delimiter` mode.
    pub delimiter: Option<Delimiter>,

    /// Width of the header, in `length` mode: 1, 2, 4 or 8.
    pub length_bytes: Option<usize>,

    /// Byte order of the header, in `length` mode.
    pub endian: Option<Endian>,

    /// Largest message to accept. `0` removes the limit, which hands a peer
    /// whose framing does not match an unbounded allocation.
    ///
    /// A counted mode rejects an oversized message from its header, before
    /// reading the payload at all. A terminator mode has to find out by
    /// running out of room.
    pub max_message: ByteSize,

    /// What to do with bytes left over when the stream ends part way through a
    /// message. Defaults to `emit` in `delimiter` mode and `error` elsewhere.
    pub at_eof: Option<AtEof>,
}

impl Default for UnframeConfig {
    fn default() -> Self {
        Self {
            mode: FrameMode::default(),
            delimiter: None,
            length_bytes: None,
            endian: None,
            max_message: ByteSize(DEFAULT_MAX_MESSAGE),
            at_eof: None,
        }
    }
}

/// Rejects an option the chosen mode ignores.
///
/// Accepting one silently is how a config comes to say something it does not
/// do: `mode = "cobs", delimiter = "\n"` reads like it sets the terminator,
/// and COBS would go on using a zero byte.
fn only_in(
    stage: &'static str,
    option: &str,
    set: bool,
    mode: FrameMode,
    wanted: FrameMode,
) -> Result<()> {
    if !set || mode == wanted {
        return Ok(());
    }

    let (mode, wanted) = (mode.name(), wanted.name());

    Err(PluginError::config(
        stage,
        format!("{option} means nothing in {mode} mode; it is an option of {wanted} mode"),
    ))
}

/// The width and byte order of a `length` header, checked.
fn length_format(
    stage: &'static str,
    bytes: Option<usize>,
    endian: Option<Endian>,
) -> Result<LengthFormat> {
    let bytes = bytes.unwrap_or(DEFAULT_LENGTH_BYTES);

    // Anything else is a header no other implementation would write.
    if !matches!(bytes, 1 | 2 | 4 | 8) {
        return Err(PluginError::config(
            stage,
            format!("length-bytes is {bytes}; it must be 1, 2, 4 or 8"),
        ));
    }

    Ok(LengthFormat {
        bytes,
        endian: endian.unwrap_or_default(),
    })
}

/// Where `needle` first appears in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    match needle {
        [byte] => haystack.iter().position(|b| b == byte),
        _ => haystack
            .windows(needle.len())
            .position(|window| window == needle),
    }
}

/// COBS-encode `input` onto the end of `out`, without the terminator.
///
/// Writes a placeholder for each block's code byte and fills it in once the
/// block's length is known, which is what lets this run in one pass.
fn cobs_encode(input: &[u8], out: &mut Vec<u8>) {
    let mut code_at = out.len();
    let mut code: u8 = 1;

    out.push(0);

    for &byte in input {
        if byte != 0 {
            out.push(byte);
            code += 1;

            if usize::from(code) <= COBS_BLOCK {
                continue;
            }
        }

        out[code_at] = code;
        code_at = out.len();
        code = 1;
        out.push(0);
    }

    out[code_at] = code;
}

/// COBS-decode one frame (with the terminator already stripped) onto the end
/// of `out`.
fn cobs_decode(frame: &[u8], out: &mut Vec<u8>) -> Result<()> {
    if frame.is_empty() {
        return Err(PluginError::runtime(
            UNFRAME,
            "empty cobs frame: two terminators in a row is not an empty message",
        ));
    }

    let mut rest = frame;

    while let Some((&code, tail)) = rest.split_first() {
        // Unreachable through `unframe`, which splits the stream on the zero
        // byte, but this decodes whatever it is handed and 0 would underflow.
        if code == 0 {
            return Err(PluginError::runtime(
                UNFRAME,
                "zero byte inside a cobs frame",
            ));
        }

        let len = usize::from(code) - 1;

        if len > tail.len() {
            return Err(PluginError::runtime(
                UNFRAME,
                "truncated cobs frame: a code byte runs past the end of the frame",
            ));
        }

        out.extend_from_slice(&tail[..len]);
        rest = &tail[len..];

        // A block shorter than the maximum stood for a zero byte in the
        // payload. One that hit the maximum was cut by length alone, and the
        // last block of a frame stands for nothing at all.
        if !rest.is_empty() && usize::from(code) <= COBS_BLOCK {
            out.push(0);
        }
    }

    Ok(())
}

/// SLIP-escape `input` onto the end of `out`, without the terminator.
fn slip_encode(input: &[u8], out: &mut Vec<u8>) {
    for &byte in input {
        match byte {
            SLIP_END => out.extend_from_slice(&[SLIP_ESC, SLIP_ESC_END]),
            SLIP_ESC => out.extend_from_slice(&[SLIP_ESC, SLIP_ESC_ESC]),
            other => out.push(other),
        }
    }
}

/// SLIP-decode one frame (with the terminator already stripped) onto the end
/// of `out`.
///
/// RFC 1055 leaves an escape followed by anything else undefined. Passing it
/// through would let a peer's bug reach the far side as payload, so it is an
/// error here.
fn slip_decode(frame: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let mut rest = frame;

    while let Some((&byte, tail)) = rest.split_first() {
        if byte != SLIP_ESC {
            out.push(byte);
            rest = tail;
            continue;
        }

        match tail.split_first() {
            Some((&SLIP_ESC_END, tail)) => {
                out.push(SLIP_END);
                rest = tail;
            }
            Some((&SLIP_ESC_ESC, tail)) => {
                out.push(SLIP_ESC);
                rest = tail;
            }
            Some((&other, _)) => {
                return Err(PluginError::runtime(
                    UNFRAME,
                    format!("{other:#04x} follows a slip escape; only 0xdc and 0xdd may"),
                ));
            }
            None => {
                return Err(PluginError::runtime(
                    UNFRAME,
                    "slip frame ends on an escape",
                ));
            }
        }
    }

    Ok(())
}

/// Where one message sits once `unframe` has found it.
#[derive(Debug, Clone, Copy)]
enum Payload {
    /// A slice of the pending buffer, forwarded without another copy.
    Borrowed(usize, usize),
    /// Rewritten into the stage's own buffer, because the wire form is
    /// escaped.
    Decoded,
}

/// What one pass over the pending bytes found.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Not a whole message yet.
    Incomplete,
    /// One message, and how many pending bytes it used, terminator included.
    Message { payload: Payload, consumed: usize },
}

pub struct Frame {
    mode: FrameMode,
    terminator: Vec<u8>,
    length: LengthFormat,
    check: bool,
    /// Scratch: the escaped frame, the length header, or the join that
    /// `ambiguous` scans. Reused across calls, so a steady stream settles on
    /// one allocation.
    out: Vec<u8>,
}

impl Frame {
    /// Whether appending the terminator to this message would put one
    /// anywhere but the end.
    ///
    /// The obvious half is a terminator inside the message. The other half
    /// only bites for a terminator that overlaps itself (`aa`, `abab`): a
    /// message ending in `a`, terminated by `aa`, puts `aaa` on the wire and
    /// the far end reads the boundary one byte early. The message contains no
    /// terminator at all, so scanning it alone would miss this.
    ///
    /// Only the join needs checking, and it is at most twice the terminator
    /// long, so this is a scan of the message plus a few bytes.
    fn ambiguous(&mut self, message: &[u8]) -> bool {
        if find(message, &self.terminator).is_some() {
            return true;
        }

        let overlap = self.terminator.len() - 1;

        if overlap == 0 {
            return false;
        }

        self.out.clear();
        self.out
            .extend_from_slice(&message[message.len().saturating_sub(overlap)..]);
        self.out.extend_from_slice(&self.terminator);

        find(&self.out, &self.terminator) != Some(self.out.len() - self.terminator.len())
    }
}

impl Plugin for Frame {
    fn name(&self) -> &str {
        FRAME
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        match self.mode {
            FrameMode::Delimiter => {
                if self.check && self.ambiguous(input) {
                    return Err(PluginError::runtime(FRAME, AMBIGUOUS_MESSAGE));
                }

                // Two appends rather than one buffer: the payload is copied
                // once either way, and this way there is nothing to grow.
                ctx.forward(input);
                ctx.forward(&self.terminator);
            }
            FrameMode::Cobs | FrameMode::Slip => {
                self.out.clear();

                if self.mode == FrameMode::Cobs {
                    cobs_encode(input, &mut self.out);
                } else {
                    slip_encode(input, &mut self.out);
                }

                self.out.extend_from_slice(&self.terminator);
                ctx.forward(&self.out);
            }
            FrameMode::Length => {
                let ceiling = self.length.ceiling();

                // The header cannot say how long this is, and a truncated
                // length is a stream the far end can never resynchronise.
                if input.len() as u64 > ceiling {
                    let (len, bytes) = (input.len(), self.length.bytes);

                    return Err(PluginError::runtime(
                        FRAME,
                        format!(
                            "message of {len} bytes does not fit a {bytes}-byte length header, \
                             which tops out at {ceiling}"
                        ),
                    ));
                }

                self.out.clear();
                self.length.encode(input.len(), &mut self.out);
                ctx.forward(&self.out);
                ctx.forward(input);
            }
            FrameMode::Netstring => {
                self.out.clear();
                self.out
                    .extend_from_slice(input.len().to_string().as_bytes());
                self.out.push(b':');
                ctx.forward(&self.out);
                ctx.forward(input);
                ctx.forward(b",");
            }
        }

        Ok(())
    }

    /// Safe on a datagram path: one message in, one message out. Framing a
    /// path that is already framed is redundant rather than wrong, and is
    /// what a datagram source feeding a stream sink wants.
    fn datagram_safe(&self) -> bool {
        true
    }
}

pub struct Unframe {
    mode: FrameMode,
    terminator: Vec<u8>,
    length: LengthFormat,
    max_message: usize,
    at_eof: AtEof,
    /// Bytes held back. Everything before `start` has already been emitted and
    /// is dropped at the end of the call, so a chunk holding many messages
    /// costs one move rather than one per message.
    buf: Vec<u8>,
    start: usize,
    /// How much of the pending bytes has already been searched, for the modes
    /// that search. A terminator can straddle a chunk boundary, so this stops
    /// just short of the end rather than at it.
    scanned: usize,
    /// Reused across calls, so a steady stream settles on one allocation.
    out: Vec<u8>,
}

impl Unframe {
    /// The bytes not yet accounted for.
    fn pending(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    /// Framing bytes that can be held alongside a message of the maximum
    /// size, so that `max-message` means the message rather than the frame.
    ///
    /// A terminator mode holds payload and at most a partial terminator: a
    /// whole one would have been found and the message emitted. A counted mode
    /// holds its header.
    fn slack(&self) -> usize {
        match self.mode {
            FrameMode::Length => self.length.bytes,
            FrameMode::Netstring => NETSTRING_DIGITS + 2,
            _ => self.terminator.len() - 1,
        }
    }

    /// Reject a length a header declared, before the payload is read. The
    /// whole point of a counted mode is that this is possible.
    fn check_declared(&self, declared: u64) -> Result<usize> {
        let max = self.max_message;
        let too_big = |len: u64| {
            let max = ByteSize(max);

            PluginError::runtime(
                UNFRAME,
                format!("a header declares {len} bytes, over the max-message of {max}"),
            )
        };

        let len = usize::try_from(declared).map_err(|_| too_big(declared))?;

        if max > 0 && len > max {
            return Err(too_big(declared));
        }

        Ok(len)
    }

    /// Reject a stream that has held more than a message's worth without
    /// completing one.
    ///
    /// The limit is on what is held, not on what passes: a stream of small
    /// messages never approaches it however long it runs.
    fn check_held(&self) -> Result<()> {
        let held = self.pending().len();

        if self.max_message > 0 && held > self.max_message + self.slack() {
            let max = ByteSize(self.max_message);

            return Err(PluginError::runtime(
                UNFRAME,
                format!(
                    "no complete message in {held} bytes (max-message is {max}): \
                     this stage's framing does not match the peer's"
                ),
            ));
        }

        Ok(())
    }

    /// Find the next whole message, decoding it into `out` if the mode
    /// escapes.
    fn step(&mut self) -> Result<Step> {
        match self.mode {
            FrameMode::Length => self.step_length(),
            FrameMode::Netstring => self.step_netstring(),
            _ => self.step_terminated(),
        }
    }

    fn step_terminated(&mut self) -> Result<Step> {
        let pending = &self.buf[self.start..];

        let Some(offset) = find(&pending[self.scanned..], &self.terminator) else {
            // A terminator can straddle the join with the next chunk, so the
            // last few bytes stay unread rather than being searched twice.
            self.scanned = pending.len().saturating_sub(self.terminator.len() - 1);

            return Ok(Step::Incomplete);
        };

        let end = self.scanned + offset;
        let consumed = end + self.terminator.len();

        self.scanned = 0;

        if !self.mode.escapes() {
            return Ok(Step::Message {
                payload: Payload::Borrowed(0, end),
                consumed,
            });
        }

        self.out.clear();

        if self.mode == FrameMode::Cobs {
            cobs_decode(&pending[..end], &mut self.out)?;
        } else {
            // An empty slip frame decodes to nothing and emits nothing, which
            // is what RFC 1055's leading terminator is for: senders use it to
            // flush line noise, and it is not a message.
            slip_decode(&pending[..end], &mut self.out)?;
        }

        Ok(Step::Message {
            payload: Payload::Decoded,
            consumed,
        })
    }

    fn step_length(&mut self) -> Result<Step> {
        let header = self.length.bytes;
        let pending = &self.buf[self.start..];

        if pending.len() < header {
            return Ok(Step::Incomplete);
        }

        let declared = self.length.decode(&pending[..header]);
        let len = self.check_declared(declared)?;
        let end = header + len;

        if pending.len() < end {
            return Ok(Step::Incomplete);
        }

        Ok(Step::Message {
            payload: Payload::Borrowed(header, end),
            consumed: end,
        })
    }

    fn step_netstring(&mut self) -> Result<Step> {
        let pending = &self.buf[self.start..];
        let searched = pending.len().min(NETSTRING_DIGITS + 1);

        let Some(colon) = pending[..searched].iter().position(|&byte| byte == b':') else {
            if pending.len() > NETSTRING_DIGITS {
                return Err(PluginError::runtime(
                    UNFRAME,
                    format!("no colon in the first {NETSTRING_DIGITS} bytes of a netstring"),
                ));
            }

            return Ok(Step::Incomplete);
        };

        let digits = &pending[..colon];
        let declared = parse_netstring_length(digits)?;
        let len = self.check_declared(declared)?;

        let end = colon + 1 + len;

        if pending.len() <= end {
            return Ok(Step::Incomplete);
        }

        // The comma is what makes a desynchronised stream announce itself on
        // the next message rather than never.
        if pending[end] != b',' {
            return Err(PluginError::runtime(
                UNFRAME,
                "netstring payload is not followed by a comma",
            ));
        }

        Ok(Step::Message {
            payload: Payload::Borrowed(colon + 1, end),
            consumed: end + 1,
        })
    }
}

/// The decimal length of a netstring, which is canonical: no sign, no spaces,
/// and no leading zero to pad a header out with.
fn parse_netstring_length(digits: &[u8]) -> Result<u64> {
    let invalid = digits.is_empty()
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits.len() > 1 && digits[0] == b'0');

    if invalid {
        return Err(PluginError::runtime(
            UNFRAME,
            "netstring length is not a canonical decimal number",
        ));
    }

    digits
        .iter()
        .copied()
        .try_fold(0u64, |acc, byte| {
            acc.checked_mul(10)?.checked_add(u64::from(byte - b'0'))
        })
        .ok_or_else(|| PluginError::runtime(UNFRAME, "netstring length overflows a u64"))
}

impl Plugin for Unframe {
    fn name(&self) -> &str {
        UNFRAME
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(input);

        loop {
            match self.step()? {
                Step::Incomplete => break,
                Step::Message { payload, consumed } => {
                    match payload {
                        Payload::Borrowed(from, to) => {
                            ctx.forward(&self.buf[self.start + from..self.start + to]);
                        }
                        Payload::Decoded => ctx.forward(&self.out),
                    }

                    // A message is a unit, which is what makes every stage
                    // below this one see one call per message.
                    ctx.boundary();
                    self.start += consumed;
                }
            }
        }

        self.buf.drain(..self.start);
        self.start = 0;

        self.check_held()
    }

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }

        match self.at_eof {
            AtEof::Emit => {
                // Only the modes whose leftover bytes are a payload allow
                // this, so there is no header to strip here.
                if self.mode.escapes() {
                    self.out.clear();

                    if self.mode == FrameMode::Cobs {
                        cobs_decode(&self.buf, &mut self.out)?;
                    } else {
                        slip_decode(&self.buf, &mut self.out)?;
                    }

                    ctx.forward(&self.out);
                } else {
                    ctx.forward(&self.buf);
                }

                ctx.boundary();
                self.buf.clear();
            }
            AtEof::Drop => self.buf.clear(),
            AtEof::Error => {
                let held = self.buf.len();

                return Err(PluginError::runtime(
                    UNFRAME,
                    format!(
                        "stream ended with {held} bytes and no complete message: \
                         set at-eof to emit or drop to accept that"
                    ),
                ));
            }
        }

        self.scanned = 0;

        Ok(())
    }

    // `datagram_safe` is left at its default of false, deliberately: this
    // stage holds bytes across calls, and the boundaries it emits are the
    // sender's framing rather than the datagrams the peer sent. On a path that
    // already has messages there is nothing for it to do.
}

pub struct FrameFactory;

impl PluginFactory for FrameFactory {
    fn name(&self) -> &str {
        FRAME
    }

    fn description(&self) -> &str {
        "Mark message boundaries on this direction"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: FrameConfig = ctx.config()?;
        let mode = config.mode;

        only_in(
            FRAME,
            "delimiter",
            config.delimiter.is_some(),
            mode,
            FrameMode::Delimiter,
        )?;
        only_in(
            FRAME,
            "check",
            config.check.is_some(),
            mode,
            FrameMode::Delimiter,
        )?;
        only_in(
            FRAME,
            "length-bytes",
            config.length_bytes.is_some(),
            mode,
            FrameMode::Length,
        )?;
        only_in(
            FRAME,
            "endian",
            config.endian.is_some(),
            mode,
            FrameMode::Length,
        )?;

        Ok(Stage::filter(Frame {
            mode,
            terminator: mode.terminator(config.delimiter.as_ref()),
            length: length_format(FRAME, config.length_bytes, config.endian)?,
            check: config.check.unwrap_or(true),
            out: Vec::new(),
        }))
    }
}

pub struct UnframeFactory;

impl PluginFactory for UnframeFactory {
    fn name(&self) -> &str {
        UNFRAME
    }

    fn description(&self) -> &str {
        "Split this direction into messages on their boundaries"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: UnframeConfig = ctx.config()?;
        let mode = config.mode;

        only_in(
            UNFRAME,
            "delimiter",
            config.delimiter.is_some(),
            mode,
            FrameMode::Delimiter,
        )?;
        only_in(
            UNFRAME,
            "length-bytes",
            config.length_bytes.is_some(),
            mode,
            FrameMode::Length,
        )?;
        only_in(
            UNFRAME,
            "endian",
            config.endian.is_some(),
            mode,
            FrameMode::Length,
        )?;

        // The leftovers of a counted frame start with a header, so there is no
        // message in them to emit. The other modes leave a payload behind.
        if config.at_eof == Some(AtEof::Emit) && mode.counted() {
            return Err(PluginError::config(
                UNFRAME,
                format!(
                    "at-eof = \"emit\" has nothing to emit in {} mode, where a partial \
                     message is a header and part of a payload",
                    mode.name()
                ),
            ));
        }

        Ok(Stage::filter(Unframe {
            mode,
            terminator: mode.terminator(config.delimiter.as_ref()),
            length: length_format(UNFRAME, config.length_bytes, config.endian)?,
            max_message: config.max_message.bytes(),
            at_eof: config.at_eof.unwrap_or_else(|| AtEof::default_for(mode)),
            buf: Vec::new(),
            start: 0,
            scanned: 0,
            out: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};
    use tocat_api::{
        ChannelId, ChannelTarget, Direction, EffectSink, Emission, HostBuilder, LogLevel,
        PipelineMeta, StageInfo,
    };

    use super::*;

    /// Every mode, for the tests that should hold across all of them.
    const MODES: [&str; 5] = ["delimiter", "cobs", "slip", "length", "netstring"];

    struct NoHost;

    impl HostBuilder for NoHost {
        fn open_channel(&mut self, _target: ChannelTarget) -> Result<ChannelId> {
            unreachable!("frame opens no side channels")
        }
    }

    struct Silent;

    impl EffectSink for Silent {
        fn write(&mut self, _channel: ChannelId, _bytes: &[u8]) {}

        fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "src", "sink")
    }

    fn build(factory: &dyn PluginFactory, value: Value) -> Result<Box<dyn Plugin>> {
        let map: Map<String, Value> = match value {
            Value::Object(map) => map,
            other => unreachable!("config must be an object, got {other}"),
        };
        let meta = meta();
        let mut host = NoHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: factory.name(),
            upstream: "src",
            downstream: "sink",
        };
        let mut ctx = BuildCtx::new(factory.name(), &map, &meta, stage, &mut host);

        match factory.build(&mut ctx)? {
            Stage::Filter(plugin) => Ok(plugin),
            Stage::External(_) => unreachable!("frame stages are filters"),
        }
    }

    fn framer(value: Value) -> Box<dyn Plugin> {
        build(&FrameFactory, value).expect("build")
    }

    fn unframer(value: Value) -> Box<dyn Plugin> {
        build(&UnframeFactory, value).expect("build")
    }

    /// The units one call emitted, split the way the pipeline would split
    /// them. Asserting on the concatenation would pass just as happily if this
    /// stage fused every message into one, which is the bug the boundaries
    /// exist to prevent.
    fn units(emission: &Emission) -> Vec<Vec<u8>> {
        let bytes = emission.bytes();
        let mut start = 0;

        emission
            .bounds()
            .iter()
            .map(|&end| {
                let unit = bytes[start..end].to_vec();
                start = end;
                unit
            })
            .chain((emission.bounds().is_empty() && !bytes.is_empty()).then(|| bytes.to_vec()))
            .collect()
    }

    fn try_feed(plugin: &mut dyn Plugin, input: &[u8]) -> Result<Vec<Vec<u8>>> {
        let meta = meta();
        let mut emission = Emission::new();
        let mut sink = Silent;
        let name = plugin.name().to_owned();

        {
            let mut ctx = Ctx::new(&meta, &name, input, &mut emission, &mut sink);
            plugin.on_bytes(&mut ctx, input)?;
        }

        Ok(units(&emission))
    }

    fn try_finish(plugin: &mut dyn Plugin) -> Result<Vec<Vec<u8>>> {
        let meta = meta();
        let mut emission = Emission::new();
        let mut sink = Silent;
        let name = plugin.name().to_owned();

        {
            let mut ctx = Ctx::new(&meta, &name, &[], &mut emission, &mut sink);
            plugin.on_eof(&mut ctx)?;
        }

        Ok(units(&emission))
    }

    fn feed(plugin: &mut dyn Plugin, input: &[u8]) -> Vec<Vec<u8>> {
        try_feed(plugin, input).expect("on_bytes")
    }

    fn finish(plugin: &mut dyn Plugin) -> Vec<Vec<u8>> {
        try_finish(plugin).expect("on_eof")
    }

    /// Everything `frame` put on the wire for these messages.
    fn wire(config: Value, messages: &[&[u8]]) -> Vec<u8> {
        let mut frame = framer(config);
        let mut out = Vec::new();

        for message in messages {
            for unit in feed(frame.as_mut(), message) {
                out.extend_from_slice(&unit);
            }
        }

        out
    }

    #[test]
    fn a_message_is_terminated_by_the_delimiter() {
        assert_eq!(wire(json!({}), &[b"one", b"two"]), b"one\ntwo\n");
    }

    #[test]
    fn the_delimiter_is_configurable() {
        assert_eq!(wire(json!({"delimiter": "\\r\\n"}), &[b"one"]), b"one\r\n");
        assert_eq!(wire(json!({"delimiter": "\\x1e"}), &[b"one"]), b"one\x1e");
        assert_eq!(wire(json!({"delimiter": "END"}), &[b"one"]), b"oneEND");
    }

    /// Every mode has to survive boundaries that fall anywhere, since the
    /// network picks them and not the sender.
    #[test]
    fn every_mode_round_trips_one_byte_at_a_time() {
        let messages: [&[u8]; 5] = [b"one", &[0, 1, 2, 0, 0, 3], &[0xff; 600], &[], b"last"];

        for mode in MODES {
            // Only the counted and escaped modes can carry arbitrary bytes.
            let messages: Vec<&[u8]> = match mode {
                "delimiter" => vec![b"one", b"two", b"", b"last"],
                _ => messages.to_vec(),
            };

            let wire = wire(json!({"mode": mode}), &messages);
            let mut unframe = unframer(json!({"mode": mode}));
            let mut seen = Vec::new();

            for byte in &wire {
                seen.extend(feed(unframe.as_mut(), &[*byte]));
            }
            seen.extend(finish(unframe.as_mut()));

            // An empty message carries no bytes, so the pipeline has no unit
            // to deliver it in and it is dropped rather than emitted.
            let expected: Vec<Vec<u8>> = messages
                .iter()
                .filter(|message| !message.is_empty())
                .map(|message| message.to_vec())
                .collect();

            assert_eq!(seen, expected, "mode {mode}");
        }
    }

    #[test]
    fn every_mode_splits_a_chunk_of_several_messages() {
        for mode in MODES {
            let wire = wire(json!({"mode": mode}), &[b"one", b"two", b"three"]);
            let mut unframe = unframer(json!({"mode": mode}));

            assert_eq!(
                feed(unframe.as_mut(), &wire),
                [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()],
                "mode {mode}",
            );
        }
    }

    /// The case a naive scan gets wrong: the delimiter itself lands across the
    /// join and is never found.
    #[test]
    fn a_multi_byte_delimiter_may_straddle_a_chunk_boundary() {
        let mut unframe = unframer(json!({"delimiter": "\\r\\n"}));

        assert!(feed(unframe.as_mut(), b"one\r").is_empty());
        assert_eq!(
            feed(unframe.as_mut(), b"\ntwo\r\n"),
            [b"one".to_vec(), b"two".to_vec()],
        );
    }

    /// A payload containing the delimiter arrives as two messages, which is a
    /// silent corruption unless someone looks for it.
    #[test]
    fn framing_a_message_containing_the_delimiter_is_rejected() {
        let mut frame = framer(json!({}));

        assert!(try_feed(frame.as_mut(), b"two\nlines").is_err());
        assert!(try_feed(frame.as_mut(), b"one line").is_ok());
    }

    /// A delimiter that overlaps itself makes "the message does not contain
    /// the delimiter" insufficient: `a` terminated by `aa` is `aaa`, which the
    /// far end splits one byte early.
    #[test]
    fn framing_a_message_that_ends_in_a_prefix_of_the_delimiter_is_rejected() {
        let mut frame = framer(json!({"delimiter": "aa"}));

        assert!(try_feed(frame.as_mut(), b"a").is_err());
        assert!(try_feed(frame.as_mut(), b"ba").is_err());
        assert!(try_feed(frame.as_mut(), b"ab").is_ok());
    }

    #[test]
    fn the_check_can_be_turned_off() {
        assert_eq!(
            wire(json!({"check": false}), &[b"two\nlines"]),
            b"two\nlines\n"
        );
    }

    /// The property the encoding exists for.
    #[test]
    fn a_cobs_frame_contains_no_zero_byte() {
        let payload: Vec<u8> = (0..=255u8).chain(0..=255u8).collect();
        let wire = wire(json!({"mode": "cobs"}), &[&payload]);

        assert_eq!(
            wire.iter().filter(|&&byte| byte == 0).count(),
            1,
            "only the terminator",
        );
        assert_eq!(wire.last(), Some(&0));
    }

    /// The boundary the encoding turns on: a block holds 254 bytes, so a run
    /// of exactly that many, one fewer and one more all take different paths.
    #[test]
    fn cobs_round_trips_at_the_block_boundary() {
        for len in [0usize, 1, 253, 254, 255, 508, 509] {
            let payload = vec![0xabu8; len];
            let mut decoded = Vec::new();
            let mut encoded = Vec::new();

            cobs_encode(&payload, &mut encoded);
            assert!(!encoded.contains(&0), "len {len} stuffed a zero byte");
            cobs_decode(&encoded, &mut decoded).expect("decode");

            assert_eq!(decoded, payload, "len {len}");
        }
    }

    #[test]
    fn cobs_round_trips_zero_bytes() {
        for payload in [
            vec![0u8],
            vec![0u8, 0],
            vec![1u8, 0, 2],
            vec![0u8; 300],
            [vec![0u8], vec![7u8; 254], vec![0u8]].concat(),
        ] {
            let mut encoded = Vec::new();
            let mut decoded = Vec::new();

            cobs_encode(&payload, &mut encoded);
            cobs_decode(&encoded, &mut decoded).expect("decode");

            assert_eq!(decoded, payload);
        }
    }

    #[test]
    fn a_corrupt_cobs_frame_is_rejected() {
        let mut unframe = unframer(json!({"mode": "cobs"}));

        // A code byte that points past the end of its own frame.
        assert!(try_feed(unframe.as_mut(), &[9u8, 1, 2, 0]).is_err());
    }

    #[test]
    fn two_cobs_terminators_in_a_row_are_rejected() {
        let mut unframe = unframer(json!({"mode": "cobs"}));

        assert!(try_feed(unframe.as_mut(), &[1u8, 0, 0]).is_err());
    }

    #[test]
    fn slip_escapes_its_own_two_bytes_and_nothing_else() {
        let wire = wire(
            json!({"mode": "slip"}),
            &[&[0x01, SLIP_END, 0x02, SLIP_ESC]],
        );

        assert_eq!(
            wire,
            [
                0x01,
                SLIP_ESC,
                SLIP_ESC_END,
                0x02,
                SLIP_ESC,
                SLIP_ESC_ESC,
                SLIP_END,
            ],
        );
    }

    /// RFC 1055 has senders lead with a terminator to flush line noise, so an
    /// empty frame is a normal thing to receive and not a message.
    #[test]
    fn slip_ignores_empty_frames() {
        let mut unframe = unframer(json!({"mode": "slip"}));

        assert_eq!(
            feed(
                unframe.as_mut(),
                &[SLIP_END, SLIP_END, b'h', b'i', SLIP_END]
            ),
            [b"hi".to_vec()],
        );
    }

    #[test]
    fn a_bad_slip_escape_is_rejected() {
        let mut unframe = unframer(json!({"mode": "slip"}));

        assert!(try_feed(unframe.as_mut(), &[SLIP_ESC, 0x99, SLIP_END]).is_err());

        let mut unframe = unframer(json!({"mode": "slip"}));

        assert!(try_feed(unframe.as_mut(), &[b'a', SLIP_ESC, SLIP_END]).is_err());
    }

    #[test]
    fn a_length_header_is_big_endian_and_four_bytes_by_default() {
        assert_eq!(wire(json!({"mode": "length"}), &[b"hi"]), b"\0\0\0\x02hi");
    }

    #[test]
    fn the_length_header_width_and_order_are_configurable() {
        assert_eq!(
            wire(json!({"mode": "length", "length-bytes": 2}), &[b"hi"]),
            b"\0\x02hi",
        );
        assert_eq!(
            wire(
                json!({"mode": "length", "length-bytes": 2, "endian": "little"}),
                &[b"hi"],
            ),
            b"\x02\0hi",
        );
        assert_eq!(
            wire(json!({"mode": "length", "length-bytes": 1}), &[b"hi"]),
            b"\x02hi",
        );
    }

    #[test]
    fn a_message_too_big_for_its_length_header_is_rejected() {
        let mut frame = framer(json!({"mode": "length", "length-bytes": 1}));

        assert!(try_feed(frame.as_mut(), &[0u8; 255]).is_ok());
        assert!(try_feed(frame.as_mut(), &[0u8; 256]).is_err());
    }

    /// What a counted mode buys: an oversized message is refused from its
    /// header, before a byte of the payload has been buffered.
    #[test]
    fn a_counted_mode_refuses_an_oversized_message_from_its_header() {
        let mut unframe = unframer(json!({"mode": "length", "max-message": 8}));

        assert!(try_feed(unframe.as_mut(), b"\0\0\x04\0").is_err());

        let mut unframe = unframer(json!({"mode": "netstring", "max-message": 8}));

        assert!(try_feed(unframe.as_mut(), b"1024:").is_err());
    }

    #[test]
    fn a_netstring_is_length_colon_payload_comma() {
        assert_eq!(
            wire(json!({"mode": "netstring"}), &[b"hello", b""]),
            b"5:hello,0:,",
        );
    }

    #[test]
    fn a_netstring_length_must_be_canonical() {
        for header in [&b"05:hello,"[..], b"+5:hello,", b" 5:hello,", b":hello,"] {
            let mut unframe = unframer(json!({"mode": "netstring"}));

            assert!(
                try_feed(unframe.as_mut(), header).is_err(),
                "accepted {header:?}",
            );
        }
    }

    /// The comma is what makes a desynchronised netstring stream announce
    /// itself on the next message rather than never.
    #[test]
    fn a_netstring_without_its_comma_is_rejected() {
        let mut unframe = unframer(json!({"mode": "netstring"}));

        assert!(try_feed(unframe.as_mut(), b"5:hello!").is_err());
    }

    #[test]
    fn a_netstring_header_that_never_ends_is_rejected() {
        let mut unframe = unframer(json!({"mode": "netstring"}));

        assert!(try_feed(unframe.as_mut(), &[b'1'; 64]).is_err());
    }

    #[test]
    fn a_trailing_partial_message_is_emitted_by_default() {
        let mut unframe = unframer(json!({}));

        assert_eq!(feed(unframe.as_mut(), b"one\ntwo"), [b"one".to_vec()]);
        assert_eq!(finish(unframe.as_mut()), [b"two".to_vec()]);
    }

    #[test]
    fn a_trailing_partial_message_can_be_refused_or_dropped() {
        let mut erroring = unframer(json!({"at-eof": "error"}));
        feed(erroring.as_mut(), b"one\ntwo");
        assert!(try_finish(erroring.as_mut()).is_err());

        let mut dropping = unframer(json!({"at-eof": "drop"}));
        feed(dropping.as_mut(), b"one\ntwo");
        assert!(finish(dropping.as_mut()).is_empty());
    }

    /// Only `delimiter` mode leaves a payload behind. Everywhere else a
    /// partial frame was cut off in transit.
    #[test]
    fn every_other_mode_refuses_a_trailing_partial_frame() {
        for mode in MODES.into_iter().filter(|mode| *mode != "delimiter") {
            let mut unframe = unframer(json!({"mode": mode}));

            feed(unframe.as_mut(), &[3u8, 1, 2]);
            assert!(try_finish(unframe.as_mut()).is_err(), "mode {mode}");
        }
    }

    /// A counted mode's leftovers start with a header, so there is no message
    /// in them to emit and the option is refused rather than guessing.
    #[test]
    fn emitting_a_partial_frame_is_not_offered_by_the_counted_modes() {
        assert!(build(&UnframeFactory, json!({"mode": "length", "at-eof": "emit"})).is_err());
        assert!(
            build(
                &UnframeFactory,
                json!({"mode": "netstring", "at-eof": "emit"})
            )
            .is_err()
        );
        assert!(build(&UnframeFactory, json!({"mode": "cobs", "at-eof": "emit"})).is_ok());
    }

    #[test]
    fn a_clean_end_of_stream_emits_nothing() {
        for mode in MODES {
            let wire = wire(json!({"mode": mode}), &[b"one"]);
            let mut unframe = unframer(json!({"mode": mode}));

            assert_eq!(
                feed(unframe.as_mut(), &wire),
                [b"one".to_vec()],
                "mode {mode}"
            );
            assert!(finish(unframe.as_mut()).is_empty(), "mode {mode}");
        }
    }

    /// A peer that never completes a message must not be able to make the
    /// relay buffer without limit.
    #[test]
    fn a_message_larger_than_the_limit_is_refused() {
        let mut unframe = unframer(json!({"max-message": 8}));

        assert!(try_feed(unframe.as_mut(), b"12345678").is_ok());
        assert!(try_feed(unframe.as_mut(), b"9").is_err());
    }

    /// The limit is on one message, not on the stream.
    #[test]
    fn the_limit_resets_with_each_message() {
        let mut unframe = unframer(json!({"max-message": 8}));

        for _ in 0..100 {
            assert_eq!(feed(unframe.as_mut(), b"12345\n"), [b"12345".to_vec()]);
        }
    }

    #[test]
    fn the_limit_can_be_removed() {
        let mut unframe = unframer(json!({"max-message": 0}));

        assert!(try_feed(unframe.as_mut(), &vec![b'x'; 4 * DEFAULT_MAX_MESSAGE]).is_ok());
    }

    /// The held bytes include part of a terminator that has not arrived yet,
    /// so a multi-byte one must not eat into the message's allowance.
    #[test]
    fn a_partial_terminator_does_not_count_against_the_limit() {
        let mut unframe = unframer(json!({"delimiter": "END", "max-message": 4}));

        assert_eq!(feed(unframe.as_mut(), b"abcdEN"), Vec::<Vec<u8>>::new());
        assert_eq!(feed(unframe.as_mut(), b"D"), [b"abcd".to_vec()]);
    }

    #[test]
    fn an_empty_delimiter_is_rejected() {
        assert!(build(&FrameFactory, json!({"delimiter": ""})).is_err());
        assert!(build(&UnframeFactory, json!({"delimiter": ""})).is_err());
    }

    #[test]
    fn nonsense_escapes_are_rejected() {
        assert!(build(&FrameFactory, json!({"delimiter": "\\q"})).is_err());
        assert!(build(&FrameFactory, json!({"delimiter": "\\xzz"})).is_err());
        assert!(build(&FrameFactory, json!({"delimiter": "\\"})).is_err());
    }

    #[test]
    fn a_delimiter_round_trips_through_its_own_escaping() {
        for raw in ["\\n", "\\r\\n", "\\0", "\\x1e", "END", "\\\\"] {
            let parsed: Delimiter = raw.parse().expect("parse");
            assert_eq!(parsed.to_string(), raw);
        }
    }

    /// An option a mode ignores is a config that says something it does not
    /// do, so it is an error rather than a no-op.
    #[test]
    fn an_option_of_another_mode_is_rejected() {
        for factory in [&FrameFactory as &dyn PluginFactory, &UnframeFactory] {
            assert!(build(factory, json!({"mode": "cobs", "delimiter": "\\0"})).is_err());
            assert!(build(factory, json!({"mode": "netstring", "delimiter": ","})).is_err());
            assert!(build(factory, json!({"length-bytes": 4})).is_err());
            assert!(build(factory, json!({"mode": "slip", "endian": "big"})).is_err());
            assert!(build(factory, json!({"mode": "length", "length-bytes": 4})).is_ok());
        }

        assert!(build(&FrameFactory, json!({"mode": "cobs", "check": false})).is_err());
    }

    #[test]
    fn an_odd_length_header_width_is_rejected() {
        for bytes in [0, 3, 5, 9] {
            assert!(
                build(
                    &FrameFactory,
                    json!({"mode": "length", "length-bytes": bytes})
                )
                .is_err(),
                "accepted {bytes}",
            );
        }
    }

    #[test]
    fn unknown_options_are_rejected() {
        assert!(build(&FrameFactory, json!({"size": 4})).is_err());
        assert!(build(&UnframeFactory, json!({"size": 4})).is_err());
    }

    /// `frame` preserves the message stream it is given; `unframe` replaces it
    /// with the sender's framing, which is not the peer's datagrams.
    #[test]
    fn only_frame_is_datagram_safe() {
        assert!(framer(json!({})).datagram_safe());
        assert!(!unframer(json!({})).datagram_safe());
    }
}
