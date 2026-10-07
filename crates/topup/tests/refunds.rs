//! Merchant refunds (design D5): request, attach the merchant's transaction, verify at finality.

mod support;

use std::collections::{BTreeMap, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::body::to_bytes;
use axum::http::{Method, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{Duration, Utc};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use sqlx::Connection as _;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use topup::api::{AppState, ClientReadLimiter, PublicOrigin, VerificationKey};
use topup::db::{self, Account, NewDeposit};
use topup::deposit_addresses::{self, ChainContracts};
use topup::outbox::{DeliveryConfig, DeliveryWorker};
use topup::refunds::{
    DestinationScreener, DestinationScreening, EvmRefundChainReader, RefundChainReader,
    RefundReadError, RefundReceipt, RefundVerificationConfig, RefundVerificationWorker,
    Verification,
};
use topup_adapters::attestation::DstackAttestor;
use topup_adapters::chain::evm::EvmClient;
use topup_core::deposit::{DepositState, RejectReason};
use topup_core::identity::deposit_id;
use topup_core::money::AtomicAmount;
use topup_core::refund::RefundTransfer;
use topup_core::route::RouteFile;
use tower::ServiceExt;
use uuid::Uuid;

use support::seed::{self, FIXTURE_TREASURY, NewAccount, NewAddress, NewCustomer};
use support::{TEST_ORIGIN, TestDatabase, merchant_request, public_key_base64, signed_request};

const ADMIN_KID: &str = "admin/v1";
const REFUND_DESTINATION: &str = "0x4444444444444444444444444444444444444444";
const REFUND_TX: &str = "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
const OTHER_TX: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const SANCTIONED: &str = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
/// The account that sends every refund transaction [`refund_rpc`] knows.
const REFUND_SENDER: &str = "0x5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e";

/// A `POST /v1/refunds` body.
fn refund_body(deposit: Uuid, destination: &str, amount: &str) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&json!({
        "deposit": format!("dep_{}", deposit.simple()),
        "destination_address": destination,
        "amount_atomic": amount,
    }))?)
}

async fn pending_credited_refund(pool: &sqlx::PgPool) -> Result<(Merchant, Uuid, String, Uuid)> {
    let app = test_router(pool, &SigningKey::from_bytes(&[43; 32]));
    let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
    let deposit = seed_deposit(
        pool,
        merchant.account.id,
        "sanctioned-credit",
        150,
        DepositState::Credited,
        None,
    )
    .await?;
    let id = merchant.refund(deposit, "100").await?;
    let refund = topup::ids::parse(topup::ids::REFUND, &id).context("refund id")?;
    Ok((merchant, deposit, id, refund))
}

async fn wait_for_deposit_lock<T>(
    pool: &sqlx::PgPool,
    task: &tokio::task::JoinHandle<T>,
) -> Result<()> {
    tokio::time::timeout(StdDuration::from_secs(5), async {
        loop {
            ensure!(
                !task.is_finished(),
                "refund verification bypassed the deposit lock"
            );
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                     WHERE datname = current_database() AND wait_event_type = 'Lock' \
                     AND query LIKE 'SELECT deposit.sanctions_hit_at IS NULL%')",
            )
            .fetch_one(pool)
            .await?;
            if blocked {
                return anyhow::Ok(());
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn a_pending_refund_cannot_be_marked_paid_after_a_sanctions_hit() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            seed::initialize_dual_chain(pool, 1).await?;
            let (merchant, deposit, id, refund) = pending_credited_refund(pool).await?;
            let mut hit = database.owner_pool.begin().await?;
            sqlx::query("UPDATE deposits SET sanctions_hit_at = now() WHERE id = $1")
                .bind(deposit)
                .execute(&mut *hit)
                .await?;
            let request = tokio::spawn({
                let merchant = merchant.clone();
                let id = id.clone();
                async move {
                    merchant
                        .post(
                            &format!("/v1/refunds/{id}/mark_paid"),
                            mark_paid_body(REFUND_TX)?,
                        )
                        .await
                }
            });
            wait_for_deposit_lock(&database.owner_pool, &request).await?;
            hit.commit().await?;
            let (status, error) =
                tokio::time::timeout(StdDuration::from_secs(5), request).await???;
            ensure!(status == StatusCode::BAD_REQUEST, "{error}");
            ensure!(
                error["error"]["code"] == "deposit_not_refundable",
                "{error}"
            );
            let hash: Option<String> =
                sqlx::query_scalar("SELECT tx_hash FROM refunds WHERE id = $1")
                    .bind(refund)
                    .fetch_one(pool)
                    .await?;
            ensure!(hash.is_none());
            ensure!(refund_status(pool, refund).await? == "pending");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_paid_pending_refund_is_not_claimed_after_a_sanctions_hit() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            seed::initialize_dual_chain(pool, 1).await?;
            let (merchant, deposit, id, refund) = pending_credited_refund(pool).await?;
            merchant.mark_paid(&id, REFUND_TX).await?;
            let paying = finalized(vec![transfer(
                FIXTURE_TREASURY,
                REFUND_DESTINATION,
                100,
                7,
            )?]);
            let worker = test_worker(pool, vec![paying.clone()], vec![paying]);
            let mut hit = database.owner_pool.begin().await?;
            sqlx::query("UPDATE deposits SET sanctions_hit_at = now() WHERE id = $1")
                .bind(deposit)
                .execute(&mut *hit)
                .await?;
            // The claim skips the locked deposit even while the marker is still uncommitted.
            let while_locked =
                tokio::time::timeout(StdDuration::from_secs(5), worker.check_once()).await;
            hit.commit().await?;
            ensure!(while_locked?? == Verification::Idle);
            ensure!(worker.check_once().await? == Verification::Idle);
            ensure!(refund_status(pool, refund).await? == "pending");
            let events: i64 =
                sqlx::query_scalar("SELECT count(*) FROM events WHERE type = 'deposit.refunded'")
                    .fetch_one(pool)
                    .await?;
            ensure!(events == 0);
            Ok(())
        })
    })
    .await
}

struct ReceiptGate {
    receipt: RefundReceipt,
    gate: Option<(Arc<Notify>, Arc<Notify>)>,
}

#[async_trait]
impl RefundChainReader for ReceiptGate {
    async fn receipt(
        &self,
        _chain_id: u64,
        _tx_hash: B256,
    ) -> Result<RefundReceipt, RefundReadError> {
        if let Some((started, resume)) = &self.gate {
            started.notify_one();
            resume.notified().await;
        }
        Ok(self.receipt.clone())
    }
    async fn origin(
        &self,
        _chain_id: u64,
        _tx_hash: B256,
    ) -> Result<Option<(Address, u64)>, RefundReadError> {
        Ok(None)
    }
}

