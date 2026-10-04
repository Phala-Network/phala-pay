//! Operator onboarding, API keys, idempotent POSTs, and rate limits (design D7, D8, D12, §12,
//! §13, §16 PR 5), in process against PostgreSQL.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use axum::body::to_bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use chrono::Utc;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use sqlx::PgPool;
use topup::api::{AppState, PublicOrigin, RateLimits, VerificationKey};
use topup::api_keys::{self, KeyKind};
use topup_adapters::attestation::DstackAttestor;
use topup_core::route::RouteFile;
use tower::ServiceExt;
use uuid::Uuid;

use support::seed::{self, NewAccount};
use support::{
    ManualClock, TEST_ORIGIN, TestDatabase, merchant_request, public_key_base64, signed_request,
};

const ADMIN_KID: &str = "admin/v1";

struct Harness {
    app: axum::Router,
    admin_key: SigningKey,
    /// Admin signatures are single-use, so each admin request is signed at a distinct second.
    created: AtomicI64,
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Harness {
    fn new(pool: &PgPool, limits: RateLimits) -> Result<Self> {
        let admin_key = SigningKey::from_bytes(&[77; 32]);
        let route: RouteFile =
            serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
        let state = AppState {
            pool: pool.clone(),
            routes: Arc::new(
                topup::routes::RouteSet::new(vec![route]).map_err(anyhow::Error::msg)?,
            ),
            admin_key: VerificationKey::from_base64(
                ADMIN_KID.to_owned(),
                &public_key_base64(&admin_key),
            )
            .map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
            client_reads: Arc::default(),
            // The limiter's clock stands still, so no limit refills between a test's requests
            // however slowly the machine answers them.
            rate_limits: Arc::new(ManualClock::new().rate_limiter(limits)),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        };
        Ok(Self {
            app: topup::api::router(state).0,
            admin_key,
            created: AtomicI64::new(Utc::now().timestamp()),
        })
    }

    async fn admin(&self, method: Method, path: &str, body: &Value) -> Result<Answer> {
        let created = self.created.fetch_sub(1, Ordering::Relaxed);
        let request = signed_request(
            method,
            path,
            serde_json::to_vec(body)?,
            ADMIN_KID,
            &self.admin_key,
            created,
        );
        answer(self.app.clone().oneshot(request).await?).await
    }

    async fn merchant(
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
        self.merchant(Method::GET, path, None, key, None).await
    }

    /// A request with a raw `Authorization` header, or none.
    async fn with_authorization(&self, authorization: Option<&str>) -> Result<Answer> {
        let mut request = axum::http::Request::get(format!("{TEST_ORIGIN}/v1/account"));
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        answer(
            self.app
                .clone()
                .oneshot(request.body(axum::body::Body::empty())?)
                .await?,
        )
        .await
    }
}

async fn answer(response: axum::response::Response) -> Result<Answer> {
    let status = response.status();
    let headers = response.headers().clone();
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

fn account_body(name: &str, charges_enabled: bool) -> Value {
    json!({
        "name": name,
        "contact": {"name": "Ada Ops", "email": "security@merchant.test"},
        "due_diligence": {
            "reference": "DD-2026-042",
            "reviewed_at": "2026-09-28",
            "reviewed_by": "operator@phala.network",
        },
        "charges_enabled": charges_enabled,
        "reason": "merchant agreement signed",
    })
}

fn secret(key: &Value) -> Result<String> {
    Ok(key["secret"].as_str().context("secret")?.to_owned())
}

/// A test account and its test key.
async fn seed_test_account(pool: &PgPool, name: &str) -> Result<(Uuid, String)> {
    let account = seed::create_account(
        pool,
        &NewAccount {
            livemode: false,
            ..NewAccount::named(name)
        },
    )
    .await?;
    Ok((
        account.id,
        seed::create_api_key(pool, account.id, false).await?,
    ))
}

async fn events(pool: &PgPool, account: Uuid) -> Result<Vec<(String, bool, String)>> {
    Ok(sqlx::query_as(
        "SELECT type, livemode, actor FROM events WHERE account_id = $1 ORDER BY created, type",
    )
    .bind(account)
    .fetch_all(pool)
    .await?)
}

async fn audit_actions(pool: &PgPool, account: Uuid) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT action, actor_type FROM audit WHERE account_id = $1 ORDER BY created_at, action",
    )
    .bind(account)
    .fetch_all(pool)
    .await?)
}

