//! rustls/quinn configuration: TLS 1.3 only, mutual authentication, pinned peers.
//!
//! Both verifiers skip CA chains and hostname checks (peers are self-signed and pinned
//! by fingerprint) but always verify the TLS 1.3 handshake signature, so a peer must
//! hold the private key of the certificate it presents. Session resumption and 0-RTT
//! are disabled: every connection presents and proves its certificate afresh, and
//! nothing on this channel is replayable.

use crate::identity::Fingerprint;
use crate::Error;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ALPN: &[u8] = b"nocomputer/1";

pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn single_cert(intermediates: &[CertificateDer<'_>]) -> Result<(), rustls::Error> {
    if intermediates.is_empty() {
        Ok(())
    } else {
        Err(rustls::Error::General("expected a single self-signed certificate".into()))
    }
}

fn no_tls12() -> Result<HandshakeSignatureValid, rustls::Error> {
    Err(rustls::Error::General("TLS 1.2 is not used".into()))
}

/// Viewer side. With `pinned` set, only that host certificate is accepted. With `None`
/// (first contact) any correctly signed certificate is accepted and the caller must have
/// the user compare the presented fingerprint before trusting it.
#[derive(Debug)]
pub(crate) struct HostVerifier {
    pub pinned: Option<Fingerprint>,
    /// The fingerprint the host presented, recorded even when it fails the pin.
    pub presented: Arc<Mutex<Option<Fingerprint>>>,
    pub algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for HostVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        single_cert(intermediates)?;
        let fingerprint = Fingerprint::of(end_entity);
        *self.presented.lock().unwrap() = Some(fingerprint);
        match self.pinned {
            Some(pin) if pin != fingerprint => Err(rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        no_tls12()
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Host side. Requires a client certificate and proof of its key, but accepts unknown
/// devices so they can ask to pair; authorization happens per connection in the host
/// against the paired-device list.
#[derive(Debug)]
pub(crate) struct ClientVerifier {
    pub algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for ClientVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, _end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], _now: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        single_cert(intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        no_tls12()
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

pub(crate) fn server_builder() -> Result<rustls::ConfigBuilder<rustls::ServerConfig, rustls::server::WantsServerCert>, Error> {
    let provider = provider();
    let algs = provider.signature_verification_algorithms;
    Ok(rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(ClientVerifier { algs })))
}

pub(crate) fn client_builder(
    pinned: Option<Fingerprint>,
    presented: Arc<Mutex<Option<Fingerprint>>>,
) -> Result<rustls::ConfigBuilder<rustls::ClientConfig, rustls::client::WantsClientCert>, Error> {
    let provider = provider();
    let algs = provider.signature_verification_algorithms;
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous() // custom verifier; it still verifies signatures (see module docs)
        .with_custom_certificate_verifier(Arc::new(HostVerifier { pinned, presented, algs })))
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("valid idle timeout")));
    t.keep_alive_interval(Some(Duration::from_secs(10)));
    t.max_concurrent_bidi_streams(4u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    Arc::new(t)
}

pub(crate) fn finish_server(mut tls: rustls::ServerConfig) -> Result<quinn::ServerConfig, Error> {
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.send_tls13_tickets = 0;
    tls.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    tls.max_early_data_size = 0;
    let quic = QuicServerConfig::try_from(tls).map_err(|e| Error::Protocol(e.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(transport());
    Ok(config)
}

pub(crate) fn finish_client(mut tls: rustls::ClientConfig) -> Result<quinn::ClientConfig, Error> {
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.resumption = rustls::client::Resumption::disabled();
    tls.enable_early_data = false;
    let quic = QuicClientConfig::try_from(tls).map_err(|e| Error::Protocol(e.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(transport());
    Ok(config)
}

/// Fingerprint of the certificate that authenticated this connection.
pub(crate) fn peer_fingerprint(conn: &quinn::Connection) -> Option<Fingerprint> {
    let certs = conn.peer_identity()?.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().map(Fingerprint::of)
}
