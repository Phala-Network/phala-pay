//! The API's conformance with Stripe's conventions (docs/design/multi-tenant.md, "API
//! conformance"), in process against PostgreSQL: event snapshots with their request and
//! `previous_attributes`, delivery health, statuses with `doc_url`, `Request-Id`, `Retry-After`,
//! and list parameters.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context, Result, ensure};
use axum::body::to_bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use chrono::{Duration, Utc};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use sqlx::PgPool;
use topup::api::{AppState, PublicOrigin, RateLimits, VerificationKey};
use topup::db::{self, NewDeposit};
use topup_adapters::attestation::DstackAttestor;
use topup_core::deposit::DepositState;
use topup_core::money::AtomicAmount;
use topup_core::route::RouteFile;
use tower::ServiceExt;
use uuid::Uuid;

use support::seed::{self, NewAccount, NewAddress, NewCustomer};
use support::{
    ManualClock, TEST_ORIGIN, TestDatabase, merchant_request, public_key_base64, signed_request,
};

const ADMIN_KID: &str = "admin/v1";
const ROUTE: &str = "phala-cloud-ethereum-pha-usd";

struct Harness {
    app: axum::Router,
    admin_key: SigningKey,
    /// Admin signatures are single-use, so each admin request is signed at a distinct second.
    created: AtomicI64,
    docs: topup::api::ApiDocs,
    /// The rate limiter's clock, which moves only when a test advances it.
    clock: ManualClock,
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Harness {
    fn new(pool: &PgPool, limits: RateLimits) -> Result<Self> {
        let admin_key = SigningKey::from_bytes(&[81; 32]);
        let clock = ManualClock::new();
        let route: RouteFile =
            serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
        let state = AppState {
            pool: pool.clone(),
            routes: Arc::new(
                topup::routes::RouteSet::new(vec![route]).map_err(anyhow::Error::msg)?,
            ),
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(
                ADMIN_KID.to_owned(),
                &public_key_base64(&admin_key),
            )
            .map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
            client_reads: Arc::default(),
            rate_limits: Arc::new(clock.rate_limiter(limits)),
            hint_limits: Arc::default(),
            transaction_hints: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            sanctions_rescreen: Arc::default(),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        };
        let (app, docs) = topup::api::router(state);
        Ok(Self {
            app,
            docs,
            admin_key,
            created: AtomicI64::new(Utc::now().timestamp()),
            clock,
        })
    }

    async fn admin_get(&self, path: &str) -> Result<Answer> {
        let created = self.created.fetch_sub(1, Ordering::Relaxed);
        let request = signed_request(
            Method::GET,
            path,
            Vec::new(),
            ADMIN_KID,
            &self.admin_key,
            created,
        );
        answer(self.app.clone().oneshot(request).await?).await
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        key: &str,
        idempotency_key: Option<&str>,
    ) -> Result<Answer> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()?
            .unwrap_or_default();
        let mut request = merchant_request(method, path, body, key);
        if let Some(idempotency_key) = idempotency_key {
            request
                .headers_mut()
                .insert("idempotency-key", idempotency_key.parse()?);
        }
        answer(self.app.clone().oneshot(request).await?).await
    }

    async fn get(&self, path: &str, key: &str) -> Result<Answer> {
        self.call(Method::GET, path, None, key, None).await
    }

    async fn post(&self, path: &str, body: &Value, key: &str) -> Result<Answer> {
        self.call(Method::POST, path, Some(body), key, None).await
    }
}

async fn answer(response: axum::response::Response) -> Result<Answer> {
    let status = response.status();
    let headers = response.headers().clone();
    ensure!(
        headers
            .get("cache-control")
            .is_some_and(|value| value == "no-store")
    );
    let bytes = to_bytes(response.into_body(), 1_048_576).await?;
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok(Answer {
        status,
        headers,
        body,
    })
}

