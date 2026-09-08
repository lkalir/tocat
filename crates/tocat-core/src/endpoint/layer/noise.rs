//! noise.rs: the Noise Protocol Framework over a byte stream, as a layer.
//!
//! The same shape as [`Tls`]: a handshake this cannot be a stage, because a
//! stage is one direction with no way to answer the peer, and a handshake is an
//! exchange. What comes back is another [`EndpointStream::Duplex`], so nothing
//! in `pump` knows this is here.
//!
//! [`Tls`]: super::Tls
//!
//! # Why this exists next to the `encrypt` plugin
//!
//! They are not alternatives. `encrypt` transforms bytes under a key the user
//! already holds and needs no peer at all, which is what makes `file:` to
//! `file:` and encryption into a pipe possible. This needs a live peer speaking
//! Noise, and in exchange derives a fresh key per connection with forward
//! secrecy, and can authenticate that peer. Neither can do the other's job, and
//! the guide says so on both pages.
//!
//! # Naming
//!
//! A Noise protocol name is `Noise_PATTERN_DH_CIPHER_HASH`, and this exposes
//! three of the four. The pattern and its optional psk modifier are one option,
//! because the modifier is part of the pattern's name in the specification and
//! its legal placements depend on how many messages the pattern has. Cipher and
//! hash are independent options, because they are independent choices.
//!
//! DH is fixed at 25519. snow's parser accepts 448, but its default resolver
//! does not implement it, so offering it would turn a name that parses into a
//! failure at connect time.
//!
//! # Boundaries
//!
//! `Fuse`, like TLS, and for a subtler reason. Noise messages *are* discrete,
//! so it is tempting to report `Preserve` the way [`Ws`] does. But `Ws` gets
//! whole messages handed to it, whereas [`AsyncWrite`] here chops whatever the
//! caller passes at [`MAX_PLAINTEXT`]. A record is therefore an artefact of
//! buffer sizes, not an application message, and claiming otherwise is the lie
//! that turns a datagram relay into a stream one with no error anywhere.
//!
//! [`Ws`]: super::Ws
//!
//! # Ordering
//!
//! [`TransportState`] holds the nonce and both peers step it in lockstep, so
//! this needs a reliable, ordered transport: one lost or reordered record and
//! everything after it fails to authenticate. Datagrams would need snow's
//! `StatelessTransportState`, a nonce on the wire and a replay window, so
//! [`Noise::check`] refuses them rather than leaving it to fail at runtime.
//!
//! # Truncation, and why there is no `Closing` here
//!
//! TLS needs its `Closing` adapter because rustls reports a missing
//! `close_notify` as an error and a relay cannot know how long the response was
//! meant to be. Noise has no close notification at all, so end of stream on a
//! record boundary is simply end of stream and is reported as such. A cut in
//! the middle of a record is different: that is unambiguously a truncated
//! record rather than a peer hanging up, and it is an error.

use std::{
    cmp::min,
    io,
    num::NonZeroUsize,
    pin::Pin,
    task::{Context, Poll, ready},
};

use anyhow::{Context as _, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use snow::{Builder, HandshakeState, TransportState, params::NoiseParams};
use tocat_api::normalize;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tracing::warn;

use crate::endpoint::{
    EndpointStream,
    parse::{Opt, ParseEndpointError},
    stream::AsyncStream,
};

/// Largest Noise message, from the specification.
const MAX_MESSAGE: usize = 65535;
/// AEAD tag length for ChaChaPoly.
const TAG_LEN: usize = 16;
/// Largest plaintext that still leaves room for the tag in one message.
const MAX_PLAINTEXT: usize = MAX_MESSAGE - TAG_LEN;
/// Width of the length prefix in front of each message.
const LEN_PREFIX: usize = 2;
/// X25519 keys and the PSK are all this long.
const KEY_LEN: usize = 32;

type NoiseKey = [u8; 32];

/// One side's static key handling: a letter of a Noise pattern name.
///
/// The letters are the whole of the key requirement rules, which is why they
/// are kept as letters here rather than being flattened into twelve variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Static {
    /// No static key at all.
    N,
    /// A static key the peer already knows.
    K,
    /// A static key sent during the handshake, encrypted.
    X,
    /// A static key sent in the first message. Initiator only.
    I,
}

impl Static {
    const fn initiator(letter: char) -> Option<Self> {
        match letter {
            'n' => Some(Static::N),
            'k' => Some(Static::K),
            'x' => Some(Static::X),
            'i' => Some(Static::I),
            _ => None,
        }
    }

    /// The responder has no `I`: there is no message before the first for it to
    /// send a key in.
    const fn responder(letter: char) -> Option<Self> {
        match letter {
            'n' => Some(Static::N),
            'k' => Some(Static::K),
            'x' => Some(Static::X),
            _ => None,
        }
    }

    const fn letter(self) -> char {
        match self {
            Static::N => 'N',
            Static::K => 'K',
            Static::X => 'X',
            Static::I => 'I',
        }
    }

