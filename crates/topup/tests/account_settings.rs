//! The account's own settings through the API: its payment settings (docs/design/payment-settings.md),
//! which accept nothing until configured, bound the merchant's terms, and feed `GET /v1/config`;
//! and the merchant's own `quotes` pause, which never lifts the operator's.

mod support;

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use axum::Router;
use axum::body::to_bytes;
use axum::http::{Method, StatusCode};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use topup::api::{AppState, PublicOrigin, VerificationKey};
use topup_adapters::attestation::DstackAttestor;
use topup_core::route::RouteFile;
use tower::ServiceExt;

use support::seed::{self, NewAccount};
use support::{TEST_ORIGIN, merchant_request, public_key_base64, signed_request, with_database};

const TEST_CHAIN: u64 = 11_155_111;

#[tokio::test]
async fn a_new_account_accepts_nothing_until_it_configures_its_payment_settings() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let (status, settings) = fixture
                .request(Method::GET, "/v1/payment_settings", Value::Null)
                .await?;
            ensure!(status == StatusCode::OK, "{settings}");
            ensure!(settings["status"] == "unconfigured" && settings["chains"] == json!([]));
            ensure!(settings["available"][0]["status"] == "not_configured", "{settings}");
            ensure!(settings["available"][0]["assets"][0]["accepted"] == false);
            ensure!(fixture.config_assets().await?.is_empty());
            ensure!(fixture.quote_error().await? == "asset_not_accepted");
            let (status, body) = fixture
                .post(
                    "/v1/deposit_addresses",
                    json!({"client_reference_id": "team-42"}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == "asset_not_accepted", "{body}");

            // Accepting the asset offers it with the merchant's terms, within the route's bounds.
            let (status, settings) = fixture
                .post(
                    "/v1/payment_settings",
                    json!({"chains": [{"chain_id": 1, "confirmations": "12",
                                       "assets": [{"asset": "pha", "quote_spread_bps": 100}]}]}),
                )
                .await?;
            ensure!(status == StatusCode::OK, "{settings}");
            ensure!(settings["status"] == "configured", "{settings}");
            ensure!(
                settings["chains"]
                    == json!([{"chain_id": 1, "confirmations": "12",
                               "assets": [{"asset": "pha", "quote_ttl_seconds": null,
                                           "quote_spread_bps": 100, "quote_tolerance_bps": null,
                                           "min_amount": null, "min_deposit_atomic": null,
                                           "max_deposit_atomic": null, "min_refund_atomic": null}]}]),
                "{settings}"
            );
            ensure!(settings["available"][0]["status"] == "active");
            let assets = fixture.config_assets().await?;
            ensure!(assets.len() == 1, "{assets:?}");
            ensure!(assets[0]["confirmations"] == "12" && assets[0]["typical_credit_seconds"] == 150);
            ensure!(assets[0]["quote_spread_bps"] == 100 && assets[0]["quote_ttl_seconds"] == 900);
            ensure!(fixture.quote_error().await? == "price_unavailable");

            // Each change is one revision and one `payment_settings.updated` in the key's mode, with
            // what changed; a repeat writes nothing.
            let revision = settings["revision"].clone();
            let (status, repeated) = fixture
                .post(
                    "/v1/payment_settings",
                    json!({"chains": [{"chain_id": 1, "confirmations": "12",
                                       "assets": [{"asset": "pha", "quote_spread_bps": 100}]}]}),
                )
                .await?;
            ensure!(status == StatusCode::OK && repeated["revision"] == revision);
            let (status, changed) = fixture
                .post(
                    "/v1/payment_settings",
                    json!({"chains": [{"chain_id": 1, "assets": [{"asset": "pha"}]}]}),
                )
                .await?;
            ensure!(status == StatusCode::OK && changed["revision"] != revision);
            let events: Vec<(bool, Value)> = sqlx::query_as(
                "SELECT livemode, data FROM events \
                 WHERE account_id = $1 AND type = 'payment_settings.updated' ORDER BY created, id",
            )
            .bind(fixture.account.id)
            .fetch_all(pool)
            .await?;
            ensure!(events.len() == 2 && events.iter().all(|(livemode, _)| *livemode));
            ensure!(events[0].1["previous_attributes"]["status"] == "unconfigured");
            ensure!(events[1].1["previous_attributes"]["chains"][0]["confirmations"] == "12");
            ensure!(fixture.config_assets().await?[0]["confirmations"] == "2");

            // `[]` accepts nothing again.
            let (status, cleared) = fixture
                .post("/v1/payment_settings", json!({"chains": []}))
                .await?;
            ensure!(status == StatusCode::OK && cleared["status"] == "configured");
            ensure!(fixture.config_assets().await?.is_empty());
            ensure!(fixture.quote_error().await? == "asset_not_accepted");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn payment_settings_are_validated_against_the_catalog_and_its_bounds() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let asset = |extra: Value| {
                let mut asset = json!({"asset": "pha"});
                if let (Some(asset), Some(extra)) = (asset.as_object_mut(), extra.as_object()) {
                    asset.extend(extra.clone());
                }
                json!({"chains": [{"chain_id": 1, "assets": [asset]}]})
            };
            for (request, param, message) in [
                (
                    json!({"chains": [{"chain_id": TEST_CHAIN, "assets": [{"asset": "pha"}]}]}),
                    "chains[0][chain_id]",
                    "not a chain of the key's mode",
                ),
                (
                    json!({"chains": [{"chain_id": 1, "assets": [{"asset": "usdc"}]}]}),
                    "chains[0][assets][0][asset]",
                    "routed: pha",
                ),
                (
                    json!({"chains": [{"chain_id": 1, "assets": [{"asset": "pha"}]},
                                      {"chain_id": 1, "assets": [{"asset": "pha"}]}]}),
                    "chains[1][chain_id]",
                    "once",
                ),
                (
                    json!({"chains": [{"chain_id": 1, "assets": [{"asset": "pha"}, {"asset": "pha"}]}]}),
                    "chains[0][assets][1][asset]",
                    "once",
                ),
                (
                    json!({"chains": [{"chain_id": 1, "assets": []}]}),
                    "chains[0][assets]",
                    "at least one",
                ),
                // Weaker than the floor, of another chain family, or malformed.
                (
                    json!({"chains": [{"chain_id": 1, "confirmations": "1", "assets": [{"asset": "pha"}]}]}),
                    "chains[0][confirmations]",
                    "floor",
                ),
                (
                    json!({"chains": [{"chain_id": 1, "confirmations": "safe", "assets": [{"asset": "pha"}]}]}),
                    "chains[0][confirmations]",
                    "Ethereum",
                ),
                (
                    json!({"chains": [{"chain_id": 1, "confirmations": "02", "assets": [{"asset": "pha"}]}]}),
                    "chains[0][confirmations]",
                    "depth",
                ),
                // Outside the route's bounds.
                (
                    asset(json!({"quote_spread_bps": 600})),
                    "chains[0][assets][0][quote_spread_bps]",
                    "between 0 and 500",
                ),
                (
                    asset(json!({"quote_ttl_seconds": 10})),
                    "chains[0][assets][0][quote_ttl_seconds]",
                    "between 30 and 3600",
                ),
                (
                    asset(json!({"min_amount": 50})),
                    "chains[0][assets][0][min_amount]",
                    "between 100",
                ),
                (
                    asset(json!({"min_refund_atomic": "1"})),
                    "chains[0][assets][0][min_refund_atomic]",
                    "between 20 and 20",
                ),
                (
                    asset(json!({"max_deposit_atomic": "1e3"})),
                    "chains[0][assets][0][max_deposit_atomic]",
                    "decimal string",
                ),
                // Terms that cannot be used together.
                (
                    asset(json!({"min_deposit_atomic": "200000000000000000000000",
                                 "max_deposit_atomic": "100"})),
                    "chains[0][assets][0]",
                    "min_deposit_atomic exceeds max_deposit_atomic",
                ),
                (
                    json!({"quote_creations_per_customer_per_minute": 61}),
                    "quote_creations_per_customer_per_minute",
                    "between 1 and 60",
                ),
            ] {
                let (status, body) = fixture.post("/v1/payment_settings", request.clone()).await?;
                ensure!(status == StatusCode::BAD_REQUEST, "{request}: {body}");
                ensure!(body["error"]["param"] == param, "{request}: {body}");
                ensure!(
                    body["error"]["message"]
                        .as_str()
                        .is_some_and(|text| text.contains(message)),
                    "{request}: {body}"
                );
            }
            let (_, settings) = fixture
                .request(Method::GET, "/v1/payment_settings", Value::Null)
                .await?;
            ensure!(settings["status"] == "unconfigured", "nothing was written: {settings}");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_chain_is_offered_only_with_both_its_settings_and_a_treasury() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            // The test mode's chain is configured, but has no treasury yet.
            let test_key = seed::create_api_key(pool, fixture.account.id, false).await?;
            let (status, settings) = fixture
                .request_with(
                    &test_key,
                    Method::POST,
                    "/v1/payment_settings",
                    json!({"chains": [{"chain_id": TEST_CHAIN, "assets": [{"asset": "pha"}]}]}),
                )
                .await?;
            ensure!(status == StatusCode::OK, "{settings}");
            ensure!(settings["livemode"] == false);
            ensure!(
                settings["available"][0]["status"] == "treasury_not_set",
                "{settings}"
            );
            let (_, config) = fixture
                .request_with(&test_key, Method::GET, "/v1/config", Value::Null)
                .await?;
            ensure!(config["assets"] == json!([]), "{config}");
            let (status, body) = fixture
                .request_with(
                    &test_key,
                    Method::POST,
                    "/v1/quotes",
                    json!({"client_reference_id": "team-42", "amount": 1000, "currency": "usd",
                           "chain_id": TEST_CHAIN, "asset": "pha"}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{body}");
            ensure!(body["error"]["code"] == "treasury_not_set", "{body}");

            // With a treasury the chain is active; the live mode's settings are untouched.
            seed::set_treasury(
                pool,
                fixture.account.id,
                false,
                TEST_CHAIN,
                seed::FIXTURE_TREASURY,
            )
            .await?;
            let (_, settings) = fixture
                .request_with(&test_key, Method::GET, "/v1/payment_settings", Value::Null)
                .await?;
            ensure!(settings["available"][0]["status"] == "active", "{settings}");
            let (_, config) = fixture
                .request_with(&test_key, Method::GET, "/v1/config", Value::Null)
                .await?;
            ensure!(config["assets"][0]["chain_id"] == TEST_CHAIN, "{config}");
            ensure!(fixture.config_assets().await?.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn the_account_has_no_confirmation_policies_any_more() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let fixture = Fixture::new(&database.app_pool).await?;
            let (status, account) = fixture
                .request(Method::GET, "/v1/account", Value::Null)
                .await?;
            ensure!(status == StatusCode::OK, "{account}");
            ensure!(account.get("confirmation_policies").is_none(), "{account}");
            let (status, body) = fixture
                .post(
                    "/v1/account",
                    json!({"confirmation_policies": [{"chain_id": 1, "confirmations": "12"}]}),
                )
                .await?;
            ensure!(status == StatusCode::NOT_FOUND, "{body}");
            Ok(())
        })
    })
    .await
}