#[tokio::test]
async fn a_sanctions_hit_during_verification_keeps_the_refund_pending() -> Result<()> {
    for valid_payment in [true, false] {
        support::with_database(|database| {
            Box::pin(async move {
                let pool = &database.app_pool;
                seed::initialize_dual_chain(pool, 1).await?;
                let (merchant, deposit, id, refund) = pending_credited_refund(pool).await?;
                merchant.mark_paid(&id, REFUND_TX).await?;
                let paying = finalized(vec![transfer(
                    FIXTURE_TREASURY,
                    REFUND_DESTINATION,
                    if valid_payment { 100 } else { 99 },
                    7,
                )?]);
                let started = Arc::new(Notify::new());
                let resume = Arc::new(Notify::new());
                let worker = RefundVerificationWorker::new(
                    pool.clone(),
                    Arc::new(
                        topup::routes::RouteSet::new(vec![route_fixture()])
                            .map_err(anyhow::Error::msg)?,
                    ),
                    ReceiptGate {
                        receipt: paying.clone(),
                        gate: Some((started.clone(), resume.clone())),
                    },
                    ReceiptGate {
                        receipt: paying,
                        gate: None,
                    },
                    RefundVerificationConfig {
                        retry_interval: StdDuration::ZERO,
                        observe_timeout: StdDuration::from_secs(10),
                        ..Default::default()
                    },
                );
                let verifying = tokio::spawn(async move { worker.check_once().await });
                tokio::time::timeout(StdDuration::from_secs(5), started.notified()).await?;
                let mut hit = database.owner_pool.begin().await?;
                sqlx::query("UPDATE deposits SET sanctions_hit_at = now() WHERE id = $1")
                    .bind(deposit)
                    .execute(&mut *hit)
                    .await?;
                resume.notify_one();
                wait_for_deposit_lock(&database.owner_pool, &verifying).await?;
                hit.commit().await?;
                ensure!(
                    tokio::time::timeout(StdDuration::from_secs(5), verifying).await???
                        == Verification::Waiting
                );
                ensure!(refund_status(pool, refund).await? == "pending");
                let events: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM events WHERE type = 'deposit.refunded'",
                )
                .fetch_one(pool)
                .await?;
                ensure!(events == 0);
                Ok(())
            })
        })
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn a_missing_bound_route_version_never_uses_the_current_refund_floor() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            seed::initialize_dual_chain(pool, 1).await?;
            let app = test_router(pool, &SigningKey::from_bytes(&[43; 32]));
            let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
            let deposit = seed_deposit(
                pool,
                merchant.account.id,
                "missing-version",
                100,
                DepositState::Rejected,
                Some(RejectReason::AssetNotAccepted),
            )
            .await?;
            let mut current = route_fixture();
            current.version = 2;
            current.merchant.min_refund_atomic =
                topup_core::route::Bounded::at(AtomicAmount::new(U256::from(1)));
            let routes =
                topup::routes::RouteSet::new(vec![current.clone()]).map_err(anyhow::Error::msg)?;
            let error = topup::payment_config::refund_floor(
                &mut *pool.acquire().await?,
                &routes,
                deposit,
                &current,
            )
            .await
            .expect_err("a missing bound version must fail closed");
            ensure!(
                error.to_string().contains("version 1 is not loaded"),
                "{error}"
            );
            let app = test_router_on(
                pool,
                &SigningKey::from_bytes(&[43; 32]),
                Arc::new(StaticScreener::Listing),
                Arc::default(),
                vec![current],
            );
            let merchant = Merchant { app, ..merchant };
            let (status, error) = merchant
                .post(
                    "/v1/refunds",
                    refund_body(deposit, REFUND_DESTINATION, "100")?,
                )
                .await?;
            ensure!(status == StatusCode::INTERNAL_SERVER_ERROR, "{error}");
            ensure!(error["error"]["code"] == "internal_error", "{error}");
            ensure!(!error.to_string().contains("version 1"));
            let refunds: i64 = sqlx::query_scalar("SELECT count(*) FROM refunds")
                .fetch_one(pool)
                .await?;
            ensure!(refunds == 0);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_refund_paid_from_the_address_treasury_succeeds_at_finality() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[43; 32]);
        let app = test_router(pool, &admin_key);
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        let other = Merchant::seed(pool, &app, "builder").await?;
        let deposit =
            seed_rejected_deposit(pool, merchant.account.id, "refund-account", 150).await?;
        let other_deposit =
            seed_rejected_deposit(pool, other.account.id, "other-account", 150).await?;

        // Another account's deposit is unknown.
        let (status, _) = merchant
            .post(
                "/v1/refunds",
                refund_body(other_deposit, REFUND_DESTINATION, "100")?,
            )
            .await?;
        ensure!(status == StatusCode::NOT_FOUND);

        // Each pause scope level holds new refunds.
        let customer = customer_id(pool, deposit).await?;
        let paused = ["refunds".to_owned()];
        seed::set_customer_paused_scopes(pool, customer, &paused).await?;
        merchant.expect_paused(deposit).await?;
        seed::set_customer_paused_scopes(pool, customer, &[]).await?;
        seed::set_account_paused_scopes(pool, merchant.account.id, &paused).await?;
        merchant.expect_paused(deposit).await?;
        seed::set_account_paused_scopes(pool, merchant.account.id, &[]).await?;
        sqlx::query(
            "INSERT INTO route_pauses (route, paused_scopes) VALUES ($1, ARRAY['refunds'])",
        )
        .bind("phala-cloud-ethereum-pha-usd")
        .execute(pool)
        .await?;
        merchant.expect_paused(deposit).await?;
        sqlx::query("UPDATE route_pauses SET paused_scopes = '{}' WHERE route = $1")
            .bind("phala-cloud-ethereum-pha-usd")
            .execute(pool)
            .await?;

        let (status, refund) = merchant
            .post(
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, "100")?,
            )
            .await?;
        ensure!(status == StatusCode::OK, "{refund}");
        ensure!(
            refund["object"] == "refund" && refund["status"] == "pending",
            "{refund}"
        );
        ensure!(refund["deposit"] == format!("dep_{}", deposit.simple()));
        ensure!(refund["amount_atomic"] == "100");
        ensure!(
            refund["treasury"] == format!("{FIXTURE_TREASURY:#x}"),
            "{refund}"
        );
        ensure!(refund["transaction_hash"].is_null() && refund["failure_reason"].is_null());
        let id = refund["id"].as_str().context("refund id")?.to_owned();
        let refund_id = topup::ids::parse(topup::ids::REFUND, &id).context("re_ id")?;

        // The refund is invisible to another account, whatever the action.
        for (method, path, body) in [
            (Method::GET, format!("/v1/refunds/{id}"), Vec::new()),
            (
                Method::POST,
                format!("/v1/refunds/{id}/mark_paid"),
                mark_paid_body(REFUND_TX)?,
            ),
            (Method::POST, format!("/v1/refunds/{id}/cancel"), Vec::new()),
        ] {
            let (status, error) = other.call(method, &path, body).await?;
            ensure!(status == StatusCode::NOT_FOUND, "{path}: {error}");
        }

        let (status, error) = merchant
            .post(
                &format!("/v1/refunds/{id}/mark_paid"),
                mark_paid_body("0xdd")?,
            )
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST, "{error}");
        ensure!(error["error"]["param"] == "transaction_hash", "{error}");

        let (status, marked) = merchant
            .post(
                &format!("/v1/refunds/{id}/mark_paid"),
                mark_paid_body(REFUND_TX)?,
            )
            .await?;
        ensure!(status == StatusCode::OK, "{marked}");
        ensure!(marked["status"] == "pending" && marked["transaction_hash"] == REFUND_TX);
        // The same transaction again is a no-op; another one is refused while it is verified.
        let (status, _) = merchant
            .post(
                &format!("/v1/refunds/{id}/mark_paid"),
                mark_paid_body(REFUND_TX)?,
            )
            .await?;
        ensure!(status == StatusCode::OK);
        let (status, error) = merchant
            .post(
                &format!("/v1/refunds/{id}/mark_paid"),
                mark_paid_body(OTHER_TX)?,
            )
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST);
        ensure!(
            error["error"]["code"] == "refund_unexpected_state",
            "{error}"
        );

        let paying = finalized(vec![transfer(
            FIXTURE_TREASURY,
            REFUND_DESTINATION,
            100,
            7,
        )?]);
        let worker = test_worker(
            pool,
            vec![RefundReceipt::Pending, paying.clone(), paying.clone()],
            vec![
                paying.clone(),
                RefundReceipt::Finalized {
                    block_number: 90,
                    block_hash: B256::repeat_byte(0x0b),
                    succeeded: true,
                    transfers: vec![transfer(FIXTURE_TREASURY, REFUND_DESTINATION, 100, 7)?],
                },
                paying,
            ],
        );
        // Not final on provider A yet, then the providers disagree on its block: it waits.
        ensure!(worker.check_once().await? == Verification::Waiting);
        ensure!(worker.check_once().await? == Verification::Waiting);
        ensure!(refund_status(pool, refund_id).await? == "pending");
        ensure!(worker.check_once().await? == Verification::Succeeded);
        ensure!(worker.check_once().await? == Verification::Idle);

        let (status, refund) = merchant
            .call(
                Method::GET,
                &format!("/v1/refunds/{id}?expand[]=deposit"),
                Vec::new(),
            )
            .await?;
        ensure!(status == StatusCode::OK);
        ensure!(
            refund["status"] == "succeeded" && refund["receipt_log_index"] == 7,
            "{refund}"
        );
        ensure!(
            refund["deposit"]["id"] == format!("dep_{}", deposit.simple()),
            "{refund}"
        );
        ensure!(
            refund["deposit"]["amount_refunded_atomic"] == "100",
            "{refund}"
        );
        ensure!(refund["deposit"]["refunded"] == false, "{refund}");
        let event = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
            "SELECT id, account_id, object_id FROM events WHERE type = 'deposit.refunded'",
        )
        .fetch_one(pool)
        .await?;
        ensure!(
            event
                == (
                    topup_core::identity::event_id("deposit.refunded", refund_id),
                    merchant.account.id,
                    deposit
                )
        );

        let (status, error) = merchant
            .post(&format!("/v1/refunds/{id}/cancel"), Vec::new())
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST);
        ensure!(
            error["error"]["code"] == "refund_unexpected_state",
            "{error}"
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn a_refused_or_repeated_refund_call_holds_no_lock_once_it_answers() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[49; 32]);
        let app = test_router(pool, &admin_key);
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        let deposit = seed_rejected_deposit(pool, merchant.account.id, "locks", 150).await?;
        // Connected, and each probe prepared, up front: a probe right after a response is then a
        // single round trip, which reaches the server before a dropped transaction's queued
        // rollback would.
        let mut probe = sqlx::PgConnection::connect(&database.app_url).await?;
        ensure!(unlocked(&mut probe, DEPOSIT_ROW, deposit).await?);

        let (status, error) = merchant
            .post(
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, "151")?,
            )
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST, "{error}");
        ensure!(
            unlocked(&mut probe, DEPOSIT_ROW, deposit).await?,
            "a refused refund request left its deposit locked"
        );

        let id = merchant.refund(deposit, "100").await?;
        let refund_id = topup::ids::parse(topup::ids::REFUND, &id).context("re_ id")?;
        merchant.mark_paid(&id, REFUND_TX).await?;
        ensure!(unlocked(&mut probe, REFUND_ROW, refund_id).await?);
        for (name, path, body, expected) in [
            (
                "a repeated mark_paid",
                format!("/v1/refunds/{id}/mark_paid"),
                mark_paid_body(REFUND_TX)?,
                StatusCode::OK,
            ),
            (
                "a mark_paid with another transaction",
                format!("/v1/refunds/{id}/mark_paid"),
                mark_paid_body(OTHER_TX)?,
                StatusCode::BAD_REQUEST,
            ),
            (
                "a cancel of a paid refund",
                format!("/v1/refunds/{id}/cancel"),
                Vec::new(),
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let (status, response) = merchant.post(&path, body).await?;
            ensure!(status == expected, "{name}: {response}");
            ensure!(
                unlocked(&mut probe, REFUND_ROW, refund_id).await?,
                "{name} left the refund locked"
            );
        }
        // So the verification worker, which skips locked refunds, claims it straight away.
        let worker = test_worker(
            pool,
            vec![RefundReceipt::Pending],
            vec![RefundReceipt::Pending],
        );
        ensure!(worker.check_once().await? == Verification::Waiting);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

const DEPOSIT_ROW: &str = "SELECT id FROM deposits WHERE id = $1 FOR UPDATE SKIP LOCKED";
const REFUND_ROW: &str = "SELECT id FROM refunds WHERE id = $1 FOR UPDATE SKIP LOCKED";

/// Whether no other transaction holds row `id`, taken as the workers take rows: `FOR UPDATE SKIP
/// LOCKED`, which returns nothing for a locked row.
async fn unlocked(probe: &mut sqlx::PgConnection, query: &'static str, id: Uuid) -> Result<bool> {
    let row: Option<Uuid> = sqlx::query_scalar(query)
        .bind(id)
        .fetch_optional(&mut *probe)
        .await?;
    Ok(row == Some(id))
}

#[tokio::test]
async fn transfers_that_do_not_pay_the_refund_fail_it_and_release_the_reservation() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[44; 32]);
        let app = test_router(pool, &admin_key);
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        // The account has since moved to another treasury; its older addresses still pay the
        // treasury they were issued for, and refunds of their deposits come from that one.
        let current_treasury = Address::repeat_byte(0x7c);
        seed::set_treasury(pool, merchant.account.id, true, 1, current_treasury).await?;

        let cases = [
            (
                "from the account's current treasury",
                finalized(vec![transfer(
                    current_treasury,
                    REFUND_DESTINATION,
                    100,
                    3,
                )?]),
                "sender_mismatch",
            ),
            (
                "to another address",
                finalized(vec![transfer(
                    FIXTURE_TREASURY,
                    "0x5555555555555555555555555555555555555555",
                    100,
                    3,
                )?]),
                "destination_mismatch",
            ),
            (
                "one base unit short",
                finalized(vec![transfer(FIXTURE_TREASURY, REFUND_DESTINATION, 99, 3)?]),
                "amount_mismatch",
            ),
            (
                "of another token",
                finalized(vec![RefundTransfer {
                    token: Address::repeat_byte(0x77),
                    ..transfer(FIXTURE_TREASURY, REFUND_DESTINATION, 100, 3)?
                }]),
                "transfer_not_found",
            ),
            (
                "reverted",
                RefundReceipt::Finalized {
                    block_number: 90,
                    block_hash: B256::repeat_byte(0x09),
                    succeeded: false,
                    transfers: Vec::new(),
                },
                "transaction_failed",
            ),
        ];
        let mut failed = Vec::new();
        for (index, (name, receipt, reason)) in cases.into_iter().enumerate() {
            let deposit =
                seed_rejected_deposit(pool, merchant.account.id, &format!("case-{index}"), 100)
                    .await?;
            let id = merchant.refund(deposit, "100").await?;
            let (status, _) = merchant
                .post(
                    &format!("/v1/refunds/{id}/mark_paid"),
                    mark_paid_body(REFUND_TX)?,
                )
                .await?;
            ensure!(status == StatusCode::OK, "{name}");
            let worker = test_worker(pool, vec![receipt.clone()], vec![receipt]);
            ensure!(worker.check_once().await? == Verification::Failed, "{name}");
            let (_, refund) = merchant
                .call(Method::GET, &format!("/v1/refunds/{id}"), Vec::new())
                .await?;
            ensure!(refund["status"] == "failed", "{name}: {refund}");
            ensure!(refund["failure_reason"] == reason, "{name}: {refund}");
            failed.push((id.clone(), reason));
            // The failed refund no longer holds the deposit: the whole amount can be refunded.
            merchant.refund(deposit, "100").await?;
        }

        // Each failure sends one `refund.failed` about its refund, and a `refund.updated` with
        // the status it left.
        let events: Vec<(Uuid, String, String, Uuid)> = sqlx::query_as(
            "SELECT id, type, object_type, object_id FROM events WHERE type = 'refund.failed' \
             ORDER BY created, id",
        )
        .fetch_all(pool)
        .await?;
        ensure!(events.len() == failed.len(), "{events:?}");
        let updated: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM events WHERE type = 'refund.updated' \
             AND data -> 'previous_attributes' ->> 'status' = 'pending' \
             AND data -> 'object' ->> 'status' = 'failed'",
        )
        .fetch_one(pool)
        .await?;
        ensure!(usize::try_from(updated)? == failed.len());
        for (id, _) in &failed {
            let refund_id = topup::ids::parse(topup::ids::REFUND, id).context("re_ id")?;
            ensure!(
                events.contains(&(
                    topup_core::identity::event_id("refund.failed", refund_id),
                    "refund.failed".to_owned(),
                    "refund".to_owned(),
                    refund_id,
                )),
                "{id}: {events:?}"
            );
        }
        // Delivered through the outbox, `data.object` is the failed refund with its reason.
        let bodies: Vec<Value> = deliver_events(pool, merchant.account.id)
            .await?
            .into_iter()
            .filter(|body| body["type"] == "refund.failed")
            .collect();
        ensure!(bodies.len() == failed.len(), "{bodies:?}");
        for (id, reason) in &failed {
            let body = bodies
                .iter()
                .find(|body| body["data"]["object"]["id"] == id.as_str())
                .with_context(|| format!("no refund.failed for {id}"))?;
            ensure!(body["type"] == "refund.failed", "{body}");
            let refund = &body["data"]["object"];
            ensure!(
                refund["object"] == "refund" && refund["status"] == "failed",
                "{body}"
            );
            ensure!(refund["failure_reason"] == *reason, "{body}");
            ensure!(refund["transaction_hash"] == REFUND_TX, "{body}");
        }
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn a_deposit_address_refund_is_paid_from_that_networks_own_treasury() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[49; 32]);
        let app = test_router(pool, &admin_key);
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        let customer = seed_customer(pool, merchant.account.id, "team-42").await?;
        // The customer's deposit address on chain 1 over the fixture treasury; the account then
        // moves chain 1 to another treasury, which supersedes that network with a forwarder over
        // the new one. Both are still credited.
        let chains = [ChainContracts::of(&route_fixture())];
        let current_treasury = Address::repeat_byte(0x7c);
        let (issued, _) =
            deposit_addresses::create(pool, &merchant.account, &customer, &chains, None).await?;
        seed::set_treasury(pool, merchant.account.id, true, 1, current_treasury).await?;
        let (moved, _) =
            deposit_addresses::create(pool, &merchant.account, &customer, &chains, None).await?;
        ensure!(moved.id == issued.id);
        let [old_network] = issued.networks.as_slice() else {
            anyhow::bail!("one network: {issued:?}");
        };
        let [current_network] = moved.networks.as_slice() else {
            anyhow::bail!("one network: {moved:?}");
        };
        ensure!(old_network.treasury == FIXTURE_TREASURY);
        ensure!(current_network.treasury == current_treasury);
        ensure!(old_network.address != current_network.address);

        // A deposit to the superseded network is refunded from that network's treasury: a
        // payment from the account's current treasury fails it, and one from the old treasury
        // pays it.
        let to_old = seed_deposit_to(pool, old_network.address_id, 100, 0x51).await?;
        let (status, refund) = merchant
            .post(
                "/v1/refunds",
                refund_body(to_old, REFUND_DESTINATION, "100")?,
            )
            .await?;
        ensure!(status == StatusCode::OK, "{refund}");
        ensure!(
            refund["treasury"] == format!("{FIXTURE_TREASURY:#x}"),
            "{refund}"
        );
        let id = refund["id"].as_str().context("refund id")?.to_owned();
        let receipt = finalized(vec![transfer(
            current_treasury,
            REFUND_DESTINATION,
            100,
            3,
        )?]);
        merchant.mark_paid(&id, REFUND_TX).await?;
        let worker = test_worker(pool, vec![receipt.clone()], vec![receipt]);
        ensure!(worker.check_once().await? == Verification::Failed);
        let (_, failed) = merchant
            .call(Method::GET, &format!("/v1/refunds/{id}"), Vec::new())
            .await?;
        ensure!(failed["failure_reason"] == "sender_mismatch", "{failed}");

        let id = merchant.refund(to_old, "100").await?;
        let receipt = finalized(vec![transfer(
            FIXTURE_TREASURY,
            REFUND_DESTINATION,
            100,
            4,
        )?]);
        merchant.mark_paid(&id, REFUND_TX).await?;
        let worker = test_worker(pool, vec![receipt.clone()], vec![receipt]);
        ensure!(worker.check_once().await? == Verification::Succeeded);

        // A deposit to the current network is refunded from the current treasury, and the old
        // treasury does not pay it.
        let to_current = seed_deposit_to(pool, current_network.address_id, 100, 0x52).await?;
        let (status, refund) = merchant
            .post(
                "/v1/refunds",
                refund_body(to_current, REFUND_DESTINATION, "100")?,
            )
            .await?;
        ensure!(status == StatusCode::OK, "{refund}");
        ensure!(
            refund["treasury"] == format!("{current_treasury:#x}"),
            "{refund}"
        );
        let id = refund["id"].as_str().context("refund id")?.to_owned();
        let receipt = finalized(vec![transfer(
            FIXTURE_TREASURY,
            REFUND_DESTINATION,
            100,
            5,
        )?]);
        merchant.mark_paid(&id, REFUND_TX).await?;
        let worker = test_worker(pool, vec![receipt.clone()], vec![receipt]);
        ensure!(worker.check_once().await? == Verification::Failed);
        let id = merchant.refund(to_current, "100").await?;
        let receipt = finalized(vec![transfer(
            current_treasury,
            REFUND_DESTINATION,
            100,
            6,
        )?]);
        merchant.mark_paid(&id, REFUND_TX).await?;
        let worker = test_worker(pool, vec![receipt.clone()], vec![receipt]);
        ensure!(worker.check_once().await? == Verification::Succeeded);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn a_transfer_log_pays_only_one_refund() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[46; 32]);
        let app = test_router(pool, &admin_key);
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        let first_deposit = seed_rejected_deposit(pool, merchant.account.id, "first", 100).await?;
        let second_deposit =
            seed_rejected_deposit(pool, merchant.account.id, "second", 100).await?;
        let first = merchant.refund(first_deposit, "100").await?;
        let second = merchant.refund(second_deposit, "100").await?;

        let (status, _) = merchant
            .post(
                &format!("/v1/refunds/{first}/mark_paid"),
                serde_json::to_vec(
                    &json!({"transaction_hash": REFUND_TX, "receipt_log_index": 7}),
                )?,
            )
            .await?;
        ensure!(status == StatusCode::OK);
        // Naming a log another refund holds is refused at once.
        let (status, error) = merchant
            .post(
                &format!("/v1/refunds/{second}/mark_paid"),
                serde_json::to_vec(
                    &json!({"transaction_hash": REFUND_TX, "receipt_log_index": 7}),
                )?,
            )
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST, "{error}");
        ensure!(error["error"]["code"] == "transfer_already_used", "{error}");
        // Without a log the transaction is accepted, and verification finds its only log taken.
        let (status, _) = merchant
            .post(
                &format!("/v1/refunds/{second}/mark_paid"),
                mark_paid_body(REFUND_TX)?,
            )
            .await?;
        ensure!(status == StatusCode::OK);

        let paying = finalized(vec![transfer(
            FIXTURE_TREASURY,
            REFUND_DESTINATION,
            100,
            7,
        )?]);
        let worker = test_worker(
            pool,
            vec![paying.clone(), paying.clone()],
            vec![paying.clone(), paying],
        );
        let mut outcomes = vec![worker.check_once().await?, worker.check_once().await?];
        outcomes.sort_by_key(|outcome| format!("{outcome:?}"));
        ensure!(
            outcomes == [Verification::Failed, Verification::Succeeded],
            "{outcomes:?}"
        );
        let (_, refund) = merchant
            .call(Method::GET, &format!("/v1/refunds/{first}"), Vec::new())
            .await?;
        ensure!(
            refund["status"] == "succeeded" && refund["receipt_log_index"] == 7,
            "{refund}"
        );
        let (_, refund) = merchant
            .call(Method::GET, &format!("/v1/refunds/{second}"), Vec::new())
            .await?;
        ensure!(refund["status"] == "failed", "{refund}");
        ensure!(
            refund["failure_reason"] == "transfer_already_used",
            "{refund}"
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn only_an_unpaid_refund_is_canceled_and_missing_receipts_keep_the_reservation() -> Result<()>
{
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[48; 32]);
        let app = test_router(pool, &admin_key);
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        let deposit = seed_rejected_deposit(pool, merchant.account.id, "cancel", 150).await?;
        let first = merchant.refund(deposit, "100").await?;
        // A pending refund reserves its amount.
        let (status, error) = merchant
            .post(
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, "60")?,
            )
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST);
        ensure!(error["error"]["code"] == "amount_too_large", "{error}");
        ensure!(error["error"]["param"] == "amount_atomic", "{error}");

        for _ in 0..2 {
            let (status, canceled) = merchant
                .post(&format!("/v1/refunds/{first}/cancel"), Vec::new())
                .await?;
            ensure!(
                status == StatusCode::OK && canceled["status"] == "canceled",
                "{canceled}"
            );
        }
        let (status, error) = merchant
            .post(
                &format!("/v1/refunds/{first}/mark_paid"),
                mark_paid_body(REFUND_TX)?,
            )
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST);
        ensure!(
            error["error"]["code"] == "refund_unexpected_state",
            "{error}"
        );

        // Once marked paid, a refund cannot be canceled: its transaction could still pay it, and
        // a new refund would pay the deposit back twice. It keeps its reservation.
        let second = merchant.refund(deposit, "150").await?;
        let (status, _) = merchant
            .post(
                &format!("/v1/refunds/{second}/mark_paid"),
                mark_paid_body(REFUND_TX)?,
            )
            .await?;
        ensure!(status == StatusCode::OK);
        let (status, error) = merchant
            .post(&format!("/v1/refunds/{second}/cancel"), Vec::new())
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST, "{error}");
        ensure!(
            error["error"]["code"] == "refund_unexpected_state",
            "{error}"
        );
        let (status, error) = merchant
            .post(
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, "1")?,
            )
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST, "{error}");
        ensure!(error["error"]["code"] == "amount_too_large", "{error}");

        // Missing receipts cannot prove a drop, even after a sender and nonce were recorded.
        // Keep the reservation: a replacement may increment the nonce without paying a refund.
        let sender = Address::repeat_byte(0x5e);
        let missing = || vec![RefundReceipt::Missing];
        let worker = test_worker_with(pool, missing(), missing(), Some((sender, 7)));
        ensure!(worker.check_once().await? == Verification::Waiting);
        let second_id = topup::ids::parse(topup::ids::REFUND, &second).context("re_ id")?;
        ensure!(refund_status(pool, second_id).await? == "pending");
        let origin: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT tx_from, tx_nonce::text FROM refunds WHERE id = $1")
                .bind(second_id)
                .fetch_one(pool)
                .await?;
        ensure!(origin == (Some(format!("{sender:#x}")), Some("7".to_owned())));
        // Losing the transaction after its origin was recorded still cannot release funds.
        let worker = test_worker_with(pool, missing(), missing(), None);
        ensure!(worker.check_once().await? == Verification::Waiting);
        let (_, dropped) = merchant
            .call(Method::GET, &format!("/v1/refunds/{second}"), Vec::new())
            .await?;
        ensure!(
            dropped["status"] == "pending" && dropped["failure_reason"].is_null(),
            "{dropped}"
        );
        let (status, error) = merchant
            .post(
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, "1")?,
            )
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST && error["error"]["code"] == "amount_too_large");
        // The list pages newest first and filters by deposit and status.
        let deposit_id = format!("dep_{}", deposit.simple());
        let (status, list) = merchant
            .call(
                Method::GET,
                &format!("/v1/refunds?deposit={deposit_id}"),
                Vec::new(),
            )
            .await?;
        ensure!(status == StatusCode::OK, "{list}");
        let ids: Vec<&str> = list["data"]
            .as_array()
            .context("data")?
            .iter()
            .filter_map(|refund| refund["id"].as_str())
            .collect();
        ensure!(ids == [second.as_str(), first.as_str()], "{list}");
        let (_, canceled) = merchant
            .call(
                Method::GET,
                "/v1/refunds?status=canceled&limit=1",
                Vec::new(),
            )
            .await?;
        ensure!(canceled["has_more"] == false && canceled["data"][0]["id"] == first);
        let (_, failed) = merchant
            .call(Method::GET, "/v1/refunds?status=failed", Vec::new())
            .await?;
        ensure!(
            failed["has_more"] == false && failed["data"] == json!([]),
            "{failed}"
        );
        let (status, _) = merchant
            .call(Method::GET, "/v1/refunds?status=done", Vec::new())
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST);
        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit WHERE action LIKE 'refund_%' ORDER BY created_at, action",
        )
        .fetch_all(pool)
        .await?;
        ensure!(
            actions
                .iter()
                .filter(|action| *action == "refund.cancel")
                .count()
                == 1
        );
        ensure!(
            actions
                .iter()
                .filter(|action| *action == "refund.mark_paid")
                .count()
                == 1
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn a_refund_transaction_no_provider_ever_returned_fails_after_a_day() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[49; 32]);
        let app = test_router(pool, &admin_key);
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        let deposit = seed_rejected_deposit(pool, merchant.account.id, "unseen", 100).await?;
        let id = merchant.refund(deposit, "100").await?;
        merchant.mark_paid(&id, REFUND_TX).await?;
        let refund_id = topup::ids::parse(topup::ids::REFUND, &id).context("re_ id")?;
        let missing = || vec![RefundReceipt::Missing];

        // Unknown to both providers, and without a sender to prove it dropped: it waits.
        let worker = test_worker(pool, missing(), missing());
        ensure!(worker.check_once().await? == Verification::Waiting);
        ensure!(refund_status(pool, refund_id).await? == "pending");

        // A day after `mark_paid`, still never seen, it fails and releases the deposit.
        sqlx::query("UPDATE refunds SET paid_at = now() - interval '25 hours' WHERE id = $1")
            .bind(refund_id)
            .execute(pool)
            .await?;
        let worker = test_worker(pool, missing(), missing());
        ensure!(worker.check_once().await? == Verification::Failed);
        let (_, refund) = merchant
            .call(Method::GET, &format!("/v1/refunds/{id}"), Vec::new())
            .await?;
        ensure!(
            refund["status"] == "failed" && refund["failure_reason"] == "transaction_not_found",
            "{refund}"
        );
        merchant.refund(deposit, "100").await?;
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn refunds_take_back_the_credit_pro_rata_in_each_deposit_snapshot() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[51; 32]);
        let client_reads = Arc::new(ClientReadLimiter::default());
        let app = test_router_with(
            pool,
            &admin_key,
            Arc::new(StaticScreener::Listing),
            Arc::clone(&client_reads),
        );
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        let deposit = seed_deposit(
            pool,
            merchant.account.id,
            "partial",
            300,
            DepositState::Credited,
            None,
        )
        .await?;
        sqlx::query("UPDATE deposits SET credit_minor = 1000 WHERE id = $1")
            .bind(deposit)
            .execute(pool)
            .await?;

        // A third of the tokens takes back a third of the credit, rounded down: 333 of 1000.
        let first = merchant.refund(deposit, "100").await?;
        merchant.mark_paid(&first, REFUND_TX).await?;
        let paying = finalized(vec![transfer(
            FIXTURE_TREASURY,
            REFUND_DESTINATION,
            100,
            0,
        )?]);
        let worker = test_worker(pool, vec![paying.clone()], vec![paying]);
        ensure!(worker.check_once().await? == Verification::Succeeded);
        // The payer's view of the deposit's quote still states what the payment credited: a
        // refund takes credit back in the merchant's ledger, it does not change the credit.
        let quote: Uuid = sqlx::query_scalar(
            "SELECT address.quote_id FROM deposits AS deposit \
             JOIN addresses AS address ON address.id = deposit.address_id WHERE deposit.id = $1",
        )
        .bind(deposit)
        .fetch_one(pool)
        .await?;
        let secret = support::issue_client_secret(pool, &client_reads, quote).await?;
        let view = support::client_quote(&app, &secret).await?;
        ensure!(
            view["payment_status"] == "credited" && view["amount_credited"] == 1000,
            "{view}"
        );
        // The rest, paid by a second log of the same transaction, takes back all of it.
        let second = merchant.refund(deposit, "200").await?;
        merchant.mark_paid(&second, REFUND_TX).await?;
        let both = finalized(vec![
            transfer(FIXTURE_TREASURY, REFUND_DESTINATION, 100, 0)?,
            transfer(FIXTURE_TREASURY, REFUND_DESTINATION, 200, 1)?,
        ]);
        let worker = test_worker(pool, vec![both.clone()], vec![both]);
        ensure!(worker.check_once().await? == Verification::Succeeded);

        // Each `deposit.refunded` carries the cumulative amounts at its refund.
        let snapshots: Vec<Value> = sqlx::query_scalar(
            "SELECT data -> 'object' FROM events WHERE type = 'deposit.refunded' \
             ORDER BY created, id",
        )
        .fetch_all(pool)
        .await?;
        let mut cumulative: Vec<(Value, Value, Value)> = snapshots
            .iter()
            .map(|deposit| {
                (
                    deposit["amount_refunded_atomic"].clone(),
                    deposit["amount_refunded"].clone(),
                    deposit["amount_reversed"].clone(),
                )
            })
            .collect();
        cumulative.sort_by_key(|(atomic, _, _)| atomic.to_string());
        ensure!(
            cumulative
                == [
                    (json!("100"), json!(333), json!(0)),
                    (json!("300"), json!(1000), json!(0)),
                ],
            "{snapshots:?}"
        );
        let (_, current) = merchant
            .call(
                Method::GET,
                &format!("/v1/deposits/dep_{}", deposit.simple()),
                Vec::new(),
            )
            .await?;
        ensure!(
            current["amount"] == 1000
                && current["amount_refunded"] == 1000
                && current["refunded"] == true,
            "{current}"
        );
        let view = support::client_quote(&app, &secret).await?;
        ensure!(
            view["payment_status"] == "credited" && view["amount_credited"] == current["amount"],
            "{view}"
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn only_a_refundable_deposit_to_a_screened_destination_is_refunded() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[61; 32]);
        let app = test_router(pool, &admin_key);
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        let deposit = seed_rejected_deposit(pool, merchant.account.id, "screened", 100).await?;

        let (status, error) = merchant
            .post("/v1/refunds", refund_body(deposit, SANCTIONED, "100")?)
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST, "{error}");
        ensure!(
            error["error"]["code"] == "destination_sanctioned",
            "{error}"
        );
        ensure!(error["error"]["param"] == "destination_address", "{error}");
        // Screening that cannot answer refuses too, for a retry.
        let unscreened = Merchant {
            app: test_router_with(
                pool,
                &admin_key,
                Arc::new(StaticScreener::Unavailable),
                Arc::default(),
            ),
            ..merchant.clone()
        };
        let (status, _) = unscreened
            .post(
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, "100")?,
            )
            .await?;
        ensure!(status == StatusCode::SERVICE_UNAVAILABLE);
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM refunds")
                .fetch_one(pool)
                .await?
                == 0
        );

        // Not refundable: a deposit still in the pipeline, and sanctioned funds.
        let detected = seed_deposit(
            pool,
            merchant.account.id,
            "detected",
            100,
            DepositState::Detected,
            None,
        )
        .await?;
        let sanctioned = seed_deposit(
            pool,
            merchant.account.id,
            "sanctioned",
            100,
            DepositState::Rejected,
            Some(RejectReason::Sanctioned),
        )
        .await?;
        for deposit in [detected, sanctioned] {
            let (status, error) = merchant
                .post(
                    "/v1/refunds",
                    refund_body(deposit, REFUND_DESTINATION, "100")?,
                )
                .await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{error}");
            ensure!(
                error["error"]["code"] == "deposit_not_refundable",
                "{error}"
            );
        }
        // A credited deposit the merchant chooses to refund.
        let credited = seed_deposit(
            pool,
            merchant.account.id,
            "credited",
            100,
            DepositState::Credited,
            None,
        )
        .await?;
        merchant.refund(credited, "100").await?;
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// The dust floor of a deposit its binding does not accept is the default of the route version
/// it was recorded on, so raising the default later never strands it (design §9).
#[tokio::test]
async fn a_raised_refund_floor_does_not_strand_a_deposit_recorded_before() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[62; 32]);
        // The account accepts nothing: the deposit is `asset_not_accepted` on version 1, whose
        // floor of 20 its 100 clears.
        let merchant = Merchant::seed(pool, &test_router(pool, &admin_key), "phala-cloud").await?;
        let deposit = seed_deposit(
            pool,
            merchant.account.id,
            "not-accepted",
            100,
            DepositState::Rejected,
            Some(RejectReason::AssetNotAccepted),
        )
        .await?;
        // Version 2 raises the default floor past the deposit.
        let mut raised = route_fixture();
        raised.version = 2;
        raised.merchant.min_refund_atomic =
            topup_core::route::Bounded::at(AtomicAmount::new(U256::from(1_000_u64)));
        let merchant = Merchant {
            app: test_router_on(
                pool,
                &admin_key,
                Arc::new(StaticScreener::Listing),
                Arc::default(),
                vec![route_fixture(), raised],
            ),
            ..merchant
        };
        merchant.refund(deposit, "100").await?;
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn a_deposit_that_could_still_be_reversed_is_not_refunded() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[64; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        let deposit =
            seed_rejected_deposit(&database.app_pool, product.id, "not-final-yet", 100).await?;
        sqlx::query("UPDATE deposits SET final_at = NULL WHERE id = $1")
            .bind(deposit)
            .execute(&database.app_pool)
            .await?;
        let app = test_router(&database.app_pool, &admin_key);
        let response = app
            .clone()
            .oneshot(merchant_request(
                Method::POST,
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, "100")?,
                &product_key,
            ))
            .await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
        ensure!(response_json(response).await?["error"]["code"] == "deposit_not_final");

        sqlx::query("UPDATE deposits SET final_at = now() WHERE id = $1")
            .bind(deposit)
            .execute(&database.app_pool)
            .await?;
        let response = app
            .oneshot(merchant_request(
                Method::POST,
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, "100")?,
                &product_key,
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn refund_idempotency_keys_replay_and_refuse_other_parameters() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[72; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        let deposit = seed_rejected_deposit(&database.app_pool, product.id, "keyed", 150).await?;
        let app = test_router(&database.app_pool, &admin_key);
        let request = |body: Vec<u8>| {
            support::merchant_request_with_key(
                Method::POST,
                "/v1/refunds",
                body,
                &product_key,
                "\"refund-1\"",
            )
        };
        // Without an amount the whole unrefunded remainder is requested.
        let body = serde_json::to_vec(&json!({
            "deposit": format!("dep_{}", deposit.simple()),
            "destination_address": REFUND_DESTINATION,
        }))?;
        let first = app.clone().oneshot(request(body.clone())).await?;
        ensure!(first.status() == StatusCode::OK);
        let first = response_json(first).await?;
        ensure!(first["amount_atomic"] == "150", "{first}");
        let repeat = app.clone().oneshot(request(body)).await?;
        ensure!(repeat.headers()["idempotent-replayed"] == "true");
        ensure!(response_json(repeat).await? == first);
        let other = app
            .clone()
            .oneshot(request(refund_body(
                deposit,
                "0x6666666666666666666666666666666666666666",
                "150",
            )?))
            .await?;
        ensure!(other.status() == StatusCode::BAD_REQUEST);
        ensure!(response_json(other).await?["error"]["type"] == "idempotency_error");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM refunds")
            .fetch_one(&database.app_pool)
            .await?;
        ensure!(count == 1);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Batched refund pages preserve both cursor directions and filters, with the same objects
/// returned by individual reads, including when creation timestamps tie.
#[tokio::test]
async fn refund_lists_page_with_stripe_cursors_and_match_individual_reads() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            seed::initialize_dual_chain(pool, 1).await?;
            let admin_key = SigningKey::from_bytes(&[49; 32]);
            let app = test_router(pool, &admin_key);
            let merchant = Merchant::seed(pool, &app, "refund-pages").await?;
            let first = seed_rejected_deposit(pool, merchant.account.id, "first", 100).await?;
            let second = seed_rejected_deposit(pool, merchant.account.id, "second", 100).await?;
            let base = Utc::now() - Duration::hours(1);
            let mut refunds = Vec::new();
            for number in 1..=12 {
                for deposit in [first, second] {
                    let id = merchant.refund(deposit, &number.to_string()).await?;
                    let refund_status = if number % 2 == 0 {
                        "pending"
                    } else {
                        let (status, canceled) = merchant
                            .post(&format!("/v1/refunds/{id}/cancel"), Vec::new())
                            .await?;
                        ensure!(status == StatusCode::OK, "{canceled}");
                        "canceled"
                    };
                    let created_at = base + Duration::seconds(number / 2);
                    let uuid = topup::ids::parse(topup::ids::REFUND, &id).context("refund id")?;
                    sqlx::query("UPDATE refunds SET created_at = $2, metadata = $3 WHERE id = $1")
                        .bind(uuid)
                        .bind(created_at)
                        .bind(json!({"number": number.to_string(), "deposit": deposit.to_string()}))
                        .execute(pool)
                        .await?;
                    let (status, object) = merchant
                        .call(Method::GET, &format!("/v1/refunds/{id}"), Vec::new())
                        .await?;
                    ensure!(status == StatusCode::OK, "{object}");
                    ensure!(object["status"] == refund_status, "{object}");
                    refunds.push((created_at, uuid, deposit, refund_status, object));
                }
            }
            refunds.sort_by_key(|refund| std::cmp::Reverse((refund.0, refund.1)));

            for deposit in [None, Some(first), Some(second)] {
                for refund_status in [None, Some("pending"), Some("canceled")] {
                    let expected: Vec<Value> = refunds
                        .iter()
                        .filter(|(_, _, row_deposit, row_status, _)| {
                            deposit.is_none_or(|deposit| deposit == *row_deposit)
                                && refund_status.is_none_or(|status| status == *row_status)
                        })
                        .map(|(_, _, _, _, object)| object.clone())
                        .collect();
                    let pages = expected.len().div_ceil(2);
                    ensure!(pages >= 3);
                    let mut query = "limit=2".to_owned();
                    if let Some(deposit) = deposit {
                        query.push_str(&format!("&deposit=dep_{}", deposit.simple()));
                    }
                    if let Some(status) = refund_status {
                        query.push_str(&format!("&status={status}"));
                    }
                    // Each scenario gets a fresh request limiter so elapsed time does not
                    // determine whether the pagination checks reach the API.
                    let merchant = Merchant {
                        app: test_router(pool, &admin_key),
                        ..merchant.clone()
                    };
                    let mut cursor = None;
                    let mut forward = Vec::new();
                    let mut last_page = Vec::new();
                    for page_number in 0..pages {
                        let mut path = format!("/v1/refunds?{query}");
                        if let Some(id) = &cursor {
                            path.push_str(&format!("&starting_after={id}"));
                        }
                        let (status, page) = merchant.call(Method::GET, &path, Vec::new()).await?;
                        ensure!(status == StatusCode::OK, "{path}: {page}");
                        ensure!(page["object"] == "list" && page["url"] == "/v1/refunds");
                        ensure!(
                            page["has_more"] == (page_number + 1 < pages),
                            "{path}: {page}"
                        );
                        let data = page["data"].as_array().context("refund page data")?;
                        ensure!(data.len() == 2, "{path}: {page}");
                        cursor = Some(
                            data.last().context("last refund")?["id"]
                                .as_str()
                                .context("refund id")?
                                .to_owned(),
                        );
                        forward.extend(data.iter().cloned());
                        last_page = data.clone();
                    }
                    ensure!(
                        forward == expected,
                        "forward order or GET objects differ: {query}"
                    );

                    // Walk from the final forward page back to the newest page. Each
                    // ending_before page stays newest first and is prepended to the result.
                    let mut backward = last_page;
                    for page_number in 1..pages {
                        let cursor = backward.first().context("first refund")?["id"]
                            .as_str()
                            .context("refund id")?;
                        let path = format!("/v1/refunds?{query}&ending_before={cursor}");
                        let (status, page) = merchant.call(Method::GET, &path, Vec::new()).await?;
                        ensure!(status == StatusCode::OK, "{path}: {page}");
                        ensure!(
                            page["has_more"] == (page_number + 1 < pages),
                            "{path}: {page}"
                        );
                        let mut data = page["data"].as_array().context("refund page data")?.clone();
                        ensure!(data.len() == 2, "{path}: {page}");
                        data.extend(backward);
                        backward = data;
                    }
                    ensure!(
                        backward == expected,
                        "backward order or GET objects differ: {query}"
                    );
                }
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn deposit_lists_page_with_stripe_cursors() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[48; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        seed_same_address_deposits(&database.app_pool, product.id, 52).await?;
        let app = test_router(&database.app_pool, &admin_key);
        let list = |query: String, key: &str| {
            let app = app.clone();
            let request = merchant_request(
                Method::GET,
                &format!("/v1/deposits{query}"),
                Vec::new(),
                key,
            );
            async move {
                let response = app.oneshot(request).await?;
                let status = response.status();
                anyhow::Ok((status, response_json(response).await?))
            }
        };
        let ids = |page: &Value| -> Result<Vec<String>> {
            page["data"]
                .as_array()
                .with_context(|| format!("no data in {page}"))?
                .iter()
                .map(|deposit| deposit["id"].as_str().map(str::to_owned).context("id"))
                .collect()
        };

        let (status, first) = list("?limit=50".to_owned(), &product_key).await?;
        ensure!(status == StatusCode::OK, "{first}");
        ensure!(first["object"] == "list" && first["url"] == "/v1/deposits");
        ensure!(first["has_more"] == true);
        let first_ids = ids(&first)?;
        ensure!(first_ids.len() == 50);
        let last = first_ids.last().context("last")?;
        let (_, second) = list(format!("?limit=50&starting_after={last}"), &product_key).await?;
        let second_ids = ids(&second)?;
        ensure!(second_ids.len() == 2 && second["has_more"] == false);
        ensure!(second_ids.iter().all(|id| !first_ids.contains(id)));
        // Paging back from the second page returns the end of the first, in the same order.
        let (_, back) = list(
            format!("?limit=3&ending_before={}", second_ids[0]),
            &product_key,
        )
        .await?;
        ensure!(ids(&back)? == first_ids[47..]);
        ensure!(back["has_more"] == true);
        let (_, default) = list(String::new(), &product_key).await?;
        ensure!(ids(&default)?.len() == 10);

        for (query, param) in [
            ("?limit=101", "limit"),
            ("?status=paid", "status"),
            ("?expand[]=quote", "expand"),
            ("?color=red", "color"),
        ] {
            let (status, error) = list(query.to_owned(), &product_key).await?;
            ensure!(status == StatusCode::BAD_REQUEST, "{query}");
            ensure!(error["error"]["param"] == param, "{query}: {error}");
        }
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn evm_reader_reads_every_transfer_only_at_finality_and_times_out() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    db::chain_reads::advance_checkpoint(
        &database.app_pool,
        1,
        db::chain_reads::Boundary {
            number: 100,
            hash: B256::from(U256::from(100_u64)),
            time: Utc::now(),
        },
    )
    .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/", post(refund_rpc))).await
    });
    let reader = EvmRefundChainReader::new(
        database.app_pool.clone(),
        BTreeMap::from([(
            1,
            Arc::new(EvmClient::with_timeout(
                &format!("http://{address}"),
                StdDuration::from_millis(50),
            )?),
        )]),
    );
    let token = route_fixture().asset.contract;
    // Scenario 1 transfers another token, 4 is above `finalized`, 5 reverted.
    for scenario in [1_u64, 2, 3, 5] {
        let receipt = reader.receipt(1, B256::from(U256::from(scenario))).await?;
        let RefundReceipt::Finalized {
            block_number,
            succeeded,
            transfers,
            ..
        } = receipt
        else {
            anyhow::bail!("scenario {scenario}: expected a finalized receipt");
        };
        ensure!(block_number == 90 && succeeded == (scenario != 5));
        let [transfer] = transfers.as_slice() else {
            anyhow::bail!("scenario {scenario}: expected one transfer");
        };
        // The log's position in its receipt, not its block-wide `logIndex` (7).
        ensure!(transfer.receipt_log_index == 0 && transfer.amount == U256::from(100_u64));
        ensure!((transfer.token == token) == (scenario != 1));
    }
    ensure!(reader.receipt(1, B256::from(U256::from(4_u64))).await? == RefundReceipt::Pending);
    ensure!(reader.receipt(1, B256::from(U256::from(7_u64))).await? == RefundReceipt::Missing);
    ensure!(
        reader.origin(1, B256::from(U256::from(2_u64))).await?
            == Some((Address::from_str(REFUND_SENDER)?, 5))
    );
    ensure!(
        reader
            .origin(1, B256::from(U256::from(7_u64)))
            .await?
            .is_none()
    );
    ensure!(matches!(
        reader.receipt(1, B256::from(U256::from(6_u64))).await,
        Err(RefundReadError::Rpc(_))
    ));
    ensure!(matches!(
        reader.receipt(2, B256::ZERO).await,
        Err(RefundReadError::UnknownChain(2))
    ));
    server.abort();
    let _ = server.await;
    Ok(())
}

#[tokio::test]
async fn worker_shutdown_cancels_a_hung_read() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[50; 32]);
        let app = test_router(pool, &admin_key);
        let merchant = Merchant::seed(pool, &app, "phala-cloud").await?;
        let deposit = seed_rejected_deposit(pool, merchant.account.id, "hung-worker", 100).await?;
        let id = merchant.refund(deposit, "100").await?;
        merchant
            .post(
                &format!("/v1/refunds/{id}/mark_paid"),
                mark_paid_body(REFUND_TX)?,
            )
            .await?;
        let started = Arc::new(Notify::new());
        let worker = RefundVerificationWorker::new(
            pool.clone(),
            Arc::default(),
            HangingReader {
                started: Arc::clone(&started),
            },
            HangingReader {
                started: Arc::new(Notify::new()),
            },
            RefundVerificationConfig {
                poll_interval: StdDuration::from_secs(60),
                retry_interval: StdDuration::ZERO,
                observe_timeout: StdDuration::from_secs(60),
            },
        );
        let cancellation = CancellationToken::new();
        let child = cancellation.clone();
        let task = tokio::spawn(async move { worker.run(child).await });
        started.notified().await;
        cancellation.cancel();
        tokio::time::timeout(StdDuration::from_millis(100), task).await??;
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn admin_nudge_reschedules_only_a_deposit_the_pump_claims() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[53; 32]);
        let (product, _) = seed_product(&database.app_pool, "phala-cloud").await?;
        let pool = &database.app_pool;
        seed::initialize_dual_chain(pool, 1).await?;
        let seed = |customer: &'static str, state| {
            seed_deposit(pool, product.id, customer, 100, state, None)
        };
        let detected = seed("nudge-detected", DepositState::Detected).await?;
        let credited = seed("nudge-credited", DepositState::Credited).await?;
        let rejected = seed_rejected_deposit(pool, product.id, "nudge-rejected", 100).await?;
        sqlx::query("UPDATE deposits SET next_attempt_at = now() + interval '1 day'")
            .execute(pool)
            .await?;
        let app = test_router(pool, &admin_key);
        let now = Utc::now().timestamp();
        let nudge = |path: String, created: i64| {
            app.clone().oneshot(signed_request(
                Method::POST,
                &path,
                Vec::new(),
                ADMIN_KID,
                &admin_key,
                created,
            ))
        };

        // The deposit is named by its `dep_` id, the only form; a bare UUID names nothing.
        let response = nudge(
            format!("/v1/admin/deposits/dep_{}/nudge", detected.simple()),
            now - 1,
        )
        .await?;
        ensure!(response.status() == StatusCode::OK);
        let nudged = response_json(response).await?;
        ensure!(
            nudged["deposit_id"] == format!("dep_{}", detected.simple()),
            "{nudged}"
        );
        let bare = nudge(format!("/v1/admin/deposits/{detected}/nudge"), now).await?;
        ensure!(bare.status() == StatusCode::NOT_FOUND);
        let (state, due): (String, bool) =
            sqlx::query_as("SELECT state, next_attempt_at <= now() FROM deposits WHERE id = $1")
                .bind(detected)
                .fetch_one(pool)
                .await?;
        ensure!(state == "detected" && due);

        // A credited deposit waits for its sweep and a rejected one is final: the pump claims
        // neither, so a nudge is refused rather than silently doing nothing.
        for (deposit, state, created) in [
            (credited, "credited", now + 1),
            (rejected, "rejected", now + 2),
        ] {
            let response = nudge(
                format!("/v1/admin/deposits/dep_{}/nudge", deposit.simple()),
                created,
            )
            .await?;
            ensure!(response.status() == StatusCode::BAD_REQUEST);
            let error = response_json(response).await?;
            ensure!(
                error["error"]["code"] == "deposit_unexpected_state",
                "{error}"
            );
            ensure!(
                error["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains(state)),
                "{error}"
            );
        }
        let audits: Vec<String> = sqlx::query_scalar(
            "SELECT subject FROM audit WHERE action = 'deposit_nudged' ORDER BY subject",
        )
        .fetch_all(pool)
        .await?;
        ensure!(
            audits == vec![format!("deposit:dep_{}", detected.simple())],
            "{audits:?}"
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn admin_daily_report_uses_seeded_integer_facts() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[52; 32]);
        let (product, _) = seed_product(&database.app_pool, "phala-cloud").await?;
        let rejected = seed_rejected_deposit(&database.app_pool, product.id, "report-rejected", 100).await?;
        let unsupported =
            seed_rejected_deposit(&database.app_pool, product.id, "report-unsupported", 25).await?;
        let credited = seed_deposit(
            &database.app_pool,
            product.id,
            "report-credited",
            200,
            DepositState::Credited,
            None,
        )
        .await?;
        sqlx::query(
            r#"
            UPDATE deposits
            SET route = NULL,
                route_version = NULL,
                reason = 'unsupported_asset',
                asset_contract = '0x7777777777777777777777777777777777777777'
            WHERE id = $1
            "#,
        )
        .bind(unsupported)
        .execute(&database.app_pool)
        .await?;
        sqlx::query("UPDATE deposits SET next_attempt_at = now() + interval '1 day', created_at = now() - interval '2 hours' WHERE id = ANY($1)")
            .bind([rejected, credited])
            .execute(&database.app_pool)
            .await?;
        // The credited deposit's fulfillment event is still waiting for the account's endpoint.
        sqlx::query(
            r#"
            WITH event AS (
                INSERT INTO events (id, account_id, livemode, type, object_type, object_id,
                                    created, actor, data)
                VALUES ($1, $2, true, 'deposit.credited', 'deposit', $3, now() - interval '1 hour',
                        'system',
                        '{"object": {"receipt_log_index": 0, "revision": 0,
                                     "block_hash": "0x", "block_time": 0}}')
                RETURNING id, account_id
            )
            INSERT INTO webhook_deliveries (event_id, endpoint_id, next_attempt_at)
            SELECT event.id, endpoint.id, now()
            FROM event
            JOIN webhook_endpoints AS endpoint ON endpoint.account_id = event.account_id
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(product.id)
        .bind(credited)
        .execute(&database.app_pool)
        .await?;
        // A merchant swept 120 of the route's token from the credited deposit's forwarder.
        sqlx::query(
            r#"
            INSERT INTO flushed
                (chain_id, tx_hash, log_index, address_id, token, treasury, amount_atomic,
                 block_number, block_hash)
            SELECT deposit.chain_id, '0x' || repeat('ab', 32), 0, deposit.address_id,
                   deposit.asset_contract, address.treasury, 120, deposit.block_number + 1,
                   '0x' || repeat('cd', 32)
            FROM deposits AS deposit
            JOIN addresses AS address ON address.id = deposit.address_id
            WHERE deposit.id = $1
            "#,
        )
        .bind(credited)
        .execute(&database.app_pool)
        .await?;
        seed_open_lock(&database.app_pool, product.id).await?;
        seed_expired_lock(&database.app_pool, product.id).await?;
        seed_refund_row(&database.app_pool, rejected, 20).await?;
        // The report sums the open reserved locks, as lock creation would have reserved them.
        sqlx::query("UPDATE quotes SET exposure_reserved = true WHERE expires_at > now()")
            .execute(&database.app_pool)
            .await?;

        let app = test_router(&database.app_pool, &admin_key);
        let now = Utc::now().timestamp();
        let response = app
            .oneshot(signed_request(
                Method::GET,
                "/v1/admin/reports/daily",
                Vec::new(),
                ADMIN_KID,
                &admin_key,
                now + 1,
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let report = response_json(response).await?;
        ensure!(report["exposure_minor"] == "50");
        let routes = report["routes"].as_array().context("report routes")?;
        let route = routes
            .iter()
            .find(|route| route["route"] == "phala-cloud-ethereum-pha-usd")
            .context("configured route report")?;
        ensure!(route["route"] == "phala-cloud-ethereum-pha-usd");
        // Forwarders hold deposits that are not reversed minus what finalized sweeps moved.
        ensure!(route["unflushed_balance_atomic"] == "180");
        ensure!(route["open_rate_lock_exposure_atomic"] == "50");
        ensure!(route["rejected_holds_atomic"] == "100");
        ensure!(route["deposits_by_state"]["rejected"] == 1);
        ensure!(route["deposits_by_state"]["credited"] == 1);
        ensure!(route["credited_undelivered"] == 1);
        ensure!(
            route["credited_undelivered_max_age_seconds"]
                .as_u64()
                .context("undelivered age")?
                >= 3_600
        );
        ensure!(route["refunds_by_status"]["pending"] == 1);
        ensure!(route["age_in_state_max_seconds"]["credited"].as_u64().context("credited age")? >= 7_000);
        ensure!(route.get("flush_planning").is_none());
        let unrouted = routes
            .iter()
            .find(|route| {
                route["route"]
                    == "unrouted:1:0x7777777777777777777777777777777777777777"
            })
            .context("unrouted asset report")?;
        ensure!(unrouted["unflushed_balance_atomic"] == "25");
        ensure!(unrouted["rejected_holds_atomic"] == "25");
        ensure!(unrouted["deposits_by_state"]["rejected"] == 1);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

async fn refund_rpc(Json(request): Json<Value>) -> Json<Value> {
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or_default();
    let unknown = |tx_hash: &str| tx_hash == format!("{:#x}", B256::from(U256::from(7_u64)));
    if method == "eth_getTransactionByHash" {
        let tx_hash = request["params"][0].as_str().unwrap_or_default();
        let transaction = json!({
            "hash": tx_hash,
            "nonce": "0x5",
            "blockHash": format!("{:#x}", B256::from(U256::from(900_u64))),
            "blockNumber": "0x5a",
            "transactionIndex": "0x0",
            "from": REFUND_SENDER,
            "to": "0x6c5ba91642f10282b576d91922ae6448c9d52f4e",
            "value": "0x0",
            "gas": "0x0",
            "maxFeePerGas": "0x0",
            "maxPriorityFeePerGas": "0x0",
            "gasPrice": "0x0",
            "input": "0x",
            "chainId": "0x1",
            "type": "0x2",
            "accessList": [],
            "v": "0x0",
            "yParity": "0x0",
            "r": "0x1",
            "s": "0x1",
        });
        let result = if unknown(tx_hash) {
            Value::Null
        } else {
            transaction
        };
        return Json(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
    if method == "eth_getTransactionCount" {
        return Json(json!({"jsonrpc": "2.0", "id": id, "result": "0x6"}));
    }
    if method == "eth_getTransactionReceipt" {
        let tx_hash = request["params"][0].as_str().unwrap_or_default();
        if tx_hash == format!("{:#x}", B256::from(U256::from(6_u64))) {
            tokio::time::sleep(StdDuration::from_millis(250)).await;
        }
        let receipt = if unknown(tx_hash) {
            Value::Null
        } else {
            refund_receipt(tx_hash)
        };
        return Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": receipt,
        }));
    }
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": finalized_block(),
    }))
}

