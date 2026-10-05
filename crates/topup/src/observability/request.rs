use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::audit::RequestRef;

/// Stripe's request id header (<https://docs.stripe.com/api/request_ids>).
static REQUEST_ID: HeaderName = HeaderName::from_static("request-id");

/// Gives every API request an id, `req_` and 32 hex digits, returned as `Request-Id`, recorded on
/// the events the request causes with its `Idempotency-Key` (a [`RequestRef`] extension), and
/// runs the request in a route-labelled span.
pub async fn request_context(mut request: Request, next: Next) -> Response {
    let request_id = format!("req_{}", Uuid::new_v4().simple());
    // An invalid key is refused by the idempotency layer; it names no request here.
    let idempotency_key = crate::api::idempotency_key_of(request.headers());
    request.extensions_mut().insert(RequestRef {
        id: request_id.clone(),
        idempotency_key,
    });
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched")
        .to_owned();
    let method = request.method().clone();
    let span = tracing::info_span!(
        "api.request",
        request_id = %request_id,
        route = %route,
        method = %method,
    );
    let start = std::time::Instant::now();
    let mut response = next.run(request).instrument(span).await;
    super::metrics::observe_http(
        &route,
        method.as_str(),
        response.status().as_u16(),
        start.elapsed(),
    );
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert(REQUEST_ID.clone(), value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router, body::Body, error_handling::HandleErrorLayer, http::StatusCode, middleware,
        routing::get,
    };
    use tokio_util::sync::CancellationToken;
    use tower::{
        ServiceBuilder, ServiceExt, limit::GlobalConcurrencyLimitLayer, load_shed::LoadShedLayer,
    };

    #[tokio::test]
    async fn records_load_shed_auth_and_unmatched_requests_without_raw_labels() {
        let entered = CancellationToken::new();
        let release = CancellationToken::new();
        let handler_entered = entered.clone();
        let handler_release = release.clone();
        let app = Router::new()
            .route(
                "/metrics-test/{id}",
                get(move || {
                    let entered = handler_entered.clone();
                    let release = handler_release.clone();
                    async move {
                        entered.cancel();
                        release.cancelled().await;
                        StatusCode::OK
                    }
                }),
            )
            .route(
                "/metrics-auth/{id}",
                get(|| async { StatusCode::OK }).route_layer(middleware::from_fn(
                    |_: Request, _: Next| async { StatusCode::UNAUTHORIZED },
                )),
            )
            .layer(
                ServiceBuilder::new()
                    .layer(HandleErrorLayer::new(|_| async {
                        StatusCode::SERVICE_UNAVAILABLE
                    }))
                    .layer(LoadShedLayer::new())
                    .layer(GlobalConcurrencyLimitLayer::new(1)),
            )
            .layer(middleware::from_fn(request_context));
        let request = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = app
            .clone()
            .oneshot(request(
                "/metrics-auth/customer-secret?client_secret=private",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let pending = tokio::spawn(
            app.clone()
                .oneshot(request("/metrics-test/customer-secret")),
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.cancelled())
            .await
            .unwrap();
        let response = app
            .clone()
            .oneshot(request("/metrics-test/another-secret"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.headers().contains_key("request-id"));
        release.cancel();
        assert_eq!(pending.await.unwrap().unwrap().status(), StatusCode::OK);
        for i in 0..50 {
            let response = app
                .clone()
                .oneshot(request(&format!(
                    "/unknown-secret-{i}?client_secret=private"
                )))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let text = super::super::metrics::render(&pool).unwrap();
        assert!(text.contains("route=\"/metrics-test/{id}\",status_class=\"5xx\""));
        assert!(text.contains("route=\"/metrics-auth/{id}\",status_class=\"4xx\""));
        assert!(text.contains("route=\"unmatched\""));
        assert!(!text.contains("customer-secret"));
        assert!(!text.contains("another-secret"));
        assert!(!text.contains("unknown-secret"));
        assert!(!text.contains("client_secret"));
        assert!(!text.contains("private"));
    }
}
