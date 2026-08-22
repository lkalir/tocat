//! `encrypt` / `decrypt`: symmetric encryption stages.
//!
//! # What goes on the wire
//!
//! Two shapes, chosen with `mode`, and the whole design follows from which one
//! a cipher can support.
//!
//! `record` is one self-contained unit per call: `nonce || ciphertext || tag`,
//! with a fresh nonce drawn for every record. Nothing is carried between
//! calls, so a lost or reordered message costs exactly that message. The
//! nonce is on the wire because a receiver cannot otherwise know it, and it is
//! fresh per record because a repeated nonce under one key is catastrophic for
//! the AEAD ciphers and merely fatal for the rest.
//!
//! `stream` is one session across the path: `nonce || ciphertext...`, with the
//! nonce emitted once ahead of the first byte and the mode's state carried
//! across calls. Nothing frames it, so it needs no [`crate::frame`] stage, and
//! it desynchronises permanently if any byte is lost.
//!
//! # Why AEAD is record only
//!
//! A session that produced one tag at end of stream would have to release
//! plaintext before that tag arrived, which makes the authentication
//! decorative for a relay: the tampered bytes have already been forwarded by
//! the time the tag says so. The alternative, buffering the transfer, is not
//! something a relay may do. The RustCrypto AEADs also expose no incremental
//! API to build such a session from. So [`Family::Aead`] is refused in stream
//! mode at build time, and an AEAD cipher on a byte stream declares
//! [`Needs::Downstream`] (encrypt) or [`Needs::Upstream`] (decrypt), which the
//! host answers by demanding `frame` and `unframe`.
//!
//! # Boundaries
//!
//! Record mode is [`Boundaries::Preserve`]: one call in, one record out. The
//! record it writes has to arrive whole, hence the `Needs`.
//!
//! Stream mode is [`Boundaries::Fuse`], which claims nothing. A block mode
//! genuinely buffers across calls, and while a keystream mode does not, what
//! it emits is only meaningful in order and entire. Declaring `Fuse` is what
//! makes the host warn when a stream session is pointed at a datagram sink,
//! which is exactly the configuration that silently corrupts.
//!
//! # How dispatch works
//!
//! Every mode is driven through one pair of traits, [`Seal`] and [`Open`],
//! with three calls: `start` opens a session with a nonce, `update`
//! transforms bytes, `finish` closes it. Record mode calls all three per
//! message; stream mode calls `start` once, `update` per chunk and `finish`
//! at end of stream. The stage owns the nonces, the framing and the rotation
//! budget; an implementation owns nothing but cipher state. That is what
//! keeps the cipher table to one match arm per cipher per direction rather
//! than one per cipher per mode.
//!
//! [`Cipher::spec`] is the single table the rest of the module reads: key
//! length, nonce length, tag length, block size and family. Anything that
//! decides something per cipher decides it from there.
//!
//! # Cost
//!
//! Both stages default to [`Execution::Detached`], on the same grounds as
//! `compress`: enough work per byte to be worth a task of its own.
//!
//! Encryption copies once, into the stage's reusable buffer, and works in
//! place from there. Decryption of a block mode copies twice, since it must
//! accumulate whole blocks and hold the last one back until it knows it is
//! not the padded tail. AEAD instances are built once at construction, so the
//! key schedule and the authenticator's tables are not per record; the block
//! and keystream modes are rebuilt per session, which in record mode means one
//! key schedule per record.
//!
//! Nonces come from a small pool of OS randomness rather than a `getrandom`
//! call per record. The pool is per instance, which is per direction per
//! connection, so it is sized in hundreds of bytes rather than kilobytes.
//!
//! Three things about the throughput of this module are worth knowing before
//! reading a profile, because each looks like a bug and is not.
//!
//! The AEAD ciphers cost about twice their underlying keystream, because the
//! encryption and authentication passes are separate rather than interleaved.
//! That is upstream's doing rather than this module's: `aes-gcm` carries a
//! `TODO` to interleave encryption with GHASH, and until it does, the
//! authentication pass costs roughly what the cipher pass does.
//!
//! CBC encryption and OFB are latency bound, not throughput bound. Each block
//! feeds the next, so no CPU can overlap them, and they land several times
//! slower than CTR under the same key. This follows from the modes and is not
//! something an implementation can recover. CBC decryption has no such chain
//! and runs with the parallel modes, so a CBC path is asymmetric.
//!
//! Everything else scales with the cipher's own work, which is a function of
//! the machine. The costs this module adds, the copies and the per-record
//! setup above, are small against any of them at stream buffer sizes, and are
//! the part that matters on small datagrams.

mod macros;

#[cfg(test)]
mod vectors;

use std::{
    path::{Path, PathBuf},
    str,
};

use ::base64::prelude::{BASE64_STANDARD, Engine as _};
use aead::{
    AeadCore, AeadInOut, Nonce,
    consts::{U12, U16},
};
use aes::{Aes128, Aes192, Aes256};
use aes_gcm::{Aes128Gcm, Aes256Gcm, AesGcm};
use aes_gcm_siv::{Aes128GcmSiv, Aes256GcmSiv, AesGcmSiv};
use aes_siv::{Aes128SivAead, Aes256SivAead};
use aria::{Aria128, Aria192, Aria256};
use ascon_aead::AsconAead128;
use camellia::{Camellia128, Camellia192, Camellia256};
use ccm::Ccm;
use cfb_mode::{BufDecryptor, BufEncryptor};
use chacha20::{ChaCha20, XChaCha20};
use chacha20poly1305::{ChaCha20Poly1305, XChaCha20Poly1305};
use cipher::{
    Array, Block, BlockCipherEncrypt, BlockModeDecrypt, BlockModeEncrypt, BlockSizeUser,
    InvalidLength, Iv, KeyInit, KeyIvInit, StreamCipher, StreamCipherError,
    block_padding::{Padding as _, Pkcs7},
    typenum::Unsigned,
};
use ctr::{Ctr32BE, Ctr64BE, Ctr128BE};
use des::{Des, TdesEde3};
use kuznyechik::Kuznyechik;
use macros::ciphers;
use magma::Magma;
use ocb3::Ocb3;
use ofb::Ofb;
use salsa20::{Salsa20, XSalsa20};
use serde::{Deserialize, Serialize};
use sm4::Sm4;
use tocat_api::{
    Boundaries, BuildCtx, ByteSize, ChannelId, ChannelTarget, Ctx, Execution, LogLevel, Needs,
    Plugin, PluginError, PluginFactory, Result, Stage,
};

pub const ENCRYPT: &str = "encrypt";
pub const DECRYPT: &str = "decrypt";

/// The widest nonce any cipher here takes, so a stage holds one in a fixed
/// array rather than an allocation.
const MAX_NONCE: usize = 24;

/// Bytes of OS randomness drawn at a time.
///
/// One `getrandom` call per record would cost more than encrypting a small
/// record. This trades that for half a kilobyte per instance, which under
/// `fork` is half a kilobyte per connection per direction.
const RANDOM_POOL: usize = 512;

/// Named once, because it is the same answer for a record cut short in
/// transit and one a peer never finished writing.
const TRUNCATED: &str = concat!(
    "record is shorter than its own header: it was cut in transit, or the ",
    "framing above this stage does not match the sender's",
);

/// What a session tried to say when it could not open.
const NO_SESSION: &str = "no session is open, which is a bug in the stage rather than in the data";

/// How each cipher is put together, and therefore what may be asked of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// Authenticated: every record carries a tag that decryption verifies.
    /// One whole message per call, with no incremental API to do otherwise.
    Aead,
    /// A block mode: work happens a block at a time and a partial block has
    /// to be padded.
    Block,
    /// A keystream mode: ciphertext is the payload XOR a keystream, so
    /// lengths are unchanged and any split is safe.
    Keystream,
}

/// Everything the rest of the module needs to know about a cipher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Spec {
    /// The spelling used in messages, which is also the declared one.
    name: &'static str,
    family: Family,
    key: usize,
    /// Nonce or IV bytes, which is what precedes a record or a session.
    nonce: usize,
    /// Tag bytes, zero outside [`Family::Aead`].
    tag: usize,
    /// Block bytes, zero outside [`Family::Block`]. The stage needs this to
    /// size a rotation budget; the cipher implementations read their own.
    block: usize,
    /// Standard
    standard: Option<&'static str>,
}

