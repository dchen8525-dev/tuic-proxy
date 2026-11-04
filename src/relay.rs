//! Byte relay between a TCP socket and a QUIC bidirectional stream.

use anyhow::{Context, Result};
use quinn::{RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

const BUFFER_SIZE: usize = 32 * 1024;

/// Relay until both directions are finished.
///
/// A TCP EOF calls `finish()` on the QUIC send side (half-close), and a QUIC
/// stream EOF shuts down the TCP write side, preserving half-close semantics.
pub async fn run(tcp: TcpStream, send: SendStream, recv: RecvStream) -> Result<()> {
    let (reader, writer) = tcp.into_split();
    tokio::try_join!(tcp_to_quic(reader, send), quic_to_tcp(recv, writer),)?;
    Ok(())
}

async fn tcp_to_quic(mut reader: OwnedReadHalf, mut send: SendStream) -> Result<()> {
    let mut buffer = vec![0u8; BUFFER_SIZE];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .context("cannot read from the TCP socket")?;
        if count == 0 {
            send.finish().context("cannot finish the QUIC stream")?;
            return Ok(());
        }
        send.write_all(&buffer[..count])
            .await
            .context("cannot write to the QUIC stream")?;
    }
}

async fn quic_to_tcp(mut recv: RecvStream, mut writer: OwnedWriteHalf) -> Result<()> {
    let mut buffer = vec![0u8; BUFFER_SIZE];
    loop {
        match recv
            .read(&mut buffer)
            .await
            .context("cannot read from the QUIC stream")?
        {
            Some(0) | None => {
                writer
                    .shutdown()
                    .await
                    .context("cannot shut down the TCP write side")?;
                return Ok(());
            }
            Some(count) => {
                writer
                    .write_all(&buffer[..count])
                    .await
                    .context("cannot write to the TCP socket")?;
            }
        }
    }
}
