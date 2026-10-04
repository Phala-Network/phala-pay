//! In-process C9 API tests backed by PostgreSQL.

mod support;

use std::str::FromStr;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use axum::body::{Body, to_bytes};
use axum::http::{Method, StatusCode};
use base64::Engine as _;
use chrono::{Duration, Utc};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use sqlx::Row;
use topup::api::{
    AppState, AttestationEvidence, AttestationFuture, AttestationRequest, Attestor, PublicOrigin,
    VerificationKey,
};
use topup::db::{Account, NewDeposit};
use topup_adapters::attestation::{AttestedWebhookKey, DstackAttestor, report_data};
use topup_core::deposit::DepositState;
use topup_core::identity::deposit_id;
use topup_core::money::AtomicAmount;
use topup_core::route::RouteFile;
use topup_core::{Ed25519PublicKey, WebhookKeyId};
use tower::ServiceExt;
use uuid::Uuid;

use support::seed::{self, NewAccount, NewAddress, NewCustomer};
use support::{
    SignatureOptions, SignatureParameter, TEST_ORIGIN, TestDatabase, merchant_request,
    public_key_base64, signed_request, signed_request_with_options,
};

const ADMIN_KID: &str = "admin/v1";

/// The admin API keeps RFC 9421 request signatures (design D7).
#[tokio::test]
async fn admin_signature_verification_vectors() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[9; 32]);
        let app = test_router(&database.app_pool, &admin_key);
        let path = "/v1/admin/reports/daily".to_owned();
        let body = serde_json::to_vec(&json!({"external_id": "signed-account"}))?;
        let now = Utc::now().timestamp();

        let response = app
            .clone()
            .oneshot(signed_request_with_options(
                Method::GET,
                &path,
                body.clone(),
                ADMIN_KID,
                &admin_key,
                now,
                &SignatureOptions {
                    parameters: vec![SignatureParameter::Created, SignatureParameter::KeyId],
                    ..SignatureOptions::default()
                },
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);

        let response = app
            .clone()
            .oneshot(signed_request_with_options(
                Method::GET,
                &path,
                serde_json::to_vec(&json!({"external_id": "with-idempotency"}))?,
                ADMIN_KID,
                &admin_key,
                now,
                &SignatureOptions {
                    idempotency_key: Some("\"deposit:test\"".to_owned()),
                    ..SignatureOptions::default()
                },
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);

        let response = app
            .clone()
            .oneshot(signed_request(
                Method::GET,
                &path,
                serde_json::to_vec(&json!({"external_id": "with-alg"}))?,
                ADMIN_KID,
                &admin_key,
                now,
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);

        let response = app
            .clone()
            .oneshot(signed_request_with_options(
                Method::GET,
                &path,
                serde_json::to_vec(&json!({"external_id": "reordered-parameters"}))?,
                ADMIN_KID,
                &admin_key,
                now,
                &SignatureOptions {
                    parameters: vec![
                        SignatureParameter::Algorithm("ed25519".to_owned()),
                        SignatureParameter::KeyId,
                        SignatureParameter::Created,
                    ],
                    ..SignatureOptions::default()
                },
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);

        let response = app
            .clone()
            .oneshot(signed_request_with_options(
                Method::GET,
                &path,
                serde_json::to_vec(&json!({"external_id": "different-label"}))?,
                ADMIN_KID,
                &admin_key,
                now,
                &SignatureOptions {
                    label: "checkout".to_owned(),
                    ..SignatureOptions::default()
                },
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);

        let origin_path = format!("{path}?failing_for_hours=48");
        let response = app
            .clone()
            .oneshot(signed_request_with_options(
                Method::GET,
                &origin_path,
                serde_json::to_vec(&json!({"external_id": "origin-form-query"}))?,
                ADMIN_KID,
                &admin_key,
                now,
                &SignatureOptions {
                    origin_form: true,
                    ..SignatureOptions::default()
                },
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);

        let response = app
            .clone()
            .oneshot(signed_request_with_options(
                Method::GET,
                &path,
                serde_json::to_vec(&json!({"external_id": "wrong-algorithm"}))?,
                ADMIN_KID,
                &admin_key,
                now,
                &SignatureOptions {
                    parameters: vec![
                        SignatureParameter::Created,
                        SignatureParameter::KeyId,
                        SignatureParameter::Algorithm("rsa-pss-sha512".to_owned()),
                    ],
                    ..SignatureOptions::default()
                },
            ))
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);

        let wrong_key = SigningKey::from_bytes(&[8; 32]);
        let response = app
            .clone()
            .oneshot(signed_request(
                Method::GET,
                &path,
                body.clone(),
                ADMIN_KID,
                &wrong_key,
                now,
            ))
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);

        let response = app
            .clone()
            .oneshot(signed_request(
                Method::GET,
                &path,
                serde_json::to_vec(&json!({"external_id": "future-created"}))?,
                ADMIN_KID,
                &admin_key,
                (Utc::now() + Duration::minutes(6)).timestamp(),
            ))
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);

        let mut tampered = signed_request(
            Method::GET,
            &path,
            body.clone(),
            ADMIN_KID,
            &admin_key,
            now,
        );
        *tampered.body_mut() = Body::from(r#"{"external_id":"tampered"}"#);
        let response = app.clone().oneshot(tampered).await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);

        let response = app
            .clone()
            .oneshot(signed_request(
                Method::GET,
                &path,
                body.clone(),
                ADMIN_KID,
                &admin_key,
                (Utc::now() - Duration::minutes(6)).timestamp(),
            ))
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);

        let mut missing_component = signed_request(
            Method::GET,
            &path,
            serde_json::to_vec(&json!({"external_id": "missing-component"}))?,
            ADMIN_KID,
            &admin_key,
            now,
        );
        missing_component.headers_mut().insert(
            "signature-input",
            format!(
                "sig1=(\"@method\" \"@target-uri\");created={now};keyid=\"{ADMIN_KID}\";alg=\"ed25519\""
            )
            .parse()?,
        );
        let response = app.clone().oneshot(missing_component).await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);

        let replay_body = serde_json::to_vec(&json!({"external_id": "replay"}))?;
        let response = app
            .clone()
            .oneshot(signed_request(
                Method::GET,
                &path,
                replay_body.clone(),
                ADMIN_KID,
                &admin_key,
                now,
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let response = app
            .oneshot(signed_request(
                Method::GET,
                &path,
                replay_body,
                ADMIN_KID,
                &admin_key,
                now,
            ))
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);
        ensure!(response_json(response).await?["error"]["code"] == "signature_replayed");
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Behind the gateway `Host` and `X-Forwarded-*` describe the internal hop; only the configured
/// public origin determines an admin request's `@target-uri`.
#[tokio::test]
async fn admin_target_uri_uses_the_configured_public_origin() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[9; 32]);
        let app = test_router(&database.app_pool, &admin_key);
        let path = "/v1/admin/reports/daily".to_owned();
        let now = Utc::now().timestamp();
        let request = |external_id: &str, origin: &str| -> Result<_> {
            let mut request = signed_request_with_options(
                Method::GET,
                &path,
                serde_json::to_vec(&json!({"external_id": external_id}))?,
                ADMIN_KID,
                &admin_key,
                now,
                &SignatureOptions {
                    origin_form: true,
                    origin: origin.to_owned(),
                    ..SignatureOptions::default()
                },
            );
            let headers = request.headers_mut();
            headers.insert("host", "topup-internal:8080".parse()?);
            headers.insert("x-forwarded-proto", "https".parse()?);
            headers.insert("x-forwarded-host", "attacker.example".parse()?);
            Ok(request)
        };

        let response = app
            .clone()
            .oneshot(request("public-origin", TEST_ORIGIN)?)
            .await?;
        ensure!(response.status() == StatusCode::OK);

        for (external_id, origin) in [
            ("internal-host", "http://topup-internal:8080"),
            ("forwarded-proto", "https://api.test"),
            ("forwarded-host", "https://attacker.example"),
            ("other-port", "http://api.test:8080"),
        ] {
            let response = app.clone().oneshot(request(external_id, origin)?).await?;
            ensure!(
                response.status() == StatusCode::UNAUTHORIZED,
                "a signature for {origin} must not verify"
            );
        }

        // A signature for one path must not authorize a request to another route.
        let mut tampered = request("tampered-path", TEST_ORIGIN)?;
        *tampered.uri_mut() = "/v1/admin/deposits/dep_00000000000000000000000000000000".parse()?;
        let response = app.clone().oneshot(tampered).await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn tenant_isolation_and_operator_pauses() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[19; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        let (other, _) = seed_product(&database.app_pool, "builder").await?;
        let app = test_router(&database.app_pool, &admin_key);
        let now = Utc::now().timestamp();

        let customer_id = seed_customer(&database.app_pool, product.id, "account-001")
            .await?
            .id;

        let other_deposit = seed_other_tenant_deposit(&database.app_pool, other.id).await?;
        let cross_tenant_path = format!("/v1/deposits/dep_{}", other_deposit.simple());
        let response = app
            .clone()
            .oneshot(merchant_request(
                Method::GET,
                &cross_tenant_path,
                Vec::new(),
                &product_key,
            ))
            .await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);

        // Pausing a customer is an operator action.
        let pause_path = format!(
            "/v1/admin/accounts/{}/customers/account-001/pause",
            product.public_id
        );
        let pause_body =
            serde_json::to_vec(&json!({"scopes": ["quotes", "settlement"], "livemode": true}))?;
        let response = app
            .clone()
            .oneshot(merchant_request(
                Method::POST,
                &pause_path,
                pause_body.clone(),
                &product_key,
            ))
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);
        let response = app
            .clone()
            .oneshot(signed_request(
                Method::POST,
                &pause_path,
                pause_body,
                ADMIN_KID,
                &admin_key,
                now,
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let paused = response_json(response).await?;
        ensure!(paused["paused_scopes"] == json!(["quotes", "settlement"]));
        let stored_scopes: Vec<String> =
            sqlx::query_scalar("SELECT paused_scopes FROM customers WHERE id = $1")
                .bind(customer_id)
                .fetch_one(&database.app_pool)
                .await?;
        ensure!(stored_scopes == ["quotes", "settlement"]);
        let audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit WHERE subject = $1")
            .bind(format!("customer:{customer_id}"))
            .fetch_one(&database.app_pool)
            .await?;
        ensure!(audit_count == 1);

        // The paused customer gets no quote.
        let paused_quote = app
            .clone()
            .oneshot(merchant_request(
                Method::POST,
                "/v1/quotes",
                serde_json::to_vec(&json!({
                    "client_reference_id": "account-001", "amount": 1000, "currency": "usd",
                    "chain_id": 1, "asset": "pha",
                }))?,
                &product_key,
            ))
            .await?;
        ensure!(paused_quote.status() == StatusCode::BAD_REQUEST);
        ensure!(response_json(paused_quote).await?["error"]["code"] == "paused");

        let admin_path = "/v1/admin/routes/phala-cloud-ethereum-pha-usd/pause";
        let admin_body = serde_json::to_vec(&json!({"scopes": ["refunds"]}))?;
        let product_signed = app
            .clone()
            .oneshot(merchant_request(
                Method::POST,
                admin_path,
                admin_body.clone(),
                &product_key,
            ))
            .await?;
        ensure!(product_signed.status() == StatusCode::UNAUTHORIZED);
        let admin_signed = app
            .clone()
            .oneshot(signed_request(
                Method::POST,
                admin_path,
                admin_body,
                ADMIN_KID,
                &admin_key,
                now,
            ))
            .await?;
        ensure!(admin_signed.status() == StatusCode::OK);
        // The service sends no transactions, so there is no `flush` scope to pause.
        let removed_scope = app
            .clone()
            .oneshot(signed_request(
                Method::POST,
                admin_path,
                serde_json::to_vec(&json!({"scopes": ["flush"]}))?,
                ADMIN_KID,
                &admin_key,
                now + 1,
            ))
            .await?;
        ensure!(removed_scope.status() == StatusCode::BAD_REQUEST);
        let admin_audit_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit WHERE subject = $1")
                .bind("route:phala-cloud-ethereum-pha-usd")
                .fetch_one(&database.app_pool)
                .await?;
        ensure!(admin_audit_count == 1);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Every merchant endpoint that names an object answers `404` to another account's key and to
/// the owning account's key in the other mode, exactly as for an object that does not exist, and
/// lists show neither (design D13: the scope comes from the credential, never the request).
#[tokio::test]
async fn every_merchant_endpoint_is_404_across_accounts_and_modes() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let admin_key = SigningKey::from_bytes(&[23; 32]);
        let (owner, owner_key) = seed_product(pool, "owner").await?;
        let (_, other_key) = seed_product(pool, "other").await?;
        // The owner's own key of the other mode.
        let owner_test_key = seed::create_api_key(pool, owner.id, false).await?;
        let owner_key_id: Uuid = sqlx::query_scalar(
            "SELECT id FROM api_keys WHERE account_id = $1 AND livemode ORDER BY created_at LIMIT 1",
        )
        .bind(owner.id)
        .fetch_one(pool)
        .await?;
        let customer = seed_customer(pool, owner.id, "owned-customer").await?;
        let address = seed::insert_address(
            pool,
            &NewAddress {
                id: Uuid::new_v4(),
                customer_id: customer.id,
                chain_id: 1,
                route: "phala-cloud-ethereum-pha-usd".to_owned(),
                salt: B256::repeat_byte(0x61),
                address: Address::repeat_byte(0x62),
            },
        )
        .await?;
        let tx_hash = B256::repeat_byte(0x63);
        ensure!(
            topup::db::insert_deposit(
                pool,
                &NewDeposit {
                    chain_id: 1,
                    tx_hash,
                    log_index: 0,
                    receipt_log_index: 0,
                    tx_from: Address::ZERO,
                    tx_nonce: 0,
                    is_final: true,
                    block_number: 5,
                    block_hash: B256::repeat_byte(0x64),
                    block_time: Utc::now(),
                    address_id: address.id,
                    route: Some("phala-cloud-ethereum-pha-usd".to_owned()),
                    route_version: Some(1),
                    asset_contract: Address::repeat_byte(0x65),
                    from_address: Address::repeat_byte(0x66),
                    amount_atomic: AtomicAmount::new(U256::from(1_000_u64)),
                    state: DepositState::Rejected,
                    reason: Some(topup_core::deposit::RejectReason::OutOfBounds),
                    next_attempt_at: Utc::now() + Duration::hours(1),
                },
            )
            .await?
        );
        let deposit = deposit_id(1, tx_hash, 0);
        // One refund to mark paid, and one to cancel: a refund marked paid cannot be canceled.
        let (refund, unpaid_refund) = (Uuid::new_v4(), Uuid::new_v4());
        for id in [refund, unpaid_refund] {
            sqlx::query(
                r#"
                INSERT INTO refunds (id, account_id, livemode, chain_id, deposit_id, amount_atomic,
                                     destination_address, status)
                SELECT $1, account_id, livemode, chain_id, id, 10, $3, 'pending'
                FROM deposits WHERE id = $2
                "#,
            )
            .bind(id)
            .bind(deposit)
            .bind(format!("{:#x}", Address::repeat_byte(0x67)))
            .execute(pool)
            .await?;
        }

        let quote = topup::ids::format(topup::ids::QUOTE, address.quote_id.context("quote address")?);
        let deposit = topup::ids::format(topup::ids::DEPOSIT, deposit);
        let refund = topup::ids::format(topup::ids::REFUND, refund);
        let unpaid_refund = topup::ids::format(topup::ids::REFUND, unpaid_refund);
        let api_key = topup::ids::format(topup::ids::API_KEY, owner_key_id);
        let refund_body = serde_json::to_vec(&json!({
            "deposit": deposit,
            "destination_address": format!("{:#x}", Address::repeat_byte(0x68)),
            "amount_atomic": "10",
        }))?;
        let metadata_body = serde_json::to_vec(&json!({ "metadata": { "owner": "yes" } }))?;
        let object_requests = [
            (Method::GET, format!("/v1/quotes/{quote}"), Vec::new()),
            (
                Method::GET,
                format!("/v1/quotes/{quote}?expand[]=deposit"),
                Vec::new(),
            ),
            (
                Method::POST,
                format!("/v1/quotes/{quote}/cancel"),
                Vec::new(),
            ),
            (Method::GET, format!("/v1/deposits/{deposit}"), Vec::new()),
            (
                Method::GET,
                format!("/v1/deposits/{deposit}?expand[]=quote"),
                Vec::new(),
            ),
            (Method::GET, format!("/v1/refunds/{refund}"), Vec::new()),
            (
                Method::GET,
                format!("/v1/refunds/{refund}?expand[]=deposit"),
                Vec::new(),
            ),
            (
                Method::POST,
                format!("/v1/refunds/{refund}/mark_paid"),
                serde_json::to_vec(
                    &json!({"transaction_hash": format!("{:#x}", B256::repeat_byte(0x69))}),
                )?,
            ),
            (
                Method::POST,
                format!("/v1/refunds/{unpaid_refund}/cancel"),
                Vec::new(),
            ),
            (Method::POST, "/v1/refunds".to_owned(), refund_body),
            (Method::GET, format!("/v1/api_keys/{api_key}"), Vec::new()),
            (
                Method::POST,
                format!("/v1/quotes/{quote}"),
                metadata_body.clone(),
            ),
            (
                Method::POST,
                format!("/v1/deposits/{deposit}"),
                metadata_body.clone(),
            ),
            (
                Method::POST,
                format!("/v1/refunds/{refund}"),
                metadata_body.clone(),
            ),
        ];
        // Key mutations the owner is not asked to make: another tenant must not reach them.
        let key_mutations = [
            (
                Method::POST,
                format!("/v1/api_keys/{api_key}/roll"),
                serde_json::to_vec(&json!({"expires_in": 60}))?,
            ),
            (Method::DELETE, format!("/v1/api_keys/{api_key}"), Vec::new()),
        ];
        let lists = [
            "/v1/deposits".to_owned(),
            format!("/v1/deposits?quote={quote}"),
            "/v1/deposits?client_reference_id=owned-customer".to_owned(),
        ];
        let app = test_router(pool, &admin_key);
        let call = |method: Method, path: &str, body: Vec<u8>, key: &str| {
            let request = merchant_request(method, path, body, key);
            let app = app.clone();
            async move {
                let response = app.oneshot(request).await?;
                let status = response.status();
                anyhow::Ok((status, response_json(response).await?))
            }
        };

        // The owner reaches every one of its objects; its refund request succeeds.
        for (method, path, body) in &object_requests {
            let (status, answer) =
                call(method.clone(), path, body.clone(), &owner_key).await?;
            ensure!(
                status == StatusCode::OK,
                "owner {method} {path}: {status} {answer}"
            );
        }
        for path in &lists {
            let (status, answer) =
                call(Method::GET, path, Vec::new(), &owner_key).await?;
            ensure!(status == StatusCode::OK && answer["data"].as_array().map(Vec::len) == Some(1));
        }

        // Another account, and the owner's own key of the other mode, see nothing.
        for (who, key) in [
            ("other account", &other_key),
            ("other mode", &owner_test_key),
        ] {
            for (method, path, body) in object_requests.iter().chain(&key_mutations) {
                let (status, answer) = call(method.clone(), path, body.clone(), key).await?;
                ensure!(
                    status == StatusCode::NOT_FOUND
                        && answer["error"]["code"] == "resource_missing",
                    "{who} {method} {path}: {status} {answer}"
                );
            }
            for path in &lists {
                let (status, answer) = call(Method::GET, path, Vec::new(), key).await?;
                ensure!(
                    status == StatusCode::OK && answer["data"] == json!([]),
                    "{who} {path}: {status} {answer}"
                );
            }
            // A cursor naming a foreign deposit fails exactly like one naming no deposit.
            let (status, foreign) = call(
                Method::GET,
                &format!("/v1/deposits?starting_after={deposit}"),
                Vec::new(),
                key,
            )
            .await?;
            let unknown = topup::ids::format(topup::ids::DEPOSIT, Uuid::new_v4());
            let (unknown_status, missing) = call(
                Method::GET,
                &format!("/v1/deposits?starting_after={unknown}"),
                Vec::new(),
                key,
            )
            .await?;
            ensure!(
                status == unknown_status && foreign == missing,
                "{who}: {foreign}"
            );
        }
        // Nothing was written for the other tenants: the owner's objects are untouched.
        let refunds: i64 = sqlx::query_scalar("SELECT count(*) FROM refunds")
            .fetch_one(pool)
            .await?;
        ensure!(
            refunds == 3,
            "only the owner's own request created a refund"
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// An instance restored from backup serves reads only, and its `/healthz` carries the boot-time
/// restore-check report (`deploy/RESTORE.md`).
#[tokio::test]
async fn read_only_router_refuses_writes_and_reports_the_restore_check() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let report = std::env::temp_dir().join(format!(
        "topup-api-restore-check-{}.json",
        std::process::id()
    ));
    let result = async {
        let admin_key = SigningKey::from_bytes(&[44; 32]);
        let app = topup::api::read_only_router(
            app_state(database.app_pool.clone(), &admin_key),
            Some(report.clone()),
        );
        let healthz = || -> Result<_> {
            Ok(axum::http::Request::builder()
                .uri("/healthz")
                .body(Body::empty())?)
        };

        let response = app.clone().oneshot(healthz()?).await?;
        ensure!(response.status() == StatusCode::OK);
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        ensure!(
            body == json!({"mode": "read-only", "restore_check": null}),
            "{body}"
        );

        std::fs::write(&report, r#"{"status":"ok","rpo_basis":"unanchored"}"#)?;
        let response = app.clone().oneshot(healthz()?).await?;
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        ensure!(body["restore_check"]["status"] == "ok", "{body}");

        let response = app
            .clone()
            .oneshot(signed_request(
                Method::POST,
                "/v1/admin/accounts",
                serde_json::to_vec(&json!({
                    "name": "Phala Cloud",
                    "contact": {"name": "Ops", "email": "ops@product.test"},
                    "due_diligence": {"reference": "DD-1", "reviewed_at": "2026-09-28",
                                      "reviewed_by": "operator"},
                    "reason": "onboarding",
                }))?,
                ADMIN_KID,
                &admin_key,
                Utc::now().timestamp(),
            ))
            .await?;
        ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
        ensure!(response.headers().contains_key("retry-after"));
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        ensure!(body["error"]["code"] == "service_restoring", "{body}");
        let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
            .fetch_one(&database.app_pool)
            .await?;
        ensure!(accounts == 0);
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&report);
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn frozen_chain_refuses_quotes() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[32; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        seed_customer(&database.app_pool, product.id, "frozen-account").await?;
        sqlx::query(
            r#"
            INSERT INTO reconciliation_blocks (block_key, scope, chain_id, check_name, reason)
            VALUES ('chain:1', 'chain', 1, 'address_derivation', 'test freeze')
            "#,
        )
        .execute(&database.app_pool)
        .await?;

        let app = test_router(&database.app_pool, &admin_key);
        let lock_body = serde_json::to_vec(&json!({
            "client_reference_id": "frozen-account", "amount": 1000, "currency": "usd",
            "chain_id": 1, "asset": "pha",
        }))?;
        let response = app
            .oneshot(merchant_request(
                Method::POST,
                "/v1/quotes",
                lock_body,
                &product_key,
            ))
            .await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
        ensure!(response_json(response).await?["error"]["code"] == "chain_frozen");
        // A refused creation leaves no lock or lock address behind.
        let leftovers: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM quotes) + (SELECT count(*) FROM addresses)",
        )
        .fetch_one(&database.app_pool)
        .await?;
        ensure!(leftovers == 0);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// `POST /v1/admin/reconciliation_blocks/{block_key}/lift` is admin-signed, needs a reason,
/// audits the lift with the block it removed, and answers a repeat with the first lift.
#[tokio::test]
async fn admin_lift_unfreezes_a_chain_once() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[34; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        seed_customer(&database.app_pool, product.id, "lift-account").await?;
        sqlx::query(
            r#"
            INSERT INTO reconciliation_blocks (block_key, scope, chain_id, check_name, reason)
            VALUES ('chain:1', 'chain', 1, 'address_derivation', 'test freeze')
            "#,
        )
        .execute(&database.app_pool)
        .await?;
        let app = test_router(&database.app_pool, &admin_key);
        let now = Utc::now().timestamp();
        let admin = |method: Method, path: &str, body: Value, created: i64| -> Result<_> {
            Ok(signed_request(
                method,
                path,
                if body.is_null() {
                    Vec::new()
                } else {
                    serde_json::to_vec(&body)?
                },
                ADMIN_KID,
                &admin_key,
                created,
            ))
        };

        let response = app
            .clone()
            .oneshot(admin(
                Method::GET,
                "/v1/admin/reports/daily",
                Value::Null,
                now,
            )?)
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let blocks = response_json(response).await?["reconciliation_blocks"].clone();
        ensure!(blocks.as_array().map(Vec::len) == Some(1));
        ensure!(blocks[0]["block_key"] == "chain:1");
        ensure!(blocks[0]["scope"] == "chain");
        ensure!(blocks[0]["check"] == "address_derivation");

        let lift = "/v1/admin/reconciliation_blocks/chain:1/lift";
        let reason = json!({"reason": "INC-7: factory confirmed, stored rows restored"});
        let response = app
            .clone()
            .oneshot(merchant_request(
                Method::POST,
                lift,
                serde_json::to_vec(&reason)?,
                &product_key,
            ))
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);
        let response = app
            .clone()
            .oneshot(admin(Method::POST, lift, json!({"reason": " "}), now + 2)?)
            .await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
        ensure!(response_json(response).await?["error"]["code"] == "parameter_invalid");
        let response = app
            .clone()
            .oneshot(admin(
                Method::POST,
                "/v1/admin/reconciliation_blocks/chain:2/lift",
                reason.clone(),
                now + 3,
            )?)
            .await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
        ensure!(response_json(response).await?["error"]["code"] == "resource_missing");

        let response = app
            .clone()
            .oneshot(admin(Method::POST, lift, reason.clone(), now + 4)?)
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let lifted = response_json(response).await?;
        ensure!(lifted["block_key"] == "chain:1");
        // A repeat answers with the first lift; generated clients percent-encode the key.
        let response = app
            .clone()
            .oneshot(admin(
                Method::POST,
                "/v1/admin/reconciliation_blocks/chain%3A1/lift",
                reason.clone(),
                now + 5,
            )?)
            .await?;
        ensure!(response.status() == StatusCode::OK);
        ensure!(response_json(response).await? == lifted);

        let audit = sqlx::query(
            "SELECT actor_type, actor_id, action, reason FROM audit \
             WHERE subject = 'reconciliation_block:chain:1'",
        )
        .fetch_all(&database.app_pool)
        .await?;
        ensure!(audit.len() == 1, "only the first lift is audited");
        ensure!(audit[0].try_get::<String, _>("actor_type")? == "admin");
        ensure!(audit[0].try_get::<String, _>("actor_id")? == ADMIN_KID);
        ensure!(audit[0].try_get::<String, _>("action")? == "reconciliation_block.lift");
        let evidence: Value = serde_json::from_str(&audit[0].try_get::<String, _>("reason")?)?;
        ensure!(evidence["reason"] == reason["reason"]);
        ensure!(evidence["block"]["check"] == "address_derivation");
        ensure!(evidence["block"]["reason"] == "test freeze");

        // The chain resumes without a restart.
        // Quote creation passes the frozen-chain check and reaches pricing, which the test
        // router does not configure.
        let response = app
            .clone()
            .oneshot(merchant_request(
                Method::POST,
                "/v1/quotes",
                serde_json::to_vec(&json!({
                    "client_reference_id": "lift-account", "amount": 1000, "currency": "usd",
                    "chain_id": 1, "asset": "pha",
                }))?,
                &product_key,
            ))
            .await?;
        ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
        let response = app
            .oneshot(admin(
                Method::GET,
                "/v1/admin/reports/daily",
                Value::Null,
                now + 7,
            )?)
            .await?;
        ensure!(response_json(response).await?["reconciliation_blocks"] == json!([]));
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// `GET /v1/admin/metrics` serves the RPC call counters to the admin key only, in the
/// Prometheus text format.
#[tokio::test]
async fn admin_metrics_serve_rpc_call_counters_to_the_admin_only() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[38; 32]);
        let (_, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        let app = test_router(&database.app_pool, &admin_key);
        let now = Utc::now().timestamp();
        let path = "/v1/admin/metrics";

        let response = app
            .clone()
            .oneshot(merchant_request(
                Method::GET,
                path,
                Vec::new(),
                &product_key,
            ))
            .await?;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(signed_request(
                Method::GET,
                path,
                Vec::new(),
                ADMIN_KID,
                &admin_key,
                now + 1,
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);
        ensure!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/plain; version=0.0.4"))
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
        let text = String::from_utf8(body.to_vec())?;
        ensure!(
            text.contains("# TYPE topup_http_requests_total counter"),
            "{text}"
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn attestation_binds_the_callers_account_keys_and_needs_a_key() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let admin_key = SigningKey::from_bytes(&[30; 32]);
        let seed = [1; 32];
        let app = topup::api::router(app_state_with_attestor(
            pool.clone(),
            &admin_key,
            Arc::new(TestAttestor(seed)),
        ))
        .0;
        let (owner, live_key) = seed_product(pool, "attested").await?;
        let test_key = seed::create_api_key(pool, owner.id, false).await?;
        let (other, other_key) = seed_product(pool, "other").await?;
        let path = "/v1/attestation?nonce=00010203";
        let attest = |key: String| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(merchant_request(Method::GET, path, Vec::new(), &key))
                    .await?;
                ensure!(response.status() == StatusCode::OK, "{}", response.status());
                response_json(response).await
            }
        };
        let check = |response: &Value, account: &Account, livemode: bool, versions: &[u32]| {
            let keys: Vec<AttestedWebhookKey> = versions
                .iter()
                .map(|&version| AttestedWebhookKey {
                    version,
                    public_key: TestAttestor::public_key(
                        seed,
                        &account.public_id,
                        livemode,
                        version,
                    ),
                })
                .collect();
            let listed: Vec<Value> = keys
                .iter()
                .map(|key| {
                    json!({
                        "version": key.version,
                        "public_key": format!(
                            "whpk_{}",
                            base64::engine::general_purpose::STANDARD.encode(key.public_key.0)
                        ),
                    })
                })
                .collect();
            let returned: Vec<Value> = response["webhook_keys"]
                .as_array()
                .context("webhook keys")?
                .iter()
                .map(|key| {
                    json!({
                        "version": key["version"],
                        "public_key": key["public_key"],
                    })
                })
                .collect();
            let expected = report_data(&[0, 1, 2, 3], &account.public_id, livemode, &keys)
                .context("report data")?;
            ensure!(response["object"] == "attestation" && response["tdx_quote"] == "");
            ensure!(response["account"] == account.public_id.as_str());
            ensure!(response["livemode"] == livemode);
            ensure!(returned == listed, "{response}");
            ensure!(response["report_data"] == hex::encode(expected));
            Ok(())
        };

        let live = attest(live_key.clone()).await?;
        check(&live, &owner, true, &[1])?;
        let test = attest(test_key.clone()).await?;
        check(&test, &owner, false, &[1])?;
        let other_account = attest(other_key).await?;
        check(&other_account, &other, true, &[1])?;
        // Every account and mode has its own key.
        let public_keys = [&live, &test, &other_account]
            .map(|response| response["webhook_keys"][0]["public_key"].clone());
        ensure!(public_keys[0] != public_keys[1] && public_keys[0] != public_keys[2]);

        // Without a key, or with an unknown one, there is no attestation.
        let anonymous = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(Body::empty())?,
            )
            .await?;
        ensure!(anonymous.status() == StatusCode::UNAUTHORIZED);
        let forged = app
            .clone()
            .oneshot(merchant_request(
                Method::GET,
                path,
                Vec::new(),
                "ppay_sk_live_0000000000000000000000000000000000000000000",
            ))
            .await?;
        ensure!(forged.status() == StatusCode::UNAUTHORIZED);

        // A roll of the live key signs with both until the old one expires; test mode is apart.
        let roll = |expires_in: u32| {
            let app = app.clone();
            let live_key = live_key.clone();
            async move {
                app.oneshot(merchant_request(
                    Method::POST,
                    "/v1/account/webhook_keys/roll",
                    serde_json::to_vec(&json!({ "expires_in": expires_in }))?,
                    &live_key,
                ))
                .await
                .map_err(anyhow::Error::from)
            }
        };
        let too_long = roll(604_801).await?;
        ensure!(too_long.status() == StatusCode::BAD_REQUEST);
        // A live roll keeps the old key for at least the 48-hour treasury time-lock.
        for too_short in [0, 3600, 172_799] {
            let refused = roll(too_short).await?;
            ensure!(refused.status() == StatusCode::BAD_REQUEST, "{too_short}");
            ensure!(response_json(refused).await?["error"]["param"] == "expires_in");
        }
        let rolled = roll(172_800).await?;
        ensure!(rolled.status() == StatusCode::OK);
        let account = response_json(rolled).await?;
        ensure!(account["webhook_keys"][0] == json!({"version": 2, "expires_at": null}));
        ensure!(account["webhook_keys"][1]["version"] == 1);
        let expires_at = account["webhook_keys"][1]["expires_at"]
            .as_i64()
            .context("the old key expires")?;
        ensure!(
            (Utc::now().timestamp() + 172_790..=Utc::now().timestamp() + 172_800)
                .contains(&expires_at)
        );
        let overlap = attest(live_key.clone()).await?;
        check(&overlap, &owner, true, &[2, 1])?;
        ensure!(overlap["webhook_keys"][1]["expires_at"] == expires_at);
        check(&attest(test_key.clone()).await?, &owner, false, &[1])?;
        let announced: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE account_id = $1 AND livemode \
             AND type = 'account.updated'",
        )
        .bind(owner.id)
        .fetch_one(pool)
        .await?;
        ensure!(announced == 1, "{announced}");
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit WHERE account_id = $1 AND action = 'webhook_key.roll'",
        )
        .bind(owner.id)
        .fetch_one(pool)
        .await?;
        ensure!(audited == 1);

        // Rolling again keeps the previous keys no longer than the new overlap.
        ensure!(roll(172_800).await?.status() == StatusCode::OK);
        check(&attest(live_key.clone()).await?, &owner, true, &[3, 2, 1])?;
        // Test mode may drop the previous key at once.
        let test_roll = app
            .clone()
            .oneshot(merchant_request(
                Method::POST,
                "/v1/account/webhook_keys/roll",
                serde_json::to_vec(&json!({ "expires_in": 0 }))?,
                &test_key,
            ))
            .await?;
        ensure!(test_roll.status() == StatusCode::OK);
        check(&attest(test_key.clone()).await?, &owner, false, &[2])?;

        // A live key attests only while the operator keeps live mode enabled (design D12).
        sqlx::query("UPDATE accounts SET charges_enabled = false WHERE id = $1")
            .bind(owner.id)
            .execute(pool)
            .await?;
        let refused = app
            .clone()
            .oneshot(merchant_request(Method::GET, path, Vec::new(), &live_key))
            .await?;
        ensure!(refused.status() == StatusCode::FORBIDDEN);
        ensure!(response_json(refused).await?["error"]["code"] == "testmode_charges_only");
        check(&attest(test_key).await?, &owner, false, &[2])?;
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn openapi_snapshot() -> Result<()> {
    let admin_key = SigningKey::from_bytes(&[29; 32]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")?;
    let state = app_state(pool, &admin_key);
    let actual = topup::api::openapi_json(state.clone())?;
    let document: Value = serde_json::from_str(&actual)?;
    ensure!(document["info"]["title"] == "Phala Pay API");
    ensure!(document["info"]["version"] == env!("CARGO_PKG_VERSION"));
    ensure!(document["info"]["description"].as_str().is_some());
    assert_query_parameters(&document)?;
    let admin = topup::api::openapi_admin_json(state)?;
    ensure!(serde_json::from_str::<Value>(&admin)?["info"]["title"] == "Phala Pay admin API");
    for (name, actual) in [("openapi.json", actual), ("openapi.admin.json", admin)] {
        let path = format!("{}/{name}", env!("CARGO_MANIFEST_DIR"));
        if std::env::var_os("UPDATE_OPENAPI").is_some() {
            std::fs::write(&path, &actual)?;
        }
        let expected = std::fs::read_to_string(&path).context("read committed OpenAPI snapshot")?;
        ensure!(
            actual == expected,
            "{name} drifted; regenerate it with UPDATE_OPENAPI=1 and review it"
        );
    }
    Ok(())
}

/// A path or method the API does not serve answers Stripe's error object, with its `doc_url`,
/// never an empty body.
#[tokio::test]
async fn unrecognized_requests_answer_the_error_object() -> Result<()> {
    let admin_key = SigningKey::from_bytes(&[29; 32]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")?;
    let router = test_router(&pool, &admin_key);
    for (method, path) in [
        (Method::GET, "/v1/no_such_resource"),
        (Method::DELETE, "/v1/config"),
    ] {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method.clone())
                    .uri(path)
                    .body(Body::empty())?,
            )
            .await?;
        ensure!(
            response.status() == StatusCode::NOT_FOUND,
            "{method} {path}"
        );
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 65_536).await?)?;
        let error = &body["error"];
        ensure!(error["type"] == "invalid_request_error" && error["code"] == "resource_missing");
        ensure!(
            error["doc_url"]
                == "https://phala-network.github.io/phala-pay/#section/Errors/resource_missing",
            "{body}"
        );
    }
    Ok(())
}

