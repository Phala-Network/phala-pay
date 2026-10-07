//! Anvil and PostgreSQL coverage for the display-only pending view (architecture §8, §12): the
//! head scan, reorg removal, the finalized hand-off, and the quote `payment` object. The head scan
//! writes no events.

mod support;

use std::str::FromStr;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::body::to_bytes;
use axum::http::{Method, StatusCode};
use chrono::{DateTime, Utc};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use topup::api::{AppState, PublicOrigin, VerificationKey};
use topup::db::NewPendingTransfer;
use topup::locks::QuoteProvider;
use topup::locks::pricing::ValidatedQuote;
use topup::routes::RouteSet;
use topup::scanner::{chain_routes, coverage_once, fast_once};
use topup_adapters::attestation::DstackAttestor;
use topup_adapters::chain::evm::{EvmClient, FinalizedReader};
use topup_core::money::{AtomicAmount, PRICE_SCALE, ScaledPrice};
use topup_core::route::RouteFile;
use tower::ServiceExt;
use uuid::Uuid;

use support::chain::{ANVIL_PRIVATE_KEY, Anvil, CHAIN_ID, forge_create, run_checked};
use support::seed::{self, NewAccount, NewAddress};
use support::{TEST_ORIGIN, TestDatabase, merchant_request, public_key_base64, with_database};

const ANVIL_DEPLOYER: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
/// With 32-slot epochs anvil reports `finalized = latest - 64`.
const FINALITY_LAG: u64 = 64;

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
            evidence: json!({"mode": "spot"}),
        })
    }
}

#[tokio::test]
async fn pending_transfers_are_display_only_until_final() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            // 32-slot epochs keep transfers above `finalized` for 64 blocks.
            let Some(anvil) = Anvil::start_if_available(&["--slots-in-an-epoch", "32"]).await?
            else {
                return Ok(());
            };
            run_scenario(database, &anvil).await
        })
    })
    .await
}

