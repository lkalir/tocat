//! Known-answer tests for the cipher table.
//!
//! The compile-time assertions in [`super::macros`] check that a row's
//! declared lengths are the ones its crate takes. What they cannot check is
//! whether the bytes on the wire are the ones the row's `standard` promises:
//! a row can have the right lengths, round trip against itself, and still
//! interoperate with nothing. That is what these vectors are for.
//!
//! Every vector's bytes come from somewhere other than this crate, recorded
//! in [`Vector::source`]. There are two kinds, and the distinction matters
//! when one fails:
//!
//! - A document reference means the bytes were transcribed from the standard
//!   that specifies them. Where OpenSSL implements the same cipher, the
//!   transcription was confirmed against it before being committed here, so a
//!   typo would already have been caught.
//! - An `OpenSSL 3.6.3` reference means no document publishes bytes for the
//!   combination, but the row's `standard` composes a mode and a primitive that
//!   between them determine the answer, and a second implementation computes
//!   it. That is a weaker claim than a published vector but a much stronger one
//!   than a value this crate produced itself.
//!
//! Rows that can have neither are named in [`UNVERIFIED`], with the reason.
//! [`every_standard_cipher_has_a_known_answer`] holds that list to its word.

use super::*;

/// One cipher's worth of known answer.
struct Vector {
    /// The declared name of the row, as [`Spec::name`] spells it.
    cipher: &'static str,
    /// Where these bytes come from. See the module comment.
    source: &'static str,
    key: &'static str,
    /// Empty for a cipher that takes no nonce or iv.
    nonce: &'static str,
    plaintext: &'static str,
    /// Exactly what the cipher emits, tag included, which is what a record
    /// carries. Usually that means the tag last, but `aes-siv` puts it first
    /// as RFC 5297 specifies, so this is not `plaintext` followed by a tag.
    ciphertext: &'static str,
}