fn test_router(pool: &sqlx::PgPool, admin_key: &SigningKey) -> axum::Router {
    topup::api::router(app_state(pool.clone(), admin_key)).0
}

fn app_state(pool: sqlx::PgPool, admin_key: &SigningKey) -> AppState {
    app_state_with_attestor(pool, admin_key, Arc::new(DstackAttestor::new()))
}

fn app_state_with_attestor(
    pool: sqlx::PgPool,
    admin_key: &SigningKey,
    attestor: Arc<dyn Attestor>,
) -> AppState {
    let route: RouteFile = serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))
        .expect("valid route fixture");
    route.validate().expect("route fixture validates");
    AppState {
        pool,
        routes: Arc::new(topup::routes::RouteSet::new(vec![route]).expect("route loads")),
        admin_key: VerificationKey::from_base64(
            ADMIN_KID.to_owned(),
            &public_key_base64(admin_key),
        )
        .expect("admin key is valid"),
        public_origin: PublicOrigin::parse(TEST_ORIGIN).expect("test origin is valid"),
        attestor,
        rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
        client_reads: Arc::default(),
        rate_limits: Arc::default(),
        screening: Arc::new(support::ClearScreener),
        contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
    }
}

/// Attestor deriving every key as `SHA-256(seed ‖ domain)`, as `topup attest --dev` does, with an
/// empty quote.
struct TestAttestor([u8; 32]);

