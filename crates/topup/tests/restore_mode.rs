//! Restore mode (docs/architecture.md §14, docs/design/multi-tenant.md §13) on PostgreSQL: a
//! restore freezes merchant requests and the crediting tasks; the operator re-applies lost
//! security changes, re-issues lost deposit addresses and quotes identically, imports the signed
//! deliveries of events so none is sent again with another body and each delivered credit stands,
//! and unfreezes once the chains are rescanned, audited.

mod support;

use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, TimeDelta, Utc};
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use sqlx::PgPool;
use topup::api::{
    AppState, AttestationError, AttestationFuture, AttestationRequest, Attestor, PublicOrigin,
    VerificationKey, WebhookKeysFuture,
};
use topup::db::{self, NewDeposit};
use topup::deposit_addresses::REISSUE_VERSIONS_AHEAD;
use topup::finality::FinalityWatch;
use topup::pump::{Pump, PumpConfig, RunOnceResult, StepSet};
use topup::restore_mode;
use topup::scanner::{chain_routes, coverage_once};
use topup::steps::confirm::ConfirmStep;
use topup_adapters::attestation::AttestedWebhookKey;
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FactoryLog, FinalizedHead, ReceiptLookup, TransferLog,
};
use topup_adapters::pricing::{Observation, PriceError, PriceSource};
use topup_core::Ed25519PublicKey;
use topup_core::address::{deposit_address_salt, forwarder_address, quote_salt};
use topup_core::deposit::DepositState;
use topup_core::identity::{credited_event_id, deposit_id};
use topup_core::money::{AtomicAmount, PRICE_SCALE, ScaledPrice};
use topup_core::route::{ChainHeads, Confirmations, RouteFile};
use topup_core::valuation::{SourceId, UnixSeconds};
use tower::ServiceExt;
use uuid::Uuid;

use support::seed::{self, NewAccount};
use support::{TEST_ORIGIN, TestDatabase, merchant_request, public_key_base64, signed_request};

const ADMIN_KID: &str = "admin/v1";
/// The key the harness issues and checks client secrets with.
const CLIENT_SECRET_KEY: [u8; 32] = [92; 32];

struct Harness {
    app: Router,
    read_only: Router,
    pool: PgPool,
    owner: PgPool,
    route: RouteFile,
    admin_key: SigningKey,
    /// Admin signatures are single-use, so each admin request is signed at a distinct second.
    created: AtomicI64,
    account: db::Account,
    key: String,
    /// Issues client secrets with the key the API checks them with.
    client_reads: Arc<topup::api::ClientReadLimiter>,
    /// What treasury and refund screening answers; `Unavailable` unless a test sets it.
    screening: Arc<SwitchScreener>,
}

/// Screening whose answer a test sets.
#[derive(Default)]
struct SwitchScreener(std::sync::Mutex<Option<bool>>);

impl SwitchScreener {
    /// `Some(true)`: sanctioned; `Some(false)`: clear; `None`: unavailable.
    fn answer(&self, sanctioned: Option<bool>) {
        *self.0.lock().expect("screener lock") = sanctioned;
    }
}

#[async_trait]
impl topup::refunds::DestinationScreener for SwitchScreener {
    async fn screen(
        &self,
        _route: &RouteFile,
        _destination: Address,
    ) -> topup::refunds::DestinationScreening {
        match *self.0.lock().expect("screener lock") {
            Some(true) => topup::refunds::DestinationScreening::Sanctioned,
            Some(false) => topup::refunds::DestinationScreening::Clear,
            None => topup::refunds::DestinationScreening::Unavailable,
        }
    }
}

struct Answer {
    status: StatusCode,
    retry_after: Option<String>,
    body: Value,
}

impl Harness {
    async fn new(database: &TestDatabase) -> Result<Self> {
        let pool = database.app_pool.clone();
        seed::initialize_dual_chain(&pool, 1).await?;
        let admin_key = SigningKey::from_bytes(&[91; 32]);
        let client_reads = Arc::new(topup::api::ClientReadLimiter::new(
            topup::client_secret::ClientSecretKey::new(topup_core::SecretKey32::new(
                CLIENT_SECRET_KEY,
            )),
            8,
        ));
        let route: RouteFile =
            serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
        let screening = Arc::new(SwitchScreener::default());
        let state = AppState {
            pool: pool.clone(),
            max_attached_pending_refunds: std::num::NonZeroU32::new(2)
                .expect("positive refund limit"),
            routes: Arc::new(
                topup::routes::RouteSet::new(vec![route.clone()]).map_err(anyhow::Error::msg)?,
            ),
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(
                ADMIN_KID.to_owned(),
                &public_key_base64(&admin_key),
            )
            .map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
            attestor: Arc::new(KeyAttestor),
            rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
            client_reads: Arc::clone(&client_reads),
            rate_limits: Arc::default(),
            hint_limits: Arc::default(),
            transaction_hints: Arc::default(),
            screening: Arc::clone(&screening) as Arc<dyn topup::refunds::DestinationScreener>,
            sanctions_rescreen: Arc::default(),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        };
        let account = seed::create_account(
            &pool,
            &NewAccount {
                webhook_url: "https://merchant.example/webhooks".to_owned(),
                ..NewAccount::named("merchant")
            },
        )
        .await?;
        let key = seed::create_api_key(&pool, account.id, true).await?;
        seed::set_treasury(&pool, account.id, true, 1, seed::FIXTURE_TREASURY).await?;
        seed::accept_routes(&pool, account.id, true, &[&route]).await?;
        // Set long before anything the tests issue over it.
        sqlx::query(
            "UPDATE treasuries SET applied_at = now() - interval '1 day' WHERE account_id = $1",
        )
        .bind(account.id)
        .execute(&database.owner_pool)
        .await?;
        // The service's heartbeat: the restore point of the backups the tests restore.
        sqlx::query("INSERT INTO heartbeat DEFAULT VALUES")
            .execute(&pool)
            .await?;
        Ok(Self {
            app: topup::api::router(state.clone()).0,
            read_only: topup::api::read_only_router(state, None),
            pool,
            owner: database.owner_pool.clone(),
            route,
            admin_key,
            created: AtomicI64::new(Utc::now().timestamp()),
            account,
            key,
            client_reads,
            screening,
        })
    }

    async fn admin(&self, method: Method, path: &str, body: &Value) -> Result<Answer> {
        self.admin_on(&self.app, method, path, body).await
    }

    async fn admin_on(
        &self,
        app: &Router,
        method: Method,
        path: &str,
        body: &Value,
    ) -> Result<Answer> {
        let created = self.created.fetch_sub(1, Ordering::Relaxed);
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(body)?
        };
        let request = signed_request(method, path, body, ADMIN_KID, &self.admin_key, created);
        answer(app.clone().oneshot(request).await?).await
    }

    async fn merchant(&self, method: Method, path: &str, body: &Value) -> Result<Answer> {
        self.merchant_with(&self.app, method, path, body, &self.key)
            .await
    }

    async fn merchant_with(
        &self,
        app: &Router,
        method: Method,
        path: &str,
        body: &Value,
        key: &str,
    ) -> Result<Answer> {
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(body)?
        };
        answer(
            app.clone()
                .oneshot(merchant_request(method, path, body, key))
                .await?,
        )
        .await
    }

    /// Records a restore as `restore-check` does after a restore on boot.
    async fn restore(&self) -> Result<restore_mode::Restore> {
        let restore = restore_mode::freeze_after_restore(&self.owner).await?;
        // These business-reconciliation fixtures start after acceptance checks succeeded.
        restore_mode::record_validation(&self.owner, &restore, "ok", &[]).await?;
        Ok(restore)
    }

    /// Sets the chain's cursor as the finalized scanner commits it.
    async fn scan_to(&self, block: i64, block_time: chrono::DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "INSERT INTO cursors (chain_id, scanned_block, scanned_block_time) VALUES (1, $1, $2) \
             ON CONFLICT (chain_id) DO UPDATE \
             SET scanned_block = EXCLUDED.scanned_block, \
                 scanned_block_time = EXCLUDED.scanned_block_time",
        )
        .bind(block)
        .bind(block_time)
        .execute(&self.pool)
        .await?;
        sqlx::query("UPDATE chain_coverage SET through_block=$1,through_time=$2,updated_at=now() WHERE chain_id=1").bind(block).bind(block_time).execute(&self.pool).await?;
        sqlx::query("UPDATE chain_checkpoints SET block_number=$1,block_time=$2 WHERE chain_id=1")
            .bind(block)
            .bind(block_time)
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE addresses SET dual_covered_through=$1 WHERE chain_id=1")
            .bind(block)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn account_id(&self) -> &str {
        &self.account.public_id
    }

    /// The delivery of `event` signed with the account's live webhook key.
    fn delivered(&self, event: &Value) -> Value {
        delivery(event, &webhook_key(self.account_id(), true, 1))
    }

    /// Lifts the freeze once the chain is rescanned past it with every address backfilled.
    async fn unfreeze(&self, restore: &restore_mode::Restore) -> Result<()> {
        self.scan_to(1_000, restore.detected_at + TimeDelta::seconds(1))
            .await?;
        sqlx::query("UPDATE addresses SET backfilled = true")
            .execute(&self.pool)
            .await?;
        let unfrozen = self
            .admin(
                Method::POST,
                "/v1/admin/restore/unfreeze",
                &checklist("reconciled"),
            )
            .await?;
        ensure!(unfrozen.status == StatusCode::OK, "{}", unfrozen.body);
        Ok(())
    }

    /// The customer's deposit address of `version`, as the merchant recomputes it.
    fn derived_address(&self, customer: &str, version: u64) -> Address {
        let contracts = &self.route.chain.contracts;
        forwarder_address(
            contracts.forwarder_factory,
            contracts.implementation,
            seed::FIXTURE_TREASURY,
            deposit_address_salt(&self.account.public_id, true, customer, version),
        )
    }
}

/// The service's webhook keys in these tests: one per account, mode, and version, from its
/// dstack path.
fn webhook_key(account: &str, livemode: bool, version: u32) -> SigningKey {
    let id = topup_core::WebhookKeyId::new(account, livemode, version).expect("an acct_ id");
    key_at(&id)
}

fn key_at(id: &topup_core::WebhookKeyId) -> SigningKey {
    SigningKey::from_bytes(&Sha256::digest(id.domain().as_bytes()).into())
}

/// Signs deliveries with [`webhook_key`]s, as the service signs them with dstack's.
struct KeySigner;

impl topup_core::Signer for KeySigner {
    async fn sign_webhook(
        &self,
        key: &topup_core::WebhookKeyId,
        payload: &[u8],
    ) -> Result<topup_core::Ed25519Signature, topup_core::SignerError> {
        Ok(topup_core::Ed25519Signature(
            key_at(key).sign(payload).to_bytes(),
        ))
    }

    async fn webhook_public_key(
        &self,
        key: &topup_core::WebhookKeyId,
    ) -> Result<Ed25519PublicKey, topup_core::SignerError> {
        Ok(Ed25519PublicKey(key_at(key).verifying_key().to_bytes()))
    }
}

/// A merchant's webhook receiver that records each delivery as it got it: the Standard Webhooks
/// headers and the raw body.
#[derive(Clone, Default)]
struct Receiver(Arc<std::sync::Mutex<Vec<Value>>>);

impl Receiver {
    /// Serves on a local port; returns its URL.
    async fn serve(&self) -> Result<String> {
        let received = self.clone();
        let app = Router::new().route(
            "/webhooks",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
                    let header = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned()
                    };
                    received.0.lock().expect("receiver lock").push(json!({
                        "webhook_id": header("webhook-id"),
                        "webhook_timestamp": header("webhook-timestamp"),
                        "webhook_signature": header("webhook-signature"),
                        "body": String::from_utf8_lossy(&body),
                    }));
                    StatusCode::NO_CONTENT
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(format!("http://{address}/webhooks"))
    }

    fn deliveries(&self) -> Vec<Value> {
        self.0.lock().expect("receiver lock").clone()
    }
}

/// Screens every sender clear.
struct ClearSanctions;

#[async_trait]
impl topup_adapters::risk::SanctionsSource for ClearSanctions {
    async fn sanctions(
        &self,
        _address: Address,
        _block_number: u64,
    ) -> topup_core::screening::SanctionsResult {
        topup_core::screening::SanctionsResult::new(topup_core::screening::SanctionsVerdict::Clear)
    }
}

/// Derives [`webhook_key`]s as dstack derives the service's; it attests nothing.
struct KeyAttestor;

impl Attestor for KeyAttestor {
    fn attest<'a>(&'a self, request: AttestationRequest<'a>) -> AttestationFuture<'a> {
        Box::pin(async move {
            let webhook_keys = self
                .webhook_keys(request.account, request.livemode, request.versions)
                .await?;
            let report_data = topup_adapters::attestation::report_data(
                request.nonce,
                request.account,
                request.livemode,
                &webhook_keys,
            )
            .ok_or(AttestationError::Unavailable)?;
            Ok(topup::api::AttestationEvidence {
                webhook_keys,
                report_data,
                quote: vec![0x7d],
            })
        })
    }

    fn webhook_keys<'a>(
        &'a self,
        account: &'a str,
        livemode: bool,
        versions: &'a [u32],
    ) -> WebhookKeysFuture<'a> {
        Box::pin(async move {
            Ok(versions
                .iter()
                .map(|&version| AttestedWebhookKey {
                    version,
                    public_key: Ed25519PublicKey(
                        webhook_key(account, livemode, version)
                            .verifying_key()
                            .to_bytes(),
                    ),
                })
                .collect())
        })
    }
}

/// The delivery of `event` as the merchant's receiver recorded it, signed with `key`.
fn delivery(event: &Value, key: &SigningKey) -> Value {
    let id = event["id"].as_str().unwrap_or_default().to_owned();
    let body = event.to_string();
    let timestamp = "1790000005";
    let signature = key.sign(format!("{id}.{timestamp}.{body}").as_bytes());
    json!({
        "webhook_id": id,
        "webhook_timestamp": timestamp,
        "webhook_signature": format!("v1a,{}", STANDARD.encode(signature.to_bytes())),
        "body": body,
    })
}

/// A delivered `deposit.credited` of `deposit`, for a transfer of `amount_atomic` to `address`.
fn credited_event(
    harness: &Harness,
    deposit: Uuid,
    tx_hash: B256,
    address: &str,
    amount_atomic: &str,
    valuation: (u64, &str, &str),
) -> Value {
    let (amount, exchange_rate, price_source) = valuation;
    json!({
        "id": topup::ids::format(topup::ids::EVENT, credited_event_id(deposit)),
        "object": "event",
        "account": harness.account_id(),
        "livemode": true,
        "type": "deposit.credited",
        "created": 1_790_000_000,
        "actor": "system",
        "request": null,
        "data": {"object": {
            "id": topup::ids::format(topup::ids::DEPOSIT, deposit),
            "object": "deposit",
            "livemode": true,
            "status": "credited",
            "chain_id": 1,
            "tx_hash": format!("{tx_hash:#x}"),
            "address": address,
            "asset_contract": format!("{:#x}", harness.route.asset.contract),
            "from_address": format!("{:#x}", Address::repeat_byte(0x74)),
            "amount_atomic": amount_atomic,
            "amount": amount,
            "currency": "usd",
            "exchange_rate": exchange_rate,
            "price_source": price_source,
            "valued_at": 1_790_000_000,
            "receipt_log_index": 0,
            "revision": 0,
            "log_index": 0,
            "block_number": 120,
            "block_hash": format!("{:#x}", B256::repeat_byte(0xb1)),
            "block_time": 1_789_999_990,
            "replaces": null,
            "replaced_by": null,
            "created": 1_789_999_995,
            "metadata": {},
        }},
    })
}

/// A detected deposit of `amount_atomic` to `address_id`, as the rescan records it.
async fn record_deposit(
    harness: &Harness,
    tx_hash: B256,
    address_id: Uuid,
    amount_atomic: U256,
) -> Result<Uuid> {
    ensure!(
        db::insert_deposit(
            &harness.pool,
            &new_deposit(harness, tx_hash, address_id, amount_atomic),
        )
        .await?
    );
    Ok(deposit_id(1, tx_hash, 0))
}

/// The transfer of `amount_atomic` to `address_id` in `tx_hash`, as a recorder reads it.
fn new_deposit(
    harness: &Harness,
    tx_hash: B256,
    address_id: Uuid,
    amount_atomic: U256,
) -> NewDeposit {
    NewDeposit {
        chain_id: 1,
        tx_hash,
        receipt_log_index: 0,
        log_index: 0,
        block_number: 120,
        block_hash: B256::repeat_byte(0xb1),
        block_time: Utc::now(),
        address_id,
        route: Some(harness.route.route.clone()),
        route_version: Some(harness.route.version),
        asset_contract: harness.route.asset.contract,
        from_address: Address::repeat_byte(0x74),
        amount_atomic: AtomicAmount::new(amount_atomic),
        state: DepositState::Detected,
        reason: None,
        next_attempt_at: Utc::now(),
        tx_from: Address::repeat_byte(0x74),
        tx_nonce: 0,
        is_final: false,
    }
}

/// A deposit's payment settings binding: its revision, or the restore whose hold it waits on.
async fn binding(harness: &Harness, deposit: Uuid) -> Result<(Option<Uuid>, Option<Uuid>)> {
    Ok(
        sqlx::query_as("SELECT settings_revision_id, settings_hold_id FROM deposits WHERE id = $1")
            .bind(deposit)
            .fetch_one(&harness.pool)
            .await?,
    )
}

/// The account's live settings state: its status, current revision, and holding restore.
async fn settings_state(harness: &Harness) -> Result<(String, Uuid, Option<Uuid>)> {
    Ok(sqlx::query_as(
        "SELECT status, current_revision_id, held_by FROM payment_settings_state \
         WHERE account_id = $1 AND livemode",
    )
    .bind(harness.account.id)
    .fetch_one(&harness.pool)
    .await?)
}

/// The revision id of a payment settings object.
fn revision_of(object: &Value) -> Result<Uuid> {
    topup::ids::parse("psrev_", object["revision"].as_str().context("revision")?)
        .context("a psrev_ id")
}

/// Runs the confirm step once on the recorded deposit, the chain showing its transfer to
/// `recipient` and spot at `spot` scaled dollars.
/// The merchant reconfirms its payment settings after the unfreeze, as `POST /v1/payment_settings`
/// does: the restore held them, and deposits recorded meanwhile wait for it.
async fn reconfirm(harness: &Harness) -> Result<()> {
    seed::accept_routes(&harness.pool, harness.account.id, true, &[&harness.route]).await?;
    Ok(())
}

async fn confirm(harness: &Harness, deposit: Uuid, recipient: Address, spot: u64) -> Result<()> {
    let step = confirm_step(harness, deposit, recipient, spot).await?;
    run_pump(
        harness,
        deposit,
        StepSet::new(Box::new(step), Box::new(Unreached)),
    )
    .await
}

/// The confirm step, the chain showing the recorded deposit's transfer to `recipient` and spot at
/// `spot` scaled dollars.
async fn confirm_step(
    harness: &Harness,
    deposit: Uuid,
    recipient: Address,
    spot: u64,
) -> Result<ConfirmStep> {
    let recorded = db::get_deposit(&harness.pool, deposit)
        .await?
        .context("recorded deposit")?;
    let chain = FinalChain(TransferLog {
        tx_hash: recorded.tx_hash,
        receipt_log_index: recorded.receipt_log_index,
        log_index: recorded.log_index,
        block_number: recorded.block_number,
        block_hash: recorded.block_hash,
        block_time: recorded.block_time,
        tx_from: recorded.tx_from.unwrap_or(Address::repeat_byte(0x74)),
        tx_nonce: recorded.tx_nonce.unwrap_or(0),
        token: recorded.asset_contract,
        from: recorded.from_address,
        to: recipient,
        amount: recorded.amount_atomic,
    });
    let price = |source: &str, value| {
        Arc::new(FixedPrice(Observation {
            source: SourceId::new(source),
            price: ScaledPrice::new(value, PRICE_SCALE).expect("test price"),
            observed_at: UnixSeconds::new(
                u64::try_from(Utc::now().timestamp()).expect("current time"),
            ),
        })) as Arc<dyn PriceSource>
    };
    // This fixture proves terminal restore evidence, rather than a provisional head.
    let mut terminal_route = harness.route.clone();
    terminal_route.chain.confirmations = Confirmations::Finalized;
    Ok(ConfirmStep::single(
        harness.pool.clone(),
        terminal_route,
        chain.clone(),
        chain,
        price("primary", spot),
        Some(price("check", spot)),
        Some(price("fx", 100_000_000)),
    ))
}