/// The response's `Request-Id`, checked to be `req_` and 32 hex digits.
fn request_id(answer: &Answer) -> Result<String> {
    let id = answer
        .headers
        .get("request-id")
        .context("Request-Id")?
        .to_str()?
        .to_owned();
    ensure!(
        id.len() == 36
            && id.starts_with("req_")
            && id[4..].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{id}"
    );
    ensure!(!answer.headers.contains_key("x-request-id"));
    Ok(id)
}

/// A live account and its live key.
async fn live_account(pool: &PgPool, name: &str) -> Result<(Uuid, String)> {
    let account = seed::create_account(pool, &NewAccount::named(name)).await?;
    Ok((
        account.id,
        seed::create_api_key(pool, account.id, true).await?,
    ))
}

/// The one event of `event_type` the key's list returns.
async fn only_event(harness: &Harness, key: &str, event_type: &str) -> Result<Value> {
    let list = harness
        .get(&format!("/v1/events?type={event_type}"), key)
        .await?;
    ensure!(list.status == StatusCode::OK, "{}", list.body);
    let data = list.body["data"].as_array().context("data")?;
    ensure!(data.len() == 1, "{event_type}: {}", list.body);
    Ok(data[0].clone())
}

/// Events are rendered when they happen and never change: a later change of the object leaves
/// them as they were, `*.updated` events carry what changed, and an event caused by a request
/// names the request and its `Idempotency-Key`.
#[tokio::test]
async fn events_are_snapshots_with_their_request_and_previous_attributes() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (_, key) = live_account(pool, "snapshots").await?;

        let created = harness
            .call(
                Method::POST,
                "/v1/webhook_endpoints",
                Some(&json!({
                    "url": "https://merchant.example/a",
                    "enabled_events": ["deposit.credited"],
                    "metadata": {"team": "payments"},
                })),
                &key,
                Some("endpoint-1"),
            )
            .await?;
        ensure!(created.status == StatusCode::OK, "{}", created.body);
        let request = request_id(&created)?;
        let endpoint = created.body["id"].as_str().context("id")?.to_owned();
        let updated = harness
            .post(
                &format!("/v1/webhook_endpoints/{endpoint}"),
                &json!({"url": "https://merchant.example/b", "metadata": {"team": "", "on_call": "ops"}}),
                &key,
            )
            .await?;
        ensure!(updated.status == StatusCode::OK, "{}", updated.body);

        // The creation's event still shows the endpoint as it was created, and its request.
        let creation = only_event(&harness, &key, "webhook_endpoint.created").await?;
        ensure!(creation["data"]["object"]["url"] == "https://merchant.example/a");
        ensure!(creation["data"].get("previous_attributes").is_none());
        ensure!(creation["request"] == json!({"id": request, "idempotency_key": "endpoint-1"}));
        let event = creation["id"].as_str().context("event id")?;
        let read = harness.get(&format!("/v1/events/{event}"), &key).await?;
        ensure!(read.body == creation, "{}", read.body);

        let update = only_event(&harness, &key, "webhook_endpoint.updated").await?;
        ensure!(update["data"]["object"]["url"] == "https://merchant.example/b");
        ensure!(update["request"]["id"] == request_id(&updated)?.as_str());
        ensure!(update["request"]["idempotency_key"].is_null());
        // Only what changed, with its former value; a changed `metadata` only its changed keys.
        let previous = &update["data"]["previous_attributes"];
        ensure!(previous["url"] == "https://merchant.example/a", "{previous}");
        ensure!(
            previous["metadata"] == json!({"team": "payments", "on_call": null}),
            "{previous}"
        );
        ensure!(previous.get("created").is_none(), "{previous}");

        // The account's pause, and a key's roll, carry what they changed.
        let paused = harness
            .post("/v1/account/pause", &json!({"scopes": ["quotes"]}), &key)
            .await?;
        ensure!(paused.status == StatusCode::OK, "{}", paused.body);
        let account = only_event(&harness, &key, "account.updated").await?;
        ensure!(account["data"]["object"]["paused_scopes"] == json!(["quotes"]));
        ensure!(account["data"]["previous_attributes"] == json!({"paused_scopes": []}));

        let keys = harness.get("/v1/api_keys", &key).await?;
        let key_id = keys.body["data"][0]["id"].as_str().context("key id")?.to_owned();
        let rolled = harness
            .post(
                &format!("/v1/api_keys/{key_id}/roll"),
                &json!({"expires_in": 3600}),
                &key,
            )
            .await?;
        ensure!(rolled.status == StatusCode::OK, "{}", rolled.body);
        let roll = only_event(&harness, &key, "api_key.updated").await?;
        ensure!(roll["data"]["object"]["status"] == "expiring");
        ensure!(
            roll["data"]["previous_attributes"] == json!({"status": "active", "expires_at": null}),
            "{roll}"
        );

        // A worker's event names no request.
        let system: Option<Option<String>> = sqlx::query_scalar(
            "SELECT request_id FROM events WHERE actor = 'system' LIMIT 1",
        )
        .fetch_optional(pool)
        .await?;
        ensure!(system.flatten().is_none());

        // The service can neither change nor delete an event.
        for statement in ["UPDATE events SET data = '{}'", "DELETE FROM events"] {
            let refused = sqlx::query(statement).execute(pool).await;
            ensure!(
                refused
                    .as_ref()
                    .err()
                    .and_then(|error| error.as_database_error())
                    .and_then(|error| error.code())
                    .as_deref()
                    == Some("42501"),
                "{statement}: {refused:?}"
            );
        }
        let unchanged = harness.get(&format!("/v1/events/{event}"), &key).await?;
        ensure!(unchanged.body == creation);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// A refund's life is its events: `refund.created`, and `refund.updated` with what changed; the
