//! Prevent storage of tenant data and credentials, including responses rejected before handlers.

use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

pub(super) async fn no_store(request: Request, next: Next) -> Response {
    let sensitive = request.uri().path() == "/v1"
        || request.uri().path().starts_with("/v1/")
        || request.headers().contains_key(header::AUTHORIZATION)
        || request.headers().contains_key("signature");
    let request_is_public_info = request.uri().path() == "/v1/attestation/info"
        && matches!(
            *request.method(),
            axum::http::Method::GET | axum::http::Method::HEAD
        )
        && !request.headers().contains_key(header::AUTHORIZATION)
        && !request.headers().contains_key("signature");
    let mut response = next.run(request).await;
    if sensitive && !(request_is_public_info && response.status().is_success()) {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