/// The document that tells another implementation what these bytes are, or
/// `None` where nothing does. Answers what the bytes are, not whether the
/// combination is wise, so a withdrawn standard still counts.
///
/// The empty string is how a row says nothing specifies it, since a macro
/// cannot take a bare `none` where it expects a literal.
const fn standard(document: &'static str) -> Option<&'static str> {
    if document.is_empty() {
        None
    } else {
        Some(document)
    }
}

const fn block(
    name: &'static str,
    key: usize,
    iv: usize,
    block: usize,
    document: &'static str,
) -> Spec {
    Spec {
        name,
        family: Family::Block,
        key,
        nonce: iv,
        tag: 0,
        block,
        standard: standard(document),
    }
}

/// A keystream mode's IV is the underlying cipher's block, so it is given
/// here rather than assumed: a 64-bit block cipher takes an 8-byte IV, and a
/// GOST session transmits half of one.
const fn keystream(name: &'static str, key: usize, iv: usize, document: &'static str) -> Spec {
    Spec {
        name,
        family: Family::Keystream,
        key,
        nonce: iv,
        tag: 0,
        block: 0,
        standard: standard(document),
    }
}

const fn aead(name: &'static str, key: usize, nonce: usize, document: &'static str) -> Spec {
    Spec {
        name,
        family: Family::Aead,
        key,
        nonce,
        tag: 16,
        block: 0,
        standard: standard(document),
    }
}

ciphers! {
    block with iv {
        // CBC
        Aes128Cbc = "aes-128-cbc", key 16, iv 16, block 16, standard "NIST SP 800-38A", cbc::Encryptor<Aes128>, cbc::Decryptor<Aes128>;
        Aes192Cbc = "aes-192-cbc", key 24, iv 16, block 16, standard "NIST SP 800-38A", cbc::Encryptor<Aes192>, cbc::Decryptor<Aes192>;
        Aes256Cbc = "aes-256-cbc" | "aes-cbc", key 32, iv 16, block 16, standard "NIST SP 800-38A", cbc::Encryptor<Aes256>, cbc::Decryptor<Aes256>;

        Aria128Cbc = "aria-128-cbc", key 16, iv 16, block 16, standard "NIST SP 800-38A + RFC 5794", cbc::Encryptor<Aria128>, cbc::Decryptor<Aria128>;
        Aria192Cbc = "aria-192-cbc", key 24, iv 16, block 16, standard "NIST SP 800-38A + RFC 5794", cbc::Encryptor<Aria192>, cbc::Decryptor<Aria192>;
        Aria256Cbc = "aria-256-cbc" | "aria-cbc", key 32, iv 16, block 16, standard "NIST SP 800-38A + RFC 5794", cbc::Encryptor<Aria256>, cbc::Decryptor<Aria256>;

        Camellia128Cbc = "camellia-128-cbc", key 16, iv 16, block 16, standard "NIST SP 800-38A + RFC 3713", cbc::Encryptor<Camellia128>, cbc::Decryptor<Camellia128>;
        Camellia192Cbc = "camellia-192-cbc", key 24, iv 16, block 16, standard "NIST SP 800-38A + RFC 3713", cbc::Encryptor<Camellia192>, cbc::Decryptor<Camellia192>;
        Camellia256Cbc = "camellia-256-cbc" | "camellia-cbc", key 32, iv 16, block 16, standard "NIST SP 800-38A + RFC 3713", cbc::Encryptor<Camellia256>, cbc::Decryptor<Camellia256>;

        MagmaCbc = "magma-cbc", key 32, iv 8, block 8, standard "NIST SP 800-38A + GOST R 34.12-2015", cbc::Encryptor<Magma>, cbc::Decryptor<Magma>;
        KuznyechikCbc = "kuznyechik-cbc" | "grasshopper-cbc", key 32, iv 16, block 16, standard "NIST SP 800-38A + GOST R 34.12-2015", cbc::Encryptor<Kuznyechik>, cbc::Decryptor<Kuznyechik>;

        Sm4Cbc = "sm4-cbc", key 16, iv 16, block 16, standard "NIST SP 800-38A + GB/T 32907-2016", cbc::Encryptor<Sm4>, cbc::Decryptor<Sm4>;

        DesCbc = "des-cbc", key 8, iv 8, block 8, standard "NIST SP 800-38A + FIPS 46-3 (withdrawn)", cbc::Encryptor<Des>, cbc::Decryptor<Des>;
        TdesCbc = "des-ede3-cbc" | "3des-cbc" | "tdes-cbc", key 24, iv 8, block 8, standard "NIST SP 800-38A + NIST SP 800-67", cbc::Encryptor<TdesEde3>, cbc::Decryptor<TdesEde3>;
    }

    block without iv {
        // ECB
        Aes128Ecb = "aes-128-ecb", key 16, block 16, standard "NIST SP 800-38A", ecb::Encryptor<Aes128>, ecb::Decryptor<Aes128>;
        Aes192Ecb = "aes-192-ecb", key 24, block 16, standard "NIST SP 800-38A", ecb::Encryptor<Aes192>, ecb::Decryptor<Aes192>;
        Aes256Ecb = "aes-256-ecb" | "aes-ecb", key 32, block 16, standard "NIST SP 800-38A", ecb::Encryptor<Aes256>, ecb::Decryptor<Aes256>;

        Aria128Ecb = "aria-128-ecb", key 16, block 16, standard "NIST SP 800-38A + RFC 5794", ecb::Encryptor<Aria128>, ecb::Decryptor<Aria128>;
        Aria192Ecb = "aria-192-ecb", key 24, block 16, standard "NIST SP 800-38A + RFC 5794", ecb::Encryptor<Aria192>, ecb::Decryptor<Aria192>;
        Aria256Ecb = "aria-256-ecb" | "aria-ecb", key 32, block 16, standard "NIST SP 800-38A + RFC 5794", ecb::Encryptor<Aria256>, ecb::Decryptor<Aria256>;

        Camellia128Ecb = "camellia-128-ecb", key 16, block 16, standard "NIST SP 800-38A + RFC 3713", ecb::Encryptor<Camellia128>, ecb::Decryptor<Camellia128>;
        Camellia192Ecb = "camellia-192-ecb", key 24, block 16, standard "NIST SP 800-38A + RFC 3713", ecb::Encryptor<Camellia192>, ecb::Decryptor<Camellia192>;
        Camellia256Ecb = "camellia-256-ecb" | "camellia-ecb", key 32, block 16, standard "NIST SP 800-38A + RFC 3713", ecb::Encryptor<Camellia256>, ecb::Decryptor<Camellia256>;

        MagmaEcb = "magma-ecb" | "magma", key 32, block 8, standard "GOST R 34.13-2015", ecb::Encryptor<Magma>, ecb::Decryptor<Magma>;
        KuznyechikEcb = "kuznyechik-ecb" | "grasshopper-ecb", key 32, block 16, standard "GOST R 34.13-2015", ecb::Encryptor<Kuznyechik>, ecb::Decryptor<Kuznyechik>;

        Sm4Ecb = "sm4-ecb", key 16, block 16, standard "NIST SP 800-38A + GB/T 32907-2016", ecb::Encryptor<Sm4>, ecb::Decryptor<Sm4>;

        DesEcb = "des-ecb", key 8, block 8, standard "NIST SP 800-38A + FIPS 46-3 (withdrawn)", ecb::Encryptor<Des>, ecb::Decryptor<Des>;
        TdesEcb = "des-ede3-ecb" | "3des-ecb" | "tdes-ecb", key 24, block 8, standard "NIST SP 800-38A + NIST SP 800-67", ecb::Encryptor<TdesEde3>, ecb::Decryptor<TdesEde3>;
    }

    stream with iv {
        // CFB
        Aes128Cfb = "aes-128-cfb", key 16, iv 16, standard "NIST SP 800-38A", BufEncryptor<Aes128>, BufDecryptor<Aes128>, cfb_seal, cfb_open;
        Aes192Cfb = "aes-192-cfb", key 24, iv 16, standard "NIST SP 800-38A", BufEncryptor<Aes192>, BufDecryptor<Aes192>, cfb_seal, cfb_open;
        Aes256Cfb = "aes-256-cfb" | "aes-cfb", key 32, iv 16, standard "NIST SP 800-38A", BufEncryptor<Aes256>, BufDecryptor<Aes256>, cfb_seal, cfb_open;

        Aria128Cfb = "aria-128-cfb", key 16, iv 16, standard "NIST SP 800-38A + RFC 5794", BufEncryptor<Aria128>, BufDecryptor<Aria128>, cfb_seal, cfb_open;
        Aria192Cfb = "aria-192-cfb", key 24, iv 16, standard "NIST SP 800-38A + RFC 5794", BufEncryptor<Aria192>, BufDecryptor<Aria192>, cfb_seal, cfb_open;
        Aria256Cfb = "aria-256-cfb" | "aria-cfb", key 32, iv 16, standard "NIST SP 800-38A + RFC 5794", BufEncryptor<Aria256>, BufDecryptor<Aria256>, cfb_seal, cfb_open;

        Camellia128Cfb = "camellia-128-cfb", key 16, iv 16, standard "NIST SP 800-38A + RFC 3713", BufEncryptor<Camellia128>, BufDecryptor<Camellia128>, cfb_seal, cfb_open;
        Camellia192Cfb = "camellia-192-cfb", key 24, iv 16, standard "NIST SP 800-38A + RFC 3713", BufEncryptor<Camellia192>, BufDecryptor<Camellia192>, cfb_seal, cfb_open;
        Camellia256Cfb = "camellia-256-cfb" | "camellia-cfb", key 32, iv 16, standard "NIST SP 800-38A + RFC 3713", BufEncryptor<Camellia256>, BufDecryptor<Camellia256>, cfb_seal, cfb_open;

        MagmaCfb = "magma-cfb", key 32, iv 8, standard "NIST SP 800-38A + GOST R 34.12-2015", BufEncryptor<Magma>, BufDecryptor<Magma>, cfb_seal, cfb_open;
        KuznyechikCfb = "kuznyechik-cfb" | "grasshopper-cfb", key 32, iv 16, standard "NIST SP 800-38A + GOST R 34.12-2015", BufEncryptor<Kuznyechik>, BufDecryptor<Kuznyechik>, cfb_seal, cfb_open;

        Sm4Cfb = "sm4-cfb", key 16, iv 16, standard "NIST SP 800-38A + GB/T 32907-2016", BufEncryptor<Sm4>, BufDecryptor<Sm4>, cfb_seal, cfb_open;

        DesCfb = "des-cfb", key 8, iv 8, standard "NIST SP 800-38A + FIPS 46-3 (withdrawn)", BufEncryptor<Des>, BufDecryptor<Des>, cfb_seal, cfb_open;
        TdesCfb = "des-ede3-cfb" | "3des-cfb" | "tdes-cfb", key 24, iv 8, standard "NIST SP 800-38A + NIST SP 800-67", BufEncryptor<TdesEde3>, BufDecryptor<TdesEde3>, cfb_seal, cfb_open;
    }

    keystream with iv {
        // CTR
        Aes128Ctr = "aes-128-ctr", key 16, iv 16, standard "NIST SP 800-38A", Ctr128BE<Aes128>, Ctr128BE<Aes128>;
        Aes192Ctr = "aes-192-ctr", key 24, iv 16, standard "NIST SP 800-38A", Ctr128BE<Aes192>, Ctr128BE<Aes192>;
        Aes256Ctr = "aes-256-ctr" | "aes-ctr", key 32, iv 16, standard "NIST SP 800-38A", Ctr128BE<Aes256>, Ctr128BE<Aes256>;

        Aria128Ctr = "aria-128-ctr", key 16, iv 16, standard "NIST SP 800-38A + RFC 5794", Ctr128BE<Aria128>, Ctr128BE<Aria128>;
        Aria192Ctr = "aria-192-ctr", key 24, iv 16, standard "NIST SP 800-38A + RFC 5794", Ctr128BE<Aria192>, Ctr128BE<Aria192>;
        Aria256Ctr = "aria-256-ctr" | "aria-ctr", key 32, iv 16, standard "NIST SP 800-38A + RFC 5794", Ctr128BE<Aria256>, Ctr128BE<Aria256>;

        Camellia128Ctr = "camellia-128-ctr", key 16, iv 16, standard "NIST SP 800-38A + RFC 3713", Ctr128BE<Camellia128>, Ctr128BE<Camellia128>;
        Camellia192Ctr = "camellia-192-ctr", key 24, iv 16, standard "NIST SP 800-38A + RFC 3713", Ctr128BE<Camellia192>, Ctr128BE<Camellia192>;
        Camellia256Ctr = "camellia-256-ctr" | "camellia-ctr", key 32, iv 16, standard "NIST SP 800-38A + RFC 3713", Ctr128BE<Camellia256>, Ctr128BE<Camellia256>;

        Sm4Ctr = "sm4-ctr", key 16, iv 16, standard "NIST SP 800-38A + GB/T 32907-2016", Ctr128BE<Sm4>, Ctr128BE<Sm4>;

        DesCtr = "des-ctr", key 8, iv 8, standard "NIST SP 800-38A + FIPS 46-3 (withdrawn)", Ctr64BE<Des>, Ctr64BE<Des>;
        TdesCtr = "des-ede3-ctr" | "3des-ctr" | "tdes-ctr", key 24, iv 8, standard "NIST SP 800-38A + NIST SP 800-67", Ctr64BE<TdesEde3>, Ctr64BE<TdesEde3>;

        // OFB
        Aes128Ofb = "aes-128-ofb", key 16, iv 16, standard "NIST SP 800-38A", Ofb<Aes128>, Ofb<Aes128>;
        Aes192Ofb = "aes-192-ofb", key 24, iv 16, standard "NIST SP 800-38A", Ofb<Aes192>, Ofb<Aes192>;
        Aes256Ofb = "aes-256-ofb" | "aes-ofb", key 32, iv 16, standard "NIST SP 800-38A", Ofb<Aes256>, Ofb<Aes256>;

        Aria128Ofb = "aria-128-ofb", key 16, iv 16, standard "NIST SP 800-38A + RFC 5794", Ofb<Aria128>, Ofb<Aria128>;
        Aria192Ofb = "aria-192-ofb", key 24, iv 16, standard "NIST SP 800-38A + RFC 5794", Ofb<Aria192>, Ofb<Aria192>;
        Aria256Ofb = "aria-256-ofb" | "aria-ofb", key 32, iv 16, standard "NIST SP 800-38A + RFC 5794", Ofb<Aria256>, Ofb<Aria256>;

        Camellia128Ofb = "camellia-128-ofb", key 16, iv 16, standard "NIST SP 800-38A + RFC 3713", Ofb<Camellia128>, Ofb<Camellia128>;
        Camellia192Ofb = "camellia-192-ofb", key 24, iv 16, standard "NIST SP 800-38A + RFC 3713", Ofb<Camellia192>, Ofb<Camellia192>;
        Camellia256Ofb = "camellia-256-ofb" | "camellia-ofb", key 32, iv 16, standard "NIST SP 800-38A + RFC 3713", Ofb<Camellia256>, Ofb<Camellia256>;

        MagmaOfb = "magma-ofb", key 32, iv 8, standard "NIST SP 800-38A + GOST R 34.12-2015", Ofb<Magma>, Ofb<Magma>;
        KuznyechikOfb = "kuznyechik-ofb" | "grasshopper-ofb", key 32, iv 16, standard "NIST SP 800-38A + GOST R 34.12-2015", Ofb<Kuznyechik>, Ofb<Kuznyechik>;

        Sm4Ofb = "sm4-ofb", key 16, iv 16, standard "NIST SP 800-38A + GB/T 32907-2016", Ofb<Sm4>, Ofb<Sm4>;

        DesOfb = "des-ofb", key 8, iv 8, standard "NIST SP 800-38A + FIPS 46-3 (withdrawn)", Ofb<Des>, Ofb<Des>;
        TdesOfb = "des-ede3-ofb" | "3des-ofb" | "tdes-ofb", key 24, iv 8, standard "NIST SP 800-38A + NIST SP 800-67", Ofb<TdesEde3>, Ofb<TdesEde3>;

        // Other
        ChaCha20 = "chacha20" | "chacha", key 32, iv 12, standard "RFC 8439 section 2.4", ChaCha20, ChaCha20;
        XChaCha20 = "xchacha20" | "xchacha", key 32, iv 24, standard "draft-irtf-cfrg-xchacha-03", XChaCha20, XChaCha20;
        Salsa20 = "salsa20" | "salsa", key 32, iv 8, standard "eSTREAM (Bernstein, 2008)", Salsa20, Salsa20;
        XSalsa20 = "xsalsa20" | "xsalsa", key 32, iv 24, standard "Bernstein, Extending the Salsa20 nonce (2011)", XSalsa20, XSalsa20;
    }

    keystream with half iv {
        MagmaCtr = "magma-ctr", key 32, iv 4, standard "GOST R 34.13-2015", Ctr32BE<Magma>, Ctr32BE<Magma>;
        KuznyechikCtr = "kuznyechik-ctr" | "grasshopper-ctr", key 32, iv 8, standard "GOST R 34.13-2015", Ctr64BE<Kuznyechik>, Ctr64BE<Kuznyechik>;
    }

    keystream without iv {
        Rc4 = "rc4", key 32, standard "RFC 6229", rc4::Rc4, rc4::Rc4;
    }

    aead {
        // CCM
        Aes128Ccm = "aes-128-ccm", key 16, nonce 12, standard "NIST SP 800-38C", Ccm<Aes128, U16, U12>;
        Aes192Ccm = "aes-192-ccm", key 24, nonce 12, standard "NIST SP 800-38C", Ccm<Aes192, U16, U12>;
        Aes256Ccm = "aes-256-ccm" | "aes-ccm", key 32, nonce 12, standard "NIST SP 800-38C", Ccm<Aes256, U16, U12>;

        Aria128Ccm = "aria-128-ccm", key 16, nonce 12, standard "NIST SP 800-38C + RFC 5794", Ccm<Aria128, U16, U12>;
        Aria192Ccm = "aria-192-ccm", key 24, nonce 12, standard "NIST SP 800-38C + RFC 5794", Ccm<Aria192, U16, U12>;
        Aria256Ccm = "aria-256-ccm" | "aria-ccm", key 32, nonce 12, standard "NIST SP 800-38C + RFC 5794", Ccm<Aria256, U16, U12>;

        Camellia128Ccm = "camellia-128-ccm", key 16, nonce 12, standard "RFC 5528", Ccm<Camellia128, U16, U12>;
        Camellia192Ccm = "camellia-192-ccm", key 24, nonce 12, standard "RFC 5528", Ccm<Camellia192, U16, U12>;
        Camellia256Ccm = "camellia-256-ccm" | "camellia-ccm", key 32, nonce 12, standard "RFC 5528", Ccm<Camellia256, U16, U12>;

        Sm4Ccm = "sm4-ccm", key 16, nonce 12, standard "RFC 8998", Ccm<Sm4, U16, U12>;

        // GCM
        Aes128Gcm = "aes-128-gcm", key 16, nonce 12, standard "NIST SP 800-38D", Aes128Gcm;
        Aes192Gcm = "aes-192-gcm", key 24, nonce 12, standard "NIST SP 800-38D", AesGcm<Aes192, U12>;
        Aes256Gcm = "aes-256-gcm" | "aes-gcm" | "aes", key 32, nonce 12, standard "NIST SP 800-38D", Aes256Gcm;

        Aria128Gcm = "aria-128-gcm", key 16, nonce 12, standard "RFC 6209", AesGcm<Aria128, U12>;
        Aria192Gcm = "aria-192-gcm", key 24, nonce 12, standard "NIST SP 800-38D + RFC 5794", AesGcm<Aria192, U12>;
        Aria256Gcm = "aria-256-gcm" | "aria-gcm" | "aria", key 32, nonce 12, standard "RFC 6209", AesGcm<Aria256, U12>;

        Camellia128Gcm = "camellia-128-gcm", key 16, nonce 12, standard "RFC 6367", AesGcm<Camellia128, U12>;
        Camellia192Gcm = "camellia-192-gcm", key 24, nonce 12, standard "NIST SP 800-38D + RFC 3713", AesGcm<Camellia192, U12>;
        Camellia256Gcm = "camellia-256-gcm" | "camellia-gcm" | "camellia", key 32, nonce 12, standard "RFC 6367", AesGcm<Camellia256, U12>;

        KuznyechikGcm = "kuznyechik-gcm" | "grasshopper-gcm", key 32, nonce 12, standard "NIST SP 800-38D + GOST R 34.12-2015", AesGcm<Kuznyechik, U12>;

        Sm4Gcm = "sm4-gcm" | "sm4", key 16, nonce 12, standard "RFC 8998", AesGcm<Sm4, U12>;

        // GCM-SIV
        Aes128GcmSiv = "aes-128-gcm-siv", key 16, nonce 12, standard "RFC 8452", Aes128GcmSiv;
        Aes192GcmSiv = "aes-192-gcm-siv", key 24, nonce 12, standard "", AesGcmSiv<Aes192>;
        Aes256GcmSiv = "aes-256-gcm-siv" | "aes-gcm-siv", key 32, nonce 12, standard "RFC 8452", Aes256GcmSiv;

        Aria128GcmSiv = "aria-128-gcm-siv", key 16, nonce 12, standard "", AesGcmSiv<Aria128>;
        Aria192GcmSiv = "aria-192-gcm-siv", key 24, nonce 12, standard "", AesGcmSiv<Aria192>;
        Aria256GcmSiv = "aria-256-gcm-siv" | "aria-gcm-siv", key 32, nonce 12, standard "", AesGcmSiv<Aria256>;

        Camellia128GcmSiv = "camellia-128-gcm-siv", key 16, nonce 12, standard "", AesGcmSiv<Camellia128>;
        Camellia192GcmSiv = "camellia-192-gcm-siv", key 24, nonce 12, standard "", AesGcmSiv<Camellia192>;
        Camellia256GcmSiv = "camellia-256-gcm-siv" | "camellia-gcm-siv", key 32, nonce 12, standard "", AesGcmSiv<Camellia256>;

        KuznyechikGcmSiv = "kuznyechik-gcm-siv" | "grasshopper-gcm-siv", key 32, nonce 12, standard "", AesGcmSiv<Kuznyechik>;

        Sm4GcmSiv = "sm4-gcm-siv", key 16, nonce 12, standard "", AesGcmSiv<Sm4>;

        // MGM
        // Blocked upstream: `mgm` 0.5.0-pre.1 is on aead 0.5 and cipher 0.3,
        // two generations behind, so nothing in it fits `AeadCipher`.
        // MagmaMgm = "magma-mgm", key 32, nonce 8, standard "GOST R 34.13-2015";

        // OCB
        Aes128Ocb = "aes-128-ocb", key 16, nonce 12, standard "RFC 7253", Ocb3<Aes128>;
        Aes192Ocb = "aes-192-ocb", key 24, nonce 12, standard "RFC 7253", Ocb3<Aes192>;
        Aes256Ocb = "aes-256-ocb" | "aes-ocb", key 32, nonce 12, standard "RFC 7253", Ocb3<Aes256>;

        Aria128Ocb = "aria-128-ocb", key 16, nonce 12, standard "RFC 7253 + RFC 5794", Ocb3<Aria128>;
        Aria192Ocb = "aria-192-ocb", key 24, nonce 12, standard "RFC 7253 + RFC 5794", Ocb3<Aria192>;
        Aria256Ocb = "aria-256-ocb" | "aria-ocb", key 32, nonce 12, standard "RFC 7253 + RFC 5794", Ocb3<Aria256>;

        Camellia128Ocb = "camellia-128-ocb", key 16, nonce 12, standard "RFC 7253 + RFC 3713", Ocb3<Camellia128>;
        Camellia192Ocb = "camellia-192-ocb", key 24, nonce 12, standard "RFC 7253 + RFC 3713", Ocb3<Camellia192>;
        Camellia256Ocb = "camellia-256-ocb" | "camellia-ocb", key 32, nonce 12, standard "RFC 7253 + RFC 3713", Ocb3<Camellia256>;

        KuznyechikOcb = "kuznyechik-ocb" | "grasshopper-ocb", key 32, nonce 12, standard "RFC 7253 + GOST R 34.12-2015", Ocb3<Kuznyechik>;

        Sm4Ocb = "sm4-ocb", key 16, nonce 12, standard "RFC 7253 + GB/T 32907-2016", Ocb3<Sm4>;

        // SIV
        Aes128Siv = "aes-128-siv", key 32, nonce 16, standard "RFC 5297", Aes128SivAead;
        Aes256Siv = "aes-256-siv" | "aes-siv", key 64, nonce 16, standard "RFC 5297", Aes256SivAead;

        // Other
        Ascon128 = "ascon-aead128" | "ascon" , key 16, nonce 16, standard "NIST SP 800-232", AsconAead128;
        Chacha20Poly1305 = "chacha20-poly1305" | "chachapoly", key 32, nonce 12, standard "RFC 8439", ChaCha20Poly1305;
        XChacha20Poly1305 = "xchacha20-poly1305" | "xchachapoly", key 32, nonce 24, standard "draft-irtf-cfrg-xchacha-03", XChaCha20Poly1305;
    }
}

/// Whether each call is its own record or the path is one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// One self-contained record per call.
    Record,
    /// One session across the path.
    Stream,
}

