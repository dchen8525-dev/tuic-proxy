//! Server application: QUIC listener, PSK authentication, DNS resolution and
//! outbound TCP connections.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::auth::{self, Psk};
use crate::target::{is_blocked_ip, Target};
use crate::{status, tls};

const DNS_TIMEOUT: Duration = Duration::from_secs(5);
const TARGET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub listen: SocketAddr,
    pub psk: String,
    /// Allow connections to loopback/private/link-local targets.
    pub allow_private: bool,
    pub max_streams: u32,
    pub idle_timeout: Duration,
    pub prefer_ipv4: bool,
}

pub struct ServerHandle {
    addr: SocketAddr,
    cancel: CancellationToken,
    task: JoinHandle<()>,
    endpoint: Endpoint,
    established_connections: Arc<AtomicUsize>,
}

impl ServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Number of QUIC connections that completed PSK authentication.
    pub fn established_connections(&self) -> usize {
        self.established_connections.load(Ordering::Relaxed)
    }

    /// Wait until the accept loop stops on its own.
    pub async fn wait(&mut self) {
        let _ = (&mut self.task).await;
    }

    pub async fn shutdown(self) {
        self.cancel.cancel();
        self.endpoint.close(0u32.into(), b"shutdown");
        let _ = self.task.await;
    }
}

pub async fn start(options: ServerOptions) -> Result<ServerHandle> {
    tls::install_crypto_provider();

    let psk = Arc::new(Psk::new(&options.psk)?);
    let config = tls::server_config(options.max_streams, options.idle_timeout)?;
    let endpoint = Endpoint::server(config, options.listen)
        .with_context(|| format!("cannot bind QUIC to {}", options.listen))?;
    let addr = endpoint
        .local_addr()
        .context("cannot read the QUIC address")?;

    let options = Arc::new(options);
    let established_connections = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let task = tokio::spawn(accept_loop(
        endpoint.clone(),
        options,
        psk,
        cancel.clone(),
        established_connections.clone(),
    ));

    tracing::info!(%addr, "QUIC proxy listening");
    Ok(ServerHandle {
        addr,
        cancel,
        task,
        endpoint,
        established_connections,
    })
}

async fn accept_loop(
    endpoint: Endpoint,
    options: Arc<ServerOptions>,
    psk: Arc<Psk>,
    cancel: CancellationToken,
    established_connections: Arc<AtomicUsize>,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            incoming = endpoint.accept() => match incoming {
                Some(incoming) => {
                    let options = options.clone();
                    let psk = psk.clone();
                    let established = established_connections.clone();
                    tokio::spawn(async move {
                        match incoming.await {
                            Ok(connection) => {
                                if let Err(error) = handle_connection(connection, psk, options, established).await {
                                    tracing::debug!(error = %format!("{error:#}"), "connection closed");
                                }
                            }
                            Err(error) => tracing::debug!(%error, "QUIC handshake failed"),
                        }
                    });
                }
                None => break,
            }
        }
    }
    tracing::debug!("QUIC accept loop stopped");
}

async fn handle_connection(
    connection: Connection,
    psk: Arc<Psk>,
    options: Arc<ServerOptions>,
    established_connections: Arc<AtomicUsize>,
) -> Result<()> {
    let remote = connection.remote_address();
    if let Err(error) = auth::authenticate_server(&connection, &psk).await {
        connection.close(0u32.into(), b"authentication failed");
        return Err(error.context("PSK authentication failed"));
    }
    established_connections.fetch_add(1, Ordering::Relaxed);
    tracing::info!(%remote, "client authenticated");

    loop {
        match connection.accept_bi().await {
            Ok((send, recv)) => {
                let options = options.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_stream(send, recv, options).await {
                        tracing::debug!(%remote, error = %format!("{error:#}"), "proxied stream failed");
                    }
                });
            }
            Err(error) => {
                tracing::debug!(%remote, %error, "connection closed");
                break;
            }
        }
    }
    Ok(())
}

async fn handle_stream(
    mut send: SendStream,
    mut recv: RecvStream,
    options: Arc<ServerOptions>,
) -> Result<()> {
    let target = match Target::read_from(&mut recv).await {
        Ok(target) => target,
        Err(error) => {
            let _ = send.write_all(&[status::BAD_REQUEST]).await;
            let _ = send.finish();
            return Err(error);
        }
    };

    let tcp = match connect_target(&target, &options).await {
        Ok(tcp) => tcp,
        Err((code, error)) => {
            let _ = send.write_all(&[code]).await;
            let _ = send.finish();
            return Err(error);
        }
    };

    send.write_all(&[status::OK])
        .await
        .context("cannot send the stream status")?;
    tracing::debug!(target = %format_target(&target), "stream connected");
    crate::relay::run(tcp, send, recv).await
}

async fn connect_target(
    target: &Target,
    options: &ServerOptions,
) -> Result<TcpStream, (u8, anyhow::Error)> {
    let addresses = resolve(target, options.prefer_ipv4)
        .await
        .map_err(|error| (status::DNS, error))?;

    if !options.allow_private {
        if let Some(blocked) = addresses.iter().find(|addr| is_blocked_ip(addr.ip())) {
            return Err((
                status::FORBIDDEN,
                anyhow!("target {target:?} resolves to the blocked address {blocked}"),
            ));
        }
    }

    let mut timed_out = true;
    let mut last_error = None;
    for address in addresses {
        match tokio::time::timeout(TARGET_CONNECT_TIMEOUT, TcpStream::connect(address)).await {
            Ok(Ok(tcp)) => {
                let _ = tcp.set_nodelay(true);
                return Ok(tcp);
            }
            Ok(Err(error)) => {
                timed_out = false;
                last_error = Some(anyhow!("{address}: {error}"));
            }
            Err(_) => {
                last_error = Some(anyhow!("{address}: timed out"));
            }
        }
    }

    Err((
        if timed_out {
            status::TIMEOUT
        } else {
            status::CONNECT
        },
        last_error.unwrap_or_else(|| anyhow!("no usable address")),
    ))
}

async fn resolve(target: &Target, prefer_ipv4: bool) -> Result<Vec<SocketAddr>> {
    match target {
        Target::Ip(address) => Ok(vec![*address]),
        Target::Domain(host, port) => {
            let resolved =
                tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host.as_str(), *port)))
                    .await
                    .map_err(|_| anyhow!("resolving {host} timed out"))?
                    .with_context(|| format!("cannot resolve {host}"))?;

            let mut ipv4 = Vec::new();
            let mut ipv6 = Vec::new();
            for address in resolved {
                if address.is_ipv4() {
                    ipv4.push(address);
                } else {
                    ipv6.push(address);
                }
            }

            let mut addresses = if prefer_ipv4 {
                ipv4.extend(ipv6);
                ipv4
            } else {
                ipv6.extend(ipv4);
                ipv6
            };
            addresses.dedup();
            if addresses.is_empty() {
                bail!("{host} did not resolve to any address");
            }
            Ok(addresses)
        }
    }
}

fn format_target(target: &Target) -> String {
    match target {
        Target::Ip(address) => address.to_string(),
        Target::Domain(host, port) => format!("{host}:{port}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_ip_targets_without_dns() {
        let target: Target = Target::Ip("127.0.0.1:8080".parse().expect("addr"));
        let addresses = resolve(&target, true).await.expect("resolve");
        assert_eq!(addresses.len(), 1);
    }
}