/// creation's event keeps the refund as it was created.
#[tokio::test]
async fn refunds_are_created_and_updated_as_events() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (account, key) = live_account(pool, "refunds").await?;
        let deposit = seed_deposit(pool, account, Utc::now()).await?;
        // Refunds are recorded directly: destination screening is unavailable in this harness.
        let refund = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO refunds (id, account_id, livemode, chain_id, deposit_id, amount_atomic, \
             destination_address, status) \
             VALUES ($1, $2, true, 1, $3, 10, '0x4343434343434343434343434343434343434343', 'pending')",
        )
        .bind(refund)
        .bind(account)
        .bind(deposit)
        .execute(pool)
        .await?;
        let refund = topup::ids::format(topup::ids::REFUND, refund);

        let updated = harness
            .post(
                &format!("/v1/refunds/{refund}"),
                &json!({"metadata": {"ticket": "311"}}),
                &key,
            )
            .await?;
        ensure!(updated.status == StatusCode::OK, "{}", updated.body);
        let canceled = harness
            .post(&format!("/v1/refunds/{refund}/cancel"), &json!({}), &key)
            .await?;
        ensure!(canceled.status == StatusCode::OK, "{}", canceled.body);
        ensure!(canceled.body["status"] == "canceled");

        let list = harness.get("/v1/events?types[]=refund.*", &key).await?;
        let events = list.body["data"].as_array().context("data")?;
        ensure!(events.len() == 2, "{}", list.body);
        // Newest first: the cancel, then the metadata update.
        ensure!(
            events[0]["data"]["previous_attributes"] == json!({"status": "pending"}),
            "{}",
            list.body
        );
        ensure!(events[0]["data"]["object"]["status"] == "canceled");
        ensure!(
            events[1]["data"]["previous_attributes"] == json!({"metadata": {"ticket": null}}),
            "{}",
            list.body
        );
        ensure!(events[1]["data"]["object"]["status"] == "pending");
        let actions: Vec<String> =
            sqlx::query_scalar("SELECT action FROM audit WHERE account_id = $1 ORDER BY created_at")
                .bind(account)
                .fetch_all(pool)
                .await?;
        ensure!(actions.contains(&"refund.cancel".to_owned()), "{actions:?}");

        // A second cancel changes nothing and announces nothing.
        let again = harness
            .post(&format!("/v1/refunds/{refund}/cancel"), &json!({}), &key)
            .await?;
        ensure!(again.status == StatusCode::OK);
        let list = harness.get("/v1/events?types[]=refund.updated", &key).await?;
        ensure!(list.body["data"].as_array().map(Vec::len) == Some(2));
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// An endpoint that has not received its events shows it, `GET /v1/events` lists what it missed,
/// and the operator's daily report lists it once it has failed longer than the threshold.
#[tokio::test]
async fn delivery_health_is_visible_to_the_merchant_and_the_operator() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (account, key) = live_account(pool, "failing").await?;
        let created = harness
            .post(
                "/v1/webhook_endpoints",
                &json!({"url": "https://merchant.example/hooks", "enabled_events": ["*"]}),
                &key,
            )
            .await?;
        ensure!(created.status == StatusCode::OK, "{}", created.body);
        let endpoint = created.body["id"].as_str().context("id")?.to_owned();
        let endpoint_id =
            topup::ids::parse(topup::ids::WEBHOOK_ENDPOINT, &endpoint).context("we_ id")?;
        // It is told of its own creation first; nothing was attempted yet.
        ensure!(created.body["pending_deliveries"] == 1, "{}", created.body);
        ensure!(created.body["oldest_pending_at"].is_i64());
        ensure!(created.body["last_attempt"].is_null());

        // Its deliveries have failed for two days; the last attempt got a 503.
        sqlx::query("UPDATE events SET created = now() - interval '2 days' WHERE account_id = $1")
            .bind(account)
            .execute(&database.owner_pool)
            .await?;
        sqlx::query(
            "UPDATE webhook_endpoints SET last_attempt_at = now(), last_attempt_status = 503 \
             WHERE id = $1",
        )
        .bind(endpoint_id)
        .execute(pool)
        .await?;
        let read = harness
            .get(&format!("/v1/webhook_endpoints/{endpoint}"), &key)
            .await?;
        ensure!(read.body["pending_deliveries"] == 1, "{}", read.body);
        ensure!(
            read.body["last_attempt"]["status_code"] == 503,
            "{}",
            read.body
        );
        ensure!(read.body["last_attempt"]["at"].is_i64());
        let listed = harness.get("/v1/webhook_endpoints", &key).await?;
        ensure!(listed.body["data"][0]["pending_deliveries"] == 1);

        let missed = harness
            .get("/v1/events?delivery_success=false", &key)
            .await?;
        let missed = missed.body["data"].as_array().context("data")?.clone();
        ensure!(missed.len() == 1 && missed[0]["type"] == "webhook_endpoint.created");
        let delivered = harness
            .get("/v1/events?delivery_success=true", &key)
            .await?;
        ensure!(delivered.body["data"] == json!([]), "{}", delivered.body);
        let invalid = harness
            .get("/v1/events?delivery_success=maybe", &key)
            .await?;
        ensure!(invalid.status == StatusCode::BAD_REQUEST);
        ensure!(invalid.body["error"]["param"] == "delivery_success");

        let report = harness.admin_get("/v1/admin/reports/daily").await?;
        ensure!(report.status == StatusCode::OK, "{}", report.body);
        ensure!(report.body["failing_for_hours"] == 24);
        let failing = &report.body["failing_webhook_endpoints"];
        ensure!(failing.as_array().map(Vec::len) == Some(1), "{failing}");
        ensure!(failing[0]["id"] == endpoint.as_str() && failing[0]["last_attempt_status"] == 503);
        ensure!(failing[0]["pending_deliveries"] == 1 && failing[0]["livemode"] == true);
        let longer = harness
            .admin_get("/v1/admin/reports/daily?failing_for_hours=72")
            .await?;
        ensure!(longer.body["failing_webhook_endpoints"] == json!([]));
        let invalid = harness
            .admin_get("/v1/admin/reports/daily?failing_for_hours=0")
            .await?;
        ensure!(invalid.status == StatusCode::BAD_REQUEST);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Business-state failures are `400` with a `doc_url`; every response names its request; a `429`