    /// Whether the side holding this letter presents a static key of its own.
    const fn has_key(self) -> bool {
        !matches!(self, Static::N)
    }

    /// Whether the *other* side must already hold this key before the handshake
    /// starts, rather than receiving it during one.
    const fn known_in_advance(self) -> bool {
        matches!(self, Static::K)
    }

    /// Whether this key reaches the other side during the handshake, which is
    /// what makes pinning it worth doing.
    const fn transmitted(self) -> bool {
        matches!(self, Static::X | Static::I)
    }
}

/// A handshake pattern and its optional pre-shared key placement.
///
/// Written as the specification writes it, so `xx`, `nnpsk0` and `xxpsk3` are
/// all names a user can copy out of the Noise documentation unchanged.
///
/// Keep in step with the `pattern` constraint in `tocat.schema.json` and the
/// table in `docs/src/guide/endpoints/noise.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct Handshake {
    initiator: Static,
    responder: Static,
    psk: Option<u8>,
}

impl Handshake {
    fn parse(text: &str) -> Option<Self> {
        let text = text.to_ascii_lowercase();

        let (letters, psk) = match text.split_once("psk") {
            Some((letters, index)) => {
                let index: u8 = index.parse().ok()?;

                if index > 3 {
                    return None;
                }

                (letters, Some(index))
            }
            None => (text.as_str(), None),
        };

        let mut letters = letters.chars();
        let initiator = Static::initiator(letters.next()?)?;
        let responder = Static::responder(letters.next()?)?;

        if letters.next().is_some() {
            return None;
        }

        Some(Handshake {
            initiator,
            responder,
            psk,
        })
    }

    /// The name as the specification spells it, for protocol names and errors.
    fn name(self) -> String {
        let mut name = String::with_capacity(8);
        name.push(self.initiator.letter());
        name.push(self.responder.letter());

        if let Some(index) = self.psk {
            name.push_str("psk");
            name.push(char::from(b'0' + index));
        }

        name
    }

    /// How many messages the handshake takes, which bounds where a psk can go.
    ///
    /// Three exactly when the initiator's static is an `X`: it cannot be sent
    /// until the responder's ephemeral has arrived to encrypt it with, so it
    /// needs a message of its own after the reply.
    const fn messages(self) -> u8 {
        if matches!(self.initiator, Static::X) {
            3
        } else {
            2
        }
    }

    /// This side's own letter.
    const fn mine(self, listening: bool) -> Static {
        if listening {
            self.responder
        } else {
            self.initiator
        }
    }

    /// The other side's letter.
    const fn theirs(self, listening: bool) -> Static {
        if listening {
            self.initiator
        } else {
            self.responder
        }
    }
}

impl TryFrom<String> for Handshake {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Handshake::parse(&text).ok_or_else(|| format!("not a Noise handshake pattern: {text}"))
    }
}

impl From<Handshake> for String {
    fn from(handshake: Handshake) -> Self {
        handshake.name()
    }
}

/// The AEAD, the `CIPHER` of a Noise protocol name.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cipher {
    #[default]
    ChaChaPoly,
    AesGcm,
}

impl Cipher {
    const fn name(self) -> &'static str {
        match self {
            Cipher::ChaChaPoly => "ChaChaPoly",
            Cipher::AesGcm => "AESGCM",
        }
    }
}

/// The hash, the `HASH` of a Noise protocol name.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Hash {
    #[default]
    Blake2s,
    Blake2b,
    Sha256,
    Sha512,
}

impl Hash {
    const fn name(self) -> &'static str {
        match self {
            Hash::Blake2s => "BLAKE2s",
            Hash::Blake2b => "BLAKE2b",
            Hash::Sha256 => "SHA256",
            Hash::Sha512 => "SHA512",
        }
    }
}

