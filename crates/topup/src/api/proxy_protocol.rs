//! A private ingress listener requiring PROXY protocol v2 before HTTP.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use axum::serve::{Listener, ListenerExt, TapIo};
use ppp::v2::{Addresses, Header, PROTOCOL_PREFIX, Protocol};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use super::deadlines::DeadlineStream;

const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING_HEADERS: usize = 256;

/// Accepts only PROXY v2 connections from a trusted ingress hop.
///
/// This listener must never be published directly: the header is an assertion by the peer,
/// without authentication. Forwarded HTTP headers are never used for connection information.
pub struct ProxyProtocolListener {
    listener: TcpListener,
    pending: JoinSet<io::Result<(DeadlineStream, SocketAddr)>>,
}

impl ProxyProtocolListener {
    /// Wraps a private bound socket. Header reads run concurrently with a five-second deadline.
    pub fn new(listener: TcpListener) -> Self {
        Self {
            listener,
            pending: JoinSet::new(),
        }
    }

    /// Preserves the asserted source address as Axum's `ConnectInfo<SocketAddr>`.
    pub fn with_connect_info(self) -> TapIo<Self, fn(&mut DeadlineStream)> {
        self.tap_io(|_| {})
    }
}

impl Listener for ProxyProtocolListener {
    type Io = DeadlineStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                accepted = Listener::accept(&mut self.listener), if self.pending.len() < MAX_PENDING_HEADERS => {
                    let (mut stream, peer) = accepted;
                    self.pending.spawn(async move {
                        let address = tokio::time::timeout(HEADER_TIMEOUT, read_header(&mut stream, peer))
                            .await
                            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "PROXY header deadline exceeded"))??;
                        Ok((DeadlineStream::new(stream), address))
                    });
                }
                completed = self.pending.join_next(), if !self.pending.is_empty() => {
                    match completed {
                        Some(Ok(Ok(connection))) => return connection,
                        // Never log the header, parser error, peer, or asserted client address.
                        _ => tracing::warn!("PROXY connection rejected: missing, invalid, or timed-out header"),
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

async fn read_header(stream: &mut TcpStream, peer: SocketAddr) -> io::Result<SocketAddr> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid PROXY v2 header");
    let mut fixed = [0; 16];
    stream.read_exact(&mut fixed).await?;
    if !fixed.starts_with(PROTOCOL_PREFIX) || !matches!(fixed[12], 0x20 | 0x21) {
        return Err(invalid());
    }
    let length = usize::from(u16::from_be_bytes([fixed[14], fixed[15]]));
    // v2's u16 length bounds allocation to 64 KiB per pending connection. Read exactly the
    // declared header so an HTTP request coalesced in the same TCP packet stays in the stream.
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await?;
    // HAProxy's specification requires ignoring the family/protocol and payload for LOCAL.
    if fixed[12] == 0x20 {
        return Ok(peer);
    }
    let mut bytes = fixed.to_vec();
    bytes.extend(payload);
    let header = Header::try_from(bytes.as_slice()).map_err(|_| invalid())?;
    match (header.protocol, header.addresses) {
        (_, Addresses::Unspecified) => Ok(peer),
        (Protocol::Stream, Addresses::IPv4(address)) => Ok(SocketAddr::new(
            address.source_address.into(),
            address.source_port,
        )),
        (Protocol::Stream, Addresses::IPv6(address)) => Ok(SocketAddr::new(
            address.source_address.into(),
            address.source_port,
        )),
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ConnectInfo;
    use tokio::io::AsyncWriteExt;
    use tokio_util::sync::CancellationToken;

    fn header(command: u8, family: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = PROTOCOL_PREFIX.to_vec();
        bytes.extend([command, family]);
        bytes.extend(u16::try_from(payload.len()).unwrap().to_be_bytes());
        bytes.extend(payload);
        bytes
    }

    fn ipv4_header() -> Vec<u8> {
        // Includes an ignored TLV, so parsing must consume more than just the addresses.
        header(
            0x21,
            0x11,
            &[192, 0, 2, 10, 127, 0, 0, 1, 0x30, 0x39, 0, 80, 4, 0, 1, 42],
        )
    }

    async fn serve() -> (
        SocketAddr,
        CancellationToken,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|ConnectInfo(address): ConnectInfo<SocketAddr>| async move {
                address.to_string()
            }),
        );
        let served = axum::serve(
            ProxyProtocolListener::new(listener).with_connect_info(),
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(stop.clone().cancelled_owned());
        (
            address,
            stop,
            tokio::spawn(std::future::IntoFuture::into_future(served)),
        )
    }

    async fn request(address: SocketAddr, header: &[u8]) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let mut bytes = header.to_vec();
        bytes.extend(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nX-Real-IP: 203.0.113.99\r\nX-Forwarded-For: 203.0.113.99\r\n\r\n");
        stream.write_all(&bytes).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_string(&mut response))
            .await
            .unwrap()
            .unwrap();
        response
    }

    #[tokio::test]
    async fn ipv4_ipv6_and_http_bytes_reach_axum_without_trusting_forwarding_headers() {
        let (address, stop, served) = serve().await;
        let response = request(address, &ipv4_header()).await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.ends_with("192.0.2.10:12345"));
        let mut payload = "2001:db8::1"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets()
            .to_vec();
        payload.extend(std::net::Ipv6Addr::LOCALHOST.octets());
        payload.extend([0x30, 0x39, 0, 80]);
        let response = request(address, &header(0x21, 0x21, &payload)).await;
        assert!(response.ends_with("[2001:db8::1]:12345"));
        stop.cancel();
        served.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn local_and_unspecified_use_peer_and_consume_exactly_the_declared_header() {
        for bytes in [
            header(0x20, 0xff, b"ignored payload"),
            header(0x21, 0, b"ignored payload"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (mut stream, peer) = listener.accept().await.unwrap();
            client.write_all(&bytes).await.unwrap();
            client.write_all(b"HTTP").await.unwrap();
            assert_eq!(read_header(&mut stream, peer).await.unwrap(), peer);
            let mut remaining = [0; 4];
            stream.read_exact(&mut remaining).await.unwrap();
            assert_eq!(&remaining, b"HTTP");
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn missing_and_invalid_headers_are_closed_without_logging_client_data() {
        let (address, stop, served) = serve().await;
        for bytes in [
            b"GET / HTTP/1.1\r\nHost: 203.0.113.99\r\n\r\n".to_vec(),
            b"PROXY TCP4 192.0.2.10 127.0.0.1 12345 80\r\n".to_vec(),
            header(0x22, 0x11, &[0; 12]),
            header(0x21, 0x11, &[0; 4]),
            header(0x21, 0x12, &[0; 12]),
        ] {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
            let mut buf = [0; 1];
            // Closing a socket with unread data can yield either EOF or a reset.
            let result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
                .await
                .unwrap();
            assert!(matches!(result, Ok(0) | Err(_)));
        }
        assert!(logs_contain("PROXY connection rejected"));
        assert!(!logs_contain("192.0.2.10"));
        assert!(!logs_contain("203.0.113.99"));
        stop.cancel();
        served.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn slow_fragmented_header_does_not_block_accept_and_shutdown_cancels_pending_reads() {
        let (address, stop, served) = serve().await;
        let mut stalled = TcpStream::connect(address).await.unwrap();
        stalled.write_all(&ipv4_header()[..8]).await.unwrap();
        assert!(
            request(address, &ipv4_header())
                .await
                .ends_with("192.0.2.10:12345")
        );
        let bytes = ipv4_header();
        stalled.write_all(&bytes[8..]).await.unwrap();
        stalled
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stalled.read_to_string(&mut response).await.unwrap();
        assert!(response.ends_with("192.0.2.10:12345"));
        let mut pending = TcpStream::connect(address).await.unwrap();
        pending.write_all(b"\r\n").await.unwrap();
        tokio::task::yield_now().await;
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(1), served)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let mut buf = [0; 1];
        let result = tokio::time::timeout(Duration::from_secs(1), pending.read(&mut buf))
            .await
            .unwrap();
        assert!(matches!(result, Ok(0) | Err(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_header_expires_after_five_seconds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        peer.write_all(b"\r\n").await.unwrap();
        let mut listener = ProxyProtocolListener::new(listener);
        let accepted = tokio::spawn(async move { listener.accept().await });
        // Let accept spawn its per-connection read before advancing the clock.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(HEADER_TIMEOUT).await;
        let mut buf = [0; 1];
        assert!(matches!(peer.read(&mut buf).await, Ok(0) | Err(_)));
        assert!(
            !accepted.is_finished(),
            "a rejected header must not reach HTTP"
        );
        accepted.abort();
        assert!(matches!(accepted.await, Err(error) if error.is_cancelled()));
    }
}