async fn run_scenario(database: &TestDatabase, anvil: &Anvil) -> Result<()> {
    let pool = &database.app_pool;
    let token = forge_create(&anvil.rpc_url, MOCK_ERC20, &[])?;
    let other_token = forge_create(&anvil.rpc_url, MOCK_ERC20, &[])?;
    anvil.mine(FINALITY_LAG + 2)?;

    let route = test_route(token);
    let route_set = Arc::new(RouteSet::new(vec![route.clone()]).map_err(anyhow::Error::msg)?);
    let chain_routes = chain_routes(&route_set)
        .into_iter()
        .next()
        .context("one chain")?;
    let admin_key = SigningKey::from_bytes(&[62; 32]);
    let product_key = seed_account(pool).await?;
    let app = topup::api::router(AppState {
        pool: pool.clone(),
        routes: Arc::clone(&route_set),
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
        screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
        contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
    })
    .0;
    let api = Api {
        app,
        key: product_key,
        quotes: std::sync::Mutex::default(),
    };
    let reader = FinalizedReader::new(Arc::new(EvmClient::new(&anvil.rpc_url)?));
    coverage_once(pool, &reader, &reader, &chain_routes, 1).await?;

    let lock_address = api.lock("checkout-1").await?;
    ensure!(
        api.payment("checkout-1").await?.is_null(),
        "unpaid quote has a payment"
    );
    ensure!(api.client_progress("checkout-1").await? == ("none".to_owned(), None));
    let other_quote = api.lock("checkout-0").await?;
    let before = Ledger::read(pool).await?;

    let snapshot = snapshot(anvil)?;
    transfer(&anvil.rpc_url, token, lock_address, 100)?;
    transfer(&anvil.rpc_url, other_token, other_quote, 5)?;
    transfer(&anvil.rpc_url, token, other_quote, 7)?;
    transfer(&anvil.rpc_url, token, other_quote, 0)?;

    fast_once(pool, &reader, &chain_routes).await?;
    // Only non-zero transfers of the routed token are requested and stored.
    ensure!(pending_rows(pool).await? == 2);
    ensure!(
        outbox_rows(pool).await? == 0,
        "the head scan wrote an event"
    );
    ensure!(
        Ledger::read(pool).await? == before,
        "a pending transfer changed deposits, locks, exposure, or transitions"
    );

    let lock = api.quote("checkout-1").await?;
    ensure!(
        lock["status"] == "open",
        "pending payment consumed the lock"
    );
    let payment = &lock["payment"];
    ensure!(payment["status"] == "seen", "unexpected payment {payment}");
    ensure!(payment["amount_atomic"] == "100");
    ensure!(payment["matches_quote"] == true);
    ensure!(
        payment["deposit"]
            .as_str()
            .is_some_and(|deposit| deposit.starts_with("dep_")),
        "{payment}"
    );
    ensure!(
        payment["confirmations"]
            .as_u64()
            .is_some_and(|value| value >= 1)
    );
    let (progress, confirmations) = api.client_progress("checkout-1").await?;
    ensure!(progress == "seen" && confirmations == payment["confirmations"].as_u64());
    let block_time = pending_block_time(pool, payment["tx_hash"].as_str().context("hash")?).await?;
    ensure!(
        payment["estimated_final_at"].as_i64()
            == Some((block_time + chrono::TimeDelta::minutes(15)).timestamp()),
        "estimated_final_at is not block time plus 15 minutes: {payment}"
    );

    // The routed token's non-zero transfer is shown, not matching its quote; the other token
    // and the zero transfer are not seen at all.
    let other = api.payment("checkout-0").await?;
    ensure!(
        other["amount_atomic"] == "7" && other["matches_quote"] == false,
        "{other}"
    );

    let _again = fast_once(pool, &reader, &chain_routes).await?;
    ensure!(pending_rows(pool).await? == 2);

    // A reorg that drops the transfers removes them from the pending view.
    revert(anvil, &snapshot)?;
    anvil.mine(8)?;
    let reorged = fast_once(pool, &reader, &chain_routes).await?;
    ensure!(
        pending_rows(pool).await? == 0,
        "unexpected reorg scan {reorged:?}"
    );
    ensure!(pending_rows(pool).await? == 0);
    ensure!(
        api.payment("checkout-1").await?.is_null(),
        "reorged payment still shown"
    );

    // Once final, the finalized scanner records the deposit and clears its pending row in the same
    // transaction, and the lock shows the finalized payment.
    transfer(&anvil.rpc_url, token, lock_address, 100)?;
    fast_once(pool, &reader, &chain_routes).await?;
    ensure!(pending_rows(pool).await? == 1);
    ensure!(
        outbox_rows(pool).await? == 0,
        "the head scan wrote an event"
    );
    anvil.mine(FINALITY_LAG)?;
    let finalized = coverage_once(pool, &reader, &reader, &chain_routes, 1).await?;
    ensure!(
        finalized.inserted == 1,
        "unexpected finalized scan {finalized:?}"
    );
    ensure!(pending_rows(pool).await? == 0, "finalized row was kept");
    let payment = api.payment("checkout-1").await?;
    ensure!(payment["status"] == "recorded", "unexpected {payment}");
    ensure!(payment["confirmations"].is_null());
    // Final but not yet valued and screened.
    ensure!(api.client_progress("checkout-1").await? == ("confirming".to_owned(), None));
    // A deposit reversed before finality is no payment; the page says it was reversed rather
    // than asking for a payment again.
    sqlx::query("UPDATE deposits SET state = 'reversed' WHERE address_id = $1")
        .bind(api.address_id(pool, "checkout-1").await?)
        .execute(pool)
        .await?;
    ensure!(api.client_progress("checkout-1").await? == ("reversed".to_owned(), None));

    // Underpay, then pay in full: the finalized underpayment does not consume the lock, so the
    // later exact payment is the one shown while it is still pending.
    let underpaid = api.lock("checkout-2").await?;
    transfer(&anvil.rpc_url, token, underpaid, 50)?;
    anvil.mine(FINALITY_LAG)?;
    coverage_once(pool, &reader, &reader, &chain_routes, 1).await?;
    transfer(&anvil.rpc_url, token, underpaid, 100)?;
    // Dust first, then the exact amount, both still pending.
    let dusted = api.lock("checkout-3").await?;
    transfer(&anvil.rpc_url, token, dusted, 1)?;
    transfer(&anvil.rpc_url, token, dusted, 100)?;
    fast_once(pool, &reader, &chain_routes).await?;
    for lock_ref in ["checkout-2", "checkout-3"] {
        let payment = api.payment(lock_ref).await?;
        ensure!(
            payment["status"] == "seen"
                && payment["amount_atomic"] == "100"
                && payment["matches_quote"] == true,
            "{lock_ref} does not show the payment that consumes it: {payment}"
        );
    }

    // Once a deposit consumed the lock, that deposit is shown, whatever else arrived. The
    // underpayment is marked as the consumer by hand: an artificial state (the pump would consume
    // with the exact payment) used only to show the consuming deposit wins over the first
    // qualifying one.
    anvil.mine(FINALITY_LAG)?;
    coverage_once(pool, &reader, &reader, &chain_routes, 1).await?;
    let underpayment: Uuid =
        sqlx::query_scalar("SELECT id FROM deposits WHERE address_id = $1 AND amount_atomic = 50")
            .bind(api.address_id(pool, "checkout-2").await?)
            .fetch_one(pool)
            .await?;
    sqlx::query(
        "UPDATE quotes SET status = 'consumed', consumed_by = $1, exposure_reserved = false, \
         closed_at = now() WHERE id = (SELECT address.quote_id FROM deposits AS deposit \
         JOIN addresses AS address ON address.id = deposit.address_id WHERE deposit.id = $1)",
    )
    .bind(underpayment)
    .execute(pool)
    .await?;
    let payment = api.payment("checkout-2").await?;
    ensure!(
        payment["status"] == "recorded"
            && payment["deposit"] == topup::ids::format(topup::ids::DEPOSIT, underpayment),
        "consumed lock does not show its consuming deposit: {payment}"
    );
    // Each side expands the other.
    let deposit_id = topup::ids::format(topup::ids::DEPOSIT, underpayment);
    let quote_id = api.id("checkout-2")?;
    let quote = api
        .call(
            Method::GET,
            &format!("/v1/quotes/{quote_id}?expand[]=deposit"),
            Value::Null,
        )
        .await?;
    ensure!(quote["status"] == "complete", "{quote}");
    ensure!(
        quote["deposit"]["id"] == deposit_id && quote["deposit"]["quote"] == quote_id,
        "{quote}"
    );
    let deposit = api
        .call(
            Method::GET,
            &format!("/v1/deposits/{deposit_id}?expand[]=quote"),
            Value::Null,
        )
        .await?;
    ensure!(
        deposit["quote"]["id"] == quote_id && deposit["quote"]["deposit"] == deposit_id,
        "{deposit}"
    );
    ensure!(deposit["client_reference_id"] == "ws-pending" && deposit["asset"] == "pha");

    // A cancelled lock credits every payment at spot, so none is in time or within tolerance.
    let cancelled = api.lock("checkout-4").await?;
    let id = api.id("checkout-4")?;
    api.call(
        Method::POST,
        &format!("/v1/quotes/{id}/cancel"),
        Value::Null,
    )
    .await?;
    rpc(anvil, "evm_increaseTime", &["2"])?;
    transfer(&anvil.rpc_url, token, cancelled, 100)?;
    anvil.mine(FINALITY_LAG)?;
    coverage_once(pool, &reader, &reader, &chain_routes, 1).await?;
    topup::locks::expire_once(pool, &route_set).await?;
    let lock = api.quote("checkout-4").await?;
    ensure!(lock["status"] == "canceled", "unexpected {lock}");
    ensure!(
        lock["payment"]["amount_atomic"] == "100" && lock["payment"]["matches_quote"] == false,
        "canceled quote shows its payment as applying: {lock}"
    );
    Ok(())
}