/// How key material is written down.
///
/// The same three the `encrypt` plugin takes, with the same default, because a
/// user who has learned one should not have to learn the other.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyFormat {
    #[default]
    Hex,
    Base64,
    /// The bytes as they are, which only makes sense for a file.
    Raw,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Noise {
    /// Which handshake to run, as the specification names it, with any psk
    /// modifier attached: `xx`, `nnpsk0`, `xxpsk3`. Required, because the only
    /// pattern that needs no key material is the one that proves nothing, and
    /// defaulting to that would be a choice made on the user's behalf.
    pub pattern: Option<Handshake>,

    /// The AEAD. Both sides must agree; there is no negotiation in Noise.
    pub cipher: Cipher,
    /// The hash. Both sides must agree.
    pub hash: Hash,

    /// The shared secret for a `psk` pattern, 32 bytes.
    pub psk: Option<String>,
    /// The same, read from a file.
    pub psk_file: Option<String>,
    /// The same, read from an environment variable.
    pub psk_env: Option<String>,

    /// This side's X25519 static private key, 32 bytes. Needed whenever this
    /// side's letter in the pattern is `K`, `X` or `I`.
    pub key: Option<String>,
    /// The same, read from a file.
    pub key_file: Option<String>,
    /// The same, read from an environment variable.
    pub key_env: Option<String>,

    /// How `psk`, `key` and `peer` are encoded.
    pub key_format: KeyFormat,

    /// The peer's expected X25519 static public key. Required when the peer's
    /// letter is `K`, since the handshake cannot start without it. Where the
    /// peer's letter is `X` or `I` the key arrives during the handshake, and
    /// this is a pin checked against it: without one, those patterns encrypt
    /// but authenticate nothing.
    pub peer: Option<String>,

    /// Arbitrary text mixed into the handshake hash. Both sides must write the
    /// same thing or the handshake fails, which makes it a cheap way to bind a
    /// session to context agreed elsewhere.
    pub prologue: Option<String>,

    /// Rekey each direction's cipher after this many records.
    ///
    /// Bounds how much data sits under one key. Not about nonce exhaustion: the
    /// Noise nonce is a 64 bit counter, unreachable, and the specification's
    /// rekey does not reset it. Both sides count the same records, so nothing
    /// has to be signalled, which is the whole reason to prefer a fixed policy
    /// over an in-band control message.
    pub rekey: Option<NonZeroUsize>,
}

