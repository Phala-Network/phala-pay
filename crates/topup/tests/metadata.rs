//! Stripe-style `metadata` on quotes, deposits, and refunds, through the API and PostgreSQL.

mod support;

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Method, StatusCode};
use chrono::Utc;
use ed25519_dalek::SigningKey;
use serde_json::{Map, Value, json};
use topup::api::{AppState, PublicOrigin, VerificationKey};
use topup::db::NewDeposit;
use topup::locks::QuoteProvider;
use topup::locks::pricing::ValidatedQuote;
use topup_adapters::attestation::DstackAttestor;
use topup_core::deposit::{DepositState, RejectReason};
use topup_core::identity::deposit_id;
use topup_core::money::{AtomicAmount, PRICE_SCALE, ScaledPrice};
use topup_core::route::RouteFile;
use tower::ServiceExt;
use uuid::Uuid;

use support::seed::{self, NewAccount};
use support::{
    TEST_ORIGIN, TestDatabase, merchant_request, merchant_request_with_key, public_key_base64,
};

struct FixedQuote;

#[async_trait]
impl QuoteProvider for FixedQuote {
    async fn quote(&self, _route: &RouteFile) -> Result<ValidatedQuote, Value> {
        let price = ScaledPrice::new(100_000_000, PRICE_SCALE).map_err(|_| json!("price"))?;
        Ok(ValidatedQuote {
            price,
            evidence: json!({ "mode": "spot" }),
        })
    }
}

/// The account's API client.
struct Merchant {
    app: axum::Router,
    api_key: String,
}

impl Merchant {
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<(StatusCode, Value)> {
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(body)?
        };
        let request = match idempotency_key {
            Some(key) => merchant_request_with_key(method, path, body, &self.api_key, key),
            None => merchant_request(method, path, body, &self.api_key),
        };
        let response = self.app.clone().oneshot(request).await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_048_576).await?;
        Ok((status, serde_json::from_slice(&bytes)?))
    }

    async fn ok(&self, method: Method, path: &str, body: &Value) -> Result<Value> {
        let (status, answer) = self.call(method.clone(), path, body, None).await?;
        ensure!(
            status == StatusCode::OK,
            "{method} {path}: {status} {answer}"
        );
        Ok(answer)
    }
}

fn quote_body(metadata: Option<Value>) -> Value {
    let mut body = json!({
        "client_reference_id": "team-42", "amount": 100, "currency": "usd", "chain_id": 1, "asset": "pha"
    });
    if let Some(metadata) = metadata {
        body["metadata"] = metadata;
    }
    body
}

/// The limits of https://docs.stripe.com/api/metadata, each answered with the parameter it
/// breaks; nothing is created.
fn invalid_metadata() -> Vec<(Value, String)> {
    let fifty_one: Map<String, Value> = (0..51).map(|n| (format!("k{n}"), json!("v"))).collect();
    let long_key = "k".repeat(41);
    vec![
        (Value::Object(fifty_one), "metadata".to_owned()),
        (
            json!({ long_key.clone(): "v" }),
            format!("metadata[{long_key}]"),
        ),
        (
            json!({ "order": "v".repeat(501) }),
            "metadata[order]".to_owned(),
        ),
        (json!({ "order": 6735 }), "metadata[order]".to_owned()),
        (json!({ "order": null }), "metadata[order]".to_owned()),
        (json!({ "a[b]": "v" }), "metadata[a[b]]".to_owned()),
        (json!(null), "metadata".to_owned()),
        (json!("not empty"), "metadata".to_owned()),
    ]
}

fn ensure_invalid(status: StatusCode, answer: &Value, param: &str) -> Result<()> {
    ensure!(status == StatusCode::BAD_REQUEST, "{status} {answer}");
    ensure!(
        answer["error"]["type"] == "invalid_request_error"
            && answer["error"]["code"] == "parameter_invalid"
            && answer["error"]["param"] == param,
        "{param}: {answer}"
    );
    Ok(())
}

