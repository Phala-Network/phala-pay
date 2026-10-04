//! Anvil and PostgreSQL integration coverage for the C3 scanner.

mod support;

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};
use topup::db;
use topup::pump::{Pump, PumpConfig, RunOnceResult, Step, StepResult, StepSet};
use topup::routes::RouteSet;
use topup::scanner::{ChainRoutes, chain_routes, initialize_cursor, scan_new_blocks, scan_once};
use topup::steps::confirm::ConfirmStep;
use topup_adapters::chain::evm::{
    ChainError, ChainReader, EvmClient, FinalizedHead, FinalizedReader, ReceiptLookup, TransferLog,
};
use topup_adapters::pricing::{Observation, PriceError, PriceSource};
use topup_core::deposit::{StepOutcome, WaitReason};
use topup_core::money::{AtomicAmount, PRICE_SCALE, ScaledPrice};
use topup_core::route::{Backstop, ChainHeads, Confirmations, RouteFile};
use topup_core::valuation::{SourceId, UnixSeconds};
use uuid::Uuid;

use support::TestDatabase;
use support::chain::{
    ANVIL_PRIVATE_KEY, Anvil, CHAIN_ID, contracts_dir, forge_create, run_checked,
};
use support::seed::{self, NewAccount, NewAddress};

/// One slot per epoch keeps anvil's finalized block close to the head for the finalized scanner.
const ANVIL_ARGS: &[&str] = &["--slots-in-an-epoch", "1"];
const ANVIL_DEPLOYER: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordedRequest {
    /// Recipients in the request's filter; empty for a token-wide request.
    addresses: Vec<Address>,
    from_block: u64,
    to_block: u64,
}

struct RecordingReader {
    finalized: u64,
    finalized_time: DateTime<Utc>,
    requests: Mutex<Vec<RecordedRequest>>,
}