/// Runs one step of the pump on `deposit`, whose events render over the harness's route.
async fn run_pump(harness: &Harness, deposit: Uuid, steps: StepSet) -> Result<()> {
    let pump = Pump::new(
        harness.pool.clone(),
        Arc::new(
            topup::routes::RouteSet::new(vec![harness.route.clone()])
                .map_err(anyhow::Error::msg)?,
        ),
        Arc::new(steps),
        PumpConfig::default(),
    )?;
    // Another due deposit may be claimed first; the held steps park it.
    for _ in 0..3 {
        if pump.run_once().await?
            == (RunOnceResult::Applied {
                deposit_id: deposit,
            })
        {
            return Ok(());
        }
    }
    anyhow::bail!("the pump did not run a step on {deposit}")
}

/// A chain final past every block, holding one transfer.
#[derive(Clone)]
struct FinalChain(TransferLog);

impl ChainReader for FinalChain {
    async fn confirmation_probe(
        &self,
        _confirmations: Confirmations,
    ) -> Result<(FinalizedHead, B256), ChainError> {
        Ok((
            FinalizedHead {
                number: 1_000_000,
                time: DateTime::UNIX_EPOCH,
            },
            B256::ZERO,
        ))
    }

    async fn factory_logs(
        &self,
        _factory: Address,
        _forwarders: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<FactoryLog>, ChainError> {
        Ok(Vec::new())
    }

    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Ok(FinalizedHead {
            number: u64::MAX,
            time: DateTime::UNIX_EPOCH,
        })
    }

    async fn transfer_logs_to(
        &self,
        _addresses: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        Ok(vec![self.0.clone()])
    }

    async fn confirmation_heads(
        &self,
        _confirmations: Confirmations,
    ) -> Result<ChainHeads, ChainError> {
        Ok(ChainHeads {
            latest: Some(u64::MAX),
            safe: Some(u64::MAX),
            finalized: u64::MAX,
        })
    }

    async fn receipt_transfer(
        &self,
        _tx_hash: B256,
        _receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        Ok(ReceiptLookup::Included {
            block_number: self.0.block_number,
            block_hash: self.0.block_hash,
            block_time: self.0.block_time,
            status: true,
            tx_from: self.0.tx_from,
            tx_nonce: self.0.tx_nonce,
            transfer: Some(Box::new(self.0.clone())),
        })
    }

    async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
        Ok(1)
    }
}

struct FixedPrice(Observation);

#[async_trait]
impl PriceSource for FixedPrice {
    async fn observe(&self) -> Result<Observation, PriceError> {
        Ok(self.0.clone())
    }
}

/// The steps after confirmation hold a confirmed deposit, so only the confirm step runs.
struct Unreached;

#[async_trait]
impl topup::pump::Step for Unreached {
    async fn run(&self, _deposit: &db::Deposit) -> topup::pump::StepResult {
        topup::pump::StepResult::new(
            topup_core::deposit::StepOutcome::Wait {
                reason: topup_core::deposit::WaitReason::Paused,
            },
            json!({"stage": "held by the test"}),
        )
    }
}

/// The stored valuation of a deposit: state, price source, price, credit.
async fn valuation(harness: &Harness, deposit: Uuid) -> Result<(String, String, String, String)> {
    Ok(sqlx::query_as(
        "SELECT state, price_source, price_scaled::text, credit_minor::text \
         FROM deposits WHERE id = $1",
    )
    .bind(deposit)
    .fetch_one(&harness.pool)
    .await?)
}

/// A `GET` without credentials, as a payer's page reads a `client_secret` view.
async fn anonymous(app: &Router, path: &str) -> Result<Answer> {
    answer(
        app.clone()
            .oneshot(Request::get(path).body(Body::empty())?)
            .await?,
    )
    .await
}

async fn answer(response: axum::response::Response) -> Result<Answer> {
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|value| value.to_str().map(str::to_owned))
        .transpose()?;
    let bytes = to_bytes(response.into_body(), 1_048_576).await?;
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok(Answer {
        status,
        retry_after,
        body,
    })
}

fn checklist(reason: &str) -> Value {
    json!({
        "reason": reason,
        "security_changes_reapplied": true,
        "deposit_addresses_reissued": true,
        "quotes_reissued": true,
        "delivered_events_imported": true,
    })
}