/// Historical legacy revisions continue to govern bound deposits after the account changes.
#[tokio::test]
async fn legacy_revisions_keep_the_terms_of_their_bound_deposits() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            sqlx::query("UPDATE accounts SET contact = $2, due_diligence = $3 WHERE id = $1")
                .bind(fixture.account.id)
                .bind(json!({"name": "Merchant", "email": "security@merchant.example"}))
                .bind(
                    json!({"reference": "DD-1", "reviewed_at": "2026-10-01", "reviewed_by": "ops"}),
                )
                .execute(&database.owner_pool)
                .await?;
            let route = fixture
                .routes
                .current_in(true)
                .next()
                .context("live route")?;
            let (_, deposit) = seed_payment(pool, fixture.account.id, route, 0x35).await?;
            let mut document =
                topup::payment_config::Document::accepting_all(&fixture.routes, true);
            document
                .chains
                .first_mut()
                .context("accepted chain")?
                .confirmations = Some(topup_core::route::Confirmations::Depth(12));
            let legacy = uuid::Uuid::new_v4();
            sqlx::query(
                "INSERT INTO payment_settings_revisions \
                 (id, account_id, livemode, kind, document, created_by) \
                 VALUES ($1, $2, true, 'legacy', $3, 'system')",
            )
            .bind(legacy)
            .bind(fixture.account.id)
            .bind(sqlx::types::Json(&document))
            .execute(&database.owner_pool)
            .await?;
            sqlx::query("UPDATE deposits SET settings_revision_id = $2 WHERE id = $1")
                .bind(deposit)
                .bind(legacy)
                .execute(&database.owner_pool)
                .await?;
            fixture.accept_the_live_route().await?;
            let mut connection = pool.acquire().await?;
            let binding = topup::payment_config::deposit_binding(&mut connection, deposit).await?;
            ensure!(
                binding
                    == topup::payment_config::Binding::Revision {
                        id: legacy,
                        document
                    }
            );
            let terms =
                topup::payment_config::deposit_terms(&mut connection, &fixture.routes, deposit)
                    .await?
                    .context("the legacy revision accepts the deposit")?;
            ensure!(terms.confirmations == topup_core::route::Confirmations::Depth(12));
            let path = format!("/v1/admin/accounts/{}", fixture.account.public_id);
            let (status, account) = fixture.admin(Method::GET, &path, Value::Null).await?;
            ensure!(status == StatusCode::OK, "{account}");
            let settings = account["payment_settings"]
                .as_object()
                .context("payment settings")?;
            ensure!(
                settings.len() == 2
                    && settings.contains_key("live")
                    && settings.contains_key("test")
            );
            Ok(())
        })
    })
    .await
}