/// The table. One entry per standard row that a known answer can reach.
#[rustfmt::skip]
const VECTORS: &[Vector] = &[
    Vector {
        cipher: "aes-128-cbc",
        source: "NIST SP 800-38A F.2",
        key: "2b7e151628aed2a6abf7158809cf4f3c",
        nonce: "000102030405060708090a0b0c0d0e0f",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2",
    },
    Vector {
        cipher: "aes-192-cbc",
        source: "NIST SP 800-38A F.2",
        key: "8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b",
        nonce: "000102030405060708090a0b0c0d0e0f",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "4f021db243bc633d7178183a9fa071e8b4d9ada9ad7dedf4e5e738763f69145a",
    },
    Vector {
        cipher: "aes-256-cbc",
        source: "NIST SP 800-38A F.2",
        key: "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4",
        nonce: "000102030405060708090a0b0c0d0e0f",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "f58c4c04d6e5f1ba779eabfb5f7bfbd69cfc4e967edb808d679f777bc6702c7d",
    },
    Vector {
        cipher: "aria-128-cbc",
        source: "OpenSSL 3.6.3 aria-128-cbc",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "61279818f092e5f1e8b67638f745d891ab64e250c9ffc0ec467c8c9c7023a606",
    },
    Vector {
        cipher: "aria-192-cbc",
        source: "OpenSSL 3.6.3 aria-192-cbc",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "b017b69d7e4d1098c1d344a51f4aafd9d18d06cc58ff4b9153ca5e1b696278c9",
    },
    Vector {
        cipher: "aria-256-cbc",
        source: "OpenSSL 3.6.3 aria-256-cbc",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "c85c85fa414e4125cb84dcde78875ced17d36b3828e65d9a136667aa357fe7c8",
    },
    Vector {
        cipher: "camellia-128-cbc",
        source: "OpenSSL 3.6.3 camellia-128-cbc",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "030a8151dd6b6bc63bfa77245c73fb86118a4ac15f9ae0952a52086c343facd5",
    },
    Vector {
        cipher: "camellia-192-cbc",
        source: "OpenSSL 3.6.3 camellia-192-cbc",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "f77d9ac9e159776d20482f82c13f6465d1634bdc7083a22bfe27ccc9525cbf33",
    },
    Vector {
        cipher: "camellia-256-cbc",
        source: "OpenSSL 3.6.3 camellia-256-cbc",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "4fd9e502118bc675ba7eafa78fbdea7fce2aa7f1e554975f4888479751e6450b",
    },
    Vector {
        cipher: "sm4-cbc",
        source: "OpenSSL 3.6.3 sm4-cbc",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "bdf1adb0355535205042f5b26edfdba8524ad798b5bb8d62fb470810e1214126",
    },
    Vector {
        cipher: "des-cbc",
        source: "OpenSSL 3.6.3 des-cbc",
        key: "4041424344454647",
        nonce: "cafebabefacedbad",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c",
        ciphertext: "f21cc744e1d6b5c84b0401c6f7c93ddeae33ee5dd2c467f2",
    },
    Vector {
        cipher: "des-ede3-cbc",
        source: "OpenSSL 3.6.3 des-ede3-cbc",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbad",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c",
        ciphertext: "185c86d6251b4223903e7018df47f36f9f7c930aeff033c9",
    },
    Vector {
        cipher: "aes-128-ecb",
        source: "NIST SP 800-38A F.1",
        key: "2b7e151628aed2a6abf7158809cf4f3c",
        nonce: "",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "3ad77bb40d7a3660a89ecaf32466ef97f5d3d58503b9699de785895a96fdbaaf",
    },
    Vector {
        cipher: "aes-192-ecb",
        source: "NIST SP 800-38A F.1",
        key: "8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b",
        nonce: "",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "bd334f1d6e45f25ff712a214571fa5cc974104846d0ad3ad7734ecb3ecee4eef",
    },
    Vector {
        cipher: "aes-256-ecb",
        source: "NIST SP 800-38A F.1",
        key: "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4",
        nonce: "",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "f3eed1bdb5d2a03c064b5a7e3db181f8591ccb10d410ed26dc5ba74a31362870",
    },
    Vector {
        cipher: "aria-128-ecb",
        source: "RFC 5794 section 5",
        key: "000102030405060708090a0b0c0d0e0f",
        nonce: "",
        plaintext: "00112233445566778899aabbccddeeff",
        ciphertext: "d718fbd6ab644c739da95f3be6451778",
    },
    Vector {
        cipher: "aria-192-ecb",
        source: "RFC 5794 section 5",
        key: "000102030405060708090a0b0c0d0e0f1011121314151617",
        nonce: "",
        plaintext: "00112233445566778899aabbccddeeff",
        ciphertext: "26449c1805dbe7aa25a468ce263a9e79",
    },
    Vector {
        cipher: "aria-256-ecb",
        source: "RFC 5794 section 5",
        key: "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        nonce: "",
        plaintext: "00112233445566778899aabbccddeeff",
        ciphertext: "f92bd7c79fb72e2f2b8f80c1972d24fc",
    },
    Vector {
        cipher: "camellia-128-ecb",
        source: "RFC 3713 section 4",
        key: "0123456789abcdeffedcba9876543210",
        nonce: "",
        plaintext: "0123456789abcdeffedcba9876543210",
        ciphertext: "67673138549669730857065648eabe43",
    },
    Vector {
        cipher: "camellia-192-ecb",
        source: "RFC 3713 section 4",
        key: "0123456789abcdeffedcba98765432100011223344556677",
        nonce: "",
        plaintext: "0123456789abcdeffedcba9876543210",
        ciphertext: "b4993401b3e996f84ee5cee7d79b09b9",
    },
    Vector {
        cipher: "camellia-256-ecb",
        source: "RFC 3713 section 4",
        key: "0123456789abcdeffedcba987654321000112233445566778899aabbccddeeff",
        nonce: "",
        plaintext: "0123456789abcdeffedcba9876543210",
        ciphertext: "9acc237dff16d76c20ef7c919e3a7509",
    },
    Vector {
        cipher: "magma-ecb",
        source: "GOST R 34.13-2015 A.2.1",
        key: "ffeeddccbbaa99887766554433221100f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
        nonce: "",
        plaintext: "92def06b3c130a59db54c704f8189d204a98fb2e67a8024c8912409b17b57e41",
        ciphertext: "2b073f0494f372a0de70e715d3556e4811d8d9e9eacfbc1e7c68260996c67efb",
    },
    Vector {
        cipher: "kuznyechik-ecb",
        source: "GOST R 34.13-2015 A.1.1",
        key: "8899aabbccddeeff0011223344556677fedcba98765432100123456789abcdef",
        nonce: "",
        plaintext: concat!(
            "1122334455667700ffeeddccbbaa998800112233445566778899aabbcceeff0a",
            "112233445566778899aabbcceeff0a002233445566778899aabbcceeff0a0011",
        ),
        ciphertext: concat!(
            "7f679d90bebc24305a468d42b9d4edcdb429912c6e0032f9285452d76718d08b",
            "f0ca33549d247ceef3f5a5313bd4b157d0b09ccde830b9eb3a02c4c5aa8ada98",
        ),
    },
    Vector {
        cipher: "sm4-ecb",
        source: "GB/T 32907-2016 appendix A",
        key: "0123456789abcdeffedcba9876543210",
        nonce: "",
        plaintext: "0123456789abcdeffedcba9876543210",
        ciphertext: "681edf34d206965e86b3e94f536e4246",
    },
    Vector {
        cipher: "des-ecb",
        source: "OpenSSL 3.6.3 des-ecb",
        key: "4041424344454647",
        nonce: "",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c",
        ciphertext: "853846b3fc2a7682bee8702952acb7f74eabd1434fa0d86f",
    },
    Vector {
        cipher: "des-ede3-ecb",
        source: "OpenSSL 3.6.3 des-ede3-ecb",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c",
        ciphertext: "7a39e0c3f2eccc92262bba99a07fb5512af66348fea7a604",
    },
    Vector {
        cipher: "aes-128-cfb",
        source: "NIST SP 800-38A F.3 (CFB128)",
        key: "2b7e151628aed2a6abf7158809cf4f3c",
        nonce: "000102030405060708090a0b0c0d0e0f",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "3b3fd92eb72dad20333449f8e83cfb4ac8a64537a0b3a93fcde3cdad9f1ce58b",
    },
    Vector {
        cipher: "aes-192-cfb",
        source: "NIST SP 800-38A F.3 (CFB128)",
        key: "8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b",
        nonce: "000102030405060708090a0b0c0d0e0f",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "cdc80d6fddf18cab34c25909c99a417467ce7f7f81173621961a2b70171d3d7a",
    },
    Vector {
        cipher: "aes-256-cfb",
        source: "NIST SP 800-38A F.3 (CFB128)",
        key: "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4",
        nonce: "000102030405060708090a0b0c0d0e0f",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "dc7e84bfda79164b7ecd8486985d386039ffed143b28b1c832113c6331e5407b",
    },
    Vector {
        cipher: "aria-128-cfb",
        source: "OpenSSL 3.6.3 aria-128-cfb",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "ee57315ff87d4df6162dea0332c2d539619feaf228764e100cfd876d1e9277e6",
    },
    Vector {
        cipher: "aria-192-cfb",
        source: "OpenSSL 3.6.3 aria-192-cfb",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "f18e5a4045d9c5b7a9f6de1b527ade2c509bd1464490c636044fa027cc249752",
    },
    Vector {
        cipher: "aria-256-cfb",
        source: "OpenSSL 3.6.3 aria-256-cfb",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "389854ea8f5ba820605e294f7f033e1bb39b726f697b55112399b2b6caff29e4",
    },
    Vector {
        cipher: "camellia-128-cfb",
        source: "OpenSSL 3.6.3 camellia-128-cfb",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "06b1ed2e6703945ba676d9fe0604409f39eceb37420d408c9dd42c8d4b92d645",
    },
    Vector {
        cipher: "camellia-192-cfb",
        source: "OpenSSL 3.6.3 camellia-192-cfb",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "f793b4d09957346fdc9cc0ccef1a6fa84c19d641478df42bc8327436a045a624",
    },
    Vector {
        cipher: "camellia-256-cfb",
        source: "OpenSSL 3.6.3 camellia-256-cfb",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "781c315aa96fdd5e136a75b03131f6b8c71c3eafa4104b375faaf5c7416b0fa0",
    },
    Vector {
        cipher: "sm4-cfb",
        source: "OpenSSL 3.6.3 sm4-cfb",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "c03e3034454b631d3ce901ed94dfec5fc67cfe7c24170ad09e3be62c8d59915a",
    },
    Vector {
        cipher: "des-cfb",
        source: "OpenSSL 3.6.3 des-cfb",
        key: "4041424344454647",
        nonce: "cafebabefacedbad",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "64a3c70c4ba547c40b4d274c92b4e4752cd35a31ec91e1ded6e5d492a23d61e0",
    },
    Vector {
        cipher: "des-ede3-cfb",
        source: "OpenSSL 3.6.3 des-ede3-cfb",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbad",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "72cc810529f2f33bf6be2eb705156dfbf06f387edf927c86205439c5038e022c",
    },
    Vector {
        cipher: "aes-128-ctr",
        source: "NIST SP 800-38A F.5",
        key: "2b7e151628aed2a6abf7158809cf4f3c",
        nonce: "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "874d6191b620e3261bef6864990db6ce9806f66b7970fdff8617187bb9fffdff",
    },
    Vector {
        cipher: "aes-192-ctr",
        source: "NIST SP 800-38A F.5",
        key: "8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b",
        nonce: "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "1abc932417521ca24f2b0459fe7e6e0b090339ec0aa6faefd5ccc2c6f4ce8e94",
    },
    Vector {
        cipher: "aes-256-ctr",
        source: "NIST SP 800-38A F.5",
        key: "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4",
        nonce: "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "601ec313775789a5b7a7f504bbf3d228f443e3ca4d62b59aca84e990cacaf5c5",
    },
    Vector {
        cipher: "aria-128-ctr",
        source: "OpenSSL 3.6.3 aria-128-ctr",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "ee57315ff87d4df6162dea0332c2d53950b21688e05ad8feeb09700527073470",
    },
    Vector {
        cipher: "aria-192-ctr",
        source: "OpenSSL 3.6.3 aria-192-ctr",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "f18e5a4045d9c5b7a9f6de1b527ade2c3a98e3aeb64babdbd3aaa366dc69e844",
    },
    Vector {
        cipher: "aria-256-ctr",
        source: "OpenSSL 3.6.3 aria-256-ctr",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "389854ea8f5ba820605e294f7f033e1b144bdd415ad74a7102da5dc7abf5e404",
    },
    Vector {
        cipher: "camellia-128-ctr",
        source: "RFC 5528 section 4.1 TV#1",
        key: "ae6852f8121067cc4bf7a5765577f39e",
        nonce: "00000030000000000000000000000001",
        plaintext: "53696e676c6520626c6f636b206d7367",
        ciphertext: "d09dc29a8214619a20877c76db1f0b3f",
    },
    Vector {
        cipher: "camellia-192-ctr",
        source: "RFC 5528 section 4.1 TV#4",
        key: "16af5b145fc9f579c175f93e3bfb0eed863d06ccfdb78515",
        nonce: "0000004836733c147d6d93cb00000001",
        plaintext: "53696e676c6520626c6f636b206d7367",
        ciphertext: "2379399e8a8d2b2b16702fc78b9e9696",
    },
    Vector {
        cipher: "camellia-256-ctr",
        source: "RFC 5528 section 4.1 TV#7",
        key: "776beff2851db06f4c8a0542c8696f6c6a81af1eec96b4d37fc1d689e6c1c104",
        nonce: "00000060db5672c97aa8f0b200000001",
        plaintext: "53696e676c6520626c6f636b206d7367",
        ciphertext: "3401f9c8247effcebd6994714c1bbb11",
    },
    Vector {
        cipher: "sm4-ctr",
        source: "OpenSSL 3.6.3 sm4-ctr",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "c03e3034454b631d3ce901ed94dfec5fa4a43afefb31bdae23801bb6d8ce1961",
    },
    Vector {
        cipher: "aes-128-ofb",
        source: "NIST SP 800-38A F.4",
        key: "2b7e151628aed2a6abf7158809cf4f3c",
        nonce: "000102030405060708090a0b0c0d0e0f",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "3b3fd92eb72dad20333449f8e83cfb4a7789508d16918f03f53c52dac54ed825",
    },
    Vector {
        cipher: "aes-192-ofb",
        source: "NIST SP 800-38A F.4",
        key: "8e73b0f7da0e6452c810f32b809079e562f8ead2522c6b7b",
        nonce: "000102030405060708090a0b0c0d0e0f",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "cdc80d6fddf18cab34c25909c99a4174fcc28b8d4c63837c09e81700c1100401",
    },
    Vector {
        cipher: "aes-256-ofb",
        source: "NIST SP 800-38A F.4",
        key: "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4",
        nonce: "000102030405060708090a0b0c0d0e0f",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "dc7e84bfda79164b7ecd8486985d38604febdc6740d20b3ac88f6ad82a4fb08d",
    },
    Vector {
        cipher: "aria-128-ofb",
        source: "OpenSSL 3.6.3 aria-128-ofb",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "ee57315ff87d4df6162dea0332c2d539d45397f2973571333d8d9a561c301ade",
    },
    Vector {
        cipher: "aria-192-ofb",
        source: "OpenSSL 3.6.3 aria-192-ofb",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "f18e5a4045d9c5b7a9f6de1b527ade2c33568cf25c72153a139035cef22beca5",
    },
    Vector {
        cipher: "aria-256-ofb",
        source: "OpenSSL 3.6.3 aria-256-ofb",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "389854ea8f5ba820605e294f7f033e1b8f6583f736a4f6a1d762b08fe3bb9f65",
    },
    Vector {
        cipher: "camellia-128-ofb",
        source: "OpenSSL 3.6.3 camellia-128-ofb",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "06b1ed2e6703945ba676d9fe0604409ffd6acf42efef29b13b9683e0c7dcc537",
    },
    Vector {
        cipher: "camellia-192-ofb",
        source: "OpenSSL 3.6.3 camellia-192-ofb",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "f793b4d09957346fdc9cc0ccef1a6fa8c6acd668b3a9e372f9f36ee167ffb29b",
    },
    Vector {
        cipher: "camellia-256-ofb",
        source: "OpenSSL 3.6.3 camellia-256-ofb",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "781c315aa96fdd5e136a75b03131f6b8b4e569c7e517a278627e7c610e563d0f",
    },
    Vector {
        cipher: "sm4-ofb",
        source: "OpenSSL 3.6.3 sm4-ofb",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "c03e3034454b631d3ce901ed94dfec5f0a5767018cbee362d52a7c643ef5b2df",
    },
    Vector {
        cipher: "des-ofb",
        source: "OpenSSL 3.6.3 des-ofb",
        key: "4041424344454647",
        nonce: "cafebabefacedbad",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "64a3c70c4ba547c48d317a833f18780de3f10e11490c51e5e8ce636223644dd4",
    },
    Vector {
        cipher: "des-ede3-ofb",
        source: "OpenSSL 3.6.3 des-ede3-ofb",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbad",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: "72cc810529f2f33b57f1bd4b3a3827ead8d7a028a9d0b678137aa5c057a80396",
    },
    Vector {
        cipher: "chacha20",
        source: "RFC 8439 appendix A.2 test vector #1",
        key: "0000000000000000000000000000000000000000000000000000000000000000",
        nonce: "000000000000000000000000",
        plaintext: concat!(
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ),
        ciphertext: concat!(
            "76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7",
            "da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586",
        ),
    },
    Vector {
        cipher: "xchacha20",
        source: "draft-irtf-cfrg-xchacha-03 A.3.2.1 (first 64 bytes)",
        key: "808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f",
        nonce: "404142434445464748494a4b4c4d4e4f5051525354555658",
        plaintext: concat!(
            "5468652064686f6c65202870726f6e6f756e6365642022646f6c652229206973",
            "20616c736f206b6e6f776e2061732074686520417369617469632077696c6420",
        ),
        ciphertext: concat!(
            "4559abba4e48c16102e8bb2c05e6947f50a786de162f9b0b7e592a9b53d0d4e9",
            "8d8d6410d540a1a6375b26d80dace4fab52384c731acbf16a5923c0c48d3575d",
        ),
    },
    Vector {
        cipher: "salsa20",
        source: "eSTREAM salsa20-full-verified.test-vectors, 256-bit key, Set 6 #0",
        key: "0053a6f94c9ff24598eb3e91e4378add3083d6297ccf2275c81b6ec11467ba0d",
        nonce: "0d74db42a91077de",
        plaintext: concat!(
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ),
        ciphertext: concat!(
            "f5fad53f79f9df58c4aea0d0ed9a9601f278112ca7180d565b420a48019670ea",
            "f24ce493a86263f677b46ace1924773d2bb25571e1aa8593758fc382b1280b71",
        ),
    },
    Vector {
        cipher: "xsalsa20",
        source:
            "Crypto++ TestVectors/salsa.txt XSalsa20, from naclcrypto-20090308 (first 64 bytes)",
        key: "1b27556473e985d462cd51197a9a46c76009549eac6474f206c4ee0844f68389",
        nonce: "69696ee955b62b73cd62bda875fc73d68219e0036b7a0b37",
        plaintext: concat!(
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ),
        ciphertext: concat!(
            "eea6a7251c1e72916d11c2cb214d3c252539121d8e234e652d651fa4c8cff880",
            "309e645a74e9e0a60d8243acd9177ab51a1beb8d5a2f5d700c093c5e55855796",
        ),
    },
    Vector {
        cipher: "magma-ctr",
        source: "GOST R 34.13-2015 A.2.2",
        key: "ffeeddccbbaa99887766554433221100f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
        nonce: "12345678",
        plaintext: "92def06b3c130a59db54c704f8189d204a98fb2e67a8024c8912409b17b57e41",
        ciphertext: "4e98110c97b7b93c3e250d93d6e85d69136d868807b2dbef568eb680ab52a12d",
    },
    Vector {
        cipher: "kuznyechik-ctr",
        source: "GOST R 34.13-2015 A.1.2",
        key: "8899aabbccddeeff0011223344556677fedcba98765432100123456789abcdef",
        nonce: "1234567890abcef0",
        plaintext: concat!(
            "1122334455667700ffeeddccbbaa998800112233445566778899aabbcceeff0a",
            "112233445566778899aabbcceeff0a002233445566778899aabbcceeff0a0011",
        ),
        ciphertext: concat!(
            "f195d8bec10ed1dbd57b5fa240bda1b885eee733f6a13e5df33ce4b33c45dee4",
            "a5eae88be6356ed3d5e877f13564a3a5cb91fab1f20cbab6d1c6d15820bdba73",
        ),
    },
    Vector {
        cipher: "rc4",
        source: "RFC 6229 section 2, 32-byte key",
        key: "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
        nonce: "",
        plaintext: "00000000000000000000000000000000",
        ciphertext: "eaa6bd25880bf93d3f5d1e4ca2611d91",
    },
    Vector {
        cipher: "aes-128-ccm",
        source: "OpenSSL 3.6.3 aes-128-ccm",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "a386b7e50cc10ddad62d6b81d0b579f4e68a5dc416ddba475b6fc08d15b1073b",
            "f560f71fa9cb00e8a6e19ec736ee43dd",
        ),
    },
    Vector {
        cipher: "aes-192-ccm",
        source: "OpenSSL 3.6.3 aes-192-ccm",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "7465ec4985ee04c29534281116ce6f222e6ab425f5e2ae79532e21458248ade3",
            "6101a7e42f566c1332526932e9d16fed",
        ),
    },
    Vector {
        cipher: "aes-256-ccm",
        source: "OpenSSL 3.6.3 aes-256-ccm",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "4e04ecafb058e6f5488113f5442cddda7af3855a99e55847298e2e965bd65b5c",
            "034aae9164b040ac67f2159c83a4a450",
        ),
    },
    Vector {
        cipher: "aria-128-ccm",
        source: "OpenSSL 3.6.3 aria-128-ccm",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "6a6af6ee9e985fb7a045abfb1e80da77030e02922376c7b85d5987589be57482",
            "9d2fbe2d304339a4d5bc3a6280b835c7",
        ),
    },
    Vector {
        cipher: "aria-192-ccm",
        source: "OpenSSL 3.6.3 aria-192-ccm",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "29fe242624866f069d9f51b1c226c5a237d9abd05b7ae816949a962e84927eaa",
            "63a4c7dfab3a3d190f4b28a998053bca",
        ),
    },
    Vector {
        cipher: "aria-256-ccm",
        source: "OpenSSL 3.6.3 aria-256-ccm",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "eafcc997f5e988fe4329411ff25c2197766010973deaaaa4b925736224be6367",
            "0f9815dd85d83a1172dcfd7916def313",
        ),
    },
    Vector {
        cipher: "sm4-ccm",
        source: "OpenSSL 3.6.3 sm4-ccm",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "7819f220f47dd7b6c01d4b9c58457a4504ba9806812cb1e029718d8e105e0c5d",
            "751fa865c1b040e1dec2c61be881e834",
        ),
    },
    Vector {
        cipher: "aes-128-gcm",
        source: "the GCM specification, test case 3",
        key: "feffe9928665731c6d6a8f9467308308",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: concat!(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72",
            "1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
        ),
        ciphertext: concat!(
            "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e",
            "21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985",
            "4d5c2af327cd64a62cf35abd2ba6fab4",
        ),
    },
    Vector {
        cipher: "aes-192-gcm",
        source: "the GCM specification, test case 9",
        key: "feffe9928665731c6d6a8f9467308308feffe9928665731c",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: concat!(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72",
            "1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
        ),
        ciphertext: concat!(
            "3980ca0b3c00e841eb06fac4872a2757859e1ceaa6efd984628593b40ca1e19c",
            "7d773d00c144c525ac619d18c84a3f4718e2448b2fe324d9ccda2710acade256",
            "9924a7c8587336bfb118024db8674a14",
        ),
    },
    Vector {
        cipher: "aes-256-gcm",
        source: "the GCM specification, test case 15",
        key: "feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: concat!(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72",
            "1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
        ),
        ciphertext: concat!(
            "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa",
            "8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662898015ad",
            "b094dac5d93471bdec1a502270e3cc6c",
        ),
    },
    Vector {
        cipher: "aria-128-gcm",
        source: "OpenSSL 3.6.3 aria-128-gcm",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "87ee4f780ea3fca9c90cb37266ff3da02debb86299b8463bc145c28e6ba4925e",
            "98527cb2ed33d0da2371e5ffd5cbbba3",
        ),
    },
    Vector {
        cipher: "aria-192-gcm",
        source: "OpenSSL 3.6.3 aria-192-gcm",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "687f6fb2c0a876d0d327af036b6e33743911fc4d1c178a87208bef04cde13537",
            "7f784fb457ea4cbefc54ee1e63746fcc",
        ),
    },
    Vector {
        cipher: "aria-256-gcm",
        source: "OpenSSL 3.6.3 aria-256-gcm",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "8e9da6ec3a0a1267cd19d41f0e776505082f95d0ce7602ceeaa38f44ab770563",
            "926c9eacdde4c1fcea71dc936d54ba97",
        ),
    },
    Vector {
        cipher: "sm4-gcm",
        source: "OpenSSL 3.6.3 sm4-gcm",
        key: "404142434445464748494a4b4c4d4e4f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "14e9cb180dacba2ad792bd0ebfb554509227a366d0981981b69da7d9d0aa5499",
            "d7c88a01581c7e265cf5821477c0ba51",
        ),
    },
    Vector {
        cipher: "aes-128-gcm-siv",
        source: "RFC 8452 appendix C.1",
        key: "01000000000000000000000000000000",
        nonce: "030000000000000000000000",
        plaintext: "01000000000000000000000000000000",
        ciphertext: "743f7c8077ab25f8624e2e948579cf77303aaf90f6fe21199c6068577437a0c4",
    },
    Vector {
        cipher: "aes-256-gcm-siv",
        source: "OpenSSL 3.6.3 aes-256-gcm-siv",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "5e5d4f48bc88bf82038fd2a7733b0beec9268896ad111819ed4c2bf419dedcec",
            "37d327b0d05e8adc9d0dd9924da9a28d",
        ),
    },
    Vector {
        cipher: "aes-128-ocb",
        source: "RFC 7253 appendix A",
        key: "000102030405060708090a0b0c0d0e0f",
        nonce: "bbaa99887766554433221106",
        plaintext: "000102030405060708090a0b0c0d0e0f",
        ciphertext: "5ce88ec2e0692706a915c00aeb8b2396f40e1c743f52436bdf06d8fa1eca343d",
    },
    Vector {
        cipher: "aes-192-ocb",
        source: "OpenSSL 3.6.3 aes-192-ocb",
        key: "404142434445464748494a4b4c4d4e4f5051525354555657",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "ea136207ce1519e16179ab3b696e4a0d4175f9b41bafe069821a56cdb1a6240e",
            "89cb9045143ad59b4959c2ce690c2ccb",
        ),
    },
    Vector {
        cipher: "aes-256-ocb",
        source: "OpenSSL 3.6.3 aes-256-ocb",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "02959473358237004fed6b8effe599595494cebb97cf7fa8a6df4302100b91d5",
            "4749653910a7e60656644389fdf1739f",
        ),
    },
    Vector {
        cipher: "aes-128-siv",
        source:
            "OpenSSL 3.6.3 aes-128-siv, nonce as the last S2V component and the tag first, per RFC 5297",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "65f0ee54afc5675ce8bc016f56a0d8edc09882aee36d75f07f26886d7ff8e6ac",
            "acadd3b85efbe5f4da232a4c5106f774",
        ),
    },
    Vector {
        cipher: "aes-256-siv",
        source:
            "OpenSSL 3.6.3 aes-256-siv, nonce as the last S2V component and the tag first, per RFC 5297",
        key: concat!(
            "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
            "606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f",
        ),
        nonce: "cafebabefacedbaddecaf888a1a2a3a4",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "528ac44b34bc37498879c23614f3064b369298182e5d57824ebdb41d69bf0b2c",
            "6f3919db6b57be1d98a3cdf61a8b3480",
        ),
    },
    Vector {
        cipher: "ascon-aead128",
        source: "ascon-c LWC_AEAD_KAT_128_128.txt count 133",
        key: "000102030405060708090a0b0c0d0e0f",
        nonce: "101112131415161718191a1b1c1d1e1f",
        plaintext: "20212223",
        ciphertext: "e8c3deee03c0a96a9b09b64cde66d9ab0796cd04",
    },
    Vector {
        cipher: "chacha20-poly1305",
        source: "OpenSSL 3.6.3 chacha20-poly1305",
        key: "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f",
        nonce: "cafebabefacedbaddecaf888",
        plaintext: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51",
        ciphertext: concat!(
            "84a42198dbc903de00d33b49c29d084335576ccb5f253e763b09d66c2b761451",
            "8a84e65e28abdb584929b1707d34dbb9",
        ),
    },
];

