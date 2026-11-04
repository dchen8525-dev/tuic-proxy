use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use psk_quic_proxy::server::{self, ServerOptions};
use psk_quic_proxy::{DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_MAX_STREAMS};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "QUIC proxy server with a pre-shared key",
    long_about = None
)]
struct Args {
    /// Shared secret, or `@PATH` to read it from a file (mode 0600 recommended).
    #[arg(long, value_name = "SECRET|@FILE")]
    psk: String,

    /// QUIC listen address (UDP).
    #[arg(long, default_value = "0.0.0.0:4433", value_name = "ADDR")]
    listen: SocketAddr,

    /// Allow proxying to loopback/private/link-local targets.
    #[arg(long)]
    allow_private: bool,
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
    let mut handle = server::start(ServerOptions {
        listen: args.listen,
        psk,
        allow_private: args.allow_private,
        max_streams: DEFAULT_MAX_STREAMS,
        idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
        prefer_ipv4: true,
    })
    .await?;

    println!("psk-server listening on {}", handle.addr());
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = handle.wait() => {},
    }
    handle.shutdown().await;
    Ok(())
}
