//! Target address encoding shared by the SOCKS5 inbound and the QUIC streams.
//!
//! The encoding deliberately mirrors the SOCKS5 address types so that a
//! domain name stays a domain name all the way to the server.  The server is
//! responsible for DNS resolution.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{bail, Context, Result};
use quinn::RecvStream;
use tokio::io::AsyncReadExt;

/// ATYP values from RFC 1928.
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl Target {
    /// Encode as `[atyp][addr][port:be]`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(19);
        match self {
            Target::Ip(SocketAddr::V4(addr)) => {
                out.push(ATYP_IPV4);
                out.extend_from_slice(&addr.ip().octets());
                out.extend_from_slice(&addr.port().to_be_bytes());
            }
            Target::Ip(SocketAddr::V6(addr)) => {
                out.push(ATYP_IPV6);
                out.extend_from_slice(&addr.ip().octets());
                out.extend_from_slice(&addr.port().to_be_bytes());
            }
            Target::Domain(host, port) => {
                if host.is_empty() || host.len() > 255 {
                    bail!("domain name length must be between 1 and 255 bytes");
                }
                out.push(ATYP_DOMAIN);
                out.push(host.len() as u8);
                out.extend_from_slice(host.as_bytes());
                out.extend_from_slice(&port.to_be_bytes());
            }
        }
        Ok(out)
    }

    /// Read an encoded target from a QUIC stream.
    pub async fn read_from(recv: &mut RecvStream) -> Result<Self> {
        let atyp = recv
            .read_u8()
            .await
            .context("cannot read the address type")?;
        match atyp {
            ATYP_IPV4 => {
                let mut bytes = [0u8; 6];
                recv.read_exact(&mut bytes)
                    .await
                    .context("cannot read the IPv4 target")?;
                let ip = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
                let port = u16::from_be_bytes([bytes[4], bytes[5]]);
                Ok(Target::Ip(SocketAddr::new(IpAddr::V4(ip), port)))
            }
            ATYP_IPV6 => {
                let mut bytes = [0u8; 18];
                recv.read_exact(&mut bytes)
                    .await
                    .context("cannot read the IPv6 target")?;
                let ip = Ipv6Addr::from(
                    <[u8; 16]>::try_from(&bytes[..16]).map_err(|_| anyhow::anyhow!("bad IPv6"))?,
                );
                let port = u16::from_be_bytes([bytes[16], bytes[17]]);
                Ok(Target::Ip(SocketAddr::new(IpAddr::V6(ip), port)))
            }
            ATYP_DOMAIN => {
                let len = recv
                    .read_u8()
                    .await
                    .context("cannot read the domain length")? as usize;
                if len == 0 {
                    bail!("empty domain name");
                }
                let mut host = vec![0u8; len];
                recv.read_exact(&mut host)
                    .await
                    .context("cannot read the domain name")?;
                let host = String::from_utf8(host).context("domain name is not valid UTF-8")?;
                let mut port = [0u8; 2];
                recv.read_exact(&mut port)
                    .await
                    .context("cannot read the target port")?;
                Ok(Target::Domain(host, u16::from_be_bytes(port)))
            }
            other => bail!("unsupported address type {other:#04x}"),
        }
    }
}

/// Address ranges that are blocked unless `--allow-private` is given.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // 100.64.0.0/10 (carrier-grade NAT).
            let octets = v4.octets();
            let shared = octets[0] == 100 && (64..=127).contains(&octets[1]);
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || shared
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_roundtrip() {
        let target = Target::Ip("192.0.2.10:443".parse().expect("addr"));
        let encoded = target.encode().expect("encode");
        assert_eq!(encoded, vec![0x01, 192, 0, 2, 10, 0x01, 0xbb]);
    }

    #[test]
    fn domain_roundtrip_encoding() {
        let target = Target::Domain("example.com".into(), 80);
        let encoded = target.encode().expect("encode");
        assert_eq!(encoded[0], 0x03);
        assert_eq!(encoded[1], 11);
        assert_eq!(&encoded[2..13], b"example.com");
        assert_eq!(&encoded[13..], &80u16.to_be_bytes());
    }

    #[test]
    fn rejects_empty_and_oversized_domains() {
        assert!(Target::Domain(String::new(), 80).encode().is_err());
        assert!(Target::Domain("a".repeat(256), 80).encode().is_err());
    }

    #[test]
    fn private_ranges_are_blocked() {
        assert!(is_blocked_ip("127.0.0.1".parse().expect("ip")));
        assert!(is_blocked_ip("10.1.2.3".parse().expect("ip")));
        assert!(is_blocked_ip("192.168.1.1".parse().expect("ip")));
        assert!(is_blocked_ip("::1".parse().expect("ip")));
        assert!(!is_blocked_ip("8.8.8.8".parse().expect("ip")));
        assert!(!is_blocked_ip("2001:4860:4860::8888".parse().expect("ip")));
    }
}