/// Standard rows no known answer can reach, and why.
///
/// Not a list of rows that are broken: each is a combination the table can
/// name and this build can run, but whose bytes no document publishes and no
/// second implementation computes, so there is nothing to compare against.
#[rustfmt::skip]
const UNVERIFIED: &[(&str, &str)] = &[
    (
        "magma-cbc",
        concat!(
            "the mode here is the generic one, because GOST R 34.13-2015 builds its ",
            "own on an initialisation register of several blocks that this row's ",
            "one-block iv cannot hold; nothing publishes the generic composition ",
            "and no other implementation offers it",
        ),
    ),
    (
        "kuznyechik-cbc",
        concat!(
            "the mode here is the generic one, because GOST R 34.13-2015 builds its ",
            "own on an initialisation register of several blocks that this row's ",
            "one-block iv cannot hold; nothing publishes the generic composition ",
            "and no other implementation offers it",
        ),
    ),
    (
        "magma-cfb",
        concat!(
            "the mode here is the generic one, because GOST R 34.13-2015 builds its ",
            "own on an initialisation register of several blocks that this row's ",
            "one-block iv cannot hold; nothing publishes the generic composition ",
            "and no other implementation offers it",
        ),
    ),
    (
        "kuznyechik-cfb",
        concat!(
            "the mode here is the generic one, because GOST R 34.13-2015 builds its ",
            "own on an initialisation register of several blocks that this row's ",
            "one-block iv cannot hold; nothing publishes the generic composition ",
            "and no other implementation offers it",
        ),
    ),
    (
        "des-ctr",
        concat!(
            "SP 800-38A specifies CTR for any block cipher but publishes vectors ",
            "only for AES, and no other implementation offers CTR over DES",
        ),
    ),
    (
        "des-ede3-ctr",
        concat!(
            "SP 800-38A specifies CTR for any block cipher but publishes vectors ",
            "only for AES, and no other implementation offers CTR over DES",
        ),
    ),
    (
        "magma-ofb",
        concat!(
            "the mode here is the generic one, because GOST R 34.13-2015 builds its ",
            "own on an initialisation register of several blocks that this row's ",
            "one-block iv cannot hold; nothing publishes the generic composition ",
            "and no other implementation offers it",
        ),
    ),
    (
        "kuznyechik-ofb",
        concat!(
            "the mode here is the generic one, because GOST R 34.13-2015 builds its ",
            "own on an initialisation register of several blocks that this row's ",
            "one-block iv cannot hold; nothing publishes the generic composition ",
            "and no other implementation offers it",
        ),
    ),
    (
        "camellia-128-ccm",
        concat!(
            "RFC 5528's CCM vectors use a 13-byte nonce, a shorter tag and non- ",
            "empty associated data, none of which this stage can express",
        ),
    ),
    (
        "camellia-192-ccm",
        concat!(
            "RFC 5528's CCM vectors use a 13-byte nonce, a shorter tag and non- ",
            "empty associated data, none of which this stage can express",
        ),
    ),
    (
        "camellia-256-ccm",
        concat!(
            "RFC 5528's CCM vectors use a 13-byte nonce, a shorter tag and non- ",
            "empty associated data, none of which this stage can express",
        ),
    ),
    (
        "camellia-128-gcm",
        concat!(
            "RFC 6367 registers the cipher suite but publishes no vectors, and no ",
            "other implementation offers GCM over Camellia",
        ),
    ),
    (
        "camellia-192-gcm",
        concat!(
            "RFC 6367 registers the cipher suite but publishes no vectors, and no ",
            "other implementation offers GCM over Camellia",
        ),
    ),
    (
        "camellia-256-gcm",
        concat!(
            "RFC 6367 registers the cipher suite but publishes no vectors, and no ",
            "other implementation offers GCM over Camellia",
        ),
    ),
    (
        "kuznyechik-gcm",
        concat!(
            "nothing publishes GCM over Kuznyechik and no other implementation ",
            "offers it",
        ),
    ),
    (
        "aria-128-ocb",
        concat!(
            "RFC 7253 publishes vectors only for AES, and no other implementation ",
            "offers OCB3 over this cipher",
        ),
    ),
    (
        "aria-192-ocb",
        concat!(
            "RFC 7253 publishes vectors only for AES, and no other implementation ",
            "offers OCB3 over this cipher",
        ),
    ),
    (
        "aria-256-ocb",
        concat!(
            "RFC 7253 publishes vectors only for AES, and no other implementation ",
            "offers OCB3 over this cipher",
        ),
    ),
    (
        "camellia-128-ocb",
        concat!(
            "RFC 7253 publishes vectors only for AES, and no other implementation ",
            "offers OCB3 over this cipher",
        ),
    ),
    (
        "camellia-192-ocb",
        concat!(
            "RFC 7253 publishes vectors only for AES, and no other implementation ",
            "offers OCB3 over this cipher",
        ),
    ),
    (
        "camellia-256-ocb",
        concat!(
            "RFC 7253 publishes vectors only for AES, and no other implementation ",
            "offers OCB3 over this cipher",
        ),
    ),
    (
        "kuznyechik-ocb",
        concat!(
            "RFC 7253 publishes vectors only for AES, and no other implementation ",
            "offers OCB3 over this cipher",
        ),
    ),
    (
        "sm4-ocb",
        concat!(
            "RFC 7253 publishes vectors only for AES, and no other implementation ",
            "offers OCB3 over this cipher",
        ),
    ),
    (
        "xchacha20-poly1305",
        concat!(
            "the only AEAD vector in draft-irtf-cfrg-xchacha-03 uses non-empty ",
            "associated data, which this stage cannot express",
        ),
    ),
];