#[tokio::test]
async fn a_new_timeline_freezes_the_service_once() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            // The migration acknowledged the timeline it ran on: no restore.
            ensure!(restore_mode::detect(&harness.pool).await?.is_none());
            ensure!(!restore_mode::is_frozen(&harness.pool).await?);
            harness.scan_to(40, Utc::now()).await?;
            sqlx::query("INSERT INTO heartbeat DEFAULT VALUES")
                .execute(&harness.pool)
                .await?;

            // A promotion out of archive recovery starts a newer timeline than the acknowledged
            // one; emulate it by acknowledging an older one.
            sqlx::query(
                "ALTER TABLE restore_timeline DROP CONSTRAINT restore_timeline_timeline_id_check",
            )
            .execute(&harness.owner)
            .await?;
            sqlx::query("UPDATE restore_timeline SET timeline_id = 0")
                .execute(&harness.owner)
                .await?;
            let restore = restore_mode::detect(&harness.pool)
                .await?
                .context("a newer timeline freezes the service")?;
            ensure!(restore.detected_by == "timeline" && restore.timeline_id >= 1);
            ensure!(restore.restored_cursors.get(&1) == Some(&40));
            ensure!(restore.restore_point.is_some());
            ensure!(restore_mode::is_frozen(&harness.pool).await?);
            // The timeline is acknowledged: a restart finds the same freeze, not another.
            ensure!(restore_mode::detect(&harness.pool).await? == Some(restore.clone()));
            // restore-check's marker keeps the one freeze too.
            ensure!(harness.restore().await?.id == restore.id);
            let restores: i64 = sqlx::query_scalar("SELECT count(*) FROM restores")
                .fetch_one(&harness.pool)
                .await?;
            ensure!(restores == 1);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn frozen_merchant_requests_answer_service_restoring_and_admin_and_health_work() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let body = json!({"name": "ci"});
            ensure!(
                harness
                    .merchant(Method::POST, "/v1/api_keys", &body)
                    .await?
                    .status
                    == StatusCode::OK
            );
            harness.restore().await?;

            for (method, path, body) in [
                (Method::POST, "/v1/api_keys", json!({"name": "ci"})),
                (
                    Method::POST,
                    "/v1/deposit_addresses",
                    json!({"client_reference_id": "team-1"}),
                ),
                (Method::POST, "/v1/account/pause", Value::Null),
                (
                    Method::POST,
                    "/v1/api_keys",
                    json!({"name": "worker", "type": "restricted", "permissions": ["quotes.read"]}),
                ),
                (Method::POST, "/v1/treasuries/trs_0/pause", Value::Null),
                (Method::POST, "/v1/treasuries/trs_0/resume", Value::Null),
                (
                    Method::POST,
                    "/v1/account/webhook_keys/roll",
                    json!({"expires_in": 0}),
                ),
                (Method::DELETE, "/v1/webhook_endpoints/we_0", Value::Null),
            ] {
                let refused = harness.merchant(method, path, &body).await?;
                ensure!(
                    refused.status == StatusCode::SERVICE_UNAVAILABLE,
                    "{path}: {}",
                    refused.status
                );
                ensure!(refused.body["error"]["code"] == "service_restoring");
                ensure!(refused.retry_after.as_deref() == Some("300"));
            }
            // Refused before the key is looked up, so no idempotency key stores the refusal; a
            // key that is not well-formed is refused in memory, before the freeze is read.
            let unknown = topup::api_keys::generate(topup::api_keys::KeyKind::Secret, true)?;
            let unknown = harness
                .merchant_with(&harness.app, Method::POST, "/v1/api_keys", &body, &unknown)
                .await?;
            ensure!(unknown.body["error"]["code"] == "service_restoring");
            let malformed = harness
                .merchant_with(
                    &harness.app,
                    Method::POST,
                    "/v1/api_keys",
                    &body,
                    "ppay_sk_live_invalid",
                )
                .await?;
            ensure!(malformed.status == StatusCode::UNAUTHORIZED);
            ensure!(malformed.body["error"]["code"] == "api_key_invalid");
            let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM idempotency_keys")
                .fetch_one(&harness.pool)
                .await?;
            ensure!(stored == 0);

            // Reads too: the restored database may hold a key revoked after the restore point as
            // valid, so no key authenticates until the operator has revoked such keys again and
            // unfrozen the service. A client secret's public read uses no key.
            for path in ["/v1/account", "/v1/quotes", "/v1/deposits", "/v1/api_keys"] {
                let refused = harness.merchant(Method::GET, path, &Value::Null).await?;
                ensure!(
                    refused.status == StatusCode::SERVICE_UNAVAILABLE,
                    "{path}: {}",
                    refused.status
                );
                ensure!(refused.body["error"]["code"] == "service_restoring");
                ensure!(refused.retry_after.as_deref() == Some("300"));
            }
            let quote_by_key = harness
                .merchant(Method::GET, "/v1/quotes/qt_0", &Value::Null)
                .await?;
            ensure!(quote_by_key.body["error"]["code"] == "service_restoring");
            let health = answer(
                harness
                    .app
                    .clone()
                    .oneshot(Request::get("/healthz").body(Body::empty())?)
                    .await?,
            )
            .await?;
            ensure!(health.status == StatusCode::OK);
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            ensure!(status.status == StatusCode::OK);
            ensure!(status.body["frozen"] == true);
            ensure!(status.body["restore"]["detected_by"] == "restore_check");
            ensure!(status.body["rescan"][0]["chain_id"] == 1);
            ensure!(status.body["rescan"][0]["complete"] == false);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn unfreeze_needs_the_rescan_and_the_checklist_and_is_audited() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let not_frozen = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/unfreeze",
                    &checklist("drill"),
                )
                .await?;
            ensure!(not_frozen.status == StatusCode::BAD_REQUEST);
            ensure!(not_frozen.body["error"]["code"] == "restore_not_frozen");

            harness
                .scan_to(100, Utc::now() - TimeDelta::hours(1))
                .await?;
            let restore = harness.restore().await?;
            let held = tokio::spawn({
                let pool = harness.pool.clone();
                async move {
                    restore_mode::wait_until_unfrozen(
                        &pool,
                        std::time::Duration::from_millis(50),
                        &tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                }
            });

            let mut unchecked = checklist("drill");
            unchecked["deposit_addresses_reissued"] = json!(false);
            let refused = harness
                .admin(Method::POST, "/v1/admin/restore/unfreeze", &unchecked)
                .await?;
            ensure!(refused.status == StatusCode::BAD_REQUEST);
            ensure!(refused.body["error"]["param"] == "deposit_addresses_reissued");

            // The chain has not finalized past the moment the restore was detected.
            let incomplete = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/unfreeze",
                    &checklist("drill"),
                )
                .await?;
            ensure!(incomplete.status == StatusCode::BAD_REQUEST);
            ensure!(incomplete.body["error"]["code"] == "restore_rescan_incomplete");

            // Caught up, but an issued address's history is not read yet.
            harness
                .scan_to(200, restore.detected_at + TimeDelta::seconds(1))
                .await?;
            let customer = seed::create_customer(
                &harness.pool,
                &seed::NewCustomer {
                    id: Uuid::new_v4(),
                    account_id: harness.account.id,
                    livemode: true,
                    client_reference_id: "team-7".to_owned(),
                    paused_scopes: Vec::new(),
                },
            )
            .await?;
            let address = seed::insert_address(
                &harness.pool,
                &seed::NewAddress {
                    id: Uuid::new_v4(),
                    customer_id: customer.id,
                    chain_id: 1,
                    route: harness.route.route.clone(),
                    salt: B256::repeat_byte(0x11),
                    address: Address::repeat_byte(0x12),
                },
            )
            .await?;
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            ensure!(status.body["rescan"][0]["pending_backfills"] == 1);
            ensure!(status.body["rescan"][0]["complete"] == false);
            sqlx::query("UPDATE addresses SET backfilled = true,dual_covered_through=(SELECT through_block FROM chain_coverage WHERE chain_id=addresses.chain_id) WHERE id = $1")
                .bind(address.id)
                .execute(&harness.pool)
                .await?;
            ensure!(!held.is_finished());

            let unfrozen = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/unfreeze",
                    &checklist("INC-42 reconciled"),
                )
                .await?;
            ensure!(unfrozen.status == StatusCode::OK, "{}", unfrozen.body);
            ensure!(unfrozen.body["id"] == restore.id.to_string());
            ensure!(unfrozen.body["unfrozen_by"] == format!("admin:{ADMIN_KID}"));
            let reason = unfrozen.body["unfreeze_reason"]
                .as_str()
                .context("reason")?;
            ensure!(reason.starts_with("INC-42 reconciled; checklist: "));
            let audit: (String, String, String) = sqlx::query_as(
                "SELECT actor_id, subject, reason FROM audit WHERE action = 'restore.unfreeze'",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!(
                audit
                    == (
                        ADMIN_KID.to_owned(),
                        format!("restore:{}", restore.id),
                        reason.to_owned()
                    )
            );
            ensure!(tokio::time::timeout(std::time::Duration::from_secs(5), held).await??);

            // Merchant writes work again; the restore stays on record.
            let created = harness
                .merchant(Method::POST, "/v1/api_keys", &json!({"name": "after"}))
                .await?;
            ensure!(created.status == StatusCode::OK);
            let again = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/unfreeze",
                    &checklist("again"),
                )
                .await?;
            ensure!(again.body["error"]["code"] == "restore_not_frozen");
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            ensure!(status.body["frozen"] == false);
            ensure!(status.body["restore"]["unfrozen_at"].is_i64());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_key_revoked_after_the_restore_point_is_revoked_again_before_the_unfreeze() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            // The merchant's second key, revoked after the restore point: the restore made it
            // valid again.
            let leaked = seed::create_api_key(&harness.pool, harness.account.id, true).await?;
            let restore_path = "/v1/admin/restore/api_keys/revoke";
            let by_prefix = json!({
                "account": harness.account_id(),
                "prefix": "ppay_sk_live_",
                "last4": &leaked[leaked.len() - 4..],
                "reason": "merchant revoked it at 10:02 PDT",
            });
            let not_frozen = harness
                .admin(Method::POST, restore_path, &by_prefix)
                .await?;
            ensure!(not_frozen.body["error"]["code"] == "restore_not_frozen");
            let restore = harness.restore().await?;
            // The restore made the key valid again, but while frozen no key reads anything.
            let account = |key: String| {
                let harness = &harness;
                async move {
                    harness
                        .merchant_with(&harness.app, Method::GET, "/v1/account", &Value::Null, &key)
                        .await
                }
            };
            let held = account(leaked.clone()).await?;
            ensure!(held.status == StatusCode::SERVICE_UNAVAILABLE);
            ensure!(held.body["error"]["code"] == "service_restoring");

            let revoked = harness
                .admin(Method::POST, restore_path, &by_prefix)
                .await?;
            ensure!(revoked.status == StatusCode::OK, "{}", revoked.body);
            ensure!(revoked.body["status"] == "revoked");
            // Repeating it is harmless.
            let repeated = harness
                .admin(Method::POST, restore_path, &by_prefix)
                .await?;
            ensure!(repeated.status == StatusCode::OK && repeated.body["id"] == revoked.body["id"]);
            ensure!(account(harness.key.clone()).await?.status == StatusCode::SERVICE_UNAVAILABLE);

            // Neither selector, or an unknown one.
            let neither = harness
                .admin(
                    Method::POST,
                    restore_path,
                    &json!({"account": harness.account_id(), "reason": "x"}),
                )
                .await?;
            ensure!(neither.status == StatusCode::BAD_REQUEST);
            let unknown = harness
                .admin(
                    Method::POST,
                    restore_path,
                    &json!({
                        "account": harness.account_id(),
                        "prefix": "ppay_sk_live_",
                        "last4": "zzzz",
                        "reason": "x",
                    }),
                )
                .await?;
            ensure!(unknown.status == StatusCode::NOT_FOUND);
            // Once unfrozen, the key revoked again is refused and the other key works.
            harness.unfreeze(&restore).await?;
            ensure!(account(leaked.clone()).await?.status == StatusCode::UNAUTHORIZED);
            ensure!(account(harness.key.clone()).await?.status == StatusCode::OK);
            let audited: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM audit WHERE action = 'restore.api_key_revoke'",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!(audited == 2);
            let announced: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM events WHERE type = 'api_key.revoked' AND account_id = $1",
            )
            .bind(harness.account.id)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(announced == 1);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_treasury_cancellation_lost_in_the_restore_is_applied_again() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let pending = seed::schedule_treasury(
                &harness.pool,
                harness.account.id,
                true,
                1,
                Address::repeat_byte(0x42),
                Utc::now() + TimeDelta::hours(48),
            )
            .await?;
            harness.restore().await?;
            let pending_id = topup::ids::format(topup::ids::TREASURY, pending);
            let missing_id = topup::ids::format(topup::ids::TREASURY, Uuid::new_v4());
            let received = |reapply: bool| {
                json!({
                    "account": harness.account_id(),
                    "livemode": true,
                    "treasuries": [
                        {"id": pending_id, "object": "treasury", "status": "canceled", "chain_id": 1,
                         "address": format!("{:#x}", Address::repeat_byte(0x42))},
                        {"id": missing_id, "status": "pending", "chain_id": 1,
                         "address": format!("{:#x}", Address::repeat_byte(0x43))},
                    ],
                    "reapply": reapply,
                    "reason": "treasury events the merchant received",
                })
            };
            let path = "/v1/admin/restore/treasuries/verify";
            let verified = harness.admin(Method::POST, path, &received(false)).await?;
            ensure!(verified.status == StatusCode::OK, "{}", verified.body);
            ensure!(verified.body["data"][0]["result"] == "cancellation_lost");
            ensure!(verified.body["data"][0]["status"] == "pending");
            ensure!(verified.body["data"][1]["result"] == "missing");
            ensure!(verified.body["data"][1]["status"].is_null());

            let reapplied = harness.admin(Method::POST, path, &received(true)).await?;
            ensure!(reapplied.body["data"][0]["result"] == "canceled");
            ensure!(reapplied.body["data"][0]["status"] == "canceled");
            let again = harness.admin(Method::POST, path, &received(true)).await?;
            ensure!(again.body["data"][0]["result"] == "matches");
            let canceled: Option<chrono::DateTime<Utc>> =
                sqlx::query_scalar("SELECT canceled_at FROM treasuries WHERE id = $1")
                    .bind(pending)
                    .fetch_one(&harness.pool)
                    .await?;
            ensure!(canceled.is_some());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_treasury_crediting_pause_lost_in_the_restore_is_applied_again() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let current: Uuid = sqlx::query_scalar(
                "SELECT id FROM treasuries WHERE account_id = $1 AND replaced_at IS NULL",
            )
            .bind(harness.account.id)
            .fetch_one(&harness.pool)
            .await?;
            harness.restore().await?;
            let paused_by = || async {
                let owners: Vec<String> =
                    sqlx::query_scalar("SELECT crediting_paused_by FROM treasuries WHERE id = $1")
                        .bind(current)
                        .fetch_one(&harness.pool)
                        .await?;
                anyhow::Ok(owners)
            };
            let received = |owners: Value, reapply: bool| {
                json!({
                    "account": harness.account_id(),
                    "livemode": true,
                    "treasuries": [{
                        "id": topup::ids::format(topup::ids::TREASURY, current),
                        "status": "active",
                        "chain_id": 1,
                        "address": format!("{:#x}", seed::FIXTURE_TREASURY),
                        "crediting_paused": true,
                        "crediting_paused_by": owners,
                    }],
                    "reapply": reapply,
                    "reason": "treasury.updated the merchant received",
                })
            };
            let path = "/v1/admin/restore/treasuries/verify";
            // The merchant paused crediting to the treasury after the restore point.
            let lost = harness
                .admin(Method::POST, path, &received(json!(["merchant"]), false))
                .await?;
            ensure!(lost.status == StatusCode::OK, "{}", lost.body);
            ensure!(lost.body["data"][0]["result"] == "matches");
            ensure!(lost.body["data"][0]["crediting"] == "pause_lost");
            ensure!(paused_by().await?.is_empty());
            let paused = harness
                .admin(Method::POST, path, &received(json!(["merchant"]), true))
                .await?;
            ensure!(paused.body["data"][0]["crediting"] == "paused");
            ensure!(paused_by().await? == vec!["merchant".to_owned()]);
            let announced: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM events WHERE type = 'treasury.updated' AND object_id = $1",
            )
            .bind(current)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(announced == 1);

            // A resume lost in the window is applied again; the operator's pause is not the
            // merchant's to lift.
            sqlx::query(
                "UPDATE treasuries SET crediting_paused_by = ARRAY['merchant', 'operator'] \
                 WHERE id = $1",
            )
            .bind(current)
            .execute(&harness.pool)
            .await?;
            let resumed = harness
                .admin(Method::POST, path, &received(json!(["operator"]), true))
                .await?;
            ensure!(resumed.body["data"][0]["crediting"] == "resumed");
            ensure!(paused_by().await? == vec!["operator".to_owned()]);
            let matches = harness
                .admin(Method::POST, path, &received(json!(["operator"]), true))
                .await?;
            ensure!(matches.body["data"][0]["crediting"] == "matches");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_deposit_address_given_out_after_the_restore_point_is_reissued_identically() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let first = harness
                .merchant(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &json!({"client_reference_id": "team-42", "metadata": {"plan": "pro"}}),
                )
                .await?;
            ensure!(first.status == StatusCode::OK && first.body["version"] == 1);
            harness.scan_to(100, Utc::now()).await?;
            let restore = harness.restore().await?;
            // The scanner moved on while frozen; the re-issued address is still read from block 100.
            harness.scan_to(250, Utc::now()).await?;

            // After the restore point the merchant rotated the address twice and holds version 3.
            let held = harness.derived_address("team-42", 3);
            let held_id = topup::ids::format(topup::ids::DEPOSIT_ADDRESS, Uuid::new_v4());
            let held_secret = harness
                .client_reads
                .key()
                .issue(harness.account_id(), &held_id)?;
            let request = json!({
                "account": harness.account_id(),
                "livemode": true,
                "client_reference_id": "team-42",
                "address": format!("{held:#x}"),
                "id": held_id,
                "client_secret": held_secret,
                "reason": "the merchant's export",
            });
            let path = "/v1/admin/restore/deposit_addresses";
            // A version that disagrees with the address is refused before anything is issued.
            let mut disagreeing = request.clone();
            disagreeing["version"] = json!(2);
            let refused = harness.admin(Method::POST, path, &disagreeing).await?;
            ensure!(
                refused.status == StatusCode::BAD_REQUEST,
                "{}",
                refused.body
            );
            let versions: i64 = sqlx::query_scalar("SELECT max(version) FROM deposit_addresses")
                .fetch_one(&harness.pool)
                .await?;
            ensure!(versions == 1);
            // A client secret the service did not issue for the id, or issued for it to another
            // account (another merchant's record of the id), is refused.
            let key = harness.client_reads.key();
            for secret in [
                key.issue(
                    harness.account_id(),
                    &topup::ids::format(topup::ids::DEPOSIT_ADDRESS, Uuid::new_v4()),
                )?,
                key.issue("acct_other", &held_id)?,
            ] {
                let mut other_secret = request.clone();
                other_secret["client_secret"] = json!(secret);
                let refused = harness.admin(Method::POST, path, &other_secret).await?;
                ensure!(
                    refused.body["error"]["param"] == "client_secret",
                    "{}",
                    refused.body
                );
            }
            let reissued = harness.admin(Method::POST, path, &request).await?;
            ensure!(reissued.status == StatusCode::OK, "{}", reissued.body);
            ensure!(reissued.body["reissued"] == true);
            // The payer's page reads the address again with the secret it holds.
            let public = anonymous(
                &harness.app,
                &format!("/v1/deposit_addresses/{held_id}?client_secret={held_secret}"),
            )
            .await?;
            ensure!(public.status == StatusCode::OK, "{}", public.body);
            let object = &reissued.body["deposit_address"];
            ensure!(object["id"] == held_id && object["version"] == 3);
            ensure!(object["status"] == "active" && object["address"] == format!("{held:#x}"));
            ensure!(object["metadata"]["plan"] == "pro");
            let versions: Vec<(i64, String)> = sqlx::query_as(
                "SELECT deposit_address.version, deposit_address.status \
                 FROM deposit_addresses AS deposit_address \
                 JOIN customers AS customer ON customer.id = deposit_address.customer_id \
                 WHERE customer.client_reference_id = 'team-42' ORDER BY 1",
            )
            .fetch_all(&harness.pool)
            .await?;
            ensure!(
                versions
                    == vec![
                        (1, "retired".to_owned()),
                        (2, "retired".to_owned()),
                        (3, "active".to_owned())
                    ],
                "{versions:?}"
            );
            // Version 2's and 3's forwarders are read from the restored cursor.
            let backfill: Vec<(i64, i64, bool)> = sqlx::query_as(
                "SELECT deposit_address.version, address.created_block, address.backfilled \
                 FROM addresses AS address \
                 JOIN deposit_addresses AS deposit_address \
                   ON deposit_address.id = address.deposit_address_id \
                 WHERE deposit_address.version > 1 ORDER BY 1",
            )
            .fetch_all(&harness.pool)
            .await?;
            ensure!(
                backfill == vec![(2, 100, false), (3, 100, false)],
                "{backfill:?}"
            );
            ensure!(restore.restored_cursors.get(&1) == Some(&100));

            // Repeating it returns the same address; a version by number works too.
            let repeated = harness.admin(Method::POST, path, &request).await?;
            ensure!(repeated.body["reissued"] == false);
            ensure!(repeated.body["deposit_address"]["id"] == held_id);
            // A repeat with a secret of the id issued to another account is refused and adds
            // nothing: the secret held was kept once, audited.
            let secret = key.issue("acct_other", &held_id)?;
            let mut other_secret = request.clone();
            other_secret["client_secret"] = json!(secret);
            let refused = harness.admin(Method::POST, path, &other_secret).await?;
            ensure!(
                refused.status == StatusCode::BAD_REQUEST
                    && refused.body["error"]["param"] == "client_secret",
                "{}",
                refused.body
            );
            let read = anonymous(
                &harness.app,
                &format!("/v1/deposit_addresses/{held_id}?client_secret={secret}"),
            )
            .await?;
            ensure!(read.status == StatusCode::NOT_FOUND, "{}", read.body);
            let (secrets, audited): (i64, i64) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM deposit_address_client_secrets \
                         WHERE deposit_address_id = $1), \
                        (SELECT count(*) FROM audit \
                         WHERE action = 'deposit_address.client_secret_restore')",
            )
            .bind(topup::ids::parse(topup::ids::DEPOSIT_ADDRESS, &held_id).context("a da_ id")?)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(
                (secrets, audited) == (1, 1),
                "{secrets} secrets, {audited} audited"
            );
            let new_customer = harness
                .admin(
                    Method::POST,
                    path,
                    &json!({
                        "account": harness.account_id(),
                        "livemode": true,
                        "client_reference_id": "team-new",
                        "version": 1,
                        "reason": "the merchant's export",
                    }),
                )
                .await?;
            ensure!(
                new_customer.body["deposit_address"]["address"]
                    == format!("{:#x}", harness.derived_address("team-new", 1))
            );
            // An address that is not the customer's is refused.
            let mut foreign = request.clone();
            foreign["address"] = json!(format!("{:#x}", Address::repeat_byte(0x99)));
            foreign["id"] = Value::Null;
            foreign["client_secret"] = Value::Null;
            let refused = harness.admin(Method::POST, path, &foreign).await?;
            ensure!(
                refused.status == StatusCode::BAD_REQUEST,
                "{}",
                refused.body
            );
            // Refused for a customer the account never had, it leaves no customer behind.
            foreign["client_reference_id"] = json!("team-ghost");
            let refused = harness.admin(Method::POST, path, &foreign).await?;
            ensure!(refused.status == StatusCode::BAD_REQUEST);
            let ghosts: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM customers WHERE client_reference_id = 'team-ghost'",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!(ghosts == 0);
            let audited: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM audit WHERE action = 'deposit_address.reissue'",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!(audited == 2);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_version_far_past_the_latest_is_refused_and_reissued_in_steps() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let first = harness
                .merchant(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &json!({"client_reference_id": "team-42"}),
                )
                .await?;
            ensure!(first.status == StatusCode::OK && first.body["version"] == 1);
            harness.restore().await?;
            let path = "/v1/admin/restore/deposit_addresses";
            let request = |version: u64| {
                json!({
                    "account": harness.account_id(),
                    "livemode": true,
                    "client_reference_id": "team-42",
                    "version": version,
                    "reason": "the merchant's export",
                })
            };
            let latest = 1 + REISSUE_VERSIONS_AHEAD;
            // A version more than the step past the latest one is refused before anything is
            // issued, however large.
            for version in [latest + 1, u64::MAX] {
                let refused = harness.admin(Method::POST, path, &request(version)).await?;
                ensure!(
                    refused.status == StatusCode::BAD_REQUEST,
                    "{}",
                    refused.body
                );
                let versions: i64 = sqlx::query_scalar("SELECT count(*) FROM deposit_addresses")
                    .fetch_one(&harness.pool)
                    .await?;
                ensure!(versions == 1, "{versions} versions");
            }
            // In steps of at most that many versions, the restore reaches it.
            for version in [
                latest,
                2 * REISSUE_VERSIONS_AHEAD,
                3 * REISSUE_VERSIONS_AHEAD,
            ] {
                let reissued = harness.admin(Method::POST, path, &request(version)).await?;
                ensure!(reissued.status == StatusCode::OK, "{}", reissued.body);
                ensure!(reissued.body["deposit_address"]["version"] == version);
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_delivered_event_is_kept_as_delivered_and_never_sent_again() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let address = harness
                .merchant(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &json!({"client_reference_id": "team-9"}),
                )
                .await?;
            let forwarder = address.body["address"]
                .as_str()
                .context("address")?
                .to_owned();
            let address_id: Uuid =
                sqlx::query_scalar("SELECT id FROM addresses WHERE address = $1 AND chain_id = 1")
                    .bind(&forwarder)
                    .fetch_one(&harness.pool)
                    .await?;
            harness.restore().await?;

            // The merchant received deposit.credited for a deposit credited after the restore
            // point; the rescan has not re-derived it yet.
            let tx_hash = B256::repeat_byte(0x5a);
            let deposit = deposit_id(1, tx_hash, 0);
            let event_id = credited_event_id(deposit);
            let amount_atomic = "100000000000000000000";
            let delivered = credited_event(
                &harness,
                deposit,
                tx_hash,
                &forwarder,
                amount_atomic,
                (2_500, "0.25000000", "spot"),
            );
            let path = "/v1/admin/restore/events";
            let import =
                |deliveries: Vec<Value>| json!({"deliveries": deliveries, "reason": "merchant log"});
            let imported = harness
                .admin(Method::POST, path, &import(vec![harness.delivered(&delivered)]))
                .await?;
            ensure!(imported.status == StatusCode::OK, "{}", imported.body);
            ensure!(imported.body["data"][0]["result"] == "imported");
            let stored = |id: Uuid| {
                let pool = harness.pool.clone();
                async move {
                    let stored: (Value, i64, i64) = sqlx::query_as(
                        "SELECT data, extract(epoch FROM created)::bigint, \
                                (SELECT count(*) FROM webhook_deliveries WHERE event_id = $1) \
                         FROM events WHERE id = $1",
                    )
                    .bind(id)
                    .fetch_one(&pool)
                    .await?;
                    anyhow::Ok(stored)
                }
            };
            ensure!(stored(event_id).await? == (delivered["data"].clone(), 1_790_000_000, 0));

            // After the unfreeze the rescan credits the deposit again: its event is recorded
            // already, so nothing is delivered and the delivered body stays.
            let mut transaction = harness.pool.begin().await?;
            db::enqueue_in(
                &mut transaction,
                &topup::routes::RouteSet::default(),
                &db::NewOutboxEvent {
                    id: event_id,
                    event_type: "deposit.credited".to_owned(),
                    account_id: harness.account.id,
                    livemode: true,
                    object: db::EventObject::Deposit(deposit),
                    next_attempt_at: Utc::now(),
                    actor: db::SYSTEM_ACTOR.to_owned(),
                    request: None,
                    signing_key_version: None,
                },
                None,
            )
            .await?;
            transaction.commit().await?;
            ensure!(stored(event_id).await? == (delivered["data"].clone(), 1_790_000_000, 0));

            // Importing it again matches.
            let again = harness
                .admin(Method::POST, path, &import(vec![harness.delivered(&delivered)]))
                .await?;
            ensure!(again.body["data"][0]["result"] == "matches");

            // Only what the service signed is imported: a body changed after signing, a body
            // signed by another key, and a signature of another account are refused, and
            // nothing of the request is imported.
            let mut altered = harness.delivered(&delivered);
            let mut body = delivered.clone();
            body["data"]["object"]["amount"] = json!(9_999);
            altered["body"] = json!(body.to_string());
            let forged = delivery(&body, &SigningKey::from_bytes(&[5; 32]));
            let other_account = delivery(&body, &webhook_key("acct_other", true, 1));
            let mut other_id = harness.delivered(&delivered);
            other_id["webhook_id"] = json!(topup::ids::format(topup::ids::EVENT, Uuid::new_v4()));
            for refused in [altered, forged, other_account, other_id] {
                let answer = harness
                    .admin(Method::POST, path, &import(vec![refused]))
                    .await?;
                ensure!(answer.status == StatusCode::BAD_REQUEST, "{}", answer.body);
                ensure!(answer.body["error"]["param"] == "deliveries");
            }
            ensure!(stored(event_id).await? == (delivered["data"].clone(), 1_790_000_000, 0));
            // A key rolled after the restore point, lost with it, still verifies.
            let mut rolled = delivered.clone();
            rolled["id"] = json!(topup::ids::format(
                topup::ids::EVENT,
                topup_core::identity::event_id("deposit.rejected", deposit)
            ));
            rolled["type"] = json!("deposit.rejected");
            let answer = harness
                .admin(
                    Method::POST,
                    path,
                    &import(vec![delivery(
                        &rolled,
                        &webhook_key(harness.account_id(), true, 2),
                    )]),
                )
                .await?;
            ensure!(answer.body["data"][0]["result"] == "imported", "{}", answer.body);
            // Up to four rolls lost with the restore are tried, and no more.
            for (version, verifies) in [(5, true), (6, false)] {
                let answer = harness
                    .admin(
                        Method::POST,
                        path,
                        &import(vec![delivery(
                            &rolled,
                            &webhook_key(harness.account_id(), true, version),
                        )]),
                    )
                    .await?;
                ensure!(
                    (answer.status == StatusCode::OK) == verifies,
                    "v{version}: {}",
                    answer.body
                );
            }

            // Compared with the ledger: pending until the rescan values the deposit.
            let findings = |body: &Value| body["delivered_events"]["findings"].clone();
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            ensure!(status.body["delivered_events"]["imported"] == 2);
            ensure!(findings(&status.body)[0]["status"] == "pending");
            record_deposit(&harness, tx_hash, address_id, U256::from(10_u64).pow(U256::from(20)))
                .await?;
            sqlx::query("UPDATE deposits SET credit_minor = 2600 WHERE id = $1")
                .bind(deposit)
                .execute(&harness.owner)
                .await?;
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            let finding = &findings(&status.body)[0];
            ensure!(finding["status"] == "mismatch", "{finding}");
            ensure!(finding["delivered_amount"] == "2500" && finding["ledger_amount"] == "2600");
            sqlx::query("UPDATE deposits SET credit_minor = 2500 WHERE id = $1")
                .bind(deposit)
                .execute(&harness.owner)
                .await?;
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            ensure!(findings(&status.body) == json!([]));

            // An event whose id is not the one its type and deposit derive is refused.
            let mut forged = delivered.clone();
            forged["id"] = json!(topup::ids::format(topup::ids::EVENT, Uuid::new_v4()));
            let refused = harness
                .admin(Method::POST, path, &import(vec![harness.delivered(&forged)]))
                .await?;
            ensure!(refused.status == StatusCode::BAD_REQUEST);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_restored_deposit_keeps_its_delivered_credit_for_its_refunds_and_reversal() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let address = harness
                .merchant(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &json!({"client_reference_id": "team-5"}),
                )
                .await?;
            let forwarder = address.body["address"]
                .as_str()
                .context("address")?
                .to_owned();
            let address_id: Uuid =
                sqlx::query_scalar("SELECT id FROM addresses WHERE address = $1 AND chain_id = 1")
                    .bind(&forwarder)
                    .fetch_one(&harness.pool)
                    .await?;
            let restore = harness.restore().await?;

            // The merchant was told 100 PHA credited $25.00 at $0.25, after the restore point.
            let tx_hash = B256::repeat_byte(0x6b);
            let deposit = deposit_id(1, tx_hash, 0);
            let delivered = credited_event(
                &harness,
                deposit,
                tx_hash,
                &forwarder,
                "100000000000000000000",
                (2_500, "0.25000000", "spot"),
            );
            let imported = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": [harness.delivered(&delivered)], "reason": "log"}),
                )
                .await?;
            ensure!(
                imported.body["data"][0]["result"] == "imported",
                "{}",
                imported.body
            );

            // The rescan re-derives it; spot is now $0.20, which would credit $20.00.
            record_deposit(
                &harness,
                tx_hash,
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            let recipient = Address::from_str(&forwarder)?;
            confirm(&harness, deposit, recipient, 20_000_000).await?;
            ensure!(
                valuation(&harness, deposit).await?
                    == (
                        "confirmed".to_owned(),
                        "spot".to_owned(),
                        "25000000".to_owned(),
                        "2500".to_owned()
                    )
            );
            let valued_at: i64 = sqlx::query_scalar(
                "SELECT extract(epoch FROM valuation_at)::bigint FROM deposits WHERE id = $1",
            )
            .bind(deposit)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(valued_at == 1_790_000_000);
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            ensure!(status.body["delivered_events"]["findings"] == json!([]));

            // Its refunds and its reversal reference the delivered credit.
            sqlx::query("UPDATE deposits SET state = 'credited', final_at = now() WHERE id = $1")
                .bind(deposit)
                .execute(&harness.owner)
                .await?;
            sqlx::query(
                "INSERT INTO refunds (id, account_id, livemode, chain_id, deposit_id, \
                 amount_atomic, destination_address, status, tx_hash, receipt_log_index, paid_at) \
                 SELECT $1, account_id, livemode, chain_id, id, 25000000000000000000, $3, \
                        'succeeded', $4, 0, now() \
                 FROM deposits WHERE id = $2",
            )
            .bind(Uuid::new_v4())
            .bind(deposit)
            .bind(format!("{:#x}", Address::repeat_byte(0x75)))
            .bind(format!("{:#x}", B256::repeat_byte(0x76)))
            .execute(&harness.owner)
            .await?;
            harness.unfreeze(&restore).await?;
            let path = format!(
                "/v1/deposits/{}",
                topup::ids::format(topup::ids::DEPOSIT, deposit)
            );
            let object = harness.merchant(Method::GET, &path, &Value::Null).await?;
            ensure!(object.status == StatusCode::OK, "{}", object.body);
            ensure!(object.body["amount"] == 2_500 && object.body["exchange_rate"] == "0.25000000");
            ensure!(object.body["amount_refunded"] == 625, "{}", object.body);
            sqlx::query("DELETE FROM refunds WHERE deposit_id = $1")
                .bind(deposit)
                .execute(&harness.owner)
                .await?;
            sqlx::query("UPDATE deposits SET state = 'reversed', final_at = NULL WHERE id = $1")
                .bind(deposit)
                .execute(&harness.owner)
                .await?;
            let object = harness.merchant(Method::GET, &path, &Value::Null).await?;
            ensure!(object.body["amount_reversed"] == 2_500, "{}", object.body);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_delivered_credit_the_chain_contradicts_holds_the_deposit_until_discarded() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let address = harness
                .merchant(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &json!({"client_reference_id": "team-6"}),
                )
                .await?;
            let forwarder = address.body["address"]
                .as_str()
                .context("address")?
                .to_owned();
            let address_id: Uuid =
                sqlx::query_scalar("SELECT id FROM addresses WHERE address = $1 AND chain_id = 1")
                    .bind(&forwarder)
                    .fetch_one(&harness.pool)
                    .await?;
            harness.restore().await?;

            // A delivered credit of 100 PHA, but the chain's transfer is 50 PHA.
            let tx_hash = B256::repeat_byte(0x7c);
            let deposit = deposit_id(1, tx_hash, 0);
            let delivered = credited_event(
                &harness,
                deposit,
                tx_hash,
                &forwarder,
                "100000000000000000000",
                (2_500, "0.25000000", "spot"),
            );
            harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": [harness.delivered(&delivered)], "reason": "log"}),
                )
                .await?;
            record_deposit(
                &harness,
                tx_hash,
                address_id,
                U256::from(5_u64) * U256::from(10_u64).pow(U256::from(19)),
            )
            .await?;
            let finding = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?
                .body["delivered_events"]["findings"][0]
                .clone();
            ensure!(finding["status"] == "contradicted", "{finding}");

            // The confirm step holds it: not valued, not credited.
            let recipient = Address::from_str(&forwarder)?;
            reconfirm(&harness).await?;
            confirm(&harness, deposit, recipient, 20_000_000).await?;
            let (state, attempt_error): (String, Option<String>) = sqlx::query_as(
                "SELECT state, (SELECT evidence ->> 'error' FROM transitions \
                                WHERE deposit_id = $1 ORDER BY created_at DESC LIMIT 1) \
                 FROM deposits WHERE id = $1",
            )
            .bind(deposit)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(state == "detected", "{state}");
            ensure!(attempt_error.as_deref() == Some("delivered_event_contradicts_chain"));
            let credit: Option<String> =
                sqlx::query_scalar("SELECT credit_minor::text FROM deposits WHERE id = $1")
                    .bind(deposit)
                    .fetch_one(&harness.pool)
                    .await?;
            ensure!(credit.is_none());

            // The operator discards the delivered credit; the deposit is valued from the chain.
            let path = "/v1/admin/restore/delivered_credits/discard";
            let request = json!({
                "deposit": topup::ids::format(topup::ids::DEPOSIT, deposit),
                "reason": "INC-7: chain shows 50 PHA; settled with the merchant",
            });
            let discarded = harness.admin(Method::POST, path, &request).await?;
            ensure!(discarded.status == StatusCode::OK, "{}", discarded.body);
            ensure!(discarded.body["discarded"] == true);
            let again = harness.admin(Method::POST, path, &request).await?;
            ensure!(again.status == StatusCode::OK);
            let unknown = harness
                .admin(
                    Method::POST,
                    path,
                    &json!({
                        "deposit": topup::ids::format(topup::ids::DEPOSIT, Uuid::new_v4()),
                        "reason": "x",
                    }),
                )
                .await?;
            ensure!(unknown.status == StatusCode::NOT_FOUND);
            let audited: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM audit WHERE action = 'restore.delivered_credit_discard'",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!(audited == 1);
            sqlx::query("UPDATE deposits SET next_attempt_at = now() WHERE id = $1")
                .bind(deposit)
                .execute(&harness.owner)
                .await?;
            confirm(&harness, deposit, recipient, 20_000_000).await?;
            ensure!(
                valuation(&harness, deposit).await?
                    == (
                        "confirmed".to_owned(),
                        "spot".to_owned(),
                        "20000000".to_owned(),
                        "1000".to_owned()
                    )
            );
            let finding = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?
                .body["delivered_events"]["findings"][0]
                .clone();
            ensure!(finding["status"] == "mismatch", "{finding}");
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_quote_given_out_after_the_restore_point_is_reissued_and_credited() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            harness.scan_to(100, Utc::now()).await?;
            let restore = harness.restore().await?;
            harness.scan_to(250, Utc::now()).await?;

            // The merchant's record of a quote created after the restore point: $10.00 at $0.10.
            let id = Uuid::new_v4();
            let qt = topup::ids::format(topup::ids::QUOTE, id);
            let contracts = &harness.route.chain.contracts;
            let address = forwarder_address(
                contracts.forwarder_factory,
                contracts.implementation,
                seed::FIXTURE_TREASURY,
                quote_salt(harness.account_id(), "team-q", &qt),
            );
            // Terms no route issues today (a window past the route's, another amount) are the
            // merchant's record all the same: the route may have changed since.
            let created = Utc::now().timestamp() - 60;
            let secret = harness
                .client_reads
                .key()
                .issue(harness.account_id(), &qt)?;
            let request = json!({
                "account": harness.account_id(),
                "livemode": true,
                "id": qt,
                "client_reference_id": "team-q",
                "chain_id": 1,
                "asset": "pha",
                "amount": 1_000,
                "amount_atomic": "100000000000000000000",
                "exchange_rate": "0.10000000",
                "address": format!("{address:#x}"),
                "created": created,
                "expires_at": created + 3_600,
                "metadata": {"order_id": "o-1"},
                "client_secret": secret,
                "reason": "the merchant's quote log",
            });
            let path = "/v1/admin/restore/quotes";
            // A forged address, a client secret the service did not issue for the quote, issued
            // for it to another account (another merchant's record of the id, or the payer's page
            // of the owner's quote), or issued before owner tags, or an asset no route has is
            // refused, and no customer is created.
            let other_quote = topup::ids::format(topup::ids::QUOTE, Uuid::new_v4());
            for (field, value) in [
                (
                    "address",
                    json!(format!("{:#x}", Address::repeat_byte(0x99))),
                ),
                (
                    "client_secret",
                    json!(
                        harness
                            .client_reads
                            .key()
                            .issue(harness.account_id(), &other_quote)?
                    ),
                ),
                (
                    "client_secret",
                    json!(harness.client_reads.key().issue("acct_other", &qt)?),
                ),
                (
                    "client_secret",
                    json!(format!("{qt}_secret_{}", "0".repeat(64))),
                ),
                ("asset", json!("usdc")),
            ] {
                let mut forged = request.clone();
                forged[field] = value;
                let refused = harness.admin(Method::POST, path, &forged).await?;
                ensure!(
                    refused.status == StatusCode::BAD_REQUEST,
                    "{field}: {}",
                    refused.body
                );
            }
            let (quotes, customers): (i64, i64) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM quotes), \
                        (SELECT count(*) FROM customers WHERE client_reference_id = 'team-q')",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!((quotes, customers) == (0, 0));

            // Re-issued first without its secret, found later in the merchant's records.
            let mut without_secret = request.clone();
            without_secret["client_secret"] = Value::Null;
            let reissued = harness.admin(Method::POST, path, &without_secret).await?;
            ensure!(reissued.status == StatusCode::OK, "{}", reissued.body);
            ensure!(reissued.body["reissued"] == true);
            let public_read = |secret: String| {
                let app = harness.app.clone();
                let path = format!("/v1/quotes/{qt}?client_secret={secret}");
                async move { anonymous(&app, &path).await }
            };
            ensure!(public_read(secret.clone()).await?.status == StatusCode::NOT_FOUND);
            let quote = &reissued.body["quote"];
            ensure!(quote["id"] == qt && quote["address"] == format!("{address:#x}"));
            ensure!(quote["amount"] == 1_000 && quote["exchange_rate"] == "0.10000000");
            ensure!(quote["metadata"] == json!({"order_id": "o-1"}));
            // Its lock is never honoured, so its window closes at the restore: its page shows it
            // expired instead of asking for a payment at the locked price.
            ensure!(
                quote["expires_at"] == restore.detected_at.timestamp(),
                "{quote}"
            );
            // A repeat with a secret of the quote issued to another account is refused and adds
            // nothing, though a read accepts its tag.
            let other = harness.client_reads.key().issue("acct_other", &qt)?;
            ensure!(harness.client_reads.key().verify(&qt, &other));
            let mut other_secret = request.clone();
            other_secret["client_secret"] = json!(other);
            let refused = harness.admin(Method::POST, path, &other_secret).await?;
            ensure!(
                refused.status == StatusCode::BAD_REQUEST
                    && refused.body["error"]["param"] == "client_secret",
                "{}",
                refused.body
            );
            ensure!(public_read(other).await?.status == StatusCode::NOT_FOUND);
            // A repeat with its secret adds it; the payer's page reads the quote again.
            let repeated = harness.admin(Method::POST, path, &request).await?;
            ensure!(repeated.body["reissued"] == false, "{}", repeated.body);
            let public = public_read(secret.clone()).await?;
            ensure!(public.status == StatusCode::OK, "{}", public.body);
            // Once it has one, another secret of the quote does not replace it.
            let other = harness
                .client_reads
                .key()
                .issue(harness.account_id(), &qt)?;
            let mut other_secret = request.clone();
            other_secret["client_secret"] = json!(other);
            let repeated = harness.admin(Method::POST, path, &other_secret).await?;
            ensure!(repeated.body["reissued"] == false);
            ensure!(public_read(other).await?.status == StatusCode::NOT_FOUND);
            ensure!(public_read(secret.clone()).await?.status == StatusCode::OK);
            let secret_restored: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM audit WHERE action = 'quote.client_secret_restore'",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!(secret_restored == 1);

            // The scanner watches it from the restored cursor, so the rescan finds its payment.
            let scanned = db::list_scan_addresses(&harness.pool, 1).await?;
            let watched = scanned
                .iter()
                .find(|scan| scan.address == address)
                .context("the re-issued quote's address is scanned")?;
            ensure!(!watched.backfilled && watched.backfill_start() == 100);
            ensure!(restore.restored_cursors.get(&1) == Some(&100));
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            ensure!(status.body["rescan"][0]["pending_backfills"] == 1);

            // Its locked price is the merchant's record, never applied: at $0.08 spot the
            // payment credits $8.00, not the quote's $10.00.
            let address_id: Uuid =
                sqlx::query_scalar("SELECT id FROM addresses WHERE quote_id = $1")
                    .bind(id)
                    .fetch_one(&harness.pool)
                    .await?;
            let tx_hash = B256::repeat_byte(0x8d);
            let deposit = record_deposit(
                &harness,
                tx_hash,
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            reconfirm(&harness).await?;
            confirm(&harness, deposit, address, 8_000_000).await?;
            ensure!(
                valuation(&harness, deposit).await?
                    == (
                        "confirmed".to_owned(),
                        "spot".to_owned(),
                        "8000000".to_owned(),
                        "800".to_owned()
                    )
            );

            // A signed deposit.credited that carries the quote's credit is the evidence: a second
            // payment the merchant was told was credited at the quote is valued at it.
            let paid = B256::repeat_byte(0x8e);
            let second = deposit_id(1, paid, 0);
            let delivered = credited_event(
                &harness,
                second,
                paid,
                &format!("{address:#x}"),
                "100000000000000000000",
                (1_000, "0.10000000", "quote"),
            );
            harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": [harness.delivered(&delivered)], "reason": "log"}),
                )
                .await?;
            record_deposit(
                &harness,
                paid,
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            confirm(&harness, second, address, 8_000_000).await?;
            ensure!(
                valuation(&harness, second).await?
                    == (
                        "confirmed".to_owned(),
                        "lock".to_owned(),
                        "10000000".to_owned(),
                        "1000".to_owned()
                    )
            );
            let (status, consumed_by): (String, Option<Uuid>) =
                sqlx::query_as("SELECT status, consumed_by FROM quotes WHERE id = $1")
                    .bind(id)
                    .fetch_one(&harness.pool)
                    .await?;
            ensure!(status == "consumed" && consumed_by == Some(second));
            let audited: i64 =
                sqlx::query_scalar("SELECT count(*) FROM audit WHERE action = 'quote.reissue'")
                    .fetch_one(&harness.pool)
                    .await?;
            ensure!(audited == 1);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_credit_delivered_by_the_service_round_trips_through_a_restore() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let receiver = Receiver::default();
            sqlx::query("UPDATE webhook_endpoints SET url = $2 WHERE account_id = $1")
                .bind(harness.account.id)
                .bind(receiver.serve().await?)
                .execute(&harness.owner)
                .await?;
            let address = harness
                .merchant(
                    Method::POST,
                    "/v1/deposit_addresses",
                    &json!({"client_reference_id": "team-rt"}),
                )
                .await?;
            let forwarder =
                Address::from_str(address.body["address"].as_str().context("address")?)?;
            let address_id: Uuid =
                sqlx::query_scalar("SELECT id FROM addresses WHERE address = $1 AND chain_id = 1")
                    .bind(format!("{forwarder:#x}"))
                    .fetch_one(&harness.pool)
                    .await?;

            // The service credits 100 PHA at $0.25 through the real steps and delivers
            // deposit.credited, which the merchant's receiver records.
            let tx_hash = B256::repeat_byte(0x9a);
            let deposit = record_deposit(
                &harness,
                tx_hash,
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            let steps = |confirm: ConfirmStep| -> Result<StepSet> {
                let screen = topup::steps::screen::ScreenStep::new(
                    harness.pool.clone(),
                    [topup::steps::screen::ScreenRoute::new(
                        harness.route.clone(),
                        Arc::new(ClearSanctions),
                    )],
                )?;
                Ok(StepSet::new(Box::new(confirm), Box::new(screen)))
            };
            for _ in ["confirm", "screen and credit"] {
                let step = confirm_step(&harness, deposit, forwarder, 25_000_000).await?;
                run_pump(&harness, deposit, steps(step)?).await?;
            }
            let credited = valuation(&harness, deposit).await?;
            ensure!(credited.0 == "credited", "{credited:?}");
            // In whole seconds, as the API renders `valued_at` and the delivery carries it.
            let valued_at = |pool: PgPool| async move {
                let at: i64 = sqlx::query_scalar(
                    "SELECT floor(extract(epoch FROM valuation_at))::bigint FROM deposits \
                     WHERE id = $1",
                )
                .bind(deposit)
                .fetch_one(&pool)
                .await?;
                anyhow::Ok(at)
            };
            let first_valued_at = valued_at(harness.pool.clone()).await?;
            let worker = topup::outbox::DeliveryWorker::new(
                harness.pool.clone(),
                Arc::new(KeySigner),
                true,
                topup::outbox::DeliveryConfig {
                    proxy: None,
                    ..topup::outbox::DeliveryConfig::default()
                },
            )?;
            ensure!(worker.run_once().await? >= 1);
            let delivered = receiver
                .deliveries()
                .into_iter()
                .find(|delivery| {
                    delivery["webhook_id"]
                        == topup::ids::format(topup::ids::EVENT, credited_event_id(deposit))
                })
                .context("the receiver recorded deposit.credited")?;

            // The restore loses the credit and its event: the rescan finds the deposit detected.
            for statement in [
                "DELETE FROM webhook_deliveries WHERE event_id = $1",
                "DELETE FROM events WHERE id = $1",
            ] {
                sqlx::query(statement)
                    .bind(credited_event_id(deposit))
                    .execute(&harness.owner)
                    .await?;
            }
            sqlx::query(
                "UPDATE deposits SET state = 'detected', valuation_at = NULL, price_scaled = NULL, \
                 price_source = NULL, credit_minor = NULL, quote = NULL, next_attempt_at = now() \
                 WHERE id = $1",
            )
            .bind(deposit)
            .execute(&harness.owner)
            .await?;
            harness.restore().await?;

            // The delivery as recorded verifies and carries the delivered credit into the ledger,
            // though spot is now $0.20.
            let imported = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": [delivered], "reason": "the merchant's receiver log"}),
                )
                .await?;
            ensure!(
                imported.body["data"][0]["result"] == "imported",
                "{}",
                imported.body
            );
            confirm(&harness, deposit, forwarder, 20_000_000).await?;
            let restored = valuation(&harness, deposit).await?;
            ensure!(
                (
                    restored.1.as_str(),
                    restored.2.as_str(),
                    restored.3.as_str()
                ) == (
                    credited.1.as_str(),
                    credited.2.as_str(),
                    credited.3.as_str()
                ),
                "{restored:?} != {credited:?}"
            );
            ensure!(valued_at(harness.pool.clone()).await? == first_valued_at);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn the_operator_attests_a_frozen_instance_that_merchant_keys_cannot() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            harness.restore().await?;
            let nonce = "00112233445566778899aabbccddeeff";
            let merchant = harness
                .merchant(
                    Method::GET,
                    &format!("/v1/attestation?nonce={nonce}"),
                    &Value::Null,
                )
                .await?;
            ensure!(merchant.body["error"]["code"] == "service_restoring");
            for app in [&harness.app, &harness.read_only] {
                let attested = harness
                    .admin_on(
                        app,
                        Method::GET,
                        &format!(
                            "/v1/admin/attestation?account={}&livemode=true&nonce={nonce}",
                            harness.account_id()
                        ),
                        &Value::Null,
                    )
                    .await?;
                ensure!(attested.status == StatusCode::OK, "{}", attested.body);
                ensure!(attested.body["account"] == harness.account_id());
                ensure!(attested.body["livemode"] == true);
                let key = webhook_key(harness.account_id(), true, 1);
                ensure!(
                    attested.body["webhook_keys"][0]["public_key"]
                        == format!("whpk_{}", STANDARD.encode(key.verifying_key().to_bytes()))
                );
                let expected = topup_adapters::attestation::report_data(
                    &hex::decode(nonce)?,
                    harness.account_id(),
                    true,
                    &[AttestedWebhookKey {
                        version: 1,
                        public_key: Ed25519PublicKey(key.verifying_key().to_bytes()),
                    }],
                )
                .context("report data")?;
                ensure!(attested.body["report_data"] == hex::encode(expected));
            }
            let unknown = harness
                .admin(
                    Method::GET,
                    &format!(
                        "/v1/admin/attestation?account={}&livemode=true&nonce={nonce}",
                        topup::ids::format(topup::ids::ACCOUNT, Uuid::new_v4())
                    ),
                    &Value::Null,
                )
                .await?;
            ensure!(unknown.status == StatusCode::NOT_FOUND);
            let bad_nonce = harness
                .admin(
                    Method::GET,
                    &format!(
                        "/v1/admin/attestation?account={}&livemode=true&nonce=zz",
                        harness.account_id()
                    ),
                    &Value::Null,
                )
                .await?;
            ensure!(bad_nonce.status == StatusCode::BAD_REQUEST);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn the_restore_check_instance_takes_only_reads_and_the_restore_reconciliation() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            // restore-check records the freeze in parallel with the read-only API, and may fail
            // before it does: a merchant key reads nothing there even before the freeze exists.
            ensure!(!restore_mode::is_frozen(&harness.pool).await?);
            let early = harness
                .merchant_with(
                    &harness.read_only,
                    Method::GET,
                    "/v1/account",
                    &Value::Null,
                    &harness.key,
                )
                .await?;
            ensure!(
                early.body["error"]["code"] == "service_restoring",
                "{}",
                early.body
            );
            harness.restore().await?;
            let refused = harness
                .merchant_with(
                    &harness.read_only,
                    Method::POST,
                    "/v1/api_keys",
                    &json!({"name": "ci"}),
                    &harness.key,
                )
                .await?;
            ensure!(refused.status == StatusCode::SERVICE_UNAVAILABLE);
            ensure!(refused.body["error"]["code"] == "service_restoring");
            ensure!(refused.retry_after.as_deref() == Some("300"));
            let admin = harness
                .admin_on(
                    &harness.read_only,
                    Method::POST,
                    &format!("/v1/admin/accounts/{}/pause", harness.account_id()),
                    &json!({"scopes": ["quotes"], "reason": "x"}),
                )
                .await?;
            ensure!(admin.body["error"]["code"] == "service_restoring");
            // Frozen, a merchant read is refused too; the operator's reads work.
            let read = harness
                .merchant_with(
                    &harness.read_only,
                    Method::GET,
                    "/v1/account",
                    &Value::Null,
                    &harness.key,
                )
                .await?;
            ensure!(read.body["error"]["code"] == "service_restoring");
            let status = harness
                .admin_on(
                    &harness.read_only,
                    Method::GET,
                    "/v1/admin/restore",
                    &Value::Null,
                )
                .await?;
            ensure!(status.status == StatusCode::OK && status.body["frozen"] == true);
            // The restore reconciliation reaches its handler: nothing scans here, so the freeze
            // cannot be lifted, but a key can be revoked again.
            let unfreeze = harness
                .admin_on(
                    &harness.read_only,
                    Method::POST,
                    "/v1/admin/restore/unfreeze",
                    &checklist("drill"),
                )
                .await?;
            ensure!(unfreeze.body["error"]["code"] == "restore_rescan_incomplete");
            let leaked = seed::create_api_key(&harness.pool, harness.account.id, true).await?;
            let revoked = harness
                .admin_on(
                    &harness.read_only,
                    Method::POST,
                    "/v1/admin/restore/api_keys/revoke",
                    &json!({
                        "account": harness.account_id(),
                        "prefix": "ppay_sk_live_",
                        "last4": &leaked[leaked.len() - 4..],
                        "reason": "merchant revoked it",
                    }),
                )
                .await?;
            ensure!(revoked.status == StatusCode::OK, "{}", revoked.body);
            Ok(())
        })
    })
    .await
}