impl RecordingReader {
    fn new(finalized: u64, finalized_time: DateTime<Utc>) -> Self {
        Self {
            finalized,
            finalized_time,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl ChainReader for RecordingReader {
    async fn factory_logs(
        &self,
        _factory: Address,
        _forwarders: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<topup_adapters::chain::evm::FactoryLog>, ChainError> {
        Ok(Vec::new())
    }

    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Ok(FinalizedHead {
            number: self.finalized,
            time: self.finalized_time,
        })
    }

    async fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        self.requests
            .lock()
            .map_err(|_| ChainError::HealthStateUnavailable)?
            .push(RecordedRequest {
                addresses: addresses.to_vec(),
                from_block,
                to_block,
            });
        Ok(Vec::new())
    }

    async fn token_transfers(
        &self,
        _tokens: &[Address],
        _recipients: &std::collections::BTreeSet<Address>,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        self.requests
            .lock()
            .map_err(|_| ChainError::HealthStateUnavailable)?
            .push(RecordedRequest {
                addresses: Vec::new(),
                from_block,
                to_block,
            });
        Ok(Vec::new())
    }

    async fn confirmation_heads(
        &self,
        _confirmations: Confirmations,
    ) -> Result<ChainHeads, ChainError> {
        let finalized = self.finalized_head().await?.number;
        Ok(ChainHeads {
            latest: Some(finalized),
            safe: Some(finalized),
            finalized,
        })
    }

    async fn receipt_transfer(
        &self,
        _tx_hash: B256,
        _receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        Ok(ReceiptLookup::Missing)
    }

    async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
        Ok(0)
    }
}

struct BackfillReader {
    recipient: Address,
    token: Address,
    fail_request: Mutex<Option<usize>>,
    requests: Mutex<Vec<RecordedRequest>>,
}

impl BackfillReader {
    fn new(recipient: Address, token: Address, fail_request: usize) -> Self {
        Self {
            recipient,
            token,
            fail_request: Mutex::new(Some(fail_request)),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().expect("request lock").len()
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl ChainReader for BackfillReader {
    async fn factory_logs(
        &self,
        _factory: Address,
        _forwarders: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<topup_adapters::chain::evm::FactoryLog>, ChainError> {
        Ok(Vec::new())
    }

    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Ok(FinalizedHead {
            number: 4_000,
            time: DateTime::UNIX_EPOCH,
        })
    }

    async fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        let request_number = {
            let mut requests = self
                .requests
                .lock()
                .map_err(|_| ChainError::HealthStateUnavailable)?;
            requests.push(RecordedRequest {
                addresses: addresses.to_vec(),
                from_block,
                to_block,
            });
            requests.len()
        };
        let should_fail = {
            let mut fail_request = self
                .fail_request
                .lock()
                .map_err(|_| ChainError::HealthStateUnavailable)?;
            if *fail_request == Some(request_number) {
                *fail_request = None;
                true
            } else {
                false
            }
        };
        if should_fail {
            return Err(ChainError::Rpc("scripted backfill failure"));
        }
        if !addresses.contains(&self.recipient) {
            return Ok(Vec::new());
        }

        let mut logs = Vec::new();
        if from_block <= 100 && 100 <= to_block {
            logs.push(mock_transfer_log(self.token, self.recipient, 100, 1));
        }
        if from_block <= 3_000 && 3_000 <= to_block {
            logs.push(mock_transfer_log(self.token, self.recipient, 3_000, 2));
        }
        Ok(logs)
    }

    async fn confirmation_heads(
        &self,
        _confirmations: Confirmations,
    ) -> Result<ChainHeads, ChainError> {
        let finalized = self.finalized_head().await?.number;
        Ok(ChainHeads {
            latest: Some(finalized),
            safe: Some(finalized),
            finalized,
        })
    }

    async fn receipt_transfer(
        &self,
        _tx_hash: B256,
        _receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        Ok(ReceiptLookup::Missing)
    }

    async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
        Ok(0)
    }
}

const BASE_SEPOLIA: u64 = 84_532;
/// Test PHA on Base Sepolia (the staging route `phala-cloud-base-sepolia-pha-usd`).
const BASE_SEPOLIA_PHA: Address =
    alloy_primitives::address!("1a6F260377e42ead1418C7C1afDFD5DE371A9284");

#[derive(Clone, Debug)]
struct StagingRequest {
    from_block: u64,
    to_block: u64,
    ok: bool,
}

/// Provider A of the staging incident: one payment to the quote address, and every
/// `refuse_every`-th transfer request refused, as a public gateway refuses a burst.
struct StagingReader {
    token: Address,
    quote: Address,
    payment_block: u64,
    finalized: Mutex<u64>,
    refuse_every: usize,
    requests: Mutex<Vec<StagingRequest>>,
}

impl StagingReader {
    fn new(
        token: Address,
        quote: Address,
        payment_block: u64,
        finalized: u64,
        refuse_every: usize,
    ) -> Self {
        Self {
            token,
            quote,
            payment_block,
            finalized: Mutex::new(finalized),
            refuse_every,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn set_finalized(&self, block: u64) {
        *self.finalized.lock().expect("finalized lock") = block;
    }

    fn requests(&self) -> Vec<StagingRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl ChainReader for StagingReader {
    async fn factory_logs(
        &self,
        _factory: Address,
        _forwarders: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<topup_adapters::chain::evm::FactoryLog>, ChainError> {
        Ok(Vec::new())
    }

    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Ok(FinalizedHead {
            number: *self
                .finalized
                .lock()
                .map_err(|_| ChainError::HealthStateUnavailable)?,
            time: DateTime::UNIX_EPOCH,
        })
    }

    async fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        let refused = {
            let mut requests = self
                .requests
                .lock()
                .map_err(|_| ChainError::HealthStateUnavailable)?;
            let refused =
                self.refuse_every > 0 && requests.len().saturating_add(1) % self.refuse_every == 0;
            requests.push(StagingRequest {
                from_block,
                to_block,
                ok: !refused,
            });
            refused
        };
        if refused {
            return Err(ChainError::Rpc("scripted provider refusal"));
        }
        let mut logs = Vec::new();
        if addresses.contains(&self.quote) && (from_block..=to_block).contains(&self.payment_block)
        {
            logs.push(mock_transfer_log(
                self.token,
                self.quote,
                self.payment_block,
                0x4b,
            ));
        }
        Ok(logs)
    }

    async fn confirmation_heads(
        &self,
        _confirmations: Confirmations,
    ) -> Result<ChainHeads, ChainError> {
        let finalized = self.finalized_head().await?.number;
        Ok(ChainHeads {
            latest: Some(finalized),
            safe: Some(finalized),
            finalized,
        })
    }

    async fn receipt_transfer(
        &self,
        _tx_hash: B256,
        _receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        Ok(ReceiptLookup::Missing)
    }

    async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
        Ok(0)
    }
}

/// The committed Base Sepolia routes: an OP-stack chain credited at depth 3, in address mode.
fn base_sepolia_routes() -> Result<ChainRoutes> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/environments/phala-network/staging/topup/topup.yaml");
    let routes = topup::config::Config::load(&path)
        .map_err(anyhow::Error::msg)?
        .routes
        .into_iter()
        .filter(|route| route.chain.chain_id == BASE_SEPOLIA)
        .collect::<Vec<_>>();
    chain_routes(&RouteSet::new(routes).map_err(anyhow::Error::msg)?)
        .into_iter()
        .next()
        .context("the Base Sepolia chain")
}

/// Issues a quote address on Base Sepolia at the chain's committed cursor, as quote creation does.
async fn issue_base_sepolia_address(
    pool: &PgPool,
    customer_id: Uuid,
    address: Address,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    seed::insert_address(
        pool,
        &NewAddress {
            id,
            customer_id,
            chain_id: BASE_SEPOLIA,
            route: "phala-cloud-base-sepolia-pha-usd".to_owned(),
            salt: B256::repeat_byte(0x5a),
            address,
        },
    )
    .await?;
    sqlx::query(
        "UPDATE addresses SET created_block = (SELECT scanned_block FROM cursors \
         WHERE cursors.chain_id = addresses.chain_id) WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(id)
}

struct RouteFixture {
    path: PathBuf,
}

impl RouteFixture {
    fn create(token: Address) -> Result<Self> {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("c3-route-{}.yaml", Uuid::new_v4()));
        let yaml = include_str!("fixtures/phala-cloud-pha.yaml")
            .replace("chain_id: 1", &format!("chain_id: {CHAIN_ID}"))
            .replace("livemode: true", "livemode: false")
            .replace(
                "0x6c5bA91642F10282b576d91922Ae6448C9d52f4E",
                &format!("{token:#x}"),
            );
        std::fs::write(&path, yaml)?;
        Ok(Self { path })
    }
}

/// Loads a route file through the same validation and grouping `topup run` uses.
fn scanner_route(path: &Path, backstop: Backstop) -> Result<ChainRoutes> {
    let mut route: RouteFile = serde_saphyr::from_str(&std::fs::read_to_string(path)?)?;
    route.asset.backstop = backstop;
    chain_routes(&RouteSet::new(vec![route]).map_err(anyhow::Error::msg)?)
        .into_iter()
        .next()
        .context("one chain route")
}

impl Drop for RouteFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[tokio::test]
async fn finalized_scanner_is_idempotent_atomic_and_backfills_new_addresses() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let Some(anvil) = Anvil::start_if_available(ANVIL_ARGS).await? else {
        database.cleanup().await?;
        return Ok(());
    };