/// How key material is written wherever it is kept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyFormat {
    /// Hexadecimal text. Whitespace is ignored, so a file with a trailing
    /// newline is read as intended.
    #[default]
    Hex,
    /// Standard base64 text, whitespace likewise ignored.
    Base64,
    /// The bytes exactly as they are, which only a file can carry sensibly.
    Raw,
}

/// How a partial block is filled out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Padding {
    /// RFC 5652 padding, which always adds between one byte and one block.
    #[default]
    Pkcs7,
    /// None, so every message must already be a whole number of blocks.
    None,
}

/// What a decrypt stage does with a record it cannot open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnFail {
    /// Fail the path.
    #[default]
    #[serde(alias = "fail")]
    Error,
    /// Discard the record, log it, and carry on with the next.
    Drop,
    /// Stop reading upstream, as if the peer had closed.
    #[serde(alias = "terminate")]
    Halt,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct EncryptConfig {
    /// Cipher to use.
    #[serde(alias = "algo", alias = "alg")]
    pub cipher: Cipher,

    /// Key as text, in `key-format`. Visible in `ps` output, so prefer
    /// `key-file` anywhere that matters.
    #[serde(default)]
    pub key: Option<String>,

    /// File holding the key.
    #[serde(default)]
    pub key_file: Option<PathBuf>,

    /// Environment variable holding the key.
    #[serde(default)]
    pub key_env: Option<String>,

    /// How the key is written, wherever it comes from.
    #[serde(default)]
    pub key_format: KeyFormat,

    /// Generate a key here and report it. Nothing else can hold it, so this
    /// is for a capture read back by the same operator, not for a link.
    #[serde(default)]
    pub random_key: bool,

    /// Where a generated key is reported. Defaults to stderr.
    #[serde(default)]
    pub key_out: Option<PathBuf>,

    /// Whether each call is its own record or the path is one session.
    #[serde(default)]
    pub mode: Option<Mode>,

    /// How a partial block is filled out, in the modes that have blocks.
    #[serde(default)]
    pub padding: Option<Padding>,

    /// Start a fresh session after this many bytes, in a keystream session.
    #[serde(default)]
    pub rotate_after: Option<ByteSize>,

    /// Using non-standard schemes is permissible
    #[serde(default)]
    pub nonstandard: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct DecryptConfig {
    /// Cipher to use. Must match the sender's.
    #[serde(alias = "algo", alias = "alg")]
    pub cipher: Cipher,

    /// Key as text, in `key-format`. Visible in `ps` output, so prefer
    /// `key-file` anywhere that matters.
    #[serde(default)]
    pub key: Option<String>,

    /// File holding the key.
    #[serde(default)]
    pub key_file: Option<PathBuf>,

    /// Environment variable holding the key.
    #[serde(default)]
    pub key_env: Option<String>,

    /// How the key is written, wherever it comes from.
    #[serde(default)]
    pub key_format: KeyFormat,

    /// Whether each call is its own record or the path is one session. Must
    /// match the sender's.
    #[serde(default)]
    pub mode: Option<Mode>,

    /// How a partial block was filled out. Must match the sender's.
    #[serde(default)]
    pub padding: Option<Padding>,

    /// How often the sender starts a fresh session. Must match the sender's.
    #[serde(default)]
    pub rotate_after: Option<ByteSize>,

    /// What to do with a record that cannot be opened.
    #[serde(default)]
    pub on_fail: Option<OnFail>,

    /// Using non-standard schemes is permissible
    #[serde(default)]
    pub nonstandard: bool,
}

/// Where key bytes come from, once the mutually exclusive options are settled.
enum KeySource<'a> {
    Text(&'a str),
    File(&'a Path),
    Env(&'a str),
    Random,
}

/// Which source was named, rejecting none and more than one.
///
/// Two sources would leave which one wins to the order the fields happen to
/// be read in, which is not something a config should decide by accident.
fn key_source<'a>(
    plugin: &'static str,
    text: Option<&'a String>,
    file: Option<&'a PathBuf>,
    env: Option<&'a String>,
    random: bool,
) -> Result<KeySource<'a>> {
    let mut sources = Vec::new();

    if let Some(text) = text {
        sources.push(KeySource::Text(text));
    }

    if let Some(file) = file {
        sources.push(KeySource::File(file));
    }

    if let Some(env) = env {
        sources.push(KeySource::Env(env));
    }

    if random {
        sources.push(KeySource::Random);
    }

    match sources.len() {
        1 => Ok(sources.remove(0)),
        0 => Err(PluginError::config(
            plugin,
            "no key: give one of key, key-file or key-env",
        )),
        _ => Err(PluginError::config(
            plugin,
            "more than one key: key, key-file, key-env and random-key are alternatives",
        )),
    }
}

/// Decode key material, which is text in every format but `raw`.
fn decode(plugin: &'static str, raw: &[u8], format: KeyFormat) -> Result<Vec<u8>> {
    if format == KeyFormat::Raw {
        return Ok(raw.to_vec());
    }

    let text = str::from_utf8(raw).map_err(|_| {
        PluginError::config(
            plugin,
            "key is not text, so it is neither hex nor base64: use key-format = \"raw\"",
        )
    })?;

    // Whitespace is noise in both encodings, so a key file written by hand,
    // or with a trailing newline, decodes to what it looks like.
    let text: String = text.split_whitespace().collect();

    match format {
        KeyFormat::Hex => hex::decode(&text)
            .map_err(|error| PluginError::config(plugin, format!("key is not hex: {error}"))),
        KeyFormat::Base64 => BASE64_STANDARD
            .decode(&text)
            .map_err(|error| PluginError::config(plugin, format!("key is not base64: {error}"))),
        KeyFormat::Raw => unreachable!("returned above"),
    }
}

/// The key, read from wherever it lives and checked against the cipher.
fn key_material(
    plugin: &'static str,
    source: KeySource<'_>,
    format: KeyFormat,
    spec: Spec,
) -> Result<Vec<u8>> {
    let key = match source {
        KeySource::Text(text) => decode(plugin, text.as_bytes(), format)?,
        KeySource::File(path) => {
            let raw = std::fs::read(path).map_err(|error| {
                PluginError::config(plugin, format!("{}: {error}", path.display()))
            })?;

            decode(plugin, &raw, format)?
        }
        KeySource::Env(name) => {
            let raw = std::env::var(name).map_err(|_| {
                PluginError::config(plugin, format!("{name} is not set in the environment"))
            })?;

            decode(plugin, raw.as_bytes(), format)?
        }
        KeySource::Random => {
            let mut key = vec![0u8; spec.key];

            getrandom::fill(&mut key).map_err(|error| {
                PluginError::config(plugin, format!("no randomness for a key: {error}"))
            })?;

            key
        }
    };

    if key.len() != spec.key {
        return Err(PluginError::config(
            plugin,
            format!(
                "{} takes a {}-byte key and this one is {} bytes",
                spec.name,
                spec.key,
                key.len()
            ),
        ));
    }

    Ok(key)
}

/// Which mode this entry runs in, defaulting by family.
///
/// An AEAD cipher is record only. Anything else defaults to a session,
/// because that is the shape that needs no framing on a byte stream, which is
/// where these stages usually sit.
fn mode(plugin: &'static str, requested: Option<Mode>, spec: Spec) -> Result<Mode> {
    match (requested, spec.family) {
        (Some(Mode::Stream), Family::Aead) => Err(PluginError::config(
            plugin,
            format!(
                "{} is authenticated, so it works one record at a time: use mode = \"record\", \
                 with frame and unframe if this path is a byte stream",
                spec.name
            ),
        )),
        (Some(mode), _) => Ok(mode),
        (None, Family::Aead) => Ok(Mode::Record),
        (None, _) => Ok(Mode::Stream),
    }
}

/// Refuses a cipher no document specifies, unless the caller asked for one.
///
/// A row with no `standard` is one the crates behind it will compute and that
/// round trips against tocat at both ends, but that no other implementation
/// is likely to agree with byte for byte. Reaching one by accident is a tunnel
/// that works until the far end is something else, so it is opt in.
fn specified(plugin: &'static str, nonstandard: bool, spec: Spec) -> Result<()> {
    if spec.standard.is_some() || nonstandard {
        return Ok(());
    }

    Err(PluginError::config(
        plugin,
        format!(
            "{} is not specified by any document, so nothing else is likely to read it: pass \
             nonstandard to use it anyway",
            spec.name
        ),
    ))
}

/// The padding scheme, rejecting the option where nothing pads.
fn padding(plugin: &'static str, requested: Option<Padding>, spec: Spec) -> Result<Padding> {
    match (requested, spec.family) {
        (Some(padding), Family::Block) => Ok(padding),
        (None, _) => Ok(Padding::default()),
        (Some(_), _) => Err(PluginError::config(
            plugin,
            format!(
                "padding means nothing for {}: only the ecb and cbc modes work a block at a time",
                spec.name
            ),
        )),
    }
}

/// A rotation budget, held as the ciphertext one session carries.
///
/// Ciphertext rather than payload is what the two ends can both count. A
/// padded block mode spends its last block on padding, so how much payload a
/// session carries depends on where the sender's chunks happened to fall,
/// while the ciphertext it puts on the wire is fixed. The receiver chunks
/// differently and has nothing but that count to find the next nonce by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rotation {
    /// Ciphertext bytes a session carries, not counting its nonce.
    total: u64,
    /// Payload bytes sealed before the session closes: `total`, less the
    /// padding block a padded block mode adds on the way out.
    allowance: u64,
    /// Payload bytes that may be sealed past the allowance because they ride
    /// inside that padding block rather than adding ciphertext of their own.
    slack: u64,
}