impl Noise {
    pub(in crate::endpoint) fn option(
        &mut self,
        opt: &Opt<'_>,
    ) -> Result<bool, ParseEndpointError> {
        match normalize(opt.key).as_str() {
            "pattern" => self.pattern = Some(pattern(opt)?),
            "cipher" => self.cipher = cipher(opt)?,
            "hash" => self.hash = hash(opt)?,
            "psk" => self.psk = Some(opt.string()?),
            "pskfile" => self.psk_file = Some(opt.string()?),
            "pskenv" => self.psk_env = Some(opt.string()?),
            "key" => self.key = Some(opt.string()?),
            "keyfile" => self.key_file = Some(opt.string()?),
            "keyenv" => self.key_env = Some(opt.string()?),
            "keyformat" => self.key_format = key_format(opt)?,
            "peer" | "peerkey" => self.peer = Some(opt.string()?),
            "prologue" => self.prologue = Some(opt.string()?),
            "rekey" => self.rekey = Some(opt.count()?),
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// Refuse a stack that cannot work, before anything is opened.
    ///
    /// Wrong-side and wrong-pattern options are errors rather than ignored, for
    /// the reason `tls` gives: an option that parses and does nothing is how a
    /// relay ends up looking configured and behaving otherwise. An
    /// unauthenticated pattern is not refused, only warned about, because
    /// talking to something that offers nothing better is a real need.
    pub(in crate::endpoint) fn check(
        &self,
        below_is_datagram: bool,
        listening: bool,
    ) -> anyhow::Result<()> {
        if below_is_datagram {
            bail!(
                "noise needs a byte stream underneath it: over a datagram transport a lost \
                 record desynchronises the nonce and nothing after it decrypts",
            );
        }

        let Some(pattern) = self.pattern else {
            bail!(
                "noise needs pattern=: two letters from nn, nk, nx, kn, kk, kx, xn, xk, xx, in, \
                 ik or ix, optionally with psk0 to psk3",
            );
        };

        let name = pattern.name();

        // A psk goes into a numbered message, so psk3 needs a pattern that has
        // a third one. Only the X initiators do.
        if let Some(index) = pattern.psk
            && u32::from(index) > u32::from(pattern.messages())
        {
            bail!(
                "pattern={name} has {} handshake messages, so psk{index} has no message to \
                     go in: psk3 needs xn, xk or xx",
                pattern.messages(),
            );
        }

        let mine = pattern.mine(listening);
        let theirs = pattern.theirs(listening);

        let psk = self.psk_sources();
        let key = self.key_sources();

        if pattern.psk.is_some() {
            if psk == 0 {
                bail!("pattern={name} needs a shared secret: give psk, psk-file or psk-env");
            }
        } else if psk > 0 {
            bail!("psk needs a psk pattern, and pattern={name} is not one: try {name}psk0");
        }

        if mine.has_key() {
            if key == 0 {
                bail!(
                    "pattern={name} makes this side {}, so it needs a static key: give key, \
                     key-file or key-env",
                    if listening {
                        "the responder"
                    } else {
                        "the initiator"
                    },
                );
            }
        } else if key > 0 {
            bail!("pattern={name} gives this side no static key, so key does nothing");
        }

        if psk > 1 {
            bail!("psk, psk-file and psk-env are alternatives, not a list");
        }

        if key > 1 {
            bail!("key, key-file and key-env are alternatives, not a list");
        }

        // `K` means the peer's key is not sent, so it has to be here already.
        // `X` and `I` send it, so peer is a pin rather than a requirement. `N`
        // means there is no such key to name.
        if theirs.known_in_advance() && self.peer.is_none() {
            bail!("pattern={name} needs peer= naming the static key the other side will use");
        }

        if !theirs.has_key() && self.peer.is_some() {
            bail!("pattern={name} gives the other side no static key, so peer names nothing");
        }

        Ok(())
    }

    /// Wrap a connection this relay dialled.
    ///
    /// `host` is only used to say which peer a failure was with. Unlike TLS
    /// there is no name to check against: a Noise peer is a key, not a
    /// hostname.
    pub(in crate::endpoint) async fn wrap_client(
        &self,
        stream: EndpointStream,
        host: &str,
    ) -> anyhow::Result<EndpointStream> {
        self.wrap(stream, true)
            .await
            .with_context(|| format!("noise handshake with {host}"))
    }

    /// Wrap a connection this relay accepted.
    pub(in crate::endpoint) async fn wrap_server(
        &self,
        stream: EndpointStream,
    ) -> anyhow::Result<EndpointStream> {
        self.wrap(stream, false)
            .await
            .context("noise handshake with client")
    }

    async fn wrap(
        &self,
        stream: EndpointStream,
        initiator: bool,
    ) -> anyhow::Result<EndpointStream> {
        let EndpointStream::Duplex(inner) = stream else {
            bail!("noise needs a two-way stream underneath it");
        };

        let pattern = self.pattern.context("noise needs pattern=")?;
        let listening = !initiator;

        // Says nothing about the pattern's reputation, only about what this
        // configuration proves: with no psk, no pre-shared peer key and no pin,
        // whoever answers is accepted.
        //
        // Split because the two ways of arriving here need different advice. A
        // peer that sends a static key is one `peer=` away from being
        // authenticated. A peer that has none cannot be, whatever is set here.
        let theirs = pattern.theirs(listening);

        if pattern.psk.is_none() && !theirs.known_in_advance() && self.peer.is_none() {
            if theirs.transmitted() {
                warn!(
                    "patter={} carries the other side's static key but nothing checks it, so \
                     whoever answers is accepted: set peer= to the key you expect",
                    pattern.name(),
                );
            } else {
                warn!(
                    "pattern={name} authenticates nobody and has no static key to pin: the \
                     connection is private from a listener but not from anything in the path. \
                     {name}psk0 fixes that with one shared secret",
                    name = pattern.name(),
                );
            }
        }

        let expected = self.peer_key()?;
        let handshake = self.handshake(pattern, initiator)?;
        let stream = run(inner, handshake, expected, self.rekey).await?;

        Ok(EndpointStream::Duplex(Box::new(stream)))
    }

    /// Build the handshake state, loading whatever key material the pattern
    /// needs.
    ///
    /// [`Noise::check`] has already rejected the combinations that cannot work,
    /// so a missing value here means the two disagreed and is worth an error
    /// rather than a panic.
    fn handshake(&self, pattern: Handshake, initiator: bool) -> anyhow::Result<HandshakeState> {
        let protocol = format!(
            "Noise_{}_25519_{}_{}",
            pattern.name(),
            self.cipher.name(),
            self.hash.name(),
        );

        let params: NoiseParams = protocol
            .parse()
            .with_context(|| format!("noise cannot speak {protocol}"))?;

        // Held out here because the builder borrows them until it is consumed.
        let psk = self.psk_material()?;
        let key = self.key_material()?;
        let peer = self.peer_key()?;

        let mut builder = Builder::new(params);

        if let Some(prologue) = &self.prologue {
            builder = builder.prologue(prologue.as_bytes())?;
        }

        if let Some(index) = pattern.psk {
            let psk = psk
                .as_ref()
                .with_context(|| format!("pattern={} needs a shared secret", pattern.name()))?;

            builder = builder.psk(index, psk)?;
        }

        if pattern.mine(!initiator).has_key() {
            let key = key
                .as_ref()
                .with_context(|| format!("pattern={} needs a static key", pattern.name()))?;

            builder = builder.local_private_key(key)?;
        }

        // Given to the builder only where the handshake cannot start without
        // it. Where the peer's key arrives during the handshake instead, it is
        // checked afterwards, and handing it over here would change which
        // pattern runs.
        if pattern.theirs(!initiator).known_in_advance() {
            let peer = peer
                .as_ref()
                .with_context(|| format!("pattern={} needs peer=", pattern.name()))?;

            builder = builder.remote_public_key(peer)?;
        }

        let state = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };

        Ok(state)
    }

    fn psk_sources(&self) -> usize {
        [
            self.psk.is_some(),
            self.psk_file.is_some(),
            self.psk_env.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count()
    }

    fn key_sources(&self) -> usize {
        [
            self.key.is_some(),
            self.key_file.is_some(),
            self.key_env.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count()
    }

    fn psk_material(&self) -> anyhow::Result<Option<NoiseKey>> {
        load(
            "psk",
            self.psk.as_deref(),
            self.psk_file.as_deref(),
            self.psk_env.as_deref(),
            self.key_format,
        )
    }

    fn key_material(&self) -> anyhow::Result<Option<NoiseKey>> {
        load(
            "key",
            self.key.as_deref(),
            self.key_file.as_deref(),
            self.key_env.as_deref(),
            self.key_format,
        )
    }

    fn peer_key(&self) -> anyhow::Result<Option<NoiseKey>> {
        load("peer", self.peer.as_deref(), None, None, self.key_format)
    }
}

/// Read one key from whichever source was given, and check its length.
///
/// Every value this loads is 32 bytes, so the check belongs here rather than at
/// each call site: snow would otherwise report a length problem as a builder
/// error that does not say which option was wrong.
fn load(
    label: &'static str,
    text: Option<&str>,
    file: Option<&str>,
    env: Option<&str>,
    format: KeyFormat,
) -> anyhow::Result<Option<NoiseKey>> {
    let raw = if let Some(text) = text {
        text.as_bytes().to_vec()
    } else if let Some(path) = file {
        std::fs::read(path).with_context(|| format!("{label}-file: {path}"))?
    } else if let Some(name) = env {
        std::env::var(name)
            .with_context(|| format!("{label}-env: {name}"))?
            .into_bytes()
    } else {
        return Ok(None);
    };

    let key = decode(label, &raw, format)?;
    let key = key.try_into().map_err(|v: Vec<u8>| {
        anyhow::anyhow!("{label} is {} bytes, and must be {KEY_LEN}", v.len())
    })?;

    Ok(Some(key))
}

/// Decode key material, which is text in every format but `raw`.
///
/// Whitespace is noise in both encodings, so a key file written by hand, or one
/// with a trailing newline, decodes to what it looks like. The same rule the
/// `encrypt` plugin follows.
fn decode(label: &'static str, raw: &[u8], format: KeyFormat) -> anyhow::Result<Vec<u8>> {
    if format == KeyFormat::Raw {
        return Ok(raw.to_vec());
    }

    let text = std::str::from_utf8(raw)
        .with_context(|| format!("{label} is not text, so it is neither hex nor base64"))?;

    let text: String = text.split_whitespace().collect();

    match format {
        KeyFormat::Hex => decode_hex(&text).with_context(|| format!("{label} is not hex")),
        KeyFormat::Base64 => BASE64_STANDARD
            .decode(&text)
            .with_context(|| format!("{label} is not base64")),
        KeyFormat::Raw => unreachable!("returned above"),
    }
}

/// Hex, without a dependency for it.
///
/// `hex` is a plugin feature rather than something this crate already has, and
/// pulling it in as an unconditional dependency to read three 32 byte values
/// is not a trade worth making.
fn decode_hex(text: &str) -> anyhow::Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        bail!("odd number of digits");
    }

    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            if !pair.iter().all(u8::is_ascii_hexdigit) {
                bail!("not a hex digit");
            }

            let digits = std::str::from_utf8(pair).expect("ascii hex digits are utf8");

            u8::from_str_radix(digits, 16).map_err(|error| anyhow::anyhow!("{error}"))
        })
        .collect()
}