#[tokio::test]
async fn metadata_is_set_merged_unset_and_copied_from_quote_to_deposit() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let account = seed::create_account(
            pool,
            &NewAccount {
                webhook_url: "https://product.test/webhooks".to_owned(),
                ..NewAccount::named("metadata")
            },
        )
        .await?;
        seed::set_treasury(pool, account.id, true, 1, seed::FIXTURE_TREASURY).await?;
        seed::accept_routes(pool, account.id, true, &[&test_route()]).await?;
        let route = test_route();
        let merchant = Merchant {
            app: router(pool, &route)?,
            api_key: seed::create_api_key(pool, account.id, true).await?,
        };

        for (metadata, param) in invalid_metadata() {
            let (status, answer) = merchant
                .call(
                    Method::POST,
                    "/v1/quotes",
                    &quote_body(Some(metadata)),
                    None,
                )
                .await?;
            ensure_invalid(status, &answer, &param)?;
        }
        let quotes: i64 = sqlx::query_scalar("SELECT count(*) FROM quotes")
            .fetch_one(pool)
            .await?;
        ensure!(quotes == 0);

        // Set on create; a key whose value is empty is not stored.
        let body = quote_body(Some(json!({ "order_id": "6735", "unset": "" })));
        let (status, quote) = merchant
            .call(Method::POST, "/v1/quotes", &body, Some("order-6735"))
            .await?;
        ensure!(status == StatusCode::OK, "{quote}");
        ensure!(
            quote["metadata"] == json!({ "order_id": "6735" }),
            "{quote}"
        );
        let quote_id = quote["id"].as_str().context("id")?.to_owned();
        let path = format!("/v1/quotes/{quote_id}");

        // An Idempotency-Key repeat must carry the same metadata.
        let (status, repeat) = merchant
            .call(Method::POST, "/v1/quotes", &body, Some("order-6735"))
            .await?;
        ensure!(status == StatusCode::OK && repeat["id"] == quote_id.as_str());
        ensure!(repeat["metadata"] == json!({ "order_id": "6735" }));
        // The repeat's secret replaced the first one.
        let client_secret = repeat["client_secret"]
            .as_str()
            .context("secret")?
            .to_owned();
        let other = quote_body(Some(json!({ "order_id": "6736" })));
        let (status, conflict) = merchant
            .call(Method::POST, "/v1/quotes", &other, Some("order-6735"))
            .await?;
        ensure!(
            status == StatusCode::BAD_REQUEST && conflict["error"]["type"] == "idempotency_error",
            "{conflict}"
        );

        // Updates merge: a value sets its key, `""` unsets it, other keys stay.
        let updated = merchant
            .ok(
                Method::POST,
                &path,
                &json!({ "metadata": { "cart": "9", "order_id": "" } }),
            )
            .await?;
        ensure!(updated["metadata"] == json!({ "cart": "9" }), "{updated}");
        ensure!(updated["id"] == quote_id.as_str() && updated["client_secret"].is_null());
        let unchanged = merchant.ok(Method::POST, &path, &json!({})).await?;
        ensure!(unchanged["metadata"] == json!({ "cart": "9" }));
        let unchanged = merchant
            .ok(Method::POST, &path, &json!({ "metadata": {} }))
            .await?;
        ensure!(unchanged["metadata"] == json!({ "cart": "9" }));
        let updated = merchant
            .ok(
                Method::POST,
                &path,
                &json!({ "metadata": { "order_id": "6735" } }),
            )
            .await?;
        let quoted = json!({ "cart": "9", "order_id": "6735" });
        ensure!(updated["metadata"] == quoted);
        let read = merchant.ok(Method::GET, &path, &Value::Null).await?;
        ensure!(read["metadata"] == quoted);

        for (metadata, param) in invalid_metadata() {
            let (status, answer) = merchant
                .call(Method::POST, &path, &json!({ "metadata": metadata }), None)
                .await?;
            ensure_invalid(status, &answer, &param)?;
        }
        // The 50-key limit applies to the merged metadata.
        let forty_nine: Map<String, Value> =
            (0..49).map(|n| (format!("k{n}"), json!("v"))).collect();
        let (status, answer) = merchant
            .call(
                Method::POST,
                &path,
                &json!({ "metadata": forty_nine }),
                None,
            )
            .await?;
        ensure_invalid(status, &answer, "metadata")?;
        let (status, answer) = merchant
            .call(Method::POST, &path, &json!({ "amount": 5 }), None)
            .await?;
        ensure!(
            status == StatusCode::BAD_REQUEST && answer["error"]["code"] == "parameter_unknown",
            "{answer}"
        );
        let read = merchant.ok(Method::GET, &path, &Value::Null).await?;
        ensure!(
            read["metadata"] == quoted,
            "a refused update changes nothing"
        );

        // The payer's view by client secret carries no metadata, as Stripe redacts it from
        // publishable-key reads.
        let public = merchant
            .app
            .clone()
            .oneshot(
                axum::http::Request::get(format!("{path}?client_secret={client_secret}"))
                    .body(Body::empty())?,
            )
            .await?;
        ensure!(public.status() == StatusCode::OK);
        let public: Value = serde_json::from_slice(&to_bytes(public.into_body(), 65_536).await?)?;
        ensure!(public.get("metadata").is_none(), "{public}");

        // The deposit to the quote's address starts with a copy of the quote's metadata.
        let address_id: Uuid = sqlx::query_scalar("SELECT id FROM addresses WHERE quote_id = $1")
            .bind(topup::ids::parse(topup::ids::QUOTE, &quote_id).context("quote id")?)
            .fetch_one(pool)
            .await?;
        let tx_hash = B256::repeat_byte(0x81);
        ensure!(
            topup::db::insert_deposit(
                pool,
                &NewDeposit {
                    chain_id: route.chain.chain_id,
                    tx_hash,
                    log_index: 0,
                    receipt_log_index: 0,
                    tx_from: Address::ZERO,
                    tx_nonce: 0,
                    is_final: true,
                    block_number: 10,
                    block_hash: B256::repeat_byte(0x82),
                    block_time: Utc::now(),
                    address_id,
                    route: Some(route.route.clone()),
                    route_version: Some(route.version),
                    asset_contract: Address::repeat_byte(0x83),
                    from_address: Address::repeat_byte(0x84),
                    amount_atomic: AtomicAmount::new(U256::from(1_000_u64)),
                    state: DepositState::Rejected,
                    reason: Some(RejectReason::OutOfBounds),
                    next_attempt_at: Utc::now(),
                },
            )
            .await?
        );
        let deposit = topup::ids::format(
            topup::ids::DEPOSIT,
            deposit_id(route.chain.chain_id, tx_hash, 0),
        );
        let deposit_path = format!("/v1/deposits/{deposit}");
        let read = merchant
            .ok(Method::GET, &deposit_path, &Value::Null)
            .await?;
        ensure!(read["metadata"] == quoted, "{read}");
        let listed = merchant
            .ok(Method::GET, "/v1/deposits", &Value::Null)
            .await?;
        ensure!(listed["data"][0]["metadata"] == quoted);

        // From then on the two are independent.
        let updated = merchant
            .ok(
                Method::POST,
                &deposit_path,
                &json!({ "metadata": { "fulfilled": "yes", "cart": "" } }),
            )
            .await?;
        let deposited = json!({ "fulfilled": "yes", "order_id": "6735" });
        ensure!(updated["metadata"] == deposited, "{updated}");
        let read = merchant.ok(Method::GET, &path, &Value::Null).await?;
        ensure!(read["metadata"] == quoted);
        let cleared = merchant
            .ok(Method::POST, &path, &json!({ "metadata": "" }))
            .await?;
        ensure!(cleared["metadata"] == json!({}));
        let read = merchant
            .ok(
                Method::GET,
                &format!("{deposit_path}?expand[]=quote"),
                &Value::Null,
            )
            .await?;
        ensure!(read["metadata"] == deposited && read["quote"]["metadata"] == json!({}));

        // Refunds: set on create, part of an Idempotency-Key repeat, updated like the others.
        let refund_body = |metadata: Value| {
            json!({
                "deposit": deposit,
                "destination_address": format!("{:#x}", Address::repeat_byte(0x85)),
                "amount_atomic": "10",
                "metadata": metadata,
            })
        };
        for (metadata, param) in invalid_metadata() {
            let (status, answer) = merchant
                .call(Method::POST, "/v1/refunds", &refund_body(metadata), None)
                .await?;
            ensure_invalid(status, &answer, &param)?;
        }
        let body = refund_body(json!({ "reason": "duplicate" }));
        let (status, refund) = merchant
            .call(Method::POST, "/v1/refunds", &body, Some("refund-1"))
            .await?;
        ensure!(status == StatusCode::OK, "{refund}");
        ensure!(refund["metadata"] == json!({ "reason": "duplicate" }));
        let refund_id = refund["id"].as_str().context("refund id")?.to_owned();
        let (status, repeat) = merchant
            .call(Method::POST, "/v1/refunds", &body, Some("refund-1"))
            .await?;
        ensure!(status == StatusCode::OK && repeat["id"] == refund_id.as_str());
        let (status, conflict) = merchant
            .call(
                Method::POST,
                "/v1/refunds",
                &refund_body(json!({ "reason": "other" })),
                Some("refund-1"),
            )
            .await?;
        ensure!(
            status == StatusCode::BAD_REQUEST && conflict["error"]["type"] == "idempotency_error",
            "{conflict}"
        );
        let refund_path = format!("/v1/refunds/{refund_id}");
        let updated = merchant
            .ok(
                Method::POST,
                &refund_path,
                &json!({ "metadata": { "ticket": "T-1" } }),
            )
            .await?;
        ensure!(updated["metadata"] == json!({ "reason": "duplicate", "ticket": "T-1" }));
        let cleared = merchant
            .ok(Method::POST, &refund_path, &json!({ "metadata": "" }))
            .await?;
        ensure!(cleared["metadata"] == json!({}));

        // An update of an object that does not exist is `404`.
        for missing in [
            format!(
                "/v1/quotes/{}",
                topup::ids::format(topup::ids::QUOTE, Uuid::new_v4())
            ),
            format!(
                "/v1/deposits/{}",
                topup::ids::format(topup::ids::DEPOSIT, Uuid::new_v4())
            ),
            format!(
                "/v1/refunds/{}",
                topup::ids::format(topup::ids::REFUND, Uuid::new_v4())
            ),
            "/v1/quotes/not-an-id".to_owned(),
        ] {
            let (status, answer) = merchant
                .call(
                    Method::POST,
                    &missing,
                    &json!({ "metadata": { "a": "b" } }),
                    None,
                )
                .await?;
            ensure!(
                status == StatusCode::NOT_FOUND && answer["error"]["code"] == "resource_missing",
                "{missing}: {answer}"
            );
        }

        // The database holds the same rules as the API.
        for table in ["quotes", "deposits", "refunds"] {
            for invalid in [
                json!({ "order": 6735 }),
                json!({ "order": "" }),
                json!({ "a[b]": "v" }),
                json!({ "k".repeat(41): "v" }),
                json!({ "order": "v".repeat(501) }),
                json!([]),
                Value::Object((0..51).map(|n| (format!("k{n}"), json!("v"))).collect()),
            ] {
                let error = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE {table} SET metadata = $1"
                )))
                .bind(&invalid)
                .execute(pool)
                .await
                .err()
                .context("invalid metadata was stored")?;
                let code = error
                    .as_database_error()
                    .and_then(|error| error.code().map(|code| code.into_owned()));
                ensure!(
                    code.as_deref() == Some("23514"),
                    "{table} {invalid}: {error}"
                );
            }
        }
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