#[tokio::test]
async fn a_lagging_head_leaves_pending_rows_above_it() -> Result<()> {
    with_database(|database| Box::pin(run_lagging_head_scenario(&database.app_pool))).await
}

async fn run_lagging_head_scenario(pool: &sqlx::PgPool) -> Result<()> {
    let (account, customer) = seed::create_account_and_customer(
        pool,
        &NewAccount {
            livemode: false,
            ..NewAccount::named("watch")
        },
        "ws-0",
    )
    .await?;
    seed::accept_assets(pool, account.id, false, CHAIN_ID, &["pha"]).await?;
    let address_id = Uuid::new_v4();
    seed::insert_address(
        pool,
        &NewAddress {
            id: address_id,
            customer_id: customer.id,
            chain_id: CHAIN_ID,
            route: "r".to_owned(),
            salt: B256::ZERO,
            address: Address::ZERO,
        },
    )
    .await?;

    // A head scan from a provider that lags behind a stored row leaves the row alone; the next
    // scan whose range covers it and does not see it removes it.
    let row = NewPendingTransfer {
        chain_id: CHAIN_ID,
        tx_hash: B256::repeat_byte(0x51),
        log_index: 0,
        receipt_log_index: 0,
        block_number: 50,
        block_hash: B256::repeat_byte(0x52),
        block_time: Utc::now(),
        address_id,
        asset_contract: Address::repeat_byte(0x53),
        from_address: Address::repeat_byte(0x54),
        amount_atomic: AtomicAmount::new(U256::from(1_u64)),
    };
    topup::db::commit_head_scan(pool, CHAIN_ID, 10, 60, std::slice::from_ref(&row)).await?;
    let lagging = topup::db::commit_head_scan(pool, CHAIN_ID, 10, 40, &[]).await?;
    ensure!(
        lagging.removed == 0,
        "a lagging head deleted a row above it"
    );
    let covering = topup::db::commit_head_scan(pool, CHAIN_ID, 10, 60, &[]).await?;
    ensure!(covering.removed == 1, "an unseen row in range was kept");
    Ok(())
}