impl TestAttestor {
    fn public_key(seed: [u8; 32], account: &str, livemode: bool, version: u32) -> Ed25519PublicKey {
        use sha2::{Digest as _, Sha256};
        let domain = WebhookKeyId::new(account, livemode, version)
            .map(|key| key.domain())
            .unwrap_or_default();
        let secret: [u8; 32] = Sha256::new()
            .chain_update(seed)
            .chain_update(domain.as_bytes())
            .finalize()
            .into();
        Ed25519PublicKey(SigningKey::from_bytes(&secret).verifying_key().to_bytes())
    }
}

impl Attestor for TestAttestor {
    fn attest<'a>(&'a self, request: AttestationRequest<'a>) -> AttestationFuture<'a> {
        Box::pin(async move {
            let webhook_keys: Vec<AttestedWebhookKey> = request
                .versions
                .iter()
                .map(|&version| AttestedWebhookKey {
                    version,
                    public_key: Self::public_key(
                        self.0,
                        request.account,
                        request.livemode,
                        version,
                    ),
                })
                .collect();
            let report_data = report_data(
                request.nonce,
                request.account,
                request.livemode,
                &webhook_keys,
            )
            .ok_or(topup::api::AttestationError::Unavailable)?;
            Ok(AttestationEvidence {
                webhook_keys,
                report_data,
                quote: Vec::new(),
            })
        })
    }