fn router(pool: &sqlx::PgPool, route: &RouteFile) -> Result<axum::Router> {
    let admin_key = SigningKey::from_bytes(&[89; 32]);
    Ok(topup::api::router(AppState {
        pool: pool.clone(),
        routes: Arc::new(
            topup::routes::RouteSet::new(vec![route.clone()]).map_err(anyhow::Error::msg)?,
        ),
        maintenance_keys: Vec::new(),
        admin_key: VerificationKey::from_base64(
            "admin/v1".to_owned(),
            &public_key_base64(&admin_key),
        )
        .map_err(anyhow::Error::msg)?,
        public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
        attestor: Arc::new(DstackAttestor::new()),
        rate_lock_quotes: Arc::new(FixedQuote),
        client_reads: Arc::default(),
        rate_limits: Arc::default(),
        screening: Arc::new(support::ClearScreener),
        contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
    })
    .0)
}

fn test_route() -> RouteFile {
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))
            .expect("route fixture");
    route.asset.decimals = 2;
    route.asset.quote_amount_decimals = 2;
    route.merchant.quote_spread_bps =
        topup_core::route::Bounded::at(topup_core::money::Bps::new(0).expect("zero bps"));
    route.merchant.min_deposit_atomic =
        topup_core::route::Bounded::at(AtomicAmount::new(U256::from(1_u64)));
    route.merchant.max_deposit_atomic =
        topup_core::route::Bounded::at(AtomicAmount::new(U256::from(1_000_000_u64)));
    route.merchant.min_amount = topup_core::route::Bounded::at(1);
    route
}