/// says when to retry.
#[tokio::test]
async fn errors_carry_stripe_statuses_request_ids_and_retry_after() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(
            pool,
            RateLimits {
                live: 1,
                test: 25,
                test_platform: 500,
            },
        )?;
        let (_, key) = live_account(pool, "errors").await?;
        let keys = harness.get("/v1/api_keys", &key).await?;
        ensure!(keys.status == StatusCode::OK);
        request_id(&keys)?;
        let own = keys.body["data"][0]["id"]
            .as_str()
            .context("key id")?
            .to_owned();

        // The account's only key cannot be revoked: a 400, documented at its `doc_url`.
        // A second later the account's one request per second is available again.
        harness.clock.advance(std::time::Duration::from_secs(1));
        let last = harness
            .call(
                Method::DELETE,
                &format!("/v1/api_keys/{own}"),
                None,
                &key,
                None,
            )
            .await?;
        ensure!(last.status == StatusCode::BAD_REQUEST, "{}", last.body);
        ensure!(last.body["error"]["code"] == "last_api_key");
        ensure!(
            last.body["error"]["doc_url"]
                == "https://phala-network.github.io/phala-pay/#section/Errors/last_api_key"
        );
        request_id(&last)?;

        // Over the account's rate limit: retry after a second.
        let limited = harness.get("/v1/account", &key).await?;
        ensure!(
            limited.status == StatusCode::TOO_MANY_REQUESTS,
            "{}",
            limited.body
        );
        ensure!(limited.body["error"]["code"] == "rate_limit");
        ensure!(limited.headers["retry-after"] == "1");
        request_id(&limited)?;

        // Failures before authentication and unknown paths name their request too.
        let unauthenticated = harness.get("/v1/account", "ppay_sk_live_unknown").await?;
        ensure!(unauthenticated.status == StatusCode::UNAUTHORIZED);
        request_id(&unauthenticated)?;
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Every list takes `limit`, `starting_after`, and `ending_before`, and a list with a `created`
/// filter takes all four bounds.
#[tokio::test]
async fn lists_take_stripes_pagination_and_created_bounds() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (account, key) = live_account(pool, "lists").await?;

        // API keys: the seeded one and two more, newest first.
        for name in ["second", "third"] {
            let created = harness
                .post("/v1/api_keys", &json!({"name": name}), &key)
                .await?;
            ensure!(created.status == StatusCode::OK, "{}", created.body);
        }
        let all = harness.get("/v1/api_keys", &key).await?;
        let ids: Vec<String> = all.body["data"]
            .as_array()
            .context("data")?
            .iter()
            .filter_map(|key| key["id"].as_str().map(str::to_owned))
            .collect();
        ensure!(ids.len() == 3, "{}", all.body);
        let first = harness.get("/v1/api_keys?limit=2", &key).await?;
        ensure!(
            first.body["has_more"] == true
                && first.body["data"].as_array().map(Vec::len) == Some(2)
        );
        let next = harness
            .get(&format!("/v1/api_keys?starting_after={}", ids[1]), &key)
            .await?;
        ensure!(next.body["has_more"] == false && next.body["data"][0]["id"] == ids[2].as_str());
        let before = harness
            .get(
                &format!("/v1/api_keys?limit=1&ending_before={}", ids[2]),
                &key,
            )
            .await?;
        ensure!(before.body["has_more"] == true && before.body["data"][0]["id"] == ids[1].as_str());
        let unknown = harness
            .get(
                &format!(
                    "/v1/api_keys?starting_after=key_{}",
                    Uuid::new_v4().simple()
                ),
                &key,
            )
            .await?;
        ensure!(unknown.status == StatusCode::BAD_REQUEST);
        ensure!(unknown.body["error"]["param"] == "starting_after");
        let both = harness
            .get(
                &format!(
                    "/v1/api_keys?starting_after={}&ending_before={}",
                    ids[0], ids[2]
                ),
                &key,
            )
            .await?;
        ensure!(both.status == StatusCode::BAD_REQUEST);

        // Treasuries: two active ones and a pending change.
        let treasury = alloy_primitives::Address::repeat_byte(0x71);
        seed::set_treasury(pool, account, true, 1, treasury).await?;
        seed::set_treasury(pool, account, true, 10, treasury).await?;
        seed::schedule_treasury(
            pool,
            account,
            true,
            1,
            alloy_primitives::Address::repeat_byte(0x72),
            Utc::now() + Duration::hours(48),
        )
        .await?;
        let page = harness.get("/v1/treasuries?limit=2", &key).await?;
        ensure!(page.status == StatusCode::OK, "{}", page.body);
        ensure!(page.body["has_more"] == true);
        let last = page.body["data"][1]["id"].as_str().context("trs_ id")?;
        let rest = harness
            .get(&format!("/v1/treasuries?starting_after={last}"), &key)
            .await?;
        ensure!(
            rest.body["has_more"] == false && rest.body["data"].as_array().map(Vec::len) == Some(1)
        );
        let back = harness
            .get(
                &format!(
                    "/v1/treasuries?ending_before={}",
                    rest.body["data"][0]["id"].as_str().context("trs_ id")?
                ),
                &key,
            )
            .await?;
        ensure!(back.body["data"] == page.body["data"], "{}", back.body);

        // Deposits: `created[gt|gte|lt|lte]`.
        let created = Utc::now() - Duration::minutes(10);
        seed_deposit(pool, account, created).await?;
        let at = created.timestamp();
        for (query, expected) in [
            (format!("created[gte]={at}"), 1),
            (format!("created[gt]={at}"), 0),
            (format!("created[lte]={at}"), 1),
            (format!("created[lt]={at}"), 0),
            (format!("created[gt]={}&created[lt]={}", at - 1, at + 1), 1),
        ] {
            let listed = harness.get(&format!("/v1/deposits?{query}"), &key).await?;
            ensure!(listed.status == StatusCode::OK, "{query}: {}", listed.body);
            ensure!(
                listed.body["data"].as_array().map(Vec::len) == Some(expected),
                "{query}: {}",
                listed.body
            );
        }
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// A credited, final live deposit of a new customer of `account`, created at `created`; returns
/// its id.
async fn seed_deposit(
    pool: &PgPool,
    account: Uuid,
    created: chrono::DateTime<Utc>,
) -> Result<Uuid> {
    let customer = seed::create_customer(
        pool,
        &NewCustomer {
            id: Uuid::new_v4(),
            account_id: account,
            livemode: true,
            client_reference_id: format!("customer-{}", Uuid::new_v4().simple()),
            paused_scopes: Vec::new(),
        },
    )
    .await?;
    let unique = alloy_primitives::keccak256(Uuid::new_v4().as_bytes());
    let address = seed::insert_address(
        pool,
        &NewAddress {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            chain_id: 1,
            route: ROUTE.to_owned(),
            salt: unique,
            address: alloy_primitives::Address::from_word(unique),
        },
    )
    .await?;
    let deposit = NewDeposit {
        chain_id: 1,
        tx_hash: alloy_primitives::keccak256(unique),
        log_index: 0,
        receipt_log_index: 0,
        tx_from: alloy_primitives::Address::ZERO,
        tx_nonce: 0,
        is_final: true,
        block_number: 100,
        block_hash: unique,
        block_time: created,
        address_id: address.id,
        route: None,
        route_version: None,
        asset_contract: alloy_primitives::Address::repeat_byte(0x42),
        from_address: alloy_primitives::Address::repeat_byte(0x43),
        amount_atomic: AtomicAmount::new(alloy_primitives::U256::from(1_000_u64)),
        state: DepositState::Detected,
        reason: None,
        next_attempt_at: Utc::now() + Duration::hours(1),
    };
    let id = topup_core::identity::deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
    ensure!(db::insert_deposit(pool, &deposit).await?);
    sqlx::query("UPDATE deposits SET state = 'credited', created_at = $2 WHERE id = $1")
        .bind(id)
        .bind(created)
        .execute(pool)
        .await?;
    Ok(id)
}