fn refund_receipt(tx_hash: &str) -> Value {
    let scenario = (1_u64..=6)
        .find(|value| tx_hash == format!("{:#x}", B256::from(U256::from(*value))))
        .unwrap_or_default();
    let asset = if scenario == 1 {
        "0x7777777777777777777777777777777777777777"
    } else {
        "0x6c5ba91642f10282b576d91922ae6448c9d52f4e"
    };
    let from = if scenario == 2 {
        "0x3333333333333333333333333333333333333333"
    } else {
        "0x0000000000000000000000000000000000007ea5"
    };
    let to = if scenario == 3 {
        "0x5555555555555555555555555555555555555555"
    } else {
        REFUND_DESTINATION
    };
    let block_number = if scenario == 4 { "0x65" } else { "0x5a" };
    let status = if scenario == 5 { "0x0" } else { "0x1" };
    json!({
        "transactionHash": tx_hash,
        "transactionIndex": "0x0",
        "blockHash": format!("{:#x}", B256::from(U256::from(900_u64))),
        "blockNumber": block_number,
        "from": from,
        "to": to,
        "cumulativeGasUsed": "0x5208",
        "gasUsed": "0x5208",
        "contractAddress": null,
        "logs": [{
            "address": asset,
            "topics": [
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                address_topic(from),
                address_topic(to)
            ],
            "data": format!("0x{:064x}", 100_u64),
            "blockNumber": block_number,
            "transactionHash": tx_hash,
            "transactionIndex": "0x0",
            "blockHash": format!("{:#x}", B256::from(U256::from(900_u64))),
            "logIndex": "0x7",
            "removed": false
        }],
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "status": status,
        "type": "0x2",
        "effectiveGasPrice": "0x1"
    })
}

