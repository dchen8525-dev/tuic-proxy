//! TLS setup for QUIC.
//!
//! QUIC always runs over TLS 1.3, so the PSK cannot replace TLS.  Instead the
//! server presents an ephemeral self-signed certificate; the client does not
//! verify it because peer identity is established afterwards by the PSK
//! handshake in [`crate::auth`], which is bound to the TLS exporter value.
//!
//! This keeps the deployment to a single shared string: no certificate files,
//! no CA, no domain name.  The trade-off is documented in the README.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::ALPN;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Install the process-wide rustls crypto provider exactly once.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Build the server-side QUIC configuration with an ephemeral self-signed certificate.
pub fn server_config(max_streams: u32, idle_timeout: Duration) -> Result<quinn::ServerConfig> {
    let certified = rcgen::generate_simple_self_signed(vec!["psk-quic-proxy".to_owned()])?;
    let cert = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(Arc::new(transport(max_streams, idle_timeout)?));
    Ok(config)
}

/// Build the client-side QUIC configuration.
pub fn client_config(max_streams: u32, idle_timeout: Duration) -> Result<quinn::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PskServerVerifier { provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(Arc::new(transport(max_streams, idle_timeout)?));
    Ok(config)
}

fn transport(max_streams: u32, idle_timeout: Duration) -> Result<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(max_streams.into());
    transport.max_concurrent_uni_streams(16u32.into());
    transport.keep_alive_interval(Some(KEEP_ALIVE));
    transport.max_idle_timeout(Some(
        idle_timeout
            .try_into()
            .map_err(|_| anyhow!("idle timeout is out of range"))?,
    ));
    Ok(transport)
}

/// Accepts any certificate chain but still verifies the handshake signature.
///
/// Identity is not taken from the certificate; it is proven by the PSK
/// handshake bound to this connection's TLS exporter.  Certificate chain
/// verification is therefore intentionally skipped.
#[derive(Debug)]
struct PskServerVerifier {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PskServerVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
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
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
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
