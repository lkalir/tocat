//! `hash` - compute the digest of bytes that traverse this stage
//!
//! ```toml
//! [[plugin]]
//! name = "hash"
//! algo = "sha256"
//! file = "digests.txt"
//! ```
//!
//! ```console
//! $ tocat file:big.iso hash tcp:host:9000
//! $ tocat -f tcp-listen:9000,fork -t file:capture.bin -p 'hash,algo=blake3'
//! ```
//!
//! Like [`tee`](crate::tee) and [`rate`](crate::rate) this never touches the
//! payload: every chunk is passed through untouched and the digest is folded in
//! on the way past. It can therefore sit anywhere in a chain, including on a
//! datagram path.
//!
//! # What is reported, and when
//!
//! `summary`, on by default, writes one line at end of stream: the digest of
//! everything that crossed the stage.
//!
//! `chunks` writes one line per chunk, each the digest of *that chunk alone*
//! rather than a running value. It is for locating where two streams diverge,
//! and it costs a finalisation and a write per chunk, so it is off by default.
//!
//! Lines follow the `sha256sum` shape, two spaces between the digest and what
//! it describes, then the algorithm and the hop this stage sits on:
//!
//! ```text
//! ba78…15ad  stream (sha256) [tcp://example.com:80_10.0.0.4:52134 -> STDIO | hash]
//! ```
//!
//! # There is not always an end of stream
//!
//! `summary` needs one, and two paths never reach it: a datagram source, and a
//! [`pipe`] held open across producers. On those, `chunks` is the only thing
//! that will ever report. That is a property of the path rather than of this
//! stage, and it is the same reason `rate` reports on a timer as well as at the
//! end.

use std::{fmt::Write, path::PathBuf};

use digest::{Digest as Dig, DynDigest};
use serde::{Deserialize, Serialize};
use tocat_api::{
    BuildCtx, ChannelId, ChannelTarget, Ctx, Plugin, PluginError, PluginFactory, Result, Stage,
};

pub const NAME: &str = "hash";

/// Which digest to compute.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Algorithm {
    Md5,
    Sha1,
    #[serde(alias = "sha2-224")]
    Sha224,
    #[serde(alias = "sha2", alias = "sha2-256")]
    #[default]
    Sha256,
    #[serde(alias = "sha2-384")]
    Sha384,
    #[serde(alias = "sha2-512")]
    Sha512,
    Sha3_224,
    #[serde(alias = "sha3")]
    Sha3_256,
    Sha3_384,
    Sha3_512,
    #[serde(alias = "blake2b")]
    Blake2,
    #[serde(alias = "blake")]
    Blake3,
}

/// Wrapper around hashers
///
/// This is necessary because blake3 doesn't implement RustCrypto's DynDigest
/// trait, so we must provide a common wrapper ourselves
enum Hasher {
    Dynamic(Box<dyn DynDigest + Send>),
    Blake3(Box<blake3::Hasher>),
}

impl Hasher {
    /// Accumulate data in the hasher
    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Dynamic(digest) => digest.update(data),
            Self::Blake3(hasher) => {
                hasher.update(data);
            }
        }
    }

    /// Compute the digest and reset the hasher
    fn finish_reset(&mut self) -> Box<[u8]> {
        match self {
            Self::Dynamic(digest) => digest.finalize_reset(),
            Self::Blake3(hasher) => {
                let hash = hasher.finalize();
                hasher.reset();
                hash.as_slice().into()
            }
        }
    }
}

impl Algorithm {
    fn hasher(&self) -> Hasher {
        match self {
            Algorithm::Md5 => Hasher::Dynamic(Box::new(md5::Md5::new())),
            Algorithm::Sha1 => Hasher::Dynamic(Box::new(sha1::Sha1::new())),
            Algorithm::Sha224 => Hasher::Dynamic(Box::new(sha2::Sha224::new())),
            Algorithm::Sha256 => Hasher::Dynamic(Box::new(sha2::Sha256::new())),
            Algorithm::Sha384 => Hasher::Dynamic(Box::new(sha2::Sha384::new())),
            Algorithm::Sha512 => Hasher::Dynamic(Box::new(sha2::Sha512::new())),
            Algorithm::Sha3_224 => Hasher::Dynamic(Box::new(sha3::Sha3_224::new())),
            Algorithm::Sha3_256 => Hasher::Dynamic(Box::new(sha3::Sha3_256::new())),
            Algorithm::Sha3_384 => Hasher::Dynamic(Box::new(sha3::Sha3_384::new())),
            Algorithm::Sha3_512 => Hasher::Dynamic(Box::new(sha3::Sha3_512::new())),
            Algorithm::Blake2 => Hasher::Dynamic(Box::new(blake2::Blake2b512::new())),
            Algorithm::Blake3 => Hasher::Blake3(Box::new(blake3::Hasher::new())),
        }
    }
}

