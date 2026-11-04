use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use tuic_proxy::client::{self, ClientOptions};
use tuic_proxy::congestion::Congestion;
use tuic_proxy::{DEFAULT_CONNECTIONS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_MAX_STREAMS};

#[derive(Debug, Parser)]
#[command(
    name = "tuic-proxy-client",
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

    /// QUIC congestion controller: `cubic` (default), `bbr`, or
    /// `loss-tolerant` (BBR whose window does not collapse on packet loss).
    /// Use a loss-tolerant mode on paths that drop a steady share of UDP.
    #[arg(long, default_value = "cubic", value_parser = Congestion::clap_parse)]
    congestion: Congestion,
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
    let psk = tuic_proxy::cli::read_psk(&args.psk)?;

    let mut handle = client::start(ClientOptions {
        server: args.server,
        socks_listen: args.socks,
        bind: SocketAddr::from(([0, 0, 0, 0], 0)),
        psk,
        max_streams: DEFAULT_MAX_STREAMS,
        idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
        prefer_ipv4: true,
        connections: DEFAULT_CONNECTIONS,
        congestion: args.congestion,
    })
    .await?;
    tracing::info!(congestion = %args.congestion, "congestion controller selected");

    println!(
        "tuic-proxy-client SOCKS5 listening on {}",
        handle.socks_addr()
    );
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
    fn version_reports_the_binary_name() {
        // Both binaries live in one package, so clap would otherwise fall back
        // to the package name (tuic-proxy).
        assert_eq!(Args::command().get_name(), "tuic-proxy-client");
    }

    #[test]
    fn congestion_flag_parses_the_supported_modes() {
        assert_eq!(Congestion::clap_parse("cubic"), Ok(Congestion::Cubic));
        assert_eq!(Congestion::clap_parse("bbr"), Ok(Congestion::Bbr));
        assert_eq!(
            Congestion::clap_parse("loss-tolerant"),
            Ok(Congestion::LossTolerant)
        );
        assert!(Congestion::clap_parse("vegas").is_err());
        // The default keeps CUBIC.
        let args = Args::try_parse_from([
            "tuic-proxy-client",
            "--server",
            "203.0.113.1",
            "--psk",
            "0123456789abcdef",
        ])
        .expect("valid arguments");
        assert_eq!(args.congestion, Congestion::Cubic);
    }

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