/// A transaction of the router in the reorganization tests, which pays from a contract.
const ROUTER_TX: B256 = B256::repeat_byte(0x0a);
/// A token without a route: a transfer of it is rejected as `unsupported_asset`.
const UNROUTED_TOKEN: Address = Address::repeat_byte(0x57);

/// Both providers of chain 1 in the reorganization tests: one canonical chain holding at most
/// the router's transfer (tests/reorged_transfer.rs).
#[derive(Clone, Default)]
struct ScriptedChain(Arc<std::sync::Mutex<ScriptedState>>);

#[derive(Default)]
struct ScriptedState {
    head: u64,
    finalized: u64,
    transfer: Option<TransferLog>,
    receipt: Option<TransferLog>,
    time: DateTime<Utc>,
}

impl ScriptedChain {
    fn set(&self, head: u64, finalized: u64, transfer: Option<TransferLog>) {
        *self.0.lock().expect("chain state") = ScriptedState {
            head,
            finalized,
            receipt: transfer.clone(),
            transfer,
            time: Utc::now(),
        };
    }

    fn state<T>(&self, read: impl FnOnce(&ScriptedState) -> T) -> T {
        read(&self.0.lock().expect("chain state"))
    }
}

impl ChainReader for ScriptedChain {
    async fn header(&self, number: u64) -> Result<(B256, DateTime<Utc>), ChainError> {
        Ok(self.state(|state| match &state.transfer {
            Some(log) if log.block_number == number => (log.block_hash, log.block_time),
            _ => (B256::ZERO, state.time),
        }))
    }