/// The operator creates an account with its contact and due diligence and hands over the first
/// test key; live mode (and the first live key) comes only from the operator, and a recovery key
/// can revoke the mode's keys. Every step is audited and is an event naming its actor.
#[tokio::test]
async fn operator_onboards_accounts_enables_live_mode_and_recovers_keys() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;

        for (field, value) in [
            ("name", json!(" ")),
            ("contact", json!({"name": "Ada", "email": "not-an-email"})),
            ("contact", json!({"name": "", "email": "a@b.test"})),
            (
                "due_diligence",
                json!({"reference": "", "reviewed_at": "2026-09-28", "reviewed_by": "op"}),
            ),
            ("reason", json!("")),
            ("webhook_url", json!("ftp://merchant.test/hooks")),
            ("live_access", json!(true)),
        ] {
            let mut body = account_body("Merchant", false);
            body[field] = value;
            let refused = harness
                .admin(Method::POST, "/v1/admin/accounts", &body)
                .await?;
            ensure!(
                refused.status == StatusCode::BAD_REQUEST,
                "{field}: {}",
                refused.body
            );
        }

        let created = harness
            .admin(
                Method::POST,
                "/v1/admin/accounts",
                &account_body("Merchant", false),
            )
            .await?;
        ensure!(created.status == StatusCode::OK, "{}", created.body);
        let account = created.body;
        let account_id =
            topup::ids::parse(topup::ids::ACCOUNT, account["id"].as_str().context("id")?)
                .context("acct_ id")?;
        ensure!(account["object"] == "account" && account["charges_enabled"] == false);
        ensure!(account["contact"]["email"] == "security@merchant.test");
        ensure!(account["due_diligence"]["reference"] == "DD-2026-042");
        let keys = account["api_keys"].as_array().context("api_keys")?;
        ensure!(keys.len() == 1, "only a test key without live mode");
        let test_key = secret(&keys[0])?;
        ensure!(test_key.starts_with("ppay_sk_test_") && keys[0]["livemode"] == false);
        ensure!(api_keys::check_format(&test_key).is_some());

        // The first key works, and nothing stored reveals it again.
        let me = harness.get("/v1/account", &test_key).await?;
        ensure!(me.status == StatusCode::OK, "{}", me.body);
        ensure!(me.body["id"] == account["id"] && me.body["livemode"] == false);
        let listed = harness.get("/v1/api_keys", &test_key).await?;
        ensure!(listed.body["data"].as_array().map(Vec::len) == Some(1));
        ensure!(listed.body["data"][0].get("secret").is_none());
        ensure!(
            listed.body["data"][0]["redacted"]
                == format!("ppay_sk_test_…{}", &test_key[test_key.len() - 4..])
        );
        let created_by: String =
            sqlx::query_scalar("SELECT created_by FROM api_keys WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(pool)
                .await?;
        ensure!(created_by == "admin");

        // A merchant key cannot reach the admin API.
        let merchant_admin = harness
            .merchant(
                Method::POST,
                "/v1/admin/accounts",
                Some(&account_body("Other", true)),
                &test_key,
                None,
            )
            .await?;
        ensure!(merchant_admin.status == StatusCode::UNAUTHORIZED);

        // No live key until the operator enables live mode.
        let early_live = harness
            .admin(
                Method::POST,
                &format!(
                    "/v1/admin/accounts/{}/api_keys",
                    account["id"].as_str().context("id")?
                ),
                &json!({"livemode": true, "reason": "asked by the contact"}),
            )
            .await?;
        ensure!(early_live.status == StatusCode::FORBIDDEN);
        ensure!(early_live.body["error"]["code"] == "testmode_charges_only");

        let account_path = format!(
            "/v1/admin/accounts/{}",
            account["id"].as_str().context("id")?
        );
        let enable = json!({"charges_enabled": true, "reason": "due diligence DD-2026-042 passed"});
        let enabled = harness.admin(Method::POST, &account_path, &enable).await?;
        ensure!(enabled.status == StatusCode::OK, "{}", enabled.body);
        ensure!(enabled.body["charges_enabled"] == true);
        let live_keys = enabled.body["api_keys"].as_array().context("api_keys")?;
        ensure!(live_keys.len() == 1 && live_keys[0]["livemode"] == true);
        let live_key = secret(&live_keys[0])?;
        ensure!(live_key.starts_with("ppay_sk_live_"));
        let live_me = harness.get("/v1/account", &live_key).await?;
        ensure!(live_me.status == StatusCode::OK && live_me.body["livemode"] == true);
        // A test key never reaches live keys.
        let test_view = harness.get("/v1/api_keys", &test_key).await?;
        ensure!(
            test_view.body["data"]
                .as_array()
                .is_some_and(|keys| keys.iter().all(|key| key["livemode"] == false))
        );
        let live_key_id = live_keys[0]["id"].as_str().context("key id")?;
        let hidden = harness
            .get(&format!("/v1/api_keys/{live_key_id}"), &test_key)
            .await?;
        ensure!(hidden.status == StatusCode::NOT_FOUND);

        // Repeating the update changes nothing and issues no second live key.
        let repeated = harness.admin(Method::POST, &account_path, &enable).await?;
        ensure!(repeated.status == StatusCode::OK);
        ensure!(repeated.body["api_keys"] == json!([]));

        // The operator sets the cap on credit before finality, $1 000 by default.
        ensure!(enabled.body["max_unfinalized_credit"] == 100_000);
        let capped = harness
            .admin(
                Method::POST,
                &account_path,
                &json!({"max_unfinalized_credit": 0, "reason": "sells irreversible goods"}),
            )
            .await?;
        ensure!(capped.status == StatusCode::OK, "{}", capped.body);
        ensure!(capped.body["max_unfinalized_credit"] == 0);

        // The operator sets the caps of one mode; the other mode keeps its defaults.
        ensure!(capped.body["limits"]["live"]["max_open_amount_per_account"] == 5_000_000);
        let limited = harness
            .admin(
                Method::POST,
                &account_path,
                &json!({
                    "limits": {"livemode": false, "max_open_quotes": 5, "max_open_amount_per_customer": 900},
                    "reason": "pilot limits approved",
                }),
            )
            .await?;
        ensure!(limited.status == StatusCode::OK, "{}", limited.body);
        let test_limits = &limited.body["limits"]["test"];
        ensure!(test_limits["max_open_quotes"] == 5 && test_limits["max_open_amount_per_customer"] == 900);
        ensure!(test_limits["max_open_amount_per_account"] == 1_000_000);
        ensure!(limited.body["limits"]["live"]["max_open_quotes"] == 1_000);
        let config = harness.get("/v1/config", &test_key).await?;
        ensure!(config.body["max_open_quotes"] == 5, "{}", config.body);
        ensure!(config.body["max_open_amount_per_customer"] == 900);
        let invalid = harness
            .admin(
                Method::POST,
                &account_path,
                &json!({"limits": {"livemode": true, "max_open_quotes": 0}, "reason": "typo"}),
            )
            .await?;
        ensure!(invalid.status == StatusCode::BAD_REQUEST, "{}", invalid.body);
        ensure!(invalid.body["error"]["param"] == "limits.max_open_quotes");

        // Turning live mode off stops live keys at once.
        let disabled = harness
            .admin(
                Method::POST,
                &account_path,
                &json!({"charges_enabled": false, "reason": "incident review"}),
            )
            .await?;
        ensure!(disabled.status == StatusCode::OK);
        let refused = harness.get("/v1/account", &live_key).await?;
        ensure!(refused.status == StatusCode::FORBIDDEN);
        ensure!(refused.body["error"]["code"] == "testmode_charges_only");

        // Recovery: the operator revokes the mode's keys and issues a new one.
        let recovered = harness
            .admin(
                Method::POST,
                &format!("{account_path}/api_keys"),
                &json!({
                    "livemode": false,
                    "revoke_existing": true,
                    "reason": "contact reported a leak; verified by phone",
                }),
            )
            .await?;
        ensure!(recovered.status == StatusCode::OK, "{}", recovered.body);
        let recovery_key = secret(&recovered.body)?;
        let old = harness.get("/v1/account", &test_key).await?;
        ensure!(old.status == StatusCode::UNAUTHORIZED);
        ensure!(old.body["error"]["code"] == "api_key_invalid");
        ensure!(harness.get("/v1/account", &recovery_key).await?.status == StatusCode::OK);

        let audit = audit_actions(pool, account_id).await?;
        for action in [
            "account.create",
            "account.update",
            "api_key.created",
            "api_key.revoked",
        ] {
            ensure!(
                audit
                    .iter()
                    .any(|(stored, actor)| stored == action && actor == "admin"),
                "{action}: {audit:?}"
            );
        }
        let updates = audit
            .iter()
            .filter(|(action, _)| action == "account.update")
            .count();
        // Enabling live mode, the cap, the limits, and disabling it; neither the repeat nor the
        // refused limits are audited.
        ensure!(updates == 4, "the repeat is not audited: {audit:?}");

        let events = events(pool, account_id).await?;
        ensure!(
            events.iter().all(|(_, _, actor)| actor == "admin"),
            "{events:?}"
        );
        for expected in [
            ("api_key.created", false),
            ("api_key.created", true),
            ("account.updated", false),
            ("account.updated", true),
            ("api_key.revoked", false),
        ] {
            ensure!(
                events
                    .iter()
                    .any(|(kind, livemode, _)| (kind.as_str(), *livemode) == expected),
                "{expected:?}: {events:?}"
            );
        }

        // Creating with live mode hands over both keys at once.
        let both = harness
            .admin(
                Method::POST,
                "/v1/admin/accounts",
                &account_body("Phala Cloud", true),
            )
            .await?;
        ensure!(both.status == StatusCode::OK);
        let modes: Vec<Value> = both.body["api_keys"]
            .as_array()
            .context("api_keys")?
            .iter()
            .map(|key| key["livemode"].clone())
            .collect();
        ensure!(modes == [json!(false), json!(true)]);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Bearer keys only: valid, missing, other schemes, malformed, bad checksum, unknown, revoked,
/// expired, and rolled keys within and after their overlap.
#[tokio::test]
async fn keys_authenticate_by_bearer_and_expire_or_revoke() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (account, key) = seed_test_account(pool, "auth").await?;

        let valid = harness
            .with_authorization(Some(&format!("Bearer {key}")))
            .await?;
        ensure!(valid.status == StatusCode::OK, "{}", valid.body);
        let lowercase = harness
            .with_authorization(Some(&format!("bearer {key}")))
            .await?;
        ensure!(lowercase.status == StatusCode::OK);
        let last_used: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_used_at FROM api_keys WHERE account_id = $1")
                .bind(account)
                .fetch_one(pool)
                .await?;
        ensure!(last_used.is_some());

        let missing = harness.with_authorization(None).await?;
        ensure!(missing.status == StatusCode::UNAUTHORIZED);
        ensure!(missing.body["error"]["code"] == "api_key_missing");
        ensure!(missing.headers["www-authenticate"] == "Bearer realm=\"Phala Pay\"");

        let basic = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{key}:"),
        );
        let mut checksum_flipped = key.clone();
        let last = checksum_flipped.pop().context("key")?;
        checksum_flipped.push(if last == 'A' { 'B' } else { 'A' });
        let unknown = api_keys::generate(KeyKind::Secret, false)?;
        for authorization in [
            format!("Basic {basic}"),
            format!("Token {key}"),
            "Bearer".to_owned(),
            // Another vendor's key shape, built so no scanner mistakes it for a real key.
            format!("Bearer sk_{}_{}", "test", "x".repeat(24)),
            format!("Bearer {}", &key[..key.len() - 1]),
            format!("Bearer {checksum_flipped}"),
            format!("Bearer {}", unknown.as_str()),
        ] {
            let refused = harness.with_authorization(Some(&authorization)).await?;
            ensure!(
                refused.status == StatusCode::UNAUTHORIZED
                    && refused.body["error"]["code"] == "api_key_invalid",
                "{authorization}: {} {}",
                refused.status,
                refused.body
            );
        }

        // A rolled key keeps working until its expiry, then answers `api_key_expired`.
        let key_id = harness.get("/v1/api_keys", &key).await?.body["data"][0]["id"]
            .as_str()
            .context("key id")?
            .to_owned();
        let rolled = harness
            .merchant(
                Method::POST,
                &format!("/v1/api_keys/{key_id}/roll"),
                Some(&json!({"expires_in": 3600})),
                &key,
                None,
            )
            .await?;
        ensure!(rolled.status == StatusCode::OK, "{}", rolled.body);
        let new_key = secret(&rolled.body)?;
        ensure!(harness.get("/v1/account", &key).await?.status == StatusCode::OK);
        ensure!(harness.get("/v1/account", &new_key).await?.status == StatusCode::OK);
        let old = harness
            .get(&format!("/v1/api_keys/{key_id}"), &new_key)
            .await?;
        ensure!(old.body["status"] == "expiring", "{}", old.body);
        ensure!(old.body["expires_at"].as_i64() > Some(Utc::now().timestamp() + 3500));
        sqlx::query("UPDATE api_keys SET expires_at = now() - interval '1 second' WHERE id = $1")
            .bind(topup::ids::parse(topup::ids::API_KEY, &key_id).context("key id")?)
            .execute(&database.owner_pool)
            .await?;
        let expired = harness.get("/v1/account", &key).await?;
        ensure!(expired.status == StatusCode::UNAUTHORIZED);
        ensure!(expired.body["error"]["code"] == "api_key_expired");

        // A key rolling itself keeps working for at least an hour, so a roll whose response is
        // lost can be recovered with it: the replay names the new key, without its secret, and
        // the old key rolls that new key with no overlap, revoking it at once.
        let new_id = rolled.body["id"].as_str().context("id")?.to_owned();
        let self_roll = format!("/v1/api_keys/{new_id}/roll");
        for body in [json!({}), json!({"expires_in": 3599})] {
            let refused = harness
                .merchant(
                    Method::POST,
                    &self_roll,
                    Some(&body),
                    &new_key,
                    Some("self"),
                )
                .await?;
            ensure!(
                refused.status == StatusCode::BAD_REQUEST,
                "{}",
                refused.body
            );
            ensure!(refused.body["error"]["code"] == "parameter_invalid");
            ensure!(refused.body["error"]["param"] == "expires_in");
        }
        let hour = json!({"expires_in": 3600});
        let lost = harness
            .merchant(
                Method::POST,
                &self_roll,
                Some(&hour),
                &new_key,
                Some("self"),
            )
            .await?;
        ensure!(lost.status == StatusCode::OK, "{}", lost.body);
        let replayed = harness
            .merchant(
                Method::POST,
                &self_roll,
                Some(&hour),
                &new_key,
                Some("self"),
            )
            .await?;
        ensure!(replayed.headers["idempotent-replayed"] == "true");
        ensure!(replayed.body.get("secret").is_none());
        let lost_id = replayed.body["id"].as_str().context("id")?.to_owned();
        ensure!(lost_id == lost.body["id"]);
        let recovered = harness
            .merchant(
                Method::POST,
                &format!("/v1/api_keys/{lost_id}/roll"),
                Some(&json!({})),
                &new_key,
                None,
            )
            .await?;
        ensure!(recovered.status == StatusCode::OK, "{}", recovered.body);
        let revoked = harness.get("/v1/account", &secret(&lost.body)?).await?;
        ensure!(revoked.body["error"]["code"] == "api_key_invalid");
        ensure!(
            harness
                .get("/v1/account", &secret(&recovered.body)?)
                .await?
                .status
                == StatusCode::OK
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// A secret key creates, lists, rolls, and revokes its mode's keys; the last active key cannot
/// be revoked, and every change is an event naming the acting key.
#[tokio::test]
async fn merchants_manage_their_keys_and_cannot_lock_themselves_out() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (account, key) = seed_test_account(pool, "manage").await?;
        let first_id = harness.get("/v1/api_keys", &key).await?.body["data"][0]["id"]
            .as_str()
            .context("id")?
            .to_owned();

        let last = harness
            .merchant(
                Method::DELETE,
                &format!("/v1/api_keys/{first_id}"),
                None,
                &key,
                None,
            )
            .await?;
        ensure!(last.status == StatusCode::BAD_REQUEST);
        ensure!(last.body["error"]["code"] == "last_api_key");

        let long_name = "n".repeat(201);
        let refused = harness
            .merchant(
                Method::POST,
                "/v1/api_keys",
                Some(&json!({"name": long_name})),
                &key,
                None,
            )
            .await?;
        ensure!(refused.status == StatusCode::BAD_REQUEST);

        let created = harness
            .merchant(
                Method::POST,
                "/v1/api_keys",
                Some(&json!({"name": "ci deploys"})),
                &key,
                None,
            )
            .await?;
        ensure!(created.status == StatusCode::OK, "{}", created.body);
        ensure!(created.body["name"] == "ci deploys" && created.body["status"] == "active");
        ensure!(created.body["type"] == "secret" && created.body["livemode"] == false);
        let second = secret(&created.body)?;
        let second_id = created.body["id"].as_str().context("id")?.to_owned();
        let created_by: String =
            sqlx::query_scalar("SELECT created_by FROM api_keys WHERE id = $1")
                .bind(topup::ids::parse(topup::ids::API_KEY, &second_id).context("id")?)
                .fetch_one(pool)
                .await?;
        ensure!(created_by == first_id);

        for body in [json!({"expires_in": 604_801}), json!({"expires_in": -1})] {
            let refused = harness
                .merchant(
                    Method::POST,
                    &format!("/v1/api_keys/{second_id}/roll"),
                    Some(&body),
                    &key,
                    None,
                )
                .await?;
            ensure!(refused.status == StatusCode::BAD_REQUEST, "{body}");
        }
        let rolled = harness
            .merchant(
                Method::POST,
                &format!("/v1/api_keys/{second_id}/roll"),
                Some(&json!({"expires_in": 604_800})),
                &key,
                None,
            )
            .await?;
        ensure!(rolled.status == StatusCode::OK);
        ensure!(rolled.body["name"] == "ci deploys");
        let again = harness
            .merchant(
                Method::POST,
                &format!("/v1/api_keys/{second_id}/roll"),
                Some(&json!({})),
                &key,
                None,
            )
            .await?;
        ensure!(again.status == StatusCode::BAD_REQUEST);
        ensure!(again.body["error"]["code"] == "api_key_inactive");

        // With other active keys, a key may revoke itself; revoking twice returns it unchanged.
        let revoked = harness
            .merchant(
                Method::DELETE,
                &format!("/v1/api_keys/{first_id}"),
                None,
                &key,
                None,
            )
            .await?;
        ensure!(revoked.status == StatusCode::OK && revoked.body["status"] == "revoked");
        ensure!(harness.get("/v1/account", &key).await?.status == StatusCode::UNAUTHORIZED);
        let twice = harness
            .merchant(
                Method::DELETE,
                &format!("/v1/api_keys/{first_id}"),
                None,
                &second,
                None,
            )
            .await?;
        ensure!(twice.status == StatusCode::OK && twice.body["status"] == "revoked");

        let listed = harness.get("/v1/api_keys", &second).await?;
        let statuses: Vec<Value> = listed.body["data"]
            .as_array()
            .context("data")?
            .iter()
            .map(|key| key["status"].clone())
            .collect();
        ensure!(
            statuses == [json!("active"), json!("expiring"), json!("revoked")],
            "{statuses:?}"
        );

        let events = events(pool, account).await?;
        let kinds: Vec<&str> = events.iter().map(|(kind, _, _)| kind.as_str()).collect();
        ensure!(
            kinds
                .iter()
                .filter(|kind| **kind == "api_key.created")
                .count()
                == 2
        );
        ensure!(kinds.contains(&"api_key.updated") && kinds.contains(&"api_key.revoked"));
        ensure!(
            events
                .iter()
                .all(|(_, livemode, actor)| !livemode && actor.starts_with("key_")),
            "{events:?}"
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Every merchant POST is idempotent per account and mode: a repeat of the same request replays
/// the first response, a different request with the same key is refused, and a key's secret is
/// never stored for replay.
#[tokio::test]
async fn posts_are_idempotent_per_account_and_mode() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (account, key) = seed_test_account(pool, "idempotent").await?;
        let (_, other_key) = seed_test_account(pool, "other").await?;
        let create = |body: Value, key: String, idempotency_key: &'static str| {
            let harness = &harness;
            async move {
                harness
                    .merchant(
                        Method::POST,
                        "/v1/api_keys",
                        Some(&body),
                        &key,
                        Some(idempotency_key),
                    )
                    .await
            }
        };
        let count = || async {
            anyhow::Ok(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM api_keys WHERE account_id = $1")
                    .bind(account)
                    .fetch_one(pool)
                    .await?,
            )
        };

        let first = create(json!({"name": "a"}), key.clone(), "retry-1").await?;
        ensure!(first.status == StatusCode::OK && first.body.get("secret").is_some());
        ensure!(!first.headers.contains_key("idempotent-replayed"));
        let replay = create(json!({"name": "a"}), key.clone(), "\"retry-1\"").await?;
        ensure!(replay.status == StatusCode::OK);
        ensure!(replay.headers["idempotent-replayed"] == "true");
        ensure!(replay.body["id"] == first.body["id"]);
        ensure!(replay.body.get("secret").is_none(), "secrets are never stored");
        ensure!(count().await? == 2);
        let stored: String = sqlx::query_scalar(
            "SELECT response::text FROM idempotency_keys WHERE account_id = $1 AND key = 'retry-1'",
        )
        .bind(account)
        .fetch_one(pool)
        .await?;
        ensure!(!stored.contains(&secret(&first.body)?), "{stored}");

        let other_body = create(json!({"name": "b"}), key.clone(), "retry-1").await?;
        ensure!(other_body.status == StatusCode::BAD_REQUEST);
        ensure!(other_body.body["error"]["type"] == "idempotency_error");
        ensure!(other_body.body["error"]["code"] == "idempotency_key_reused");
        let other_path = harness
            .merchant(
                Method::POST,
                "/v1/quotes",
                Some(&json!({"name": "a"})),
                &key,
                Some("retry-1"),
            )
            .await?;
        ensure!(other_path.status == StatusCode::BAD_REQUEST);
        ensure!(other_path.body["error"]["type"] == "idempotency_error");

        // Keys are scoped per account and mode.
        let other_account = create(json!({"name": "a"}), other_key.clone(), "retry-1").await?;
        ensure!(other_account.status == StatusCode::OK);
        ensure!(!other_account.headers.contains_key("idempotent-replayed"));
        let live_key = seed::create_api_key(pool, account, true).await?;
        sqlx::query("UPDATE accounts SET charges_enabled = true WHERE id = $1")
            .bind(account)
            .execute(pool)
            .await?;
        let other_mode = create(json!({"name": "a"}), live_key, "retry-1").await?;
        ensure!(other_mode.status == StatusCode::OK && other_mode.body["livemode"] == true);
        ensure!(!other_mode.headers.contains_key("idempotent-replayed"));

        // A request that failed validation did not execute and is not saved (Stripe): the same
        // key then runs a corrected request.
        let long = json!({"name": "n".repeat(201)});
        let refused = create(long.clone(), key.clone(), "retry-2").await?;
        ensure!(refused.status == StatusCode::BAD_REQUEST);
        ensure!(refused.body["error"]["code"] == "parameter_invalid");
        let refused_again = create(long, key.clone(), "retry-2").await?;
        ensure!(refused_again.status == StatusCode::BAD_REQUEST);
        ensure!(!refused_again.headers.contains_key("idempotent-replayed"));
        let corrected = create(json!({"name": "b"}), key.clone(), "retry-2").await?;
        ensure!(corrected.status == StatusCode::OK, "{}", corrected.body);

        // A request that executed and failed is saved and replayed as it failed.
        let revoked = corrected.body["id"].as_str().context("id")?.to_owned();
        let revoke = harness
            .merchant(Method::DELETE, &format!("/v1/api_keys/{revoked}"), None, &key, None)
            .await?;
        ensure!(revoke.status == StatusCode::OK, "{}", revoke.body);
        let roll = format!("/v1/api_keys/{revoked}/roll");
        let inactive = harness
            .merchant(Method::POST, &roll, Some(&json!({})), &key, Some("retry-3"))
            .await?;
        ensure!(inactive.status == StatusCode::BAD_REQUEST, "{}", inactive.body);
        ensure!(inactive.body["error"]["code"] == "api_key_inactive");
        let replayed = harness
            .merchant(Method::POST, &roll, Some(&json!({})), &key, Some("retry-3"))
            .await?;
        ensure!(replayed.status == StatusCode::BAD_REQUEST);
        ensure!(replayed.headers["idempotent-replayed"] == "true");
        ensure!(replayed.body == inactive.body);

        // A request still running holds its key; one that never finished frees it after a
        // minute; after 24 hours a key may be used for anything.
        let before = count().await?;
        sqlx::query(
            "INSERT INTO idempotency_keys (account_id, livemode, key, fingerprint) \
             VALUES ($1, false, 'running', $2)",
        )
        .bind(account)
        .bind(vec![0_u8; 32])
        .execute(pool)
        .await?;
        let running = create(json!({"name": "a"}), key.clone(), "running").await?;
        ensure!(running.status == StatusCode::BAD_REQUEST, "another fingerprint");
        let fingerprint: Vec<u8> = {
            let mut digest = <sha2::Sha256 as sha2::Digest>::new();
            sha2::Digest::update(&mut digest, b"POST\0/v1/api_keys\0{\"name\":\"a\"}");
            sha2::Digest::finalize(digest).to_vec()
        };
        sqlx::query(
            "UPDATE idempotency_keys SET fingerprint = $2 WHERE account_id = $1 AND key = 'running'",
        )
        .bind(account)
        .bind(&fingerprint)
        .execute(pool)
        .await?;
        let in_use = create(json!({"name": "a"}), key.clone(), "running").await?;
        ensure!(in_use.status == StatusCode::CONFLICT, "{}", in_use.body);
        ensure!(in_use.body["error"]["code"] == "idempotency_key_in_use");
        sqlx::query(
            "UPDATE idempotency_keys SET created_at = now() - interval '2 minutes' \
             WHERE account_id = $1 AND key = 'running'",
        )
        .bind(account)
        .execute(pool)
        .await?;
        let taken_over = create(json!({"name": "a"}), key.clone(), "running").await?;
        ensure!(taken_over.status == StatusCode::OK, "{}", taken_over.body);
        ensure!(count().await? == before + 1);
        sqlx::query(
            "UPDATE idempotency_keys SET created_at = now() - interval '25 hours' \
             WHERE account_id = $1 AND key = 'running'",
        )
        .bind(account)
        .execute(pool)
        .await?;
        let reused = create(json!({"name": "c"}), key.clone(), "running").await?;
        ensure!(reused.status == StatusCode::OK && reused.body["name"] == "c");
        let pruned: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM idempotency_keys WHERE created_at < now() - interval '24 hours'",
        )
        .fetch_one(pool)
        .await?;
        ensure!(pruned == 0);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Keys older than 24 hours are pruned in the background, every account's, not by a request's