/// The rotation budget, if any.
///
/// Offered wherever there is a nonce to replace: a keystream session, where
/// it bounds the keystream drawn under one nonce, and CBC, where it bounds
/// the chain. ECB has none, and in record mode every record already draws its
/// own, so both are refused rather than quietly doing nothing.
///
/// What rotation does not do is replace the key. The amount of data one key
/// may safely carry is a property of the key and the block size, and a fresh
/// IV does not extend it. That bound is the operator's to respect.
fn rotation(
    plugin: &'static str,
    requested: Option<ByteSize>,
    mode: Mode,
    padding: Padding,
    spec: Spec,
) -> Result<Option<Rotation>> {
    let Some(requested) = requested else {
        return Ok(None);
    };

    if mode != Mode::Stream {
        return Err(PluginError::config(
            plugin,
            "rotate-after means nothing in record mode: every record already carries its own nonce",
        ));
    }

    if spec.nonce == 0 {
        return Err(PluginError::config(
            plugin,
            format!(
                "rotate-after means nothing for {}: it has no nonce to replace part way through",
                spec.name
            ),
        ));
    }

    let total = requested.bytes() as u64;

    if spec.family != Family::Block {
        if total == 0 {
            return Err(PluginError::config(
                plugin,
                "rotate-after is zero, which would start a session per byte",
            ));
        }

        return Ok(Some(Rotation {
            total,
            allowance: total,
            slack: 0,
        }));
    }

    let size = spec.block as u64;

    if !total.is_multiple_of(size) {
        return Err(PluginError::config(
            plugin,
            format!(
                "rotate-after must be a multiple of {}'s {size}-byte block, since a session ends \
                 on a block boundary",
                spec.name
            ),
        ));
    }

    // A padded session spends its last block on padding, so the budget has to
    // cover that block and still leave a block for payload.
    let (allowance, slack) = match padding {
        Padding::Pkcs7 => (total.saturating_sub(size), size - 1),
        Padding::None => (total, 0),
    };

    if allowance == 0 {
        return Err(PluginError::config(
            plugin,
            format!(
                "rotate-after is {total} bytes, which leaves {} no room for payload: a padded \
                 session spends its last block on padding, so the budget needs at least two",
                spec.name
            ),
        ));
    }

    Ok(Some(Rotation {
        total,
        allowance,
        slack,
    }))
}

/// What to do with a record that will not open, rejecting the option in
/// stream mode.
///
/// Dropping part of a session desynchronises everything after it, so there is
/// nothing to carry on with; a session that cannot be decoded ends the path.
fn on_fail(requested: Option<OnFail>, mode: Mode) -> Result<OnFail> {
    match (requested, mode) {
        (Some(_), Mode::Stream) => Err(PluginError::config(
            DECRYPT,
            "on-fail means nothing in stream mode: dropping part of a session desynchronises \
             everything after it",
        )),
        (Some(on_fail), _) => Ok(on_fail),
        (None, _) => Ok(OnFail::default()),
    }
}

/// Where a generated key is announced.
///
/// stdout is refused for the reason `hash` refuses it: on a stdio endpoint it
/// carries relay payload.
fn key_target(path: Option<&PathBuf>) -> Result<ChannelTarget> {
    let Some(path) = path else {
        return Ok(ChannelTarget::Stderr);
    };

    match path.to_str() {
        Some("-" | "stderr" | "/dev/stderr" | "/dev/fd/2") => Ok(ChannelTarget::Stderr),
        Some("stdout" | "/dev/stdout" | "/dev/fd/1") => Err(PluginError::config(
            ENCRYPT,
            "refusing to write a key to stdout, it may carry relay payload; use `-` for stderr",
        )),
        _ => Ok(ChannelTarget::File {
            path: path.clone(),
            append: true,
        }),
    }
}

/// Buffered OS randomness.
struct Random {
    pool: [u8; RANDOM_POOL],
    used: usize,
}

impl Random {
    fn new() -> Self {
        // Starting used means the first nonce fills the pool, so there is no
        // separate initialisation path.
        Self {
            pool: [0; RANDOM_POOL],
            used: RANDOM_POOL,
        }
    }