    let result = run_scenario(&database, &anvil).await;
    drop(anvil);
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn backfill_failure_keeps_marker_unset_then_retries_without_duplicates() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };

    let result = run_backfill_retry_scenario(&database).await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn a_payment_seen_before_the_chain_stopped_is_recorded_after_a_restart() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };

    let result = run_stop_restart_scenario(&database).await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn a_new_chain_starts_its_cursor_at_the_finalized_head() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };

    let result = run_new_chain_scenario(&database).await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn scanner_shards_actual_requests_and_includes_lock_addresses() -> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };

    let result = run_sharding_scenario(&database).await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

#[tokio::test]
async fn scanned_transfer_confirms_with_two_providers_and_waits_for_a_lagging_provider()
-> Result<()> {
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let Some(primary_anvil) = Anvil::start_if_available(ANVIL_ARGS).await? else {
        database.cleanup().await?;
        return Ok(());
    };
    let Some(lagging_anvil) = Anvil::start_if_available(ANVIL_ARGS).await? else {
        drop(primary_anvil);
        database.cleanup().await?;
        return Ok(());
    };

    let result = run_confirm_scenario(&database, &primary_anvil, &lagging_anvil).await;
    drop(lagging_anvil);
    drop(primary_anvil);
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

async fn run_scenario(database: &TestDatabase, anvil: &Anvil) -> Result<()> {
    run_checked("forge", &["build"], Some(&contracts_dir()))?;
    let supported_token = deploy_token(&anvil.rpc_url)?;
    let unsupported_token = deploy_token(&anvil.rpc_url)?;
    let nft = forge_create(
        &anvil.rpc_url,
        "test/mocks/MockTokens.sol:MockERC721Transfer",
        &[],
    )?;
    let tracked_one = Address::from([0x11_u8; 20]);
    let tracked_two = Address::from([0x22_u8; 20]);
    let tracked_later = Address::from([0x33_u8; 20]);

    let account_id = seed_account(&database.app_pool).await?;
    insert_address(&database.app_pool, account_id, tracked_one, 1).await?;
    insert_address(&database.app_pool, account_id, tracked_two, 2).await?;
    transfer(&anvil.rpc_url, supported_token, tracked_one, 101)?;
    transfer(&anvil.rpc_url, supported_token, tracked_two, 202)?;
    transfer(&anvil.rpc_url, unsupported_token, tracked_one, 303)?;
    mint_nft(&anvil.rpc_url, nft, tracked_one, 404)?;
    anvil.mine(2)?;

    let route_fixture = RouteFixture::create(supported_token)?;
    let routes = scanner_route(&route_fixture.path, Backstop::Addresses)?;
    let reader = reader(&anvil.rpc_url)?;
    let expected_cursor = reader.finalized_head().await?.number;
    let first = scan_once(&database.app_pool, &reader, &routes).await?;
    ensure!(
        first.inserted == 3,
        "expected three deposits, got {first:?}"
    );
    assert_deposit_counts(&database.app_pool, 3, 1).await?;
    assert_unsupported_asset_events(&database.app_pool, account_id).await?;
    ensure!(
        db::get_cursor(&database.app_pool, CHAIN_ID).await? == Some(expected_cursor),
        "cursor did not advance past the skipped ERC-721 Transfer"
    );

    sqlx::query("UPDATE cursors SET scanned_block = 0 WHERE chain_id = $1")
        .bind(i64::try_from(CHAIN_ID)?)
        .execute(&database.app_pool)
        .await?;
    let duplicate = scan_once(&database.app_pool, &reader, &routes).await?;
    ensure!(duplicate.inserted == 0, "duplicate logs inserted again");
    assert_deposit_counts(&database.app_pool, 3, 1).await?;
    assert_unsupported_asset_events(&database.app_pool, account_id).await?;

    let cursor_before_failure = db::get_cursor(&database.app_pool, CHAIN_ID)
        .await?
        .context("cursor after first scan")?;
    transfer(&anvil.rpc_url, supported_token, tracked_one, 404)?;
    anvil.mine(2)?;
    install_insert_failure(&database.owner_pool).await?;
    ensure!(
        scan_once(&database.app_pool, &reader, &routes)
            .await
            .is_err(),
        "forced insert failure must fail the scan"
    );
    ensure!(
        db::get_cursor(&database.app_pool, CHAIN_ID).await? == Some(cursor_before_failure),
        "cursor advanced despite a rolled-back deposit insert"
    );
    remove_insert_failure(&database.owner_pool).await?;
    let recovered = scan_once(&database.app_pool, &reader, &routes).await?;
    ensure!(recovered.inserted == 1);

    transfer(&anvil.rpc_url, supported_token, tracked_later, 505)?;
    let transfer_block = current_block(&anvil.rpc_url)?;
    anvil.mine(2)?;
    let before_address = scan_once(&database.app_pool, &reader, &routes).await?;
    ensure!(before_address.inserted == 0);
    let later_id = insert_address(&database.app_pool, account_id, tracked_later, 3).await?;
    sqlx::query("UPDATE addresses SET created_block = $2 WHERE id = $1")
        .bind(later_id)
        .bind(i64::try_from(transfer_block)?)
        .execute(&database.app_pool)
        .await?;
    let backfill = scan_once(&database.app_pool, &reader, &routes).await?;
    ensure!(backfill.inserted == 1, "new address was not backfilled");
    let backfilled: bool = sqlx::query("SELECT backfilled FROM addresses WHERE id = $1")
        .bind(later_id)
        .fetch_one(&database.app_pool)
        .await?
        .try_get(0)?;
    ensure!(backfilled, "new address backfill marker was not committed");
    assert_deposit_counts(&database.app_pool, 5, 1).await?;

    let previous = reader.finalized_head().await?;
    ensure!(previous.number > 0);
    anvil.reset()?;
    for _ in 0..2 {
        ensure!(
            matches!(
                reader.finalized_head().await,
                Err(ChainError::FinalizedHeadRegressed { current: 0, .. })
            ),
            "a finalized head below the highest read is refused on every read"
        );
    }
    Ok(())
}

/// A deposit born `rejected(unsupported_asset)` emits exactly one `deposit.rejected` event.
async fn assert_unsupported_asset_events(pool: &PgPool, customer_id: Uuid) -> Result<()> {
    let rows = sqlx::query(
        r#"
        SELECT event.id, event.account_id, event.livemode, event.object_id, deposit.reason
        FROM events AS event
        JOIN deposits AS deposit ON deposit.id = event.object_id
        WHERE event.type = 'deposit.rejected' AND event.object_type = 'deposit'
        "#,
    )
    .fetch_all(pool)
    .await?;
    ensure!(
        rows.len() == 1,
        "expected one deposit.rejected event, got {}",
        rows.len()
    );
    let (account_id, livemode): (Uuid, bool) =
        sqlx::query_as("SELECT account_id, livemode FROM customers WHERE id = $1")
            .bind(customer_id)
            .fetch_one(pool)
            .await?;
    let deposit: Uuid = rows[0].try_get("object_id")?;
    ensure!(
        rows[0].try_get::<Option<String>, _>("reason")?.as_deref() == Some("unsupported_asset")
    );
    ensure!(
        rows[0].try_get::<Uuid, _>("account_id")? == account_id
            && rows[0].try_get::<bool, _>("livemode")? == livemode,
        "event does not name the owning account and mode"
    );
    ensure!(
        rows[0].try_get::<Uuid, _>("id")?
            == topup_core::identity::event_id("deposit.rejected", deposit),
        "event id is not derived from the deposit"
    );
    Ok(())
}

async fn run_backfill_retry_scenario(database: &TestDatabase) -> Result<()> {
    let token = Address::from([0x44_u8; 20]);
    let recipient = Address::from([0x55_u8; 20]);
    let account_id = seed_account(&database.app_pool).await?;
    let address_id = insert_address(&database.app_pool, account_id, recipient, 1).await?;
    sqlx::query("UPDATE addresses SET created_block = 1 WHERE id = $1")
        .bind(address_id)
        .execute(&database.app_pool)
        .await?;
    sqlx::query("INSERT INTO cursors (chain_id, scanned_block) VALUES ($1, 4000)")
        .bind(i64::try_from(CHAIN_ID)?)
        .execute(&database.app_pool)
        .await?;

    let route_fixture = RouteFixture::create(token)?;
    let routes = scanner_route(&route_fixture.path, Backstop::Addresses)?;
    let reader = BackfillReader::new(recipient, token, 2);

    ensure!(
        matches!(
            scan_once(&database.app_pool, &reader, &routes).await,
            Err(topup::scanner::ScannerError::Chain(ChainError::Rpc(_)))
        ),
        "the scripted second backfill window must fail"
    );
    ensure!(!address_backfilled(&database.app_pool, address_id).await?);
    ensure!(deposit_count(&database.app_pool).await? == 1);

    let failed_request_count = reader.request_count();
    let recovered = scan_once(&database.app_pool, &reader, &routes).await?;
    ensure!(
        recovered.inserted == 1,
        "only the missing window should insert"
    );
    let resumed = reader.requests();
    ensure!(
        resumed
            .get(failed_request_count..)
            .is_some_and(|requests| requests.iter().all(|request| request.from_block == 2_001)),
        "the retry resumes after the committed window instead of reading it again: {resumed:?}"
    );
    ensure!(address_backfilled(&database.app_pool, address_id).await?);
    ensure!(deposit_count(&database.app_pool).await? == 2);

    let completed_request_count = reader.request_count();
    let completed = scan_once(&database.app_pool, &reader, &routes).await?;
    ensure!(completed.inserted == 0);
    ensure!(
        reader.request_count() == completed_request_count,
        "a completed address was backfilled again"
    );
    Ok(())
}

/// The Base Sepolia staging incident of 2026-09-29, on its block numbers. The chain's first pass
/// walked its history from genesis, so the quote address, issued at the cursor the walk had
/// committed, was created 20 windows below the `finalized` head the pass then committed without
/// it. The head loop saw the payment, the chain stopped before the route's confirmation reached it
/// (then `safe`; now the depth 3 a payment reaches 4 s after inclusion), and after the
/// restart the head loop's window no longer covers it: only the finalized backstop can record it,
/// once the address's backfill completes, on a provider that refuses one request in five.
async fn run_stop_restart_scenario(database: &TestDatabase) -> Result<()> {
    const ISSUED_AT: u64 = 47_404_000;
    const STOPPED_AT: u64 = 47_444_900;
    const PAYMENT: u64 = 47_445_875;
    const RESTARTED_AT: u64 = 47_450_000;
    let pool = &database.app_pool;
    let routes = base_sepolia_routes()?;
    let token = BASE_SEPOLIA_PHA;
    let quote = Address::from([0xfa_u8; 20]);
    let reader = StagingReader::new(token, quote, PAYMENT, STOPPED_AT, 5);

    // 1. The chain starts scanning; 2. the quote address is issued at the committed cursor, and
    // the pass then commits through `finalized` without it.
    db::commit_scan(pool, BASE_SEPOLIA, &[], &[], Some(ISSUED_AT), None).await?;
    let customer_id = seed_account(pool).await?;
    let address_id = issue_base_sepolia_address(pool, customer_id, quote).await?;
    db::commit_scan(pool, BASE_SEPOLIA, &[], &[], Some(STOPPED_AT), None).await?;

    // 3. The head loop sees the payment two blocks deep, before the route's depth 3 reaches it.
    let seen = scan_new_blocks(
        pool,
        &reader,
        &routes,
        ChainHeads {
            latest: Some(PAYMENT + 1),
            safe: None,
            finalized: STOPPED_AT,
        },
    )
    .await?
    .context("the head loop scans the new blocks")?;
    ensure!(seen.inserted == 0 && seen.commit.seen == 1, "{seen:?}");
    ensure!(db::list_address_pending(pool, address_id).await?.len() == 1);

    // 4. The chain stops. 5. After the restart, the head loop reads only the last window.
    reader.set_finalized(RESTARTED_AT);
    let resumed = scan_new_blocks(
        pool,
        &reader,
        &routes,
        ChainHeads {
            latest: Some(RESTARTED_AT + 1_000),
            safe: None,
            finalized: RESTARTED_AT,
        },
    )
    .await?
    .context("the head loop scans after the restart")?;
    ensure!(resumed.from_block > PAYMENT && resumed.inserted == 0);

    // Each failed backstop pass keeps the windows it committed, so the retries complete.
    let mut passes = 0;
    while deposit_count(pool).await? == 0 {
        passes += 1;
        ensure!(
            passes <= 20,
            "the payment was never recorded: {:?}",
            reader.requests()
        );
        let _ = scan_once(pool, &reader, &routes).await;
    }
    ensure!(address_backfilled(pool, address_id).await?);
    ensure!(db::list_address_pending(pool, address_id).await?.is_empty());
    let recorded = db::get_deposit(
        pool,
        topup_core::identity::deposit_id(BASE_SEPOLIA, B256::from([0x4b_u8; 32]), 0),
    )
    .await?
    .context("the seen payment is the recorded deposit")?;
    ensure!(recorded.block_number == PAYMENT && recorded.address_id == address_id);
    // No backfill window the backstop committed is read again.
    let mut backfilled = reader
        .requests()
        .into_iter()
        .filter(|request| request.ok && request.to_block <= STOPPED_AT)
        .map(|request| request.from_block)
        .collect::<Vec<_>>();
    let reads = backfilled.len();
    backfilled.sort_unstable();
    backfilled.dedup();
    ensure!(
        backfilled.len() == reads,
        "a committed backfill window was read again: {:?}",
        reader.requests()
    );
    Ok(())
}

/// A chain added to a running service starts at its `finalized` head, so an address issued on it
/// is backfilled from there, not from genesis.
async fn run_new_chain_scenario(database: &TestDatabase) -> Result<()> {
    const ADDED_AT: u64 = 47_444_900;
    let pool = &database.app_pool;
    let routes = base_sepolia_routes()?;
    let quote = Address::from([0xfa_u8; 20]);
    let reader = StagingReader::new(BASE_SEPOLIA_PHA, quote, ADDED_AT + 975, ADDED_AT, 0);

    let started = initialize_cursor(pool, &reader, BASE_SEPOLIA).await?;
    ensure!(started == Some(ADDED_AT), "{started:?}");
    ensure!(db::get_cursor(pool, BASE_SEPOLIA).await? == Some(ADDED_AT));
    let customer_id = seed_account(pool).await?;
    let address_id = issue_base_sepolia_address(pool, customer_id, quote).await?;

    reader.set_finalized(ADDED_AT + 3_000);
    ensure!(
        initialize_cursor(pool, &reader, BASE_SEPOLIA)
            .await?
            .is_none()
    );
    ensure!(db::get_cursor(pool, BASE_SEPOLIA).await? == Some(ADDED_AT));
    let stats = scan_once(pool, &reader, &routes).await?;
    ensure!(stats.inserted == 1 && stats.backfilled_addresses == 1);
    ensure!(address_backfilled(pool, address_id).await?);
    let requests = reader.requests();
    ensure!(
        requests
            .iter()
            .all(|request| request.from_block >= ADDED_AT),
        "nothing below the chain's start is read: {requests:?}"
    );
    Ok(())
}

async fn run_sharding_scenario(database: &TestDatabase) -> Result<()> {
    let token = Address::from([0x66_u8; 20]);
    let account_id = seed_account(&database.app_pool).await?;
    insert_address(&database.app_pool, account_id, indexed_address(1), 1).await?;
    let lock_address = indexed_address(2);
    for index in 0..1_000_u64 {
        insert_lock_address(
            &database.app_pool,
            account_id,
            indexed_address(index.saturating_add(2)),
            index,
        )
        .await?;
    }
    sqlx::query("UPDATE addresses SET backfilled = true WHERE chain_id = $1")
        .bind(i64::try_from(CHAIN_ID)?)
        .execute(&database.app_pool)
        .await?;

    let route_fixture = RouteFixture::create(token)?;
    let routes = scanner_route(&route_fixture.path, Backstop::Addresses)?;
    let finalized_time = DateTime::from_timestamp(1_700_000_000, 0).context("finalized time")?;
    let reader = RecordingReader::new(4_001, finalized_time);
    let first = scan_once(&database.app_pool, &reader, &routes).await?;
    ensure!(
        first.cursor == 0,
        "cursor advanced before all address pages committed"
    );
    let stats = scan_once(&database.app_pool, &reader, &routes).await?;
    ensure!(stats.cursor == 4_001);
    let scanned_block_time: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT scanned_block_time FROM cursors WHERE chain_id = $1")
            .bind(i64::try_from(CHAIN_ID)?)
            .fetch_one(&database.app_pool)
            .await?;
    ensure!(
        scanned_block_time == Some(finalized_time),
        "cursor must record the finalized head's block time, got {scanned_block_time:?}"
    );

    let requests = reader.requests();
    ensure!(requests.len() == 6, "unexpected requests: {requests:?}");
    for request in &requests {
        ensure!(request.addresses.len() <= 1_000);
        ensure!(
            request
                .to_block
                .checked_sub(request.from_block)
                .and_then(|width| width.checked_add(1))
                .is_some_and(|width| width <= 2_000)
        );
    }
    ensure!(
        requests
            .iter()
            .any(|request| request.addresses.contains(&lock_address)),
        "lock address was omitted from the scanner filter"
    );
    let shapes = requests
        .iter()
        .map(|request| {
            (
                request.addresses.len(),
                request.from_block,
                request.to_block,
            )
        })
        .collect::<Vec<_>>();
    ensure!(
        shapes
            == vec![
                (1_000, 1, 2_000),
                (1_000, 2_001, 4_000),
                (1_000, 4_001, 4_001),
                (1, 1, 2_000),
                (1, 2_001, 4_000),
                (1, 4_001, 4_001),
            ],
        "unexpected request sharding: {shapes:?}"
    );

    // Token mode keeps each address page bounded and filters token-wide logs to that page.
    let token_routes = scanner_route(&route_fixture.path, Backstop::Token)?;
    let token_reader = RecordingReader::new(8_001, finalized_time);
    scan_once(&database.app_pool, &token_reader, &token_routes).await?;
    scan_once(&database.app_pool, &token_reader, &token_routes).await?;
    let shapes = token_reader
        .requests()
        .iter()
        .map(|request| {
            (
                request.addresses.len(),
                request.from_block,
                request.to_block,
            )
        })
        .collect::<Vec<_>>();
    ensure!(
        shapes
            == vec![
                (0, 4_002, 6_001),
                (0, 6_002, 8_001),
                (0, 4_002, 6_001),
                (0, 6_002, 8_001)
            ],
        "unexpected token-mode requests: {shapes:?}"
    );
    Ok(())
}

async fn run_confirm_scenario(
    database: &TestDatabase,
    primary_anvil: &Anvil,
    lagging_anvil: &Anvil,
) -> Result<()> {
    run_checked("forge", &["build"], Some(&contracts_dir()))?;
    let token = deploy_token(&primary_anvil.rpc_url)?;
    let tracked = Address::from([0x91_u8; 20]);
    let account_id = seed_account(&database.app_pool).await?;
    insert_address(&database.app_pool, account_id, tracked, 1).await?;
    transfer(&primary_anvil.rpc_url, token, tracked, 1_000)?;
    primary_anvil.mine(2)?;

    let fixture = RouteFixture::create(token)?;
    let scanner_routes = scanner_route(&fixture.path, Backstop::Token)?;
    let scanner_reader = reader(&primary_anvil.rpc_url)?;
    ensure!(
        scan_once(&database.app_pool, &scanner_reader, &scanner_routes)
            .await?
            .inserted
            == 1
    );

    let mut route: RouteFile = serde_saphyr::from_str(&std::fs::read_to_string(&fixture.path)?)?;
    route.asset.decimals = 2;
    route.asset.quote_amount_decimals = 2;
    route.merchant.min_amount = topup_core::route::Bounded::at(1);
    let now = u64::try_from(chrono::Utc::now().timestamp())?;
    let primary_price: Arc<dyn PriceSource> =
        Arc::new(FixedPrice(observation("kraken", 10_000_000, now)));
    let check_price: Arc<dyn PriceSource> =
        Arc::new(FixedPrice(observation("binance", 10_000_000, now)));
    let fx_price: Arc<dyn PriceSource> =
        Arc::new(FixedPrice(observation("kraken", 100_000_000, now)));
    let confirm = ConfirmStep::single(
        database.app_pool.clone(),
        route.clone(),
        reader(&primary_anvil.rpc_url)?,
        reader(&primary_anvil.rpc_url)?,
        Arc::clone(&primary_price),
        Some(Arc::clone(&check_price)),
        Some(Arc::clone(&fx_price)),
    );
    let pump = Pump::new(
        database.app_pool.clone(),
        Arc::default(),
        Arc::new(wait_steps().with_detected(Box::new(confirm))),
        PumpConfig::default(),
    )?;
    let confirmed_id: Uuid = sqlx::query_scalar("SELECT id FROM deposits LIMIT 1")
        .fetch_one(&database.app_pool)
        .await?;
    ensure!(
        pump.run_once().await?
            == RunOnceResult::Applied {
                deposit_id: confirmed_id
            }
    );
    let confirmed = db::get_deposit(&database.app_pool, confirmed_id)
        .await?
        .context("confirmed deposit")?;
    ensure!(confirmed.state == topup_core::deposit::DepositState::Confirmed);
    ensure!(confirmed.price_scaled == Some(10_000_000));
    ensure!(confirmed.credit_minor == Some(topup_core::money::MinorAmount::new(100)));
    ensure!(confirmed.quote.is_some());
    let evidence: Value =
        sqlx::query_scalar("SELECT evidence FROM transitions WHERE deposit_id = $1")
            .bind(confirmed_id)
            .fetch_one(&database.app_pool)
            .await?;
    ensure!(evidence["stage"] == "confirmed");
    // Confirmation announces nothing.
    let outbox_count: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE object_id = $1")
        .bind(confirmed_id)
        .fetch_one(&database.app_pool)
        .await?;
    ensure!(outbox_count == 0);

    transfer(&primary_anvil.rpc_url, token, tracked, 2_000)?;
    primary_anvil.mine(2)?;
    ensure!(
        scan_once(&database.app_pool, &scanner_reader, &scanner_routes)
            .await?
            .inserted
            == 1
    );
    let lagging_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM deposits WHERE state = 'detected' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&database.app_pool)
    .await?;
    let lagging_deposit = db::get_deposit(&database.app_pool, lagging_id)
        .await?
        .context("lagging deposit")?;
    let lagging_confirm = ConfirmStep::single(
        database.app_pool.clone(),
        route,
        reader(&primary_anvil.rpc_url)?,
        reader(&lagging_anvil.rpc_url)?,
        primary_price,
        Some(check_price),
        Some(fx_price),
    );
    let result = lagging_confirm.run(&lagging_deposit).await;
    ensure!(
        result.outcome
            == StepOutcome::Wait {
                reason: WaitReason::Finality
            }
    );
    Ok(())
}