/// The row a name refers to, resolved through the table rather than serde,
/// so a vector naming something the table does not declare fails here.
fn cipher_of(name: &str) -> Cipher {
    *Cipher::ALL
        .iter()
        .find(|cipher| cipher.spec().name == name)
        .unwrap_or_else(|| panic!("{name} is not a cipher in the table"))
}

fn bytes(hex: &str, what: &str, cipher: &str) -> Vec<u8> {
    hex::decode(hex).unwrap_or_else(|_| panic!("{cipher}: {what} is not hex"))
}

/// Seal `plaintext` under a fixed nonce, which is the whole point of working
/// at this layer: the stage draws its nonce at random, so nothing above here
/// can be pinned to a published answer in the encrypting direction.
fn seal(cipher: Cipher, key: &[u8], nonce: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut sealer = sealer(cipher, key.to_vec(), Padding::None).expect("a sealer");
    let mut out = Vec::new();

    sealer.start(nonce).expect("start");
    sealer.update(plaintext, &mut out).expect("update");
    sealer.finish(&mut out).expect("finish");

    out
}

fn open(cipher: Cipher, key: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut opener = opener(cipher, key.to_vec(), Padding::None).expect("an opener");
    let mut out = Vec::new();

    opener.start(nonce).expect("start");
    opener.update(ciphertext, &mut out).expect("update");
    opener.finish(&mut out).expect("finish");

    out
}