    async fn factory_logs(
        &self,
        _factory: Address,
        _forwarders: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<FactoryLog>, ChainError> {
        Ok(Vec::new())
    }

    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Ok(FinalizedHead {
            number: self.state(|state| state.finalized),
            time: self.state(|state| state.time),
        })
    }

    async fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        Ok(self.state(|state| {
            state
                .transfer
                .iter()
                .filter(|log| {
                    addresses.contains(&log.to)
                        && (from_block..=to_block).contains(&log.block_number)
                })
                .cloned()
                .collect()
        }))
    }

    async fn confirmation_heads(
        &self,
        _confirmations: Confirmations,
    ) -> Result<ChainHeads, ChainError> {
        Ok(self.state(|state| ChainHeads {
            latest: Some(state.head),
            safe: None,
            finalized: state.finalized,
        }))
    }

    async fn receipt_transfer(
        &self,
        tx_hash: B256,
        receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        Ok(self.state(|state| match &state.receipt {
            Some(log) if log.tx_hash == tx_hash => ReceiptLookup::Included {
                block_number: log.block_number,
                block_hash: log.block_hash,
                block_time: log.block_time,
                status: true,
                tx_from: log.tx_from,
                tx_nonce: log.tx_nonce,
                transfer: state
                    .transfer
                    .as_ref()
                    .filter(|transfer| transfer.receipt_log_index == receipt_log_index)
                    .map(|transfer| Box::new(transfer.clone())),
            },
            _ => ReceiptLookup::Missing,
        }))
    }

    async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
        panic!("finality never searches sender nonces")
    }
}

/// The router's transfer of `amount` whole tokens of `token` to `to`, in `block_number`.
fn router_transfer(
    token: Address,
    to: Address,
    block_number: u64,
    block_byte: u8,
    amount: u64,
) -> TransferLog {
    TransferLog {
        tx_hash: ROUTER_TX,
        receipt_log_index: 0,
        log_index: 3,
        block_number,
        block_hash: B256::repeat_byte(block_byte),
        // Match PostgreSQL's persisted timestamp precision in this scripted chain.
        block_time: DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
            .expect("fixture timestamp"),
        tx_from: Address::repeat_byte(0x77),
        tx_nonce: 7,
        token,
        from: Address::repeat_byte(0x66),
        to,
        amount: AtomicAmount::new(U256::from(amount) * U256::from(10_u64).pow(U256::from(18))),
    }
}

/// The service's pipeline on a [`ScriptedChain`], crediting at depth 2 as fast credit does, so a
/// credited deposit can still be reversed.
struct Pipeline {
    pool: PgPool,
    chain: ScriptedChain,
    routes: Arc<topup::routes::RouteSet>,
    pump: Pump,
    watch: FinalityWatch,
}

impl Pipeline {
    async fn watch_once(&self, harness: &Harness) -> Result<topup::finality::WatchStats> {
        let head = self.chain.finalized_head().await?;
        db::chain_reads::advance_checkpoint(
            &harness.pool,
            1,
            db::chain_reads::Boundary {
                number: head.number,
                hash: B256::ZERO,
                time: head.time,
            },
        )
        .await?;
        let clock: DateTime<Utc> = sqlx::query_scalar(
            "SELECT GREATEST(now(),COALESCE(max(finality_check_at),now())) FROM deposits WHERE deposit_finality_pending(deposits)",
        ).fetch_one(&harness.pool).await?;
        Ok(self.watch.watch_once_at(1, clock).await?)
    }

    fn new(harness: &Harness) -> Result<Self> {
        let mut route = harness.route.clone();
        route.chain.confirmations = Confirmations::Depth(2);
        let routes = Arc::new(
            topup::routes::RouteSet::new(vec![route.clone()]).map_err(anyhow::Error::msg)?,
        );
        let chain = ScriptedChain::default();
        let price = |source: &str, value| {
            Arc::new(FixedPrice(Observation {
                source: SourceId::new(source),
                price: ScaledPrice::new(value, PRICE_SCALE).expect("test price"),
                observed_at: UnixSeconds::new(
                    u64::try_from(Utc::now().timestamp()).expect("current time"),
                ),
            })) as Arc<dyn PriceSource>
        };
        let confirm = ConfirmStep::single(
            harness.pool.clone(),
            route.clone(),
            chain.clone(),
            chain.clone(),
            price("primary", 25_000_000),
            Some(price("check", 25_000_000)),
            Some(price("fx", 100_000_000)),
        );
        let screen = topup::steps::screen::ScreenStep::new(
            harness.pool.clone(),
            [topup::steps::screen::ScreenRoute::new(
                route.clone(),
                Arc::new(ClearSanctions),
            )],
        )?;
        let pump = Pump::new(
            harness.pool.clone(),
            Arc::clone(&routes),
            Arc::new(StepSet::new(Box::new(confirm), Box::new(screen))),
            PumpConfig::default(),
        )?;
        let watch = FinalityWatch::single(
            harness.pool.clone(),
            Arc::clone(&routes),
            1,
            chain.clone(),
            chain.clone(),
        )
        .with_pump(Arc::new(pump.clone()));
        Ok(Self {
            pool: harness.pool.clone(),
            chain,
            routes,
            pump,
            watch,
        })
    }

    /// The per-block scan's record of `transfer` to the address row `address_id`, at the route's
    /// confirmation.
    async fn fast_scan(
        &self,
        harness: &Harness,
        transfer: &TransferLog,
        address_id: Uuid,
    ) -> Result<()> {
        let routed = transfer.token == harness.route.asset.contract;
        let deposit = NewDeposit {
            chain_id: 1,
            tx_hash: transfer.tx_hash,
            receipt_log_index: transfer.receipt_log_index,
            log_index: transfer.log_index,
            block_number: transfer.block_number,
            block_hash: transfer.block_hash,
            block_time: transfer.block_time,
            address_id,
            route: routed.then(|| harness.route.route.clone()),
            route_version: routed.then_some(harness.route.version),
            asset_contract: transfer.token,
            from_address: transfer.from,
            amount_atomic: transfer.amount,
            state: if routed {
                DepositState::Detected
            } else {
                DepositState::Rejected
            },
            reason: (!routed).then_some(topup_core::deposit::RejectReason::UnsupportedAsset),
            next_attempt_at: Utc::now(),
            tx_from: transfer.tx_from,
            tx_nonce: transfer.tx_nonce,
            is_final: false,
        };
        let committed =
            db::commit_confirmed_scan(&harness.pool, 1, &[deposit], transfer.block_number).await?;
        ensure!(committed.inserted == 1);
        Ok(())
    }

    /// The finalized scanner's pass, as the rescan after a restore runs it; returns the deposits it
    /// recorded.
    async fn finalized_scan(&self, harness: &Harness) -> Result<u64> {
        topup::checkpoint::advance(&harness.pool, 1, &self.chain, &self.chain).await?;
        let routes = chain_routes(&self.routes)
            .into_iter()
            .next()
            .context("the chain's routes")?;
        Ok(
            coverage_once(&harness.pool, &self.chain, &self.chain, &routes, 1)
                .await?
                .inserted,
        )
    }

    /// Runs the pump until nothing is due.
    async fn settle(&self) -> Result<()> {
        for _ in 0..10 {
            let clock: DateTime<Utc> = sqlx::query_scalar(
                "SELECT GREATEST(now(),COALESCE(max(next_attempt_at),now())) FROM deposits",
            )
            .fetch_one(&self.pool)
            .await?;
            if self.pump.run_once_at(clock).await? == RunOnceResult::Idle {
                return Ok(());
            }
        }
        anyhow::bail!("the pump did not settle")
    }
}

impl Harness {
    /// Points the account's endpoint at a receiver that records each delivery as it got it.
    async fn receiver(&self) -> Result<Receiver> {
        let receiver = Receiver::default();
        sqlx::query("UPDATE webhook_endpoints SET url = $2 WHERE account_id = $1")
            .bind(self.account.id)
            .bind(receiver.serve().await?)
            .execute(&self.owner)
            .await?;
        Ok(receiver)
    }

    /// Delivers every due event, signed as the service signs them.
    async fn deliver(&self) -> Result<()> {
        let worker = topup::outbox::DeliveryWorker::new(
            self.pool.clone(),
            Arc::new(KeySigner),
            true,
            topup::outbox::DeliveryConfig {
                proxy: None,
                ..topup::outbox::DeliveryConfig::default()
            },
        )?;
        while worker.run_once().await? > 0 {}
        Ok(())
    }

    /// The customer's deposit address, created through the API: its forwarder on chain 1 and
    /// that forwarder's address row.
    async fn deposit_address(&self, customer: &str) -> Result<(Address, Uuid)> {
        let created = self
            .merchant(
                Method::POST,
                "/v1/deposit_addresses",
                &json!({"client_reference_id": customer}),
            )
            .await?;
        ensure!(created.status == StatusCode::OK, "{}", created.body);
        let forwarder = Address::from_str(created.body["address"].as_str().context("address")?)?;
        let address_id =
            sqlx::query_scalar("SELECT id FROM addresses WHERE address = $1 AND chain_id = 1")
                .bind(format!("{forwarder:#x}"))
                .fetch_one(&self.pool)
                .await?;
        Ok((forwarder, address_id))
    }

    /// Removes `deposits`, recorded after the restore point, and everything recorded with them,
    /// as a restore from a backup taken before them does.
    async fn forget_deposits(&self, deposits: &[Uuid]) -> Result<()> {
        let mut transaction = self.owner.begin().await?;
        // The ledger's append-only triggers guard the service, not the backup's contents.
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *transaction)
            .await?;
        for statement in [
            "DELETE FROM webhook_deliveries WHERE event_id IN \
             (SELECT id FROM events WHERE object_id = ANY($1))",
            "DELETE FROM events WHERE object_id = ANY($1)",
            "DELETE FROM transitions WHERE deposit_id = ANY($1)",
            "DELETE FROM deposits WHERE id = ANY($1) AND replaces IS NOT NULL",
            "DELETE FROM deposits WHERE id = ANY($1)",
        ] {
            sqlx::query(statement)
                .bind(deposits)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}

/// The delivery the receiver recorded of the `event_type` event about `object`.
fn received(receiver: &Receiver, event_type: &str, object: &str) -> Result<Value> {
    receiver
        .deliveries()
        .into_iter()
        .find(|delivery| {
            serde_json::from_str::<Value>(delivery["body"].as_str().unwrap_or_default()).is_ok_and(
                |body| body["type"] == event_type && body["data"]["object"]["id"] == object,
            )
        })
        .with_context(|| format!("the receiver recorded {event_type} of {object}"))
}

/// The body of a recorded delivery.
fn delivered_body(delivery: &Value) -> Result<Value> {
    Ok(serde_json::from_str(
        delivery["body"].as_str().context("body")?,
    )?)
}

#[tokio::test]
async fn a_restored_unverified_reversed_record_preserves_its_canonical_successor() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let receiver = harness.receiver().await?;
            let (forwarder, address_id) = harness.deposit_address("team-reorg").await?;
            harness.scan_to(5, Utc::now()).await?;
            // The backup is taken here: it predates both deposits.
            let pipeline = Pipeline::new(&harness)?;

            // The router pays 100 PHA, credited at $25.00 before finality; its transaction is
            // re-included against other state and the same receipt position pays 90 PHA: the
            // finality watch reverses the first deposit and the second is credited at $22.50.
            let first = router_transfer(harness.route.asset.contract, forwarder, 10, 0xaa, 100);
            pipeline.chain.set(12, 5, Some(first.clone()));
            pipeline.fast_scan(&harness, &first, address_id).await?;
            pipeline.settle().await?;
            let old = deposit_id(1, ROUTER_TX, 0);
            let initial: Value = sqlx::query_scalar("SELECT evidence FROM transitions WHERE deposit_id=$1 ORDER BY created_at DESC LIMIT 1").bind(old).fetch_one(&harness.pool).await?;
            ensure!(db::get_deposit(&harness.pool,old).await?.context("original")?.state==topup_core::deposit::DepositState::Credited,"original did not credit: {initial}");
            ensure!(valuation(&harness, old).await.context("original valuation")?.0 == "credited");
            let second = router_transfer(harness.route.asset.contract, forwarder, 11, 0xbb, 90);
            pipeline.chain.set(30, 20, Some(second));
            ensure!(pipeline.watch_once(&harness).await?.reversed == 1);
            pipeline.settle().await?;
            let new = topup_core::identity::deposit_revision_id(1, ROUTER_TX, 0, 1);
            let credited = valuation(&harness, new).await.context("successor valuation")?;
            ensure!(
                credited.0 == "credited" && credited.3 == "2250",
                "{credited:?}"
            );
            harness.deliver().await?;
            let (old_id, new_id) = (
                topup::ids::format(topup::ids::DEPOSIT, old),
                topup::ids::format(topup::ids::DEPOSIT, new),
            );
            let deliveries = vec![
                received(&receiver, "deposit.credited", &old_id)?,
                received(&receiver, "deposit.reversed", &old_id)?,
                received(&receiver, "deposit.credited", &new_id)?,
            ];

            // The restore loses both deposits and their events; the merchant's receiver has
            // every delivery.
            harness.forget_deposits(&[old, new]).await?;
            harness.restore().await?;
            let imported = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": deliveries, "reason": "the merchant's receiver log"}),
                )
                .await?;
            ensure!(imported.status == StatusCode::OK, "{}", imported.body);
            for result in imported.body["data"].as_array().context("results")? {
                ensure!(result["result"] == "imported", "{}", imported.body);
            }
            ensure!(
                imported.body["data"][1]["reversed_deposit"] == "restored"
                    && imported.body["data"][0]["reversed_deposit"].is_null(),
                "{}",
                imported.body
            );

            // Historical reversed evidence reconstructed from delivered events has no dual
            // marker. It must not be compared with the active canonical successor.
            let historical: bool = sqlx::query_scalar(
                "SELECT state='reversed' AND dual_verified_at IS NULL FROM deposits WHERE id=$1",
            ).bind(old).fetch_one(&harness.pool).await?;
            ensure!(historical);
            ensure!(pipeline.finalized_scan(&harness).await? == 1);
            ensure!(db::chain_reads::coverage(&harness.pool,1).await?.context("coverage")?.number == 20);
            let frozen: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM reconciliation_blocks WHERE scope='chain' AND chain_id=1)",
            ).fetch_one(&harness.pool).await?;
            ensure!(!frozen, "historical restored reversal froze its canonical successor");
            let recorded: Vec<(Uuid, String, i64, Option<Uuid>)> = sqlx::query_as(
                "SELECT id, state, revision, replaces FROM deposits \
                 WHERE chain_id = 1 AND tx_hash = $1 ORDER BY revision",
            )
            .bind(format!("{ROUTER_TX:#x}"))
            .fetch_all(&harness.pool)
            .await?;
            ensure!(
                recorded
                    == vec![
                        (old, "reversed".to_owned(), 0, None),
                        (new, "detected".to_owned(), 1, Some(old)),
                    ],
                "{recorded:?}"
            );
            // Each deposit renders as the merchant last received it.
            for (id, status, amount, replaces, replaced_by) in [
                (&old_id, "reversed", 2_500, Value::Null, json!(new_id)),
                (&new_id, "pending", 0, json!(old_id), Value::Null),
            ] {
                let read = harness
                    .admin(
                        Method::GET,
                        &format!("/v1/admin/deposits/{id}"),
                        &Value::Null,
                    )
                    .await?;
                ensure!(read.status == StatusCode::OK, "{}", read.body);
                ensure!(read.body["status"] == status, "{}", read.body);
                ensure!(read.body["replaces"] == replaces, "{}", read.body);
                ensure!(read.body["replaced_by"] == replaced_by, "{}", read.body);
                if status == "reversed" {
                    ensure!(read.body["amount"] == amount, "{}", read.body);
                    ensure!(read.body["amount_reversed"] == amount, "{}", read.body);
                }
            }
            // The second deposit is valued at its delivered credit, not at today's spot; nothing
            // is sent again.
            confirm(&harness, new, forwarder, 20_000_000).await?;
            let restored = valuation(&harness, new).await.context("successor valuation")?;
            ensure!(
                (restored.2.as_str(), restored.3.as_str()) == ("25000000", "2250"),
                "{restored:?}"
            );
            let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries")
                .fetch_one(&harness.pool)
                .await?;
            ensure!(queued == 0, "{queued} deliveries queued");
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            ensure!(
                status.body["delivered_events"]["findings"] == json!([]),
                "{}",
                status.body
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_rejected_deposit_reversed_before_finality_round_trips_through_a_restore() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let receiver = harness.receiver().await?;
            let (forwarder, address_id) = harness.deposit_address("team-token").await?;
            harness.scan_to(5, Utc::now()).await?;
            let pipeline = Pipeline::new(&harness)?;

            // A token without a route is rejected unvalued; its final receipt no longer has
            // that transfer, so the rejected deposit is reversed, still unvalued.
            let transfer = router_transfer(UNROUTED_TOKEN, forwarder, 10, 0xaa, 100);
            pipeline.chain.set(12, 5, Some(transfer.clone()));
            pipeline.fast_scan(&harness, &transfer, address_id).await?;
            pipeline.chain.set(30, 20, Some(transfer));
            pipeline.chain.0.lock().expect("chain state").transfer = None;
            ensure!(pipeline.watch_once(&harness).await?.reversed == 1);
            harness.deliver().await?;
            let deposit = deposit_id(1, ROUTER_TX, 0);
            let public_id = topup::ids::format(topup::ids::DEPOSIT, deposit);
            let deliveries = vec![
                received(&receiver, "deposit.rejected", &public_id)?,
                received(&receiver, "deposit.reversed", &public_id)?,
            ];
            ensure!(delivered_body(&deliveries[1])?["data"]["object"]["amount"].is_null());

            harness.forget_deposits(&[deposit]).await?;
            let restore = harness.restore().await?;
            let imported = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": deliveries, "reason": "the merchant's receiver log"}),
                )
                .await?;
            ensure!(imported.status == StatusCode::OK, "{}", imported.body);
            ensure!(
                imported.body["data"][0]["result"] == "imported"
                    && imported.body["data"][1]["result"] == "imported"
                    && imported.body["data"][1]["reversed_deposit"] == "restored",
                "{}",
                imported.body
            );
            // Imported again, the deposit is in the ledger already.
            let again = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": [deliveries[1]], "reason": "again"}),
                )
                .await?;
            ensure!(
                again.body["data"][0]["result"] == "matches"
                    && again.body["data"][0]["reversed_deposit"] == "recorded",
                "{}",
                again.body
            );
            // The reversed deposit is back as the merchant last received it, with no credit.
            let read = harness
                .admin(
                    Method::GET,
                    &format!("/v1/admin/deposits/{public_id}"),
                    &Value::Null,
                )
                .await?;
            ensure!(read.status == StatusCode::OK, "{}", read.body);
            ensure!(read.body["status"] == "reversed" && read.body["amount"].is_null());
            let credits: i64 = sqlx::query_scalar("SELECT count(*) FROM restore_delivered_credits")
                .fetch_one(&harness.pool)
                .await?;
            ensure!(credits == 0);

            // The rescan finds nothing at the position, and after the unfreeze nothing is sent.
            ensure!(pipeline.finalized_scan(&harness).await? == 0);
            let sent = receiver.deliveries().len();
            harness.unfreeze(&restore).await?;
            harness.deliver().await?;
            ensure!(
                receiver.deliveries().len() == sent,
                "{:?}",
                receiver.deliveries()
            );
            Ok(())
        })
    })
    .await
}

