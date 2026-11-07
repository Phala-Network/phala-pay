//! An Info-only boundary for containers that must not hold the dstack socket.

use axum::Router;
use axum::error_handling::HandleErrorLayer;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use topup_adapters::attestation::DstackAttestor;
use tower::{ServiceBuilder, limit::GlobalConcurrencyLimitLayer, load_shed::LoadShedLayer};

async fn info(State(attestor): State<DstackAttestor>) -> Response {
    match attestor.guest_info().await {
        Ok(info) => (
            [(header::CACHE_CONTROL, "public, max-age=60")],
            axum::Json(info),
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CACHE_CONTROL, "no-store")],
        )
            .into_response(),
    }
}

fn router(attestor: DstackAttestor) -> Router {
    Router::new()
        .route("/Info", axum::routing::get(info))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_| async {
                    StatusCode::SERVICE_UNAVAILABLE
                }))
                .layer(LoadShedLayer::new())
                .layer(GlobalConcurrencyLimitLayer::new(32)),
        )
        .with_state(attestor)
}

pub(super) async fn serve(bind: std::net::SocketAddr) -> anyhow::Result<std::process::ExitCode> {
    use anyhow::Context as _;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .context("failed to bind Info listener")?;
    axum::serve(
        topup::api::DeadlineListener::new(listener),
        router(DstackAttestor::new()),
    )
    .await
    .context("Info listener failed")?;
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn only_info_is_available_and_never_arbitrary_guest_rpc() {
        let app = router(DstackAttestor::with_endpoint("http://127.0.0.1:1"));
        for (method, path, expected) in [
            ("POST", "/Info", StatusCode::METHOD_NOT_ALLOWED),
            ("GET", "/GetKey", StatusCode::NOT_FOUND),
            ("POST", "/GetKey", StatusCode::NOT_FOUND),
            ("POST", "/GetQuote", StatusCode::NOT_FOUND),
            ("POST", "/GetTlsKey", StatusCode::NOT_FOUND),
            ("GET", "/Info", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }
}
