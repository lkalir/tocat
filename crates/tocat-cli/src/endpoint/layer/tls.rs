//! tls.rs: TLS over a byte stream, as a layer.
//!
//! A handshake is not a stage. A stage is synchronous and pure and has no way
//! to send bytes back upstream, and a handshake is a stateful exchange with the
//! peer, so this belongs to the endpoint rather than to the pipeline. What it
//! produces is another [`EndpointStream::Duplex`], which is why nothing in
//! `pump` had to change to carry it.
//!
//! # Boundaries
//!
//! TLS declares [`Fuse`]. A record is not a message: a peer's single write may
//! arrive as several records or share one with the next write, so anything that
//! claimed to preserve boundaries here would be lying in the way that turns a
//! datagram relay into a stream relay with no error anywhere.
//!
//! # Verification
//!
//! Four verifiers, one shape. The default checks the platform trust store,
//! `cafile=` checks a file instead, `pin=` checks the leaf's fingerprint and
//! ignores the chain, and `verify=none` checks nothing. The first three are the
//! answers to give people; the fourth exists because a socat replacement that
//! cannot talk to a self-signed appliance is not a socat replacement, and it
//! logs a warning naming the endpoint every time it is used, because the
//! failure mode of a bypass is that it is invisible afterwards.
//!
//! `pin=` is worth reaching for before `verify=none`: an appliance with a
//! self-signed certificate has a stable fingerprint, and pinning it is a real
//! check rather than the absence of one.

use std::{fs::File, io::BufReader, sync::Arc};

use anyhow::{Context as _, bail};
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, ServerConfig,
    SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject as _},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tocat_api::normalize;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::warn;

use crate::endpoint::{
    EndpointStream,
    parse::{Opt, ParseEndpointError},
};

/// How the peer's certificate is checked.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verify {
    /// Chain to a trust anchor, and match the name. The default.
    #[default]
    Peer,
    /// Nothing at all. Spelled as a word rather than as `0` so that a config
    /// file carrying it reads as an admission.
    None,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Tls {
    /// Trust anchors from a PEM file instead of the platform's store.
    pub cafile: Option<String>,

    /// The leaf certificate's SHA-256 fingerprint, hex, with or without a
    /// `sha256:` prefix. Checked instead of the chain, not as well as it.
    pub pin: Option<String>,

    pub verify: Verify,

    /// The name to send in SNI and to check the certificate against. Defaults
    /// to the host the transport dialled, which is what makes `tls:host:443`
    /// work without saying anything twice.
    pub servername: Option<String>,

    /// Protocols to offer, in preference order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alpn: Vec<String>,

    /// The server's certificate chain, PEM. Required to accept.
    pub cert: Option<String>,
    /// The server's private key, PEM.
    pub keyfile: Option<String>,
}

impl Tls {
    pub(in crate::endpoint) fn option(
        &mut self,
        opt: &Opt<'_>,
    ) -> Result<bool, ParseEndpointError> {
        match normalize(opt.key).as_str() {
            "cafile" | "ca" => self.cafile = Some(opt.string()?),
            "pin" => self.pin = Some(opt.string()?),
            "verify" => self.verify = verify(opt)?,
            "servername" | "sni" => self.servername = Some(opt.string()?),
            "alpn" => self.alpn.push(opt.string()?),
            "cert" => self.cert = Some(opt.string()?),
            "keyfile" | "key" => self.keyfile = Some(opt.string()?),
            _ => return Ok(false),
        }

        Ok(true)
    }

    /// A layer over a datagram transport cannot work, and saying so here keeps
    /// the promise that everything is rejected before an endpoint is opened.
    pub(in crate::endpoint) fn check(
        &self,
        below_is_datagram: bool,
        listening: bool,
    ) -> anyhow::Result<()> {
        if below_is_datagram {
            bail!("tls needs a byte stream underneath it; DTLS is not supported");
        }

        // Wrong-side options are refused rather than ignored. Accepting one that does
        // nothing is how a relay ends up looking configured and behaving
        // otherwise.
        if listening {
            if self.cert.is_none() || self.keyfile.is_none() {
                bail!("tls-listen needs cert= and keyfile=");
            }

            for (name, set) in [
                ("cafile", self.cafile.is_some()),
                ("pin", self.pin.is_some()),
                ("servername", self.servername.is_some()),
                ("verify", self.verify != Verify::default()),
            ] {
                if set {
                    bail!(
                        "{name} is a client option and does nothing on tls-listen; client \
                         certificates are not supported yet"
                    );
                }
            }
        } else if self.cert.is_some() || self.keyfile.is_some() {
            bail!("cert and keyfile are for tls-listen; client certificates are not supported yet");
        }

        if self.pin.is_some() && self.verify == Verify::None {
            bail!("pin and verify=none contradict each other: pick the check or drop it");
        }

        Ok(())
    }

