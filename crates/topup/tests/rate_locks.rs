//! PostgreSQL-backed C10 rate-lock API and lifecycle tests.

mod support;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use chrono::Utc;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use sqlx::Connection as _;
use sqlx::Row as _;
use tokio::sync::Notify;
use topup::api::{AppState, PublicOrigin, VerificationKey};
use topup::audit::Actor;
use topup::db::{Account, Customer};
use topup::locks::pricing::ValidatedQuote;
use topup::locks::{self, QuoteProvider, RateLockError};
use topup::tenancy::Scope;
use topup_adapters::attestation::DstackAttestor;
use topup_adapters::pricing::Observation;
use topup_core::deposit::{DepositState, RejectReason};
use topup_core::money::{AtomicAmount, MinorAmount, PRICE_SCALE, ScaledPrice};
use topup_core::route::RouteFile;
use topup_core::valuation::{SourceId, UnixSeconds};
use tower::ServiceExt;
use tracing_test::traced_test;
use uuid::Uuid;

use support::seed::{self, NewAccount, NewCustomer};
use support::{
    ManualClock, TEST_ORIGIN, TestDatabase, merchant_request, merchant_request_with_key,
    public_key_base64,
};

const ADMIN_KID: &str = "admin/v1";

struct FixedQuote;

#[async_trait]
impl QuoteProvider for FixedQuote {
    async fn quote(
        &self,
        _route: &RouteFile,
        _deadline: tokio::time::Instant,
    ) -> Result<ValidatedQuote, Value> {
        Ok(ValidatedQuote {
            price: ScaledPrice::new(100_000_000, PRICE_SCALE).expect("fixed quote"),
            evidence: json!({
                "mode": "spot",
                "primary": Observation {
                    source: SourceId::new("test"),
                    price: ScaledPrice::new(100_000_000, PRICE_SCALE).expect("fixed quote"),
                    observed_at: UnixSeconds::new(
                        u64::try_from(Utc::now().timestamp()).expect("non-negative timestamp")
                    ),
                }.price.value().to_string()
            }),
        })
    }
}

#[tokio::test]
async fn concurrent_quotes_share_one_price_fetch() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            struct CountedPrice(AtomicUsize);
            #[async_trait]
            impl topup_adapters::pricing::PriceSource for CountedPrice {
                async fn observe(
                    &self,
                ) -> Result<Observation, topup_adapters::pricing::PriceError> {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    Ok(Observation {
                        source: SourceId::new("chainlink"),
                        price: ScaledPrice::new(100_000_000, PRICE_SCALE).unwrap(),
                        observed_at: UnixSeconds::new(
                            u64::try_from(Utc::now().timestamp()).unwrap(),
                        ),
                    })
                }
            }
            struct SimultaneousQuotes {
                provider: locks::ConfiguredQuoteProvider,
                gate: tokio::sync::Barrier,
            }
            #[async_trait]
            impl QuoteProvider for SimultaneousQuotes {
                async fn quote(
                    &self,
                    route: &RouteFile,
                    _deadline: tokio::time::Instant,
                ) -> Result<ValidatedQuote, Value> {
                    self.gate.wait().await;
                    self.provider.quote(route, _deadline).await
                }
            }
            let (account, _) = seed_product(&database.app_pool, "coalesced-quotes").await?;
            let customer = seed_account(&database.app_pool, account.id, "concurrent").await?;
            let mut route = test_route();
            route.pricing.mode = topup_core::route::PricingMode::Stablecoin;
            let source = Arc::new(CountedPrice(AtomicUsize::new(0)));
            let runtime = Arc::new(locks::pricing::PricingRuntime::injected(
                source.clone(),
                None,
                None,
            ));
            let runtimes = Arc::new(std::collections::BTreeMap::from([(
                (route.route.clone(), route.version),
                runtime,
            )]));
            let provider: Arc<dyn QuoteProvider> = Arc::new(SimultaneousQuotes {
                provider: locks::ConfiguredQuoteProvider::from_runtimes(runtimes),
                gate: tokio::sync::Barrier::new(10),
            });
            let results = futures_util::future::join_all((0..10).map(|_| {
                locks::price(
                    &database.app_pool,
                    &provider,
                    &account,
                    &customer,
                    &route,
                    MinorAmount::new(100),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(60),
                )
            }))
            .await;
            for result in results {
                let _ = result?;
            }
            ensure!(
                source.0.load(Ordering::SeqCst) == 1,
                "10 quotes must share one source fetch"
            );
            Ok(())
        })
    })
    .await
}

/// A quote is created with its `client_secret` and its saved response in one transaction, so a
/// request that failed while rendering its response created nothing, and is replayed as it
/// failed: a retry with the same `Idempotency-Key` never creates a quote (Stripe saves the result
/// of every executed request, including a `500`).
#[tokio::test]
async fn a_failure_before_the_response_is_saved_creates_no_quote_and_is_replayed() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[44; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        seed_account(&database.app_pool, product.id, "retried").await?;
        let app = topup::api::router(AppState {
            pool: database.app_pool.clone(),
            routes: Arc::new(test_routes()),
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(
                ADMIN_KID.to_owned(),
                &public_key_base64(&admin_key),
            )
            .map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(FixedQuote),
            client_reads: Arc::default(),
            rate_limits: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        })
        .0;
        let create = || -> Result<_> {
            Ok(merchant_request_with_key(
                Method::POST,
                "/v1/quotes",
                serde_json::to_vec(&json!({
                    "client_reference_id": "retried", "amount": 100, "currency": "usd",
                    "chain_id": 1, "asset": "pha"
                }))?,
                &product_key,
                "create-once",
            ))
        };
        // Rendering the response fails after the quote is inserted.
        sqlx::query("REVOKE SELECT ON pending_transfers FROM topup_app")
            .execute(&database.owner_pool)
            .await?;
        let failed = app.clone().oneshot(create()?).await?;
        ensure!(failed.status() == StatusCode::INTERNAL_SERVER_ERROR);
        ensure!(failed.headers().get("idempotent-replayed").is_none());
        ensure!(response_json(failed).await?["error"]["code"] == "internal_error");
        sqlx::query("GRANT SELECT ON pending_transfers TO topup_app")
            .execute(&database.owner_pool)
            .await?;
        let retried = app.clone().oneshot(create()?).await?;
        ensure!(retried.status() == StatusCode::INTERNAL_SERVER_ERROR);
        ensure!(retried.headers()["idempotent-replayed"] == "true");
        let (count, with_secret): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(client_secret_hash) FROM quotes WHERE account_id = $1",
        )
        .bind(product.id)
        .fetch_one(&database.app_pool)
        .await?;
        ensure!(
            count == 0 && with_secret == 0,
            "{count} quotes, {with_secret} with a secret"
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Prices at [`FixedQuote`], the first call only once released.
struct FirstQuoteWaits {
    calls: AtomicUsize,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl QuoteProvider for FirstQuoteWaits {
    async fn quote(
        &self,
        route: &RouteFile,
        _deadline: tokio::time::Instant,
    ) -> Result<ValidatedQuote, Value> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        FixedQuote.quote(route, _deadline).await
    }
}

/// A quotes API whose first price fetch waits for the test, and the same quote request with the
/// `Idempotency-Key` `slow`.
struct SlowQuote {
    app: axum::Router,
    quotes: Arc<FirstQuoteWaits>,
    account: Account,
    key: String,
}

impl SlowQuote {
    async fn new(database: &TestDatabase) -> Result<Self> {
        let admin_key = SigningKey::from_bytes(&[45; 32]);
        let (account, key) = seed_product(&database.app_pool, "phala-cloud").await?;
        seed_account(&database.app_pool, account.id, "slow").await?;
        let quotes = Arc::new(FirstQuoteWaits {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let app = topup::api::router(AppState {
            pool: database.app_pool.clone(),
            routes: Arc::new(test_routes()),
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(
                ADMIN_KID.to_owned(),
                &public_key_base64(&admin_key),
            )
            .map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: quotes.clone(),
            client_reads: Arc::default(),
            rate_limits: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        })
        .0;
        Ok(Self {
            app,
            quotes,
            account,
            key,
        })
    }

    fn create(&self) -> Result<axum::http::Request<Body>> {
        Ok(merchant_request_with_key(
            Method::POST,
            "/v1/quotes",
            serde_json::to_vec(&json!({
                "client_reference_id": "slow", "amount": 100, "currency": "usd",
                "chain_id": 1, "asset": "pha"
            }))?,
            &self.key,
            "slow",
        ))
    }

    /// Starts the request; it waits in its price fetch, before its transaction, until
    /// `quotes.release`.
    async fn start(&self) -> Result<tokio::task::JoinHandle<Result<Response, Infallible>>> {
        let request = tokio::spawn(self.app.clone().oneshot(self.create()?));
        self.quotes.entered.notified().await;
        Ok(request)
    }

    /// Makes the running request's key older than the takeover interval.
    async fn age_key(&self, database: &TestDatabase) -> Result<()> {
        sqlx::query(
            "UPDATE idempotency_keys SET created_at = now() - interval '2 minutes' \
             WHERE key = 'slow'",
        )
        .execute(&database.app_pool)
        .await?;
        Ok(())
    }

    async fn quotes(&self, database: &TestDatabase) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT count(*) FROM quotes WHERE account_id = $1")
                .bind(self.account.id)
                .fetch_one(&database.app_pool)
                .await?,
        )
    }
}

/// Holds quote inserts inside their transaction until [`release`](Self::release): a trigger
/// takes an advisory lock this connection holds.
struct InsertGate(sqlx::pool::PoolConnection<sqlx::Postgres>);

const INSERT_GATE: i64 = 704_300_001;

impl InsertGate {
    async fn close(database: &TestDatabase) -> Result<Self> {
        for statement in [
            "CREATE FUNCTION gate_quote_insert() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN PERFORM pg_advisory_xact_lock(704300001); RETURN NEW; END $$",
            "CREATE TRIGGER gate_quote_insert BEFORE INSERT ON quotes FOR EACH ROW \
             EXECUTE FUNCTION gate_quote_insert()",
        ] {
            sqlx::query(statement).execute(&database.owner_pool).await?;
        }
        let mut holder = database.owner_pool.acquire().await?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(INSERT_GATE)
            .execute(&mut *holder)
            .await?;
        Ok(Self(holder))
    }

    async fn release(mut self) -> Result<()> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(INSERT_GATE)
            .execute(&mut *self.0)
            .await?;
        Ok(())
    }
}

/// Waits until a backend of the test database waits on a lock of `kinds` (`pg_stat_activity`'s
/// `wait_event`, such as `advisory` or `transactionid`).
async fn wait_for_lock_wait(database: &TestDatabase, kinds: &[&str]) -> Result<()> {
    let kinds: Vec<String> = kinds.iter().map(|kind| (*kind).to_owned()).collect();
    for _ in 0..1_500 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
             AND wait_event_type = 'Lock' AND wait_event = ANY($1)",
        )
        .bind(&kinds)
        .fetch_one(&database.owner_pool)
        .await?;
        if waiting > 0 {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    anyhow::bail!("no backend waits on {kinds:?}")
}