#[test]
fn every_known_answer_is_the_published_one() {
    for vector in VECTORS {
        let cipher = cipher_of(vector.cipher);
        let spec = cipher.spec();
        let key = bytes(vector.key, "key", vector.cipher);
        let nonce = bytes(vector.nonce, "nonce", vector.cipher);
        let plaintext = bytes(vector.plaintext, "plaintext", vector.cipher);
        let ciphertext = bytes(vector.ciphertext, "ciphertext", vector.cipher);

        // A vector whose own lengths disagree with the row is testing
        // something other than the row, so say so here rather than fail
        // obscurely inside the cipher.
        assert_eq!(
            key.len(),
            spec.key,
            "{}: the vector's key is not the length the row declares",
            vector.cipher,
        );
        assert_eq!(
            nonce.len(),
            spec.nonce,
            "{}: the vector's nonce is not the length the row declares",
            vector.cipher,
        );
        assert_eq!(
            ciphertext.len(),
            plaintext.len() + spec.tag,
            "{}: the vector's ciphertext is not its plaintext plus its tag",
            vector.cipher,
        );

        assert_eq!(
            hex::encode(seal(cipher, &key, &nonce, &plaintext)),
            vector.ciphertext,
            "{} did not produce the bytes given by {}",
            vector.cipher,
            vector.source,
        );

        assert_eq!(
            hex::encode(open(cipher, &key, &nonce, &ciphertext)),
            vector.plaintext,
            "{} did not read back the bytes given by {}",
            vector.cipher,
            vector.source,
        );
    }
}