/// Two writes of one account and mode apply one after the other: the second waits for the
/// first's commit and then reads the revision it wrote (design §4, §7).
#[tokio::test]
async fn concurrent_payment_settings_writes_apply_one_after_the_other() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let scope = topup::tenancy::Scope::new(fixture.account.id, true);
            // Queue two real POSTs behind a recorder's state lock, in a known order.
            let mut recorder = pool.begin().await?;
            topup::payment_config::load_for_share(&mut recorder, scope).await?;
            let post = |document: Value| {
                let app = fixture.app.clone();
                let key = fixture.live_key.clone();
                tokio::spawn(async move {
                    let response = app
                        .oneshot(merchant_request(
                            Method::POST,
                            "/v1/payment_settings",
                            serde_json::to_vec(&document)?,
                            &key,
                        ))
                        .await?;
                    let status = response.status();
                    let bytes = to_bytes(response.into_body(), 1_048_576).await?;
                    anyhow::Ok((status, serde_json::from_slice::<Value>(&bytes)?))
                })
            };
            let first = post(json!({"chains": [{"chain_id": 1,
                                                 "assets": [{"asset": "pha"}]}]}));
            wait_for_settings_writers(&database.owner_pool, 1).await?;
            let second = post(json!({"chains": []}));
            wait_for_settings_writers(&database.owner_pool, 2).await?;
            recorder.commit().await?;
            let (status, settings) =
                tokio::time::timeout(std::time::Duration::from_secs(5), first).await???;
            ensure!(status == StatusCode::OK, "{settings}");
            ensure!(settings["chains"][0]["chain_id"] == 1, "{settings}");
            let (status, settings) =
                tokio::time::timeout(std::time::Duration::from_secs(5), second).await???;
            ensure!(status == StatusCode::OK, "{settings}");
            ensure!(settings["chains"] == json!([]), "{settings}");
            let (_, current) = fixture
                .request(Method::GET, "/v1/payment_settings", Value::Null)
                .await?;
            ensure!(current["revision"] == settings["revision"], "{current}");
            ensure!(current["chains"] == json!([]), "{current}");
            let events: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM events WHERE account_id = $1 AND livemode \
                 AND type = 'payment_settings.updated'",
            )
            .bind(fixture.account.id)
            .fetch_one(pool)
            .await?;
            ensure!(events == 2);
            let written: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM payment_settings_revisions \
                 WHERE account_id = $1 AND livemode AND kind = 'configured'",
            )
            .bind(fixture.account.id)
            .fetch_one(pool)
            .await?;
            ensure!(written == 2);
            Ok(())
        })
    })
    .await
}