    fn webhook_keys<'a>(
        &'a self,
        account: &'a str,
        livemode: bool,
        versions: &'a [u32],
    ) -> topup::api::WebhookKeysFuture<'a> {
        Box::pin(async move {
            Ok(versions
                .iter()
                .map(|&version| AttestedWebhookKey {
                    version,
                    public_key: Self::public_key(self.0, account, livemode, version),
                })
                .collect())
        })
    }
}

fn assert_query_parameters(document: &Value) -> Result<()> {
    let cases = [(
        "/v1/deposits",
        &[
            "client_reference_id",
            "quote",
            "status",
            "tx_hash",
            "created[gt]",
            "created[gte]",
            "created[lt]",
            "created[lte]",
            "limit",
            "starting_after",
            "ending_before",
            "expand[]",
        ][..],
    )];
    for (path, expected_names) in cases {
        let parameters = document["paths"][path]["get"]["parameters"]
            .as_array()
            .context("OpenAPI operation parameters")?;
        for name in expected_names {
            let parameter = parameters
                .iter()
                .find(|parameter| parameter["name"] == *name)
                .with_context(|| format!("missing query parameter {name}"))?;
            ensure!(parameter["in"] == "query");
            ensure!(parameter.get("required").and_then(Value::as_bool) != Some(true));
        }
    }

    let nonce = document["paths"]["/v1/attestation"]["get"]["parameters"]
        .as_array()
        .context("attestation parameters")?
        .iter()
        .find(|parameter| parameter["name"] == "nonce")
        .context("missing nonce parameter")?;
    ensure!(nonce["in"] == "query");
    ensure!(nonce["required"] == true);
    Ok(())
}