/// Credits the recorded `deposit` through the confirm and screen steps, the chain showing its
/// transfer to `recipient` and spot at `spot` scaled dollars.
async fn credit(harness: &Harness, deposit: Uuid, recipient: Address, spot: u64) -> Result<()> {
    for _ in ["confirm", "screen and credit"] {
        let confirm = confirm_step(harness, deposit, recipient, spot).await?;
        let screen = topup::steps::screen::ScreenStep::new(
            harness.pool.clone(),
            [topup::steps::screen::ScreenRoute::new(
                harness.route.clone(),
                Arc::new(ClearSanctions),
            )],
        )?;
        run_pump(
            harness,
            deposit,
            StepSet::new(Box::new(confirm), Box::new(screen)),
        )
        .await?;
    }
    ensure!(valuation(harness, deposit).await?.0 == "credited");
    Ok(())
}

#[tokio::test]
async fn addresses_paid_over_a_treasury_change_lost_in_the_restore_are_reissued_and_credited()
-> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let receiver = harness.receiver().await?;
            let contracts = &harness.route.chain.contracts;
            let over = |treasury: Address, salt: B256| {
                forwarder_address(
                    contracts.forwarder_factory,
                    contracts.implementation,
                    treasury,
                    salt,
                )
            };
            // The backup holds the merchant's change of treasury from A to B, pending.
            let (former, changed) = (seed::FIXTURE_TREASURY, Address::repeat_byte(0xb2));
            let pending = seed::schedule_treasury(
                &harness.pool,
                harness.account.id,
                true,
                1,
                changed,
                Utc::now() - TimeDelta::minutes(1),
            )
            .await?;
            harness.scan_to(100, Utc::now()).await?;
            let routes = Arc::new(
                topup::routes::RouteSet::new(vec![harness.route.clone()])
                    .map_err(anyhow::Error::msg)?,
            );

            // After the restore point B applies; the merchant gives a customer a deposit address,
            // now over B, which is paid and credited, and records a quote over B.
            ensure!(
                topup::treasuries::apply_due(
                    &harness.pool,
                    &routes,
                    &support::ClearScreener,
                    Utc::now()
                )
                .await?
                    == 1
            );
            let (forwarder, address_id) = harness.deposit_address("team-b").await?;
            let salt = deposit_address_salt(harness.account_id(), true, "team-b", 1);
            ensure!(forwarder == over(changed, salt));
            let tx_hash = B256::repeat_byte(0xd1);
            let deposit =
                record_deposit(&harness, tx_hash, address_id, U256::from(10_u64).pow(U256::from(20)))
                    .await?;
            credit(&harness, deposit, forwarder, 25_000_000).await?;
            let credited = valuation(&harness, deposit).await?;
            harness.deliver().await?;
            let pending_id = topup::ids::format(topup::ids::TREASURY, pending);
            let applied = received(&receiver, "treasury.updated", &pending_id)?;
            let applied_body = delivered_body(&applied)?;
            ensure!(applied_body["data"]["object"]["status"] == "active");
            let applied_at = applied_body["created"].as_i64().context("created")?;
            let former_id: Uuid = sqlx::query_scalar(
                "SELECT id FROM treasuries WHERE address = $1 AND account_id = $2",
            )
            .bind(format!("{former:#x}"))
            .bind(harness.account.id)
            .fetch_one(&harness.pool)
            .await?;
            let replaced = received(
                &receiver,
                "treasury.updated",
                &topup::ids::format(topup::ids::TREASURY, former_id),
            )?;
            let credited_delivery = received(
                &receiver,
                "deposit.credited",
                &topup::ids::format(topup::ids::DEPOSIT, deposit),
            )?;

            // The restore brings back the backup: B pending, A current, no deposit address, no
            // deposit, no events.
            harness.forget_deposits(&[deposit]).await?;
            for statement in [
                "UPDATE treasuries SET applied_at = NULL WHERE address = $1",
                "UPDATE treasuries SET replaced_at = NULL WHERE address = $2",
                "DELETE FROM webhook_deliveries",
                "DELETE FROM events WHERE type LIKE 'treasury.%'",
                "DELETE FROM addresses WHERE deposit_address_id IS NOT NULL",
                "DELETE FROM deposit_address_client_secrets",
                "DELETE FROM deposit_addresses",
                "DELETE FROM customers WHERE client_reference_id = 'team-b'",
            ] {
                sqlx::query(statement)
                    .bind(format!("{changed:#x}"))
                    .bind(format!("{former:#x}"))
                    .execute(&harness.owner)
                    .await?;
            }
            harness.scan_to(100, Utc::now()).await?;
            let restore = harness.restore().await?;
            harness.scan_to(250, Utc::now()).await?;

            // The merchant's latest objects of B and A show the change applied after the restore
            // point: B `active`, A `replaced` by it. Both are the one lost application.
            let replaced_object = delivered_body(&replaced)?["data"]["object"].clone();
            ensure!(replaced_object["status"] == "replaced");
            let verify = json!({
                "account": harness.account_id(),
                "livemode": true,
                "treasuries": [applied_body["data"]["object"], replaced_object],
                "reapply": true,
                "reason": "treasury events the merchant received",
            });
            let verified = harness
                .admin(Method::POST, "/v1/admin/restore/treasuries/verify", &verify)
                .await?;
            ensure!(verified.status == StatusCode::OK, "{}", verified.body);
            let results = |body: &Value| -> Vec<(Value, Value, Value)> {
                body["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|row| {
                        (
                            row["result"].clone(),
                            row["status"].clone(),
                            row["crediting"].clone(),
                        )
                    })
                    .collect()
            };
            ensure!(
                results(&verified.body)
                    == vec![
                        (json!("application_lost"), json!("pending"), json!("matches")),
                        (json!("replacement_lost"), json!("active"), json!("matches")),
                    ],
                "{}",
                verified.body
            );
            // Until B is restored, the customer's address over it is not the account's.
            let address_request = json!({
                "account": harness.account_id(),
                "livemode": true,
                "client_reference_id": "team-b",
                "address": format!("{forwarder:#x}"),
                "reason": "the merchant's export",
            });
            let path = "/v1/admin/restore/deposit_addresses";
            let refused = harness.admin(Method::POST, path, &address_request).await?;
            ensure!(refused.status == StatusCode::BAD_REQUEST, "{}", refused.body);

            // B is restored from its signed application; the former treasury's notice, a
            // tampered body, or a delivery signed with another key is not an application.
            let apply = |delivery: &Value| {
                json!({"delivery": delivery, "reason": "the merchant's treasury.updated"})
            };
            let apply_path = "/v1/admin/restore/treasuries/apply";
            let mut tampered = applied.clone();
            tampered["body"] = json!(
                applied["body"]
                    .as_str()
                    .context("body")?
                    .replace(&format!("{changed:#x}"), &format!("{:#x}", Address::repeat_byte(0xee)))
            );
            let other_key = delivery(&applied_body, &webhook_key("acct_other", true, 1));
            for refused in [&replaced, &tampered, &other_key] {
                let answer = harness
                    .admin(Method::POST, apply_path, &apply(refused))
                    .await?;
                ensure!(answer.status == StatusCode::BAD_REQUEST, "{}", answer.body);
            }
            let screened_at = |pool: PgPool| async move {
                let at: chrono::DateTime<Utc> =
                    sqlx::query_scalar("SELECT screened_at FROM treasuries WHERE id = $1")
                        .bind(pending)
                        .fetch_one(&pool)
                        .await?;
                anyhow::Ok(at)
            };
            let screened_before = screened_at(harness.pool.clone()).await?;
            let restored = harness
                .admin(Method::POST, apply_path, &apply(&applied))
                .await?;
            ensure!(restored.status == StatusCode::OK, "{}", restored.body);
            ensure!(restored.body["applied"] == true, "{}", restored.body);
            ensure!(restored.body["treasury"]["status"] == "active");
            let again = harness
                .admin(Method::POST, apply_path, &apply(&applied))
                .await?;
            ensure!(again.body["applied"] == false, "{}", again.body);
            // Its events are not sent again: the merchant has them, and new ones would carry the
            // restore's time. Screening is unavailable here, so the restored `screened_at` stands
            // for the rescreen after the unfreeze.
            let announced: i64 =
                sqlx::query_scalar("SELECT count(*) FROM events WHERE type LIKE 'treasury.%'")
                    .fetch_one(&harness.pool)
                    .await?;
            ensure!(announced == 0);
            ensure!(screened_at(harness.pool.clone()).await? == screened_before);
            // As in force when it applied: B from its application, A until then.
            let history: Vec<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
                "SELECT address, extract(epoch FROM applied_at)::bigint, \
                        extract(epoch FROM replaced_at)::bigint \
                 FROM treasuries WHERE account_id = $1 ORDER BY created_at",
            )
            .bind(harness.account.id)
            .fetch_all(&harness.pool)
            .await?;
            ensure!(
                history[1] == (format!("{changed:#x}"), Some(applied_at), None)
                    && history[0].2 == Some(applied_at),
                "{history:?}"
            );
            let verified = harness
                .admin(Method::POST, "/v1/admin/restore/treasuries/verify", &verify)
                .await?;
            ensure!(
                results(&verified.body)
                    == vec![
                        (json!("matches"), json!("active"), json!("matches")),
                        (json!("matches"), json!("replaced"), json!("matches")),
                    ],
                "{}",
                verified.body
            );
            let audited: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM audit WHERE action = 'restore.treasury_apply'",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!(audited == 1);

            // The deposit address and the quotes are re-issued over the treasury in force when
            // each was issued.
            let reissued = harness.admin(Method::POST, path, &address_request).await?;
            ensure!(reissued.status == StatusCode::OK, "{}", reissued.body);
            ensure!(reissued.body["deposit_address"]["address"] == format!("{forwarder:#x}"));
            // An address given out over A before B applied comes back over A, superseded as B's
            // application left it: still watched from the restored cursor, and credited.
            let early_salt = deposit_address_salt(harness.account_id(), true, "team-early-da", 1);
            let early = over(former, early_salt);
            let mut early_request = address_request.clone();
            early_request["client_reference_id"] = json!("team-early-da");
            early_request["address"] = json!(format!("{early:#x}"));
            let reissued_early = harness.admin(Method::POST, path, &early_request).await?;
            ensure!(reissued_early.status == StatusCode::OK, "{}", reissued_early.body);
            ensure!(
                reissued_early.body["deposit_address"]["address"]
                    == format!("{:#x}", over(changed, early_salt)),
                "{}",
                reissued_early.body
            );
            let superseded: Option<chrono::DateTime<Utc>> =
                sqlx::query_scalar("SELECT superseded_at FROM addresses WHERE address = $1")
                    .bind(format!("{early:#x}"))
                    .fetch_one(&harness.pool)
                    .await?;
            ensure!(superseded.is_some());
            let watched = db::list_scan_addresses(&harness.pool, 1)
                .await?
                .into_iter()
                .find(|scan| scan.address == early)
                .context("the superseded network is scanned")?;
            ensure!(watched.backfill_start() == 100);
            // By its version alone, an address gets a network over every treasury in force since
            // the restore point: over A superseded, over B current.
            let mut by_version = address_request.clone();
            by_version["client_reference_id"] = json!("team-v");
            by_version["address"] = Value::Null;
            by_version["version"] = json!(1);
            let reissued_version = harness.admin(Method::POST, path, &by_version).await?;
            ensure!(reissued_version.status == StatusCode::OK, "{}", reissued_version.body);
            let version_salt = deposit_address_salt(harness.account_id(), true, "team-v", 1);
            let networks: Vec<(String, bool)> = sqlx::query_as(
                "SELECT address, superseded_at IS NOT NULL FROM addresses \
                 WHERE deposit_address_id = (SELECT id FROM deposit_addresses \
                     WHERE customer_id = (SELECT id FROM customers \
                         WHERE client_reference_id = 'team-v')) \
                 ORDER BY superseded_at IS NOT NULL",
            )
            .fetch_all(&harness.pool)
            .await?;
            ensure!(
                networks
                    == vec![
                        (format!("{:#x}", over(changed, version_salt)), false),
                        (format!("{:#x}", over(former, version_salt)), true),
                    ],
                "{networks:?}"
            );
            let quote = |customer: &str, treasury: Address, created: i64| {
                let qt = topup::ids::format(topup::ids::QUOTE, Uuid::new_v4());
                let address = over(treasury, quote_salt(harness.account_id(), customer, &qt));
                json!({
                    "account": harness.account_id(),
                    "livemode": true,
                    "id": qt,
                    "client_reference_id": customer,
                    "chain_id": 1,
                    "asset": "pha",
                    "amount": 1_000,
                    "amount_atomic": "100000000000000000000",
                    "exchange_rate": "0.10000000",
                    "address": format!("{address:#x}"),
                    "created": created,
                    "expires_at": created + 900,
                    "reason": "the merchant's quote log",
                })
            };
            // A quote created in the second B applied is re-issued over either treasury: the
            // recorded application time is not the instant the quote saw. A quote over A created
            // well after B applied, or created well before the restore point, is refused.
            let restore_point = restore.restore_point.context("restore point")?.timestamp();
            for (customer, treasury, created, status) in [
                ("team-q", changed, applied_at + 5, StatusCode::OK),
                ("team-same-b", changed, applied_at, StatusCode::OK),
                ("team-same-a", former, applied_at, StatusCode::OK),
                // `created` read before the quote waited on the treasury lock B's pass held.
                ("team-before-b", changed, applied_at - 1, StatusCode::OK),
                ("team-early", former, applied_at - 30, StatusCode::OK),
                ("team-late", former, applied_at + 600, StatusCode::BAD_REQUEST),
                ("team-old", former, restore_point - 600, StatusCode::BAD_REQUEST),
            ] {
                let answer = harness
                    .admin(
                        Method::POST,
                        "/v1/admin/restore/quotes",
                        &quote(customer, treasury, created),
                    )
                    .await?;
                ensure!(answer.status == status, "{customer}: {}", answer.body);
            }

            // The rescan credits the payment to the same deposit of the re-issued address, at its
            // delivered credit.
            let imported = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": [credited_delivery], "reason": "receiver log"}),
                )
                .await?;
            ensure!(imported.body["data"][0]["result"] == "imported", "{}", imported.body);
            let reissued_address: Uuid = sqlx::query_scalar(
                "SELECT id FROM addresses WHERE address = $1 AND chain_id = 1",
            )
            .bind(format!("{forwarder:#x}"))
            .fetch_one(&harness.pool)
            .await?;
            let rederived =
                record_deposit(&harness, tx_hash, reissued_address, U256::from(10_u64).pow(U256::from(20)))
                    .await?;
            ensure!(rederived == deposit);
            reconfirm(&harness).await?;
            confirm(&harness, deposit, forwarder, 20_000_000).await?;
            let valued = valuation(&harness, deposit).await?;
            ensure!((&valued.2, &valued.3) == (&credited.2, &credited.3), "{valued:?}");
            let customer: String = sqlx::query_scalar(
                "SELECT customer.client_reference_id FROM deposits AS deposit \
                 JOIN customers AS customer ON customer.id = deposit.customer_id \
                 WHERE deposit.id = $1",
            )
            .bind(deposit)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(customer == "team-b");
            ensure!(restore.restored_cursors.get(&1) == Some(&100));
            Ok(())
        })
    })
    .await
}

/// A `treasury.updated` announcing that `treasury` became `active` from `pending`, as the time-lock
/// renders it, at `created`.
fn application_event(
    harness: &Harness,
    treasury: Uuid,
    chain_id: u64,
    address: Address,
    created: i64,
) -> Value {
    json!({
        "id": topup::ids::format(topup::ids::EVENT, Uuid::new_v4()),
        "object": "event",
        "account": harness.account_id(),
        "livemode": true,
        "type": "treasury.updated",
        "created": created,
        "actor": "system",
        "request": null,
        "data": {
            "object": {
                "id": topup::ids::format(topup::ids::TREASURY, treasury),
                "object": "treasury",
                "livemode": true,
                "chain_id": chain_id,
                "address": format!("{address:#x}"),
                "kind": "eoa",
                "status": "active",
            },
            "previous_attributes": {"status": "pending"},
        },
    })
}

#[tokio::test]
async fn a_treasury_change_is_applied_again_only_as_its_signed_application_shows_it() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let now = Utc::now().timestamp();
            let changed = Address::repeat_byte(0xb3);
            // A change the merchant canceled, restored canceled.
            let canceled = seed::schedule_treasury(
                &harness.pool,
                harness.account.id,
                true,
                1,
                Address::repeat_byte(0xb4),
                Utc::now() - TimeDelta::hours(1),
            )
            .await?;
            sqlx::query(
                "UPDATE treasuries SET canceled_at = now(), cancellation_reason = 'requested' \
                 WHERE id = $1",
            )
            .bind(canceled)
            .execute(&harness.pool)
            .await?;
            // A change whose time-lock ends in an hour.
            let pending = seed::schedule_treasury(
                &harness.pool,
                harness.account.id,
                true,
                1,
                changed,
                Utc::now() + TimeDelta::hours(1),
            )
            .await?;
            harness.restore().await?;
            let apply = |event: &Value| {
                let delivery = harness.delivered(event);
                let harness = &harness;
                async move {
                    harness
                        .admin(
                            Method::POST,
                            "/v1/admin/restore/treasuries/apply",
                            &json!({"delivery": delivery, "reason": "INC-1"}),
                        )
                        .await
                }
            };
            let refused = |answer: &Answer, status: StatusCode| {
                ensure!(
                    answer.status == status,
                    "{}: {}",
                    answer.status,
                    answer.body
                );
                Ok(())
            };

            // Unknown to the restored database: the merchant proves it again after the unfreeze.
            refused(
                &apply(&application_event(
                    &harness,
                    Uuid::new_v4(),
                    1,
                    changed,
                    now,
                ))
                .await?,
                StatusCode::NOT_FOUND,
            )?;
            // Canceled in the restored database, yet signed as applied: escalate.
            refused(
                &apply(&application_event(
                    &harness,
                    canceled,
                    1,
                    Address::repeat_byte(0xb4),
                    now,
                ))
                .await?,
                StatusCode::BAD_REQUEST,
            )?;
            // Signed correctly, but naming another address or chain than the restored change.
            for (chain_id, address) in [(1, Address::repeat_byte(0xee)), (8453, changed)] {
                refused(
                    &apply(&application_event(
                        &harness, pending, chain_id, address, now,
                    ))
                    .await?,
                    StatusCode::BAD_REQUEST,
                )?;
            }
            // An object of another mode than its event's.
            let mut other_mode = application_event(&harness, pending, 1, changed, now);
            other_mode["data"]["object"]["livemode"] = json!(false);
            refused(&apply(&other_mode).await?, StatusCode::BAD_REQUEST)?;
            // Applied before its time-lock ended, or in the future.
            for created in [now, now + 7_200] {
                refused(
                    &apply(&application_event(&harness, pending, 1, changed, created)).await?,
                    StatusCode::BAD_REQUEST,
                )?;
            }

            // Once its time-lock ended it applies, screened when screening answers: a sanctions
            // list naming it now refuses it; a clear answer records the screening.
            sqlx::query(
                "UPDATE treasuries SET effective_at = now() - interval '10 minutes' WHERE id = $1",
            )
            .bind(pending)
            .execute(&harness.pool)
            .await?;
            let event = application_event(&harness, pending, 1, changed, now - 60);
            harness.screening.answer(Some(true));
            refused(&apply(&event).await?, StatusCode::BAD_REQUEST)?;
            let status: Option<chrono::DateTime<Utc>> =
                sqlx::query_scalar("SELECT applied_at FROM treasuries WHERE id = $1")
                    .bind(pending)
                    .fetch_one(&harness.pool)
                    .await?;
            ensure!(status.is_none());
            harness.screening.answer(Some(false));
            let applied = apply(&event).await?;
            ensure!(applied.status == StatusCode::OK, "{}", applied.body);
            ensure!(applied.body["applied"] == true);
            let (applied_at, screened_now): (i64, bool) = sqlx::query_as(
                "SELECT extract(epoch FROM applied_at)::bigint, \
                        screened_at > now() - interval '1 minute' \
                 FROM treasuries WHERE id = $1",
            )
            .bind(pending)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(applied_at == now - 60 && screened_now);
            // Audited in the transaction that applied it, once.
            let audited: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM audit WHERE action = 'restore.treasury_apply'",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!(audited == 1);
            Ok(())
        })
    })
    .await
}

