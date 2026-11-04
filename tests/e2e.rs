//! End-to-end tests: real QUIC over loopback, real SOCKS5 clients.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tuic_proxy::client::{self, ClientHandle, ClientOptions};
use tuic_proxy::congestion::Congestion;
use tuic_proxy::server::{self, ServerHandle, ServerOptions};

const PSK: &str = "test-psk-0123456789abcdef0123456789abcdef";

struct Proxy {
    server: ServerHandle,
    client: ClientHandle,
}

impl Proxy {
    fn socks_addr(&self) -> SocketAddr {
        self.client.socks_addr()
    }

    async fn shutdown(self) {
        self.client.shutdown().await;
        self.server.shutdown().await;
    }
}

async fn start_proxy(psk: &str) -> Proxy {
    let server = server::start(ServerOptions {
        listen: "127.0.0.1:0".parse().expect("server addr"),
        psk: psk.to_owned(),
        allow_private: true,
        max_streams: 1024,
        idle_timeout: Duration::from_secs(30),
        prefer_ipv4: true,
        congestion: Congestion::default(),
    })
    .await
    .expect("start server");

    let client = client::start(ClientOptions {
        server: server.addr().to_string(),
        socks_listen: "127.0.0.1:0".parse().expect("socks addr"),
        bind: "127.0.0.1:0".parse().expect("bind addr"),
        psk: psk.to_owned(),
        max_streams: 1024,
        idle_timeout: Duration::from_secs(30),
        prefer_ipv4: true,
        connections: 1,
        congestion: Congestion::default(),
    })
    .await
    .expect("start client");

    Proxy { server, client }
}