impl std::fmt::Display for Algorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Algorithm::Md5 => write!(f, "md5"),
            Algorithm::Sha1 => write!(f, "sha1"),
            Algorithm::Sha224 => write!(f, "sha224"),
            Algorithm::Sha256 => write!(f, "sha256"),
            Algorithm::Sha384 => write!(f, "sha384"),
            Algorithm::Sha512 => write!(f, "sha512"),
            Algorithm::Sha3_224 => write!(f, "sha3-224"),
            Algorithm::Sha3_256 => write!(f, "sha3-256"),
            Algorithm::Sha3_384 => write!(f, "sha3-384"),
            Algorithm::Sha3_512 => write!(f, "sha3-512"),
            Algorithm::Blake2 => write!(f, "blake2"),
            Algorithm::Blake3 => write!(f, "blake3"),
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct HashConfig {
    /// Algorithm to use
    #[serde(default, alias = "algo", alias = "alg", alias = "hasher")]
    pub algorithm: Algorithm,

    /// Individually hash each inbound chunk
    #[serde(default)]
    pub chunks: bool,

    /// Hash entire stream of data
    #[serde(default = "default_true")]
    pub summary: bool,

    /// Write to specific path
    #[serde(default)]
    pub file: Option<PathBuf>,

    /// Append instead of truncate
    #[serde(default = "default_true")]
    pub append: bool,
}

impl HashConfig {
    /// Resolve `file` to a channel target.
    ///
    /// stdout is rejected rather than supported: on a stdio endpoint it carries
    /// relay payload, and mixing a dump into it corrupts the transfer.
    pub fn target(&self) -> Result<ChannelTarget> {
        let Some(path) = &self.file else {
            return Ok(ChannelTarget::Stderr);
        };

        match path.to_str() {
            Some("-" | "stderr" | "/dev/stderr" | "/dev/fd/2") => Ok(ChannelTarget::Stderr),
            Some("stdout" | "/dev/stdout" | "/dev/fd/1") => Err(PluginError::config(
                NAME,
                "refusing to write to stdout, it may carry relay payload; use `-` for stderr",
            )),
            _ => Ok(ChannelTarget::File {
                path: path.clone(),
                append: self.append,
            }),
        }
    }
}

pub struct Hash {
    algorithm: Algorithm,
    chunk: Option<Hasher>,
    stream: Option<Hasher>,
    channel: ChannelId,
    chunks_seen: u64,
    label: String,
    line: String,
}

impl Hash {
    fn write_line(&mut self, ctx: &mut Ctx<'_>, digest: &[u8], what: std::fmt::Arguments<'_>) {
        self.line.clear();

        for byte in digest {
            const HEX: [u8; 16] = *b"0123456789abcdef";

            self.line.push(HEX[usize::from(byte >> 4)] as char);
            self.line.push(HEX[usize::from(byte & 0x0f)] as char);
        }

        let _ = writeln!(
            self.line,
            "  {what} ({algorithm}) [{label}]",
            algorithm = self.algorithm,
            label = self.label
        );

        ctx.side_write(self.channel, self.line.as_bytes());
    }
}

impl Plugin for Hash {
    fn name(&self) -> &str {
        NAME
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        ctx.pass_through();

        if let Some(stream) = &mut self.stream {
            stream.update(input);
        }

        let digest = self.chunk.as_mut().map(|chunk| {
            chunk.update(input);
            chunk.finish_reset()
        });

        if let Some(digest) = digest {
            self.chunks_seen += 1;

            let n = self.chunks_seen;
            self.write_line(ctx, &digest, format_args!("chunk {n}"));
        }

        Ok(())
    }

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        let digest = self.stream.as_mut().map(Hasher::finish_reset);

        if let Some(digest) = digest {
            self.write_line(ctx, &digest, format_args!("stream"));
        }