    /// Fill `out`, which is never longer than a nonce.
    fn fill(&mut self, out: &mut [u8]) -> Result<()> {
        debug_assert!(out.len() <= RANDOM_POOL, "a nonce fits in the pool");

        if out.is_empty() {
            return Ok(());
        }

        if self.used + out.len() > RANDOM_POOL {
            getrandom::fill(&mut self.pool).map_err(|error| {
                PluginError::runtime(ENCRYPT, format!("no randomness for a nonce: {error}"))
            })?;

            self.used = 0;
        }

        out.copy_from_slice(&self.pool[self.used..self.used + out.len()]);
        self.used += out.len();

        Ok(())
    }
}

/// One session's worth of encryption, with the cipher already resolved.
///
/// `out` is cleared by the caller; `update` and `finish` append to it.
trait Seal: Send {
    /// Open a session with `nonce`, discarding whatever came before.
    fn start(&mut self, nonce: &[u8]) -> Result<()>;

    /// Encrypt `input`. May hold bytes back, in the modes that have blocks.
    fn update(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()>;

    /// Close the session, emitting whatever it was holding.
    fn finish(&mut self, out: &mut Vec<u8>) -> Result<()>;
}

/// The inverse of [`Seal`], driven the same way.
trait Open: Send {
    fn start(&mut self, nonce: &[u8]) -> Result<()>;

    fn update(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()>;

    fn finish(&mut self, out: &mut Vec<u8>) -> Result<()>;
}

/// The whole blocks of `buf`, which is all of it wherever this is called.
fn blocks_mut<M: BlockSizeUser>(buf: &mut [u8]) -> &mut [Block<M>] {
    Array::slice_as_chunks_mut(buf).0
}

/// Build a cipher from a key of the length its spec promised.
fn keyed<A: KeyInit>(plugin: &'static str, key: &[u8]) -> Result<A> {
    A::new_from_slice(key)
        .map_err(|_| PluginError::config(plugin, "key is the wrong length for this cipher"))
}

/// An AEAD cipher, which seals one whole message per session.
///
/// Built once and used per record: `encrypt_in_place` takes `&self`, so the
/// key schedule and the authenticator's tables are not rebuilt per message.
struct AeadCipher<A: AeadCore> {
    aead: A,
    nonce: Nonce<A>,
}

impl<A: AeadCore> AeadCipher<A> {
    fn new(aead: A) -> Self {
        Self {
            aead,
            nonce: Nonce::<A>::default(),
        }
    }

    fn set_nonce(&mut self, plugin: &'static str, nonce: &[u8]) -> Result<()> {
        self.nonce = Nonce::<A>::try_from(nonce).map_err(|_| {
            PluginError::runtime(plugin, "nonce is the wrong length for this cipher")
        })?;

        Ok(())
    }
}

impl<A: AeadInOut + Send> Seal for AeadCipher<A> {
    fn start(&mut self, nonce: &[u8]) -> Result<()> {
        self.set_nonce(ENCRYPT, nonce)
    }

    fn update(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(input);

        self.aead
            .encrypt_in_place(&self.nonce, &[], out)
            .map_err(|_| PluginError::runtime(ENCRYPT, "the cipher refused a message this size"))
    }

    fn finish(&mut self, _out: &mut Vec<u8>) -> Result<()> {
        Ok(())
    }
}

impl<A: AeadInOut + Send> Open for AeadCipher<A> {
    fn start(&mut self, nonce: &[u8]) -> Result<()> {
        self.set_nonce(DECRYPT, nonce)
    }

    fn update(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(input);

        self.aead.decrypt_in_place(&self.nonce, &[], out).map_err(|_| {
            PluginError::runtime(
                DECRYPT,
                "record failed authentication: wrong key, wrong cipher, or the bytes were changed \
                 in transit",
            )
        })
    }

    fn finish(&mut self, _out: &mut Vec<u8>) -> Result<()> {
        Ok(())
    }
}

/// A block mode, driven a block at a time so a session can span calls.
///
/// `init` is what tells ECB from CBC; see [`Init`].
struct BlockCipher<M> {
    key: Vec<u8>,
    init: Init<M>,
    padding: Padding,
    /// The session, absent until `start` and taken by `finish`.
    mode: Option<M>,
    /// Bytes of a block that is not full yet, and on the decrypt side the
    /// block held back in case it is the padded tail.
    pending: Vec<u8>,
}

impl<M> BlockCipher<M> {
    fn new(key: Vec<u8>, padding: Padding, init: Init<M>) -> Self {
        Self {
            key,
            init,
            padding,
            mode: None,
            pending: Vec::new(),
        }
    }

    fn open_session(&mut self, plugin: &'static str, iv: &[u8]) -> Result<()> {
        self.mode = Some((self.init)(&self.key, iv).map_err(|_| {
            PluginError::runtime(plugin, "key or iv is the wrong length for this cipher")
        })?);
        self.pending.clear();

        Ok(())
    }
}

/// How a mode is built for a session, which is the one thing the families do
/// not agree on: CBC and the keystream modes take a key and an IV, ECB takes a
/// key alone, and their constructors share no trait a bound could tell them
/// apart by.
///
/// Two blanket impls over the same generic would overlap however the bounds
/// were written, because coherence is decided without consulting which types
/// implement what. So the difference is a function pointer, chosen where the
/// cipher is, and each family keeps one impl of [`Seal`] and one of [`Open`].
type Init<M> = fn(&[u8], &[u8]) -> Result<M, InvalidLength>;

/// A key and an IV, for a mode that takes one.
fn with_iv<M: KeyIvInit>(key: &[u8], iv: &[u8]) -> Result<M, InvalidLength> {
    M::new_from_slices(key, iv)
}

/// GOST counter mode transmits half a block and starts the counter at
/// `iv || 0`, so what arrives is zero-extended to the block the crate wants.
/// Every other mode here puts the whole IV on the wire, which is why this is
/// the one place the transmitted width and the constructed width differ.
fn with_half_iv<M: KeyIvInit>(key: &[u8], iv: &[u8]) -> Result<M, InvalidLength> {
    let mut block = Iv::<M>::default();

    if iv.len() * 2 != block.len() {
        return Err(InvalidLength);
    }

    block[..iv.len()].copy_from_slice(iv);

    M::new_from_slices(key, &block)
}

/// A key alone, for a mode with no IV to take.
fn without_iv<M: KeyInit>(key: &[u8], _iv: &[u8]) -> Result<M, InvalidLength> {
    M::new_from_slice(key)
}

/// What a length-preserving cipher does to a buffer, in place.
///
/// The companion to [`Init`], and there for the same reason: CTR and OFB
/// transform through [`StreamCipher`], while CFB's buffered types do it
/// through an inherent method that no trait covers. A pointer chosen where
/// the cipher is lets both share one struct rather than one each.
type Apply<C> = fn(&mut C, &mut [u8]) -> Result<(), StreamCipherError>;

/// The transform for anything that implements [`StreamCipher`].
fn xor_keystream<C: StreamCipher>(cipher: &mut C, buf: &mut [u8]) -> Result<(), StreamCipherError> {
    cipher.try_apply_keystream(buf)
}

/// The transform for CFB, whose buffered types carry a partial block position
/// across calls and expose the work as an inherent method rather than a trait
/// one. Neither can fail, so neither reports anything.
fn cfb_seal<C: BlockCipherEncrypt>(
    cipher: &mut BufEncryptor<C>,
    buf: &mut [u8],
) -> Result<(), StreamCipherError> {
    cipher.encrypt(buf);
    Ok(())
}

/// CFB decrypts with the block cipher's encrypt direction, like every mode
/// that builds a keystream, so the bound here is not a mistake.
fn cfb_open<C: BlockCipherEncrypt>(
    cipher: &mut BufDecryptor<C>,
    buf: &mut [u8],
) -> Result<(), StreamCipherError> {
    cipher.decrypt(buf);
    Ok(())
}

impl<M: BlockModeEncrypt + Send> Seal for BlockCipher<M> {
    fn start(&mut self, iv: &[u8]) -> Result<()> {
        self.open_session(ENCRYPT, iv)
    }

    fn update(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let size = M::BlockSize::USIZE;
        // Field by field rather than through a helper, so that `pending`
        // stays borrowable alongside the session.
        let mode = self
            .mode
            .as_mut()
            .ok_or_else(|| PluginError::runtime(ENCRYPT, NO_SESSION))?;
        let mut rest = input;

        // Finish the block left over from the last call before anything else,
        // so `pending` is empty for the bulk path below.
        if !self.pending.is_empty() {
            let take = (size - self.pending.len()).min(rest.len());

            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];

            if self.pending.len() < size {
                return Ok(());
            }

            let at = out.len();

            out.extend_from_slice(&self.pending);
            mode.encrypt_blocks(blocks_mut::<M>(&mut out[at..]));
            self.pending.clear();
        }

        let full = rest.len() - rest.len() % size;
        let at = out.len();

        out.extend_from_slice(&rest[..full]);
        mode.encrypt_blocks(blocks_mut::<M>(&mut out[at..]));
        self.pending.extend_from_slice(&rest[full..]);

        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let Some(mut mode) = self.mode.take() else {
            return Ok(());
        };

        if self.padding == Padding::None {
            return if self.pending.is_empty() {
                Ok(())
            } else {
                Err(PluginError::runtime(
                    ENCRYPT,
                    "padding is off and the message is not a whole number of blocks",
                ))
            };
        }

        // PKCS#7 always adds a block, so a session that ended on a boundary
        // still emits one. That is what lets the far end know where to stop.
        let mut tail = Block::<M>::default();
        let pos = self.pending.len();

        tail[..pos].copy_from_slice(&self.pending);
        Pkcs7::pad(&mut tail, pos);
        mode.encrypt_block(&mut tail);
        out.extend_from_slice(&tail);
        self.pending.clear();

        Ok(())
    }
}

impl<M: BlockModeDecrypt + Send> Open for BlockCipher<M> {
    fn start(&mut self, iv: &[u8]) -> Result<()> {
        self.open_session(DECRYPT, iv)
    }

    /// Costs a copy the encrypt side does not: bytes are accumulated so that
    /// whole blocks can be decrypted, and with padding the last full block is
    /// held back until it is known not to be the padded tail.
    fn update(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let size = M::BlockSize::USIZE;
        let mode = self
            .mode
            .as_mut()
            .ok_or_else(|| PluginError::runtime(DECRYPT, NO_SESSION))?;

        self.pending.extend_from_slice(input);

        let reserve = if self.padding == Padding::Pkcs7 {
            size
        } else {
            0
        };
        let ready = self.pending.len().saturating_sub(reserve);
        let ready = ready - ready % size;

        if ready == 0 {
            return Ok(());
        }

        let at = out.len();

        out.extend_from_slice(&self.pending[..ready]);
        mode.decrypt_blocks(blocks_mut::<M>(&mut out[at..]));
        self.pending.drain(..ready);

        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let size = M::BlockSize::USIZE;
        let Some(mut mode) = self.mode.take() else {
            return Ok(());
        };

        if self.pending.is_empty() {
            // A padded session always ends with a padding block, so nothing
            // left over means the sender's last block never arrived.
            return match self.padding {
                Padding::Pkcs7 => Err(PluginError::runtime(DECRYPT, TRUNCATED)),
                Padding::None => Ok(()),
            };
        }

        if !self.pending.len().is_multiple_of(size) {
            return Err(PluginError::runtime(DECRYPT, TRUNCATED));
        }

        let at = out.len();

        out.extend_from_slice(&self.pending);
        mode.decrypt_blocks(blocks_mut::<M>(&mut out[at..]));
        self.pending.clear();

        if self.padding == Padding::Pkcs7 {
            let kept = {
                let tail = <&Block<M>>::try_from(&out[out.len() - size..])
                    .map_err(|_| PluginError::runtime(DECRYPT, TRUNCATED))?;

                Pkcs7::unpad(tail)
                    .map_err(|_| {
                        PluginError::runtime(
                            DECRYPT,
                            "padding is malformed: wrong key, wrong cipher, or the bytes were \
                             changed in transit",
                        )
                    })?
                    .len()
            };

            out.truncate(out.len() - size + kept);
        }

        Ok(())
    }
}

/// A keystream mode, where encryption and decryption are the same operation.
///
/// `init` and `apply` are the two things the members of this family disagree
/// on. Most take a key and an IV and transform through [`StreamCipher`]; one
/// with no IV, or whose transform is an inherent method rather than a trait
/// one, supplies its own pointers instead. See [`Init`] and [`Apply`].
///
/// The bounds live on the pointers, so the struct itself needs none, and a
/// cipher that fits neither trait still fits here.
struct KeystreamCipher<C> {
    key: Vec<u8>,
    init: Init<C>,
    apply: Apply<C>,
    cipher: Option<C>,
}

impl<C> KeystreamCipher<C> {
    fn new(key: Vec<u8>, init: Init<C>, apply: Apply<C>) -> Self {
        Self {
            key,
            init,
            apply,
            cipher: None,
        }
    }