async fn start_echo() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind echo server");
    let addr = listener.local_addr().expect("echo addr");
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (mut reader, mut writer) = tcp.split();
                let mut buffer = vec![0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(count) => {
                            if writer.write_all(&buffer[..count]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    (addr, task)
}

/// Perform a full SOCKS5 CONNECT and return the TCP stream plus the reply code.
async fn socks_connect(
    proxy: SocketAddr,
    host: &str,
    port: u16,
) -> std::io::Result<(TcpStream, u8)> {
    let mut tcp = TcpStream::connect(proxy)
        .await
        .map_err(|e| step("connect", e))?;
    // Production also treats this as best-effort; Windows can report EINVAL
    // here under a burst of parallel loopback connects even though the socket
    // is usable.
    let _ = tcp.set_nodelay(true);

    tcp.write_all(&[0x05, 0x01, 0x00])
        .await
        .map_err(|e| step("write greeting", e))?;
    let mut selection = [0u8; 2];
    tcp.read_exact(&mut selection)
        .await
        .map_err(|e| step("read method", e))?;
    assert_eq!(selection, [0x05, 0x00]);

    let mut request = vec![0x05, 0x01, 0x00];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(0x01);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(0x04);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            request.push(0x03);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    tcp.write_all(&request)
        .await
        .map_err(|e| step("write request", e))?;

    let mut reply = [0u8; 10];
    tcp.read_exact(&mut reply)
        .await
        .map_err(|e| step("read reply", e))?;
    Ok((tcp, reply[1]))
}

fn step(what: &str, error: std::io::Error) -> std::io::Error {
    std::io::Error::new(error.kind(), format!("{what}: {error}"))
}

async fn expect_echo(proxy: SocketAddr, host: &str, port: u16, payload: &[u8]) {
    let (mut tcp, reply) = socks_connect(proxy, host, port)
        .await
        .expect("SOCKS connect");
    assert_eq!(reply, 0x00, "SOCKS request failed for {host}:{port}");
    tcp.write_all(payload).await.expect("write payload");
    let mut echoed = vec![0u8; payload.len()];
    tcp.read_exact(&mut echoed).await.expect("read echo");
    assert_eq!(echoed, payload);
}

#[tokio::test]
async fn ipv4_target_roundtrip() {
    let (echo_addr, _echo) = start_echo().await;
    let proxy = start_proxy(PSK).await;
    expect_echo(
        proxy.socks_addr(),
        &echo_addr.ip().to_string(),
        echo_addr.port(),
        b"hello over quic",
    )
    .await;
    proxy.shutdown().await;
}

#[tokio::test]
async fn domain_target_is_resolved_by_the_server() {
    let (echo_addr, _echo) = start_echo().await;
    let proxy = start_proxy(PSK).await;
    // `localhost` is resolved on the server, not by the client.
    expect_echo(
        proxy.socks_addr(),
        "localhost",
        echo_addr.port(),
        b"dns on server",
    )
    .await;
    proxy.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_streams_share_one_quic_connection() {
    let (echo_addr, _echo) = start_echo().await;
    let proxy = start_proxy(PSK).await;
    let proxy_addr = proxy.socks_addr();

    let mut tasks = Vec::new();
    for index in 0..32u32 {
        let payload = format!("stream-{index}").into_bytes();
        tasks.push(tokio::spawn(async move {
            expect_echo(
                proxy_addr,
                &echo_addr.ip().to_string(),
                echo_addr.port(),
                &payload,
            )
            .await;
        }));
    }
    for task in tasks {
        task.await.expect("task");
    }

    assert_eq!(
        proxy.server.established_connections(),
        1,
        "all streams should multiplex over a single QUIC connection"
    );
    proxy.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_are_spread_over_the_connection_pool() {
    let (echo_addr, _echo) = start_echo().await;
    let server = server::start(ServerOptions {
        listen: "127.0.0.1:0".parse().expect("server addr"),
        psk: PSK.to_owned(),
        allow_private: true,
        max_streams: 64,
        idle_timeout: Duration::from_secs(30),
        prefer_ipv4: true,
        congestion: Congestion::default(),
    })
    .await
    .expect("start server");
    let client = client::start(ClientOptions {
        server: server.addr().to_string(),
        socks_listen: "127.0.0.1:0".parse().expect("socks addr"),
        bind: "127.0.0.1:0".parse().expect("bind addr"),
        psk: PSK.to_owned(),
        max_streams: 64,
        idle_timeout: Duration::from_secs(30),
        prefer_ipv4: true,
        connections: 2,
        congestion: Congestion::default(),
    })
    .await
    .expect("start client");

    for index in 0..4u32 {
        let payload = format!("pooled-{index}").into_bytes();
        expect_echo(
            client.socks_addr(),
            &echo_addr.ip().to_string(),
            echo_addr.port(),
            &payload,
        )
        .await;
    }

    assert_eq!(
        server.established_connections(),
        2,
        "streams should be spread over both pooled connections"
    );
    client.shutdown().await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_payload_is_relayed_intact() {
    let (echo_addr, _echo) = start_echo().await;
    let proxy = start_proxy(PSK).await;
    let (tcp, reply) = socks_connect(
        proxy.socks_addr(),
        &echo_addr.ip().to_string(),
        echo_addr.port(),
    )
    .await
    .expect("SOCKS connect");
    assert_eq!(reply, 0x00);

    let payload: Vec<u8> = (0..2 * 1024 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();
    let (mut reader, mut writer) = tcp.into_split();
    let writer_payload = payload.clone();
    let writer = tokio::spawn(async move {
        writer
            .write_all(&writer_payload)
            .await
            .expect("write payload");
        writer.shutdown().await.expect("finish payload");
    });

    let mut received = vec![0u8; payload.len()];
    timeout(Duration::from_secs(30), reader.read_exact(&mut received))
        .await
        .expect("large transfer timed out")
        .expect("read echo");
    writer.await.expect("writer task");
    assert_eq!(received, payload);
    proxy.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loss_tolerant_controller_relays_a_large_payload() {
    let (echo_addr, _echo) = start_echo().await;
    let server = server::start(ServerOptions {
        listen: "127.0.0.1:0".parse().expect("server addr"),
        psk: PSK.to_owned(),
        allow_private: true,
        max_streams: 1024,
        idle_timeout: Duration::from_secs(30),
        prefer_ipv4: true,
        congestion: Congestion::LossTolerant,
    })
    .await
    .expect("start server");
    let client = client::start(ClientOptions {
        server: server.addr().to_string(),
        socks_listen: "127.0.0.1:0".parse().expect("socks addr"),
        bind: "127.0.0.1:0".parse().expect("bind addr"),
        psk: PSK.to_owned(),
        max_streams: 1024,
        idle_timeout: Duration::from_secs(30),
        prefer_ipv4: true,
        connections: 1,
        congestion: Congestion::LossTolerant,
    })
    .await
    .expect("start client");

    let (tcp, reply) = socks_connect(
        client.socks_addr(),
        &echo_addr.ip().to_string(),
        echo_addr.port(),
    )
    .await
    .expect("SOCKS connect");
    assert_eq!(reply, 0x00);
    let payload: Vec<u8> = (0..1024 * 1024).map(|index| (index % 253) as u8).collect();
    let (mut reader, mut writer) = tcp.into_split();
    let outgoing = payload.clone();
    let writer_task = tokio::spawn(async move {
        writer.write_all(&outgoing).await.expect("write payload");
        writer.shutdown().await.expect("finish payload");
    });
    let mut received = vec![0u8; payload.len()];
    timeout(Duration::from_secs(30), reader.read_exact(&mut received))
        .await
        .expect("transfer timed out")
        .expect("read echo");
    writer_task.await.expect("writer task");
    assert_eq!(received, payload);

    client.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn wrong_psk_is_rejected() {
    let (echo_addr, _echo) = start_echo().await;
    let server = server::start(ServerOptions {
        listen: "127.0.0.1:0".parse().expect("server addr"),
        psk: "correct-psk-0123456789abcdef".to_owned(),
        allow_private: true,
        max_streams: 64,
        idle_timeout: Duration::from_secs(10),
        prefer_ipv4: true,
        congestion: Congestion::default(),
    })
    .await
    .expect("start server");
    let client = client::start(ClientOptions {
        server: server.addr().to_string(),
        socks_listen: "127.0.0.1:0".parse().expect("socks addr"),
        bind: "127.0.0.1:0".parse().expect("bind addr"),
        psk: "wrong-psk-0123456789abcdef".to_owned(),
        max_streams: 64,
        idle_timeout: Duration::from_secs(10),
        prefer_ipv4: true,
        connections: 1,
        congestion: Congestion::default(),
    })
    .await
    .expect("start client");

    let result = timeout(
        Duration::from_secs(20),
        socks_connect(
            client.socks_addr(),
            &echo_addr.ip().to_string(),
            echo_addr.port(),
        ),
    )
    .await;

    match result {
        Ok(Ok((_tcp, reply))) => assert_ne!(reply, 0x00, "wrong PSK must not succeed"),
        Ok(Err(_)) => {}
        Err(_) => panic!("wrong-PSK attempt hung"),
    }

    client.shutdown().await;
    server.shutdown().await;
}