struct FixedPrice(Observation);

#[async_trait]
impl PriceSource for FixedPrice {
    async fn observe(&self) -> Result<Observation, PriceError> {
        Ok(self.0.clone())
    }
}

fn observation(source: &str, price: u64, observed_at: u64) -> Observation {
    Observation {
        source: SourceId::new(source),
        price: ScaledPrice::new(price, PRICE_SCALE).expect("integration price"),
        observed_at: UnixSeconds::new(observed_at),
    }
}

fn deploy_token(rpc_url: &str) -> Result<Address> {
    forge_create(rpc_url, "test/mocks/MockTokens.sol:MockERC20", &[])
}

fn mint_nft(rpc_url: &str, token: Address, recipient: Address, token_id: u64) -> Result<()> {
    run_checked(
        "cast",
        &[
            "send",
            "--rpc-url",
            rpc_url,
            "--private-key",
            ANVIL_PRIVATE_KEY,
            &format!("{token:#x}"),
            "mint(address,uint256)",
            &format!("{recipient:#x}"),
            &token_id.to_string(),
        ],
        None,
    )?;
    Ok(())
}

fn transfer(rpc_url: &str, token: Address, recipient: Address, amount: u64) -> Result<()> {
    run_checked(
        "cast",
        &[
            "send",
            "--rpc-url",
            rpc_url,
            "--private-key",
            ANVIL_PRIVATE_KEY,
            &format!("{token:#x}"),
            "mint(address,uint256)",
            ANVIL_DEPLOYER,
            &amount.to_string(),
        ],
        None,
    )?;
    run_checked(
        "cast",
        &[
            "send",
            "--rpc-url",
            rpc_url,
            "--private-key",
            ANVIL_PRIVATE_KEY,
            &format!("{token:#x}"),
            "transfer(address,uint256)",
            &format!("{recipient:#x}"),
            &amount.to_string(),
        ],
        None,
    )?;
    Ok(())
}

