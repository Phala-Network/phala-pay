//! The balance (`GET /v1/balance`), the sweeps (`GET /v1/sweeps`), and the forwarders
//! (`GET /v1/forwarders`) of docs/design/multi-tenant.md D4 and §13: balances come from deposits
//! not reversed minus finalized `Flushed` events, a sweep is such an event, a deposit is `swept`
//! once a sweep after it moved its forwarder's balance, and `sweepable` never lists a forwarder
//! holding a sanctioned deposit or paying a sanctioned treasury. The operator's deposit view is
//! the merchant's with `admin` internals.

mod support;

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use axum::Router;
use axum::body::to_bytes;
use axum::http::{Method, StatusCode};
use chrono::Utc;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use topup::api::{AppState, PublicOrigin, VerificationKey};
use topup::db::{self, NewDeposit};
use topup::refunds::{DestinationScreener, DestinationScreening};
use topup_adapters::attestation::DstackAttestor;
use topup_core::deposit::{DepositState, RejectReason};
use topup_core::money::AtomicAmount;
use topup_core::route::RouteFile;
use tower::ServiceExt;
use uuid::Uuid;

use support::seed::{self, NewAccount, NewAddress};
use support::{TEST_ORIGIN, merchant_request, public_key_base64, signed_request, with_database};

const ADMIN_KEY: [u8; 32] = [49; 32];

