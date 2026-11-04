//! Client application: SOCKS5 listener plus a shared QUIC tunnel.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::auth::Psk;
use crate::socks5;
use crate::tls;
use crate::tunnel::Tunnel;

#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Proxy server address, `host:port` or `[ipv6]:port`.
    pub server: String,
    /// SOCKS5 listen address.
    pub socks_listen: SocketAddr,
    /// Local UDP bind address used by the QUIC endpoint.
    pub bind: SocketAddr,
    pub psk: String,
    pub max_streams: u32,
    pub idle_timeout: Duration,
    pub prefer_ipv4: bool,
    /// Number of QUIC connections the streams are spread over.
    pub connections: usize,
}

pub struct ClientHandle {
    socks_addr: SocketAddr,
    cancel: CancellationToken,
    task: JoinHandle<()>,
    endpoint: quinn::Endpoint,
}

impl ClientHandle {
    pub fn socks_addr(&self) -> SocketAddr {
        self.socks_addr
    }

    /// Wait until the accept loop stops on its own.
    pub async fn wait(&mut self) {
        let _ = (&mut self.task).await;
    }

    /// Stop accepting new SOCKS connections and close the QUIC endpoint.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        self.endpoint.close(0u32.into(), b"shutdown");
        let _ = self.task.await;
    }
}

pub async fn start(options: ClientOptions) -> Result<ClientHandle> {
    tls::install_crypto_provider();

    let psk = Arc::new(Psk::new(&options.psk)?);
    let config = tls::client_config(options.max_streams, options.idle_timeout)?;

    let (server, server_name) = resolve_server(&options.server, options.prefer_ipv4).await?;

    // The CLI leaves the bind address unspecified; match its address family to
    // the resolved server so an IPv6 server works without an extra flag.
    let bind = adaptive_bind(options.bind, server);
    let mut endpoint = quinn::Endpoint::client(bind)
        .with_context(|| format!("cannot bind the QUIC client to {bind}"))?;
    endpoint.set_default_client_config(config);

    let tunnel = Arc::new(Tunnel::new(
        endpoint.clone(),
        server,
        server_name,
        psk,
        options.connections,
    ));

    let listener = TcpListener::bind(options.socks_listen)
        .await
        .with_context(|| format!("cannot bind SOCKS5 to {}", options.socks_listen))?;
    let socks_addr = listener
        .local_addr()
        .context("cannot read the SOCKS5 address")?;

    let cancel = CancellationToken::new();
    let task = tokio::spawn(accept_loop(listener, tunnel, cancel.clone()));

    tracing::info!(%socks_addr, "SOCKS5 inbound listening");
    Ok(ClientHandle {
        socks_addr,
        cancel,
        task,
        endpoint,
    })
}

async fn accept_loop(listener: TcpListener, tunnel: Arc<Tunnel>, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((tcp, peer)) => {
                    let _ = tcp.set_nodelay(true);
                    let tunnel = tunnel.clone();
                    tokio::spawn(async move {
                        match socks5::serve(tcp, tunnel).await {
                            Ok(()) => tracing::debug!(%peer, "SOCKS connection closed"),
                            Err(error) => tracing::debug!(%peer, error = %format!("{error:#}"), "SOCKS connection failed"),
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot accept a SOCKS connection");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
    tracing::debug!("SOCKS5 accept loop stopped");
}

/// Match an unspecified bind address to the server's address family.
fn adaptive_bind(bind: SocketAddr, server: SocketAddr) -> SocketAddr {
    if bind.ip().is_unspecified() && bind.is_ipv4() != server.is_ipv4() {
        let ip = if server.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        };
        SocketAddr::new(ip, bind.port())
    } else {
        bind
    }
}

/// Split `host[:port]` and resolve it, returning the chosen address and the SNI name.
async fn resolve_server(server: &str, prefer_ipv4: bool) -> Result<(SocketAddr, String)> {
    let (host, port) = split_host_port(server)?;
    let mut addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("cannot resolve {server}"))?
        .collect();
    if addresses.is_empty() {
        bail!("{server} did not resolve to any address");
    }
    addresses.sort_by_key(|addr| {
        let is_v4 = addr.is_ipv4();
        if is_v4 == prefer_ipv4 {
            0
        } else {
            1
        }
    });
    let address = addresses[0];
    Ok((address, host))
}

/// Parse `host[:port]`, `[ipv6]` or `[ipv6]:port`.  The port defaults to 4433.
fn split_host_port(value: &str) -> Result<(String, u16)> {
    if let Some(rest) = value.strip_prefix('[') {
        let (host, remainder) = rest.split_once(']').ok_or_else(|| {
            anyhow!("invalid server address {value:?}, expected [ipv6] or [ipv6]:port")
        })?;
        if host.is_empty() {
            bail!("invalid server address {value:?}: empty host");
        }
        let port = match remainder.strip_prefix(':') {
            Some(port) => parse_port(value, port)?,
            None if remainder.is_empty() => crate::DEFAULT_SERVER_PORT,
            None => bail!("invalid server address {value:?}"),
        };
        return Ok((host.to_owned(), port));
    }

    match value.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                bail!("invalid server address {value:?}: empty host");
            }
            if host.contains(':') {
                bail!("IPv6 addresses must be bracketed, e.g. [::1]:4433");
            }
            Ok((host.to_owned(), parse_port(value, port)?))
        }
        None => {
            if value.is_empty() {
                bail!("the server address must not be empty");
            }
            Ok((value.to_owned(), crate::DEFAULT_SERVER_PORT))
        }
    }
}

fn parse_port(value: &str, port: &str) -> Result<u16> {
    port.parse::<u16>()
        .with_context(|| format!("invalid port in {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_host_and_port() {
        assert_eq!(
            split_host_port("example.com:443").expect("valid"),
            ("example.com".to_owned(), 443)
        );
        assert_eq!(
            split_host_port("[::1]:4433").expect("valid"),
            ("::1".to_owned(), 4433)
        );
        // No port means the default.
        assert_eq!(
            split_host_port("example.com").expect("valid"),
            ("example.com".to_owned(), crate::DEFAULT_SERVER_PORT)
        );
        assert_eq!(
            split_host_port("[::1]").expect("valid"),
            ("::1".to_owned(), crate::DEFAULT_SERVER_PORT)
        );
        assert!(split_host_port(":443").is_err());
        // Bare IPv6 is ambiguous and must be bracketed.
        assert!(split_host_port("::1").is_err());
    }

    #[test]
    fn mirror_bind_family_for_ipv6_servers() {
        let v4 = "0.0.0.0:0".parse().expect("v4");
        let v6_server = "[::1]:4433".parse().expect("v6 server");
        assert!(adaptive_bind(v4, v6_server).is_ipv6());
        let v6 = "[::]:0".parse().expect("v6");
        let v4_server = "1.2.3.4:4433".parse().expect("v4 server");
        assert!(adaptive_bind(v6, v4_server).is_ipv4());
        // A concrete bind address is left alone.
        let specific = "127.0.0.1:5000".parse().expect("specific");
        assert_eq!(adaptive_bind(specific, v6_server), specific);
    }
}