fn current_block(rpc_url: &str) -> Result<u64> {
    let output = run_checked("cast", &["block-number", "--rpc-url", rpc_url], None)?;
    String::from_utf8(output.stdout)?
        .trim()
        .parse::<u64>()
        .context("parse anvil block number")
}

/// Seeds an account and one customer, and returns the customer id.
async fn seed_account(pool: &PgPool) -> Result<Uuid> {
    let (account, customer) = seed::create_account_and_customer(
        pool,
        &NewAccount {
            livemode: false,
            webhook_url: "https://product.test/webhooks".to_owned(),
            ..NewAccount::named("scanner-test")
        },
        "workspace-scanner",
    )
    .await?;
    seed::accept_assets(pool, account.id, false, CHAIN_ID, &["pha"]).await?;
    Ok(customer.id)
}

async fn insert_address(
    pool: &PgPool,
    customer_id: Uuid,
    address: Address,
    version: u64,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    seed::insert_address(
        pool,
        &NewAddress {
            id,
            customer_id,
            chain_id: CHAIN_ID,
            route: "phala-cloud-ethereum-pha-usd".to_owned(),
            salt: B256::from([u8::try_from(version)?; 32]),
            address,
        },
    )
    .await?;
    Ok(id)
}

async fn insert_lock_address(
    pool: &PgPool,
    customer_id: Uuid,
    address: Address,
    index: u64,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    seed::insert_address(
        pool,
        &NewAddress {
            id,
            customer_id,
            chain_id: CHAIN_ID,
            route: "phala-cloud-ethereum-pha-usd".to_owned(),
            salt: indexed_word(index),
            address,
        },
    )
    .await?;
    Ok(id)
}