/// Drive the handshake, then check any pinned peer key.
///
/// No timeout here on purpose: a peer that stops mid-handshake would hang this
/// forever, and bounding connection setup is the caller's job, in the one place
/// that already does it for every layer.
async fn run(
    mut inner: Box<dyn AsyncStream>,
    mut state: HandshakeState,
    expected: Option<NoiseKey>,
    rekey: Option<NonZeroUsize>,
) -> anyhow::Result<NoiseStream> {
    let mut message = vec![0_u8; MAX_MESSAGE];
    let mut payload = vec![0_u8; MAX_MESSAGE];

    while !state.is_handshake_finished() {
        if state.is_my_turn() {
            let len = state.write_message(&[], &mut message)?;
            write_message(&mut inner, &message[..len]).await?;
        } else {
            let len = read_message(&mut inner, &mut message).await?;
            state.read_message(&message[..len], &mut payload)?;
        }
    }

    // For `xx` the peer's key is only known now, so the pin is checked here and
    // the connection dropped on a mismatch. For `ik` when dialling this is a
    // tautology, but the listening side can still pin its clients with it.
    if let Some(expected) = expected {
        match state.get_remote_static() {
            Some(actual) if actual == expected.as_slice() => {}
            Some(_) => bail!("peer static key does not match peer="),
            None => bail!("the handshake established no peer static key to check peer= against"),
        }
    }

    Ok(NoiseStream::new(
        inner,
        state.into_transport_mode()?,
        rekey.map(NonZeroUsize::get),
    ))
}

/// Write one length-prefixed handshake message.
///
/// Flushed immediately: the peer cannot take its turn until it has the whole
/// message, and there may be a buffering transport underneath.
async fn write_message(inner: &mut Box<dyn AsyncStream>, message: &[u8]) -> anyhow::Result<()> {
    let len = u16::try_from(message.len()).context("handshake message exceeds 65535 bytes")?;

    inner.write_all(&len.to_be_bytes()).await?;
    inner.write_all(message).await?;
    inner.flush().await?;

    Ok(())
}