/// A request slower than the takeover interval is fenced once a repeat takes its key over: the
/// repeat creates the quote, and the slow request, resuming after its price fetch, can no longer
/// commit, so one quote exists and every later repeat replays it.
#[tokio::test]
async fn a_takeover_fences_the_slow_request_it_replaced() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let slow = SlowQuote::new(&database).await?;
        let first = slow.start().await?;
        slow.age_key(&database).await?;
        let repeat = slow.app.clone().oneshot(slow.create()?).await?;
        ensure!(repeat.status() == StatusCode::OK);
        ensure!(repeat.headers().get("idempotent-replayed").is_none());
        let created = response_json(repeat).await?;

        slow.quotes.release.notify_one();
        let fenced = first.await??;
        ensure!(fenced.status() == StatusCode::CONFLICT);
        ensure!(response_json(fenced).await?["error"]["code"] == "idempotency_key_in_use");
        ensure!(slow.quotes(&database).await? == 1);
        let replayed = slow.app.clone().oneshot(slow.create()?).await?;
        ensure!(replayed.headers()["idempotent-replayed"] == "true");
        ensure!(response_json(replayed).await?["id"] == created["id"]);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// A repeat that arrives past the takeover interval while the first request is inside its
/// transaction waits for the key's row lock, and finds the response the first request saved:
/// one quote, and the repeat replays it.
#[tokio::test]
async fn a_repeat_during_the_transaction_waits_and_replays_it() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let slow = SlowQuote::new(&database).await?;
        let gate = InsertGate::close(&database).await?;
        let first = slow.start().await?;
        slow.age_key(&database).await?;
        slow.quotes.release.notify_one();
        wait_for_lock_wait(&database, &["advisory"]).await?;

        let repeat = tokio::spawn(slow.app.clone().oneshot(slow.create()?));
        wait_for_lock_wait(&database, &["transactionid", "tuple"]).await?;
        gate.release().await?;
        let first = first.await??;
        ensure!(first.status() == StatusCode::OK);
        ensure!(first.headers().get("idempotent-replayed").is_none());
        let created = response_json(first).await?;
        let repeat = repeat.await??;
        ensure!(repeat.status() == StatusCode::OK);
        ensure!(repeat.headers()["idempotent-replayed"] == "true");
        ensure!(response_json(repeat).await? == created);
        ensure!(slow.quotes(&database).await? == 1);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// A request dropped inside its transaction, as when its client disconnects, commits nothing: a
/// repeat past the takeover interval waits for the transaction to roll back, takes the key over,
/// and creates the one quote.
#[tokio::test]
async fn a_request_dropped_in_its_transaction_commits_nothing() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let slow = SlowQuote::new(&database).await?;
        let gate = InsertGate::close(&database).await?;
        let first = slow.start().await?;
        slow.age_key(&database).await?;
        slow.quotes.release.notify_one();
        wait_for_lock_wait(&database, &["advisory"]).await?;
        first.abort();
        ensure!(first.await.is_err_and(|error| error.is_cancelled()));

        let repeat = tokio::spawn(slow.app.clone().oneshot(slow.create()?));
        wait_for_lock_wait(&database, &["transactionid", "tuple"]).await?;
        gate.release().await?;
        let repeat = repeat.await??;
        ensure!(repeat.status() == StatusCode::OK);
        ensure!(repeat.headers().get("idempotent-replayed").is_none());
        let created = response_json(repeat).await?;
        ensure!(slow.quotes(&database).await? == 1);
        let replayed = slow.app.clone().oneshot(slow.create()?).await?;
        ensure!(replayed.headers()["idempotent-replayed"] == "true");
        ensure!(response_json(replayed).await?["id"] == created["id"]);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// A transaction PostgreSQL rolled back by a deadlock or a serialization failure (`40P01`,
/// `40001`) changed nothing: the request answers a `503` with `Retry-After` that its key does not
/// save, so a retry with the key runs it.
#[tokio::test]
async fn a_transaction_rolled_back_by_a_conflict_is_an_unsaved_503() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let slow = SlowQuote::new(&database).await?;
        // The first price fetch does not wait.
        slow.quotes.release.notify_one();
        sqlx::query(
            "CREATE FUNCTION conflict_quote_insert() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'conflict' USING ERRCODE = TG_ARGV[0]; END $$",
        )
        .execute(&database.owner_pool)
        .await?;
        for (code, trigger) in [
            (
                "40P01",
                "CREATE TRIGGER conflict_quote_insert BEFORE INSERT ON quotes FOR EACH ROW \
                 EXECUTE FUNCTION conflict_quote_insert('40P01')",
            ),
            (
                "40001",
                "CREATE TRIGGER conflict_quote_insert BEFORE INSERT ON quotes FOR EACH ROW \
                 EXECUTE FUNCTION conflict_quote_insert('40001')",
            ),
        ] {
            sqlx::query(trigger).execute(&database.owner_pool).await?;
            let refused = slow.app.clone().oneshot(slow.create()?).await?;
            ensure!(
                refused.status() == StatusCode::SERVICE_UNAVAILABLE,
                "{code}"
            );
            ensure!(refused.headers().contains_key("retry-after"), "{code}");
            ensure!(response_json(refused).await?["error"]["code"] == "unavailable");
            sqlx::query("DROP TRIGGER conflict_quote_insert ON quotes")
                .execute(&database.owner_pool)
                .await?;
            let saved: i64 =
                sqlx::query_scalar("SELECT count(*) FROM idempotency_keys WHERE key = 'slow'")
                    .fetch_one(&database.app_pool)
                    .await?;
            ensure!(saved == 0, "{code}: the key was released");
        }
        let retried = slow.app.clone().oneshot(slow.create()?).await?;
        ensure!(retried.status() == StatusCode::OK);
        ensure!(retried.headers().get("idempotent-replayed").is_none());
        ensure!(slow.quotes(&database).await? == 1);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// A repeat that waits for its key's row past `lock_timeout` (`55P03`), behind a transaction