fn finalized_block() -> Value {
    json!({
        "hash": format!("{:#x}", B256::from(U256::from(100_u64))),
        "parentHash": format!("{:#x}", B256::from(U256::from(99_u64))),
        "sha3Uncles": format!("{:#x}", B256::ZERO),
        "miner": "0x0000000000000000000000000000000000000000",
        "stateRoot": format!("{:#x}", B256::ZERO),
        "transactionsRoot": format!("{:#x}", B256::ZERO),
        "receiptsRoot": format!("{:#x}", B256::ZERO),
        "logsBloom": format!("0x{}", "00".repeat(256)),
        "difficulty": "0x0",
        "number": "0x64",
        "gasLimit": "0x1c9c380",
        "gasUsed": "0x0",
        "timestamp": "0x0",
        "extraData": "0x",
        "mixHash": format!("{:#x}", B256::ZERO),
        "nonce": "0x0000000000000000",
        "transactions": [],
        "uncles": []
    })
}

fn address_topic(address: &str) -> String {
    format!("0x{:0>64}", address.trim_start_matches("0x"))
}

/// One account's API calls with its secret key.
#[derive(Clone)]
struct Merchant {
    app: Router,
    account: Account,
    key: String,
}

impl Merchant {
    async fn seed(pool: &sqlx::PgPool, app: &Router, name: &str) -> Result<Self> {
        let (account, key) = seed_product(pool, name).await?;
        Ok(Self {
            app: app.clone(),
            account,
            key,
        })
    }