/// Read one length-prefixed handshake message, returning its length.
async fn read_message(inner: &mut Box<dyn AsyncStream>, buf: &mut [u8]) -> anyhow::Result<usize> {
    let mut prefix = [0_u8; LEN_PREFIX];
    inner.read_exact(&mut prefix).await?;

    let len = usize::from(u16::from_be_bytes(prefix));
    inner.read_exact(&mut buf[..len]).await?;

    Ok(len)
}

/// Which half of a record the read side is collecting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prefix,
    Body,
}

/// An established session, as a byte stream.
///
/// Buffers are sized once here rather than grown per record: this is on the
/// path of every relayed byte and a record can be 64 KiB.
pub struct NoiseStream {
    inner: Box<dyn AsyncStream>,
    state: TransportState,
    rekey: Option<usize>,

    /// Ciphertext being collected, or the length prefix while in
    /// [`Phase::Prefix`].
    read_buf: Vec<u8>,
    read_pos: usize,
    read_need: usize,
    phase: Phase,
    /// Decrypted bytes not yet handed to the caller.
    plain: Vec<u8>,
    plain_pos: usize,
    plain_len: usize,
    received: usize,

    /// One encoded record on its way to the transport.
    write_buf: Vec<u8>,
    write_pos: usize,
    write_len: usize,
    sent: usize,
}

impl NoiseStream {
    fn new(inner: Box<dyn AsyncStream>, state: TransportState, rekey: Option<usize>) -> Self {
        Self {
            inner,
            state,
            rekey,
            read_buf: vec![0_u8; MAX_MESSAGE],
            read_pos: 0,
            read_need: LEN_PREFIX,
            phase: Phase::Prefix,
            plain: vec![0_u8; MAX_MESSAGE],
            plain_pos: 0,
            plain_len: 0,
            received: 0,
            write_buf: vec![0_u8; LEN_PREFIX + MAX_MESSAGE],
            write_pos: 0,
            write_len: 0,
            sent: 0,
        }
    }

    /// Push a buffered record out.
    ///
    /// An empty buffer is the only state in which encrypting the next record is
    /// safe: the nonce advances at encryption time, so records have to reach
    /// the peer in the order they were made.
    fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.write_pos < self.write_len {
            let n = ready!(
                Pin::new(&mut self.inner)
                    .poll_write(cx, &self.write_buf[self.write_pos..self.write_len])
            )?;

            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }

            self.write_pos += n;
        }

        self.write_pos = 0;
        self.write_len = 0;

        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for NoiseStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            if this.plain_pos < this.plain_len {
                let n = min(buf.remaining(), this.plain_len - this.plain_pos);
                buf.put_slice(&this.plain[this.plain_pos..this.plain_pos + n]);
                this.plain_pos += n;

                return Poll::Ready(Ok(()));
            }

            while this.read_pos < this.read_need {
                let mut chunk = ReadBuf::new(&mut this.read_buf[this.read_pos..this.read_need]);
                ready!(Pin::new(&mut this.inner).poll_read(cx, &mut chunk))?;

                let n = chunk.filled().len();

                if n == 0 {
                    // End of stream only means anything on a record boundary.
                    // Anywhere else the connection was cut mid-record.
                    if this.phase == Phase::Prefix && this.read_pos == 0 {
                        return Poll::Ready(Ok(()));
                    }

                    return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                }

                this.read_pos += n;
            }

            if this.phase == Phase::Prefix {
                let len = usize::from(u16::from_be_bytes([this.read_buf[0], this.read_buf[1]]));

                // Every record carries a tag, so a shorter one is malformed and
                // would otherwise spin this loop on a zero length body.
                if len < TAG_LEN {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "noise: record shorter than its authentication tag",
                    )));
                }

                this.phase = Phase::Body;
                this.read_pos = 0;
                this.read_need = len;

                continue;
            }

            let n = this
                .state
                .read_message(&this.read_buf[..this.read_need], &mut this.plain)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

            this.plain_len = n;
            this.plain_pos = 0;
            this.phase = Phase::Prefix;
            this.read_pos = 0;
            this.read_need = LEN_PREFIX;
            this.received += 1;

            if let Some(every) = this.rekey
                && this.received.is_multiple_of(every)
            {
                this.state.rekey_incoming();
            }
        }
    }
}

impl AsyncWrite for NoiseStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        ready!(this.poll_send(cx))?;

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let take = min(buf.len(), MAX_PLAINTEXT);
        let n = this
            .state
            .write_message(&buf[..take], &mut this.write_buf[LEN_PREFIX..])
            .map_err(io::Error::other)?;

        let len = u16::try_from(n).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "noise: record exceeds 65535 bytes",
            )
        })?;

        this.write_buf[..LEN_PREFIX].copy_from_slice(&len.to_be_bytes());
        this.write_pos = 0;
        this.write_len = LEN_PREFIX + n;
        this.sent += 1;

        if let Some(every) = this.rekey
            && this.sent.is_multiple_of(every)
        {
            this.state.rekey_outgoing();
        }

        // Reporting the plaintext consumed before the record has necessarily
        // reached the transport is what keeps a half written record from being
        // encrypted a second time; `poll_flush` and the next `poll_write`
        // finish it.
        let _ = this.poll_send(cx)?;

        Poll::Ready(Ok(take))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_send(cx))?;

        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_send(cx))?;

        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