/// holding it, answers `409 idempotency_key_in_use` and saves nothing: a later repeat replays.
#[tokio::test]
async fn a_repeat_waiting_past_the_lock_timeout_is_a_409() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let slow = SlowQuote::new(&database).await?;
        // The first price fetch does not wait.
        slow.quotes.release.notify_one();
        let first = slow.app.clone().oneshot(slow.create()?).await?;
        ensure!(first.status() == StatusCode::OK);
        let created = response_json(first).await?;
        let mut holder = database.owner_pool.begin().await?;
        sqlx::query("SELECT 1 FROM idempotency_keys WHERE key = 'slow' FOR UPDATE")
            .execute(&mut *holder)
            .await?;
        let repeat = slow.app.clone().oneshot(slow.create()?).await?;
        ensure!(repeat.status() == StatusCode::CONFLICT);
        ensure!(response_json(repeat).await?["error"]["code"] == "idempotency_key_in_use");
        holder.rollback().await?;
        let replayed = slow.app.clone().oneshot(slow.create()?).await?;
        ensure!(replayed.headers()["idempotent-replayed"] == "true");
        ensure!(response_json(replayed).await?["id"] == created["id"]);
        ensure!(slow.quotes(&database).await? == 1);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn quotes_api_is_idempotent_rate_limited_paused_tenant_safe_and_emits_eip681() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[43; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        let (other, other_key) = seed_product(&database.app_pool, "builder").await?;
        let account = seed_account(&database.app_pool, product.id, "account-rl").await?;
        let other_account = seed_account(&database.app_pool, other.id, "account-rl").await?;
        let route = test_route();
        limit_quote_rate(&database.app_pool, product.id, 1).await?;
        let app = topup::api::router(AppState {
            pool: database.app_pool.clone(),
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
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(FixedQuote),
            client_reads: Arc::default(),
            rate_limits: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        })
        .0;
        let now = Utc::now().timestamp();
        let quote_body = |account: &str, amount: u64| {
            serde_json::to_vec(&json!({
                "client_reference_id": account, "amount": amount, "currency": "usd",
                "chain_id": 1, "asset": "pha"
            }))
        };
        let create = |key: &str, body: Vec<u8>| {
            merchant_request_with_key(Method::POST, "/v1/quotes", body, &product_key, key)
        };

        let created = app
            .clone()
            .oneshot(create("key-1", quote_body("account-rl", 100)?))
            .await?;
        ensure!(created.status() == StatusCode::OK);
        ensure!(created.headers()["cache-control"] == "no-store");
        let created = response_json(created).await?;
        let quote_id = created["id"].as_str().context("id")?.to_owned();
        ensure!(quote_id.starts_with("qt_") && quote_id.len() == 35);
        ensure!(created["object"] == "quote" && created["status"] == "open");
        ensure!(created["client_reference_id"] == "account-rl" && created["amount"] == 100);
        ensure!(created["currency"] == "usd" && created["asset"] == "pha");
        ensure!(created["amount_atomic"] == "100" && created["exchange_rate"] == "1.00000000");
        ensure!(created["expires_at"].as_i64().context("expires_at")? > now);
        ensure!(created["payment"].is_null() && created["deposit"].is_null());
        ensure!(
            created["payment_uri"]
                == format!(
                    "ethereum:{:#x}@1/transfer?address={}&uint256=100",
                    route.asset.contract,
                    created["address"].as_str().context("address")?
                )
        );
        // The address salt's reference is the quote id.
        let salt = topup_core::address::quote_salt(&product.public_id, "account-rl", &quote_id);
        let expected = topup_core::address::forwarder_address(
            route.chain.contracts.forwarder_factory,
            route.chain.contracts.implementation,
            seed::FIXTURE_TREASURY,
            salt,
        );
        ensure!(created["address"] == format!("{expected:#x}"));

        let first_secret = created["client_secret"]
            .as_str()
            .context("client_secret")?
            .to_owned();
        // `{quote_id}_secret_`, then a 16-byte nonce (with its owner tag) and a 16-byte tag in hex.
        let random = first_secret
            .strip_prefix(&format!("{quote_id}_secret_"))
            .context("client_secret names its quote")?;
        ensure!(random.len() == 64 && random.bytes().all(|byte| byte.is_ascii_hexdigit()));

        // A retry with the same key and request replays the first response, secret included.
        let retried = app
            .clone()
            .oneshot(create("key-1", quote_body("account-rl", 100)?))
            .await?;
        ensure!(retried.status() == StatusCode::OK);
        ensure!(retried.headers()["idempotent-replayed"] == "true");
        ensure!(retried.headers()["cache-control"] == "no-store");
        let retried = response_json(retried).await?;
        ensure!(retried == created, "{retried}");
        let client_secret = first_secret.as_str();

        // The payer's browser reads the public view with the secret alone, from any origin.
        let public = app
            .clone()
            .oneshot(client_read(&quote_id, client_secret)?)
            .await?;
        ensure!(public.status() == StatusCode::OK);
        ensure!(public.headers()["access-control-allow-origin"] == "*");
        ensure!(public.headers()["cache-control"] == "no-store");
        let public = response_json(public).await?;
        ensure!(
            public
                == json!({
                    "id": quote_id, "object": "quote", "livemode": true, "status": "open",
                    "amount": 100,
                    "currency": "usd", "asset": "pha", "decimals": route.asset.decimals,
                    "chain_id": 1, "amount_atomic": "100", "address": created["address"],
                    "payment_uri": created["payment_uri"], "expires_at": created["expires_at"],
                    "cancel_requested_at": null,
                    "payment_status": "none", "confirmations": null,
                    "amount_credited": null, "typical_credit_seconds": 900,
                }),
            "{public}"
        );
        // Signed reads never return the secret; unsigned reads need the quote's own secret.
        let signed = app
            .clone()
            .oneshot(merchant_request(
                Method::GET,
                &format!("/v1/quotes/{quote_id}"),
                Vec::new(),
                &product_key,
            ))
            .await?;
        ensure!(signed.status() == StatusCode::OK);
        let signed = response_json(signed).await?;
        ensure!(signed["client_reference_id"] == "account-rl" && signed["client_secret"].is_null());
        // The list pages the account's quotes newest first and filters by customer and status.
        let listed = app
            .clone()
            .oneshot(merchant_request(
                Method::GET,
                "/v1/quotes?client_reference_id=account-rl&status=open",
                Vec::new(),
                &product_key,
            ))
            .await?;
        ensure!(listed.status() == StatusCode::OK);
        let listed = response_json(listed).await?;
        ensure!(listed["object"] == "list" && listed["url"] == "/v1/quotes");
        ensure!(listed["data"][0]["id"] == quote_id.as_str(), "{listed}");
        ensure!(listed["data"][0]["client_secret"].is_null());
        for query in ["client_reference_id=nobody", "status=expired"] {
            let other = app
                .clone()
                .oneshot(merchant_request(
                    Method::GET,
                    &format!("/v1/quotes?{query}"),
                    Vec::new(),
                    &product_key,
                ))
                .await?;
            ensure!(response_json(other).await?["data"] == json!([]), "{query}");
        }
        let other_quote = format!("qt_{}", Uuid::new_v4().simple());
        for (path, secret) in [
            (other_quote.as_str(), client_secret),
            (
                quote_id.as_str(),
                &format!("{quote_id}_secret_{}", "0".repeat(64)),
            ),
        ] {
            let refused = app.clone().oneshot(client_read(path, secret)?).await?;
            ensure!(refused.status() == StatusCode::NOT_FOUND);
            ensure!(refused.headers()["access-control-allow-origin"] == "*");
            ensure!(response_json(refused).await?["error"]["code"] == "resource_missing");
        }
        let unsigned = app
            .clone()
            .oneshot(
                axum::http::Request::get(format!("/v1/quotes/{quote_id}")).body(Body::empty())?,
            )
            .await?;
        ensure!(unsigned.status() == StatusCode::UNAUTHORIZED);
        let lock_count: i64 = sqlx::query_scalar("SELECT count(*) FROM quotes")
            .fetch_one(&database.app_pool)
            .await?;
        ensure!(lock_count == 1);

        let mismatched = app
            .clone()
            .oneshot(create("key-1", quote_body("account-rl", 200)?))
            .await?;
        ensure!(mismatched.status() == StatusCode::BAD_REQUEST);
        let mismatched = response_json(mismatched).await?;
        ensure!(mismatched["error"]["type"] == "idempotency_error");
        ensure!(mismatched["error"]["code"] == "idempotency_key_reused");

        let limited = app
            .clone()
            .oneshot(create("key-2", quote_body("account-rl", 100)?))
            .await?;
        ensure!(limited.status() == StatusCode::TOO_MANY_REQUESTS);
        // Retryable once the customer's creation of the last minute leaves it.
        let retry_after: u64 = limited
            .headers()
            .get("retry-after")
            .context("Retry-After")?
            .to_str()?
            .parse()?;
        ensure!((1..=60).contains(&retry_after), "{retry_after}");
        ensure!(response_json(limited).await?["error"]["code"] == "customer_rate_limit");

        // Invalid parameters name the parameter.
        for (body, code, param) in [
            (quote_body("account-rl", 0)?, "amount_too_small", "amount"),
            (
                serde_json::to_vec(
                    &json!({"client_reference_id": "a", "amount": 1, "currency": "eur",
                    "chain_id": 1, "asset": "pha"}),
                )?,
                "parameter_invalid",
                "currency",
            ),
            (
                serde_json::to_vec(
                    &json!({"client_reference_id": "a", "amount": 1, "currency": "usd",
                    "chain_id": 1, "asset": "usdc"}),
                )?,
                "parameter_invalid",
                "asset",
            ),
            (
                serde_json::to_vec(&json!({"client_reference_id": "a", "currency": "usd",
                    "chain_id": 1, "asset": "pha"}))?,
                "parameter_missing",
                "amount",
            ),
            (
                serde_json::to_vec(
                    &json!({"client_reference_id": "a", "amount": 1, "currency": "usd",
                    "chain_id": 1, "asset": "pha", "product_lock_ref": "x"}),
                )?,
                "parameter_unknown",
                "product_lock_ref",
            ),
        ] {
            let answer = app
                .clone()
                .oneshot(create(&format!("key-bad-{param}-{code}"), body))
                .await?;
            ensure!(answer.status() == StatusCode::BAD_REQUEST, "{code}");
            let answer = response_json(answer).await?;
            ensure!(
                answer["error"]["type"] == "invalid_request_error",
                "{answer}"
            );
            ensure!(answer["error"]["code"] == code, "{answer}");
            ensure!(answer["error"]["param"] == param, "{answer}");
        }

        // A quote for an account the service has not seen creates it, like a checkout session.
        let implicit = app
            .clone()
            .oneshot(create("key-implicit", quote_body("implicit-rl", 100)?))
            .await?;
        ensure!(implicit.status() == StatusCode::OK);
        ensure!(response_json(implicit).await?["client_reference_id"] == "implicit-rl");
        let implicit_accounts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM customers WHERE account_id = $1 \
             AND client_reference_id = 'implicit-rl'",
        )
        .bind(product.id)
        .fetch_one(&database.app_pool)
        .await?;
        ensure!(implicit_accounts == 1);

        // Another account sees neither the quote nor its key.
        let path = format!("/v1/quotes/{quote_id}");
        let cross_tenant = app
            .clone()
            .oneshot(merchant_request(Method::GET, &path, Vec::new(), &other_key))
            .await?;
        ensure!(cross_tenant.status() == StatusCode::NOT_FOUND);
        ensure!(response_json(cross_tenant).await?["error"]["code"] == "resource_missing");
        let cross_tenant_cancel = app
            .clone()
            .oneshot(merchant_request(
                Method::POST,
                &format!("{path}/cancel"),
                Vec::new(),
                &other_key,
            ))
            .await?;
        ensure!(cross_tenant_cancel.status() == StatusCode::NOT_FOUND);
        ensure!(quote_status(&database.app_pool, &quote_id).await? == "open");
        let other_locks: i64 =
            sqlx::query_scalar("SELECT count(*) FROM quotes WHERE customer_id = $1")
                .bind(other_account.id)
                .fetch_one(&database.app_pool)
                .await?;
        ensure!(other_locks == 0);

        seed::set_customer_paused_scopes(&database.app_pool, account.id, &["quotes".to_owned()])
            .await?;
        let paused = app
            .clone()
            .oneshot(create("key-3", quote_body("account-rl", 100)?))
            .await?;
        ensure!(paused.status() == StatusCode::BAD_REQUEST);
        ensure!(response_json(paused).await?["error"]["code"] == "paused");
        // A repeat creates nothing, so the pause does not hide the quote the product showed.
        let paused_replay = app
            .clone()
            .oneshot(create("key-1", quote_body("account-rl", 100)?))
            .await?;
        ensure!(paused_replay.status() == StatusCode::OK);
        ensure!(response_json(paused_replay).await?["address"] == created["address"]);
        let paused_mismatch = app
            .clone()
            .oneshot(create("key-1", quote_body("account-rl", 200)?))
            .await?;
        ensure!(paused_mismatch.status() == StatusCode::BAD_REQUEST);

        seed::set_customer_paused_scopes(&database.app_pool, account.id, &[]).await?;
        seed::set_account_paused_scopes(&database.app_pool, product.id, &["quotes".to_owned()])
            .await?;
        let product_paused = app
            .clone()
            .oneshot(create("key-4", quote_body("account-rl", 100)?))
            .await?;
        ensure!(product_paused.status() == StatusCode::BAD_REQUEST);

        seed::set_account_paused_scopes(&database.app_pool, product.id, &[]).await?;
        sqlx::query("INSERT INTO route_pauses (route, paused_scopes) VALUES ($1, ARRAY['quotes'])")
            .bind(&route.route)
            .execute(&database.app_pool)
            .await?;
        let route_paused = app
            .clone()
            .oneshot(create("key-5", quote_body("account-rl", 100)?))
            .await?;
        ensure!(route_paused.status() == StatusCode::BAD_REQUEST);

        let get = app
            .clone()
            .oneshot(merchant_request(
                Method::GET,
                &path,
                Vec::new(),
                &product_key,
            ))
            .await?;
        ensure!(get.status() == StatusCode::OK);
        ensure!(response_json(get).await?["status"] == "open");

        let cancel = |id: &str| {
            merchant_request(
                Method::POST,
                &format!("/v1/quotes/{id}/cancel"),
                Vec::new(),
                &product_key,
            )
        };
        let canceled = app.clone().oneshot(cancel(&quote_id)).await?;
        ensure!(canceled.status() == StatusCode::OK);
        let cancel_response = response_json(canceled).await?;
        ensure!(
            cancel_response["status"] == "open"
                && cancel_response["cancel_requested_at"].is_number()
        );
        // Completion waits for this address's own complete dual coverage.
        ensure!(
            topup::locks::expire_once(
                &database.app_pool,
                &topup::routes::RouteSet::new(vec![route.clone()]).unwrap()
            )
            .await?
                == 0
        );
        finalize_chain_past_now(&database.app_pool).await?;
        ensure!(
            topup::locks::expire_once(
                &database.app_pool,
                &topup::routes::RouteSet::new(vec![route.clone()]).unwrap()
            )
            .await?
                == 1
        );
        // Canceling again returns the canceled quote.
        let again = app.clone().oneshot(cancel(&quote_id)).await?;
        ensure!(again.status() == StatusCode::OK);
        ensure!(response_json(again).await?["status"] == "canceled");
        let audit_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit WHERE action = 'quote.cancel'")
                .fetch_one(&database.app_pool)
                .await?;
        ensure!(audit_count == 1);
        ensure!(quote_status(&database.app_pool, &quote_id).await? == "cancelled");

        // A quote whose single-use address already received funds is no longer unpaid.
        sqlx::query("DELETE FROM route_pauses WHERE route = $1")
            .bind(&route.route)
            .execute(&database.app_pool)
            .await?;
        sqlx::query("UPDATE quotes SET created_at = now() - interval '2 minutes'")
            .execute(&database.app_pool)
            .await?;
        let paid = app
            .clone()
            .oneshot(create("key-6", quote_body("account-rl", 100)?))
            .await?;
        ensure!(paid.status() == StatusCode::OK);
        let paid_id = response_json(paid).await?["id"]
            .as_str()
            .context("id")?
            .to_owned();
        let paid_address_id = quote_address_id(&database.app_pool, &paid_id).await?;
        insert_rejected_deposit(&database.app_pool, &route, paid_address_id).await?;
        let refused = app.clone().oneshot(cancel(&paid_id)).await?;
        ensure!(refused.status() == StatusCode::BAD_REQUEST);
        ensure!(response_json(refused).await?["error"]["code"] == "quote_payment_received");
        ensure!(quote_status(&database.app_pool, &paid_id).await? == "open");

        // An unpaid quote whose payment window has closed stays open until chain-time expiry,
        // and can no longer be canceled.
        sqlx::query("UPDATE quotes SET created_at = now() - interval '2 minutes'")
            .execute(&database.app_pool)
            .await?;
        let lapsed = app
            .clone()
            .oneshot(create("key-7", quote_body("account-rl", 100)?))
            .await?;
        ensure!(lapsed.status() == StatusCode::OK);
        let lapsed_id = response_json(lapsed).await?["id"]
            .as_str()
            .context("id")?
            .to_owned();
        sqlx::query("UPDATE quotes SET expires_at = now() - interval '1 second' WHERE id = $1")
            .bind(topup::ids::parse(topup::ids::QUOTE, &lapsed_id).context("quote id")?)
            .execute(&database.app_pool)
            .await?;
        let window_closed = app.clone().oneshot(cancel(&lapsed_id)).await?;
        ensure!(window_closed.status() == StatusCode::BAD_REQUEST);
        ensure!(response_json(window_closed).await?["error"]["code"] == "quote_window_closed");
        ensure!(quote_status(&database.app_pool, &lapsed_id).await? == "open");

        let read_config = || {
            app.clone().oneshot(merchant_request(
                Method::GET,
                "/v1/config",
                Vec::new(),
                &product_key,
            ))
        };
        let config = read_config().await?;
        ensure!(config.status() == StatusCode::OK);
        let config = response_json(config).await?;
        ensure!(config["object"] == "config" && config["currency"] == "usd");
        // The live defaults, until the operator sets the account's caps.
        ensure!(config["max_open_quotes"] == 1_000, "{config}");
        ensure!(config["max_open_amount_per_account"] == 5_000_000);
        ensure!(config["max_open_amount_per_customer"] == 500_000);
        set_limits(
            &database.app_pool,
            product.id,
            false,
            Some(7),
            Some(700),
            Some(70),
        )
        .await?;
        let unchanged = response_json(read_config().await?).await?;
        ensure!(
            unchanged["max_open_amount_per_account"] == 5_000_000,
            "{unchanged}"
        );
        set_limits(
            &database.app_pool,
            product.id,
            true,
            Some(3),
            Some(300),
            None,
        )
        .await?;
        let adjusted = response_json(read_config().await?).await?;
        ensure!(adjusted["max_open_quotes"] == 3, "{adjusted}");
        ensure!(adjusted["max_open_amount_per_account"] == 300);
        ensure!(adjusted["max_open_amount_per_customer"] == 500_000);
        ensure!(
            config["assets"].as_array().map(Vec::len) == Some(1),
            "{config}"
        );
        let asset = &config["assets"][0];
        ensure!(asset["chain_id"] == 1 && asset["asset"] == "pha" && asset["decimals"] == 2);
        ensure!(asset["contract"] == format!("{:#x}", route.asset.contract));
        ensure!(asset["min_amount"] == 1 && asset["quote_ttl_seconds"] == 900);
        ensure!(asset["quote_spread_bps"] == 0 && asset["quote_tolerance_bps"] == 100);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn client_secret_reads_are_limited_per_object_and_forgeries_cost_nothing() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let admin_key = SigningKey::from_bytes(&[46; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        seed_account(&database.app_pool, product.id, "flooded").await?;
        let app = topup::api::router(AppState {
            pool: database.app_pool.clone(),
            routes: Arc::new(
                topup::routes::RouteSet::new(vec![test_route()]).map_err(anyhow::Error::msg)?,
            ),
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(
                ADMIN_KID.to_owned(),
                &public_key_base64(&admin_key),
            )
            .map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(FixedQuote),
            // The budget's clock stands still, so no read refills it however slowly the
            // machine answers the flood.
            client_reads: Arc::new(ManualClock::new().client_read_limiter()),
            rate_limits: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        })
        .0;
        let create = |key: &str| -> Result<_> {
            let body = serde_json::to_vec(&json!({
                "client_reference_id": "flooded", "amount": 100, "currency": "usd",
                "chain_id": 1, "asset": "pha"
            }))?;
            Ok(merchant_request_with_key(
                Method::POST,
                "/v1/quotes",
                body,
                &product_key,
                key,
            ))
        };
        let mut quotes = Vec::new();
        for key in ["flood-1", "flood-2"] {
            let created = response_json(app.clone().oneshot(create(key)?).await?).await?;
            let id = created["id"].as_str().context("id")?.to_owned();
            let secret = created["client_secret"]
                .as_str()
                .context("secret")?
                .to_owned();
            quotes.push((id, secret));
        }
        let read = |id: &str, secret: &str| {
            let app = app.clone();
            let request = client_read(id, secret);
            async move { Ok::<_, anyhow::Error>(app.oneshot(request?).await?) }
        };
        let (flooded, genuine) = (&quotes[0].0, &quotes[0].1);

        // A new quote's secret reads at once.
        ensure!(read(flooded, genuine).await?.status() == StatusCode::OK);

        // Forged secrets, well formed or not, for the quote and for made-up ids are refused
        // before the database and charge no budget: the genuine reader keeps all of its reads.
        let forgeries = (0..2_000_u32).map(|index| {
            let id = if index % 2 == 0 {
                flooded.clone()
            } else {
                format!("qt_{}", Uuid::new_v4().simple())
            };
            let secret = match index % 3 {
                0 => format!("{id}_secret_{:064x}", index),
                // The genuine secret with its last tag digit changed, or moved to another id.
                1 if id == *flooded => format!(
                    "{}{}",
                    &genuine[..genuine.len() - 1],
                    if genuine.ends_with('0') { '1' } else { '0' }
                ),
                1 => genuine.replacen(flooded.as_str(), &id, 1),
                _ => format!("{id}_secret_x"),
            };
            (id, secret)
        });
        for (id, secret) in forgeries {
            let refused = read(&id, &secret).await?;
            ensure!(refused.status() == StatusCode::NOT_FOUND, "{id} {secret}");
        }

        // A flood with the genuine secret is limited to its quote's budget of 120 a minute (one
        // read spent above), retryable once its next read refills at two a second, and does not
        // starve another quote.
        for _ in 1..120 {
            ensure!(read(flooded, genuine).await?.status() == StatusCode::OK);
        }
        let limited = read(flooded, genuine).await?;
        ensure!(limited.status() == StatusCode::TOO_MANY_REQUESTS);
        let retry_after: u64 = limited.headers()["retry-after"].to_str()?.parse()?;
        ensure!(retry_after == 1, "{retry_after}");
        ensure!(limited.headers()["access-control-allow-origin"] == "*");
        ensure!(response_json(limited).await?["error"]["code"] == "rate_limit");
        ensure!(read(&quotes[1].0, &quotes[1].1).await?.status() == StatusCode::OK);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn usd_stated_amount_rounds_token_amount_up() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        struct ThreeDollarQuote;

        #[async_trait]
        impl QuoteProvider for ThreeDollarQuote {
            async fn quote(
                &self,
                _route: &RouteFile,
                _deadline: tokio::time::Instant,
            ) -> Result<ValidatedQuote, Value> {
                Ok(ValidatedQuote {
                    price: ScaledPrice::new(300_000_000, PRICE_SCALE).expect("fixed quote"),
                    evidence: json!({"mode": "test"}),
                })
            }
        }

        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let account = seed_account(&database.app_pool, product.id, "round-up").await?;
        // A cent at $3 a token is 33⅓ ten-thousandths of one: the quote rounds up to 34.
        let mut route = test_route();
        route.asset.decimals = 4;
        route.asset.quote_amount_decimals = 4;
        let quotes: Arc<dyn QuoteProvider> = Arc::new(ThreeDollarQuote);
        let (lock, _) = locks::create(
            &database.app_pool,
            &quotes,
            &client_secret_key(),
            &product,
            &account,
            &route,
            MinorAmount::new(1),
            &Default::default(),
        )
        .await?;
        ensure!(lock.amount_atomic.value() == U256::from(34_u64));
        ensure!(lock.credit_minor.value() == 1);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn quoted_amount_rounds_up_to_the_routes_amount_decimals() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        struct CentsPriceQuote;

        #[async_trait]
        impl QuoteProvider for CentsPriceQuote {
            async fn quote(
                &self,
                _route: &RouteFile,
                _deadline: tokio::time::Instant,
            ) -> Result<ValidatedQuote, Value> {
                Ok(ValidatedQuote {
                    // $0.0365 per token.
                    price: ScaledPrice::new(3_650_000, PRICE_SCALE).expect("fixed quote"),
                    evidence: json!({"mode": "test"}),
                })
            }
        }

        let admin_key = SigningKey::from_bytes(&[45; 32]);
        let (product, product_key) = seed_product(&database.app_pool, "phala-cloud").await?;
        seed_account(&database.app_pool, product.id, "short-amount").await?;
        let mut route = test_route();
        route.asset.decimals = 18;
        route.asset.quote_amount_decimals = 4;
        route.merchant.max_deposit_atomic =
            topup_core::route::Bounded::at(AtomicAmount::new(U256::MAX));
        let app = topup::api::router(AppState {
            pool: database.app_pool.clone(),
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
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(CentsPriceQuote),
            client_reads: Arc::default(),
            rate_limits: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        })
        .0;
        let body = serde_json::to_vec(&json!({
            "client_reference_id": "short-amount", "amount": 1_000, "currency": "usd",
            "chain_id": 1, "asset": "pha"
        }))?;
        let response = app
            .oneshot(merchant_request_with_key(
                Method::POST,
                "/v1/quotes",
                body,
                &product_key,
                "short-amount-1",
            ))
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let created = response_json(response).await?;
        // $10.00 at $0.0365 is 273.972602739726027398 PHA at the minimal amount; the quote asks
        // for 273.9727, and the credit stays exactly $10.00.
        ensure!(created["amount"] == 1_000);
        ensure!(created["amount_atomic"] == "273972700000000000000");
        ensure!(
            created["payment_uri"]
                == format!(
                    "ethereum:{:#x}@1/transfer?address={}&uint256=273972700000000000000",
                    route.asset.contract,
                    created["address"].as_str().context("address")?
                )
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn caps_are_per_account_and_mode_and_expiry_releases_them() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let first = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let second = seed_product_without_key(&database.app_pool, "builder").await?;
        seed::set_treasury(
            &database.app_pool,
            first.id,
            false,
            1,
            seed::FIXTURE_TREASURY,
        )
        .await?;
        let first_customer = seed_account(&database.app_pool, first.id, "first").await?;
        let second_customer = seed_account(&database.app_pool, first.id, "second").await?;
        let other_customer = seed_account(&database.app_pool, second.id, "other").await?;
        let test_customer = seed::create_customer(
            &database.app_pool,
            &NewCustomer {
                id: Uuid::new_v4(),
                account_id: first.id,
                livemode: false,
                client_reference_id: "tester".to_owned(),
                paused_scopes: Vec::new(),
            },
        )
        .await?;
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let route = test_route();
        let mut test_mode_route = test_route();
        test_mode_route.livemode = false;
        seed::configure_payments(
            &database.app_pool,
            first.id,
            false,
            &topup::payment_config::Document {
                quote_creations_per_customer_per_minute: Some(
                    topup::payment_config::MAX_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE,
                ),
                ..topup::payment_config::Document::accepting([&test_mode_route])
            },
        )
        .await?;
        let create = |customer: &Customer, route: &RouteFile, credit: u64| {
            let (pool, quotes, first, customer, route) = (
                database.app_pool.clone(),
                Arc::clone(&quotes),
                first.clone(),
                customer.clone(),
                route.clone(),
            );
            async move {
                let account = if customer.account_id == first.id {
                    first
                } else {
                    seed_account_row(&pool, customer.account_id).await?
                };
                Ok::<_, anyhow::Error>(
                    locks::create(
                        &pool,
                        &quotes,
                        &client_secret_key(),
                        &account,
                        &customer,
                        &route,
                        MinorAmount::new(credit),
                        &Default::default(),
                    )
                    .await,
                )
            }
        };

        // One customer's open credit is capped.
        set_limits(
            &database.app_pool,
            first.id,
            true,
            None,
            Some(1_000),
            Some(100),
        )
        .await?;
        let (customer_lock, _) = create(&first_customer, &route, 100).await??;
        ensure!(matches!(
            create(&first_customer, &route, 1).await?,
            Err(RateLockError::ExposureCap {
                scope: "customer",
                ..
            })
        ));
        cancel_lock(&database.app_pool, &first, customer_lock.id).await?;
        finalize_chain_past_now(&database.app_pool).await?;
        ensure!(locks::expire_once(&database.app_pool, &test_routes()).await? == 1);

        // The account's open credit is capped per mode, and a test quote never uses live
        // headroom: the test cap fills first, and the live cap then still admits its own.
        set_limits(
            &database.app_pool,
            first.id,
            true,
            None,
            Some(100),
            Some(1_000),
        )
        .await?;
        set_limits(&database.app_pool, first.id, false, None, Some(100), None).await?;
        create(&test_customer, &test_mode_route, 100).await??;
        let (expiring, _) = create(&first_customer, &route, 100).await??;
        for (customer, route) in [
            (&second_customer, &route),
            (&test_customer, &test_mode_route),
        ] {
            ensure!(matches!(
                create(customer, route, 1).await?,
                Err(RateLockError::ExposureCap {
                    scope: "account",
                    ..
                })
            ));
        }
        // No cap spans accounts.
        create(&other_customer, &route, 1).await??;
        // Nor is the number of open quotes shared: the other account's cap is its own.
        set_limits(&database.app_pool, second.id, true, Some(1), None, None).await?;
        ensure!(matches!(
            create(&other_customer, &route, 1).await?,
            Err(RateLockError::QuoteCountCap(1))
        ));

        sqlx::query("UPDATE quotes SET expires_at = now() - interval '1 second' WHERE id = $1")
            .bind(expiring.id)
            .execute(&database.app_pool)
            .await?;
        finalize_chain_past_now(&database.app_pool).await?;
        ensure!(locks::expire_once(&database.app_pool, &test_routes()).await? == 1);
        let event = sqlx::query(
            "SELECT id, account_id, object_type, object_id FROM events \
             WHERE type = 'quote.expired'",
        )
        .fetch_one(&database.app_pool)
        .await?;
        ensure!(
            event.try_get::<Uuid, _>("id")?
                == topup_core::identity::event_id("quote.expired", expiring.id)
        );
        ensure!(event.try_get::<Uuid, _>("account_id")? == first.id);
        ensure!(
            event
                .try_get::<Option<String>, _>("object_type")?
                .as_deref()
                == Some("quote")
        );
        ensure!(event.try_get::<Option<Uuid>, _>("object_id")? == Some(expiring.id));
        // The expiry released the live headroom.
        create(&second_customer, &route, 100).await??;
        let customer_status: String = sqlx::query_scalar("SELECT status FROM quotes WHERE id = $1")
            .bind(customer_lock.id)
            .fetch_one(&database.app_pool)
            .await?;
        ensure!(customer_status == "cancelled");
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn unpaid_lock_expires_only_once_the_finalized_chain_passes_its_window() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let account = seed_account(&database.app_pool, product.id, "unpaid").await?;
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let lock = create_lock(&database, &quotes, &product, &account, &test_route()).await?;
        let expires_at = Utc::now() - chrono::Duration::minutes(5);
        sqlx::query("UPDATE quotes SET expires_at = $2 WHERE id = $1")
            .bind(lock.id)
            .bind(expires_at)
            .execute(&database.app_pool)
            .await?;
        let account_key = format!("account:{}", account.id);

        // The wall-clock window has closed, but the scanner has not committed a finalized block
        // past it (never, then stalled at the deadline itself): the lock stays open and reserved.
        ensure!(locks::expire_once(&database.app_pool, &test_routes()).await? == 0);
        set_finalized_time(&database.app_pool, expires_at).await?;
        ensure!(locks::expire_once(&database.app_pool, &test_routes()).await? == 0);
        ensure!(lock_status(&database.app_pool, lock.id).await? == "open");
        // Connected, and the probe prepared, up front: the probe right after the refusal is then
        // a single round trip, which reaches the server before a dropped transaction's queued
        // rollback would.
        let mut probe = sqlx::PgConnection::connect(&database.app_url).await?;
        ensure!(quote_unlocked(&mut probe, lock.id).await?);
        ensure!(matches!(
            cancel_lock(&database.app_pool, &product, lock.id).await,
            Err(RateLockError::WindowClosed)
        ));
        // The refusal left the quote unlocked for the expiry below, which skips locked quotes.
        ensure!(
            quote_unlocked(&mut probe, lock.id).await?,
            "a refused cancel left the quote locked"
        );
        ensure!(exposure(&database.app_pool, &account_key).await? == 100);
        let events: i64 =
            sqlx::query_scalar("SELECT count(*) FROM events WHERE type = 'quote.expired'")
                .fetch_one(&database.app_pool)
                .await?;
        ensure!(events == 0);

        set_finalized_time(
            &database.app_pool,
            expires_at + chrono::Duration::seconds(12),
        )
        .await?;
        ensure!(locks::expire_once(&database.app_pool, &test_routes()).await? == 1);
        ensure!(lock_status(&database.app_pool, lock.id).await? == "expired");
        ensure!(matches!(
            cancel_lock(&database.app_pool, &product, lock.id).await,
            Err(RateLockError::NotOpen(_))
        ));
        ensure!(exposure(&database.app_pool, &account_key).await? == 0);
        let events: i64 =
            sqlx::query_scalar("SELECT count(*) FROM events WHERE type = 'quote.expired'")
                .fetch_one(&database.app_pool)
                .await?;
        ensure!(events == 1);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn another_chains_cursor_never_expires_a_lock() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let account = seed_account(&database.app_pool, product.id, "cross-chain").await?;
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let lock = create_lock(&database, &quotes, &product, &account, &test_route()).await?;
        let expires_at = Utc::now() - chrono::Duration::minutes(5);
        sqlx::query("UPDATE quotes SET expires_at = $2 WHERE id = $1")
            .bind(lock.id)
            .bind(expires_at)
            .execute(&database.app_pool)
            .await?;

        // The lock is on chain 1, whose cursor is still at the deadline; chain 2 is far past it.
        set_finalized_time(&database.app_pool, expires_at).await?;
        sqlx::query(
            "INSERT INTO cursors (chain_id, scanned_block, scanned_block_time) VALUES (2, 0, $1)",
        )
        .bind(Utc::now())
        .execute(&database.app_pool)
        .await?;
        ensure!(locks::expire_once(&database.app_pool, &test_routes()).await? == 0);
        ensure!(lock_status(&database.app_pool, lock.id).await? == "open");
        ensure!(exposure(&database.app_pool, &format!("account:{}", account.id)).await? == 100);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn new_lock_addresses_start_scanning_at_the_chain_cursor() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let account = seed_account(&database.app_pool, product.id, "cursor").await?;
        let route = test_route();
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let before_cursor = create_lock(&database, &quotes, &product, &account, &route).await?;
        sqlx::query("UPDATE chain_coverage SET through_block=1234 WHERE chain_id=1")
            .execute(&database.app_pool)
            .await?;
        let after_cursor = create_lock(&database, &quotes, &product, &account, &route).await?;
        for (address_id, expected) in [
            (before_cursor.address_id, 0),
            (after_cursor.address_id, 1234),
        ] {
            let (created_block, backfilled): (i64, bool) =
                sqlx::query_as("SELECT created_block, backfilled FROM addresses WHERE id = $1")
                    .bind(address_id)
                    .fetch_one(&database.app_pool)
                    .await?;
            ensure!(created_block == expected);
            ensure!(!backfilled);
        }
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn rate_limited_creation_does_not_fetch_a_price() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        struct CountingQuote(AtomicUsize);

        #[async_trait]
        impl QuoteProvider for CountingQuote {
            async fn quote(
                &self,
                route: &RouteFile,
                _deadline: tokio::time::Instant,
            ) -> Result<ValidatedQuote, Value> {
                self.0.fetch_add(1, Ordering::SeqCst);
                FixedQuote.quote(route, _deadline).await
            }
        }

        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let account = seed_account(&database.app_pool, product.id, "limited").await?;
        let route = test_route();
        limit_quote_rate(&database.app_pool, product.id, 1).await?;
        let counter = Arc::new(CountingQuote(AtomicUsize::new(0)));
        let quotes: Arc<dyn QuoteProvider> = counter.clone();
        create_lock(&database, &quotes, &product, &account, &route).await?;
        ensure!(matches!(
            locks::create(
                &database.app_pool,
                &quotes,
                &client_secret_key(),
                &product,
                &account,
                &route,
                MinorAmount::new(100),
                &Default::default(),
            )
            .await,
            // Retryable once the one creation of the last minute leaves it.
            Err(RateLockError::RateLimited {
                retry_after: 1..=60
            })
        ));
        ensure!(counter.0.load(Ordering::SeqCst) == 1);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn cancel_refuses_a_lock_whose_address_received_any_deposit() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let account = seed_account(&database.app_pool, product.id, "paid").await?;
        let route = test_route();
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let lock = create_lock(&database, &quotes, &product, &account, &route).await?;
        insert_rejected_deposit(&database.app_pool, &route, lock.address_id).await?;
        ensure!(matches!(
            cancel_lock(&database.app_pool, &product, lock.id).await,
            Err(RateLockError::PendingPayment)
        ));
        ensure!(lock_status(&database.app_pool, lock.id).await? == "open");
        ensure!(exposure(&database.app_pool, &format!("account:{}", account.id)).await? == 100);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// The payer's view reports what was credited, not what was quoted: an underpayment valued at
/// spot is credited at its own amount. Its typical credit time is that of the confirmation the
/// account's payments on the chain wait for: Base's default depth 3, then a deeper depth, `safe`,
/// and `finalized` as the account's policy requires more; a shallower depth is refused.
#[tokio::test]
async fn the_client_view_reports_the_credit_of_a_spot_valued_underpayment() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let pool = &database.app_pool;
        let (product, product_key) = seed_product(pool, "phala-cloud").await?;
        seed_account(pool, product.id, "underpaid").await?;
        const BASE: u64 = 8_453;
        seed::initialize_dual_chain(pool, BASE).await?;
        seed::set_treasury(pool, product.id, true, BASE, seed::FIXTURE_TREASURY).await?;
        let mut route = test_route();
        route.chain.chain_id = BASE;
        route.pricing.sequencer_uptime = Some(topup_core::price::Sequencer {
            feed: "BASE_SEQUENCER_UPTIME".into(),
            grace_s: 3600,
        });
        route.chain.confirmations = topup_core::route::ChainFamily::OpStack.default_confirmations();
        seed::accept_routes(pool, product.id, true, &[&route]).await?;
        let admin_key = SigningKey::from_bytes(&[44; 32]);
        let app = topup::api::router(AppState {
            pool: pool.clone(),
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
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(FixedQuote),
            client_reads: Arc::default(),
            rate_limits: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        })
        .0;
        let body = serde_json::to_vec(&json!({
            "client_reference_id": "underpaid", "amount": 100, "currency": "usd",
            "chain_id": BASE, "asset": "pha"
        }))?;
        let created = app
            .clone()
            .oneshot(merchant_request(
                Method::POST,
                "/v1/quotes",
                body,
                &product_key,
            ))
            .await?;
        ensure!(created.status() == StatusCode::OK);
        let created = response_json(created).await?;
        let quote_id = created["id"].as_str().context("id")?;
        let secret = created["client_secret"].as_str().context("client_secret")?;
        let read = || async {
            let response = app.clone().oneshot(client_read(quote_id, secret)?).await?;
            ensure!(response.status() == StatusCode::OK);
            response_json(response).await
        };

        let unpaid = read().await?;
        ensure!(unpaid["amount_credited"].is_null(), "{unpaid}");
        ensure!(unpaid["typical_credit_seconds"] == 7, "{unpaid}");
        let set_confirmations = |confirmations: &'static str| {
            let app = app.clone();
            let product_key = &product_key;
            async move {
                let settings = serde_json::to_vec(&json!({"chains": [{
                    "chain_id": BASE, "confirmations": confirmations, "assets": [{"asset": "pha"}],
                }]}))?;
                let response = app
                    .oneshot(merchant_request(
                        Method::POST,
                        "/v1/payment_settings",
                        settings,
                        product_key,
                    ))
                    .await?;
                anyhow::Ok(response.status())
            }
        };
        // Weaker than the chain's floor is refused; a stricter requirement governs later quotes and
        // payments, while this quote keeps the confirmation it was issued with.
        ensure!(set_confirmations("2").await? == StatusCode::BAD_REQUEST);
        ensure!(set_confirmations("finalized").await? == StatusCode::OK);
        ensure!(read().await?["typical_credit_seconds"] == 7);

        // 40 of the quoted 100 atomic units, valued at spot: 40 cents, not the quote's 100.
        let address_id = quote_address_id(pool, quote_id).await?;
        let inserted = topup::db::insert_deposit(
            pool,
            &topup::db::NewDeposit {
                chain_id: BASE,
                tx_hash: B256::repeat_byte(0x81),
                log_index: 0,
                receipt_log_index: 0,
                tx_from: Address::repeat_byte(0x84),
                tx_nonce: 0,
                is_final: false,
                block_number: 10,
                block_hash: B256::repeat_byte(0x82),
                block_time: Utc::now(),
                address_id,
                route: Some(route.route.clone()),
                route_version: Some(route.version),
                asset_contract: route.asset.contract,
                from_address: Address::repeat_byte(0x84),
                amount_atomic: AtomicAmount::new(U256::from(40_u64)),
                state: DepositState::Detected,
                reason: None,
                next_attempt_at: Utc::now(),
            },
        )
        .await?;
        ensure!(inserted);
        let confirming = read().await?;
        ensure!(
            confirming["payment_status"] == "confirming" && confirming["amount_credited"].is_null(),
            "{confirming}"
        );
        sqlx::query(
            "UPDATE deposits SET state = 'credited', price_source = 'spot', credit_minor = 40, \
             valuation_at = now() WHERE address_id = $1",
        )
        .bind(address_id)
        .execute(pool)
        .await?;
        let credited = read().await?;
        ensure!(
            credited["payment_status"] == "credited"
                && credited["amount"] == 100
                && credited["amount_credited"] == 40,
            "{credited}"
        );
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
#[traced_test]
async fn a_creation_reaching_ninety_percent_of_the_account_cap_raises_an_alert() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let account = seed_account(&database.app_pool, product.id, "exposed").await?;
        let route = test_route();
        set_limits(
            &database.app_pool,
            product.id,
            true,
            None,
            Some(110),
            Some(110),
        )
        .await?;
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        create_lock(&database, &quotes, &product, &account, &route).await?;

        ensure!(logs_contain("TopupLockExposureNearCap"));
        ensure!(logs_contain("tags.scope=\"account\""));
        ensure!(!logs_contain("tags.scope=\"customer\""));
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_creations_never_exceed_the_shared_exposure_cap() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        const ATTEMPTS: usize = 12;
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        set_limits(
            &database.app_pool,
            product.id,
            true,
            None,
            Some(500),
            Some(1_000),
        )
        .await?;
        let route = Arc::new(test_route());
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..ATTEMPTS {
            let account =
                seed_account(&database.app_pool, product.id, &format!("racer-{index}")).await?;
            let (pool, quotes, product, route) = (
                database.app_pool.clone(),
                Arc::clone(&quotes),
                product.clone(),
                Arc::clone(&route),
            );
            tasks.spawn(async move {
                locks::create(
                    &pool,
                    &quotes,
                    &client_secret_key(),
                    &product,
                    &account,
                    &route,
                    MinorAmount::new(100),
                    &Default::default(),
                )
                .await
            });
        }
        let mut successes = 0_u64;
        while let Some(joined) = tasks.join_next().await {
            match joined? {
                Ok(_) => successes += 1,
                Err(RateLockError::ExposureCap {
                    scope: "account", ..
                }) => {}
                Err(error) => anyhow::bail!("unexpected creation failure: {error}"),
            }
        }
        ensure!(successes * 100 <= 500);
        ensure!(successes == 5);
        let open: String = sqlx::query_scalar(
            "SELECT coalesce(sum(credit_minor), 0)::text FROM quotes WHERE status = 'open'",
        )
        .fetch_one(&database.app_pool)
        .await?;
        let open = open.parse::<u64>()?;
        ensure!(open == successes * 100);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiring_two_accounts_does_not_deadlock_with_a_concurrent_creation() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let first = seed_account(&database.app_pool, product.id, "expiring-a").await?;
        let second = seed_account(&database.app_pool, product.id, "expiring-b").await?;
        let route = test_route();
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let first_lock = create_lock(&database, &quotes, &product, &first, &route).await?;
        let second_lock = create_lock(&database, &quotes, &product, &second, &route).await?;
        sqlx::query(
            "UPDATE quotes SET expires_at = now() - interval '1 second' WHERE id = ANY($1)",
        )
        .bind([first_lock.id, second_lock.id])
        .execute(&database.app_pool)
        .await?;
        finalize_chain_past_now(&database.app_pool).await?;
        let routes = test_routes();
        let (expired, created) = tokio::join!(
            locks::expire_once(&database.app_pool, &routes),
            create_lock(&database, &quotes, &product, &second, &route),
        );
        ensure!(expired? == 2);
        created?;

        ensure!(exposure(&database.app_pool, &format!("account:{}", first.id)).await? == 0);
        ensure!(exposure(&database.app_pool, &format!("account:{}", second.id)).await? == 100);
        ensure!(exposure(&database.app_pool, &format!("product:{}", product.id)).await? == 100);
        ensure!(exposure(&database.app_pool, "global").await? == 100);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_waits_for_an_uncommitted_deposit_to_the_lock_address() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let account = seed_account(&database.app_pool, product.id, "racing").await?;
        let route = test_route();
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let lock = create_lock(&database, &quotes, &product, &account, &route).await?;

        // A scanner transaction has inserted the payment but not committed yet.
        let mut scanner = database.app_pool.begin().await?;
        insert_deposit_in(&mut scanner, lock.address_id, 0x81).await?;
        let pool = database.app_pool.clone();
        let (cancel_product, cancel_quote) = (product.clone(), lock.id);
        let cancel =
            tokio::spawn(async move { cancel_lock(&pool, &cancel_product, cancel_quote).await });
        for _ in 0..200 {
            if cancel.is_finished() || lock_waiters(&database.app_pool).await? > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        scanner.commit().await?;
        ensure!(matches!(cancel.await?, Err(RateLockError::PendingPayment)));
        ensure!(lock_status(&database.app_pool, lock.id).await? == "open");
        ensure!(exposure(&database.app_pool, &format!("account:{}", account.id)).await? == 100);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
#[traced_test]
async fn failing_expiry_scans_alert_and_recover() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        let account = seed_account(&database.app_pool, product.id, "failing").await?;
        let route = test_route();
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let lock = create_lock(&database, &quotes, &product, &account, &route).await?;
        sqlx::query("UPDATE quotes SET expires_at = now() - interval '1 second' WHERE id = $1")
            .bind(lock.id)
            .execute(&database.app_pool)
            .await?;
        // The batch fails at its `quote.expired` event and rolls back.
        sqlx::query("REVOKE INSERT ON events FROM topup_app")
            .execute(&database.owner_pool)
            .await?;
        finalize_chain_past_now(&database.app_pool).await?;

        let cancellation = tokio_util::sync::CancellationToken::new();
        let worker = locks::ExpiryWorker::new(
            database.app_pool.clone(),
            Arc::new(test_routes()),
            std::time::Duration::from_millis(10),
        );
        let worker_cancellation = cancellation.clone();
        let running = tokio::spawn(async move { worker.run(worker_cancellation).await });
        // The current-thread test runtime polls the spawned worker inside this test's span.
        for _ in 0..400 {
            if logs_contain("TopupLockExpiryFailing") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        ensure!(
            logs_contain("TopupLockExpiryFailing"),
            "expiry never failed"
        );
        ensure!(lock_status(&database.app_pool, lock.id).await? == "open");

        sqlx::query("GRANT INSERT ON events TO topup_app")
            .execute(&database.owner_pool)
            .await?;
        for _ in 0..400 {
            if lock_status(&database.app_pool, lock.id).await? == "expired" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancellation.cancel();
        running.await?;
        ensure!(lock_status(&database.app_pool, lock.id).await? == "expired");
        ensure!(exposure(&database.app_pool, "global").await? == 0);
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exposure_is_exact_after_concurrent_create_consume_cancel_and_expire() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        const ACCOUNTS: usize = 4;
        let product = seed_product_without_key(&database.app_pool, "phala-cloud").await?;
        set_limits(
            &database.app_pool,
            product.id,
            true,
            None,
            Some(1_000_000),
            Some(1_000_000),
        )
        .await?;
        let route = Arc::new(test_route());
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let mut accounts = Vec::new();
        for index in 0..ACCOUNTS {
            let account =
                seed_account(&database.app_pool, product.id, &format!("c-{index}")).await?;
            let mut ids = std::collections::HashMap::new();
            for name in ["consume", "cancel", "expire", "keep"] {
                let lock = create_lock(&database, &quotes, &product, &account, &route).await?;
                if name == "expire" {
                    sqlx::query("UPDATE quotes SET expires_at = now() WHERE id = $1")
                        .bind(lock.id)
                        .execute(&database.app_pool)
                        .await?;
                }
                ids.insert(name, lock.id);
            }
            accounts.push((account, ids["cancel"], ids["consume"]));
        }
        finalize_chain_past_now(&database.app_pool).await?;

        let mut tasks = tokio::task::JoinSet::new();
        for (index, (account, cancel_id, consume_id)) in accounts.iter().cloned().enumerate() {
            let (pool, quotes, product, route) = (
                database.app_pool.clone(),
                Arc::clone(&quotes),
                product.clone(),
                Arc::clone(&route),
            );
            tasks.spawn(async move {
                for _ in 0..3 {
                    locks::create(
                        &pool,
                        &quotes,
                        &client_secret_key(),
                        &product,
                        &account,
                        &route,
                        MinorAmount::new(100),
                        &Default::default(),
                    )
                    .await?;
                }
                cancel_lock(&pool, &product, cancel_id).await?;
                consume_lock(&pool, consume_id, u8::try_from(index)?).await?;
                anyhow::Ok(())
            });
        }
        {
            let pool = database.app_pool.clone();
            tasks.spawn(async move {
                while sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM quotes WHERE status='expired'",
                )
                .fetch_one(&pool)
                .await?
                    < ACCOUNTS as i64
                {
                    locks::expire_once(&pool, &test_routes()).await?;
                }
                anyhow::Ok(())
            });
        }
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while let Some(task) = tasks.join_next().await {
                task??;
            }
            anyhow::Ok(())
        })
        .await
        .context("lifecycle operations never completed")??;

        // Cancellation reserves exposure until the dual scan catches up after the request.
        finalize_chain_past_now(&database.app_pool).await?;
        locks::expire_once(&database.app_pool, &test_routes()).await?;
        ensure!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM quotes WHERE status='cancelled'")
                .fetch_one(&database.app_pool)
                .await?
                == ACCOUNTS as i64
        );

        // Per account: "keep" plus three new locks remain reserved.
        let open = exposure(&database.app_pool, "global").await?;
        ensure!(open == 4 * 100 * ACCOUNTS as u64, "{open}");
        ensure!(exposure(&database.app_pool, &format!("product:{}", product.id)).await? == open);
        for (account, _, _) in &accounts {
            ensure!(exposure(&database.app_pool, &format!("account:{}", account.id)).await? == 400);
        }
        Ok(())
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

/// Commits the test chain's scanner cursor through a finalized block just past every lock already
/// overdue by wall clock, as the scanner does on reaching the finalized head.
async fn finalize_chain_past_now(pool: &sqlx::PgPool) -> Result<()> {
    set_finalized_time(pool, Utc::now() + chrono::Duration::seconds(1)).await
}

async fn set_finalized_time(pool: &sqlx::PgPool, time: chrono::DateTime<Utc>) -> Result<()> {
    sqlx::query(
        "UPDATE chain_coverage SET through_time=$1,through_block=through_block+1 WHERE chain_id=1",
    )
    .bind(time)
    .execute(pool)
    .await?;
    sqlx::query("UPDATE addresses SET dual_covered_through=(SELECT through_block FROM chain_coverage WHERE chain_id=1) WHERE chain_id=1").execute(pool).await?;
    sqlx::query(
        r#"
        INSERT INTO cursors (chain_id, scanned_block, scanned_block_time)
        VALUES (1, 0, $1)
        ON CONFLICT (chain_id) DO UPDATE SET scanned_block_time = EXCLUDED.scanned_block_time
        "#,
    )
    .bind(time)
    .execute(pool)
    .await?;
    Ok(())
}

/// Consumes a lock the way the confirm step does: a leased deposit transition with consumption.
async fn consume_lock(pool: &sqlx::PgPool, quote_id: Uuid, number: u8) -> Result<()> {
    let address_id: Uuid = sqlx::query_scalar("SELECT id FROM addresses WHERE quote_id = $1")
        .bind(quote_id)
        .fetch_one(pool)
        .await?;
    let mut transaction = pool.begin().await?;
    let deposit_id = insert_deposit_in(&mut transaction, address_id, number).await?;
    let lease_token = Uuid::new_v4();
    sqlx::query(
        "UPDATE deposits SET state = 'detected', reason = NULL, lease_token = $2, lease_until = now() + interval '5 minutes' WHERE id = $1",
    )
    .bind(deposit_id)
    .bind(lease_token)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    let advance = topup_core::deposit::next(
        DepositState::Detected,
        &topup_core::deposit::StepOutcome::Advance,
    )?;
    let effects = topup::db::TransitionEffects {
        lock_consumption: Some(topup::db::LockConsumption {
            address_id,
            idempotent: false,
        }),
        ..topup::db::TransitionEffects::default()
    };
    let mut transaction = pool.begin().await?;
    let applied = topup::db::apply_transition(
        &mut transaction,
        &topup::routes::RouteSet::default(),
        deposit_id,
        DepositState::Detected,
        lease_token,
        topup::db::TransitionUpdate {
            transition: advance,
            rejection_reason: None,
            attempt: 0,
            next_attempt_at: Utc::now(),
        },
        topup::db::TransitionWrites {
            evidence: &json!({"test": "consume"}),
            effects: &effects,
            outbox_events: &[],
        },
    )
    .await?;
    ensure!(
        applied == topup::db::ApplyTransitionResult::Applied,
        "consumption was not applied: {applied:?}"
    );
    transaction.commit().await?;
    let status: String = sqlx::query_scalar("SELECT status FROM quotes WHERE id = $1")
        .bind(quote_id)
        .fetch_one(pool)
        .await?;
    ensure!(status == "consumed");
    Ok(())
}

/// Inserts a rejected deposit to `address_id`, under the address's account, mode, and customer.
async fn insert_deposit_in(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    address_id: Uuid,
    number: u8,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO deposits (
            id, chain_id, tx_hash, log_index, block_number, block_hash, block_time,
            address_id, account_id, livemode, customer_id, asset_contract, from_address,
            amount_atomic, state, reason, next_attempt_at, receipt_log_index, tx_from, tx_nonce,
            settings_revision_id
        )
        SELECT $1, 1, $2, 0, 10, $3, now(), address.id, address.account_id, address.livemode,
               quote.customer_id, $5, $6, 100, 'rejected', 'unsupported_asset', now(), 0, $6, 0,
               settings.current_revision_id
        FROM addresses AS address
        JOIN quotes AS quote ON quote.id = address.quote_id
        JOIN payment_settings_state AS settings
            ON settings.account_id = address.account_id AND settings.livemode = address.livemode
        WHERE address.id = $4
        "#,
    )
    .bind(id)
    .bind(format!("{:#x}", B256::repeat_byte(number)))
    .bind(format!("{:#x}", B256::repeat_byte(number.wrapping_add(1))))
    .bind(address_id)
    .bind(format!("{:#x}", Address::repeat_byte(0x73)))
    .bind(format!("{:#x}", Address::repeat_byte(0x74)))
    .execute(&mut **transaction)
    .await?;
    Ok(id)
}

