//! A small QUIC proxy with a pre-shared key.
//!
//! Topology:
//!
//! ```text
//! LAN client --SOCKS5--> psk-client --QUIC/TLS (PSK-authenticated)--> psk-server --> target
//! ```
//!
//! Every QUIC connection carries one authenticated control stream and any number
//! of proxied bidirectional streams.  One SOCKS5 TCP connection maps to one QUIC
//! stream, so a single QUIC connection multiplexes all of the LAN clients.

#![forbid(unsafe_code)]

pub mod auth;
pub mod cli;
pub mod client;
pub mod relay;
pub mod server;
pub mod socks5;
pub mod target;
pub mod tls;
pub mod tunnel;

/// TLS ALPN protocol identifier.  Both peers must agree on it.
pub const ALPN: &[u8] = b"psk-quic-proxy/1";

/// Protocol version carried in the authentication hello.
pub const VERSION: u8 = 1;

/// Status byte written by the server at the start of every data stream.
pub mod status {
    /// The target connection is established; the rest of the stream is raw data.
    pub const OK: u8 = 0;
    /// Malformed request or unsupported address type.
    pub const BAD_REQUEST: u8 = 1;
    /// The server could not resolve the target host name.
    pub const DNS: u8 = 2;
    /// The server could not connect to the target.
    pub const CONNECT: u8 = 3;
    /// Connecting to the target timed out.
    pub const TIMEOUT: u8 = 4;
    /// The target address is blocked by policy (private/loopback ranges).
    pub const FORBIDDEN: u8 = 5;
}

/// Default maximum number of concurrently open proxied streams per QUIC connection.
pub const DEFAULT_MAX_STREAMS: u32 = 1024;

/// Default QUIC idle timeout in seconds.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 60;

/// Default number of QUIC connections the client spreads streams over.
pub const DEFAULT_CONNECTIONS: usize = 4;

/// Default port used when `--server` omits one.
pub const DEFAULT_SERVER_PORT: u16 = 4433;