    async fn call(&self, method: Method, path: &str, body: Vec<u8>) -> Result<(StatusCode, Value)> {
        let response = self
            .app
            .clone()
            .oneshot(merchant_request(method, path, body, &self.key))
            .await?;
        let status = response.status();
        Ok((status, response_json(response).await?))
    }

    async fn post(&self, path: &str, body: Vec<u8>) -> Result<(StatusCode, Value)> {
        self.call(Method::POST, path, body).await
    }

    /// Creates a refund to the test destination and returns its `re_` id.
    async fn refund(&self, deposit: Uuid, amount: &str) -> Result<String> {
        let (status, refund) = self
            .post(
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, amount)?,
            )
            .await?;
        ensure!(status == StatusCode::OK, "{refund}");
        ensure!(refund["status"] == "pending", "{refund}");
        Ok(refund["id"].as_str().context("refund id")?.to_owned())
    }

    /// Attaches `transaction_hash` to refund `id`.
    async fn mark_paid(&self, id: &str, transaction_hash: &str) -> Result<()> {
        let (status, refund) = self
            .post(
                &format!("/v1/refunds/{id}/mark_paid"),
                mark_paid_body(transaction_hash)?,
            )
            .await?;
        ensure!(status == StatusCode::OK, "{refund}");
        Ok(())
    }

    async fn expect_paused(&self, deposit: Uuid) -> Result<()> {
        let (status, error) = self
            .post(
                "/v1/refunds",
                refund_body(deposit, REFUND_DESTINATION, "100")?,
            )
            .await?;
        ensure!(status == StatusCode::BAD_REQUEST, "{error}");
        ensure!(error["error"]["code"] == "paused", "{error}");
        Ok(())
    }
}