/// A delivered `deposit.reversed` of deposit revision `revision` at the router transaction's
/// position 0 on chain 1, paid to `address`, whose successor is `replaced_by`.
fn reversed_event(
    harness: &Harness,
    revision: u64,
    address: Address,
    replaced_by: Option<Uuid>,
) -> Value {
    let deposit = topup_core::identity::deposit_revision_id(1, ROUTER_TX, 0, revision);
    json!({
        "id": topup::ids::format(
            topup::ids::EVENT,
            topup_core::identity::reversed_event_id(deposit)
        ),
        "object": "event",
        "account": harness.account_id(),
        "livemode": true,
        "type": "deposit.reversed",
        "created": 1_790_000_100,
        "actor": "system",
        "request": null,
        "data": {"object": {
            "id": topup::ids::format(topup::ids::DEPOSIT, deposit),
            "object": "deposit",
            "livemode": true,
            "status": "reversed",
            "chain_id": 1,
            "tx_hash": format!("{ROUTER_TX:#x}"),
            "receipt_log_index": 0,
            "revision": revision,
            "log_index": 3,
            "block_number": 110,
            "block_hash": format!("{:#x}", B256::repeat_byte(0xaa)),
            "block_time": 1_790_000_000,
            "address": format!("{address:#x}"),
            "asset_contract": format!("{UNROUTED_TOKEN:#x}"),
            "from_address": format!("{:#x}", Address::repeat_byte(0x66)),
            "amount_atomic": "1000",
            "amount": null,
            "exchange_rate": null,
            "price_source": null,
            "valued_at": null,
            "replaces": null,
            "replaced_by": replaced_by.map(|id| topup::ids::format(topup::ids::DEPOSIT, id)),
            "created": 1_790_000_010,
            "metadata": {},
        }},
    })
}

#[tokio::test]
async fn a_reversed_deposit_is_restored_only_on_its_own_account_and_untaken_position() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let (forwarder, address_id) = harness.deposit_address("team-own").await?;
            // Another account's issued address.
            let (_, other_customer) = seed::create_account_and_customer(
                &harness.pool,
                &NewAccount::named("other"),
                "team-other",
            )
            .await?;
            let theirs = Address::repeat_byte(0x3c);
            seed::insert_address(
                &harness.pool,
                &seed::NewAddress {
                    id: Uuid::new_v4(),
                    customer_id: other_customer.id,
                    chain_id: 1,
                    route: harness.route.route.clone(),
                    salt: B256::repeat_byte(0x3c),
                    address: theirs,
                },
            )
            .await?;
            harness.restore().await?;
            let import = |event: Value| {
                let delivery = harness.delivered(&event);
                let harness = &harness;
                async move {
                    harness
                        .admin(
                            Method::POST,
                            "/v1/admin/restore/events",
                            &json!({"deliveries": [delivery], "reason": "receiver log"}),
                        )
                        .await
                }
            };
            let deposits = |pool: PgPool| async move {
                let count: i64 = sqlx::query_scalar("SELECT count(*) FROM deposits")
                    .fetch_one(&pool)
                    .await?;
                anyhow::Ok(count)
            };

            // Neither another account's address nor one never issued takes the deposit.
            for address in [theirs, Address::repeat_byte(0x3d)] {
                let answer = import(reversed_event(&harness, 0, address, None)).await?;
                ensure!(
                    answer.body["data"][0]["reversed_deposit"] == "address_unknown",
                    "{}",
                    answer.body
                );
            }
            ensure!(deposits(harness.pool.clone()).await? == 0);

            // The rescan reached the position first and recorded the final transfer under the
            // reversed deposit's id: it is not restored over it, and the status says so.
            record_deposit(
                &harness,
                ROUTER_TX,
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            let successor = topup_core::identity::deposit_revision_id(1, ROUTER_TX, 0, 1);
            let answer = import(reversed_event(&harness, 0, forwarder, Some(successor))).await?;
            ensure!(
                answer.body["data"][0]["reversed_deposit"] == "rescanned",
                "{}",
                answer.body
            );
            let state: String = sqlx::query_scalar("SELECT state FROM deposits WHERE id = $1")
                .bind(deposit_id(1, ROUTER_TX, 0))
                .fetch_one(&harness.pool)
                .await?;
            ensure!(state == "detected");
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            let findings = &status.body["delivered_events"]["findings"];
            ensure!(
                findings
                    .as_array()
                    .is_some_and(|findings| findings.iter().any(|finding| {
                        finding["type"] == "deposit.reversed" && finding["status"] == "rescanned"
                    })),
                "{}",
                status.body
            );
            Ok(())
        })
    })
    .await
}

/// The merchant's record of quote `id` for `customer` at `address`, created at `created`.
fn quote_record(
    harness: &Harness,
    id: Uuid,
    customer: &str,
    address: Address,
    created: i64,
) -> Value {
    json!({
        "account": harness.account_id(),
        "livemode": true,
        "id": topup::ids::format(topup::ids::QUOTE, id),
        "client_reference_id": customer,
        "chain_id": 1,
        "asset": "pha",
        "amount": 1_000,
        "amount_atomic": "100000000000000000000",
        "exchange_rate": "0.10000000",
        "address": format!("{address:#x}"),
        "created": created,
        "expires_at": created + 900,
        "reason": "the merchant's quote log",
    })
}

#[tokio::test]
async fn a_quote_the_backup_holds_is_reissued_idempotently_whenever_it_was_created() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            // A quote the service issued before the backup.
            let customer = seed::create_customer(
                &harness.pool,
                &seed::NewCustomer {
                    id: Uuid::new_v4(),
                    account_id: harness.account.id,
                    livemode: true,
                    client_reference_id: "team-held".to_owned(),
                    paused_scopes: Vec::new(),
                },
            )
            .await?;
            let held = seed::insert_address(
                &harness.pool,
                &seed::NewAddress {
                    id: Uuid::new_v4(),
                    customer_id: customer.id,
                    chain_id: 1,
                    route: harness.route.route.clone(),
                    salt: B256::repeat_byte(0x4e),
                    address: Address::repeat_byte(0x4e),
                },
            )
            .await?;
            let quote: Uuid = sqlx::query_scalar("SELECT quote_id FROM addresses WHERE id = $1")
                .bind(held.id)
                .fetch_one(&harness.pool)
                .await?;
            let restore = harness.restore().await?;
            let restore_point = restore.restore_point.context("restore point")?.timestamp();
            // Sent again, even as created long before the restore point, it is returned as it is.
            let record = quote_record(
                &harness,
                quote,
                "team-held",
                Address::repeat_byte(0x4e),
                restore_point - 3_600,
            );
            let answer = harness
                .admin(Method::POST, "/v1/admin/restore/quotes", &record)
                .await?;
            ensure!(answer.status == StatusCode::OK, "{}", answer.body);
            ensure!(answer.body["reissued"] == false, "{}", answer.body);
            // A quote the restored database does not hold, created that long before the restore
            // point, is not one the restore lost.
            let contracts = &harness.route.chain.contracts;
            let other = Uuid::new_v4();
            let address = forwarder_address(
                contracts.forwarder_factory,
                contracts.implementation,
                seed::FIXTURE_TREASURY,
                quote_salt(
                    harness.account_id(),
                    "team-old",
                    &topup::ids::format(topup::ids::QUOTE, other),
                ),
            );
            let answer = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/quotes",
                    &quote_record(&harness, other, "team-old", address, restore_point - 3_600),
                )
                .await?;
            ensure!(answer.status == StatusCode::BAD_REQUEST, "{}", answer.body);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_restore_without_a_restore_point_reissues_nothing() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            // A restored database without a heartbeat.
            let mut transaction = harness.owner.begin().await?;
            sqlx::query("SET LOCAL session_replication_role = replica")
                .execute(&mut *transaction)
                .await?;
            sqlx::query("DELETE FROM heartbeat")
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
            let restore = harness.restore().await?;
            ensure!(restore.restore_point.is_none());
            let address = harness.derived_address("team-np", 1);
            let answer = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/deposit_addresses",
                    &json!({
                        "account": harness.account_id(),
                        "livemode": true,
                        "client_reference_id": "team-np",
                        "address": format!("{address:#x}"),
                        "reason": "the merchant's export",
                    }),
                )
                .await?;
            ensure!(
                answer.status == StatusCode::BAD_REQUEST
                    && answer.body["error"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("no restore point")),
                "{}",
                answer.body
            );
            let quote = Uuid::new_v4();
            let answer = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/quotes",
                    &quote_record(
                        &harness,
                        quote,
                        "team-np",
                        Address::repeat_byte(0x51),
                        Utc::now().timestamp(),
                    ),
                )
                .await?;
            ensure!(
                answer.status == StatusCode::BAD_REQUEST
                    && answer.body["error"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("no restore point")),
                "{}",
                answer.body
            );
            let (addresses, quotes): (i64, i64) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM deposit_addresses), (SELECT count(*) FROM quotes)",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!((addresses, quotes) == (0, 0));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_time_lock_change_records_when_it_applied_not_when_its_pass_began() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let pending = seed::schedule_treasury(
                &harness.pool,
                harness.account.id,
                true,
                1,
                Address::repeat_byte(0xb5),
                Utc::now() - TimeDelta::hours(1),
            )
            .await?;
            let routes = Arc::new(
                topup::routes::RouteSet::new(vec![harness.route.clone()])
                    .map_err(anyhow::Error::msg)?,
            );
            // A pass that began half an hour ago, screening a batch, reaches the change now.
            let pass_began = Utc::now() - TimeDelta::minutes(30);
            ensure!(
                topup::treasuries::apply_due(
                    &harness.pool,
                    &routes,
                    &support::ClearScreener,
                    pass_began
                )
                .await?
                    == 1
            );
            let (applied_late, replaced_late): (bool, bool) = sqlx::query_as(
                "SELECT (SELECT applied_at > now() - interval '1 minute' FROM treasuries \
                         WHERE id = $1), \
                        (SELECT replaced_at > now() - interval '1 minute' FROM treasuries \
                         WHERE account_id = $2 AND replaced_at IS NOT NULL)",
            )
            .bind(pending)
            .bind(harness.account.id)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(applied_late && replaced_late);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_successor_the_reversal_did_not_name_is_not_a_rescan_conflict() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let (forwarder, address_id) = harness.deposit_address("team-unnamed").await?;
            harness.restore().await?;
            // The reversal recorded no successor: the transfer that took the position paid no
            // issued address then.
            let reversed = harness.delivered(&reversed_event(&harness, 0, forwarder, None));
            let import = |delivery: Value| {
                let harness = &harness;
                async move {
                    harness
                        .admin(
                            Method::POST,
                            "/v1/admin/restore/events",
                            &json!({"deliveries": [delivery], "reason": "receiver log"}),
                        )
                        .await
                }
            };
            let answer = import(reversed.clone()).await?;
            ensure!(
                answer.body["data"][0]["reversed_deposit"] == "restored",
                "{}",
                answer.body
            );
            // The rescan records the final transfer at the next revision: a successor, not a
            // conflict, though no delivery named it.
            record_deposit(
                &harness,
                ROUTER_TX,
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            let successor = topup_core::identity::deposit_revision_id(1, ROUTER_TX, 0, 1);
            let state: String = sqlx::query_scalar("SELECT state FROM deposits WHERE id = $1")
                .bind(successor)
                .fetch_one(&harness.pool)
                .await?;
            ensure!(state == "detected");
            let again = import(reversed).await?;
            ensure!(
                again.body["data"][0]["reversed_deposit"] == "recorded",
                "{}",
                again.body
            );
            let status = harness
                .admin(Method::GET, "/v1/admin/restore", &Value::Null)
                .await?;
            ensure!(
                status.body["delivered_events"]["findings"]
                    .as_array()
                    .is_some_and(|findings| findings
                        .iter()
                        .all(|finding| finding["status"] != "rescanned")),
                "{}",
                status.body
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_replaced_treasury_is_a_lost_replacement_only_beside_its_lost_application() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let changed = Address::repeat_byte(0xb6);
            let pending = seed::schedule_treasury(
                &harness.pool,
                harness.account.id,
                true,
                1,
                changed,
                Utc::now() - TimeDelta::hours(1),
            )
            .await?;
            let former: Uuid = sqlx::query_scalar(
                "SELECT id FROM treasuries WHERE account_id = $1 AND applied_at IS NOT NULL",
            )
            .bind(harness.account.id)
            .fetch_one(&harness.pool)
            .await?;
            harness.restore().await?;
            let object = |id: Uuid, address: Address, status: &str| {
                json!({
                    "id": topup::ids::format(topup::ids::TREASURY, id),
                    "status": status,
                    "chain_id": 1,
                    "address": format!("{address:#x}"),
                })
            };
            let verify = |treasuries: Vec<Value>| {
                let harness = &harness;
                async move {
                    let answer = harness
                        .admin(
                            Method::POST,
                            "/v1/admin/restore/treasuries/verify",
                            &json!({
                                "account": harness.account_id(),
                                "livemode": true,
                                "treasuries": treasuries,
                                "reason": "treasury events",
                            }),
                        )
                        .await?;
                    ensure!(answer.status == StatusCode::OK, "{}", answer.body);
                    Ok::<_, anyhow::Error>(
                        answer.body["data"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|row| row["result"].as_str().unwrap_or_default().to_owned())
                            .collect::<Vec<_>>(),
                    )
                }
            };
            let replaced = object(former, seed::FIXTURE_TREASURY, "replaced");
            // Beside its application, in either order.
            ensure!(
                verify(vec![replaced.clone(), object(pending, changed, "active")]).await?
                    == ["replacement_lost", "application_lost"]
            );
            // Alone, beside the pending change received canceled, or beside another address
            // received active, it is not explained: `differs`.
            ensure!(verify(vec![replaced.clone()]).await? == ["differs"]);
            ensure!(
                verify(vec![replaced.clone(), object(pending, changed, "canceled")]).await?
                    == ["differs", "cancellation_lost"]
            );
            ensure!(
                verify(vec![
                    replaced,
                    object(pending, Address::repeat_byte(0xb7), "active")
                ])
                .await?
                    == ["differs", "differs"]
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_treasury_change_on_a_chain_without_a_route_stays_pending() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let changed = Address::repeat_byte(0xb8);
            let pending = seed::schedule_treasury(
                &harness.pool,
                harness.account.id,
                true,
                8453,
                changed,
                Utc::now() - TimeDelta::hours(1),
            )
            .await?;
            harness.restore().await?;
            harness.screening.answer(Some(false));
            let event = application_event(
                &harness,
                pending,
                8453,
                changed,
                Utc::now().timestamp() - 60,
            );
            let answer = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/treasuries/apply",
                    &json!({"delivery": harness.delivered(&event), "reason": "INC-1"}),
                )
                .await?;
            ensure!(answer.status == StatusCode::BAD_REQUEST, "{}", answer.body);
            let applied: Option<chrono::DateTime<Utc>> =
                sqlx::query_scalar("SELECT applied_at FROM treasuries WHERE id = $1")
                    .bind(pending)
                    .fetch_one(&harness.pool)
                    .await?;
            ensure!(applied.is_none());
            Ok(())
        })
    })
    .await
}

/// A deposit's state and rejection reason.
async fn outcome(harness: &Harness, deposit: Uuid) -> Result<(String, Option<String>)> {
    Ok(
        sqlx::query_as("SELECT state, reason FROM deposits WHERE id = $1")
            .bind(deposit)
            .fetch_one(&harness.pool)
            .await?,
    )
}

