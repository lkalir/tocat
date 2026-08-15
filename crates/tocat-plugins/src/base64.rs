//! `base64` / `unbase64`: base64 stages for tocat.
//!
//! # One message per call
//!
//! Both stages assume every `on_bytes` call carries exactly one complete
//! message, and neither holds bytes back between calls. Base64 packs 3 source
//! bytes into 4 characters, so a message cut anywhere other than a group
//! boundary cannot be decoded on its own; owning that boundary is the job of
//! the `frame` / `unframe` pair, which belongs immediately outside these
//! stages in the pipeline. Feed `unbase64` a raw socket read and it will
//! decode whatever prefix happens to be group-aligned and reject the rest.
//!
//! That contract is the datagram contract, so both stages report
//! [`datagram_safe`](Plugin::datagram_safe) as true: one call in, one unit
//! out, nothing carried across.
//!
//! The failure is loud rather than silent: a message whose length is not a
//! whole number of groups is a build-your-pipeline-differently error naming
//! `unframe`, not a corrupt payload delivered downstream. The one case that
//! cannot be caught is a message cut exactly on a group boundary, which
//! decodes to a truncated payload; `unframe` is what rules it out.
//!
//! # Direction
//!
//! `direction = "both"` would transcode *both* paths, which is almost never
//! what anyone wants. Declare the pair explicitly, one stage per direction:
//!
//! ```toml
//! # near end of a base64-armored hop
//! [[plugin]]
//! name = "frame"
//! direction = "sink-to-source"
//!
//! [[plugin]]
//! name = "base64"
//! direction = "source-to-sink"
//!
//! [[plugin]]
//! name = "unbase64"
//! direction = "sink-to-source"
//! ```
//!
//! The far end runs the mirror image (`unbase64` forward, `base64` reverse),
//! and the two relays carry arbitrary bytes across a hop that only tolerates
//! text.
//!
//! # Interop
//!
//! `alphabet = "url-safe"` selects the RFC 4648 section 5 alphabet (`-` and
//! `_`) in place of the standard one; set it at both ends of a hop.
//!
//! `base64` always pads. `unbase64` requires padding by default, because
//! under the one-message-per-call contract a short final group is far more
//! likely to be a framing bug than a peer that omits `=`. Set
//! `accept-unpadded = true` for a peer that really does omit it, at the cost
//! of that diagnostic: a message truncated by 1 or 2 characters then decodes
//! to a short payload instead of erroring.
//!
//! Whitespace is not stripped. A trailing newline is a frame delimiter, and
//! stripping it here would paper over an `unframe` stage that is missing or
//! misconfigured.

use ::base64::{
    decoded_len_estimate, encoded_len,
    engine::GeneralPurpose,
    prelude::{BASE64_STANDARD, BASE64_URL_SAFE, Engine},
};
use serde::{Deserialize, Serialize};
use tocat_api::{BuildCtx, Ctx, Plugin, PluginError, PluginFactory, Result, Stage};

pub const BASE64: &str = "base64";
pub const UNBASE64: &str = "unbase64";

/// Names the contract in the one place an operator will read it.
const MISFRAMED: &str = concat!(
    "message is not a whole number of base64 groups: unbase64 decodes one ",
    "complete message per call, so it needs an unframe stage ahead of it",
);

/// Which RFC 4648 alphabet a stage speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Alphabet {
    /// Section 4: `+` and `/`.
    #[default]
    Standard,
    /// Section 5: `-` and `_`, safe in URLs and filenames.
    UrlSafe,
}

impl Alphabet {
    /// Both engines are `static`s in the `base64` crate, so a stage holds a
    /// reference instead of building a decode table per pipeline.
    fn engine(self) -> &'static GeneralPurpose {
        match self {
            Self::Standard => &BASE64_STANDARD,
            Self::UrlSafe => &BASE64_URL_SAFE,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Base64Config {
    /// Alphabet to encode with.
    #[serde(default)]
    pub alphabet: Alphabet,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Unbase64Config {
    /// Alphabet to decode.
    #[serde(default)]
    pub alphabet: Alphabet,
    /// Restore padding the peer omitted instead of rejecting the message.
    #[serde(default)]
    pub accept_unpadded: bool,
}

/// Maps [`::base64::DecodeSliceError`] to [`PluginError`].
fn decode_error(err: ::base64::DecodeSliceError) -> PluginError {
    PluginError::runtime(UNBASE64, err)
}

/// Maps [`::base64::EncodeSliceError`] to [`PluginError`].
fn encode_error(err: ::base64::EncodeSliceError) -> PluginError {
    PluginError::runtime(BASE64, err)
}

pub struct Base64 {
    engine: &'static GeneralPurpose,
    /// Reused across calls, so a steady stream settles on one allocation.
    out: Vec<u8>,
}

impl Base64 {
    fn new(alphabet: Alphabet) -> Self {
        Self {
            engine: alphabet.engine(),
            out: Vec::new(),
        }
    }
}

impl Plugin for Base64 {
    fn name(&self) -> &str {
        BASE64
    }

    /// Encodes one whole message, padding included.
    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }

        let len = encoded_len(input.len(), true)
            .ok_or_else(|| PluginError::runtime(BASE64, "encoded message would overflow usize"))?;
        self.out.resize(len, 0);
        let written = self
            .engine
            .encode_slice(input, &mut self.out)
            .map_err(encode_error)?;
        ctx.forward(&self.out[..written]);

        Ok(())
    }

    /// Safe on a datagram path: one message in, one message out, no state
    /// carried between calls.
    fn datagram_safe(&self) -> bool {
        true
    }
}

pub struct Unbase64 {
    engine: &'static GeneralPurpose,
    accept_unpadded: bool,
    /// Only touched for an unpadded message, so the padded path stays a
    /// single copy out of `input` and into `out`.
    repadded: Vec<u8>,
    /// Reused across calls, so a steady stream settles on one allocation.
    out: Vec<u8>,
}

impl Unbase64 {
    fn new(config: Unbase64Config) -> Self {
        Self {
            engine: config.alphabet.engine(),
            accept_unpadded: config.accept_unpadded,
            repadded: Vec::new(),
            out: Vec::new(),
        }
    }

