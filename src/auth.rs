//! PSK authentication performed inside an already-established QUIC connection.
//!
//! TLS encrypts the link, but the certificate is an ephemeral self-signed one
//! that the client does not verify.  Identity therefore comes from the PSK:
//! both sides prove knowledge of the key with an HMAC that is bound to the
//! connection through a TLS exporter value.  A man-in-the-middle that
//! terminates TLS on both sides cannot relay the proofs because each TLS leg
//! produces a different exporter value.
//!
//! Wire format of the fixed-size hello (69 bytes, in both directions):
//!
//! ```text
//! magic[4] = "PQP1" | version: u8 | nonce[32] | hmac[32]
//! ```

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use quinn::Connection;
use rand::RngCore;
use sha2::Sha256;

use crate::VERSION;

const MAGIC: &[u8; 4] = b"PQP1";
const NONCE_LEN: usize = 32;
const TAG_LEN: usize = 32;
const HELLO_LEN: usize = 4 + 1 + NONCE_LEN + TAG_LEN;

/// Minimum accepted PSK length.  The string is stretched with HKDF, but a very
/// short passphrase still has very little entropy.
const MIN_PSK_LEN: usize = 16;

/// A pre-shared key, expanded once at startup.
#[derive(Clone)]
pub struct Psk {
    key: [u8; 32],
}

impl Psk {
    /// Expand the command-line PSK string into a fixed-size key.
    pub fn new(secret: &str) -> Result<Self> {
        let bytes = secret.as_bytes();
        if bytes.len() < MIN_PSK_LEN {
            bail!(
				"PSK must be at least {MIN_PSK_LEN} bytes (use 32+ random bytes, e.g. `openssl rand -hex 32`)"
			);
        }
        if bytes.len() < 32 {
            tracing::warn!(
                "PSK is shorter than 32 bytes; use a longer random secret for better security"
            );
        }

        let hk = Hkdf::<Sha256>::new(Some(b"psk-quic-proxy/v1/salt"), bytes);
        let mut key = [0u8; 32];
        hk.expand(b"psk-quic-proxy/v1/auth-key", &mut key)
            .map_err(|_| anyhow!("cannot derive the PSK"))?;
        Ok(Self { key })
    }

    fn mac(&self, label: &[u8], exporter: &[u8], parts: &[&[u8]]) -> Result<Hmac<Sha256>> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key)
            .map_err(|_| anyhow!("invalid HMAC key length"))?;
        mac.update(b"psk-quic-proxy/v1/");
        mac.update(label);
        mac.update(exporter);
        for part in parts {
            mac.update(part);
        }
        Ok(mac)
    }

    fn proof(&self, label: &[u8], exporter: &[u8], parts: &[&[u8]]) -> Result<[u8; TAG_LEN]> {
        let mac = self.mac(label, exporter, parts)?;
        Ok(mac.finalize().into_bytes().into())
    }

    fn verify(&self, label: &[u8], exporter: &[u8], parts: &[&[u8]], tag: &[u8]) -> Result<()> {
        self.mac(label, exporter, parts)?
            .verify_slice(tag)
            .map_err(|_| anyhow!("PSK authentication failed"))
    }
}

/// Bind the PSK proof to this specific QUIC/TLS connection.
fn exporter(conn: &Connection) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, b"psk-quic-proxy/v1 exporter", &[])
        .map_err(|error| anyhow!("cannot export QUIC keying material: {error:?}"))?;
    Ok(out)
}

fn random_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

fn encode_hello(nonce: &[u8; NONCE_LEN], tag: &[u8; TAG_LEN]) -> [u8; HELLO_LEN] {
    let mut out = [0u8; HELLO_LEN];
    out[..4].copy_from_slice(MAGIC);
    out[4] = VERSION;
    out[5..5 + NONCE_LEN].copy_from_slice(nonce);
    out[5 + NONCE_LEN..].copy_from_slice(tag);
    out
}