async fn lock_waiters(pool: &sqlx::PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM pg_stat_activity
        WHERE datname = current_database()
          AND wait_event_type = 'Lock'
          AND pid <> pg_backend_pid()
        "#,
    )
    .fetch_one(pool)
    .await?)
}

async fn create_lock(
    database: &TestDatabase,
    quotes: &Arc<dyn QuoteProvider>,
    product: &Account,
    account: &Customer,
    route: &RouteFile,
) -> Result<locks::RateLock> {
    let (lock, _) = locks::create(
        &database.app_pool,
        quotes,
        &client_secret_key(),
        product,
        account,
        route,
        MinorAmount::new(100),
        &Default::default(),
    )
    .await?;
    Ok(lock)
}

/// Cancels the account's quote `quote_id` as its API key does.
/// Whether no other transaction holds quote `id`, taken as the expiry takes quotes: `FOR UPDATE
/// SKIP LOCKED`, which returns nothing for a locked row.
async fn quote_unlocked(probe: &mut sqlx::PgConnection, id: Uuid) -> Result<bool> {
    let row: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM quotes WHERE id = $1 FOR UPDATE SKIP LOCKED")
            .bind(id)
            .fetch_optional(&mut *probe)
            .await?;
    Ok(row == Some(id))
}

async fn cancel_lock(
    pool: &sqlx::PgPool,
    product: &Account,
    quote_id: Uuid,
) -> Result<locks::RateLock, RateLockError> {
    locks::cancel(
        pool,
        &test_routes(),
        Scope::new(product.id, true),
        &Actor::api_key(format!("key_{}", Uuid::nil().simple())),
        quote_id,
    )
    .await
}