async fn wait_for_settings_writers(pool: &sqlx::PgPool, count: i64) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock' \
                 AND query LIKE '%FROM payment_settings_state AS state%'",
            )
            .fetch_one(pool)
            .await?;
            if waiting == count {
                return anyhow::Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("settings POSTs did not reach the state lock")?
}

/// A quote shows the terms it was issued with, confirmation included, whatever the catalog says
/// now (design §8).
#[tokio::test]
async fn a_quote_shows_the_terms_it_was_issued_with_after_the_floor_rises() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let (account, live_key) = Fixture::seed_account(pool).await?;
            let issued = live_route(1, "2", "")?;
            let (address_id, _) = seed_payment(pool, account.id, &issued, 0x31).await?;
            let quote: uuid::Uuid =
                sqlx::query_scalar("SELECT quote_id FROM addresses WHERE id = $1")
                    .bind(address_id)
                    .fetch_one(pool)
                    .await?;
            sqlx::query("UPDATE quotes SET terms = $2 WHERE id = $1")
                .bind(quote)
                .bind(serde_json::to_value(
                    topup::payment_config::Terms::defaults(&issued),
                )?)
                .execute(&database.owner_pool)
                .await?;
            // The operator now credits the chain at five confirmations.
            let fixture = Fixture::for_account(
                pool,
                account,
                live_key,
                Fixture::catalog(vec![live_route(1, "5", "")?])?,
            )?;
            let (status, object) = fixture
                .request(
                    Method::GET,
                    &format!("/v1/quotes/{}", topup::locks::quote_id(quote)),
                    Value::Null,
                )
                .await?;
            ensure!(status == StatusCode::OK, "{object}");
            ensure!(object["terms"]["confirmations"] == "2", "{object}");
            Ok(())
        })
    })
    .await
}