fn pattern(opt: &Opt<'_>) -> Result<Handshake, ParseEndpointError> {
    Handshake::parse(&normalize(opt.text()?)).ok_or_else(|| {
        ParseEndpointError::InvalidFlag(format!(
            "pattern={}, which is two letters from nn, nk, nx, kn, kk, kx, xn, xk, xx, in, ik or \
             ix, optionally with psk0 to psk3",
            opt.text().unwrap_or_default(),
        ))
    })
}

fn cipher(opt: &Opt<'_>) -> Result<Cipher, ParseEndpointError> {
    match normalize(opt.text()?).as_str() {
        "chachapoly" | "chacha20poly1305" | "chacha20" => Ok(Cipher::ChaChaPoly),
        "aesgcm" | "aes256gcm" | "aes" => Ok(Cipher::AesGcm),
        other => Err(ParseEndpointError::InvalidFlag(format!(
            "cipher={other}, which is chachapoly or aesgcm"
        ))),
    }
}

fn hash(opt: &Opt<'_>) -> Result<Hash, ParseEndpointError> {
    match normalize(opt.text()?).as_str() {
        "blake2s" => Ok(Hash::Blake2s),
        "blake2b" => Ok(Hash::Blake2b),
        "sha256" => Ok(Hash::Sha256),
        "sha512" => Ok(Hash::Sha512),
        other => Err(ParseEndpointError::InvalidFlag(format!(
            "hash={other}, which is blake2s, blake2b, sha256 or sha512"
        ))),
    }
}