fn indexed_address(index: u64) -> Address {
    Address::from_word(indexed_word(index))
}

fn indexed_word(index: u64) -> B256 {
    let mut bytes = [0_u8; 32];
    bytes[24..].copy_from_slice(&index.to_be_bytes());
    B256::from(bytes)
}

fn mock_transfer_log(
    token: Address,
    recipient: Address,
    block_number: u64,
    marker: u8,
) -> TransferLog {
    TransferLog {
        tx_hash: B256::from([marker; 32]),
        log_index: 0,
        receipt_log_index: 0,
        tx_from: alloy_primitives::Address::ZERO,
        tx_nonce: 0,
        block_number,
        block_hash: B256::from([marker.saturating_add(10); 32]),
        block_time: DateTime::from_timestamp(i64::from(marker), 0).expect("test timestamp"),
        token,
        from: Address::from([0x77_u8; 20]),
        to: recipient,
        amount: AtomicAmount::new(U256::from(u64::from(marker))),
    }
}

async fn address_backfilled(pool: &PgPool, id: Uuid) -> Result<bool> {
    Ok(
        sqlx::query("SELECT backfilled FROM addresses WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?
            .try_get(0)?,
    )
}

async fn deposit_count(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query("SELECT count(*) FROM deposits")
        .fetch_one(pool)
        .await?
        .try_get(0)?)
}

