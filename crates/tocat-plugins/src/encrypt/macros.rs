/// Declares the cipher table: the [`Cipher`] enum, its [`Cipher::spec`]
/// entries, `ALL`, and both dispatch halves, from one row per cipher.
///
/// Rows are grouped by shape rather than by family, because what a row has to
/// say depends on whether the mode takes an IV, whether it transmits all of
/// it, and whether its transform is a trait method. The group supplies what
/// the whole group agrees on: the construction pointer, and which of
/// [`block`], [`keystream`] and [`aead`] builds the spec.
///
/// Every row names the document that specifies its bytes, or `""` where
/// nothing does. See [`standard`].
///
/// Each row is checked against the crate behind it at compile time. The
/// declared key, IV, block, nonce and tag lengths have to be the ones the type
/// actually takes, and a nonce has to fit [`MAX_NONCE`]. A row that lies is a
/// build failure naming the cipher, rather than a stage that fails when a key
/// is loaded, or one that round-trips with itself and interoperates with
/// nothing. What the assertions cannot catch is a row whose lengths are right
/// and whose construction nobody else implements; that is what `standard`
/// records and what a known-answer test is for.
///
/// The expansion names items from the module that invokes it, so this is not
/// exported. Import it by path: `pub(crate) use ciphers;` here, and
/// `use self::macros::ciphers;` there.
macro_rules! ciphers {
    (
        block with iv {$(
            $biv_var:ident = $biv_name:literal $(| $biv_alias:literal)*,
                key $biv_key:literal, iv $biv_iv:literal, block $biv_block:literal,
                standard $biv_std:literal,
                $biv_seal:ty, $biv_open:ty;
        )*}
        block without iv {$(
            $ecb_var:ident = $ecb_name:literal $(| $ecb_alias:literal)*,
                key $ecb_key:literal, block $ecb_block:literal,
                standard $ecb_std:literal,
                $ecb_seal:ty, $ecb_open:ty;
        )*}
        stream with iv {$(
            $skiv_var:ident = $skiv_name:literal $(| $skiv_alias:literal)*,
                key $skiv_key:literal, iv $skiv_iv:literal,
                standard $skiv_std:literal,
                $skiv_seal:ty, $skiv_open:ty, $skiv_seal_apply:ident, $skiv_open_apply:ident;
        )*}
        keystream with iv {$(
            $kiv_var:ident = $kiv_name:literal $(| $kiv_alias:literal)*,
                key $kiv_key:literal, iv $kiv_iv:literal,
                standard $kiv_std:literal,
                $kiv_seal:ty, $kiv_open:ty;
        )*}
        keystream with half iv {$(
            $khiv_var:ident = $khiv_name:literal $(| $khiv_alias:literal)*,
                key $khiv_key:literal, iv $khiv_iv:literal,
                standard $khiv_std:literal,
                $khiv_seal:ty, $khiv_open:ty;
        )*}
        keystream without iv {$(
            $k_var:ident = $k_name:literal $(| $k_alias:literal)*,
                key $k_key:literal,
                standard $k_std:literal,
                $k_seal:ty, $k_open:ty;
        )*}
        aead {$(
            $a_var:ident = $a_name:literal $(| $a_alias:literal)*,
                key $a_key:literal, nonce $a_nonce:literal,
                standard $a_std:literal,
                $a_type:ty;
        )*}
    ) => {
        /// Which cipher a stage runs.
        ///
        /// Spellings are matched the way every other identifier in tocat is,
        /// so `aes-256-gcm`, `AES256GCM` and `aes_256_gcm` are one cipher.
        /// Aliases are matched as written, since they are not among the
        /// declared names the normaliser compares against.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
        #[serde(rename_all = "kebab-case")]
        pub enum Cipher {
            $( #[serde(rename = $biv_name $(, alias = $biv_alias)*)] $biv_var, )*
            $( #[serde(rename = $ecb_name $(, alias = $ecb_alias)*)] $ecb_var, )*
            $( #[serde(rename = $skiv_name $(, alias = $skiv_alias)*)] $skiv_var, )*
            $( #[serde(rename = $kiv_name $(, alias = $kiv_alias)*)] $kiv_var, )*
            $( #[serde(rename = $khiv_name $(, alias = $khiv_alias)*)] $khiv_var, )*
            $( #[serde(rename = $k_name $(, alias = $k_alias)*)] $k_var, )*
            $( #[serde(rename = $a_name $(, alias = $a_alias)*)] $a_var, )*
        }

        impl Cipher {
            /// Every cipher this build knows, in table order.
            ///
            /// Not test-only: a docs table or a listing that reads this
            /// cannot drift from the table, which is the point of having it.
            #[allow(unused)]
            pub(crate) const ALL: &'static [Self] = &[
                $( Self::$biv_var, )*
                $( Self::$ecb_var, )*
                $( Self::$skiv_var, )*
                $( Self::$kiv_var, )*
                $( Self::$khiv_var, )*
                $( Self::$k_var, )*
                $( Self::$a_var, )*
            ];

            /// The one table. Everything decided per cipher is decided from
            /// here.
            const fn spec(self) -> Spec {
                match self {
                    $( Self::$biv_var =>
                        block($biv_name, $biv_key, $biv_iv, $biv_block, $biv_std), )*
                    $( Self::$ecb_var =>
                        block($ecb_name, $ecb_key, 0, $ecb_block, $ecb_std), )*
                    $( Self::$skiv_var =>
                        keystream($skiv_name, $skiv_key, $skiv_iv, $skiv_std), )*
                    $( Self::$kiv_var =>
                        keystream($kiv_name, $kiv_key, $kiv_iv, $kiv_std), )*
                    $( Self::$khiv_var =>
                        keystream($khiv_name, $khiv_key, $khiv_iv, $khiv_std), )*
                    $( Self::$k_var =>
                        keystream($k_name, $k_key, 0, $k_std), )*
                    $( Self::$a_var =>
                        aead($a_name, $a_key, $a_nonce, $a_std), )*
                }
            }
        }

        /// The encrypting half of the cipher table.
        fn sealer(cipher: Cipher, key: Vec<u8>, padding: Padding) -> Result<Box<dyn Seal>> {
            Ok(match cipher {
                $( Cipher::$biv_var =>
                    Box::new(BlockCipher::<$biv_seal>::new(key, padding, with_iv)), )*
                $( Cipher::$ecb_var =>
                    Box::new(BlockCipher::<$ecb_seal>::new(key, padding, without_iv)), )*
                $( Cipher::$skiv_var =>
                    Box::new(KeystreamCipher::<$skiv_seal>::new(key, with_iv, $skiv_seal_apply)), )*
                $( Cipher::$kiv_var =>
                    Box::new(KeystreamCipher::<$kiv_seal>::new(key, with_iv, xor_keystream)), )*
                $( Cipher::$khiv_var =>
                    Box::new(KeystreamCipher::<$khiv_seal>::new(
                        key,
                        with_half_iv,
                        xor_keystream
                    )), )*
                $( Cipher::$k_var =>
                    Box::new(KeystreamCipher::<$k_seal>::new(key, without_iv, xor_keystream)), )*
                $( Cipher::$a_var =>
                    Box::new(AeadCipher::new(keyed::<$a_type>(ENCRYPT, &key)?)), )*
            })
        }

        /// The decrypting half of the cipher table.
        fn opener(cipher: Cipher, key: Vec<u8>, padding: Padding) -> Result<Box<dyn Open>> {
            Ok(match cipher {
                $( Cipher::$biv_var =>
                    Box::new(BlockCipher::<$biv_open>::new(key, padding, with_iv)), )*
                $( Cipher::$ecb_var =>
                    Box::new(BlockCipher::<$ecb_open>::new(key, padding, without_iv)), )*
                $( Cipher::$skiv_var =>
                    Box::new(KeystreamCipher::<$skiv_open>::new(key, with_iv, $skiv_open_apply)), )*
                $( Cipher::$kiv_var =>
                    Box::new(KeystreamCipher::<$kiv_open>::new(key, with_iv, xor_keystream)), )*
                $( Cipher::$khiv_var =>
                    Box::new(KeystreamCipher::<$khiv_open>::new(
                        key,
                        with_half_iv,
                        xor_keystream
                    )), )*
                $( Cipher::$k_var =>
                    Box::new(KeystreamCipher::<$k_open>::new(key, without_iv, xor_keystream)), )*
                $( Cipher::$a_var =>
                    Box::new(AeadCipher::new(keyed::<$a_type>(DECRYPT, &key)?)), )*
            })
        }

        // What follows is the table checking itself against the crates. Each
        // block is anonymous, so rows need no unique name for it.
        $(
            const _: () = {
                assert!(
                    <<$biv_seal as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE == $biv_key
                        && <<$biv_open as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE
                            == $biv_key,
                    concat!($biv_name, ": declared key length is not the one the crate takes"),
                );
                assert!(
                    <<$biv_seal as cipher::IvSizeUser>::IvSize as Unsigned>::USIZE == $biv_iv
                        && <<$biv_open as cipher::IvSizeUser>::IvSize as Unsigned>::USIZE
                            == $biv_iv,
                    concat!($biv_name, ": declared iv length is not the one the crate takes"),
                );
                assert!(
                    <<$biv_seal as BlockSizeUser>::BlockSize as Unsigned>::USIZE == $biv_block
                        && <<$biv_open as BlockSizeUser>::BlockSize as Unsigned>::USIZE
                            == $biv_block,
                    concat!($biv_name, ": declared block size is not the one the crate takes"),
                );
                assert!(
                    $biv_iv <= MAX_NONCE,
                    concat!($biv_name, ": iv is wider than the stage's nonce buffer"),
                );
            };
        )*
        $(
            const _: () = {
                assert!(
                    <<$ecb_seal as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE == $ecb_key
                        && <<$ecb_open as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE
                            == $ecb_key,
                    concat!($ecb_name, ": declared key length is not the one the crate takes"),
                );
                assert!(
                    <<$ecb_seal as BlockSizeUser>::BlockSize as Unsigned>::USIZE == $ecb_block
                        && <<$ecb_open as BlockSizeUser>::BlockSize as Unsigned>::USIZE
                            == $ecb_block,
                    concat!($ecb_name, ": declared block size is not the one the crate takes"),
                );
            };
        )*
        $(
            const _: () = {
                assert!(
                    <<$skiv_seal as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE == $skiv_key
                        && <<$skiv_open as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE
                            == $skiv_key,
                    concat!($skiv_name, ": declared key length is not the one the crate takes"),
                );
                assert!(
                    <<$skiv_seal as cipher::IvSizeUser>::IvSize as Unsigned>::USIZE == $skiv_iv
                        && <<$skiv_open as cipher::IvSizeUser>::IvSize as Unsigned>::USIZE
                            == $skiv_iv,
                    concat!($skiv_name, ": declared iv length is not the one the crate takes"),
                );
                assert!(
                    $skiv_iv <= MAX_NONCE,
                    concat!($skiv_name, ": iv is wider than the stage's nonce buffer"),
                );
            };
        )*
        $(
            const _: () = {
                assert!(
                    <<$kiv_seal as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE == $kiv_key
                        && <<$kiv_open as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE
                            == $kiv_key,
                    concat!($kiv_name, ": declared key length is not the one the crate takes"),
                );
                assert!(
                    <<$kiv_seal as cipher::IvSizeUser>::IvSize as Unsigned>::USIZE == $kiv_iv
                        && <<$kiv_open as cipher::IvSizeUser>::IvSize as Unsigned>::USIZE
                            == $kiv_iv,
                    concat!($kiv_name, ": declared iv length is not the one the crate takes"),
                );
                assert!(
                    $kiv_iv <= MAX_NONCE,
                    concat!($kiv_name, ": iv is wider than the stage's nonce buffer"),
                );
            };
        )*
        $(
            const _: () = {
                assert!(
                    <<$khiv_seal as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE == $khiv_key
                        && <<$khiv_open as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE
                            == $khiv_key,
                    concat!($khiv_name, ": declared key length is not the one the crate takes"),
                );
                assert!(
                    <<$khiv_seal as cipher::IvSizeUser>::IvSize as Unsigned>::USIZE
                        == 2 * $khiv_iv
                        && <<$khiv_open as cipher::IvSizeUser>::IvSize as Unsigned>::USIZE
                            == 2 * $khiv_iv,
                    concat!(
                        $khiv_name,
                        ": a half iv is transmitted, so the crate's iv has to be twice what this \
                         row declares",
                    ),
                );
                assert!(
                    $khiv_iv <= MAX_NONCE,
                    concat!($khiv_name, ": iv is wider than the stage's nonce buffer"),
                );
            };
        )*
        $(
            const _: () = {
                assert!(
                    <<$k_seal as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE == $k_key
                        && <<$k_open as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE == $k_key,
                    concat!($k_name, ": declared key length is not the one the crate takes"),
                );
            };
        )*
        $(
            const _: () = {
                assert!(
                    <<$a_type as cipher::KeySizeUser>::KeySize as Unsigned>::USIZE == $a_key,
                    concat!($a_name, ": declared key length is not the one the crate takes"),
                );
                assert!(
                    <<$a_type as AeadCore>::NonceSize as Unsigned>::USIZE == $a_nonce,
                    concat!($a_name, ": declared nonce length is not the one the crate takes"),
                );
                assert!(
                    <<$a_type as AeadCore>::TagSize as Unsigned>::USIZE == 16,
                    concat!(
                        $a_name,
                        ": tag is not 16 bytes, which `aead` hardcodes; a truncated-tag cipher \
                         needs a tag parameter on the row and on the spec helper",
                    ),
                );
                assert!(
                    $a_nonce <= MAX_NONCE,
                    concat!($a_name, ": nonce is wider than the stage's nonce buffer"),
                );
            };
        )*
    };
}

pub(crate) use ciphers;