    /// Takes the fields it needs rather than `&mut self`, so the caller can
    /// pass a slice borrowed from another field.
    fn decode_into(
        engine: &GeneralPurpose,
        out: &mut Vec<u8>,
        ctx: &mut Ctx<'_>,
        message: &[u8],
    ) -> Result<()> {
        out.resize(decoded_len_estimate(message.len()), 0);
        let written = engine.decode_slice(message, out).map_err(decode_error)?;
        ctx.forward(&out[..written]);

        Ok(())
    }
}

impl Plugin for Unbase64 {
    fn name(&self) -> &str {
        UNBASE64
    }

    /// Decodes one whole message.
    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }

        // A short final group is legal base64 only when padding is optional;
        // a remainder of 1 is never legal, whatever the peer intended.
        let remainder = input.len() % 4;

        if remainder == 0 {
            return Self::decode_into(self.engine, &mut self.out, ctx, input);
        }

        if remainder == 1 || !self.accept_unpadded {
            return Err(PluginError::runtime(UNBASE64, MISFRAMED));
        }

        self.repadded.clear();
        self.repadded.extend_from_slice(input);
        self.repadded.resize(input.len() + 4 - remainder, b'=');

        Self::decode_into(self.engine, &mut self.out, ctx, &self.repadded)
    }

    /// Safe on a datagram path: one message in, one message out, no state
    /// carried between calls.
    fn datagram_safe(&self) -> bool {
        true
    }
}

pub struct Base64Factory;

impl PluginFactory for Base64Factory {
    fn name(&self) -> &str {
        BASE64
    }

    fn description(&self) -> &str {
        "base64-encode this direction, one message per chunk"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: Base64Config = ctx.config()?;

        Ok(Stage::filter(Base64::new(config.alphabet)))
    }
}

pub struct Unbase64Factory;

impl PluginFactory for Unbase64Factory {
    fn name(&self) -> &str {
        UNBASE64
    }

    fn description(&self) -> &str {
        "base64-decode this direction, one message per chunk"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: Unbase64Config = ctx.config()?;

        Ok(Stage::filter(Unbase64::new(config)))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tocat_api::{
        ChannelId, ChannelTarget, Direction, EffectSink, Emission, HostBuilder, LogLevel,
        PipelineMeta, StageInfo,
    };

    use super::*;

    /// 43 bytes: not a multiple of 3, so a round trip ends in real padding.
    const SAMPLE: &[u8] = b"the quick brown fox jumps over 13 lazy dogs";

    struct NullHost;

    impl HostBuilder for NullHost {
        fn open_channel(&mut self, _target: ChannelTarget) -> Result<ChannelId> {
            Ok(ChannelId(0))
        }
    }

    #[derive(Default)]
    struct Silent;

    impl EffectSink for Silent {
        fn write(&mut self, _channel: ChannelId, _bytes: &[u8]) {}
        fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "src", "sink")
    }