/// The design's floor-tightening scenario (§12): a new route version raises the chain's floor and
/// tightens a bound. The historical version keeps its terms; the chain's floor is the current
/// version's; a deposit not credited yet waits for the stricter of it and its bound requirement;
/// and the pair whose terms the new bound breaks is disabled and reported.
#[tokio::test]
async fn the_operator_tightens_the_floor_and_a_bound_in_a_new_route_version() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let (account, live_key) = Fixture::seed_account(pool).await?;
            let v1 = live_route(
                1,
                "2",
                "merchant:\n  min_amount: { default: 100 }\n  \
                 min_deposit_atomic: { default: \"20000000000000000000\", \
                 max: \"1000000000000000000000\" }\n  \
                 max_deposit_atomic: { default: \"200000000000000000000000\" }\n  \
                 min_refund_atomic: { default: \"20\" }\n",
            )?;
            let v2 = live_route(
                2,
                "5",
                "merchant:\n  min_amount: { default: 100 }\n  \
                 min_deposit_atomic: { default: \"20000000000000000000\", \
                 max: \"1000000000000000000000\" }\n  \
                 max_deposit_atomic: { default: \"100000000000000000000\", \
                 max: \"100000000000000000000\" }\n  \
                 min_refund_atomic: { default: \"20\" }\n",
            )?;
            let before = Fixture::for_account(
                pool,
                account.clone(),
                live_key.clone(),
                Fixture::catalog(vec![v1.clone()])?,
            )?;
            let (status, settings) = before
                .post(
                    "/v1/payment_settings",
                    json!({"chains": [{"chain_id": 1, "confirmations": "3", "assets": [
                        {"asset": "pha", "min_deposit_atomic": "500000000000000000000"}]}]}),
                )
                .await?;
            ensure!(status == StatusCode::OK, "{settings}");
            let (_, deposit) = seed_payment(pool, account.id, &v1, 0x32).await?;

            let after = Fixture::for_account(
                pool,
                account.clone(),
                live_key,
                Fixture::catalog(vec![v1.clone(), v2.clone()])?,
            )?;
            let floor = after.routes.chain(1).context("chain 1")?.confirmations;
            ensure!(floor == topup_core::route::Confirmations::Depth(5));
            let mut connection = pool.acquire().await?;
            let binding = topup::payment_config::deposit_binding(&mut connection, deposit).await?;
            ensure!(binding.confirmations(1) == Some(topup_core::route::Confirmations::Depth(3)));
            ensure!(
                topup::payment_config::required_confirmations(floor, binding.confirmations(1))
                    == topup_core::route::Confirmations::Depth(5)
            );
            // The deposit keeps its version's terms: v1 accepts its merchant's minimum.
            let terms =
                topup::payment_config::deposit_terms(&mut connection, &after.routes, deposit)
                    .await?
                    .context("the deposit's version accepts it")?;
            ensure!(terms.max_deposit_atomic == v1.merchant.max_deposit_atomic.default);

            let (status, settings) = after
                .request(Method::GET, "/v1/payment_settings", Value::Null)
                .await?;
            ensure!(status == StatusCode::OK, "{settings}");
            ensure!(
                settings["available"][0]["confirmations"]["floor"] == "5",
                "{settings}"
            );
            ensure!(
                settings["available"][0]["assets"][0]["enabled"] == false,
                "{settings}"
            );
            ensure!(after.config_assets().await?.is_empty());
            let invalid = topup::payment_config::invalid_pairs(pool, &after.routes).await?;
            ensure!(
                invalid
                    .iter()
                    .any(|(id, livemode, route, _)| *id == account.id
                        && *livemode
                        && *route == v2.route),
                "{invalid:?}"
            );
            // A requirement below the new floor is refused.
            let (status, refused) = after
                .post(
                    "/v1/payment_settings",
                    json!({"chains": [{"chain_id": 1, "confirmations": "3",
                                       "assets": [{"asset": "pha"}]}]}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{refused}");
            ensure!(
                refused["error"]["param"] == "chains[0][confirmations]",
                "{refused}"
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_merchant_pauses_its_own_quotes_and_never_lifts_the_operators_pause() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            fixture.accept_the_live_route().await?;
            let (status, account) = fixture
                .post("/v1/account/pause", json!({"scopes": ["quotes"]}))
                .await?;
            ensure!(status == StatusCode::OK, "{account}");
            ensure!(account["paused_scopes"] == json!(["quotes"]), "{account}");
            ensure!(fixture.quote_error().await? == "paused");
            let (status, body) = fixture
                .post(
                    "/v1/deposit_addresses",
                    json!({"client_reference_id": "team-42"}),
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST && body["error"]["code"] == "paused");

            // The operator pauses too; the merchant's resume lifts only its own pause.
            seed::set_account_paused_scopes(pool, fixture.account.id, &["quotes".to_owned()])
                .await?;
            let (status, account) = fixture
                .post("/v1/account/resume", json!({"scopes": ["quotes"]}))
                .await?;
            ensure!(status == StatusCode::OK, "{account}");
            ensure!(account["paused_scopes"] == json!(["quotes"]), "{account}");
            ensure!(fixture.quote_error().await? == "paused");
            let own: Vec<String> =
                sqlx::query_scalar("SELECT self_paused_scopes FROM accounts WHERE id = $1")
                    .bind(fixture.account.id)
                    .fetch_one(pool)
                    .await
                    .map(|scopes: Vec<String>| scopes)?;
            ensure!(own.is_empty());
            seed::set_account_paused_scopes(pool, fixture.account.id, &[]).await?;
            ensure!(fixture.quote_error().await? != "paused");

            // Only `quotes` is the merchant's to pause.
            for scopes in [
                json!(["settlement"]),
                json!([]),
                json!(["quotes", "refunds"]),
            ] {
                let (status, body) = fixture
                    .post("/v1/account/pause", json!({"scopes": scopes}))
                    .await?;
                ensure!(status == StatusCode::BAD_REQUEST, "{body}");
                ensure!(body["error"]["param"] == "scopes");
            }
            Ok(())
        })
    })
    .await
}

