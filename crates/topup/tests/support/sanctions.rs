//! SLS contract fixture. Official hostnames resolve only to this owned loopback listener.
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct PublicationState {
    pub xml: String,
    pub sha: String,
    pub malformed: bool,
}

async fn headers(request: Request, next: Next) -> Response {
    if !request.headers().contains_key("user-agent") {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.method() == axum::http::Method::POST
        && !request.headers().contains_key("content-length")
    {
        return StatusCode::LENGTH_REQUIRED.into_response();
    }
    next.run(request).await
}

async fn preview(State(state): State<Arc<Mutex<PublicationState>>>) -> Json<serde_json::Value> {
    let state = state.lock().unwrap();
    if state.malformed {
        return Json(serde_json::json!({"changed_contract":true}));
    }
    Json(serde_json::json!([
        {"fileName":"SDN.CSV","hashCodes":null,"lastUpdated":"2026-10-05T00:00:00Z"},
        {"fileName":"SDN.XML","hashCodes":serde_json::json!({"SHA-256":state.sha}).to_string(),"lastUpdated":"2026-10-05T00:00:00Z"},
        {"fileName":"SDN.PDF","hashCodes":null,"lastUpdated":"2026-10-05T00:00:00Z"}
    ]))
}
async fn download_xml(State(state): State<Arc<Mutex<PublicationState>>>) -> String {
    state.lock().unwrap().xml.clone()
}

pub struct Fixture {
    pub state: Arc<Mutex<PublicationState>>,
    pub origin: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    pub async fn new(xml: impl Into<String>) -> Result<Self> {
        Self::with_routes(xml, Router::new()).await
    }
    pub async fn with_routes(xml: impl Into<String>, extra: Router) -> Result<Self> {
        let xml = xml.into();
        let state = Arc::new(Mutex::new(PublicationState {
            sha: hex::encode(Sha256::digest(xml.as_bytes())),
            xml,
            malformed: false,
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let origin = format!("http://{address}");
        let download = format!(
            "http://wc2h-sls-prod-public-published.s3.us-gov-west-1.amazonaws.com:{}/sdn.xml",
            address.port()
        );
        let sls = Router::new()
            .route("/api/PublicationPreview/SdnList", post(preview))
            .route(
                "/api/download/SDN.XML",
                get(move || async move { (StatusCode::FOUND, [("location", download)]) }),
            )
            .route("/sdn.xml", get(download_xml))
            .layer(middleware::from_fn(headers))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, sls.merge(extra)).await.unwrap();
        });
        Ok(Self {
            state,
            origin,
            task,
        })
    }
    pub fn publish(&self, xml: String) {
        let mut state = self.state.lock().unwrap();
        state.sha = hex::encode(Sha256::digest(xml.as_bytes()));
        state.xml = xml;
        state.malformed = false;
    }
}