    /// Wrap a connection this relay dialled.
    ///
    /// `host` is what the transport was pointed at, and becomes the SNI name
    /// unless `servername=` overrode it.
    pub(in crate::endpoint) async fn wrap_client(
        &self,
        stream: EndpointStream,
        host: &str,
    ) -> anyhow::Result<EndpointStream> {
        let EndpointStream::Duplex(inner) = stream else {
            bail!("tls needs a two-way stream underneath it");
        };

        let name = self.servername.as_deref().unwrap_or(host);
        let server_name = ServerName::try_from(name.to_owned())
            .with_context(|| format!("{name} is not a valid server name"))?;

        let mut config = self.client_config()?;
        config.alpn_protocols = self.alpn.iter().map(|p| p.as_bytes().to_vec()).collect();

        let tls = TlsConnector::from(Arc::new(config))
            .connect(server_name, inner)
            .await
            .with_context(|| format!("tls handshake with {name}"))?;

        Ok(EndpointStream::Duplex(Box::new(tls)))
    }

    /// Wrap a connection this relay accepted.
    pub(in crate::endpoint) async fn wrap_server(
        &self,
        stream: EndpointStream,
    ) -> anyhow::Result<EndpointStream> {
        let EndpointStream::Duplex(inner) = stream else {
            bail!("tls needs a two-way stream underneath it");
        };

        let mut config = self.server_config()?;
        config.alpn_protocols = self.alpn.iter().map(|p| p.as_bytes().to_vec()).collect();

        let tls = TlsAcceptor::from(Arc::new(config))
            .accept(inner)
            .await
            .context("tls handshake with client")?;

        Ok(EndpointStream::Duplex(Box::new(tls)))
    }

    fn client_config(&self) -> anyhow::Result<ClientConfig> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

        let builder = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .context("no protocol versions available")?;

        let config = match (&self.pin, &self.verify) {
            (Some(pin), _) => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Pinned::new(pin, provider)?))
                .with_no_client_auth(),

            (None, Verify::None) => {
                warn!(
                    "certificate verification is off for this endpoint: the connection is \
                       encrypted but the peer is unauthenticated"
                );

                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(Unverified(provider)))
                    .with_no_client_auth()
            }

            (None, Verify::Peer) => builder
                .with_root_certificates(self.roots()?)
                .with_no_client_auth(),
        };

        Ok(config)
    }

    /// The trust anchors: a file if one was named, the platform's store
    /// otherwise.
    fn roots(&self) -> anyhow::Result<RootCertStore> {
        let mut roots = RootCertStore::empty();

        let Some(path) = &self.cafile else {
            let found = rustls_native_certs::load_native_certs();

            for cert in found.certs {
                let _ = roots.add(cert);
            }

            if roots.is_empty() {
                bail!("no trust anchors in the platform store; name one with cafile=");
            }

            return Ok(roots);
        };

        let mut reader =
            BufReader::new(File::open(path).with_context(|| format!("opening {path}"))?);

        for cert in rustls_pemfile::certs(&mut reader) {
            roots.add(cert.with_context(|| format!("reading {path}"))?)?;
        }

        if roots.is_empty() {
            bail!("{path} contains no certificates");
        }

        Ok(roots)
    }

    fn server_config(&self) -> anyhow::Result<ServerConfig> {
        let (Some(cert), Some(keyfile)) = (&self.cert, &self.keyfile) else {
            bail!("tls-listen needs cert= and keyfile=");
        };

        let mut reader =
            BufReader::new(File::open(cert).with_context(|| format!("opening {cert}"))?);

        let chain = rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("reading {cert}"))?;

        let key =
            PrivateKeyDer::from_pem_file(keyfile).with_context(|| format!("reading {keyfile}"))?;

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

        ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("no protocol versions available")?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .context("the certificate and key do not go together")
    }
}

/// Match the leaf's fingerprint, ignore the chain.
#[derive(Debug)]
struct Pinned {
    fingerprint: Vec<u8>,
    provider: Arc<CryptoProvider>,
}

impl Pinned {
    fn new(pin: &str, provider: Arc<CryptoProvider>) -> anyhow::Result<Self> {
        let hex = pin.strip_prefix("sha256:").unwrap_or(pin).replace(':', "");

        let fingerprint = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<Vec<_>, _>>()
            .context("pin is not hex")?;

        if fingerprint.len() != 32 {
            bail!(
                "a sha256 pin is 32 bytes, this one is {}",
                fingerprint.len()
            );
        }

        Ok(Self {
            fingerprint,
            provider,
        })
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        if Sha256::digest(end_entity.as_ref()).as_slice() == self.fingerprint {
            return Ok(ServerCertVerified::assertion());
        }

        Err(TlsError::General(
            "the peer's certificate does not match pin=".to_owned(),
        ))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `verify=none`: the certificate is accepted whatever it is.
///
/// The signature checks are still real. They prove the peer holds the key in
/// the certificate it sent, which is worth nothing on its own but costs nothing
/// and keeps the handshake honest about what it did.
#[derive(Debug)]
struct Unverified(Arc<CryptoProvider>);

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn verify(opt: &Opt<'_>) -> Result<Verify, ParseEndpointError> {
    match normalize(opt.text()?).as_str() {
        "peer" | "on" | "true" | "1" => Ok(Verify::Peer),
        "none" => Ok(Verify::None),
        other => Err(ParseEndpointError::InvalidFlag(format!(
            "verify={other}, which is peer or none",
        ))),
    }
}
