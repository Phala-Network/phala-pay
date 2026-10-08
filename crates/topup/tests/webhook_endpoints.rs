//! Webhook endpoints and events through the API (design D11, §11, §13, §16 PR 8), in process
//! against PostgreSQL. Delivery itself is tested in `outbox.rs`.

mod support;

use std::sync::Arc;

use anyhow::{Result, ensure};
use axum::body::to_bytes;
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use topup::api::{AppState, PublicOrigin, VerificationKey};
use topup_adapters::attestation::DstackAttestor;
use tower::ServiceExt;

use support::seed::{self, NewAccount};
use support::{TestDatabase, merchant_request};

struct Harness {
    app: axum::Router,
}

struct Answer {
    status: StatusCode,
    body: Value,
}

impl Harness {
    /// A service behind an `https` origin, as deployed: URLs are held to the scheme and port rules.
    fn new(pool: &PgPool) -> Result<Self> {
        let state = AppState {
            pool: pool.clone(),
            routes: Arc::new(topup::routes::RouteSet::new(Vec::new()).map_err(anyhow::Error::msg)?),
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(
                "admin/v1".to_owned(),
                "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=",
            )
            .map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse("https://api.test")?,
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
            client_reads: Arc::default(),
            rate_limits: Arc::default(),
            hint_limits: Arc::default(),
            transaction_hints: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        };
        Ok(Self {
            app: topup::api::router(state).0,
        })
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        key: &str,
    ) -> Result<Answer> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()?
            .unwrap_or_default();
        let response = self
            .app
            .clone()
            .oneshot(merchant_request(method, path, body, key))
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_048_576).await?;
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)?
        };
        Ok(Answer { status, body })
    }

    async fn get(&self, path: &str, key: &str) -> Result<Answer> {
        self.call(Method::GET, path, None, key).await
    }

    async fn post(&self, path: &str, body: &Value, key: &str) -> Result<Answer> {
        self.call(Method::POST, path, Some(body), key).await
    }

    async fn create(&self, key: &str, url: &str) -> Result<Answer> {
        self.post(
            "/v1/webhook_endpoints",
            &json!({ "url": url, "enabled_events": ["deposit.credited"] }),
            key,
        )
        .await
    }
}

/// A live account and a secret key of each mode.
async fn account(pool: &PgPool, name: &str) -> Result<(String, String, String)> {
    let account = seed::create_account(pool, &NewAccount::named(name)).await?;
    let live = seed::create_api_key(pool, account.id, true).await?;
    let test = seed::create_api_key(pool, account.id, false).await?;
    Ok((account.public_id, live, test))
}