/// The same answer, with the plaintext handed over in two pieces.
///
/// This is where a buffering mistake shows: CFB's types carry a partial block
/// across calls, and [`BlockCipher`] holds an incomplete block in `pending`.
/// Either can be wrong in a way that a single whole-message call never
/// reveals. An AEAD is left out because its `update` is one-shot.
#[test]
fn a_known_answer_survives_being_cut_anywhere() {
    for vector in VECTORS {
        let cipher = cipher_of(vector.cipher);

        if cipher.spec().family == Family::Aead {
            continue;
        }

        let key = bytes(vector.key, "key", vector.cipher);
        let nonce = bytes(vector.nonce, "nonce", vector.cipher);
        let plaintext = bytes(vector.plaintext, "plaintext", vector.cipher);

        for at in [1, plaintext.len() / 2, plaintext.len() - 1] {
            let mut sealer = sealer(cipher, key.clone(), Padding::None).expect("a sealer");
            let mut out = Vec::new();

            sealer.start(&nonce).expect("start");
            sealer.update(&plaintext[..at], &mut out).expect("head");
            sealer.update(&plaintext[at..], &mut out).expect("tail");
            sealer.finish(&mut out).expect("finish");

            assert_eq!(
                hex::encode(&out),
                vector.ciphertext,
                "{} answered differently when cut at {at}",
                vector.cipher,
            );
        }
    }
}