/// Sum of open reserved lock credit in one scope: `account:<customer id>`, `product:<account
/// id>`, or `global`.
async fn exposure(pool: &sqlx::PgPool, scope: &str) -> Result<u64> {
    let open: String = sqlx::query_scalar(
        r#"
        SELECT coalesce(sum(credit_minor), 0)::text
        FROM quotes
        WHERE status = 'open' AND exposure_reserved
          AND $1 IN ('global', 'account:' || customer_id::text, 'product:' || account_id::text)
        "#,
    )
    .bind(scope)
    .fetch_one(pool)
    .await?;
    Ok(open.parse()?)
}

/// Status of the quote `quote_id`.
async fn lock_status(pool: &sqlx::PgPool, quote_id: Uuid) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT status FROM quotes WHERE id = $1")
            .bind(quote_id)
            .fetch_one(pool)
            .await?,
    )
}

async fn quote_status(pool: &sqlx::PgPool, quote_id: &str) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT status FROM quotes WHERE id = $1")
            .bind(topup::ids::parse(topup::ids::QUOTE, quote_id).context("quote id")?)
            .fetch_one(pool)
            .await?,
    )
}

async fn quote_address_id(pool: &sqlx::PgPool, quote_id: &str) -> Result<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT id FROM addresses WHERE quote_id = $1")
            .bind(topup::ids::parse(topup::ids::QUOTE, quote_id).context("quote id")?)
            .fetch_one(pool)
            .await?,
    )
}