/// Shared documented errors match responses produced by authentication, rate admission and
/// restore middleware, including optional 503 retry delays and tenant cache protection.
#[tokio::test]
async fn shared_error_and_cache_contracts_match_http_responses() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits { live: 1, test: 25, test_platform: 500 })?;
        let (account, key) = live_account(pool, "contracts").await?;
        for document in [&harness.docs.merchant, &harness.docs.admin] {
            for (_, path) in document["paths"].as_object().context("paths")? {
                for (method, operation) in path.as_object().context("operations")? {
                    if !["get", "post", "delete", "patch", "put"].contains(&method.as_str()) { continue; }
                    if matches!(operation["operationId"].as_str(), Some("submit_quote_transaction" | "submit_deposit_address_transaction")) {
                        ensure!(operation["responses"].as_object().is_some_and(|responses| responses.len()==1 && responses.contains_key("202")));
                        continue;
                    }
                    let unavailable = &operation["responses"]["503"];
                    ensure!(unavailable["content"]["application/json"]["schema"]["$ref"] == "#/components/schemas/ErrorResponse");
                    ensure!(unavailable["headers"]["Retry-After"]["required"] == false);
                    ensure!(unavailable["headers"]["Retry-After"]["schema"]["minimum"] == 1);
                    if document == &harness.docs.merchant {
                        ensure!(operation["responses"]["403"]["content"]["application/json"]["schema"]["$ref"] == "#/components/schemas/ErrorResponse");
                        ensure!(operation["responses"]["429"]["headers"]["Retry-After"]["required"] == true);
                    }
                }
            }
        }
        let ok = harness.get("/v1/account", &key).await?;
        ensure!(ok.status == StatusCode::OK);
        ensure!(ok.headers["cache-control"] == "no-store");
        let limited = harness.get("/v1/account", &key).await?;
        ensure!(limited.status == StatusCode::TOO_MANY_REQUESTS);
        ensure!(limited.headers["retry-after"] == "1");
        ensure!(limited.headers["cache-control"] == "no-store");
        let invalid = harness.get("/v1/account", "ppay_sk_live_invalid").await?;
        ensure!(invalid.status == StatusCode::UNAUTHORIZED);
        ensure!(invalid.headers["cache-control"] == "no-store");
        sqlx::query("UPDATE accounts SET charges_enabled=false WHERE id=$1").bind(account).execute(pool).await?;
        let forbidden = harness.get("/v1/account", &key).await?;
        ensure!(forbidden.status == StatusCode::FORBIDDEN);
        ensure!(forbidden.body["error"]["code"] == "testmode_charges_only");
        ensure!(forbidden.headers["cache-control"] == "no-store");
        sqlx::query("UPDATE accounts SET charges_enabled=true WHERE id=$1").bind(account).execute(pool).await?;
        sqlx::query("INSERT INTO restores(id,detected_by,timeline_id) VALUES($1,'restore_check',1)")
            .bind(Uuid::new_v4()).execute(pool).await?;
        let restoring = harness.get("/v1/account", &key).await?;
        ensure!(restoring.status == StatusCode::SERVICE_UNAVAILABLE);
        ensure!(restoring.body["error"]["code"] == "service_restoring");
        ensure!(restoring.headers["retry-after"] == "300");
        ensure!(restoring.headers["cache-control"] == "no-store");
        // A handler's temporary unavailability is allowed to omit Retry-After.
        let admin = harness.admin_get(&format!("/v1/admin/attestation?account=acct_{}&livemode=true&nonce=01",account.simple())).await?;
        ensure!(admin.status == StatusCode::SERVICE_UNAVAILABLE, "{}", admin.body);
        ensure!(!admin.headers.contains_key("retry-after"));
        ensure!(admin.headers["cache-control"] == "no-store");
        Ok(())
        })
    })
    .await
}