/// Every table a pending row must never touch.
#[derive(Debug, Eq, PartialEq)]
struct Ledger {
    deposits: i64,
    transitions: i64,
    locks: Vec<(String, Option<Uuid>, bool)>,
}

impl Ledger {
    async fn read(pool: &sqlx::PgPool) -> Result<Self> {
        Ok(Self {
            deposits: sqlx::query_scalar("SELECT count(*) FROM deposits")
                .fetch_one(pool)
                .await?,
            transitions: sqlx::query_scalar("SELECT count(*) FROM transitions")
                .fetch_one(pool)
                .await?,
            locks: sqlx::query_as(
                "SELECT status, consumed_by, exposure_reserved FROM quotes ORDER BY id",
            )
            .fetch_all(pool)
            .await?,
        })
    }
}

struct Api {
    app: axum::Router,
    /// The account's test secret key.
    key: String,
    /// Quote ids and client secrets by the test's name for them.
    quotes: std::sync::Mutex<std::collections::BTreeMap<String, (String, String)>>,
}

impl Api {
    /// Creates a 100-cent quote for `ws-pending` and returns its address.
    async fn lock(&self, name: &str) -> Result<Address> {
        let quote = self
            .call(
                Method::POST,
                "/v1/quotes",
                json!({"client_reference_id": "ws-pending", "amount": 100, "currency": "usd",
                       "chain_id": CHAIN_ID, "asset": "pha"}),
            )
            .await?;
        let id = quote["id"].as_str().context("quote id")?.to_owned();
        let secret = quote["client_secret"]
            .as_str()
            .context("client secret")?
            .to_owned();
        self.quotes
            .lock()
            .map_err(|_| anyhow::anyhow!("quote map poisoned"))?
            .insert(name.to_owned(), (id, secret));
        Ok(Address::from_str(
            quote["address"].as_str().context("quote address")?,
        )?)
    }

    fn id(&self, name: &str) -> Result<String> {
        Ok(self.quote_entry(name)?.0)
    }

    fn quote_entry(&self, name: &str) -> Result<(String, String)> {
        self.quotes
            .lock()
            .map_err(|_| anyhow::anyhow!("quote map poisoned"))?
            .get(name)
            .cloned()
            .context("unknown quote")
    }

    /// The payment progress the payer's page reads with the client secret.
    async fn client_progress(&self, name: &str) -> Result<(String, Option<u64>)> {
        let (id, secret) = self.quote_entry(name)?;
        let response = self
            .app
            .clone()
            .oneshot(
                axum::http::Request::get(format!("/v1/quotes/{id}?client_secret={secret}"))
                    .body(axum::body::Body::empty())?,
            )
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let quote: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1_048_576).await?)?;
        Ok((
            quote["payment_status"]
                .as_str()
                .context("payment_status")?
                .to_owned(),
            quote["confirmations"].as_u64(),
        ))
    }

    async fn address_id(&self, pool: &sqlx::PgPool, name: &str) -> Result<Uuid> {
        let quote = topup::ids::parse(topup::ids::QUOTE, &self.id(name)?).context("quote id")?;
        Ok(
            sqlx::query_scalar("SELECT id FROM addresses WHERE quote_id = $1")
                .bind(quote)
                .fetch_one(pool)
                .await?,
        )
    }

    async fn quote(&self, name: &str) -> Result<Value> {
        let id = self.id(name)?;
        self.call(Method::GET, &format!("/v1/quotes/{id}"), Value::Null)
            .await
    }

    async fn payment(&self, name: &str) -> Result<Value> {
        Ok(self.quote(name).await?["payment"].clone())
    }

    async fn call(&self, method: Method, path: &str, body: Value) -> Result<Value> {
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body)?
        };
        let response = self
            .app
            .clone()
            .oneshot(merchant_request(method, path, body, &self.key))
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_048_576).await?;
        ensure!(
            status == StatusCode::OK,
            "{path} returned {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        Ok(serde_json::from_slice(&bytes)?)
    }
}

