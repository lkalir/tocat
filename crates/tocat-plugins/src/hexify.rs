//! `hexify` / `unhexify`: hex stages for tocat.
//!
//! # One message per call
//!
//! Both stages assume every `on_bytes` call carries exactly one complete
//! message, and neither holds bytes back between calls. Hex packs 1 source
//! byte into 2 characters, so a message cut on an odd offset cannot be decoded
//! on its own; owning that boundary is the job of the `frame` / `unframe`
//! pair, which belongs immediately outside these stages in the pipeline. Feed
//! `unhexify` a raw socket read and it will reject anything that did not
//! happen to land on a byte boundary.
//!
//! That contract is the datagram contract, so both stages report
//! [`Boundaries::Preserve`]: one call in, one unit out, nothing carried
//! across.
//!
//! The failure is loud rather than silent: an odd-length message is a
//! build-your-pipeline-differently error naming `unframe`, not a corrupt
//! payload delivered downstream. The one case that cannot be caught is a
//! message cut on an even offset, which decodes to a truncated payload;
//! `unframe` is what rules it out. Hex has the shortest group of any of these
//! codecs, so that case is also the likeliest: half of all cuts land on one.
//!
//! # Direction
//!
//! `direction = "both"` would transcode *both* paths, which is almost never
//! what anyone wants. Declare the pair explicitly, one stage per direction:
//!
//! ```toml
//! # near end of a hex-armored hop
//! [[plugin]]
//! name = "frame"
//! direction = "sink-to-source"
//!
//! [[plugin]]
//! name = "hexify"
//! direction = "source-to-sink"
//!
//! [[plugin]]
//! name = "unhexify"
//! direction = "sink-to-source"
//! ```
//!
//! The far end runs the mirror image (`unhexify` forward, `hexify` reverse),
//! and the two relays carry arbitrary bytes across a hop that only tolerates
//! text.
//!
//! # Interop
//!
//! `case = "upper"` emits `A`-`F` in place of `a`-`f`. Unlike the base64
//! `alphabet` option it does not have to agree with the far end: `unhexify`
//! accepts either case, and mixtures of the two, whatever the `hexify` on the
//! other side is set to. It is a choice about what a human reading the wire
//! sees, nothing more.
//!
//! Whitespace is not stripped. A trailing newline is a frame delimiter, and
//! stripping it here would paper over an `unframe` stage that is missing or
//! misconfigured. Neither is a `0x` prefix accepted: this is a wire codec, not
//! a parser for hex written for people.
//!
//! # Size
//!
//! Hex doubles, exactly and with no padding, which is a steep price next to
//! base64's four-for-three but a perfectly predictable one. Every source byte
//! is two characters at a fixed offset, so a message stays sliceable and
//! greppable on the wire, which is the usual reason to pay it.

use serde::{Deserialize, Serialize};
use tocat_api::{
    Boundaries, BuildCtx, Ctx, Needs, Plugin, PluginError, PluginFactory, Result, Stage,
};

pub const HEXIFY: &str = "hexify";
pub const UNHEXIFY: &str = "unhexify";

/// Names the contract in the one place an operator will read it.
const MISFRAMED: &str = concat!(
    "message has an odd number of hex digits: unhexify decodes one complete ",
    "message per call, so it needs an unframe stage ahead of it",
);

/// Which case a stage writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Case {
    /// Digits and `a`-`f`.
    #[default]
    #[serde(alias = "lower")]
    Lowercase,
    /// Digits and `A`-`F`.
    #[serde(alias = "upper")]
    Uppercase,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct HexifyConfig {
    /// Case to encode with.
    #[serde(default)]
    pub case: Case,
}

/// Decoding has nothing to configure, but the type still earns its keep:
/// `deny_unknown_fields` on an empty struct is what turns `unhexify,case=upper`
/// into an error instead of an option that silently does nothing.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct UnhexifyConfig {}

/// Maps [`hex::FromHexError`] to [`PluginError`].
fn encode_error(err: hex::FromHexError) -> PluginError {
    PluginError::runtime(HEXIFY, err)
}

/// Maps [`hex::FromHexError`] to [`PluginError`].
///
/// Separate from [`encode_error`] only so a decode failure is attributed to
/// the stage that actually failed.
fn decode_error(err: hex::FromHexError) -> PluginError {
    PluginError::runtime(UNHEXIFY, err)
}

pub struct Hexify {
    case: Case,
    /// Reused across calls, so a steady stream settles on one allocation.
    out: Vec<u8>,
}

impl Hexify {
    fn new(case: Case) -> Self {
        Self {
            case,
            out: Vec::new(),
        }
    }
}

impl Plugin for Hexify {
    fn name(&self) -> &str {
        HEXIFY
    }

    /// Encodes one whole message.
    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }

        let len = input
            .len()
            .checked_mul(2)
            .ok_or_else(|| PluginError::runtime(HEXIFY, "encoded message would overflow usize"))?;
        // Shrinks as well as grows, so a short message following a long one
        // cannot trail the tail of its predecessor.
        self.out.resize(len, 0);
        hex::encode_to_slice(input, &mut self.out).map_err(encode_error)?;

        if self.case == Case::Uppercase {
            self.out.make_ascii_uppercase();
        }

        ctx.forward(&self.out);

        Ok(())
    }

    /// Safe on a datagram path: one message in, one message out, no state
    /// carried between calls.
    fn boundaries(&self) -> Boundaries {
        Boundaries::Preserve
    }
}