/// The payment settings scenarios of a restore (docs/design/payment-settings.md §11, §12): an
/// undelivered later update, two deliveries in one second, and a merchant that holds no event.
#[tokio::test]
async fn restored_payment_settings_are_held_until_the_merchant_sends_its_complete_configuration()
-> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let (forwarder, address_id) = harness.deposit_address("team-held").await?;
            let restored = settings_state(&harness).await?.1;
            let restore = harness.restore().await?;
            // The transaction that records the restore holds every account and mode.
            ensure!(
                settings_state(&harness).await? == ("held".to_owned(), restored, Some(restore.id))
            );

            // The merchant removed PHA after the restore point, and that update was never
            // delivered. The two deliveries it holds share a `created` second and still accept
            // PHA: none is imported to choose or order a configuration, so none lifts the hold.
            let updated = |id: u8| {
                json!({
                    "id": topup::ids::format(topup::ids::EVENT, Uuid::from_bytes([id; 16])),
                    "object": "event",
                    "account": harness.account_id(),
                    "livemode": true,
                    "type": "payment_settings.updated",
                    "created": 1_790_000_000,
                    "actor": "system",
                    "request": null,
                    "data": {"object": {
                        "object": "payment_settings",
                        "livemode": true,
                        "status": "configured",
                        "chains": [{"chain_id": 1, "assets": [{"asset": "pha"}]}],
                    }},
                })
            };
            let refused = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({
                        "deliveries": [
                            harness.delivered(&updated(1)),
                            harness.delivered(&updated(2)),
                        ],
                        "reason": "the merchant's webhook log",
                    }),
                )
                .await?;
            ensure!(
                refused.status == StatusCode::BAD_REQUEST,
                "{}",
                refused.body
            );
            ensure!(
                refused.body["error"]["message"]
                    == "only deposit.credited, deposit.rejected, and deposit.reversed events are \
                        imported",
                "{}",
                refused.body
            );
            ensure!(settings_state(&harness).await?.0 == "held");

            // A payment of PHA recorded meanwhile names the restore, not a revision, and waits.
            let deposit = record_deposit(
                &harness,
                B256::repeat_byte(0x91),
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            ensure!(binding(&harness, deposit).await? == (None, Some(restore.id)));
            harness.unfreeze(&restore).await?;
            confirm(&harness, deposit, forwarder, 25_000_000).await?;
            ensure!(outcome(&harness, deposit).await? == ("detected".to_owned(), None));

            // Nothing is issued on held settings.
            let settings = harness
                .merchant(Method::GET, "/v1/payment_settings", &Value::Null)
                .await?;
            ensure!(settings.body["status"] == "held", "{}", settings.body);
            let config = harness
                .merchant(Method::GET, "/v1/config", &Value::Null)
                .await?;
            ensure!(config.body["assets"] == json!([]), "{}", config.body);
            let quote = harness
                .merchant(
                    Method::POST,
                    "/v1/quotes",
                    &json!({
                        "client_reference_id": "team-held", "amount": 100, "currency": "usd",
                        "chain_id": 1, "asset": "pha"
                    }),
                )
                .await?;
            ensure!(
                quote.body["error"]["code"] == "payment_settings_unconfirmed",
                "{}",
                quote.body
            );

            // A partial update is no reconfirmation: nothing of the restored settings is assumed.
            for partial in [
                json!({}),
                json!({"quote_creations_per_customer_per_minute": 5}),
            ] {
                let refused = harness
                    .merchant(Method::POST, "/v1/payment_settings", &partial)
                    .await?;
                ensure!(
                    refused.status == StatusCode::BAD_REQUEST,
                    "{}",
                    refused.body
                );
                ensure!(refused.body["error"]["code"] == "parameter_missing");
                ensure!(refused.body["error"]["param"] == "chains");
            }
            ensure!(settings_state(&harness).await?.0 == "held");
            ensure!(binding(&harness, deposit).await? == (None, Some(restore.id)));

            // The merchant's complete configuration, without PHA, lifts the hold and binds the
            // waiting payment to it, which then rejects it.
            let lifted = harness
                .merchant(Method::POST, "/v1/payment_settings", &json!({"chains": []}))
                .await?;
            ensure!(lifted.status == StatusCode::OK, "{}", lifted.body);
            ensure!(lifted.body["status"] == "configured", "{}", lifted.body);
            let revision = revision_of(&lifted.body)?;
            ensure!(revision != restored);
            ensure!(binding(&harness, deposit).await? == (Some(revision), None));
            // Its next attempt, as the waiting deposit's backoff comes due.
            sqlx::query("UPDATE deposits SET next_attempt_at = now() WHERE id = $1")
                .bind(deposit)
                .execute(&harness.pool)
                .await?;
            confirm(&harness, deposit, forwarder, 25_000_000).await?;
            ensure!(
                outcome(&harness, deposit).await?
                    == ("rejected".to_owned(), Some("asset_not_accepted".to_owned()))
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn an_unchanged_complete_configuration_reconfirms_held_settings() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let (forwarder, address_id) = harness.deposit_address("team-same").await?;
            let pha = json!({"chains": [{"chain_id": 1, "assets": [{"asset": "pha"}]}]});
            let mut limited = pha.clone();
            limited["quote_creations_per_customer_per_minute"] = json!(5);
            let before = harness
                .merchant(Method::POST, "/v1/payment_settings", &limited)
                .await?;
            ensure!(before.status == StatusCode::OK, "{}", before.body);
            let restored = revision_of(&before.body)?;
            let restore = harness.restore().await?;
            let deposit = record_deposit(
                &harness,
                B256::repeat_byte(0x92),
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            harness.unfreeze(&restore).await?;
            let updates = || async {
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM events \
                     WHERE account_id = $1 AND type = 'payment_settings.updated'",
                )
                .bind(harness.account.id)
                .fetch_one(&harness.pool)
                .await
            };
            let announced = updates().await?;

            // The same chains reconfirm: a new revision and event, and the rate the merchant did
            // not send again takes its default rather than the restored value.
            let reconfirmed = harness
                .merchant(Method::POST, "/v1/payment_settings", &pha)
                .await?;
            ensure!(reconfirmed.status == StatusCode::OK, "{}", reconfirmed.body);
            ensure!(reconfirmed.body["status"] == "configured");
            ensure!(reconfirmed.body["quote_creations_per_customer_per_minute"].is_null());
            let revision = revision_of(&reconfirmed.body)?;
            ensure!(revision != restored);
            ensure!(updates().await? == announced + 1);
            ensure!(binding(&harness, deposit).await? == (Some(revision), None));
            confirm(&harness, deposit, forwarder, 25_000_000).await?;
            ensure!(
                valuation(&harness, deposit).await?
                    == (
                        "confirmed".to_owned(),
                        "spot".to_owned(),
                        "25000000".to_owned(),
                        "2500".to_owned()
                    )
            );

            // Once configured, a request that changes nothing writes nothing.
            let again = harness
                .merchant(Method::POST, "/v1/payment_settings", &pha)
                .await?;
            ensure!(revision_of(&again.body)? == revision);
            ensure!(updates().await? == announced + 1);
            Ok(())
        })
    })
    .await
}

/// The barrier (docs/design/payment-settings.md §7): a recorder holds its account and mode's
/// state row from before its insert to its commit, and the hold lift locks it for update.
#[tokio::test]
async fn a_recorder_racing_the_hold_lift_is_bound_by_it() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let (_, address_id) = harness.deposit_address("team-race").await?;
            harness.restore().await?;
            let scope = topup::tenancy::Scope::new(harness.account.id, true);
            let document = topup::payment_config::Document::accepting([&harness.route]);
            let amount = U256::from(10_u64).pow(U256::from(20));
            let wait = std::time::Duration::from_millis(300);
            let bound = std::time::Duration::from_secs(5);

            // A rescan reads `held` and records a deposit, and has not committed yet.
            let mut recorder = harness.pool.begin().await?;
            let first = db::insert_deposit_in(
                &mut recorder,
                &new_deposit(&harness, B256::repeat_byte(0x81), address_id, amount),
                db::Evidence::Finalized,
            )
            .await?
            .context("recorded")?;

            // The merchant's reconfirmation waits for that transaction, then binds its deposit.
            let (written_tx, mut written) = tokio::sync::oneshot::channel();
            let (commit, committed) = tokio::sync::oneshot::channel::<()>();
            let lift = tokio::spawn({
                let pool = harness.pool.clone();
                async move {
                    let mut transaction = pool.begin().await?;
                    let write =
                        topup::payment_config::write(&mut transaction, scope, &document, "test")
                            .await?;
                    let revision = topup::payment_config::load(&mut transaction, scope)
                        .await?
                        .revision;
                    written_tx
                        .send((write.bound, revision))
                        .map_err(|_| anyhow::anyhow!("the test stopped"))?;
                    committed.await?;
                    transaction.commit().await?;
                    anyhow::Ok(())
                }
            });
            tokio::time::sleep(wait).await;
            ensure!(
                written.try_recv().is_err(),
                "the lift did not wait for the recorder"
            );
            recorder.commit().await?;
            let (bound_pending, revision) = tokio::time::timeout(bound, written).await??;
            ensure!(bound_pending == 1);

            // A rescan that starts during the lift waits for it and reads the new revision.
            let recording = tokio::spawn({
                let pool = harness.pool.clone();
                let deposit = new_deposit(&harness, B256::repeat_byte(0x82), address_id, amount);
                async move { db::insert_deposit(&pool, &deposit).await }
            });
            tokio::time::sleep(wait).await;
            ensure!(
                !recording.is_finished(),
                "the recorder did not wait for the lift"
            );
            commit
                .send(())
                .map_err(|_| anyhow::anyhow!("the lift stopped"))?;
            tokio::time::timeout(bound, lift).await???;
            ensure!(tokio::time::timeout(bound, recording).await???);

            ensure!(binding(&harness, first).await? == (Some(revision), None));
            let second = deposit_id(1, B256::repeat_byte(0x82), 0);
            ensure!(binding(&harness, second).await? == (Some(revision), None));
            let pending: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM deposits WHERE settings_hold_id IS NOT NULL",
            )
            .fetch_one(&harness.pool)
            .await?;
            ensure!(pending == 0);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn delivered_outcomes_stand_whatever_the_reconfirmed_settings_accept() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let (forwarder, address_id) = harness.deposit_address("team-delivered").await?;
            let restore = harness.restore().await?;
            let address = format!("{forwarder:#x}");
            let amount = "100000000000000000000";
            let credited_hash = B256::repeat_byte(0x61);
            let credited = deposit_id(1, credited_hash, 0);
            let rejected_hash = B256::repeat_byte(0x62);
            let rejected = deposit_id(1, rejected_hash, 0);
            let delivered_credit = credited_event(
                &harness,
                credited,
                credited_hash,
                &address,
                amount,
                (2_500, "0.25000000", "spot"),
            );
            let mut delivered_rejection = credited_event(
                &harness,
                rejected,
                rejected_hash,
                &address,
                amount,
                (2_500, "0.25000000", "spot"),
            );
            delivered_rejection["id"] = json!(topup::ids::format(
                topup::ids::EVENT,
                topup_core::identity::event_id("deposit.rejected", rejected)
            ));
            delivered_rejection["type"] = json!("deposit.rejected");
            delivered_rejection["data"]["object"]["status"] = json!("rejected");
            delivered_rejection["data"]["object"]["rejection_reason"] = json!("below_minimum");
            let imported = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({
                        "deliveries": [
                            harness.delivered(&delivered_credit),
                            harness.delivered(&delivered_rejection),
                        ],
                        "reason": "the merchant's webhook log",
                    }),
                )
                .await?;
            ensure!(imported.status == StatusCode::OK, "{}", imported.body);
            let amount = U256::from(10_u64).pow(U256::from(20));
            record_deposit(&harness, credited_hash, address_id, amount).await?;
            harness.unfreeze(&restore).await?;

            // The reconfirmed settings accept nothing, yet the delivered credit stands.
            let none = harness
                .merchant(Method::POST, "/v1/payment_settings", &json!({"chains": []}))
                .await?;
            ensure!(none.status == StatusCode::OK, "{}", none.body);
            ensure!(binding(&harness, credited).await? == (Some(revision_of(&none.body)?), None));
            confirm(&harness, credited, forwarder, 20_000_000).await?;
            ensure!(
                valuation(&harness, credited).await?
                    == (
                        "confirmed".to_owned(),
                        "spot".to_owned(),
                        "25000000".to_owned(),
                        "2500".to_owned()
                    )
            );

            // Settings that accept PHA, and its amount within every bound, do not undo the
            // delivered rejection.
            let pha = harness
                .merchant(
                    Method::POST,
                    "/v1/payment_settings",
                    &json!({"chains": [{"chain_id": 1, "assets": [{"asset": "pha"}]}]}),
                )
                .await?;
            ensure!(pha.status == StatusCode::OK, "{}", pha.body);
            record_deposit(&harness, rejected_hash, address_id, amount).await?;
            ensure!(binding(&harness, rejected).await? == (Some(revision_of(&pha.body)?), None));
            confirm(&harness, rejected, forwarder, 20_000_000).await?;
            ensure!(
                outcome(&harness, rejected).await?
                    == ("rejected".to_owned(), Some("below_minimum".to_owned()))
            );
            Ok(())
        })
    })
    .await
}

/// Sanctions screening that names one sender.
struct NamesSender(Address);

#[async_trait]
impl topup_adapters::risk::SanctionsSource for NamesSender {
    async fn sanctions(
        &self,
        address: Address,
        _block_number: u64,
    ) -> topup_core::screening::SanctionsResult {
        let answer = if address == self.0 {
            topup_core::screening::SanctionsVerdict::Sanctioned
        } else {
            topup_core::screening::SanctionsVerdict::Clear
        };
        topup_core::screening::SanctionsResult::new(answer)
    }
}

/// A sanctions hit on a delivered credit is compliance, not commercial policy (design §11): the
/// credit stands, the alert is raised, and its forwarder is never offered for a sweep.
#[tokio::test]
#[tracing_test::traced_test]
async fn a_sanctions_hit_keeps_a_delivered_credit_and_blocks_its_sweep() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let (forwarder, address_id) = harness.deposit_address("team-listed").await?;
            let restore = harness.restore().await?;
            let tx_hash = B256::repeat_byte(0x63);
            let deposit = deposit_id(1, tx_hash, 0);
            let delivered = credited_event(
                &harness,
                deposit,
                tx_hash,
                &format!("{forwarder:#x}"),
                "100000000000000000000",
                (2_500, "0.25000000", "spot"),
            );
            let imported = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": [harness.delivered(&delivered)], "reason": "log"}),
                )
                .await?;
            ensure!(imported.status == StatusCode::OK, "{}", imported.body);
            record_deposit(
                &harness,
                tx_hash,
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            confirm(&harness, deposit, forwarder, 20_000_000).await?;
            sqlx::query("UPDATE deposits SET final_at = now() WHERE id = $1")
                .bind(deposit)
                .execute(&harness.pool)
                .await?;

            // A disagreement on replay cannot undo delivered value or create a sanctions hit.
            struct SplitSanctions;
            #[async_trait]
            impl topup_adapters::risk::SanctionsSource for SplitSanctions {
                async fn sanctions(&self,_:Address,_block_number:u64)->topup_core::screening::SanctionsResult {
                    topup_core::screening::SanctionsResult::new(topup_core::screening::SanctionsVerdict::Uncertain)
                }
            }
            let before=topup::db::get_deposit(&harness.pool,deposit).await?.context("replayed deposit")?.credit_minor;
            let split=topup::steps::screen::ScreenStep::new(harness.pool.clone(),[
                topup::steps::screen::ScreenRoute::new(harness.route.clone(),Arc::new(SplitSanctions))])?;
            run_pump(&harness,deposit,StepSet::new(Box::new(Unreached),Box::new(split))).await?;
            let held=topup::db::get_deposit(&harness.pool,deposit).await?.context("held replay")?;
            ensure!(held.state==topup_core::deposit::DepositState::Confirmed);
            ensure!(held.credit_minor==before);
            let hit: bool = sqlx::query_scalar("SELECT sanctions_hit_at IS NOT NULL FROM deposits WHERE id=$1").bind(deposit).fetch_one(&harness.pool).await?;
            ensure!(!hit);
            let recorded:Value=sqlx::query_scalar("SELECT evidence FROM transitions WHERE deposit_id=$1 ORDER BY created_at DESC LIMIT 1").bind(deposit).fetch_one(&harness.pool).await?;
            ensure!(recorded["sanctions_hold"]==true);
            sqlx::query("UPDATE deposits SET next_attempt_at=now()-interval '1 second' WHERE id=$1").bind(deposit).execute(&harness.pool).await?;

            // The list now names the sender.
            let screen = topup::steps::screen::ScreenStep::new(
                harness.pool.clone(),
                [topup::steps::screen::ScreenRoute::new(
                    harness.route.clone(),
                    Arc::new(NamesSender(Address::repeat_byte(0x74))),
                )],
            )?;
            run_pump(
                &harness,
                deposit,
                StepSet::new(Box::new(Unreached), Box::new(screen)),
            )
            .await?;
            ensure!(outcome(&harness, deposit).await? == ("credited".to_owned(), None));
            ensure!(logs_contain("TopupDeliveredCreditSanctioned"));
            let rejected: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM events WHERE type = 'deposit.rejected' AND object_id = $1",
            )
            .bind(deposit)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(rejected == 0);
            let sanctioned: bool = sqlx::query_scalar(
                "SELECT sanctions_hit_at IS NOT NULL FROM deposits WHERE id = $1",
            )
            .bind(deposit)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(sanctioned);

            harness.unfreeze(&restore).await?;
            harness.screening.answer(Some(false));
            let refund = json!({
                "deposit": topup::ids::format(topup::ids::DEPOSIT, deposit),
                "destination_address": format!("{:#x}", Address::repeat_byte(0x75)),
            });
            let refused = harness
                .merchant(Method::POST, "/v1/refunds", &refund)
                .await?;
            ensure!(
                refused.status == StatusCode::BAD_REQUEST,
                "{}",
                refused.body
            );
            ensure!(
                refused.body["error"]["code"] == "deposit_not_refundable",
                "{}",
                refused.body
            );
            let sweepable = harness
                .merchant(
                    Method::GET,
                    &format!(
                        "/v1/forwarders?sweepable={:#x}",
                        harness.route.asset.contract
                    ),
                    &Value::Null,
                )
                .await?;
            ensure!(sweepable.status == StatusCode::OK, "{}", sweepable.body);
            ensure!(sweepable.body["data"] == json!([]), "{}", sweepable.body);
            // The recorded hit is what keeps it: without it, the forwarder is offered.
            sqlx::query("UPDATE deposits SET sanctions_hit_at = NULL WHERE id = $1")
                .bind(deposit)
                .execute(&harness.owner)
                .await?;
            let offered = harness
                .merchant(
                    Method::GET,
                    &format!(
                        "/v1/forwarders?sweepable={:#x}",
                        harness.route.asset.contract
                    ),
                    &Value::Null,
                )
                .await?;
            ensure!(
                offered.body["data"][0]["address"] == format!("{forwarder:#x}"),
                "{}",
                offered.body
            );
            let allowed = harness
                .merchant(Method::POST, "/v1/refunds", &refund)
                .await?;
            ensure!(allowed.status == StatusCode::OK, "{}", allowed.body);
            Ok(())
        })
    })
    .await
}

/// A delivered rejection the chain contradicts holds the deposit, as a delivered credit does: the
/// whole transfer identity is checked, not only its token and amount.
#[tokio::test]
async fn a_delivered_rejection_the_chain_contradicts_holds_the_deposit() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let (forwarder, address_id) = harness.deposit_address("team-other-sender").await?;
            let restore = harness.restore().await?;
            let tx_hash = B256::repeat_byte(0x64);
            let deposit = deposit_id(1, tx_hash, 0);
            let mut rejection = credited_event(
                &harness,
                deposit,
                tx_hash,
                &format!("{forwarder:#x}"),
                "100000000000000000000",
                (2_500, "0.25000000", "spot"),
            );
            rejection["id"] = json!(topup::ids::format(
                topup::ids::EVENT,
                topup_core::identity::event_id("deposit.rejected", deposit)
            ));
            rejection["type"] = json!("deposit.rejected");
            rejection["data"]["object"]["status"] = json!("rejected");
            rejection["data"]["object"]["rejection_reason"] = json!("below_minimum");
            // The delivery names another sender than the chain's.
            rejection["data"]["object"]["from_address"] =
                json!(format!("{:#x}", Address::repeat_byte(0x99)));
            let imported = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/events",
                    &json!({"deliveries": [harness.delivered(&rejection)], "reason": "log"}),
                )
                .await?;
            ensure!(imported.status == StatusCode::OK, "{}", imported.body);
            record_deposit(
                &harness,
                tx_hash,
                address_id,
                U256::from(10_u64).pow(U256::from(20)),
            )
            .await?;
            harness.unfreeze(&restore).await?;
            reconfirm(&harness).await?;
            confirm(&harness, deposit, forwarder, 20_000_000).await?;
            ensure!(outcome(&harness, deposit).await? == ("detected".to_owned(), None));
            let evidence: Value = sqlx::query_scalar(
                "SELECT evidence FROM transitions WHERE deposit_id = $1 \
                 ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .bind(deposit)
            .fetch_one(&harness.pool)
            .await?;
            ensure!(
                evidence["error"] == "delivered_event_contradicts_chain",
                "{evidence}"
            );
            ensure!(evidence["field"] == "from_address", "{evidence}");
            let (_, findings) =
                restore_mode::delivered_event_findings(&harness.pool, &restore).await?;
            ensure!(
                findings.iter().any(
                    |finding| finding.deposit_id == deposit && finding.status == "contradicted"
                ),
                "{findings:?}"
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn failed_critical_checks_block_unfreeze_and_override_is_explicit_and_audited() -> Result<()>
{
    support::with_database(|database| {
        Box::pin(async move {
            let harness = Harness::new(database).await?;
            let restore = harness.restore().await?;
            harness
                .scan_to(1000, restore.detected_at + TimeDelta::seconds(1))
                .await?;
            sqlx::query("UPDATE addresses SET backfilled = true")
                .execute(&harness.pool)
                .await?;
            restore_mode::record_validation(
                &harness.owner,
                &restore,
                "incomplete",
                &["custody_balance RPC failed".to_owned()],
            )
            .await?;
            for reason in ["reconciled", "override-critical-checks:"] {
                let answer = harness
                    .admin(
                        Method::POST,
                        "/v1/admin/restore/unfreeze",
                        &checklist(reason),
                    )
                    .await?;
                ensure!(answer.status == StatusCode::BAD_REQUEST);
                ensure!(restore_mode::is_frozen(&harness.pool).await?);
            }
            let answer = harness
                .admin(
                    Method::POST,
                    "/v1/admin/restore/unfreeze",
                    &checklist(
                        "override-critical-checks: INC-42 independent custody evidence accepted",
                    ),
                )
                .await?;
            ensure!(answer.status == StatusCode::OK, "{}", answer.body);
            let reason: String = sqlx::query_scalar(
                "SELECT reason FROM audit WHERE action = 'restore.critical_checks_override'",
            )
            .fetch_one(&harness.owner)
            .await?;
            ensure!(reason == "INC-42 independent custody evidence accepted");
            ensure!(!restore_mode::is_frozen(&harness.pool).await?);
            Ok(())
        })
    })
    .await
}