struct Fixture {
    app: Router,
    routes: Arc<topup::routes::RouteSet>,
    admin_key: SigningKey,
    account: topup::db::Account,
    live_key: String,
}

impl Fixture {
    async fn new(pool: &sqlx::PgPool) -> Result<Self> {
        let (account, live_key) = Self::seed_account(pool).await?;
        Self::for_account(pool, account, live_key, Self::routes()?)
    }

    /// A live merchant account with a key and a treasury on chain 1.
    async fn seed_account(pool: &sqlx::PgPool) -> Result<(topup::db::Account, String)> {
        let account = seed::create_account(pool, &NewAccount::named("merchant")).await?;
        let live_key = seed::create_api_key(pool, account.id, true).await?;
        seed::set_treasury(pool, account.id, true, 1, seed::FIXTURE_TREASURY).await?;
        Ok((account, live_key))
    }

    /// The catalog: the fixture's PHA route on chain 1 (live) and on Sepolia (test).
    fn routes() -> Result<Arc<topup::routes::RouteSet>> {
        Self::catalog(vec![live_route(1, "2", "")?])
    }

    /// `live` and the test route on Sepolia.
    fn catalog(live: Vec<RouteFile>) -> Result<Arc<topup::routes::RouteSet>> {
        let test_route: RouteFile = serde_saphyr::from_str(
            &include_str!("fixtures/phala-cloud-pha.yaml")
                .replace(
                    "route: phala-cloud-ethereum-pha-usd",
                    "route: phala-cloud-sepolia-pha",
                )
                .replace("chain_id: 1", &format!("chain_id: {TEST_CHAIN}"))
                .replace("livemode: true", "livemode: false"),
        )?;
        Ok(Arc::new(
            topup::routes::RouteSet::new(live.into_iter().chain([test_route]).collect())
                .map_err(anyhow::Error::msg)?,
        ))
    }