fn mark_paid_body(transaction_hash: &str) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &json!({ "transaction_hash": transaction_hash }),
    )?)
}

/// A transfer of the route fixture's token, at `receipt_log_index` in its receipt.
fn transfer(
    from: Address,
    to: &str,
    amount: u64,
    receipt_log_index: u64,
) -> Result<RefundTransfer> {
    Ok(RefundTransfer {
        receipt_log_index,
        token: route_fixture().asset.contract,
        from,
        to: Address::from_str(to)?,
        amount: U256::from(amount),
    })
}

fn finalized(transfers: Vec<RefundTransfer>) -> RefundReceipt {
    RefundReceipt::Finalized {
        block_number: 90,
        block_hash: B256::repeat_byte(0x09),
        succeeded: true,
        transfers,
    }
}

async fn refund_status(pool: &sqlx::PgPool, id: Uuid) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT status FROM refunds WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

/// One provider answering with a script of receipts, in order, and with a fixed view of the
/// transaction's sender and nonce and of the sender's nonce at `finalized`.
struct ScriptedReader {
    receipts: Mutex<VecDeque<RefundReceipt>>,
    origin: Option<(Address, u64)>,
}

#[async_trait]
impl RefundChainReader for ScriptedReader {
    async fn receipt(
        &self,
        chain_id: u64,
        tx_hash: B256,
    ) -> Result<RefundReceipt, RefundReadError> {
        assert_eq!(chain_id, 1);
        assert_eq!(tx_hash, B256::from_str(REFUND_TX).expect("valid refund tx"));
        self.receipts
            .lock()
            .map_err(|_| RefundReadError::Rpc("script lock"))?
            .pop_front()
            .ok_or(RefundReadError::Rpc("script exhausted"))
    }