fn id(answer: &Answer) -> String {
    answer.body["id"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn endpoints_are_created_listed_updated_and_deleted() -> Result<()> {
    let Some(context) = TestDatabase::create().await? else {
        return Ok(());
    };
    let harness = Harness::new(&context.app_pool)?;
    let (account, live, test) = account(&context.app_pool, "merchant").await?;

    // Live mode takes https on 443 only; test mode also http on 80.
    for url in [
        "http://merchant.example/hooks",
        "https://merchant.example:8443/hooks",
        "https://user:secret@merchant.example/hooks",
    ] {
        let refused = harness.create(&live, url).await?;
        ensure!(
            refused.status == StatusCode::BAD_REQUEST,
            "{url}: {}",
            refused.body
        );
        ensure!(refused.body["error"]["param"] == "url", "{}", refused.body);
    }
    let unknown = harness
        .post(
            "/v1/webhook_endpoints",
            &json!({ "url": "https://merchant.example/hooks", "enabled_events": ["deposit.*"] }),
            &live,
        )
        .await?;
    ensure!(unknown.status == StatusCode::BAD_REQUEST);
    ensure!(unknown.body["error"]["param"] == "enabled_events");
    let test_endpoint = harness
        .create(&test, "http://merchant.example/hooks")
        .await?;
    ensure!(
        test_endpoint.status == StatusCode::OK,
        "{}",
        test_endpoint.body
    );
    ensure!(test_endpoint.body["livemode"] == false);

    let created = harness
        .post(
            "/v1/webhook_endpoints",
            &json!({
                "url": "https://merchant.example/hooks",
                "enabled_events": ["deposit.credited", "refund.failed"],
                "description": "orders",
                "metadata": { "team": "payments" },
            }),
            &live,
        )
        .await?;
    ensure!(created.status == StatusCode::OK, "{}", created.body);
    let endpoint = id(&created);
    ensure!(endpoint.starts_with("we_"));
    ensure!(created.body["object"] == "webhook_endpoint" && created.body["livemode"] == true);
    ensure!(created.body["status"] == "enabled" && created.body["disabled_reason"].is_null());
    ensure!(created.body["enabled_events"] == json!(["deposit.credited", "refund.failed"]));
    ensure!(created.body["metadata"] == json!({ "team": "payments" }));
    ensure!(created.body.get("deleted").is_none());

    let listed = harness.get("/v1/webhook_endpoints", &live).await?;
    ensure!(
        listed.body["data"].as_array().map(Vec::len) == Some(1),
        "{}",
        listed.body
    );
    ensure!(
        harness
            .get(&format!("/v1/webhook_endpoints/{endpoint}"), &live)
            .await?
            .body
            == created.body
    );

    let updated = harness
        .post(
            &format!("/v1/webhook_endpoints/{endpoint}"),
            &json!({
                "url": "https://hooks.merchant.example/v2",
                "description": "",
                "metadata": { "team": "", "tier": "1" },
                "disabled": true,
            }),
            &live,
        )
        .await?;
    ensure!(updated.status == StatusCode::OK, "{}", updated.body);
    ensure!(updated.body["url"] == "https://hooks.merchant.example/v2");
    ensure!(updated.body["description"].is_null());
    ensure!(updated.body["metadata"] == json!({ "tier": "1" }));
    ensure!(updated.body["status"] == "disabled" && updated.body["disabled_reason"].is_null());

    let deleted = harness
        .call(
            Method::DELETE,
            &format!("/v1/webhook_endpoints/{endpoint}"),
            None,
            &live,
        )
        .await?;
    ensure!(
        deleted.body == json!({ "id": endpoint, "object": "webhook_endpoint", "deleted": true })
    );
    let gone = harness
        .get(&format!("/v1/webhook_endpoints/{endpoint}"), &live)
        .await?;
    ensure!(gone.status == StatusCode::NOT_FOUND);
    ensure!(harness.get("/v1/webhook_endpoints", &live).await?.body["data"] == json!([]));

    // The audit log: every change is an event naming the key that made it.
    let events = harness
        .get("/v1/events?type=webhook_endpoint.*", &live)
        .await?;
    ensure!(events.status == StatusCode::OK, "{}", events.body);
    let data = events.body["data"].as_array().cloned().unwrap_or_default();
    let types: Vec<&str> = data
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    ensure!(
        types
            == [
                "webhook_endpoint.deleted",
                "webhook_endpoint.updated",
                "webhook_endpoint.created"
            ],
        "{types:?}"
    );
    let key_id: String = sqlx::query_scalar(
        "SELECT 'key_' || replace(id::text, '-', '') FROM api_keys \
         WHERE key_hash = sha256($1::bytea)",
    )
    .bind(live.as_bytes())
    .fetch_one(&context.app_pool)
    .await?;
    for event in &data {
        ensure!(event["actor"] == key_id.as_str() && event["account"] == account.as_str());
        ensure!(event["livemode"] == true && event["data"]["object"]["id"] == endpoint.as_str());
    }
    let update = data.get(1).cloned().unwrap_or_default();
    ensure!(update["data"]["previous_attributes"]["url"] == "https://merchant.example/hooks");
    ensure!(update["data"]["previous_attributes"]["status"] == "enabled");
    ensure!(
        data.first()
            .is_some_and(|event| event["data"]["object"]["deleted"] == true)
    );
    let one = harness
        .get(
            &format!(
                "/v1/events/{}",
                data.first()
                    .map(|event| event["id"].clone())
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or_default()
            ),
            &live,
        )
        .await?;
    ensure!(one.status == StatusCode::OK && Some(&one.body) == data.first());

    // Pagination and the other mode's events.
    let page = harness
        .get("/v1/events?type=webhook_endpoint.*&limit=1", &live)
        .await?;
    ensure!(page.body["has_more"] == true);
    let after = page.body["data"][0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let next = harness
        .get(
            &format!("/v1/events?type=webhook_endpoint.*&limit=5&starting_after={after}"),
            &live,
        )
        .await?;
    ensure!(
        next.body["data"].as_array().map(Vec::len) == Some(2) && next.body["has_more"] == false
    );
    let test_events = harness.get("/v1/events", &test).await?;
    ensure!(
        test_events.body["data"].as_array().map(Vec::len) == Some(1),
        "{}",
        test_events.body
    );

    context.cleanup().await
}

#[tokio::test]
async fn a_mode_has_at_most_sixteen_endpoints() -> Result<()> {
    let Some(context) = TestDatabase::create().await? else {
        return Ok(());
    };
    let harness = Harness::new(&context.app_pool)?;
    let (_, live, test) = account(&context.app_pool, "merchant").await?;
    let mut first = String::new();
    for number in 0..16 {
        let created = harness
            .create(&live, &format!("https://merchant.example/hooks/{number}"))
            .await?;
        ensure!(created.status == StatusCode::OK, "{}", created.body);
        if number == 0 {
            first = id(&created);
        }
    }
    let over = harness
        .create(&live, "https://merchant.example/hooks/16")
        .await?;
    ensure!(over.status == StatusCode::BAD_REQUEST);
    ensure!(
        over.body["error"]["code"] == "webhook_endpoint_cap_exceeded",
        "{}",
        over.body
    );
    // The limit is per mode, and a deleted endpoint frees its place.
    ensure!(
        harness
            .create(&test, "https://merchant.example/hooks")
            .await?
            .status
            == StatusCode::OK
    );
    harness
        .call(
            Method::DELETE,
            &format!("/v1/webhook_endpoints/{first}"),
            None,
            &live,
        )
        .await?;
    ensure!(
        harness
            .create(&live, "https://merchant.example/hooks/16")
            .await?
            .status
            == StatusCode::OK
    );

    context.cleanup().await
}

/// Another account's, or the other mode's, endpoints and events answer like missing ones.
#[tokio::test]
async fn endpoints_and_events_are_not_found_across_accounts_and_modes() -> Result<()> {
    let Some(context) = TestDatabase::create().await? else {
        return Ok(());
    };
    let harness = Harness::new(&context.app_pool)?;
    let (_, own, own_test) = account(&context.app_pool, "own").await?;
    let (_, other, _) = account(&context.app_pool, "other").await?;
    let endpoint = id(&harness.create(&own, "https://own.example/hooks").await?);
    let other_endpoint = id(&harness
        .create(&other, "https://other.example/hooks")
        .await?);
    let events = harness.get("/v1/events", &own).await?;
    let event = events.body["data"][0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    ensure!(event.starts_with("evt_"));

    for key in [&other, &own_test] {
        let path = format!("/v1/webhook_endpoints/{endpoint}");
        ensure!(harness.get(&path, key).await?.status == StatusCode::NOT_FOUND);
        let update = harness
            .post(&path, &json!({ "disabled": true }), key)
            .await?;
        ensure!(update.status == StatusCode::NOT_FOUND);
        let delete = harness.call(Method::DELETE, &path, None, key).await?;
        ensure!(delete.status == StatusCode::NOT_FOUND);
        let test = harness
            .post(&format!("{path}/test"), &json!({}), key)
            .await?;
        ensure!(test.status == StatusCode::NOT_FOUND);
        ensure!(
            harness
                .get(&format!("/v1/events/{event}"), key)
                .await?
                .status
                == StatusCode::NOT_FOUND
        );
        let resend = harness
            .post(
                &format!("/v1/events/{event}/resend"),
                &json!({ "webhook_endpoint": endpoint }),
                key,
            )
            .await?;
        ensure!(resend.status == StatusCode::NOT_FOUND);
    }
    // Nor may an account resend its event to another account's endpoint.
    let foreign = harness
        .post(
            &format!("/v1/events/{event}/resend"),
            &json!({ "webhook_endpoint": other_endpoint }),
            &own,
        )
        .await?;
    ensure!(foreign.status == StatusCode::NOT_FOUND);
    ensure!(
        foreign.body["error"]["param"] == "webhook_endpoint",
        "{}",
        foreign.body
    );
    let listed = harness.get("/v1/webhook_endpoints", &own_test).await?;
    ensure!(listed.body["data"] == json!([]));
    let unchanged = harness
        .get(&format!("/v1/webhook_endpoints/{endpoint}"), &own)
        .await?;
    ensure!(unchanged.body["status"] == "enabled");

    context.cleanup().await
}

#[tokio::test]
async fn events_are_resent_and_endpoints_tested_through_the_api() -> Result<()> {
    let Some(context) = TestDatabase::create().await? else {
        return Ok(());
    };
    let harness = Harness::new(&context.app_pool)?;
    let (_, live, _) = account(&context.app_pool, "merchant").await?;
    let endpoint = id(&harness
        .create(&live, "https://merchant.example/hooks")
        .await?);
    let created = harness.get("/v1/events", &live).await?.body["data"][0].clone();
    ensure!(created["type"] == "webhook_endpoint.created" && created["pending_webhooks"] == 1);
    let event = created["id"].as_str().unwrap_or_default().to_owned();

    let tested = harness
        .post(
            &format!("/v1/webhook_endpoints/{endpoint}/test"),
            &json!({}),
            &live,
        )
        .await?;
    ensure!(tested.status == StatusCode::OK, "{}", tested.body);
    ensure!(tested.body["type"] == "webhook_endpoint.test" && tested.body["pending_webhooks"] == 1);
    ensure!(tested.body["data"]["object"]["id"] == endpoint.as_str());

    sqlx::query("UPDATE webhook_deliveries SET delivered_at = now()")
        .execute(&context.app_pool)
        .await?;
    let resent = harness
        .post(
            &format!("/v1/events/{event}/resend"),
            &json!({ "webhook_endpoint": endpoint }),
            &live,
        )
        .await?;
    ensure!(resent.status == StatusCode::OK, "{}", resent.body);
    ensure!(resent.body["id"] == event.as_str() && resent.body["pending_webhooks"] == 1);

    let missing = harness
        .post(
            "/v1/events/evt_00000000000000000000000000000000/resend",
            &json!({ "webhook_endpoint": endpoint }),
            &live,
        )
        .await?;
    ensure!(missing.status == StatusCode::NOT_FOUND);
    harness
        .post(
            &format!("/v1/webhook_endpoints/{endpoint}"),
            &json!({ "disabled": true }),
            &live,
        )
        .await?;
    let disabled = harness
        .post(
            &format!("/v1/events/{event}/resend"),
            &json!({ "webhook_endpoint": endpoint }),
            &live,
        )
        .await?;
    ensure!(disabled.status == StatusCode::BAD_REQUEST);
    ensure!(
        disabled.body["error"]["code"] == "webhook_endpoint_disabled",
        "{}",
        disabled.body
    );

    context.cleanup().await
}

/// The notice of a URL change goes to the former URL (`crates/topup/src/webhook_endpoints.rs`). Until
/// it is delivered it is pending, but not in the endpoint's backlog: a former URL the merchant
/// took down must not leave the endpoint looking unhealthy.
#[tokio::test]
async fn a_notice_to_a_former_url_is_not_in_the_endpoints_backlog() -> Result<()> {
    let Some(context) = TestDatabase::create().await? else {
        return Ok(());
    };
    let pool = &context.app_pool;
    let harness = Harness::new(pool)?;
    let (_, _, test) = account(pool, "merchant").await?;
    let created = harness.create(&test, "http://old.example/hooks").await?;
    ensure!(created.status == StatusCode::OK, "{}", created.body);
    let path = format!("/v1/webhook_endpoints/{}", id(&created));
    let before = harness.get(&path, &test).await?.body["pending_deliveries"].clone();

    let moved = harness
        .post(&path, &json!({ "url": "http://new.example/hooks" }), &test)
        .await?;
    ensure!(moved.status == StatusCode::OK, "{}", moved.body);
    let notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webhook_deliveries \
         WHERE url = 'http://old.example/hooks' AND delivered_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    ensure!(notices == 1);
    let after = harness.get(&path, &test).await?;
    ensure!(
        after.body["pending_deliveries"] == before,
        "{} vs {before}",
        after.body
    );
    Ok(())
}