#[tokio::test]
async fn balance_sweeps_and_sweepable_forwarders_follow_final_deposits_and_flushes() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let token = format!("{:#x}", fixture.route.asset.contract);
            // a: 100 final, 30 swept. b: 50 final, 20 not final yet. c: 10 final beside a
            // sanctioned deposit of 5. d: reversed.
            let a = fixture.address(1).await?;
            let b = fixture.address(2).await?;
            let c = fixture.address(3).await?;
            let d = fixture.address(4).await?;
            fixture
                .deposit(&a, 1, 100, DepositState::Credited, None, true)
                .await?;
            fixture.flushed(&a, 30).await?;
            fixture
                .deposit(&b, 2, 50, DepositState::Credited, None, true)
                .await?;
            fixture
                .deposit(&b, 3, 20, DepositState::Credited, None, false)
                .await?;
            fixture
                .deposit(&c, 4, 10, DepositState::Credited, None, true)
                .await?;
            fixture
                .deposit(
                    &c,
                    5,
                    5,
                    DepositState::Rejected,
                    Some(RejectReason::Sanctioned),
                    true,
                )
                .await?;
            fixture
                .deposit(&d, 6, 40, DepositState::Reversed, None, true)
                .await?;

            let (status, balance) = fixture.get("/v1/balance", Clear).await?;
            ensure!(status == StatusCode::OK, "{balance}");
            ensure!(
                balance
                    == json!({
                        "object": "balance", "livemode": true,
                        "unswept": [{
                            "chain_id": 1, "token": token, "asset": "pha",
                            "amount_atomic": "155", "final_amount_atomic": "135",
                        }],
                    }),
                "{balance}"
            );
            let (_, test_mode) = fixture
                .get_with("/v1/balance", Clear, &fixture.test_key)
                .await?;
            ensure!(test_mode["unswept"] == json!([]));

            // The sweep is the finalized Flushed event, and it marks the deposit before it swept.
            let (status, sweeps) = fixture.get("/v1/sweeps", Clear).await?;
            ensure!(status == StatusCode::OK, "{sweeps}");
            let sweep = &sweeps["data"][0];
            ensure!(
                sweeps["data"].as_array().map(Vec::len) == Some(1),
                "{sweeps}"
            );
            ensure!(sweep["object"] == "sweep" && sweep["amount_atomic"] == "30");
            ensure!(sweep["id"].as_str().is_some_and(|id| id.starts_with("sw_")));
            ensure!(sweep["forwarder"] == format!("fwd_{}", a.id.simple()));
            ensure!(sweep["address"] == format!("{:#x}", a.address));
            ensure!(sweep["token"] == token && sweep["asset"] == "pha");
            ensure!(sweep["treasury"] == format!("{:#x}", seed::FIXTURE_TREASURY));
            for (path, count) in [
                (format!("/v1/sweeps?forwarder=fwd_{}", b.id.simple()), 0),
                ("/v1/sweeps?chain_id=10".to_owned(), 0),
                (format!("/v1/sweeps?token={token}"), 1),
            ] {
                let (_, page) = fixture.get(&path, Clear).await?;
                ensure!(
                    page["data"].as_array().map(Vec::len) == Some(count),
                    "{path}"
                );
            }
            let (_, deposits) = fixture.get("/v1/deposits", Clear).await?;
            for deposit in deposits["data"].as_array().context("data")? {
                let swept = deposit["address"] == format!("{:#x}", a.address);
                ensure!(deposit["swept"] == swept, "{deposit}");
            }

            // Sweepable forwarders: final unswept, never sanctioned, only to a clear treasury.
            let sweepable = format!("/v1/forwarders?sweepable={token}");
            let (status, page) = fixture.get(&sweepable, Clear).await?;
            ensure!(status == StatusCode::OK, "{page}");
            let mut listed: Vec<&str> = page["data"]
                .as_array()
                .context("data")?
                .iter()
                .filter_map(|forwarder| forwarder["id"].as_str())
                .collect();
            listed.sort_unstable();
            let mut expected = vec![
                format!("fwd_{}", a.id.simple()),
                format!("fwd_{}", b.id.simple()),
            ];
            expected.sort();
            ensure!(listed == expected, "{page}");
            let (_, sanctioned) = fixture.get(&sweepable, Sanctioned).await?;
            ensure!(sanctioned["data"] == json!([]));
            let (status, _) = fixture.get(&sweepable, Unavailable).await?;
            ensure!(status == StatusCode::SERVICE_UNAVAILABLE);
            let (status, _) = fixture.get("/v1/forwarders?sweepable=nope", Clear).await?;
            ensure!(status == StatusCode::BAD_REQUEST);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn the_forwarder_export_names_every_derivation_and_pages() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let mut issued = Vec::new();
            for number in 1..=3 {
                issued.push(fixture.address(number).await?);
            }
            let mut seen = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let path = match &cursor {
                    Some(id) => format!("/v1/forwarders?limit=2&starting_after={id}"),
                    None => "/v1/forwarders?limit=2".to_owned(),
                };
                let (status, page) = fixture.get(&path, Clear).await?;
                ensure!(status == StatusCode::OK, "{page}");
                let data = page["data"].as_array().context("data")?;
                seen.extend(data.iter().cloned());
                cursor = data
                    .last()
                    .and_then(|last| last["id"].as_str())
                    .map(str::to_owned);
                if page["has_more"] != true {
                    break;
                }
            }
            ensure!(seen.len() == 3, "{seen:?}");
            let factory = format!("{:#x}", fixture.route.chain.contracts.forwarder_factory);
            for forwarder in &seen {
                let matching = issued
                    .iter()
                    .find(|issued| forwarder["id"] == format!("fwd_{}", issued.id.simple()))
                    .context("an issued forwarder")?;
                ensure!(
                    *forwarder
                        == json!({
                            "id": forwarder["id"], "object": "forwarder", "livemode": true,
                            "chain_id": 1, "address": format!("{:#x}", matching.address),
                            "factory": factory, "salt": format!("{:#x}", matching.salt),
                            "treasury": format!("{:#x}", seed::FIXTURE_TREASURY),
                            "quote": forwarder["quote"], "deposit_address": null,
                            "superseded_at": null,
                        }),
                    "{forwarder}"
                );
                ensure!(
                    forwarder["quote"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("qt_"))
                );
            }
            // Pages do not overlap, and a page before a cursor comes back in order.
            let mut ids: Vec<&str> = seen.iter().filter_map(|a| a["id"].as_str()).collect();
            let last = ids.last().copied().context("last")?.to_owned();
            ids.dedup();
            ensure!(ids.len() == 3);
            let (_, before) = fixture
                .get(&format!("/v1/forwarders?ending_before={last}"), Clear)
                .await?;
            let before: Vec<&str> = before["data"]
                .as_array()
                .context("data")?
                .iter()
                .filter_map(|a| a["id"].as_str())
                .collect();
            ensure!(before == ids[..2], "{before:?}");
            let (_, test_mode) = fixture
                .get_with("/v1/forwarders", Clear, &fixture.test_key)
                .await?;
            ensure!(test_mode["data"] == json!([]));
            let (status, _) = fixture.get("/v1/forwarders?limit=0", Clear).await?;
            ensure!(status == StatusCode::BAD_REQUEST);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn the_operator_reads_a_deposit_as_its_account_does_with_admin_internals() -> Result<()> {
    with_database(|database| {
        Box::pin(async move {
            let pool = &database.app_pool;
            let fixture = Fixture::new(pool).await?;
            let a = fixture.address(1).await?;
            fixture
                .deposit(&a, 1, 100, DepositState::Detected, None, false)
                .await?;
            let (_, deposits) = fixture.get("/v1/deposits", Clear).await?;
            let merchant = deposits["data"][0].clone();
            ensure!(merchant["status"] == "pending" && merchant.get("admin").is_none());
            let id = merchant["id"].as_str().context("id")?;
            let response = fixture
                .app(Arc::new(Clear))?
                .oneshot(signed_request(
                    Method::GET,
                    &format!("/v1/admin/deposits/{id}"),
                    Vec::new(),
                    "admin/v1",
                    &SigningKey::from_bytes(&ADMIN_KEY),
                    Utc::now().timestamp(),
                ))
                .await?;
            ensure!(response.status() == StatusCode::OK);
            let mut operator: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1_048_576).await?)?;
            let admin = operator
                .as_object_mut()
                .and_then(|object| object.remove("admin"))
                .context("admin")?;
            ensure!(operator == merchant, "{operator} != {merchant}");
            ensure!(admin["state"] == "detected" && admin["route"] == fixture.route.route);
            ensure!(
                admin["account"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("acct_"))
            );
            ensure!(admin["transitions"] == json!([]) && admin["events"] == json!([]));
            Ok(())
        })
    })
    .await
}