#[derive(Default)]
pub struct Unhexify {
    /// Reused across calls, so a steady stream settles on one allocation.
    out: Vec<u8>,
}

impl Plugin for Unhexify {
    fn name(&self) -> &str {
        UNHEXIFY
    }

    /// Decodes one whole message.
    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }

        // `hex` would reject this too, but with a message about digits rather
        // than about the framing that is actually missing.
        if !input.len().is_multiple_of(2) {
            return Err(PluginError::runtime(UNHEXIFY, MISFRAMED));
        }

        self.out.resize(input.len() / 2, 0);
        hex::decode_to_slice(input, &mut self.out).map_err(decode_error)?;
        ctx.forward(&self.out);

        Ok(())
    }

    /// Safe on a datagram path: one message in, one message out, no state
    /// carried between calls.
    fn boundaries(&self) -> Boundaries {
        Boundaries::Preserve
    }

    fn needs(&self) -> Needs {
        Needs::Upstream
    }
}

pub struct HexifyFactory;

impl PluginFactory for HexifyFactory {
    fn name(&self) -> &str {
        HEXIFY
    }

    fn description(&self) -> &str {
        "hex-encode this direction, one message per chunk"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: HexifyConfig = ctx.config()?;

        Ok(Stage::filter(Hexify::new(config.case)))
    }
}

pub struct UnhexifyFactory;

impl PluginFactory for UnhexifyFactory {
    fn name(&self) -> &str {
        UNHEXIFY
    }

    fn description(&self) -> &str {
        "hex-decode this direction, one message per chunk"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        // Parsed and discarded: the only job is to reject options that do not
        // exist.
        let _: UnhexifyConfig = ctx.config()?;

        Ok(Stage::filter(Unhexify::default()))
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

    /// 43 bytes, and odd, so a length bug shows up as a shifted nibble rather
    /// than as nothing at all.
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
            Stage::External(_) => unreachable!("hex stages are filters"),
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
        let mut encoder = build(&HexifyFactory, json!({}));
        let mut decoder = build(&UnhexifyFactory, json!({}));

        let wire = feed(encoder.as_mut(), SAMPLE);
        assert_eq!(feed(decoder.as_mut(), &wire), SAMPLE);
    }

    #[test]
    fn round_trips_every_length() {
        let mut encoder = build(&HexifyFactory, json!({}));
        let mut decoder = build(&UnhexifyFactory, json!({}));

        for len in 0..=64usize {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();

            let wire = feed(encoder.as_mut(), &plain);
            assert_eq!(feed(decoder.as_mut(), &wire), plain, "length {len}");
        }
    }

    #[test]
    fn round_trips_every_byte_value() {
        let plain: Vec<u8> = (0..=u8::MAX).collect();

        let mut encoder = build(&HexifyFactory, json!({}));
        let mut decoder = build(&UnhexifyFactory, json!({}));

        let wire = feed(encoder.as_mut(), &plain);
        assert_eq!(feed(decoder.as_mut(), &wire), plain);
    }

    /// The contract: a call is a message, so each one encodes independently
    /// and each one is decodable on its own.
    #[test]
    fn each_call_is_a_self_contained_message() {
        let mut encoder = build(&HexifyFactory, json!({}));
        let mut decoder = build(&UnhexifyFactory, json!({}));

        let first = feed(encoder.as_mut(), b"one");
        let second = feed(encoder.as_mut(), b"two");

        assert_eq!(first, b"6f6e65");
        assert_eq!(second, b"74776f");

        // Nothing carries over, so the second decodes without the first.
        assert_eq!(feed(decoder.as_mut(), &second), b"two");
        assert_eq!(feed(decoder.as_mut(), &first), b"one");
    }

    /// The scratch buffer outlives the call, so a short message after a long
    /// one is where a missed truncation would show.
    #[test]
    fn a_short_message_does_not_trail_a_longer_one() {
        let mut encoder = build(&HexifyFactory, json!({}));
        let mut decoder = build(&UnhexifyFactory, json!({}));

        feed(encoder.as_mut(), SAMPLE);
        assert_eq!(feed(encoder.as_mut(), b"z"), b"7a");

        feed(decoder.as_mut(), b"6f6e6520747769636520");
        assert_eq!(feed(decoder.as_mut(), b"7a"), b"z");
    }

    #[test]
    fn encoding_doubles_the_message() {
        let mut encoder = build(&HexifyFactory, json!({}));

        assert_eq!(feed(encoder.as_mut(), SAMPLE).len(), SAMPLE.len() * 2);
    }

    #[test]
    fn empty_message_emits_nothing() {
        let mut encoder = build(&HexifyFactory, json!({}));
        let mut decoder = build(&UnhexifyFactory, json!({}));

        assert!(feed(encoder.as_mut(), b"").is_empty());
        assert!(feed(decoder.as_mut(), b"").is_empty());
    }