    fn open_session(&mut self, plugin: &'static str, iv: &[u8]) -> Result<()> {
        self.cipher = Some((self.init)(&self.key, iv).map_err(|_| {
            PluginError::runtime(plugin, "key or iv is the wrong length for this cipher")
        })?);

        Ok(())
    }

    /// The keystream is its own inverse, so both directions are this.
    fn transform(&mut self, plugin: &'static str, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let cipher = self
            .cipher
            .as_mut()
            .ok_or_else(|| PluginError::runtime(plugin, NO_SESSION))?;
        let at = out.len();

        out.extend_from_slice(input);

        (self.apply)(cipher, &mut out[at..])
            .map_err(|_| PluginError::runtime(plugin, "the keystream for this nonce is exhausted"))
    }
}

impl<C: Send> Seal for KeystreamCipher<C> {
    fn start(&mut self, iv: &[u8]) -> Result<()> {
        self.open_session(ENCRYPT, iv)
    }

    fn update(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        self.transform(ENCRYPT, input, out)
    }

    fn finish(&mut self, _out: &mut Vec<u8>) -> Result<()> {
        self.cipher = None;

        Ok(())
    }
}

impl<C: Send> Open for KeystreamCipher<C> {
    fn start(&mut self, iv: &[u8]) -> Result<()> {
        self.open_session(DECRYPT, iv)
    }

    fn update(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        self.transform(DECRYPT, input, out)
    }

    fn finish(&mut self, _out: &mut Vec<u8>) -> Result<()> {
        self.cipher = None;

        Ok(())
    }
}

pub struct Encrypt {
    cipher: Box<dyn Seal>,
    mode: Mode,
    /// Room for the widest nonce, of which `nonce_len` bytes are used.
    nonce: [u8; MAX_NONCE],
    nonce_len: usize,
    random: Random,
    /// The ciphertext budget a session runs under. `None` never rotates.
    rotate_after: Option<Rotation>,
    /// Payload bytes sealed under the current session.
    fed: u64,
    /// Ciphertext the current session has emitted, which for a block mode
    /// lags `fed` by whatever is still waiting to fill a block.
    emitted: u64,
    started: bool,
    /// A key generated here, to be reported on the first call.
    generated: Option<(ChannelId, String)>,
    out: Vec<u8>,
}

impl Encrypt {
    /// Report a generated key, once.
    ///
    /// It happens here rather than at build time because a factory has no way
    /// to reach a channel: side writes need a [`Ctx`].
    fn announce(&mut self, ctx: &mut Ctx<'_>) {
        if let Some((channel, line)) = self.generated.take() {
            ctx.side_write(channel, line.as_bytes());
        }
    }

    /// Draw a nonce, open a session with it, and put it on the wire ahead of
    /// whatever that session produces.
    fn open_session(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        let len = self.nonce_len;
        let nonce = &mut self.nonce[..len];

        self.random.fill(nonce)?;
        self.cipher.start(nonce)?;
        ctx.forward(nonce);

        self.fed = 0;
        self.emitted = 0;
        self.started = true;

        Ok(())
    }

    /// Close the current session, emitting whatever it held back.
    fn close_session(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        self.out.clear();
        self.cipher.finish(&mut self.out)?;

        if !self.out.is_empty() {
            ctx.forward(&self.out);
        }

        self.started = false;

        Ok(())
    }
}

impl Plugin for Encrypt {
    fn name(&self) -> &str {
        ENCRYPT
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        self.announce(ctx);

        if input.is_empty() {
            return Ok(());
        }

        if self.mode == Mode::Record {
            self.open_session(ctx)?;
            self.out.clear();
            self.cipher.update(input, &mut self.out)?;
            self.cipher.finish(&mut self.out)?;
            ctx.forward(&self.out);

            return Ok(());
        }

        // A session rotates at an exact ciphertext offset rather than at
        // whatever call boundary happens to cross the budget, because the far
        // end chunks differently and has only that count to go on. Feeding is
        // capped so the session cannot overshoot: the allowance is what fits
        // before the padding block, and the slack is what fits inside it.
        let mut rest = input;

        while !rest.is_empty() {
            if !self.started {
                self.open_session(ctx)?;
            }

            let take = match self.rotate_after {
                Some(rotation) => {
                    let room = rotation.allowance + rotation.slack - self.fed;

                    rest.len().min(room as usize)
                }
                None => rest.len(),
            };

            self.out.clear();
            self.cipher.update(&rest[..take], &mut self.out)?;

            if !self.out.is_empty() {
                ctx.forward(&self.out);
            }

            self.fed += take as u64;
            self.emitted += self.out.len() as u64;
            rest = &rest[take..];

            if self
                .rotate_after
                .is_some_and(|rotation| self.emitted >= rotation.allowance)
            {
                self.close_session(ctx)?;
            }
        }

        Ok(())
    }

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        // A path that carried nothing still reports a key generated for it,
        // since an operator asked for one.
        self.announce(ctx);

        if self.mode == Mode::Record || !self.started {
            return Ok(());
        }

        self.close_session(ctx)
    }

    fn boundaries(&self) -> Boundaries {
        match self.mode {
            Mode::Record => Boundaries::Preserve,
            Mode::Stream => Boundaries::Fuse,
        }
    }

    fn needs(&self) -> Needs {
        match self.mode {
            // The record it writes has to arrive whole to be readable.
            Mode::Record => Needs::Downstream,
            Mode::Stream => Needs::Nothing,
        }
    }
}

pub struct Decrypt {
    cipher: Box<dyn Open>,
    mode: Mode,
    nonce_len: usize,
    tag_len: usize,
    on_fail: OnFail,
    rotate_after: Option<u64>,
    opened: u64,
    /// Stream mode: a session's nonce as it arrives, which may straddle calls.
    nonce: Vec<u8>,
    started: bool,
    out: Vec<u8>,
}

impl Decrypt {
    /// One whole record: header, body, and nothing carried onwards.
    fn record(&mut self, input: &[u8]) -> Result<()> {
        let Some((nonce, body)) = input.split_at_checked(self.nonce_len) else {
            return Err(PluginError::runtime(DECRYPT, TRUNCATED));
        };

        if body.len() < self.tag_len {
            return Err(PluginError::runtime(DECRYPT, TRUNCATED));
        }

        self.cipher.start(nonce)?;
        self.out.clear();
        self.cipher.update(body, &mut self.out)?;
        self.cipher.finish(&mut self.out)
    }

    /// Bytes of a session, which may open one, close one, or both.
    fn session(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        let mut rest = input;

        while !rest.is_empty() {
            if !self.started {
                let take = (self.nonce_len - self.nonce.len()).min(rest.len());

                self.nonce.extend_from_slice(&rest[..take]);
                rest = &rest[take..];

                if self.nonce.len() < self.nonce_len {
                    return Ok(());
                }

                self.cipher.start(&self.nonce)?;
                self.nonce.clear();
                self.opened = 0;
                self.started = true;

                continue;
            }

            let take = match self.rotate_after {
                Some(budget) => rest.len().min((budget - self.opened) as usize),
                None => rest.len(),
            };

            self.out.clear();
            self.cipher.update(&rest[..take], &mut self.out)?;

            if !self.out.is_empty() {
                ctx.forward(&self.out);
            }

            self.opened += take as u64;
            rest = &rest[take..];

            if self.rotate_after == Some(self.opened) {
                self.out.clear();
                self.cipher.finish(&mut self.out)?;

                if !self.out.is_empty() {
                    ctx.forward(&self.out);
                }

                self.started = false;
            }
        }

        Ok(())
    }

    /// What happens to a record that would not open.
    fn rejected(&self, ctx: &mut Ctx<'_>, error: PluginError) -> Result<()> {
        match self.on_fail {
            OnFail::Error => Err(error),
            OnFail::Drop => {
                ctx.drop_chunk();
                ctx.log(LogLevel::Warn, "record could not be opened and was dropped");

                Ok(())
            }
            OnFail::Halt => {
                ctx.halt("record could not be opened");

                Ok(())
            }
        }
    }
}

impl Plugin for Decrypt {
    fn name(&self) -> &str {
        DECRYPT
    }

    fn on_bytes(&mut self, ctx: &mut Ctx<'_>, input: &[u8]) -> Result<()> {
        if input.is_empty() {
            return Ok(());
        }

        if self.mode == Mode::Stream {
            return self.session(ctx, input);
        }

        match self.record(input) {
            Ok(()) => {
                ctx.forward(&self.out);

                Ok(())
            }
            Err(error) => self.rejected(ctx, error),
        }
    }

    fn on_eof(&mut self, ctx: &mut Ctx<'_>) -> Result<()> {
        if self.mode == Mode::Record {
            return Ok(());
        }

        if !self.started {
            // Part of a nonce and nothing else is a stream that stopped
            // between the header and the payload.
            return if self.nonce.is_empty() {
                Ok(())
            } else {
                Err(PluginError::runtime(DECRYPT, TRUNCATED))
            };
        }

        self.out.clear();
        self.cipher.finish(&mut self.out)?;

        if !self.out.is_empty() {
            ctx.forward(&self.out);
        }

        self.started = false;

        Ok(())
    }

    fn boundaries(&self) -> Boundaries {
        match self.mode {
            Mode::Record => Boundaries::Preserve,
            Mode::Stream => Boundaries::Fuse,
        }
    }

    fn needs(&self) -> Needs {
        match self.mode {
            // One whole record per call, which is what it reads.
            Mode::Record => Needs::Upstream,
            Mode::Stream => Needs::Nothing,
        }
    }
}

pub struct EncryptFactory;

impl PluginFactory for EncryptFactory {
    fn name(&self) -> &str {
        ENCRYPT
    }

    fn description(&self) -> &str {
        "encrypt this path with a symmetric cipher"
    }

    /// Enough work per byte to be worth a task of its own, as with `compress`.
    fn execution(&self) -> Execution {
        Execution::Detached
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: EncryptConfig = ctx.config()?;
        let spec = config.cipher.spec();

        specified(ENCRYPT, config.nonstandard, spec)?;

        if config.key_out.is_some() && !config.random_key {
            return Err(PluginError::config(
                ENCRYPT,
                "key-out has nothing to report: it names where a random-key is written",
            ));
        }

        let mode = mode(ENCRYPT, config.mode, spec)?;
        let padding = padding(ENCRYPT, config.padding, spec)?;
        let rotate_after = rotation(ENCRYPT, config.rotate_after, mode, padding, spec)?;
        let source = key_source(
            ENCRYPT,
            config.key.as_ref(),
            config.key_file.as_ref(),
            config.key_env.as_ref(),
            config.random_key,
        )?;
        let key = key_material(ENCRYPT, source, config.key_format, spec)?;

        let generated = if config.random_key {
            let channel = ctx.open_channel(key_target(config.key_out.as_ref())?)?;
            let stage = ctx.stage();
            let line = format!(
                "{key}  key ({cipher}) [{label} | {name}]\n",
                key = hex::encode(&key),
                cipher = spec.name,
                label = stage.label(),
                name = stage.name,
            );

            Some((channel, line))
        } else {
            None
        };

        Ok(Stage::filter(Encrypt {
            cipher: sealer(config.cipher, key, padding)?,
            mode,
            nonce: [0; MAX_NONCE],
            nonce_len: spec.nonce,
            random: Random::new(),
            rotate_after,
            fed: 0,
            emitted: 0,
            started: false,
            generated,
            out: Vec::new(),
        }))
    }
}