fn decode_hello(hello: &[u8; HELLO_LEN]) -> Result<(&[u8; NONCE_LEN], &[u8])> {
    if &hello[..4] != MAGIC {
        bail!("bad authentication magic");
    }
    if hello[4] != VERSION {
        bail!("unsupported protocol version {}", hello[4]);
    }
    let nonce: &[u8; NONCE_LEN] = hello[5..5 + NONCE_LEN]
        .try_into()
        .map_err(|_| anyhow!("bad nonce"))?;
    Ok((nonce, &hello[5 + NONCE_LEN..]))
}

/// Client side: open the control stream, prove the PSK, and verify the server.
pub async fn authenticate_client(conn: &Connection, psk: &Psk) -> Result<()> {
    // `endpoint.connect(..).await` only resolves after the QUIC handshake, so
    // the TLS exporter is available here.
    let exporter = exporter(conn)?;
    let client_nonce = random_nonce();
    let client_tag = psk.proof(b"client", &exporter, &[&client_nonce])?;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .context("cannot open the authentication stream")?;
    send.write_all(&encode_hello(&client_nonce, &client_tag))
        .await
        .context("cannot write the client hello")?;
    send.finish().context("cannot finish the client hello")?;

    let mut reply = [0u8; HELLO_LEN];
    recv.read_exact(&mut reply)
        .await
        .context("cannot read the server hello")?;

    let (server_nonce, server_tag) = decode_hello(&reply)?;
    psk.verify(
        b"server",
        &exporter,
        &[&client_nonce, server_nonce],
        server_tag,
    )?;

    tracing::debug!("PSK authentication succeeded");
    Ok(())
}

/// Server side: read the client proof and answer with the server proof.
pub async fn authenticate_server(conn: &Connection, psk: &Psk) -> Result<()> {
    // `Incoming::await` only resolves after the QUIC handshake, so the TLS
    // exporter is available here.
    let exporter = exporter(conn)?;
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .context("client did not open the authentication stream")?;

    let mut hello = [0u8; HELLO_LEN];
    recv.read_exact(&mut hello)
        .await
        .context("cannot read the client hello")?;
    let (client_nonce, client_tag) = decode_hello(&hello)?;

    psk.verify(b"client", &exporter, &[client_nonce], client_tag)?;

    let server_nonce = random_nonce();
    let server_tag = psk.proof(b"server", &exporter, &[client_nonce, &server_nonce])?;
    send.write_all(&encode_hello(&server_nonce, &server_tag))
        .await
        .context("cannot write the server hello")?;
    send.finish().context("cannot finish the server hello")?;

    tracing::debug!("PSK authentication succeeded");
    Ok(())
}

/// Convenience wrapper used by the client tunnel.
pub type SharedPsk = Arc<Psk>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic() {
        let a = Psk::new("0123456789abcdef0123456789abcdef").expect("valid psk");
        let b = Psk::new("0123456789abcdef0123456789abcdef").expect("valid psk");
        let c = Psk::new("0123456789abcdef0123456789abcdee").expect("valid psk");
        assert_eq!(a.key, b.key);
        assert_ne!(a.key, c.key);
    }

    #[test]
    fn short_psk_is_rejected() {
        assert!(Psk::new("too-short").is_err());
    }

    #[test]
    fn proofs_verify_and_bind_to_context() {
        let psk = Psk::new("0123456789abcdef0123456789abcdef").expect("valid psk");
        let exporter = [7u8; 32];
        let nonce = [9u8; 32];
        let tag = psk.proof(b"client", &exporter, &[&nonce]).expect("proof");
        psk.verify(b"client", &exporter, &[&nonce], &tag)
            .expect("valid proof");

        assert!(psk.verify(b"server", &exporter, &[&nonce], &tag).is_err());
        let other_exporter = [8u8; 32];
        assert!(psk
            .verify(b"client", &other_exporter, &[&nonce], &tag)
            .is_err());
        let other_nonce = [10u8; 32];
        assert!(psk
            .verify(b"client", &exporter, &[&other_nonce], &tag)
            .is_err());
    }

    #[test]
    fn hello_roundtrip() {
        let nonce = [3u8; NONCE_LEN];
        let tag = [4u8; TAG_LEN];
        let hello = encode_hello(&nonce, &tag);
        let (decoded_nonce, decoded_tag) = decode_hello(&hello).expect("decodes");
        assert_eq!(decoded_nonce, &nonce);
        assert_eq!(decoded_tag, &tag);
    }
}