async fn insert_rejected_deposit(
    pool: &sqlx::PgPool,
    route: &RouteFile,
    address_id: Uuid,
) -> Result<()> {
    let inserted = topup::db::insert_deposit(
        pool,
        &topup::db::NewDeposit {
            chain_id: route.chain.chain_id,
            tx_hash: B256::repeat_byte(0x71),
            log_index: 0,
            receipt_log_index: 0,
            tx_from: alloy_primitives::Address::ZERO,
            tx_nonce: 0,
            is_final: true,
            block_number: 10,
            block_hash: B256::repeat_byte(0x72),
            block_time: Utc::now(),
            address_id,
            route: None,
            route_version: None,
            asset_contract: Address::repeat_byte(0x73),
            from_address: Address::repeat_byte(0x74),
            amount_atomic: AtomicAmount::new(U256::from(100_u64)),
            state: DepositState::Rejected,
            reason: Some(RejectReason::UnsupportedAsset),
            next_attempt_at: Utc::now(),
        },
    )
    .await?;
    ensure!(inserted);
    Ok(())
}

/// The route set quotes of [`test_route`] render with in their events.
fn test_routes() -> topup::routes::RouteSet {
    topup::routes::RouteSet::new(vec![test_route()]).expect("route fixture loads")
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

/// A live account with a treasury on chain 1, and its live secret key.
async fn seed_product(pool: &sqlx::PgPool, name: &str) -> Result<(Account, String)> {
    seed::initialize_dual_chain(pool, 1).await?;
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
    limit_quote_rate(
        pool,
        account.id,
        topup::payment_config::MAX_QUOTE_CREATIONS_PER_CUSTOMER_PER_MINUTE,
    )
    .await?;
    Ok((account, key))
}

/// Accepts the test route's asset with one customer's quote creations per minute at `rate`.
async fn limit_quote_rate(pool: &sqlx::PgPool, account_id: Uuid, rate: u64) -> Result<()> {
    seed::configure_payments(
        pool,
        account_id,
        true,
        &topup::payment_config::Document {
            quote_creations_per_customer_per_minute: Some(rate),
            ..topup::payment_config::Document::accepting([&test_route()])
        },
    )
    .await?;
    Ok(())
}

async fn seed_product_without_key(pool: &sqlx::PgPool, name: &str) -> Result<Account> {
    Ok(seed_product(pool, name).await?.0)
}

/// Sets the caps of `account_id` in one mode; `None` keeps the mode's default.
async fn set_limits(
    pool: &sqlx::PgPool,
    account_id: Uuid,
    livemode: bool,
    quotes: Option<u64>,
    account_minor: Option<u64>,
    customer_minor: Option<u64>,
) -> Result<()> {
    let mut connection = pool.acquire().await?;
    topup::limits::update(
        &mut connection,
        Scope::new(account_id, livemode),
        &topup::limits::LimitsChange {
            max_open_quotes: quotes,
            max_open_amount_per_account: account_minor,
            max_open_amount_per_customer: customer_minor,
            max_active_deposit_addresses: None,
        },
    )
    .await?;
    Ok(())
}

/// The account `account_id`.
async fn seed_account_row(pool: &sqlx::PgPool, account_id: Uuid) -> Result<Account> {
    topup::db::get_account(pool, account_id)
        .await?
        .context("account")
}

/// A live customer of `account_id`.
async fn seed_account(
    pool: &sqlx::PgPool,
    account_id: Uuid,
    client_reference_id: &str,
) -> Result<Customer> {
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

/// An unsigned `GET /v1/quotes/{id}?client_secret=…`, as a browser sends it.
fn client_read(quote_id: &str, client_secret: &str) -> Result<axum::http::Request<Body>> {
    Ok(axum::http::Request::get(format!(
        "/v1/quotes/{quote_id}?client_secret={client_secret}"
    ))
    .body(Body::empty())?)
}

async fn response_json(response: axum::response::Response) -> Result<Value> {
    let bytes = to_bytes(response.into_body(), 1_048_576).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[allow(dead_code)]
fn empty_body() -> Body {
    Body::empty()
}

/// A client-secret key for quotes created outside the API.
fn client_secret_key() -> topup::client_secret::ClientSecretKey {
    topup::client_secret::ClientSecretKey::ephemeral()
}

/// A batched page shows the same payment as each scoped detail read, without pulling quotes or
/// payments from another tenant/mode or changing cursor direction, filters, or has_more.
#[tokio::test]
async fn quote_pages_preserve_payments_tenant_mode_and_cursor_semantics() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
        let pool = &database.app_pool;
        let route = test_route();
        let quotes: Arc<dyn QuoteProvider> = Arc::new(FixedQuote);
        let (account, key) = seed_product(pool, "batch-quotes").await?;
        let customer = seed_account(pool, account.id, "shared-reference").await?;
        let (other, _) = seed_product(pool, "other-batch-quotes").await?;
        let other_customer = seed_account(pool, other.id, "shared-reference").await?;
        let foreign = create_lock(database, &quotes, &other, &other_customer, &route).await?;
        let mut test_route = route.clone();
        test_route.livemode = false;
        seed::configure_payments(pool, account.id, false,
            &topup::payment_config::Document::accepting([&test_route])).await?;
        seed::set_treasury(pool, account.id, false, 1, seed::FIXTURE_TREASURY).await?;
        let test_customer = seed::create_customer(pool, &NewCustomer {
            id: Uuid::new_v4(), account_id: account.id, livemode: false,
            client_reference_id: "shared-reference".to_owned(), paused_scopes: Vec::new(),
        }).await?;
        let test_lock = create_lock(database, &quotes, &account, &test_customer, &test_route).await?;
        let mut live = Vec::new();
        for n in 0..4 {
            let lock = create_lock(database, &quotes, &account, &customer, &route).await?;
            sqlx::query("UPDATE quotes SET created_at='2026-01-01'::timestamptz + $2 * interval '1 second' WHERE id=$1")
                .bind(lock.id).bind(n).execute(&database.owner_pool).await?;
            live.push(lock);
        }
        consume_lock(pool, live[0].id, 0x61).await?;
        insert_rejected_deposit(pool, &route, live[1].address_id).await?;
        let mut tx = database.owner_pool.begin().await?;
        let earlier = insert_deposit_in(&mut tx,live[0].address_id,0x60).await?;
        sqlx::query("UPDATE deposits SET block_number=9,asset_contract=$2 WHERE id=$1")
            .bind(earlier).bind(format!("{:#x}",route.asset.contract)).execute(&mut *tx).await?;
        let reversed = insert_deposit_in(&mut tx,live[3].address_id,0x68).await?;
        sqlx::query("UPDATE deposits SET state='reversed',reason=NULL WHERE id=$1").bind(reversed).execute(&mut *tx).await?;
        tx.commit().await?;
        // A matching pending transfer beats an unsupported recorded payment; another quote's
        // matching pending payment must never appear on a quote with no observations.
        let mut pending = [(&live[1],0x62), (&live[2],0x63), (&foreign,0x64), (&test_lock,0x65), (&live[3],0x68)]
            .into_iter().map(|(lock, hash)| topup::db::NewPendingTransfer {
                chain_id: 1, tx_hash: B256::repeat_byte(hash), receipt_log_index: 0, log_index: 0,
                block_number: 20, block_hash: B256::repeat_byte(0x66), block_time: Utc::now(),
                address_id: lock.address_id, asset_contract: route.asset.contract,
                from_address: Address::repeat_byte(0x67), amount_atomic: lock.amount_atomic,
            }).collect::<Vec<_>>();
        // A stale copy of the recorded unsupported transfer must not become a matching payment.
        let mut duplicate = pending[0].clone();
        duplicate.tx_hash = B256::repeat_byte(0x71);
        duplicate.block_number = 19;
        pending.push(duplicate);
        topup::db::commit_head_scan(pool,1,19,21,&pending).await?;
        let admin_key = SigningKey::from_bytes(&[48;32]);
        let app = topup::api::router(AppState {
            pool: pool.clone(), routes: Arc::new(test_routes()),
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(ADMIN_KID.to_owned(), &public_key_base64(&admin_key)).map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse(TEST_ORIGIN)?, attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: quotes, client_reads: Arc::default(), rate_limits: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        }).0;
        let get = |path: String| {
            let app = app.clone();
            let key = key.clone();
            async move {
                let response = app.oneshot(merchant_request(Method::GET,&path,Vec::new(),&key)).await?;
                ensure!(response.status()==StatusCode::OK,"{path}: {}",response.status());
                ensure!(response.headers()["cache-control"]=="no-store");
                response_json(response).await
            }
        };
        let all = get("/v1/quotes?client_reference_id=shared-reference".to_owned()).await?;
        let rows = all["data"].as_array().context("quote data")?;
        ensure!(rows.len()==4 && all["has_more"]==false,"{all}");
        for (row, lock) in rows.iter().zip(live.iter().rev()) {
            let detail = get(format!("/v1/quotes/{}",locks::quote_id(lock.id))).await?;
            ensure!(row==&detail,"batched and detail payment differ: {row}, {detail}");
        }
        ensure!(rows[0]["payment"].is_null());
        ensure!(rows[1]["payment"]["status"]=="seen" && rows[1]["payment"]["matches_quote"]==true);
        ensure!(rows[2]["payment"]["status"]=="seen" && rows[2]["payment"]["matches_quote"]==true);
        ensure!(rows[2]["payment"]["tx_hash"]==format!("{:#x}",B256::repeat_byte(0x62)),"pending duplicate selected: {}",rows[2]);
        ensure!(rows[3]["payment"]["status"]=="recorded");
        ensure!(rows[3]["payment"]["tx_hash"]==format!("{:#x}",B256::repeat_byte(0x61)),"consumed_by priority lost: {}",rows[3]);
        let first = get("/v1/quotes?limit=2".to_owned()).await?;
        ensure!(first["has_more"]==true && first["data"]==json!(rows[..2]));
        let next = get(format!("/v1/quotes?limit=2&starting_after={}",locks::quote_id(live[2].id))).await?;
        ensure!(next["has_more"]==false && next["data"]==json!(rows[2..]));
        let before = get(format!("/v1/quotes?limit=2&ending_before={}",locks::quote_id(live[1].id))).await?;
        ensure!(before["has_more"]==false && before["data"]==first["data"]);
        let completed = get("/v1/quotes?status=complete".to_owned()).await?;
        ensure!(completed["data"]==json!([rows[3]]));
        for id in [foreign.id,test_lock.id] {
            let response = app.clone().oneshot(merchant_request(Method::GET,
                &format!("/v1/quotes?starting_after={}",locks::quote_id(id)),Vec::new(),&key)).await?;
            ensure!(response.status()==StatusCode::BAD_REQUEST);
        }
        Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn exhausted_database_snapshot_budget_returns_retryable_price_unavailable() -> Result<()> {
    support::with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let (account, key) = seed_product(pool, "phala-cloud").await?;
            seed_account(pool, account.id, "budget-checkout").await?;
            let mut route = test_route();
            route.pricing.mode = topup_core::route::PricingMode::Stablecoin;
            route.asset.symbol = "usdc".into();
            route.pricing.primary.clear();
            route.pricing.check.clear();
            route.pricing.fx.clear();
            route.pricing.sources = vec![topup_core::price::Source::Chainlink {
                feed: "USDC_USD".into(), chain_id: 1, observation_chain_id: None,
            }];
            route.validate()?;
            seed::accept_routes(pool, account.id, true, &[&route]).await?;
            sqlx::query("INSERT INTO daily_budgets(day,name,used) VALUES ((now() AT TIME ZONE 'UTC')::date,'price:1',100)")
                .execute(pool).await?;
            // An exhausted budget must fail before contacting either endpoint.
            let endpoint = Arc::new(topup_adapters::chain::evm::EvmClient::new("http://127.0.0.1:1")?.with_chain_id(1));
            let routes = Arc::new(topup::routes::RouteSet::with_rpc(vec![route], std::collections::BTreeMap::from([
                (1, topup::chain_rpc::ChainRpc { read: endpoint.clone(), verify: endpoint }),
            ])).map_err(anyhow::Error::msg)?);
            let runtimes = locks::pricing::PricingRuntime::build_all(&routes, pool.clone()).map_err(anyhow::Error::msg)?;
            let admin = SigningKey::from_bytes(&[44;32]);
            let app = topup::api::router(AppState {
                pool:pool.clone(), routes,
                admin_key:VerificationKey::from_base64(ADMIN_KID.into(), &public_key_base64(&admin)).map_err(anyhow::Error::msg)?,
                maintenance_keys:Vec::new(), public_origin:PublicOrigin::parse(TEST_ORIGIN)?,
                attestor:Arc::new(DstackAttestor::new()), rate_lock_quotes:Arc::new(locks::ConfiguredQuoteProvider::from_runtimes(runtimes)),
                client_reads:Arc::default(),rate_limits:Arc::default(),
                screening:Arc::new(topup::refunds::UnavailableDestinationScreener),
                contract_signatures:Arc::new(topup::treasuries::UnavailableContractSignatures),
            }).0;
            let response = app.oneshot(merchant_request_with_key(Method::POST,"/v1/quotes",serde_json::to_vec(&json!({
                "client_reference_id":"budget-checkout","amount":100,"currency":"usd","chain_id":1,"asset":"usdc",
            }))?,&key,"snapshot-cap")).await?;
            ensure!(response.status()==StatusCode::SERVICE_UNAVAILABLE);
            ensure!(response_json(response).await?["error"]["code"]=="price_unavailable");
            ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM quotes").fetch_one(pool).await?==0);
            ensure!(sqlx::query_scalar::<_,i32>("SELECT used FROM daily_budgets WHERE name='price:1'").fetch_one(pool).await?==100);
            Ok(())
        })
    }).await
}