fn key_format(opt: &Opt<'_>) -> Result<KeyFormat, ParseEndpointError> {
    match normalize(opt.text()?).as_str() {
        "hex" => Ok(KeyFormat::Hex),
        "base64" | "b64" => Ok(KeyFormat::Base64),
        "raw" | "binary" => Ok(KeyFormat::Raw),
        other => Err(ParseEndpointError::InvalidFlag(format!(
            "key-format={other}, which is hex, base64 or raw"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{EndpointSpec, LayerSpec};

    /// Every fundamental interactive pattern, in the order the specification
    /// tabulates them.
    const PATTERNS: [&str; 12] = [
        "nn", "nk", "nx", "kn", "kk", "kx", "xn", "xk", "xx", "in", "ik", "ix",
    ];

    /// A 32 byte value, which is all any of the key options accepts.
    const KEY: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn layer(spec: &str) -> Noise {
        let spec: EndpointSpec = spec.parse().expect("parses");

        match spec.layers.into_iter().next() {
            Some(LayerSpec::Noise(noise)) => noise,
            other => panic!("wrong layer: {other:?}"),
        }
    }

    fn dialling(spec: &str) -> anyhow::Result<()> {
        layer(spec).check(false, false)
    }

    fn listening(spec: &str) -> anyhow::Result<()> {
        layer(spec).check(false, true)
    }

    #[test]
    fn every_pattern_parses_and_round_trips() {
        for name in PATTERNS {
            let parsed = Handshake::parse(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(parsed.name().to_ascii_lowercase(), name);
            assert_eq!(Handshake::parse(&parsed.name()), Some(parsed), "{name}");
        }
    }

    #[test]
    fn rejects_names_that_are_not_patterns() {
        // `I` is an initiator-only letter, so it is invalid second.
        for bad in [
            "", "n", "ii", "ni", "xi", "xxx", "xxpsk4", "xxpsk", "psk0", "nnpskx",
        ] {
            assert!(Handshake::parse(bad).is_none(), "{bad} should not parse");
        }
    }

    #[test]
    fn only_x_initiators_take_three_messages() {
        for name in PATTERNS {
            let expected = if name.starts_with('x') { 3 } else { 2 };
            assert_eq!(
                Handshake::parse(name).unwrap().messages(),
                expected,
                "{name}"
            );
        }
    }

    /// The requirement table written out by hand from the specification, so it
    /// is not the same derivation checking itself.
    #[test]
    fn key_rules_match_the_specification() {
        // pattern, initiator has a key, responder has a key,
        // initiator needs the peer's up front, responder needs the peer's up front
        let table = [
            ("nn", false, false, false, false),
            ("nk", false, true, true, false),
            ("nx", false, true, false, false),
            ("kn", true, false, false, true),
            ("kk", true, true, true, true),
            ("kx", true, true, false, true),
            ("xn", true, false, false, false),
            ("xk", true, true, true, false),
            ("xx", true, true, false, false),
            ("in", true, false, false, false),
            ("ik", true, true, true, false),
            ("ix", true, true, false, false),
        ];

        for (name, initiator_key, responder_key, initiator_peer, responder_peer) in table {
            let p = Handshake::parse(name).unwrap();

            assert_eq!(p.mine(false).has_key(), initiator_key, "{name} own key");
            assert_eq!(p.mine(true).has_key(), responder_key, "{name} own key");
            assert_eq!(
                p.theirs(false).known_in_advance(),
                initiator_peer,
                "{name} peer up front",
            );
            assert_eq!(
                p.theirs(true).known_in_advance(),
                responder_peer,
                "{name} peer up front",
            );
        }
    }

    /// A key is pinnable exactly when the handshake carries it, which is the
    /// third of the three things a letter can mean and the reason `peer` is
    /// optional rather than required or refused.
    #[test]
    fn a_key_is_pinnable_when_the_handshake_carries_it() {
        for name in PATTERNS {
            let p = Handshake::parse(name).unwrap();

            assert_eq!(
                p.theirs(false).transmitted(),
                name.ends_with('x'),
                "{name} pinnable by the initiator",
            );
            assert_eq!(
                p.theirs(true).transmitted(),
                name.starts_with('x') || name.starts_with('i'),
                "{name} pinnable by the responder",
            );

            // The three meanings are exhaustive: a letter either has no key, or
            // has one the peer holds already, or has one it sends.
            let theirs = p.theirs(false);
            assert_eq!(
                theirs.has_key(),
                theirs.known_in_advance() || theirs.transmitted(),
                "{name}",
            );
        }
    }

    #[test]
    fn a_pattern_is_required() {
        assert!(dialling("noise:host:1").is_err());
    }

    #[test]
    fn a_static_key_is_required_where_the_letter_calls_for_one() {
        assert!(dialling("noise:host:1,pattern=nn").is_ok());
        assert!(dialling("noise:host:1,pattern=xn").is_err());
        assert!(dialling(&format!("noise:host:1,pattern=xn,key={KEY}")).is_ok());

        // `nx` gives the initiator no key and the responder one, so the same
        // pattern demands different things of the two sides.
        assert!(dialling("noise:host:1,pattern=nx").is_ok());
        assert!(listening("noise-listen:1,pattern=nx").is_err());
        assert!(listening(&format!("noise-listen:1,pattern=nx,key={KEY}")).is_ok());
    }

    #[test]
    fn a_peer_key_is_required_where_it_is_never_sent() {
        assert!(dialling("noise:host:1,pattern=nk").is_err());
        assert!(dialling(&format!("noise:host:1,pattern=nk,peer={KEY}")).is_ok());

        // The responder in `nk` holds its own key and needs nothing from the
        // initiator, which has none.
        assert!(listening(&format!("noise-listen:1,pattern=nk,key={KEY}")).is_ok());
    }

    #[test]
    fn a_peer_key_is_refused_where_there_is_none_to_name() {
        assert!(dialling(&format!("noise:host:1,pattern=nn,peer={KEY}")).is_err());
        assert!(listening(&format!("noise-listen:1,pattern=xn,peer={KEY}")).is_ok());
    }

    #[test]
    fn a_psk_needs_a_psk_pattern_and_a_message_to_go_in() {
        assert!(dialling(&format!("noise:host:1,pattern=nn,psk={KEY}")).is_err());
        assert!(dialling(&format!("noise:host:1,pattern=nnpsk0,psk={KEY}")).is_ok());
        assert!(dialling("noise:host:1,pattern=nnpsk0").is_err());

        // Two message patterns have no third message for psk3 to sit in.
        assert!(dialling(&format!("noise:host:1,pattern=nnpsk3,psk={KEY}")).is_err());
        assert!(dialling(&format!("noise:host:1,pattern=xxpsk3,key={KEY},psk={KEY}")).is_ok());
    }

    #[test]
    fn key_sources_are_alternatives() {
        assert!(
            dialling(&format!(
                "noise:host:1,pattern=xn,key={KEY},key-env=TOCAT_TEST_KEY"
            ))
            .is_err()
        );
    }

    #[test]
    fn a_datagram_transport_is_refused() {
        assert!(layer("noise:host:1,pattern=nn").check(true, false).is_err());
    }

    #[test]
    fn the_suite_is_chosen_independently_of_the_pattern() {
        let noise = layer("noise:host:1,pattern=nn,cipher=aesgcm,hash=sha512");

        assert_eq!(noise.cipher, Cipher::AesGcm);
        assert_eq!(noise.hash, Hash::Sha512);

        // Defaults, so an unadorned pattern keeps working unchanged.
        let noise = layer("noise:host:1,pattern=nn");

        assert_eq!(noise.cipher, Cipher::ChaChaPoly);
        assert_eq!(noise.hash, Hash::Blake2s);
    }

    #[test]
    fn a_key_must_be_the_right_length() {
        let noise = layer("noise:host:1,pattern=xn,key=00");

        assert!(noise.key_material().is_err());
    }
}
