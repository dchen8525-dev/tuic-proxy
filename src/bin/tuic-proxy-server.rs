use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use tuic_proxy::congestion::Congestion;
use tuic_proxy::server::{self, ServerOptions};
use tuic_proxy::{DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_MAX_STREAMS};

#[derive(Debug, Parser)]
#[command(
    name = "tuic-proxy-server",
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

    /// QUIC congestion controller: `cubic` (default), `bbr`, or
    /// `loss-tolerant` (BBR whose window does not collapse on packet loss).
    /// Use a loss-tolerant mode on paths that drop a steady share of UDP.
    #[arg(long, default_value = "cubic", value_parser = Congestion::clap_parse)]
    congestion: Congestion,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let psk = tuic_proxy::cli::read_psk(&args.psk)?;
    let mut handle = server::start(ServerOptions {
        listen: args.listen,
        psk,
        allow_private: args.allow_private,
        max_streams: DEFAULT_MAX_STREAMS,
        idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
        prefer_ipv4: true,
        congestion: args.congestion,
    })
    .await?;
    tracing::info!(congestion = %args.congestion, "congestion controller selected");

    println!("tuic-proxy-server listening on {}", handle.addr());
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
    use clap::CommandFactory;

    #[test]
    fn congestion_flag_defaults_to_cubic() {
        let args = Args::try_parse_from(["tuic-proxy-server", "--psk", "0123456789abcdef"])
            .expect("valid arguments");
        assert_eq!(args.congestion, Congestion::Cubic);
        let args = Args::try_parse_from([
            "tuic-proxy-server",
            "--psk",
            "0123456789abcdef",
            "--congestion",
            "loss-tolerant",
        ])
        .expect("valid arguments");
        assert_eq!(args.congestion, Congestion::LossTolerant);
    }

    #[test]
    fn version_reports_the_binary_name() {
        // Both binaries live in one package, so clap would otherwise fall back
        // to the package name (tuic-proxy).
        assert_eq!(Args::command().get_name(), "tuic-proxy-server");
    }
}