struct Fixture {
    pool: sqlx::PgPool,
    route: RouteFile,
    customer: Uuid,
    live_key: String,
    test_key: String,
}

struct Issued {
    id: Uuid,
    address: Address,
    salt: B256,
}

impl Fixture {
    async fn new(pool: &sqlx::PgPool) -> Result<Self> {
        let (account, customer) =
            seed::create_account_and_customer(pool, &NewAccount::named("merchant"), "team-42")
                .await?;
        seed::set_treasury(pool, account.id, true, 1, seed::FIXTURE_TREASURY).await?;
        Ok(Self {
            pool: pool.clone(),
            route: serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?,
            customer: customer.id,
            live_key: seed::create_api_key(pool, account.id, true).await?,
            test_key: seed::create_api_key(pool, account.id, false).await?,
        })
    }

    async fn address(&self, number: u8) -> Result<Issued> {
        let issued = Issued {
            id: Uuid::new_v4(),
            address: Address::repeat_byte(0x40 + number),
            salt: B256::repeat_byte(number),
        };
        seed::insert_address(
            &self.pool,
            &NewAddress {
                id: issued.id,
                customer_id: self.customer,
                chain_id: 1,
                route: self.route.route.clone(),
                salt: issued.salt,
                address: issued.address,
            },
        )
        .await?;
        Ok(issued)
    }

    async fn deposit(
        &self,
        address: &Issued,
        number: u8,
        amount: u64,
        state: DepositState,
        reason: Option<RejectReason>,
        is_final: bool,
    ) -> Result<()> {
        ensure!(
            db::insert_deposit(
                &self.pool,
                &NewDeposit {
                    chain_id: 1,
                    tx_hash: B256::repeat_byte(0xd0 + number),
                    receipt_log_index: 0,
                    log_index: 0,
                    block_number: 10,
                    block_hash: B256::repeat_byte(0xbb),
                    block_time: Utc::now(),
                    address_id: address.id,
                    route: Some(self.route.route.clone()),
                    route_version: Some(self.route.version),
                    asset_contract: self.route.asset.contract,
                    from_address: Address::repeat_byte(0x74),
                    amount_atomic: AtomicAmount::new(U256::from(amount)),
                    state,
                    reason,
                    next_attempt_at: Utc::now(),
                    tx_from: Address::repeat_byte(0x74),
                    tx_nonce: u64::from(number),
                    is_final,
                },
            )
            .await?
        );
        Ok(())
    }

    async fn flushed(&self, address: &Issued, amount: u64) -> Result<()> {
        sqlx::query(
            "INSERT INTO flushed (chain_id, tx_hash, log_index, address_id, token, treasury, \
             amount_atomic, block_number, block_hash) \
             VALUES (1, $1, 0, $2, $3, $4, $5::numeric, 11, $6)",
        )
        .bind(format!("{:#x}", B256::repeat_byte(0xf1)))
        .bind(address.id)
        .bind(format!("{:#x}", self.route.asset.contract))
        .bind(format!("{:#x}", seed::FIXTURE_TREASURY))
        .bind(amount.to_string())
        .bind(format!("{:#x}", B256::repeat_byte(0xbc)))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(
        &self,
        path: &str,
        screening: impl DestinationScreener + 'static,
    ) -> Result<(StatusCode, Value)> {
        self.get_with(path, screening, &self.live_key).await
    }

    async fn get_with(
        &self,
        path: &str,
        screening: impl DestinationScreener + 'static,
        key: &str,
    ) -> Result<(StatusCode, Value)> {
        let response = self
            .app(Arc::new(screening))?
            .oneshot(merchant_request(Method::GET, path, Vec::new(), key))
            .await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_048_576).await?;
        Ok((status, serde_json::from_slice(&bytes)?))
    }

    fn app(&self, screening: Arc<dyn DestinationScreener>) -> Result<Router> {
        let admin_key = SigningKey::from_bytes(&ADMIN_KEY);
        Ok(topup::api::router(AppState {
            pool: self.pool.clone(),
            routes: Arc::new(
                topup::routes::RouteSet::new(vec![self.route.clone()])
                    .map_err(anyhow::Error::msg)?,
            ),
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
            screening,
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        })
        .0)
    }
}

struct Clear;
struct Sanctioned;
struct Unavailable;

#[async_trait::async_trait]
impl DestinationScreener for Clear {
    async fn screen(&self, _: &RouteFile, _: Address) -> DestinationScreening {
        DestinationScreening::Clear
    }
}

#[async_trait::async_trait]
impl DestinationScreener for Sanctioned {
    async fn screen(&self, _: &RouteFile, _: Address) -> DestinationScreening {
        DestinationScreening::Sanctioned
    }
}

#[async_trait::async_trait]
impl DestinationScreener for Unavailable {
    async fn screen(&self, _: &RouteFile, _: Address) -> DestinationScreening {
        DestinationScreening::Unavailable
    }
}