async fn assert_deposit_counts(pool: &PgPool, total: i64, unsupported: i64) -> Result<()> {
    let total_count: i64 = sqlx::query("SELECT count(*) FROM deposits")
        .fetch_one(pool)
        .await?
        .try_get(0)?;
    let unsupported_count: i64 = sqlx::query(
        "SELECT count(*) FROM deposits WHERE state = 'rejected' AND reason = 'unsupported_asset' AND route IS NULL AND route_version IS NULL",
    )
    .fetch_one(pool)
    .await?
    .try_get(0)?;
    ensure!(
        total_count == total,
        "expected {total} deposits, got {total_count}"
    );
    ensure!(
        unsupported_count == unsupported,
        "expected {unsupported} unsupported deposits, got {unsupported_count}"
    );
    Ok(())
}

async fn install_insert_failure(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(
        r#"
        CREATE FUNCTION fail_c3_deposit_insert()
        RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            RAISE EXCEPTION 'forced scanner insert failure';
        END;
        $$;
        CREATE TRIGGER fail_c3_deposit_insert
        BEFORE INSERT ON deposits
        FOR EACH ROW EXECUTE FUNCTION fail_c3_deposit_insert();
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn remove_insert_failure(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(
        r#"
        DROP TRIGGER fail_c3_deposit_insert ON deposits;
        DROP FUNCTION fail_c3_deposit_insert();
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Leaves every deposit waiting so only the step under test advances state.
struct WaitStep;

#[async_trait]
impl Step for WaitStep {
    async fn run(&self, _deposit: &db::Deposit) -> StepResult {
        StepResult::new(
            StepOutcome::Wait {
                reason: WaitReason::Paused,
            },
            serde_json::json!({"outcome": "wait"}),
        )
    }
}

fn wait_steps() -> StepSet {
    StepSet::new(Box::new(WaitStep), Box::new(WaitStep))
}

fn reader(rpc_url: &str) -> Result<FinalizedReader> {
    Ok(FinalizedReader::new(Arc::new(EvmClient::new(rpc_url)?)))
}