    async fn origin(
        &self,
        _chain_id: u64,
        _tx_hash: B256,
    ) -> Result<Option<(Address, u64)>, RefundReadError> {
        Ok(self.origin)
    }
}

struct HangingReader {
    started: Arc<Notify>,
}

#[async_trait]
impl RefundChainReader for HangingReader {
    async fn receipt(
        &self,
        _chain_id: u64,
        _tx_hash: B256,
    ) -> Result<RefundReceipt, RefundReadError> {
        self.started.notify_one();
        std::future::pending().await
    }

    async fn origin(
        &self,
        _chain_id: u64,
        _tx_hash: B256,
    ) -> Result<Option<(Address, u64)>, RefundReadError> {
        std::future::pending().await
    }
}

fn test_worker(
    pool: &sqlx::PgPool,
    primary: Vec<RefundReceipt>,
    secondary: Vec<RefundReceipt>,
) -> RefundVerificationWorker<ScriptedReader, ScriptedReader> {
    test_worker_with(pool, primary, secondary, None)
}

/// [`test_worker`] whose providers both return the transaction as sent by `origin`, and the
/// sender's nonce at `finalized` as `finalized_nonce`.
fn test_worker_with(
    pool: &sqlx::PgPool,
    primary: Vec<RefundReceipt>,
    secondary: Vec<RefundReceipt>,
    origin: Option<(Address, u64)>,
) -> RefundVerificationWorker<ScriptedReader, ScriptedReader> {
    let reader = |receipts: Vec<RefundReceipt>| ScriptedReader {
        receipts: Mutex::new(receipts.into()),
        origin,
    };
    RefundVerificationWorker::new(
        pool.clone(),
        Arc::new(topup::routes::RouteSet::new(vec![route_fixture()]).expect("route fixture loads")),
        reader(primary),
        reader(secondary),
        RefundVerificationConfig {
            poll_interval: StdDuration::ZERO,
            retry_interval: StdDuration::ZERO,
            observe_timeout: StdDuration::from_secs(1),
        },
    )
}

/// Screening that lists [`SANCTIONED`], or that cannot answer.
enum StaticScreener {
    Listing,
    Unavailable,
}