/// claim; a key a claim holds is skipped, not waited for.
#[tokio::test]
async fn expired_idempotency_keys_are_pruned_off_the_request_path() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (account, key) = seed_test_account(pool, "pruned").await?;
        let (other, _) = seed_test_account(pool, "other").await?;
        for (owner, key, age) in [
            (account, "old", "25 hours"),
            (other, "old", "25 hours"),
            (other, "held", "25 hours"),
            (other, "fresh", "23 hours"),
        ] {
            sqlx::query(
                "INSERT INTO idempotency_keys (account_id, livemode, key, fingerprint, created_at) \
                 VALUES ($1, false, $2, $3, now() - $4::interval)",
            )
            .bind(owner)
            .bind(key)
            .bind(vec![0_u8; 32])
            .bind(age)
            .execute(pool)
            .await?;
        }
        let keys = || async {
            let mut keys = sqlx::query_scalar::<_, String>(
                "SELECT account_id::text || '/' || key FROM idempotency_keys",
            )
            .fetch_all(pool)
            .await?;
            keys.sort();
            anyhow::Ok(keys)
        };

        // A claim leaves the other account's expired keys alone.
        let created = harness
            .merchant(
                Method::POST,
                "/v1/api_keys",
                Some(&json!({"name": "a"})),
                &key,
                Some("new"),
            )
            .await?;
        ensure!(created.status == StatusCode::OK, "{}", created.body);
        let mut expected = vec![
            format!("{account}/new"),
            format!("{account}/old"),
            format!("{other}/fresh"),
            format!("{other}/held"),
            format!("{other}/old"),
        ];
        expected.sort();
        ensure!(keys().await? == expected);

        let pruner = topup::api::IdempotencyKeyPruner::new(pool.clone(), Duration::from_secs(600));
        let mut claim = pool.begin().await?;
        sqlx::query(
            "SELECT 1 FROM idempotency_keys WHERE account_id = $1 AND key = 'held' FOR UPDATE",
        )
        .bind(other)
        .execute(&mut *claim)
        .await?;
        let pruned = tokio::time::timeout(Duration::from_secs(5), pruner.prune_once()).await??;
        ensure!(pruned == 2, "{pruned}");
        let mut expected = vec![
            format!("{account}/new"),
            format!("{other}/fresh"),
            format!("{other}/held"),
        ];
        expected.sort();
        ensure!(keys().await? == expected);
        claim.rollback().await?;
        ensure!(pruner.prune_once().await? == 1);
        ensure!(keys().await?.len() == 2);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// A request's changes and its saved response commit in one transaction (Brandur Leach,