    fn build(factory: &dyn PluginFactory, config: Value) -> Box<dyn Plugin> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: factory.name(),
            upstream: "src",
            downstream: "sink",
        };
        let mut ctx = BuildCtx::new(factory.name(), &map, &meta, stage, &mut host);
        match factory.build(&mut ctx).expect("build") {
            Stage::Filter(plugin) => plugin,
            Stage::External(_) => unreachable!("base64 stages are filters"),
        }
    }

    fn try_feed(plugin: &mut dyn Plugin, message: &[u8]) -> Result<Vec<u8>> {
        let name = plugin.name().to_owned();
        let meta = meta();
        let mut emission = Emission::new();
        let mut sink = Silent;
        {
            let mut ctx = Ctx::new(&meta, &name, message, &mut emission, &mut sink);
            plugin.on_bytes(&mut ctx, message)?;
        }

        Ok(emission.bytes().to_vec())
    }

    fn feed(plugin: &mut dyn Plugin, message: &[u8]) -> Vec<u8> {
        try_feed(plugin, message).expect("on_bytes")
    }

    #[test]
    fn round_trips_a_message() {
        let mut encoder = build(&Base64Factory, json!({}));
        let mut decoder = build(&Unbase64Factory, json!({}));

        let wire = feed(encoder.as_mut(), SAMPLE);
        assert_eq!(feed(decoder.as_mut(), &wire), SAMPLE);
    }

    #[test]
    fn round_trips_every_length() {
        let mut encoder = build(&Base64Factory, json!({}));
        let mut decoder = build(&Unbase64Factory, json!({}));

        for len in 0..=64usize {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();

            let wire = feed(encoder.as_mut(), &plain);
            assert_eq!(feed(decoder.as_mut(), &wire), plain, "length {len}");
        }
    }

    /// The contract: a call is a message, so each one encodes independently
    /// and each one is decodable on its own.
    #[test]
    fn each_call_is_a_self_contained_message() {
        let mut encoder = build(&Base64Factory, json!({}));
        let mut decoder = build(&Unbase64Factory, json!({}));

        let first = feed(encoder.as_mut(), b"one");
        let second = feed(encoder.as_mut(), b"two");

        assert_eq!(first, BASE64_STANDARD.encode(b"one").into_bytes());
        assert_eq!(second, BASE64_STANDARD.encode(b"two").into_bytes());

        // Nothing carries over, so the second decodes without the first.
        assert_eq!(feed(decoder.as_mut(), &second), b"two");
        assert_eq!(feed(decoder.as_mut(), &first), b"one");
    }

    #[test]
    fn encodes_a_message_that_needs_padding() {
        let mut encoder = build(&Base64Factory, json!({}));

        assert_eq!(feed(encoder.as_mut(), b"ab"), b"YWI=");
    }

    #[test]
    fn empty_message_emits_nothing() {
        let mut encoder = build(&Base64Factory, json!({}));
        let mut decoder = build(&Unbase64Factory, json!({}));

        assert!(feed(encoder.as_mut(), b"").is_empty());
        assert!(feed(decoder.as_mut(), b"").is_empty());
    }

    /// Splitting a message mid-group is the mistake this stage cannot fix, so
    /// it has to name the fix instead.
    #[test]
    fn decoder_rejects_a_message_cut_mid_group() {
        let wire = BASE64_STANDARD.encode(SAMPLE).into_bytes();
        let mut decoder = build(&Unbase64Factory, json!({}));

        for cut in [1usize, 2, 3] {
            let err = try_feed(decoder.as_mut(), &wire[..wire.len() - cut])
                .expect_err("a partial group must not decode");
            assert!(
                err.to_string().contains("unframe"),
                "the error must point at the framing stage: {err}",
            );
        }
    }

    #[test]
    fn decoder_accepts_an_unpadded_message_when_configured() {
        let mut wire = BASE64_STANDARD.encode(SAMPLE).into_bytes();
        while wire.last() == Some(&b'=') {
            wire.pop();
        }

        let mut decoder = build(&Unbase64Factory, json!({ "accept-unpadded": true }));
        assert_eq!(feed(decoder.as_mut(), &wire), SAMPLE);

        // A remainder of 1 is not base64 under any padding rule.
        assert!(try_feed(decoder.as_mut(), &wire[..5]).is_err());
    }

    #[test]
    fn decoder_rejects_characters_outside_the_alphabet() {
        let mut decoder = build(&Unbase64Factory, json!({}));

        assert!(try_feed(decoder.as_mut(), b"aGVs*G8=").is_err());
    }

    #[test]
    fn url_safe_alphabet_avoids_plus_and_slash() {
        let plain = [0xfb_u8, 0xff, 0xbf];
        let config = json!({ "alphabet": "url-safe" });

        let mut encoder = build(&Base64Factory, config.clone());
        let wire = feed(encoder.as_mut(), &plain);
        assert_eq!(wire, b"-_-_");

        let mut decoder = build(&Unbase64Factory, config);
        assert_eq!(feed(decoder.as_mut(), &wire), plain);
    }

    /// The guide says both stages may sit on a datagram path, which is only
    /// true if they say so themselves: the trait defaults to false.
    #[test]
    fn both_stages_are_datagram_safe() {
        let encoder = build(&Base64Factory, json!({}));
        let decoder = build(&Unbase64Factory, json!({}));

        assert!(encoder.datagram_safe());
        assert!(decoder.datagram_safe());
    }

    #[test]
    fn rejects_unknown_config_keys() {
        let map = json!({ "level": 3 }).as_object().unwrap().clone();
        let meta = meta();
        let mut host = NullHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: BASE64,
            upstream: "src",
            downstream: "sink",
        };
        let mut ctx = BuildCtx::new(BASE64, &map, &meta, stage, &mut host);

        assert!(Base64Factory.build(&mut ctx).is_err());
    }
}