#[async_trait]
impl DestinationScreener for StaticScreener {
    async fn screen(&self, route: &RouteFile, destination: Address) -> DestinationScreening {
        assert_eq!(route.chain.chain_id, 1);
        match self {
            Self::Unavailable => DestinationScreening::Unavailable,
            Self::Listing if destination == Address::from_str(SANCTIONED).expect("address") => {
                DestinationScreening::Sanctioned
            }
            Self::Listing => DestinationScreening::Clear,
        }
    }
}

fn test_router(pool: &sqlx::PgPool, admin_key: &SigningKey) -> Router {
    test_router_with(
        pool,
        admin_key,
        Arc::new(StaticScreener::Listing),
        Arc::default(),
    )
}

fn test_router_with(
    pool: &sqlx::PgPool,
    admin_key: &SigningKey,
    screening: Arc<dyn DestinationScreener>,
    client_reads: Arc<ClientReadLimiter>,
) -> Router {
    test_router_on(
        pool,
        admin_key,
        screening,
        client_reads,
        vec![route_fixture()],
    )
}

fn test_router_on(
    pool: &sqlx::PgPool,
    admin_key: &SigningKey,
    screening: Arc<dyn DestinationScreener>,
    client_reads: Arc<ClientReadLimiter>,
    routes: Vec<RouteFile>,
) -> Router {
    let state = AppState {
        pool: pool.clone(),
        routes: Arc::new(topup::routes::RouteSet::new(routes).expect("routes load")),
        maintenance_keys: Vec::new(),
        admin_key: VerificationKey::from_base64(
            ADMIN_KID.to_owned(),
            &public_key_base64(admin_key),
        )
        .expect("admin key is valid"),
        public_origin: PublicOrigin::parse(TEST_ORIGIN).expect("test origin is valid"),
        attestor: Arc::new(DstackAttestor::new()),
        rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
        client_reads,
        rate_limits: Arc::default(),
        screening,
        contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
    };
    topup::api::router(state).0
}

/// Delivers every pending event of `account_id` to a local receiver and returns the bodies.
async fn deliver_events(pool: &sqlx::PgPool, account_id: Uuid) -> Result<Vec<Value>> {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&received);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/", listener.local_addr()?);
    let receiver = Router::new().route(
        "/",
        post(move |Json(body): Json<Value>| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().expect("receiver lock").push(body);
                StatusCode::OK
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, receiver).await });
    sqlx::query("UPDATE webhook_endpoints SET url = $2 WHERE account_id = $1")
        .bind(account_id)
        .bind(&url)
        .execute(pool)
        .await?;
    let worker = DeliveryWorker::new(
        pool.clone(),
        Arc::new(WebhookSigner(SigningKey::from_bytes(&[11; 32]))),
        true,
        DeliveryConfig {
            request_timeout: StdDuration::from_secs(2),
            claim_lease: StdDuration::from_secs(30),
            poll_interval: StdDuration::from_millis(10),
            response_body_limit: 16,
            age_alert_threshold: StdDuration::from_secs(60),
            ..DeliveryConfig::default()
        },
    )?;
    for _ in 0..20 {
        if worker.run_once().await? == 0 {
            break;
        }
    }
    server.abort();
    let _ = server.await;
    let bodies = received.lock().expect("receiver lock").clone();
    Ok(bodies)
}

struct WebhookSigner(SigningKey);

/// Signs with one key whatever the account; this receiver does not verify signatures.
impl topup_core::Signer for WebhookSigner {
    async fn sign_webhook(
        &self,
        _key: &topup_core::WebhookKeyId,
        content: &[u8],
    ) -> Result<topup_core::Ed25519Signature, topup_core::SignerError> {
        use ed25519_dalek::Signer as _;
        Ok(topup_core::Ed25519Signature(
            self.0.sign(content).to_bytes(),
        ))
    }

    async fn webhook_public_key(
        &self,
        _key: &topup_core::WebhookKeyId,
    ) -> Result<topup_core::Ed25519PublicKey, topup_core::SignerError> {
        Ok(topup_core::Ed25519PublicKey(
            self.0.verifying_key().to_bytes(),
        ))
    }
}

async fn seed_refund_row(pool: &sqlx::PgPool, deposit_id: Uuid, amount: u64) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO refunds (
            id, account_id, livemode, chain_id, deposit_id, amount_atomic, destination_address,
            status
        )
        SELECT $1, account_id, livemode, chain_id, id, $3::text::numeric, $4, 'pending'
        FROM deposits WHERE id = $2
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(deposit_id)
    .bind(amount.to_string())
    .bind(REFUND_DESTINATION)
    .execute(pool)
    .await?;
    Ok(())
}

fn route_fixture() -> RouteFile {
    let route: RouteFile = serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))
        .expect("route fixture parses");
    route.validate().expect("route fixture validates");
    route
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
    seed::set_treasury(pool, account.id, true, 1, FIXTURE_TREASURY).await?;
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

async fn seed_rejected_deposit(
    pool: &sqlx::PgPool,
    product_id: Uuid,
    external_id: &str,
    amount: u64,
) -> Result<Uuid> {
    seed_deposit(
        pool,
        product_id,
        external_id,
        amount,
        DepositState::Rejected,
        Some(RejectReason::OutOfBounds),
    )
    .await
}

async fn seed_deposit(
    pool: &sqlx::PgPool,
    product_id: Uuid,
    external_id: &str,
    amount: u64,
    state: DepositState,
    reason: Option<RejectReason>,
) -> Result<Uuid> {
    let customer = seed_customer(pool, product_id, external_id).await?;
    let index = Uuid::new_v4().as_u128();
    let address = seed::insert_address(
        pool,
        &NewAddress {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            chain_id: 1,
            route: "phala-cloud-ethereum-pha-usd".to_owned(),
            salt: B256::from(U256::from(index)),
            address: Address::from_word(B256::from(U256::from(index))),
        },
    )
    .await?;
    let tx_hash = B256::from(U256::from(
        index.checked_add(1).context("test hash overflow")?,
    ));
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
            block_number: 80,
            block_hash: B256::from(U256::from(
                index.checked_add(2).context("test block overflow")?,
            )),
            block_time: Utc::now() - Duration::hours(2),
            address_id: address.id,
            route: Some("phala-cloud-ethereum-pha-usd".to_owned()),
            route_version: Some(1),
            asset_contract: route_fixture().asset.contract,
            from_address: Address::from_str("0x3333333333333333333333333333333333333333")?,
            amount_atomic: AtomicAmount::new(U256::from(amount)),
            state,
            reason,
            next_attempt_at: Utc::now() + Duration::hours(1),
        },
    )
    .await?;
    Ok(deposit_id(1, tx_hash, 0))
}

/// A final, rejected deposit of the route fixture's token to the forwarder row `address_id`.
async fn seed_deposit_to(
    pool: &sqlx::PgPool,
    address_id: Uuid,
    amount: u64,
    tag: u8,
) -> Result<Uuid> {
    let tx_hash = B256::repeat_byte(tag);
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
                block_number: 80,
                block_hash: B256::repeat_byte(tag.wrapping_add(1)),
                block_time: Utc::now() - Duration::hours(2),
                address_id,
                route: Some("phala-cloud-ethereum-pha-usd".to_owned()),
                route_version: Some(1),
                asset_contract: route_fixture().asset.contract,
                from_address: Address::from_str("0x3333333333333333333333333333333333333333")?,
                amount_atomic: AtomicAmount::new(U256::from(amount)),
                state: DepositState::Rejected,
                reason: Some(RejectReason::OutOfBounds),
                next_attempt_at: Utc::now() + Duration::hours(1),
            },
        )
        .await?
    );
    Ok(deposit_id(1, tx_hash, 0))
}

async fn customer_id(pool: &sqlx::PgPool, deposit_id: Uuid) -> Result<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT customer_id FROM deposits WHERE id = $1")
            .bind(deposit_id)
            .fetch_one(pool)
            .await?,
    )
}

async fn seed_open_lock(pool: &sqlx::PgPool, product_id: Uuid) -> Result<()> {
    seed_lock(pool, product_id, "open-lock", 50, "1 hour").await
}

async fn seed_expired_lock(pool: &sqlx::PgPool, product_id: Uuid) -> Result<()> {
    seed_lock(pool, product_id, "expired-lock", 70, "-1 hour").await
}

async fn seed_lock(
    pool: &sqlx::PgPool,
    product_id: Uuid,
    external_id: &str,
    amount: u64,
    expiry: &str,
) -> Result<()> {
    let customer = seed_customer(pool, product_id, external_id).await?;
    let address = seed::insert_address(
        pool,
        &NewAddress {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            chain_id: 1,
            route: "phala-cloud-ethereum-pha-usd".to_owned(),
            salt: B256::from(U256::from(Uuid::new_v4().as_u128())),
            address: Address::from_word(B256::from(U256::from(Uuid::new_v4().as_u128()))),
        },
    )
    .await?;
    sqlx::query(
        r#"
        UPDATE quotes
        SET amount_atomic = $2::text::numeric, price_scaled = 100000000,
            credit_minor = $2::text::numeric, expires_at = now() + $3::text::interval,
            status = 'open', closed_at = NULL
        WHERE id = $1
        "#,
    )
    .bind(address.quote_id)
    .bind(amount.to_string())
    .bind(expiry)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_same_address_deposits(
    pool: &sqlx::PgPool,
    product_id: Uuid,
    count: u64,
) -> Result<String> {
    let customer = seed_customer(pool, product_id, "support-pages").await?;
    let receiving = Address::from_str("0x5656565656565656565656565656565656565656")?;
    let address = seed::insert_address(
        pool,
        &NewAddress {
            id: Uuid::new_v4(),
            customer_id: customer.id,
            chain_id: 1,
            route: "phala-cloud-ethereum-pha-usd".to_owned(),
            salt: B256::from(U256::from(99_u64)),
            address: receiving,
        },
    )
    .await?;
    for index in 1..=count {
        topup::db::insert_deposit(
            pool,
            &NewDeposit {
                chain_id: 1,
                tx_hash: B256::from(U256::from(index)),
                log_index: 0,
                receipt_log_index: 0,
                tx_from: alloy_primitives::Address::ZERO,
                tx_nonce: 0,
                is_final: true,
                block_number: index,
                block_hash: B256::from(U256::from(index.checked_add(1).context("block hash")?)),
                block_time: Utc::now(),
                address_id: address.id,
                route: Some("phala-cloud-ethereum-pha-usd".to_owned()),
                route_version: Some(1),
                asset_contract: route_fixture().asset.contract,
                from_address: Address::from_str("0x3333333333333333333333333333333333333333")?,
                amount_atomic: AtomicAmount::new(U256::from(100_u64)),
                state: DepositState::Rejected,
                reason: Some(RejectReason::OutOfBounds),
                next_attempt_at: Utc::now(),
            },
        )
        .await?;
    }
    Ok(format!("{receiving:#x}"))
}

async fn response_json(response: axum::response::Response) -> Result<Value> {
    let bytes = to_bytes(response.into_body(), 1_048_576).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
