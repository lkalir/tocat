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

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::{Context as _, bail};
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, ServerConfig,
    SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject as _},
    server::WebPkiClientVerifier,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tocat_api::normalize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
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

/// Whether a server asks its clients for a certificate.
///
/// Its own key rather than a second meaning for `verify=`: on a client that
/// word means "check the peer", and a server reading `verify=none` as "clients
/// need not authenticate" would be saying something quite different with the
/// same spelling.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientAuth {
    /// No certificate is asked for, which is what a public server does.
    #[default]
    None,
    /// Asked for, and checked if one is offered. A client with none is still
    /// accepted, so this authenticates the clients that have a certifiacte
    /// without restricting who may connect.
    Optional,
    /// Asked for, checked, and a client without one is refused.
    Required,
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

    /// The certificate chain this side presents, PEM. Required on
    /// `tls-listen`; on `tls` it is the client half of mutual TLS.
    pub cert: Option<String>,
    /// The matching private key, PEM.
    pub keyfile: Option<String>,

    /// Whether clients are asked for a certificate. `tls-listen` only, and it
    /// needs `cafile=` to say who may issue them.
    pub client_auth: ClientAuth,
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
            "clientauth" => self.client_auth = client_auth(opt)?,
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
                ("pin", self.pin.is_some()),
                ("servername", self.servername.is_some()),
                ("verify", self.verify != Verify::default()),
            ] {
                if set {
                    bail!("{name} is a client option and does nothing on tls-listen");
                }
            }

            match (self.client_auth, self.cafile.is_some()) {
                // Who may issue a client certificate has no default worth guessing: the platform
                // store would trust every public authority to vouce for anyone who connects.
                (ClientAuth::Required | ClientAuth::Optional, false) => {
                    bail!("client-auth needs cafile= naming who may issue client certificates");
                }
                (ClientAuth::None, true) => {
                    bail!(
                        "cafile on tls-listen names the client certificate issuer, so it does \
                         nothing without client-auth=required or client-auth=optional",
                    );
                }
                _ => {}
            }
        } else {
            if self.client_auth != ClientAuth::default() {
                bail!(
                    "client-auth is a tls-listen option: a client presents a certificate with \
                     cert= and keyfile= rather than asking for one",
                );
            }

            if self.cert.is_some() != self.keyfile.is_some() {
                bail!("cert and keyfile go together: neither is any use without the other");
            }
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

        Ok(EndpointStream::Duplex(Box::new(Closing(tls))))
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

        Ok(EndpointStream::Duplex(Box::new(Closing(tls))))
    }

    fn client_config(&self) -> anyhow::Result<ClientConfig> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

        let builder = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .context("no protocol versions available")?;

        // The verifier first and the identity second, because the two are independent:
        // pairing every verification mode with every identity by hand would be
        // six branches, saying the same thing twice.
        let builder = match (&self.pin, &self.verify) {
            (Some(pin), _) => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Pinned::new(pin, provider.clone())?)),

            // The verifier first and the identity second, because the two are independent: pairing
            // every verification mode with every identity by hand would be six
            // branches, saying the same thing twice.
            (None, Verify::None) => {
                warn!(
                    "certificate verification is off for this endpoint: the connection is \
                     encrypted but the peer is unauthenticated"
                );

                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(Unverified(provider.clone())))
            }

            (None, Verify::Peer) => builder.with_root_certificates(self.roots()?),
        };

        let (Some(cert), Some(keyfile)) = (&self.cert, &self.keyfile) else {
            return Ok(builder.with_no_client_auth());
        };

        builder
            .with_client_auth_cert(chain(cert)?, key(keyfile)?)
            .context("the client certificate and key do not go together")
    }

    /// The trust anchors: a file if one was named, the platform's store
    /// otherwise.
    ///
    /// Both sides use this, and it means different things on each. On a client
    /// it is who may vouce for the server; on a listener it is who may issue a
    /// client certificate, where there is no sensible default and `cafile=` is
    /// therefore required.
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

        for cert in rustls_pki_types::CertificateDer::pem_file_iter(path)
            .with_context(|| format!("opening {path}"))?
        {
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

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

        let builder = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .context("the certificate and key do not go together")?;

        let builder = match self.client_auth {
            ClientAuth::None => builder.with_no_client_auth(),
            auth => {
                // `roots` is the client certificate issuer here rather than a
                // server trust anchor, which is why cafile is
                // required above: falling back to the platform store would
                // accept a client certificate from any public authority.
                let verifier =
                    WebPkiClientVerifier::builder_with_provider(Arc::new(self.roots()?), provider);

                let verifier = match auth {
                    ClientAuth::Optional => verifier.allow_unauthenticated(),
                    _ => verifier,
                };

                builder.with_client_cert_verifier(
                    verifier
                        .build()
                        .context("building the client certificate verifier")?,
                )
            }
        };

        builder
            .with_single_cert(chain(cert)?, key(keyfile)?)
            .context("the certificate and key do not go together")
    }
}

/// A PEM certificate chain from a file.
fn chain(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    rustls_pki_types::CertificateDer::pem_file_iter(path)
        .with_context(|| format!("opening {path}"))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("reading {path}"))
}

/// A PEM private key from a file.
fn key(path: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(path).with_context(|| format!("reading {path}"))
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

fn client_auth(opt: &Opt<'_>) -> Result<ClientAuth, ParseEndpointError> {
    match normalize(opt.text()?).as_str() {
        "none" | "off" => Ok(ClientAuth::None),
        "optional" => Ok(ClientAuth::Optional),
        "required" | "require" | "on" => Ok(ClientAuth::Required),
        other => Err(ParseEndpointError::InvalidFlag(format!(
            "client-auth={other}, which is none, optional, or required"
        ))),
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

/// A TLS stream that treats a peer hanging up as end of stream.
///
/// rustls reports a connection that closes without `close_notify` as an error,
/// because a reader that cares about truncation cannot otherwise tell a
/// complete response from a cut one. That is the right default for an
/// application. It is the wrong one for a relay: tocat has no idea how long the
/// peer's response was supposed to be, so it cannot check, and an HTTP/1.1
/// server that has sent its `Content-Length` and closed has done nothing wrong.
/// Reporting that as a failed run turns an ordinary request into an error.
///
/// Anything that does care about truncation is running above tocat and can
/// check the lengths itself, which is the only place the check can be made
/// honestly.
///
/// Only reads are affected. A write to a closed connection is still an error,
/// and so is every other TLS failure.
struct Closing<S>(S);

impl<S: AsyncRead + Unpin> AsyncRead for Closing<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match Pin::new(&mut self.0).poll_read(cx, buf) {
            // Nothing was read into `buf` in this case, so returning `Ok`
            // leaves it empty, which is exactly how end of stream is spelled.
            Poll::Ready(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Closing<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