    fn for_account(
        pool: &sqlx::PgPool,
        account: topup::db::Account,
        live_key: String,
        routes: Arc<topup::routes::RouteSet>,
    ) -> Result<Self> {
        let admin_key = SigningKey::from_bytes(&[48; 32]);
        let app = topup::api::router(AppState {
            pool: pool.clone(),
            routes: Arc::clone(&routes),
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(
                "admin/v1".to_owned(),
                &public_key_base64(&admin_key),
            )
            .map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
            client_reads: Arc::default(),
            rate_limits: Arc::default(),
            hint_limits: Arc::default(),
            transaction_hints: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        })
        .0;
        Ok(Self {
            app,
            routes,
            admin_key,
            account,
            live_key,
        })
    }

    async fn admin(&self, method: Method, path: &str, body: Value) -> Result<(StatusCode, Value)> {
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body)?
        };
        let request = signed_request(
            method,
            path,
            body,
            "admin/v1",
            &self.admin_key,
            chrono::Utc::now().timestamp(),
        );
        let response = self.app.clone().oneshot(request).await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_048_576).await?;
        Ok((status, serde_json::from_slice(&bytes)?))
    }

    async fn request_with(
        &self,
        key: &str,
        method: Method,
        path: &str,
        body: Value,
    ) -> Result<(StatusCode, Value)> {
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body)?
        };
        let response = self
            .app
            .clone()
            .oneshot(merchant_request(method, path, body, key))
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_048_576).await?;
        Ok((status, serde_json::from_slice(&bytes)?))
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Value,
    ) -> Result<(StatusCode, Value)> {
        self.request_with(&self.live_key, method, path, body).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<(StatusCode, Value)> {
        self.request(Method::POST, path, body).await
    }

    async fn accept_the_live_route(&self) -> Result<()> {
        let (status, body) = self
            .post(
                "/v1/payment_settings",
                json!({"chains": [{"chain_id": 1, "assets": [{"asset": "pha"}]}]}),
            )
            .await?;
        ensure!(status == StatusCode::OK, "{body}");
        Ok(())
    }

    async fn config_assets(&self) -> Result<Vec<Value>> {
        let (status, config) = self.request(Method::GET, "/v1/config", Value::Null).await?;
        ensure!(status == StatusCode::OK, "{config}");
        config["assets"].as_array().cloned().context("assets")
    }

    /// The error code of a quote request; pricing is unavailable in these tests, so an accepted,
    /// unpaused request fails later with `unavailable`.
    async fn quote_error(&self) -> Result<String> {
        let (status, body) = self
            .post(
                "/v1/quotes",
                json!({"client_reference_id": "team-42", "amount": 1000, "currency": "usd",
                       "chain_id": 1, "asset": "pha"}),
            )
            .await?;
        ensure!(!status.is_success(), "{body}");
        body["error"]["code"]
            .as_str()
            .map(str::to_owned)
            .context("error code")
    }
}