async fn pending_rows(pool: &sqlx::PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM pending_transfers")
        .fetch_one(pool)
        .await?)
}

async fn outbox_rows(pool: &sqlx::PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(pool)
        .await?)
}

async fn pending_block_time(pool: &sqlx::PgPool, tx_hash: &str) -> Result<DateTime<Utc>> {
    Ok(
        sqlx::query_scalar("SELECT block_time FROM pending_transfers WHERE tx_hash = $1")
            .bind(tx_hash)
            .fetch_one(pool)
            .await?,
    )
}

/// Seeds a test-mode account and its customer `ws-pending`; returns the account's test key.
async fn seed_account(pool: &sqlx::PgPool) -> Result<String> {
    let (account, _) = seed::create_account_and_customer(
        pool,
        &NewAccount {
            livemode: false,
            webhook_url: "https://product.test/webhooks".to_owned(),
            ..NewAccount::named("phala-cloud")
        },
        "ws-pending",
    )
    .await?;
    seed::set_treasury(pool, account.id, false, CHAIN_ID, seed::FIXTURE_TREASURY).await?;
    seed::accept_assets(pool, account.id, false, CHAIN_ID, &["pha"]).await?;
    Ok(seed::create_api_key(pool, account.id, false).await?)
}

fn test_route(token: Address) -> RouteFile {
    let yaml = include_str!("fixtures/phala-cloud-pha.yaml")
        .replace("chain_id: 1", &format!("chain_id: {CHAIN_ID}"))
        .replace("livemode: true", "livemode: false")
        .replace(
            "0x6c5bA91642F10282b576d91922Ae6448C9d52f4E",
            &format!("{token:#x}"),
        );
    let mut route: RouteFile = serde_saphyr::from_str(&yaml).expect("route fixture");
    route.asset.decimals = 2;
    route.asset.quote_amount_decimals = 2;
    route.merchant.quote_spread_bps =
        topup_core::route::Bounded::at(topup_core::money::Bps::new(0).expect("zero bps"));
    route.merchant.min_deposit_atomic =
        topup_core::route::Bounded::at(AtomicAmount::new(U256::from(1_u64)));
    route.merchant.min_amount = topup_core::route::Bounded::at(1);
    route
}

const MOCK_ERC20: &str = "test/mocks/MockTokens.sol:MockERC20";

fn rpc(anvil: &Anvil, method: &str, params: &[&str]) -> Result<String> {
    let mut arguments = vec!["rpc", "--rpc-url", &anvil.rpc_url, method];
    arguments.extend_from_slice(params);
    let output = run_checked("cast", &arguments, None)?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn snapshot(anvil: &Anvil) -> Result<String> {
    Ok(rpc(anvil, "evm_snapshot", &[])?
        .trim_matches('"')
        .to_owned())
}

fn revert(anvil: &Anvil, snapshot: &str) -> Result<()> {
    ensure!(
        rpc(anvil, "evm_revert", &[snapshot])? == "true",
        "revert failed"
    );
    Ok(())
}

fn transfer(rpc_url: &str, token: Address, recipient: Address, amount: u64) -> Result<()> {
    for (signature, target) in [
        ("mint(address,uint256)", ANVIL_DEPLOYER.to_owned()),
        ("transfer(address,uint256)", format!("{recipient:#x}")),
    ] {
        run_checked(
            "cast",
            &[
                "send",
                "--rpc-url",
                rpc_url,
                "--private-key",
                ANVIL_PRIVATE_KEY,
                &format!("{token:#x}"),
                signature,
                &target,
                &amount.to_string(),
            ],
            None,
        )?;
    }
    Ok(())
}
