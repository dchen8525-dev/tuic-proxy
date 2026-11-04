//! Minimal SOCKS5 inbound (RFC 1928 CONNECT).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::target::Target;
use crate::tunnel::Tunnel;

/// Handle one accepted SOCKS5 client.
pub async fn serve(mut tcp: TcpStream, tunnel: Arc<Tunnel>) -> Result<()> {
    negotiate(&mut tcp).await?;
    let target = read_request(&mut tcp).await?;

    let streams =
        match tokio::time::timeout(Duration::from_secs(10), tunnel.open_stream(&target)).await {
            Ok(Ok(streams)) => streams,
            Ok(Err(error)) => {
                let _ = send_reply(&mut tcp, 0x01).await;
                return Err(error);
            }
            Err(_) => {
                let _ = send_reply(&mut tcp, 0x06).await;
                bail!("opening the upstream stream timed out");
            }
        };

    let (send, mut recv) = streams;

    // The server reports whether it could reach the target before any payload.
    let mut status = [0u8; 1];
    recv.read_exact(&mut status)
        .await
        .context("cannot read the upstream status")?;
    if status[0] != crate::status::OK {
        let _ = send_reply(&mut tcp, socks_reply_for(status[0])).await;
        return Ok(());
    }

    reply(&mut tcp, 0x00).await?;
    crate::relay::run(tcp, send, recv).await
}

fn socks_reply_for(status: u8) -> u8 {
    match status {
        crate::status::DNS => 0x04,       // host unreachable
        crate::status::CONNECT => 0x05,   // connection refused
        crate::status::TIMEOUT => 0x06,   // TTL expired
        crate::status::FORBIDDEN => 0x02, // connection not allowed
        _ => 0x01,                        // general failure
    }
}

async fn negotiate(tcp: &mut TcpStream) -> Result<()> {
    let version = tcp
        .read_u8()
        .await
        .context("cannot read the SOCKS version")?;
    if version != 0x05 {
        bail!("unsupported SOCKS version {version}");
    }
    let count = tcp
        .read_u8()
        .await
        .context("cannot read the method count")? as usize;
    if count == 0 {
        bail!("no SOCKS methods offered");
    }
    let mut methods = vec![0u8; count];
    tcp.read_exact(&mut methods)
        .await
        .context("cannot read the offered methods")?;

    // Only the no-authentication method is supported.
    let method = if methods.contains(&0x00) { 0x00 } else { 0xff };

    tcp.write_all(&[0x05, method])
        .await
        .context("cannot send the method selection")?;

    if method == 0x00 {
        Ok(())
    } else {
        bail!("no acceptable SOCKS authentication method")
    }
}

async fn read_request(tcp: &mut TcpStream) -> Result<Target> {
    let mut header = [0u8; 4];
    tcp.read_exact(&mut header)
        .await
        .context("cannot read the SOCKS request")?;
    if header[0] != 0x05 {
        bail!("bad SOCKS request version");
    }
    if header[1] != 0x01 {
        let _ = send_reply(tcp, 0x07).await;
        bail!("unsupported SOCKS command {:#04x}", header[1]);
    }

    let target = match header[3] {
        0x01 => {
            let mut bytes = [0u8; 6];
            tcp.read_exact(&mut bytes)
                .await
                .context("cannot read the IPv4 target")?;
            let ip = std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
            let port = u16::from_be_bytes([bytes[4], bytes[5]]);
            Target::Ip((ip, port).into())
        }
        0x04 => {
            let mut bytes = [0u8; 18];
            tcp.read_exact(&mut bytes)
                .await
                .context("cannot read the IPv6 target")?;
            let ip = std::net::Ipv6Addr::from(
                <[u8; 16]>::try_from(&bytes[..16]).map_err(|_| anyhow::anyhow!("bad IPv6"))?,
            );
            let port = u16::from_be_bytes([bytes[16], bytes[17]]);
            Target::Ip((ip, port).into())
        }
        0x03 => {
            let len = tcp
                .read_u8()
                .await
                .context("cannot read the domain length")? as usize;
            if len == 0 {
                bail!("empty domain name");
            }
            let mut host = vec![0u8; len];
            tcp.read_exact(&mut host)
                .await
                .context("cannot read the domain name")?;
            let host = String::from_utf8(host).context("domain name is not valid UTF-8")?;
            let mut port = [0u8; 2];
            tcp.read_exact(&mut port)
                .await
                .context("cannot read the target port")?;
            Target::Domain(host, u16::from_be_bytes(port))
        }
        other => {
            let _ = send_reply(tcp, 0x08).await;
            bail!("unsupported address type {other:#04x}");
        }
    };

    Ok(target)
}

async fn send_reply(tcp: &mut TcpStream, code: u8) -> Result<()> {
    reply(tcp, code).await
}

async fn reply(tcp: &mut TcpStream, reply: u8) -> Result<()> {
    // BND.ADDR/BND.PORT are not meaningful here.
    let response = [0x05, reply, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
    tcp.write_all(&response)
        .await
        .context("cannot send the SOCKS reply")?;
    tcp.flush().await.context("cannot flush the SOCKS reply")?;
    Ok(())
}