        Ok(())
    }

    /// Safe on a datagram path since hashing a message does not change it
    fn datagram_safe(&self) -> bool {
        true
    }
}

pub struct HashFactory;

impl PluginFactory for HashFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "digest the bytes crossing this stage"
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: HashConfig = ctx.config()?;

        if !config.summary && !config.chunks {
            return Err(PluginError::config(
                NAME,
                "summary and chunks are both off, so this stage would do nothing",
            ));
        }

        let channel = ctx.open_channel(config.target()?)?;
        let stage = ctx.stage();
        let label = format!("{} | {}", stage.label(), stage.name);

        Ok(Stage::filter(Hash {
            algorithm: config.algorithm,
            chunk: config.chunks.then(|| config.algorithm.hasher()),
            stream: config.summary.then(|| config.algorithm.hasher()),
            channel,
            chunks_seen: 0,
            label,
            line: String::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tocat_api::{
        ChannelId, ChannelTarget, Direction, EffectSink, Emission, Emit, HostBuilder, LogLevel,
        PipelineMeta, Result as PluginResult, StageInfo,
    };

    use super::*;

    /// Digests are written to a side channel rather than emitted, so this is
    /// what the assertions are about.
    #[derive(Default)]
    struct Recorder {
        written: Vec<String>,
    }

    impl EffectSink for Recorder {
        fn write(&mut self, _channel: ChannelId, bytes: &[u8]) {
            self.written
                .push(String::from_utf8_lossy(bytes).into_owned());
        }

        fn log(&mut self, _level: LogLevel, _stage: &str, _message: &str) {}
    }

    struct NullHost;

    impl HostBuilder for NullHost {
        fn open_channel(&mut self, _target: ChannelTarget) -> PluginResult<ChannelId> {
            Ok(ChannelId(0))
        }
    }

    fn meta() -> PipelineMeta {
        PipelineMeta::new(Direction::SourceToSink, "src", "sink")
    }

    fn stage() -> StageInfo<'static> {
        StageInfo {
            index: 0,
            total: 1,
            name: NAME,
            upstream: "src",
            downstream: "sink",
        }
    }

    fn try_build(config: serde_json::Value) -> PluginResult<Box<dyn Plugin>> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let mut ctx = BuildCtx::new(NAME, &map, &meta, stage(), &mut host);

        match HashFactory.build(&mut ctx)? {
            Stage::Filter(plugin) => Ok(plugin),
            Stage::External(_) => unreachable!("hash is a filter"),
        }
    }

    fn build(config: serde_json::Value) -> Box<dyn Plugin> {
        try_build(config).expect("build")
    }

    fn build_config(config: serde_json::Value) -> HashConfig {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let ctx = BuildCtx::new(NAME, &map, &meta, stage(), &mut host);

        ctx.config().expect("config")
    }

    /// One chunk, returning the emission so a test can assert the payload was
    /// never materialised.
    fn feed(plugin: &mut dyn Plugin, sink: &mut Recorder, input: &[u8]) -> Emission {
        let meta = meta();
        let mut emission = Emission::new();

        {
            let mut ctx = Ctx::new(&meta, NAME, input, &mut emission, sink);
            plugin.on_bytes(&mut ctx, input).expect("on_bytes");
        }

        emission
    }

    fn eof(plugin: &mut dyn Plugin, sink: &mut Recorder) {
        let meta = meta();
        let mut emission = Emission::new();

        {
            let mut ctx = Ctx::new(&meta, NAME, &[], &mut emission, sink);
            plugin.on_eof(&mut ctx).expect("on_eof");
        }
    }

    /// The digest out of a line, which is everything before the two spaces.
    fn digest_of(line: &str) -> &str {
        line.split("  ").next().expect("a digest")
    }

    #[test]
    fn the_payload_is_never_materialised() {
        let mut plugin = build(json!({}));
        let mut sink = Recorder::default();

        let emission = feed(&mut *plugin, &mut sink, b"abc");

        assert_eq!(emission.emit(), Emit::Passthrough);
        assert!(emission.bytes().is_empty(), "hash must not copy the stream");
    }

    #[test]
    fn the_summary_is_the_digest_of_everything() {
        let mut plugin = build(json!({"algo": "sha256"}));
        let mut sink = Recorder::default();

        feed(&mut *plugin, &mut sink, b"a");
        feed(&mut *plugin, &mut sink, b"bc");
        assert!(sink.written.is_empty(), "nothing is reported until the end");

        eof(&mut *plugin, &mut sink);

        assert_eq!(sink.written.len(), 1);
        assert_eq!(
            digest_of(&sink.written[0]),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256 of \"abc\", however it was chunked",
        );
        assert!(sink.written[0].contains("stream (sha256)"));
        assert!(sink.written[0].ends_with("[src -> sink | hash]\n"));
    }

    #[test]
    fn the_algorithm_is_the_one_that_was_asked_for() {
        let mut plugin = build(json!({"algo": "sha1"}));
        let mut sink = Recorder::default();

        feed(&mut *plugin, &mut sink, b"abc");
        eof(&mut *plugin, &mut sink);

        assert_eq!(
            digest_of(&sink.written[0]),
            "a9993e364706816aba3e25717850c26c9cd0d89d",
        );
    }

    #[test]
    fn chunks_are_digested_one_at_a_time() {
        let mut plugin = build(json!({"chunks": true, "summary": false}));
        let mut sink = Recorder::default();

        feed(&mut *plugin, &mut sink, b"abc");
        feed(&mut *plugin, &mut sink, b"def");

        assert_eq!(sink.written.len(), 2);
        assert!(sink.written[0].contains("chunk 1"), "chunks are one-based");
        assert!(sink.written[1].contains("chunk 2"));
        assert_ne!(
            digest_of(&sink.written[0]),
            digest_of(&sink.written[1]),
            "each line is that chunk alone, not a running value",
        );
    }

    /// The one relationship between the two modes that has to hold: with a
    /// single chunk, that chunk's digest is the stream's.
    #[test]
    fn one_chunk_digests_the_same_either_way() {
        let mut plugin = build(json!({"chunks": true}));
        let mut sink = Recorder::default();

        feed(&mut *plugin, &mut sink, b"abc");
        eof(&mut *plugin, &mut sink);

        assert_eq!(sink.written.len(), 2);
        assert_eq!(digest_of(&sink.written[0]), digest_of(&sink.written[1]));
    }

    #[test]
    fn a_hasher_left_over_from_one_stream_does_not_taint_the_next() {
        let mut plugin = build(json!({"chunks": true, "summary": false}));
        let mut sink = Recorder::default();

        feed(&mut *plugin, &mut sink, b"abc");
        feed(&mut *plugin, &mut sink, b"abc");

        assert_eq!(
            digest_of(&sink.written[0]),
            digest_of(&sink.written[1]),
            "finish_reset must leave the hasher empty",
        );
    }

    #[test]
    fn the_algorithm_is_spelled_however_you_like() {
        for spelling in ["sha256", "SHA-256", "sha_256", "sha2"] {
            assert_eq!(
                build_config(json!({ "algo": spelling })).algorithm,
                Algorithm::Sha256,
                "{spelling} did not name sha256",
            );
        }

        assert_eq!(
            build_config(json!({"algo": "sha3-512"})).algorithm,
            Algorithm::Sha3_512,
        );
        assert_eq!(
            build_config(json!({"algo": "blake"})).algorithm,
            Algorithm::Blake3,
        );
    }

    #[test]
    fn the_defaults_are_a_stream_digest_on_stderr() {
        let config = build_config(json!({}));

        assert_eq!(config.algorithm, Algorithm::Sha256);
        assert!(config.summary);
        assert!(!config.chunks);
        assert!(config.append);
        assert!(matches!(config.target(), Ok(ChannelTarget::Stderr)));
    }

    #[test]
    fn an_unknown_algorithm_or_option_is_rejected() {
        assert!(try_build(json!({"algo": "crc32"})).is_err());
        assert!(try_build(json!({"algorithm": "sha256", "bytes": 8})).is_err());
    }

    #[test]
    fn a_stage_that_would_report_nothing_is_rejected() {
        assert!(
            try_build(json!({"summary": false})).is_err(),
            "neither summary nor chunks is a typo, not an intent",
        );
    }

    #[test]
    fn stdout_is_refused_because_it_may_carry_payload() {
        assert!(try_build(json!({"file": "stdout"})).is_err());
        assert!(try_build(json!({"file": "-"})).is_ok(), "`-` is stderr");
    }
}