pub struct DecryptFactory;

impl PluginFactory for DecryptFactory {
    fn name(&self) -> &str {
        DECRYPT
    }

    fn description(&self) -> &str {
        "decrypt this path with a symmetric cipher"
    }

    fn execution(&self) -> Execution {
        Execution::Detached
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: DecryptConfig = ctx.config()?;
        let spec = config.cipher.spec();

        specified(DECRYPT, config.nonstandard, spec)?;

        let mode = mode(DECRYPT, config.mode, spec)?;
        let padding = padding(DECRYPT, config.padding, spec)?;
        // The decrypt side counts the same ciphertext the encrypt side does,
        // so the budget is all it needs from the arithmetic above.
        let rotate_after = rotation(DECRYPT, config.rotate_after, mode, padding, spec)?
            .map(|rotation| rotation.total);
        let on_fail = on_fail(config.on_fail, mode)?;
        let source = key_source(
            DECRYPT,
            config.key.as_ref(),
            config.key_file.as_ref(),
            config.key_env.as_ref(),
            false,
        )?;
        let key = key_material(DECRYPT, source, config.key_format, spec)?;

        Ok(Stage::filter(Decrypt {
            cipher: opener(config.cipher, key, padding)?,
            mode,
            nonce_len: spec.nonce,
            tag_len: spec.tag,
            on_fail,
            rotate_after,
            opened: 0,
            nonce: Vec::new(),
            started: false,
            out: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tocat_api::{
        ChannelId, ChannelTarget, Direction, EffectSink, Emission, HostBuilder, LogLevel,
        PipelineMeta, Result as PluginResult, StageInfo,
    };

    use super::*;

    /// 64 bytes, which is the widest key in the table (`aes-256-siv` takes
    /// two 32-byte keys), and enough for every other cipher once truncated.
    const KEY: &str = concat!(
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f",
    );

    #[derive(Default)]
    struct Recorder {
        logged: Vec<String>,
        halted: Vec<String>,
        written: Vec<String>,
    }

    impl EffectSink for Recorder {
        fn write(&mut self, _channel: ChannelId, bytes: &[u8]) {
            self.written
                .push(String::from_utf8_lossy(bytes).into_owned());
        }

        fn log(&mut self, _level: LogLevel, _stage: &str, message: &str) {
            self.logged.push(message.to_owned());
        }

        fn halt(&mut self, _stage: &str, reason: &str) {
            self.halted.push(reason.to_owned());
        }
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

    fn stage(name: &'static str) -> StageInfo<'static> {
        StageInfo {
            index: 0,
            total: 1,
            name,
            upstream: "src",
            downstream: "sink",
        }
    }

    /// [`KEY`] cut to the length the named cipher takes.
    fn key_for(cipher: &str) -> &'static str {
        &KEY[..cipher_of(cipher).spec().key * 2]
    }

    /// The key every test uses unless it is testing key handling.
    fn keyed_config(cipher: &str, extra: Value) -> Value {
        let key = key_for(cipher);
        let mut config = json!({ "cipher": cipher, "key": key });
        let object = config.as_object_mut().expect("object");

        for (name, value) in extra.as_object().expect("object") {
            object.insert(name.clone(), value.clone());
        }

        config
    }

    fn cipher_of(name: &str) -> Cipher {
        serde_json::from_value(Value::String(name.to_owned())).expect("a known cipher")
    }

    fn try_encrypt(config: Value) -> PluginResult<Box<dyn Plugin>> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let mut ctx = BuildCtx::new(ENCRYPT, &map, &meta, stage(ENCRYPT), &mut host);

        match EncryptFactory.build(&mut ctx)? {
            Stage::Filter(plugin) => Ok(plugin),
            Stage::External(_) => unreachable!("encrypt is a filter"),
        }
    }

    fn try_decrypt(config: Value) -> PluginResult<Box<dyn Plugin>> {
        let map = config.as_object().expect("object").clone();
        let meta = meta();
        let mut host = NullHost;
        let mut ctx = BuildCtx::new(DECRYPT, &map, &meta, stage(DECRYPT), &mut host);

        match DecryptFactory.build(&mut ctx)? {
            Stage::Filter(plugin) => Ok(plugin),
            Stage::External(_) => unreachable!("decrypt is a filter"),
        }
    }

    fn encrypt(cipher: &str, extra: Value) -> Box<dyn Plugin> {
        try_encrypt(keyed_config(cipher, extra)).expect("build")
    }

    fn decrypt(cipher: &str, extra: Value) -> Box<dyn Plugin> {
        try_decrypt(keyed_config(cipher, extra)).expect("build")
    }

    fn feed(plugin: &mut dyn Plugin, sink: &mut Recorder, input: &[u8]) -> Vec<u8> {
        let meta = meta();
        let mut emission = Emission::new();

        {
            let mut ctx = Ctx::new(&meta, "stage", input, &mut emission, sink);
            plugin.on_bytes(&mut ctx, input).expect("on_bytes");
        }

        emission.bytes().to_vec()
    }

    fn try_feed(
        plugin: &mut dyn Plugin,
        sink: &mut Recorder,
        input: &[u8],
    ) -> PluginResult<Vec<u8>> {
        let meta = meta();
        let mut emission = Emission::new();

        {
            let mut ctx = Ctx::new(&meta, "stage", input, &mut emission, sink);
            plugin.on_bytes(&mut ctx, input)?;
        }

        Ok(emission.bytes().to_vec())
    }

    fn eof(plugin: &mut dyn Plugin, sink: &mut Recorder) -> Vec<u8> {
        let meta = meta();
        let mut emission = Emission::new();

        {
            let mut ctx = Ctx::new(&meta, "stage", &[], &mut emission, sink);
            plugin.on_eof(&mut ctx).expect("on_eof");
        }

        emission.bytes().to_vec()
    }

    /// One message through both halves of a pair, which is the property that
    /// matters for every cipher.
    fn round_trip(cipher: &str, extra: Value, message: &[u8]) -> Vec<u8> {
        let mut sink = Recorder::default();
        let mut encoder = encrypt(cipher, extra.clone());
        let mut decoder = decrypt(cipher, extra);

        let mut sealed = feed(&mut *encoder, &mut sink, message);
        sealed.extend_from_slice(&eof(&mut *encoder, &mut sink));

        let mut opened = feed(&mut *decoder, &mut sink, &sealed);
        opened.extend_from_slice(&eof(&mut *decoder, &mut sink));

        opened
    }

    #[test]
    fn every_cipher_round_trips() {
        let message = b"the quick brown fox jumps over the lazy dog, twice over".as_slice();

        for cipher in Cipher::ALL {
            let name = cipher.spec().name;
            let extra = match cipher.spec().standard {
                // A row nothing specifies still has to round trip with
                // itself; it just has to say so first.
                None => json!({ "nonstandard": true }),
                Some(_) => json!({}),
            };

            assert_eq!(
                round_trip(name, extra, message),
                message,
                "{name} did not round trip",
            );
        }
    }

    /// A known answer, read the way a peer would send it.
    ///
    /// The vectors themselves drive the ciphers directly, because the
    /// encrypting side draws its nonce at random and cannot be pinned to a
    /// published answer. Decryption can: a stage handed a nonce and a
    /// published ciphertext has to produce the published plaintext. That
    /// covers the one thing the vectors cannot, which is that the nonce goes
    /// where the other implementation expects to find it.
    ///
    /// One cipher per shape, since what is under test is the framing rather
    /// than the arithmetic: an aead with a tag, a block mode with an iv, one
    /// with none, a buffered stream, a keystream, and the half-iv path.
    #[test]
    fn a_known_answer_decodes_through_the_stage() {
        for name in [
            "aes-256-gcm",
            "aes-256-cbc",
            "aes-256-ecb",
            "aes-256-cfb",
            "chacha20",
            "kuznyechik-ctr",
        ] {
            let cipher = cipher_of(name);
            let (key, nonce, plaintext, ciphertext) = super::vectors::vector(name);
            let mut config = json!({ "cipher": name, "key": hex::encode(&key) });

            // The vectors are whole blocks, so the peer this stands in for
            // pads nothing; asking for padding elsewhere is refused.
            if cipher.spec().family == Family::Block {
                config
                    .as_object_mut()
                    .expect("object")
                    .insert("padding".to_owned(), json!("none"));
            }

            let mut record = nonce.clone();
            record.extend_from_slice(&ciphertext);

            let mut sink = Recorder::default();
            let mut decoder = try_decrypt(config).expect("build");
            let mut opened = feed(&mut *decoder, &mut sink, &record);
            opened.extend_from_slice(&eof(&mut *decoder, &mut sink));

            assert_eq!(
                hex::encode(&opened),
                hex::encode(&plaintext),
                "{name} did not read its own known answer off the wire",
            );
        }
    }

    #[test]
    fn a_record_is_its_nonce_then_its_ciphertext_and_tag() {
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-gcm", json!({}));

        let sealed = feed(&mut *encoder, &mut sink, b"abcd");

        assert_eq!(sealed.len(), 12 + 4 + 16);
        assert_ne!(&sealed[12..16], b"abcd", "the payload must be encrypted");
    }

    #[test]
    fn each_record_gets_its_own_nonce() {
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-gcm", json!({}));

        let first = feed(&mut *encoder, &mut sink, b"abcd");
        let second = feed(&mut *encoder, &mut sink, b"abcd");

        assert_ne!(first, second, "a repeated nonce would break the cipher");
    }

    #[test]
    fn a_stream_is_one_nonce_and_then_the_payload() {
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-ctr", json!({}));

        let first = feed(&mut *encoder, &mut sink, b"abcd");
        let second = feed(&mut *encoder, &mut sink, b"efgh");

        assert_eq!(first.len(), 16 + 4, "the iv rides ahead of the first chunk");
        assert_eq!(second.len(), 4, "and is not repeated");
    }

    #[test]
    fn a_stream_survives_being_cut_anywhere() {
        let message = b"a stream cipher does not care where the chunks fall".as_slice();
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-cbc", json!({}));
        let mut decoder = decrypt("aes-256-cbc", json!({}));

        let mut sealed = Vec::new();

        for chunk in message.chunks(7) {
            sealed.extend_from_slice(&feed(&mut *encoder, &mut sink, chunk));
        }

        sealed.extend_from_slice(&eof(&mut *encoder, &mut sink));

        let mut opened = Vec::new();

        for chunk in sealed.chunks(5) {
            opened.extend_from_slice(&feed(&mut *decoder, &mut sink, chunk));
        }

        opened.extend_from_slice(&eof(&mut *decoder, &mut sink));

        assert_eq!(opened, message);
    }

    #[test]
    fn a_rotating_session_starts_again_at_the_budget() {
        let message = b"0123456789abcdefghij".as_slice();
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-ctr", json!({"rotate-after": 8}));
        let mut decoder = decrypt("aes-256-ctr", json!({"rotate-after": 8}));

        let sealed = feed(&mut *encoder, &mut sink, message);

        // Three sessions: two full budgets and the four bytes left over.
        assert_eq!(sealed.len(), message.len() + 3 * 16);
        assert_eq!(feed(&mut *decoder, &mut sink, &sealed), message);
    }

    #[test]
    fn rotation_lands_in_the_same_place_however_the_bytes_arrive() {
        let message = b"0123456789abcdefghij".as_slice();
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-ctr", json!({"rotate-after": 8}));
        let mut sealed = Vec::new();

        for chunk in message.chunks(3) {
            sealed.extend_from_slice(&feed(&mut *encoder, &mut sink, chunk));
        }

        let mut decoder = decrypt("aes-256-ctr", json!({"rotate-after": 8}));
        let mut opened = Vec::new();

        for chunk in sealed.chunks(11) {
            opened.extend_from_slice(&feed(&mut *decoder, &mut sink, chunk));
        }

        assert_eq!(opened, message);
    }

    #[test]
    fn a_block_session_rotates_on_a_ciphertext_boundary() {
        let message: Vec<u8> = (0..96u32).map(|byte| byte as u8).collect();
        let extra = json!({"rotate-after": 64});
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-cbc", extra.clone());

        let mut sealed = feed(&mut *encoder, &mut sink, &message);
        sealed.extend_from_slice(&eof(&mut *encoder, &mut sink));

        // A full session is its iv and its 64 bytes of ciphertext, three
        // blocks of payload and one of padding. The stream then ends part way
        // through the next one.
        assert_eq!(sealed.len(), 16 + 64 + 16 + 48);
        assert_eq!(round_trip("aes-256-cbc", extra, &message), message);
    }

    #[test]
    fn a_block_rotation_lands_in_the_same_place_however_the_bytes_arrive() {
        let message: Vec<u8> = (0..200u32).map(|byte| (byte * 7) as u8).collect();
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-128-cbc", json!({"rotate-after": 48}));
        let mut sealed = Vec::new();

        for chunk in message.chunks(13) {
            sealed.extend_from_slice(&feed(&mut *encoder, &mut sink, chunk));
        }

        sealed.extend_from_slice(&eof(&mut *encoder, &mut sink));

        let mut decoder = decrypt("aes-128-cbc", json!({"rotate-after": 48}));
        let mut opened = Vec::new();

        for chunk in sealed.chunks(7) {
            opened.extend_from_slice(&feed(&mut *decoder, &mut sink, chunk));
        }

        opened.extend_from_slice(&eof(&mut *decoder, &mut sink));

        assert_eq!(opened, message);
    }

    #[test]
    fn a_block_rotation_budget_has_to_fit_the_blocks() {
        assert!(
            try_encrypt(keyed_config("aes-256-cbc", json!({"rotate-after": 24}))).is_err(),
            "a session ends on a block boundary",
        );
        assert!(
            try_encrypt(keyed_config("aes-256-cbc", json!({"rotate-after": 16}))).is_err(),
            "one block is all padding and no room for payload",
        );
        assert!(
            try_encrypt(keyed_config("aes-256-cbc", json!({"rotate-after": 32}))).is_ok(),
            "two blocks is the smallest padded session",
        );

        let unpadded = json!({"rotate-after": 16, "padding": "none"});

        assert!(
            try_encrypt(keyed_config("aes-256-cbc", unpadded.clone())).is_ok(),
            "with nothing padding, one block is a whole session",
        );
        assert_eq!(
            round_trip("aes-256-cbc", unpadded, &[7u8; 64]),
            [7u8; 64],
            "an unpadded session is exactly its budget",
        );
    }

    #[test]
    fn a_tampered_record_is_refused() {
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-gcm", json!({}));
        let mut decoder = decrypt("aes-256-gcm", json!({}));

        let mut sealed = feed(&mut *encoder, &mut sink, b"abcd");
        let last = sealed.len() - 1;
        sealed[last] ^= 1;

        assert!(try_feed(&mut *decoder, &mut sink, &sealed).is_err());
    }

    #[test]
    fn a_tampered_record_can_be_dropped_instead() {
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-gcm", json!({}));
        let mut decoder = decrypt("aes-256-gcm", json!({"on-fail": "drop"}));

        let mut sealed = feed(&mut *encoder, &mut sink, b"abcd");
        sealed[0] ^= 1;

        let opened = try_feed(&mut *decoder, &mut sink, &sealed).expect("dropped, not failed");

        assert!(opened.is_empty(), "nothing may reach the far side");
        assert_eq!(sink.logged.len(), 1, "and the drop is reported");
    }

    #[test]
    fn a_tampered_record_can_halt_the_path() {
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-gcm", json!({}));
        let mut decoder = decrypt("aes-256-gcm", json!({"on-fail": "halt"}));

        let mut sealed = feed(&mut *encoder, &mut sink, b"abcd");
        sealed[0] ^= 1;

        try_feed(&mut *decoder, &mut sink, &sealed).expect("halted, not failed");

        assert_eq!(sink.halted.len(), 1);
    }

    #[test]
    fn a_truncated_record_is_refused() {
        let mut sink = Recorder::default();
        let mut decoder = decrypt("aes-256-gcm", json!({}));

        assert!(try_feed(&mut *decoder, &mut sink, &[0u8; 20]).is_err());
    }

    #[test]
    fn the_wrong_key_does_not_decode() {
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-gcm", json!({}));
        let sealed = feed(&mut *encoder, &mut sink, b"abcd");

        let mut decoder = try_decrypt(json!({
            "cipher": "aes-256-gcm",
            "key": "f".repeat(64),
        }))
        .expect("build");

        assert!(try_feed(&mut *decoder, &mut sink, &sealed).is_err());
    }

    #[test]
    fn padding_can_be_turned_off_for_a_block_aligned_peer() {
        assert_eq!(
            round_trip("aes-256-cbc", json!({"padding": "none"}), &[7u8; 32]),
            [7u8; 32],
        );
    }

    #[test]
    fn an_unaligned_message_is_refused_when_nothing_pads() {
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-cbc", json!({"padding": "none"}));

        feed(&mut *encoder, &mut sink, &[7u8; 20]);

        let meta = meta();
        let mut emission = Emission::new();
        let mut ctx = Ctx::new(&meta, "stage", &[], &mut emission, &mut sink);

        assert!(encoder.on_eof(&mut ctx).is_err());
    }

    #[test]
    fn an_aead_cipher_refuses_stream_mode() {
        assert!(try_encrypt(keyed_config("aes-256-gcm", json!({"mode": "stream"}))).is_err());
        assert!(try_encrypt(keyed_config("aes-256-gcm", json!({"mode": "record"}))).is_ok());
    }

    #[test]
    fn the_mode_defaults_by_family() {
        assert_eq!(
            encrypt("aes-256-gcm", json!({})).boundaries(),
            Boundaries::Preserve,
        );
        assert_eq!(
            encrypt("aes-256-ctr", json!({})).boundaries(),
            Boundaries::Fuse,
        );
    }

    #[test]
    fn a_record_stage_needs_its_boundaries() {
        assert_eq!(encrypt("aes-256-gcm", json!({})).needs(), Needs::Downstream);
        assert_eq!(decrypt("aes-256-gcm", json!({})).needs(), Needs::Upstream);
        assert_eq!(encrypt("aes-256-ctr", json!({})).needs(), Needs::Nothing);
        assert_eq!(decrypt("aes-256-ctr", json!({})).needs(), Needs::Nothing);
    }

    #[test]
    fn an_option_that_would_do_nothing_is_refused() {
        assert!(
            try_encrypt(keyed_config("aes-256-gcm", json!({"padding": "none"}))).is_err(),
            "gcm has no blocks to pad",
        );
        assert!(
            try_encrypt(keyed_config("aes-256-ecb", json!({"rotate-after": "1M"}))).is_err(),
            "ecb has no nonce to replace",
        );
        assert!(
            try_encrypt(keyed_config(
                "aes-256-ctr",
                json!({"mode": "record", "rotate-after": "1M"}),
            ))
            .is_err(),
            "a record already carries its own nonce",
        );
        assert!(
            try_decrypt(keyed_config("aes-256-ctr", json!({"on-fail": "drop"}))).is_err(),
            "dropping part of a session desynchronises the rest",
        );
    }

    #[test]
    fn exactly_one_key_is_required() {
        assert!(try_encrypt(json!({"cipher": "aes-256-gcm"})).is_err());
        assert!(
            try_encrypt(json!({
                "cipher": "aes-256-gcm",
                "key": key_for("aes-256-gcm"),
                "key-env": "TOCAT_TEST_KEY",
            }))
            .is_err()
        );
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused() {
        assert!(try_encrypt(json!({"cipher": "aes-256-gcm", "key": "0011"})).is_err());
        assert!(try_encrypt(json!({"cipher": "aes-128-gcm", "key": KEY})).is_err());
    }

    #[test]
    fn a_key_can_be_written_however_it_was_stored() {
        let hex = &KEY[..32];
        let base64 = BASE64_STANDARD.encode(hex::decode(hex).expect("hex"));
        let mut sink = Recorder::default();

        // The same key in two encodings has to be the same key, which is what
        // sealing with one and opening with the other proves.
        let mut encoder = encrypt("aes-128-ctr", json!({}));
        let mut decoder = try_decrypt(json!({
            "cipher": "aes-128-ctr",
            "key": base64,
            "key-format": "base64",
        }))
        .expect("build");

        let sealed = feed(&mut *encoder, &mut sink, b"abcd");

        assert_eq!(feed(&mut *decoder, &mut sink, &sealed), b"abcd");
    }

    #[test]
    fn a_generated_key_is_reported_once() {
        let mut sink = Recorder::default();
        let mut encoder = try_encrypt(json!({
            "cipher": "aes-256-gcm",
            "random-key": true,
        }))
        .expect("build");

        feed(&mut *encoder, &mut sink, b"abcd");
        feed(&mut *encoder, &mut sink, b"abcd");

        assert_eq!(sink.written.len(), 1);
        assert!(sink.written[0].contains("key (aes-256-gcm)"));
    }

    #[test]
    fn a_generated_key_is_reported_even_on_an_empty_path() {
        let mut sink = Recorder::default();
        let mut encoder = try_encrypt(json!({
            "cipher": "aes-256-gcm",
            "random-key": true,
        }))
        .expect("build");

        eof(&mut *encoder, &mut sink);

        assert_eq!(sink.written.len(), 1);
    }

    #[test]
    fn a_key_cannot_be_written_where_payload_goes() {
        assert!(
            try_encrypt(json!({
                "cipher": "aes-256-gcm",
                "random-key": true,
                "key-out": "stdout",
            }))
            .is_err()
        );
    }

    #[test]
    fn key_out_without_a_generated_key_is_refused() {
        assert!(try_encrypt(keyed_config("aes-256-gcm", json!({"key-out": "-"}))).is_err());
    }

    #[test]
    fn the_cipher_is_spelled_however_you_like() {
        for spelling in ["aes-256-gcm", "AES256GCM", "aes_256_gcm"] {
            assert!(
                try_encrypt(json!({"cipher": spelling, "key": key_for("aes-256-gcm")})).is_ok(),
                "{spelling} did not name aes-256-gcm",
            );
        }
    }

    #[test]
    fn an_unknown_cipher_or_option_is_refused() {
        let key = key_for("aes-256-gcm");

        assert!(try_encrypt(json!({"cipher": "rot13", "key": key})).is_err());
        assert!(try_encrypt(json!({"cipher": "aes-256-gcm", "key": key, "rounds": 3})).is_err());
    }

    /// Both halves refuse it, and each says so in its own name. The decrypt
    /// side reporting `encrypt` would send someone to the wrong stage.
    #[test]
    fn a_cipher_no_document_specifies_needs_the_opt_in() {
        let cipher = "sm4-gcm-siv";
        let key = key_for(cipher);
        let plain = json!({ "cipher": cipher, "key": key });
        let opted = json!({ "cipher": cipher, "key": key, "nonstandard": true });

        let Err(refused) = try_encrypt(plain.clone()) else {
            panic!("encrypt built {cipher} without the opt-in");
        };
        assert!(refused.to_string().contains(cipher), "{refused}");

        let Err(refused) = try_decrypt(plain) else {
            panic!("decrypt built {cipher} without the opt-in");
        };
        assert!(refused.to_string().contains(DECRYPT), "{refused}");

        assert!(try_encrypt(opted.clone()).is_ok());
        assert!(try_decrypt(opted).is_ok());
    }

    #[test]
    fn an_empty_message_emits_nothing() {
        let mut sink = Recorder::default();
        let mut encoder = encrypt("aes-256-gcm", json!({}));

        assert!(feed(&mut *encoder, &mut sink, b"").is_empty());
    }
}