/// "Implementing Stripe-like Idempotency Keys in Postgres"): when the response cannot be saved,
/// nothing is created, and a repeat after the takeover interval runs the request once. A
/// response the client never received is replayed, never run again.
#[tokio::test]
async fn a_saved_result_commits_with_the_changes_it_reports() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (account, key) = seed_test_account(pool, "atomic").await?;
        let body = json!({"name": "atomic"});
        let create = || {
            harness.merchant(
                Method::POST,
                "/v1/api_keys",
                Some(&body),
                &key,
                Some("atomic"),
            )
        };
        let count = || async {
            anyhow::Ok(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM api_keys WHERE account_id = $1")
                    .bind(account)
                    .fetch_one(pool)
                    .await?,
            )
        };
        let age_key = || async {
            sqlx::query(
                "UPDATE idempotency_keys SET created_at = now() - interval '2 minutes' \
                 WHERE account_id = $1 AND key = 'atomic'",
            )
            .bind(account)
            .execute(pool)
            .await
        };
        let before = count().await?;

        // Saving the response fails, as when the process stops before it: the key the request
        // created is rolled back with it, and the request keeps holding its idempotency key.
        for statement in [
            "CREATE FUNCTION fail_save() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'response not saved'; END $$",
            "CREATE TRIGGER fail_save BEFORE UPDATE ON idempotency_keys FOR EACH ROW \
             WHEN (NEW.response IS NOT NULL) EXECUTE FUNCTION fail_save()",
        ] {
            sqlx::query(statement).execute(&database.owner_pool).await?;
        }
        let failed = create().await?;
        ensure!(
            failed.status == StatusCode::INTERNAL_SERVER_ERROR,
            "{}",
            failed.body
        );
        ensure!(
            count().await? == before,
            "nothing commits without its saved response"
        );
        sqlx::query("DROP FUNCTION fail_save() CASCADE")
            .execute(&database.owner_pool)
            .await?;
        let in_use = create().await?;
        ensure!(in_use.status == StatusCode::CONFLICT, "{}", in_use.body);
        ensure!(count().await? == before);

        // After the takeover interval the repeat runs the request, once.
        age_key().await?;
        let ran = create().await?;
        ensure!(ran.status == StatusCode::OK, "{}", ran.body);
        ensure!(!ran.headers.contains_key("idempotent-replayed"));
        ensure!(count().await? == before + 1);

        // Its response is lost on the way back: a repeat, however late, replays it.
        age_key().await?;
        let replayed = create().await?;
        ensure!(replayed.status == StatusCode::OK, "{}", replayed.body);
        ensure!(replayed.headers["idempotent-replayed"] == "true");
        ensure!(replayed.body["id"] == ran.body["id"]);
        ensure!(count().await? == before + 1, "the request ran once");
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Cancellation while saving a response rolls back the business write with the result. A
/// cancelled claim is recoverable after the existing takeover interval; a committed result
/// remains replayable if the client disappears just after commit.
#[tokio::test]
async fn cancelled_idempotent_handler_rolls_back_and_can_be_retried() -> Result<()> {
    support::with_database(|database| Box::pin(async move {
        let pool=&database.app_pool;
        let harness=Harness::new(pool,RateLimits::default())?;
        let (account,key)=seed_test_account(pool,"cancelled").await?;
        let before:i64=sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE account_id=$1").bind(account).fetch_one(pool).await?;
        for statement in [
            "CREATE FUNCTION pause_save() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(987654321); RETURN NEW; END $$",
            "CREATE TRIGGER pause_save BEFORE UPDATE ON idempotency_keys FOR EACH ROW WHEN (NEW.response IS NOT NULL) EXECUTE FUNCTION pause_save()",
        ] {sqlx::query(statement).execute(&database.owner_pool).await?;}
        let mut gate=database.owner_pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(987654321)").execute(&mut *gate).await?;
        let body=json!({"name":"cancelled"});
        let mut request=merchant_request(Method::POST,"/v1/api_keys",serde_json::to_vec(&body)?,&key);
        request.headers_mut().insert("idempotency-key","cancelled".parse()?);
        let running=tokio::spawn(harness.app.clone().oneshot(request));
        tokio::time::timeout(Duration::from_secs(10),async {
            loop {
                let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND NOT granted AND objid=987654321 AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))").fetch_one(&database.owner_pool).await?;
                if blocked {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        }).await??;
        running.abort();
        ensure!(running.await.unwrap_err().is_cancelled());
        gate.rollback().await?;
        // Drain the cancelled connection's queued rollback before inspecting the result.
        tokio::time::timeout(Duration::from_secs(10),async {
            loop {
                let locked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND objid=987654321 AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))").fetch_one(&database.owner_pool).await?;
                if !locked {break;}
                sqlx::query("SELECT 1").execute(pool).await?;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        }).await??;
        let after:i64=sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE account_id=$1").bind(account).fetch_one(pool).await?;
        ensure!(after==before,"cancelled business write committed");
        let response:Option<Value>=sqlx::query_scalar("SELECT response FROM idempotency_keys WHERE account_id=$1 AND key='cancelled'").bind(account).fetch_one(pool).await?;
        ensure!(response.is_none(),"cancelled response committed");
        sqlx::query("DROP FUNCTION pause_save() CASCADE").execute(&database.owner_pool).await?;
        sqlx::query("UPDATE idempotency_keys SET created_at=now()-interval '2 minutes' WHERE account_id=$1 AND key='cancelled'").bind(account).execute(pool).await?;
        let retried=harness.merchant(Method::POST,"/v1/api_keys",Some(&body),&key,Some("cancelled")).await?;
        ensure!(retried.status==StatusCode::OK,"{}",retried.body);
        let replay=harness.merchant(Method::POST,"/v1/api_keys",Some(&body),&key,Some("cancelled")).await?;
        ensure!(replay.headers["idempotent-replayed"]=="true");
        ensure!(replay.body["id"]==retried.body["id"]);
        let count:i64=sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE account_id=$1").bind(account).fetch_one(pool).await?;
        ensure!(count==before+1);
        Ok(())
    })).await
}

