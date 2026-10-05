//! Finite budgets for requests and connections, including incomplete HTTP headers.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::serve::{Listener, ListenerExt, TapIo};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Instant, Sleep};

pub(super) const BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
// A hard lifetime also bounds trickled headers (and HTTP/2 streams). Clients reconnect after
// this finite keep-alive budget; activity cannot extend it indefinitely.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) async fn request_deadline(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            crate::observability::metrics::request_deadline_exceeded(method.as_str());
            super::error::ApiError::request_deadline_exceeded().into_response()
        }
    }
}

/// API listener with a read-idle deadline and a hard connection/header deadline.
pub struct DeadlineListener(TcpListener);

impl DeadlineListener {
    /// Wraps the application's bound listener without changing its peer addresses.
    pub fn new(listener: TcpListener) -> Self {
        Self(listener)
    }

    /// Uses Axum's `TapIo` connection-info adapter to preserve `ConnectInfo<SocketAddr>`.
    pub fn with_connect_info(self) -> TapIo<Self, fn(&mut DeadlineStream)> {
        self.tap_io(|_| {})
    }
}

impl Listener for DeadlineListener {
    type Io = DeadlineStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (stream, address) = Listener::accept(&mut self.0).await;
        (DeadlineStream::new(stream), address)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

/// Transport returned by [`DeadlineListener`].
pub struct DeadlineStream {
    stream: TcpStream,
    idle: Pin<Box<Sleep>>,
    lifetime: Pin<Box<Sleep>>,
}

impl DeadlineStream {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            idle: Box::pin(tokio::time::sleep(IDLE_TIMEOUT)),
            lifetime: Box::pin(tokio::time::sleep(CONNECTION_TIMEOUT)),
        }
    }

    fn expired(&mut self, cx: &mut Context<'_>, read: bool) -> bool {
        self.lifetime.as_mut().poll(cx).is_ready()
            || (read && self.idle.as_mut().poll(cx).is_ready())
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "API connection deadline exceeded")
}

impl AsyncRead for DeadlineStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.expired(cx, true) {
            return Poll::Ready(Err(timed_out()));
        }
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.idle.as_mut().reset(Instant::now() + IDLE_TIMEOUT);
        }
        result
    }
}

impl AsyncWrite for DeadlineStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.expired(cx, false) {
            return Poll::Ready(Err(timed_out()));
        }
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.expired(cx, false) {
            return Poll::Ready(Err(timed_out()));
        }
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    #[tokio::test(start_paused = true)]
    async fn request_deadline_cancels_a_stalled_handler() {
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::post(|| async {
                    std::future::pending::<()>().await;
                    axum::http::StatusCode::OK
                }),
            )
            .layer(axum::middleware::from_fn(request_deadline));
        let response = app
            .oneshot(Request::post("/").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "2");
        let body = axum::body::to_bytes(response.into_body(), 1_048_576)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "unavailable");
        assert_eq!(
            body["error"]["message"],
            "the request did not complete within its deadline; retry"
        );
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let metrics = crate::observability::metrics::render(&pool).unwrap();
        assert!(metrics.contains("topup_api_request_deadline_exceeded_total{method=\"POST\"} 1"));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_and_trickled_headers_have_finite_transport_budgets() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = DeadlineStream::new(socket);
        let mut buf = [0; 1];
        let error = stream.read(&mut buf).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        // A byte every five seconds keeps the idle deadline alive, but cannot extend the
        // connection lifetime: an incomplete header cannot hold a socket forever.
        let (socket, _) = {
            let mut second = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let accepted = listener.accept().await.unwrap();
            std::mem::swap(&mut peer, &mut second);
            accepted
        };
        let mut stream = DeadlineStream::new(socket);
        for _ in 0..11 {
            peer.write_all(b"X").await.unwrap();
            assert_eq!(stream.read(&mut buf).await.unwrap(), 1);
            tokio::time::advance(Duration::from_secs(5)).await;
        }
        tokio::time::advance(Duration::from_secs(5)).await;
        peer.write_all(b"X").await.unwrap();
        assert_eq!(
            stream.read(&mut buf).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