/// A live account signing with `key`, with a webhook endpoint.
/// A live account and its live secret key.
async fn seed_product(pool: &sqlx::PgPool, name: &str) -> Result<(Account, String)> {
    let account = seed::create_account(
        pool,
        &NewAccount {
            webhook_url: "https://product.test/webhooks".to_owned(),
            ..NewAccount::named(name)
        },
    )
    .await?;
    let key = seed::create_api_key(pool, account.id, true).await?;
    seed::set_treasury(pool, account.id, true, 1, seed::FIXTURE_TREASURY).await?;
    let route: RouteFile = serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
    seed::accept_routes(pool, account.id, true, &[&route]).await?;
    Ok((account, key))
}

async fn seed_customer(
    pool: &sqlx::PgPool,
    account_id: Uuid,
    client_reference_id: &str,
) -> Result<topup::db::Customer> {
    Ok(seed::create_customer(
        pool,
        &NewCustomer {
            id: Uuid::new_v4(),
            account_id,
            livemode: true,
            client_reference_id: client_reference_id.to_owned(),
            paused_scopes: Vec::new(),
        },
    )
    .await?)
}

/// A live deposit of a new customer of `account_id`.
async fn seed_other_tenant_deposit(pool: &sqlx::PgPool, account_id: Uuid) -> Result<Uuid> {
    let customer = seed_customer(pool, account_id, "other-account").await?;
    let address = seed::insert_address(
        pool,
        &NewAddress {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            chain_id: 1,
            route: "other-route".to_owned(),
            salt: B256::from_str(
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )?,
            address: Address::from_str("0x1111111111111111111111111111111111111111")?,
        },
    )
    .await?;
    let tx_hash =
        B256::from_str("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")?;
    topup::db::insert_deposit(
        pool,
        &NewDeposit {
            chain_id: 1,
            tx_hash,
            log_index: 0,
            receipt_log_index: 0,
            tx_from: alloy_primitives::Address::ZERO,
            tx_nonce: 0,
            is_final: true,
            block_number: 1,
            block_hash: B256::from_str(
                "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            )?,
            block_time: Utc::now(),
            address_id: address.id,
            route: Some("other-route".to_owned()),
            route_version: Some(1),
            asset_contract: Address::from_str("0x2222222222222222222222222222222222222222")?,
            from_address: Address::from_str("0x3333333333333333333333333333333333333333")?,
            amount_atomic: AtomicAmount::new(U256::from(100_u64)),
            state: DepositState::Detected,
            reason: None,
            next_attempt_at: Utc::now(),
        },
    )
    .await?;
    Ok(deposit_id(1, tx_hash, 0))
}

async fn response_json(response: axum::response::Response) -> Result<Value> {
    let bytes = to_bytes(response.into_body(), 1_048_576).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Cache protection encloses router errors and the boot-time read-only gate, even before auth.
#[tokio::test]
async fn tenant_http_errors_and_read_only_rejections_are_not_stored() -> Result<()> {
    let admin_key = SigningKey::from_bytes(&[29; 32]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")?;
    let state = app_state(pool, &admin_key);
    for router in [
        topup::api::router(state.clone()).0,
        topup::api::read_only_router(state, None),
    ] {
        for (method, path) in [
            (Method::GET, "/v1/no_such_resource"),
            (Method::DELETE, "/v1/config"),
            (Method::GET, "/v1/quotes/qt_invalid?client_secret=invalid"),
            (Method::POST, "/v1/api_keys"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())?,
                )
                .await?;
            ensure!(response.status().is_client_error() || response.status().is_server_error());
            ensure!(response.headers()["cache-control"] == "no-store", "{path}");
        }
        let document = router
            .clone()
            .oneshot(axum::http::Request::get("/openapi.json").body(Body::empty())?)
            .await?;
        ensure!(document.status() == StatusCode::OK);
        ensure!(!document.headers().contains_key("cache-control"));
        let credentialed = router
            .oneshot(
                axum::http::Request::get("/unknown")
                    .header("Authorization", "Bearer invalid")
                    .body(Body::empty())?,
            )
            .await?;
        ensure!(credentialed.headers()["cache-control"] == "no-store");
    }
    Ok(())
}

/// Saturating the router's shared admission permits reaches the real HandleErrorLayer path,
/// before auth or a handler. No worker tasks are spawned; every pending future is dropped.
#[tokio::test]
async fn load_shed_responses_match_the_shared_unavailable_contract() -> Result<()> {
    use std::future::{Future, poll_fn};
    use std::task::Poll;
    use std::time::Duration;

    support::with_database(|database| {
        Box::pin(async move {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&database.app_url)
                .await?;
            let held = pool.acquire().await?;
            let admin_key = SigningKey::from_bytes(&[49; 32]);
            let (app, docs) = topup::api::router(app_state(pool.clone(), &admin_key));
            let mut pending = Vec::with_capacity(256);
            for _ in 0..256 {
                let request =
                    axum::http::Request::get("/v1/admin/reports/daily")
                        .body(Body::from_stream(futures_util::stream::pending::<
                            Result<axum::body::Bytes, std::io::Error>,
                        >()))?;
                let mut future = Box::pin(app.clone().oneshot(request));
                poll_fn(|context| match future.as_mut().poll(context) {
                    Poll::Pending => Poll::Ready(Ok(())),
                    Poll::Ready(_) => Poll::Ready(Err(anyhow::anyhow!(
                        "admitted admin request did not wait for its incomplete body"
                    ))),
                })
                .await?;
                pending.push(future);
            }
            let request = axum::http::Request::get("/v1/account").body(Body::empty())?;
            let response =
                tokio::time::timeout(Duration::from_secs(2), app.oneshot(request)).await??;
            // Release every permit and connection before assertions so failures leave no work behind.
            drop(pending);
            drop(held);
            pool.close().await;
            ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
            ensure!(response.headers()["content-type"] == "application/json");
            ensure!(response.headers()["retry-after"] == "1");
            ensure!(response.headers()["cache-control"] == "no-store");
            let request_id = response.headers()["request-id"].to_str()?;
            ensure!(request_id.starts_with("req_") && request_id.len() == 36);
            let body = response_json(response).await?;
            ensure!(body["error"]["type"] == "api_error" && body["error"]["code"] == "unavailable");
            let documented = &docs.merchant["paths"]["/v1/account"]["get"]["responses"]["503"];
            ensure!(
                documented["content"]["application/json"]["schema"]["$ref"]
                    == "#/components/schemas/ErrorResponse"
            );
            ensure!(documented["headers"]["Retry-After"]["required"] == false);
            Ok(())
        })
    })
    .await
}

/// Admission leases drain writes, boot health reopens the new process, and expiry needs no worker.
#[tokio::test]
async fn instance_maintenance_admission_and_expiry() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let key = SigningKey::from_bytes(&[91; 32]);
        let app = test_router(&database.app_pool, &key);
        let now = Utc::now().timestamp();
        let account = seed::create_account(&database.app_pool, &NewAccount::named("maintenance-test")).await?;
        let merchant_key = seed::create_api_key(&database.app_pool, account.id, false).await?;
        seed::set_account_paused_scopes(&database.app_pool, account.id, &["quotes".to_owned()]).await?;
        let pause = |owner: &str, duration: i64| -> Result<_> {
            Ok(signed_request(
                Method::POST, "/v1/admin/instance/pause",
                serde_json::to_vec(&json!({"owner": owner, "duration_seconds": duration, "reason": "test upgrade"}))?,
                ADMIN_KID, &key, now,
            ))
        };
        let response = app.clone().oneshot(pause("deploy-1", 901)?).await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
        let response = app.clone().oneshot(pause("deploy-1", 900)?).await?;
        ensure!(response.status() == StatusCode::OK);
        let response = app.clone().oneshot(pause("deploy-2", 900)?).await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST, "another owner cannot replace the lease");
        for path in ["/v1/quotes", "/v1/deposit_addresses"] {
            let request = axum::http::Request::post(path)
                .header("Authorization", format!("Bearer {merchant_key}"))
                .header("Idempotency-Key", "upgrade-test")
                .body(Body::empty())?;
            let response = app.clone().oneshot(request).await?;
            ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
            ensure!(response.headers()["retry-after"] == "5");
            ensure!(!response.headers().contains_key("idempotent-replayed"));
            let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
            ensure!(body["error"]["code"] == "service_maintenance");
        }
        let claims: i64 = sqlx::query_scalar("SELECT count(*) FROM idempotency_keys WHERE key = 'upgrade-test'")
            .fetch_one(&database.app_pool).await?;
        ensure!(claims == 0, "maintenance never claims or saves the idempotency key");
        let response = app.clone().oneshot(signed_request(Method::POST,
            "/v1/admin/accounts", b"{}".to_vec(), ADMIN_KID, &key, now)).await?;
        ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE, "admin business mutations are paused too");
        let response = app.clone().oneshot(axum::http::Request::get("/v1/account")
            .header("Authorization", format!("Bearer {merchant_key}"))
            .body(Body::empty())?).await?;
        ensure!(response.status() == StatusCode::OK, "merchant reads continue");
        let response = app.clone().oneshot(signed_request(Method::GET,
            "/v1/admin/reports/daily", vec![], ADMIN_KID, &key, now)).await?;
        ensure!(response.status() == StatusCode::OK, "reads continue during maintenance");
        let response = app.clone().oneshot(axum::http::Request::get("/healthz").body(Body::empty())?).await?;
        ensure!(response.status() == StatusCode::OK, "health is independent of admission");
        let response = app.clone().oneshot(signed_request(Method::POST,
            "/v1/admin/instance/resume", serde_json::to_vec(&json!({"owner":"deploy-2", "reason":"stale cleanup"}))?,
            ADMIN_KID, &key, now)).await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST, "stale cleanup cannot lift the lease");
        let response = app.clone().oneshot(signed_request(Method::POST,
            "/v1/admin/instance/resume", serde_json::to_vec(&json!({"owner":"deploy-1", "reason":"healthy"}))?,
            ADMIN_KID, &key, now)).await?;
        ensure!(response.status() == StatusCode::OK);
        let response = app.clone().oneshot(axum::http::Request::post("/v1/quotes")
                .header("Authorization", format!("Bearer {merchant_key}"))
                .header("Idempotency-Key", "upgrade-test")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))?).await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST, "normal admission resumes");
        let response = app.clone().oneshot(pause("deploy-3", 1)?).await?;
        ensure!(response.status() == StatusCode::OK);
        // The process deadline expires without a deployment runner or expiry worker.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let response = app.clone().oneshot(axum::http::Request::post("/v1/quotes")
                .header("Authorization", format!("Bearer {merchant_key}"))
                .header("Idempotency-Key", "upgrade-test")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))?).await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST, "deadline auto-clears admission");
        let response = app.clone().oneshot(signed_request(Method::GET,
            "/v1/admin/instance/pause", vec![], ADMIN_KID, &key, now)).await?;
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        ensure!(body["paused_scopes"] == json!([]));
        // A replacement process gates writes until its own health passes. No schema migration
        // or persisted maintenance lease can block an older rollback image's startup.
        let (restarted, _) = topup::api::booting_router(app_state(database.app_pool.clone(), &key));
        let write = || axum::http::Request::post("/v1/quotes")
            .header("Authorization", format!("Bearer {merchant_key}"))
            .header("Content-Type", "application/json")
            .body(Body::from("{}"));
        let response = restarted.clone().oneshot(write()?).await?;
        ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE, "boot waits for health");
        let response = restarted.clone().oneshot(axum::http::Request::get("/healthz").body(Body::empty())?).await?;
        ensure!(response.status() == StatusCode::OK);
        let response = restarted.clone().oneshot(write()?).await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST, "healthy boot auto-clears maintenance");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit WHERE subject = 'instance'")
            .fetch_one(&database.owner_pool).await?;
        ensure!(count == 3, "only accepted pause/resume mutations are audited");
        let business_scopes: Vec<String> = sqlx::query_scalar("SELECT paused_scopes FROM accounts WHERE id = $1")
            .bind(account.id).fetch_one(&database.app_pool).await?;
        ensure!(business_scopes == vec!["quotes"], "upgrade never clears business incident pauses");
        Ok(())
    }.await;
    database.cleanup().await?;
    result
}