    /// One call in, one unit out: empty bounds is what says so, and asserting
    /// on the bytes alone would not catch a stage that split them.
    #[test]
    fn a_message_leaves_as_one_unit() {
        let mut plugin = build(&HexifyFactory, json!({}));
        let meta = meta();
        let mut emission = Emission::new();
        let mut sink = Silent;
        {
            let mut ctx = Ctx::new(&meta, HEXIFY, SAMPLE, &mut emission, &mut sink);
            plugin.on_bytes(&mut ctx, SAMPLE).expect("on_bytes");
        }

        assert_eq!(emission.bounds(), &[] as &[usize]);
    }

    #[test]
    fn uppercase_is_opt_in() {
        let mut lower = build(&HexifyFactory, json!({}));
        let mut upper = build(&HexifyFactory, json!({ "case": "upper" }));

        assert_eq!(feed(lower.as_mut(), b"\xde\xad\xbe\xef"), b"deadbeef");
        assert_eq!(feed(upper.as_mut(), b"\xde\xad\xbe\xef"), b"DEADBEEF");
    }

    #[test]
    fn case_is_spelled_either_way() {
        for spelling in ["upper", "uppercase"] {
            let mut encoder = build(&HexifyFactory, json!({ "case": spelling }));
            assert_eq!(feed(encoder.as_mut(), b"\xff"), b"FF", "case={spelling}");
        }

        for spelling in ["lower", "lowercase"] {
            let mut encoder = build(&HexifyFactory, json!({ "case": spelling }));
            assert_eq!(feed(encoder.as_mut(), b"\xff"), b"ff", "case={spelling}");
        }
    }

    /// Unlike the base64 `alphabet`, `case` does not have to agree across a
    /// hop, and the docs say so.
    #[test]
    fn decoding_accepts_either_case_and_a_mixture() {
        let mut decoder = build(&UnhexifyFactory, json!({}));

        for wire in [b"deadbeef", b"DEADBEEF", b"DeAdBeEf"] {
            assert_eq!(
                feed(decoder.as_mut(), wire),
                b"\xde\xad\xbe\xef",
                "wire {}",
                String::from_utf8_lossy(wire),
            );
        }
    }

    /// Splitting a message mid-byte is the mistake this stage cannot fix, so
    /// it has to name the fix instead.
    #[test]
    fn decoder_rejects_a_message_cut_mid_byte() {
        let mut encoder = build(&HexifyFactory, json!({}));
        let wire = feed(encoder.as_mut(), SAMPLE);
        let mut decoder = build(&UnhexifyFactory, json!({}));

        let err = try_feed(decoder.as_mut(), &wire[..wire.len() - 1])
            .expect_err("half a byte must not decode");
        assert!(
            err.to_string().contains("unframe"),
            "the error must point at the framing stage: {err}",
        );
    }

    #[test]
    fn decoder_rejects_characters_outside_the_alphabet() {
        let mut decoder = build(&UnhexifyFactory, json!({}));

        assert!(try_feed(decoder.as_mut(), b"dead*eef").is_err());
        // Whitespace included: a delimiter is framing, not payload.
        assert!(try_feed(decoder.as_mut(), b"de ad").is_err());
        // And no `0x` prefix, which is hex for people rather than for wires.
        assert!(try_feed(decoder.as_mut(), b"0xdead").is_err());
    }

    /// A decode failure is not the encoder's fault, and an operator reading
    /// the log should not be sent to the wrong stage.
    #[test]
    fn a_decode_failure_names_unhexify() {
        let mut decoder = build(&UnhexifyFactory, json!({}));

        let err = try_feed(decoder.as_mut(), b"zz").expect_err("not hex");
        let message = err.to_string();
        assert!(message.contains(UNHEXIFY), "{message}");
    }

    /// The guide says both stages may sit on a datagram path, which is only
    /// true if they say so themselves: the trait defaults to fusing.
    #[test]
    fn both_stages_preserve_boundaries() {
        let encoder = build(&HexifyFactory, json!({}));
        let decoder = build(&UnhexifyFactory, json!({}));

        assert_eq!(encoder.boundaries(), Boundaries::Preserve);
        assert_eq!(decoder.boundaries(), Boundaries::Preserve);
    }

    #[test]
    fn rejects_unknown_config_keys() {
        for (name, factory) in [
            (HEXIFY, &HexifyFactory as &dyn PluginFactory),
            (UNHEXIFY, &UnhexifyFactory as &dyn PluginFactory),
        ] {
            // `alphabet` is the base64 option, and the likeliest thing to
            // arrive here by habit.
            let map = json!({ "alphabet": "url-safe" })
                .as_object()
                .unwrap()
                .clone();
            let meta = meta();
            let mut host = NullHost;
            let stage = StageInfo {
                index: 0,
                total: 1,
                name,
                upstream: "src",
                downstream: "sink",
            };
            let mut ctx = BuildCtx::new(name, &map, &meta, stage, &mut host);

            assert!(factory.build(&mut ctx).is_err(), "{name}");
        }
    }
}
