use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use psk_quic_proxy::client::{self, ClientOptions};
use psk_quic_proxy::{DEFAULT_CONNECTIONS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_MAX_STREAMS};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "SOCKS5 client for the PSK QUIC proxy",
    long_about = None
)]
struct Args {
    /// Proxy server address: host[:port] or [ipv6][:port].  The port defaults to 4433.
    #[arg(long, value_name = "HOST[:PORT]")]
    server: String,

    /// Shared secret, or `@PATH` to read it from a file.
    #[arg(long, value_name = "SECRET|@FILE")]
    psk: String,

    /// SOCKS5 listen address, or just a port (binds 127.0.0.1).
    #[arg(long, default_value = "127.0.0.1:1080", value_name = "ADDR|PORT", value_parser = parse_listen)]
    socks: SocketAddr,
}

/// Accept either `ADDR:PORT` or a bare `PORT` (meaning `127.0.0.1:PORT`).
fn parse_listen(value: &str) -> Result<SocketAddr, String> {
    if let Ok(port) = value.parse::<u16>() {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    value
        .parse()
        .map_err(|_| format!("invalid listen address {value:?}, expected ADDR:PORT or PORT"))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let psk = psk_quic_proxy::cli::read_psk(&args.psk)?;

    let mut handle = client::start(ClientOptions {
        server: args.server,
        socks_listen: args.socks,
        bind: SocketAddr::from(([0, 0, 0, 0], 0)),
        psk,
        max_streams: DEFAULT_MAX_STREAMS,
        idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
        prefer_ipv4: true,
        connections: DEFAULT_CONNECTIONS,
    })
    .await?;

    println!("psk-client SOCKS5 listening on {}", handle.socks_addr());
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = handle.wait() => {},
    }
    handle.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_accepts_a_bare_port() {
        assert_eq!(
            parse_listen("1080").expect("port"),
            "127.0.0.1:1080".parse().expect("addr")
        );
        assert_eq!(
            parse_listen("0.0.0.0:1080").expect("addr"),
            "0.0.0.0:1080".parse().expect("addr")
        );
        assert!(parse_listen("nope").is_err());
    }
}