/// Authorization runs before the idempotency layer, as Stripe's: a restricted key cannot replay
/// the response to a request it may not make, and its `403` does not take the key, so the same
/// request by a secret key then runs.
#[tokio::test]
async fn authorization_runs_before_an_idempotent_replay() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(pool, RateLimits::default())?;
        let (_, key) = seed_test_account(pool, "authorized").await?;
        let restricted = harness
            .merchant(
                Method::POST,
                "/v1/api_keys",
                Some(&json!({"type": "restricted", "permissions": ["quotes.read"]})),
                &key,
                None,
            )
            .await?;
        ensure!(restricted.status == StatusCode::OK, "{}", restricted.body);
        let restricted = secret(&restricted.body)?;
        let create = |name: &'static str, key: String, idempotency_key: &'static str| {
            let harness = &harness;
            async move {
                harness
                    .merchant(
                        Method::POST,
                        "/v1/api_keys",
                        Some(&json!({"name": name})),
                        &key,
                        Some(idempotency_key),
                    )
                    .await
            }
        };

        // A `HEAD` needs what its `GET` needs.
        let head = harness
            .merchant(Method::HEAD, "/v1/account", None, &key, None)
            .await?;
        ensure!(head.status == StatusCode::OK, "{}", head.body);
        let head = harness
            .merchant(Method::HEAD, "/v1/api_keys", None, &restricted, None)
            .await?;
        ensure!(head.status == StatusCode::FORBIDDEN);

        let created = create("ci", key.clone(), "shared").await?;
        ensure!(created.status == StatusCode::OK, "{}", created.body);
        let refused = create("ci", restricted.clone(), "shared").await?;
        ensure!(refused.status == StatusCode::FORBIDDEN, "{}", refused.body);
        ensure!(refused.body["error"]["code"] == "permission_denied");
        ensure!(!refused.headers.contains_key("idempotent-replayed"));

        let denied = create("later", restricted, "fresh").await?;
        ensure!(denied.status == StatusCode::FORBIDDEN, "{}", denied.body);
        let ran = create("later", key.clone(), "fresh").await?;
        ensure!(ran.status == StatusCode::OK, "{}", ran.body);
        ensure!(!ran.headers.contains_key("idempotent-replayed"));
        ensure!(ran.body["name"] == "later" && ran.body.get("secret").is_some());
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Each account and mode has its own rate limit, and test mode shares a platform ceiling. The
/// harness's limiter clock stands still, so every request lands in the same instant: with limits
/// of a few per second, a real clock refills a token within the test's own requests.
#[tokio::test]
async fn requests_are_rate_limited_per_account_and_mode() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let harness = Harness::new(
            pool,
            RateLimits {
                live: 3,
                test: 2,
                test_platform: 3,
            },
        )?;
        let (account, test_key) = seed_test_account(pool, "limited").await?;
        sqlx::query("UPDATE accounts SET charges_enabled = true WHERE id = $1")
            .bind(account)
            .execute(pool)
            .await?;
        let live_key = seed::create_api_key(pool, account, true).await?;
        let (_, other_key) = seed_test_account(pool, "neighbour").await?;

        let mut statuses = Vec::new();
        for _ in 0..3 {
            statuses.push(harness.get("/v1/account", &test_key).await?.status);
        }
        ensure!(
            statuses
                == [
                    StatusCode::OK,
                    StatusCode::OK,
                    StatusCode::TOO_MANY_REQUESTS
                ],
            "{statuses:?}"
        );
        let limited = harness.get("/v1/account", &test_key).await?;
        ensure!(limited.body["error"]["code"] == "rate_limit");
        // The live mode of the same account has its own limit.
        for _ in 0..3 {
            ensure!(harness.get("/v1/account", &live_key).await?.status == StatusCode::OK);
        }
        ensure!(
            harness.get("/v1/account", &live_key).await?.status == StatusCode::TOO_MANY_REQUESTS
        );
        // Another test account has its own limit but shares the test-mode ceiling of 3, of
        // which the first account used 2.
        ensure!(harness.get("/v1/account", &other_key).await?.status == StatusCode::OK);
        ensure!(
            harness.get("/v1/account", &other_key).await?.status == StatusCode::TOO_MANY_REQUESTS
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Restricted keys (design PR 12, Stripe's restricted keys): a secret key creates one with a
/// subset of the grantable permissions, a `write` includes its `read`, and the key reaches only
/// what it holds. No grant lets it manage keys, treasuries, webhook endpoints, webhook keys, or
/// account settings, and it cannot create keys itself.
#[tokio::test]
async fn restricted_keys_hold_only_their_grants() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let limits = RateLimits {
            live: 1_000,
            test: 1_000,
            test_platform: 1_000,
        };
        let harness = Harness::new(pool, limits)?;
        let (account, key) = seed_test_account(pool, "restricted").await?;

        let create = |body: Value| {
            let harness = &harness;
            let key = key.clone();
            async move {
                harness
                    .merchant(Method::POST, "/v1/api_keys", Some(&body), &key, None)
                    .await
            }
        };
        for (body, param) in [
            (
                json!({"type": "restricted", "permissions": ["api_keys.write"]}),
                "permissions",
            ),
            (
                json!({"type": "restricted", "permissions": ["treasury.write"]}),
                "permissions",
            ),
            (
                json!({"type": "restricted", "permissions": ["endpoints.write"]}),
                "permissions",
            ),
            (
                json!({"type": "restricted", "permissions": ["account.write"]}),
                "permissions",
            ),
            (
                json!({"type": "restricted", "permissions": ["quotes.admin"]}),
                "permissions",
            ),
            (
                json!({"type": "restricted", "permissions": []}),
                "permissions",
            ),
            (json!({"type": "restricted"}), "permissions"),
            (json!({"permissions": ["quotes.read"]}), "permissions"),
            (json!({"type": "publishable"}), "type"),
        ] {
            let refused = create(body.clone()).await?;
            ensure!(
                refused.status == StatusCode::BAD_REQUEST,
                "{body}: {}",
                refused.body
            );
            ensure!(
                refused.body["error"]["param"] == param,
                "{body}: {}",
                refused.body
            );
        }

        let created = create(json!({
            "name": "checkout server",
            "type": "restricted",
            "permissions": ["quotes.write", "deposit_addresses.write", "deposits.read",
                            "events.read", "refunds.read", "account.read"],
        }))
        .await?;
        ensure!(created.status == StatusCode::OK, "{}", created.body);
        ensure!(created.body["type"] == "restricted");
        let runtime = secret(&created.body)?;
        ensure!(runtime.starts_with("ppay_rk_test_"));
        ensure!(
            api_keys::check_format(&runtime).map(|format| format.kind) == Some(KeyKind::Restricted)
        );
        ensure!(
            created.body["permissions"]
                == json!([
                    "account.read",
                    "deposit_addresses.read",
                    "deposit_addresses.write",
                    "deposits.read",
                    "events.read",
                    "quotes.read",
                    "quotes.write",
                    "refunds.read",
                ]),
            "{}",
            created.body
        );
        let runtime_id = created.body["id"].as_str().context("id")?.to_owned();

        // What the runtime key reaches.
        for path in [
            "/v1/account",
            "/v1/payment_settings",
            "/v1/config",
            "/v1/quotes",
            "/v1/deposit_addresses",
            "/v1/deposits",
            "/v1/refunds",
            "/v1/events",
        ] {
            let allowed = harness.get(path, &runtime).await?;
            ensure!(allowed.status == StatusCode::OK, "{path}: {}", allowed.body);
        }
        // What it does not: reads it was not granted, and every management write.
        let treasury = "trs_4d8a2c6e0b1f47a3c5e7d9b1a3c5e7f9";
        let denied: Vec<(Method, String, Option<Value>)> = vec![
            (Method::GET, "/v1/webhook_endpoints".to_owned(), None),
            (Method::GET, "/v1/treasuries".to_owned(), None),
            (Method::GET, "/v1/balance".to_owned(), None),
            (Method::GET, "/v1/api_keys".to_owned(), None),
            (
                Method::POST,
                "/v1/api_keys".to_owned(),
                Some(json!({"name": "escalate"})),
            ),
            (
                Method::POST,
                format!("/v1/api_keys/{runtime_id}/roll"),
                Some(json!({"expires_in": 0})),
            ),
            (Method::DELETE, format!("/v1/api_keys/{runtime_id}"), None),
            (
                Method::POST,
                "/v1/treasuries/challenge".to_owned(),
                Some(json!({"chain_id": 11_155_111,
                            "address": "0x936c1991f8da9a919fa11b557a3514719f5a4504"})),
            ),
            (
                Method::POST,
                format!("/v1/treasuries/{treasury}/cancel"),
                None,
            ),
            (
                Method::POST,
                format!("/v1/treasuries/{treasury}/pause"),
                None,
            ),
            (
                Method::POST,
                format!("/v1/treasuries/{treasury}/resume"),
                None,
            ),
            (
                Method::POST,
                "/v1/webhook_endpoints".to_owned(),
                Some(json!({"url": "https://attacker.example/hook",
                            "enabled_events": ["*"]})),
            ),
            (
                Method::POST,
                "/v1/account/webhook_keys/roll".to_owned(),
                Some(json!({"expires_in": 0})),
            ),
            // A restricted key reads the payment settings but never widens what is accepted.
            (
                Method::POST,
                "/v1/payment_settings".to_owned(),
                Some(json!({"chains": []})),
            ),
            (
                Method::POST,
                "/v1/account/pause".to_owned(),
                Some(json!({"scopes": ["quotes"]})),
            ),
            (
                Method::POST,
                "/v1/refunds".to_owned(),
                Some(json!({"deposit": "dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516",
                            "destination_address": "0x0f45147a02e4c9d91aff20024e22095536fd5053"})),
            ),
        ];
        for (method, path, body) in denied {
            let refused = harness
                .merchant(method.clone(), &path, body.as_ref(), &runtime, None)
                .await?;
            ensure!(
                refused.status == StatusCode::FORBIDDEN
                    && refused.body["error"]["code"] == "permission_denied",
                "{method} {path}: {} {}",
                refused.status,
                refused.body
            );
        }

        // A grantable read is reachable once granted.
        let reader =
            create(json!({"type": "restricted", "permissions": ["endpoints.read"]})).await?;
        ensure!(reader.status == StatusCode::OK, "{}", reader.body);
        let reader = secret(&reader.body)?;
        ensure!(harness.get("/v1/webhook_endpoints", &reader).await?.status == StatusCode::OK);
        ensure!(harness.get("/v1/quotes", &reader).await?.status == StatusCode::FORBIDDEN);

        // Rolling a restricted key keeps its kind and grants; it never counts as the mode's
        // last lasting key, so the secret key cannot be revoked in its favour.
        let rolled = harness
            .merchant(
                Method::POST,
                &format!("/v1/api_keys/{runtime_id}/roll"),
                Some(&json!({"expires_in": 0})),
                &key,
                None,
            )
            .await?;
        ensure!(rolled.status == StatusCode::OK, "{}", rolled.body);
        ensure!(rolled.body["type"] == "restricted");
        ensure!(rolled.body["permissions"] == created.body["permissions"]);
        ensure!(secret(&rolled.body)?.starts_with("ppay_rk_test_"));
        let secret_id = harness.get("/v1/api_keys", &key).await?.body["data"]
            .as_array()
            .context("data")?
            .iter()
            .find(|listed| listed["type"] == "secret")
            .and_then(|listed| listed["id"].as_str())
            .context("secret key id")?
            .to_owned();
        let last = harness
            .merchant(
                Method::DELETE,
                &format!("/v1/api_keys/{secret_id}"),
                None,
                &key,
                None,
            )
            .await?;
        ensure!(
            last.body["error"]["code"] == "last_api_key",
            "{}",
            last.body
        );

        let created_events = events(pool, account)
            .await?
            .into_iter()
            .filter(|(kind, _, _)| kind == "api_key.created")
            .count();
        ensure!(created_events == 3, "{created_events}");
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn anonymous_slow_bodies_do_not_shed_database_health_checks() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            use std::future::Future as _;
            use std::task::Poll;
            let harness = Harness::new(&database.app_pool, RateLimits::default())?;
            let stalled = || {
                axum::http::Request::post("/v1/admin/accounts")
                    .body(axum::body::Body::from_stream(
                        futures_util::stream::pending::<Result<axum::body::Bytes, std::io::Error>>(
                        ),
                    ))
                    .unwrap()
            };
            let mut requests = (0..256)
                .map(|_| Box::pin(harness.app.clone().oneshot(stalled())))
                .collect::<Vec<_>>();
            std::future::poll_fn(|cx| {
                for request in &mut requests {
                    assert!(
                        request.as_mut().poll(cx).is_pending(),
                        "slow body unexpectedly completed before saturation"
                    );
                }
                Poll::Ready(())
            })
            .await;
            let busy = harness.app.clone().oneshot(stalled()).await?;
            ensure!(busy.status() == StatusCode::SERVICE_UNAVAILABLE);
            let started = std::time::Instant::now();
            let health = harness
                .app
                .oneshot(axum::http::Request::get("/healthz").body(axum::body::Body::empty())?)
                .await?;
            ensure!(
                health.status() == StatusCode::OK,
                "health was shed by anonymous work"
            );
            println!(
                "SLOW BODY admitted=256 health_status=200 health_elapsed={:?}",
                started.elapsed()
            );
            drop(requests);
            Ok(())
        })
    })
    .await
}