/// Version `version` of the fixture's live PHA route, crediting at `confirmations`, with
/// `merchant` replacing its merchant section when not empty.
fn live_route(version: u64, confirmations: &str, merchant: &str) -> Result<RouteFile> {
    let mut yaml = include_str!("fixtures/phala-cloud-pha.yaml")
        .replace("version: 1", &format!("version: {version}"))
        .replace(
            "confirmations: finalized",
            &format!("confirmations: {confirmations}"),
        );
    if !merchant.is_empty() {
        let start = yaml.find("merchant:").context("merchant section")?;
        yaml.truncate(start);
        yaml.push_str(merchant);
    }
    Ok(serde_saphyr::from_str(&yaml)?)
}

/// A customer's forwarder on `route` (with a canceled quote, as `seed::insert_address` gives it)
/// and a detected deposit to it; returns their ids.
async fn seed_payment(
    pool: &sqlx::PgPool,
    account_id: uuid::Uuid,
    route: &RouteFile,
    byte: u8,
) -> Result<(uuid::Uuid, uuid::Uuid)> {
    let customer = seed::create_customer(
        pool,
        &seed::NewCustomer {
            id: uuid::Uuid::new_v4(),
            account_id,
            livemode: true,
            client_reference_id: format!("team-{byte}"),
            paused_scopes: Vec::new(),
        },
    )
    .await?;
    let address = seed::insert_address(
        pool,
        &seed::NewAddress {
            id: uuid::Uuid::new_v4(),
            customer_id: customer.id,
            chain_id: 1,
            route: route.route.clone(),
            salt: alloy_primitives::B256::repeat_byte(byte),
            address: alloy_primitives::Address::repeat_byte(byte),
        },
    )
    .await?;
    let tx_hash = alloy_primitives::B256::repeat_byte(byte.wrapping_add(1));
    ensure!(
        topup::db::insert_deposit(
            pool,
            &topup::db::NewDeposit {
                chain_id: 1,
                tx_hash,
                receipt_log_index: 0,
                log_index: 0,
                block_number: 100,
                block_hash: alloy_primitives::B256::repeat_byte(9),
                block_time: chrono::Utc::now(),
                address_id: address.id,
                route: Some(route.route.clone()),
                route_version: Some(route.version),
                asset_contract: route.asset.contract,
                from_address: alloy_primitives::Address::repeat_byte(10),
                amount_atomic: topup_core::money::AtomicAmount::new(alloy_primitives::U256::from(
                    1_000_u64
                ),),
                state: topup_core::deposit::DepositState::Detected,
                reason: None,
                next_attempt_at: chrono::Utc::now(),
                tx_from: alloy_primitives::Address::repeat_byte(10),
                tx_nonce: 0,
                is_final: false,
            },
        )
        .await?
    );
    Ok((address.id, topup_core::identity::deposit_id(1, tx_hash, 0)))
}
