//! Client-side QUIC tunnel: a small pool of lazily opened, authenticated
//! connections shared by all SOCKS5 sessions.
//!
//! A single QUIC connection is limited by one congestion window.  On a
//! long-RTT path that caps aggregate throughput, so streams are spread over
//! several connections instead (round-robin).  The pool is opened lazily and
//! each slot reconnects on demand.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::sync::Mutex;

use crate::auth::{self, Psk};
use crate::target::Target;
use crate::tls::CONNECT_TIMEOUT;

pub struct Tunnel {
    endpoint: Endpoint,
    server: SocketAddr,
    server_name: String,
    psk: Arc<Psk>,
    pool: Vec<Mutex<Option<Connection>>>,
    next: AtomicUsize,
}

impl Tunnel {
    pub fn new(
        endpoint: Endpoint,
        server: SocketAddr,
        server_name: String,
        psk: Arc<Psk>,
        connections: usize,
    ) -> Self {
        let connections = connections.max(1);
        Self {
            endpoint,
            server,
            server_name,
            psk,
            pool: (0..connections).map(|_| Mutex::new(None)).collect(),
            next: AtomicUsize::new(0),
        }
    }

    /// Open a bidirectional QUIC stream for one proxied connection and write
    /// the target header.  The caller owns both halves afterwards.
    pub async fn open_stream(&self, target: &Target) -> Result<(SendStream, RecvStream)> {
        let header = target.encode()?;
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.pool.len();

        for attempt in 0..2 {
            let connection = self.connection(index).await?;
            match connection.open_bi().await {
                Ok((mut send, recv)) => {
                    send.write_all(&header)
                        .await
                        .context("cannot write the target header")?;
                    return Ok((send, recv));
                }
                Err(error) if attempt == 0 => {
                    tracing::debug!(%error, "stored QUIC connection is unusable; reconnecting");
                    *self.pool[index].lock().await = None;
                }
                Err(error) => {
                    return Err(anyhow!("cannot open a QUIC stream: {error}"));
                }
            }
        }

        Err(anyhow!("cannot open a QUIC stream"))
    }

    async fn connection(&self, index: usize) -> Result<Connection> {
        let mut guard = self.pool[index].lock().await;
        if let Some(connection) = guard.as_ref() {
            return Ok(connection.clone());
        }

        let connection = tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.endpoint.connect(self.server, &self.server_name)?,
        )
        .await
        .map_err(|_| anyhow!("connecting to {} timed out", self.server))?
        .context("cannot connect to the proxy server")?;

        tokio::time::timeout(
            CONNECT_TIMEOUT,
            auth::authenticate_client(&connection, &self.psk),
        )
        .await
        .map_err(|_| anyhow!("PSK authentication timed out"))?
        .context("PSK authentication failed")?;

        tracing::info!(server = %self.server, slot = index, "QUIC tunnel established");
        *guard = Some(connection.clone());
        spawn_stats(connection.clone(), index);
        Ok(connection)
    }
}

/// Periodically log QUIC path statistics when debug logging is enabled.
fn spawn_stats(connection: Connection, slot: usize) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if connection.close_reason().is_some() {
                break;
            }
            let stats = connection.stats();
            tracing::debug!(
                slot,
                rtt_ms = stats.path.rtt.as_millis() as u64,
                cwnd = stats.path.cwnd,
                sent_packets = stats.path.sent_packets,
                lost_packets = stats.path.lost_packets,
                congestion_events = stats.path.congestion_events,
                mtu = stats.path.current_mtu,
                "QUIC path stats"
            );
        }
    });
}

/// Idle timeout helper used by the binaries.
pub fn default_idle_timeout() -> Duration {
    Duration::from_secs(crate::DEFAULT_IDLE_TIMEOUT_SECS)
}