/// Every row that names a standard is either checked or excused by name.
///
/// The excuse list is held to its word in both directions, so it cannot
/// quietly outlive the reason for it: an entry that acquires a vector, or one
/// that names a row the table no longer has, fails here.
#[test]
fn every_standard_cipher_has_a_known_answer() {
    for cipher in Cipher::ALL {
        let spec = cipher.spec();

        if spec.standard.is_none() || UNVERIFIED.iter().any(|(name, _)| *name == spec.name) {
            continue;
        }

        assert!(
            VECTORS.iter().any(|vector| vector.cipher == spec.name),
            "{} claims {} but nothing checks its bytes against it",
            spec.name,
            spec.standard.expect("a standard"),
        );
    }

    for (name, _) in UNVERIFIED {
        assert!(
            !VECTORS.iter().any(|vector| vector.cipher == *name),
            "{name} is excused from a known answer but has one",
        );
        assert!(
            Cipher::ALL.iter().any(|cipher| cipher.spec().name == *name),
            "{name} is excused from a known answer but is no longer in the table",
        );
    }
}

/// One vector's bytes, for the stage-level tests next door.
///
/// Those pin the framing this module deliberately steps around: everything
/// here drives a cipher directly, so nothing here would notice if a record
/// stopped carrying its nonce where a peer expects it.
pub(super) fn vector(cipher: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
    let vector = VECTORS
        .iter()
        .find(|vector| vector.cipher == cipher)
        .unwrap_or_else(|| panic!("{cipher} has no known answer"));

    (
        bytes(vector.key, "key", cipher),
        bytes(vector.nonce, "nonce", cipher),
        bytes(vector.plaintext, "plaintext", cipher),
        bytes(vector.ciphertext, "ciphertext", cipher),
    )
}
