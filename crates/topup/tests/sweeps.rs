//! Chain-sourced sweeps on Anvil and PostgreSQL (docs/design/multi-tenant.md §4, §13, §16 PR 4):
//! the finalized scanner indexes the permissionless factory's events, whoever sent them, for
//! known `(address, treasury)` pairs, and reconciliation per forwarder freezes a chain whose
//! ledger disagrees with it.

mod support;

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_primitives::{Address, B256, Bytes, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::json;
use sqlx::PgPool;
use topup::db::{self, Deposit};
use topup::pump::{Pump, PumpConfig, RunOnceResult, Step, StepResult, StepSet};
use topup::reconciler::{CheckName, Reconciler, ReconciliationChain, chain_is_blocked};
use topup::routes::RouteSet;
use topup::scanner::{ChainRoutes, chain_routes, coverage_once};
use topup_adapters::chain::evm::{EvmClient, FactoryLog, FinalizedReader};
use topup_adapters::chain::flush::{
    DecodedFlushFailed, DecodedFlushed, DecodedForwarderCreated, FactoryEvent,
};
use topup_core::address::forwarder_address;
use topup_core::deposit::StepOutcome;
use topup_core::route::RouteFile;
use uuid::Uuid;

use support::TestDatabase;
use support::chain::{ANVIL_PRIVATE_KEY, Anvil, CHAIN_ID, forge_create, run_checked};
use support::seed::{self, FIXTURE_TREASURY, NewAccount, NewAddress, NewCustomer};

/// Anvil's second default account: a merchant, or anyone else, calling the public `flush`.
const SWEEPER_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const AMOUNT: u64 = 1_000;
/// Anvil keeps `finalized` 64 blocks behind the head.
const FINALITY_DEPTH: u64 = 64;

#[tokio::test]
async fn a_third_partys_flush_marks_deposits_swept_only_after_finality() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let paid = chain.seed_address(1).await?;
            let unsettled = chain.seed_address(2).await?;
            chain.pay(paid.forwarder)?;
            chain.pay(unsettled.forwarder)?;
            chain.finalize()?;
            chain.scan().await?;
            let paid_deposit = chain.deposit_of(&paid).await?;
            let unsettled_deposit = chain.deposit_of(&unsettled).await?;
            // Credited as the pump would; only the first has reached finality.
            chain.credit(paid_deposit, true).await?;
            chain.credit(unsettled_deposit, false).await?;

            // Anyone may sweep: the merchant's wallet, a script, or a stranger paying the gas.
            chain.flush(&[paid.salt, unsettled.salt])?;
            let stats = chain.scan().await?;
            ensure!(
                stats.factory.flushed == 0,
                "an unfinalized flush was indexed"
            );
            ensure!(chain.state(paid_deposit).await? == "credited");
            ensure!(chain.count("SELECT count(*) FROM flushed").await? == 0);

            chain.finalize()?;
            let stats = chain.scan().await?;
            ensure!(stats.factory.created == 2, "{:?}", stats.factory);
            ensure!(stats.factory.flushed == 2, "{:?}", stats.factory);
            ensure!(stats.factory.swept == 1, "{:?}", stats.factory);
            ensure!(chain.state(paid_deposit).await? == "swept");
            // A deposit that could still be reversed stays credited until it is final.
            ensure!(chain.state(unsettled_deposit).await? == "credited");
            let evidence: serde_json::Value = sqlx::query_scalar(
                "SELECT evidence FROM transitions WHERE deposit_id = $1 AND to_state = 'swept'",
            )
            .bind(paid_deposit)
            .fetch_one(chain.pool())
            .await?;
            ensure!(evidence["source"] == "finalized_flushed", "{evidence}");
            let deployed: Option<i64> =
                sqlx::query_scalar("SELECT deployed_block FROM addresses WHERE id = $1")
                    .bind(paid.id)
                    .fetch_one(chain.pool())
                    .await?;
            ensure!(deployed.is_some());

            // Once the finality watch settles it, the repair pass sweeps it with the same rule.
            sqlx::query("UPDATE deposits SET final_at = now() WHERE id = $1")
                .bind(unsettled_deposit)
                .execute(chain.pool())
                .await?;
            let repaired = chain
                .reconciler()
                .check(CheckName::MissingFlushLink)
                .await?;
            ensure!(repaired.len() == 1 && repaired[0].repair_applied);
            ensure!(chain.state(unsettled_deposit).await? == "swept");

            // Each forwarder's balance is its deposits minus its finalized sweeps.
            ensure!(
                chain
                    .reconciler()
                    .check(CheckName::CustodyBalance)
                    .await?
                    .is_empty()
            );
            ensure!(!chain_is_blocked(chain.pool(), CHAIN_ID).await?);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn the_pump_leaves_a_credited_deposit_to_its_finalized_sweep() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let paid = chain.seed_address(1).await?;
            chain.pay(paid.forwarder)?;
            chain.finalize()?;
            chain.scan().await?;
            let deposit = chain.deposit_of(&paid).await?;
            chain.credit(deposit, true).await?;
            let transitions = "SELECT count(*) FROM transitions";
            let before = chain.count(transitions).await?;

            // A due credited deposit is not claimed, so waiting for its sweep writes nothing.
            let calls = Arc::new(AtomicUsize::new(0));
            let step = || Box::new(CountingStep(Arc::clone(&calls))) as Box<dyn Step>;
            let pump = Pump::new(
                chain.pool().clone(),
                Arc::default(),
                Arc::new(StepSet::new(step(), step())),
                PumpConfig::default(),
            )?;
            ensure!(pump.run_once().await? == RunOnceResult::Idle);
            ensure!(calls.load(Ordering::SeqCst) == 0);
            ensure!(chain.count(transitions).await? == before);

            // The finalized `Flushed` event alone sweeps it, with one transition.
            chain.flush(&[paid.salt])?;
            chain.finalize()?;
            chain.scan().await?;
            ensure!(chain.state(deposit).await? == "swept");
            let written: Vec<(String, String)> = sqlx::query_as(
                "SELECT from_state, to_state FROM transitions WHERE deposit_id = $1",
            )
            .bind(deposit)
            .fetch_all(chain.pool())
            .await?;
            ensure!(
                written == [("credited".to_owned(), "swept".to_owned())],
                "{written:?}"
            );
            ensure!(chain.count(transitions).await? == before + 1);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_failing_target_is_reported_and_leaves_its_deposits_unswept() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let blocked = chain.seed_address(3).await?;
            let open = chain.seed_address(4).await?;
            chain.pay(blocked.forwarder)?;
            chain.pay(open.forwarder)?;
            chain.finalize()?;
            chain.scan().await?;
            let blocked_deposit = chain.deposit_of(&blocked).await?;
            let open_deposit = chain.deposit_of(&open).await?;
            chain.credit(blocked_deposit, true).await?;
            chain.credit(open_deposit, true).await?;

            // The token refuses transfers from one forwarder, as an issuer blacklist would.
            chain.send(
                chain.token,
                ANVIL_PRIVATE_KEY,
                "setBlockedForwarder(address)",
                &[&format!("{:#x}", blocked.forwarder)],
            )?;
            chain.flush(&[blocked.salt, open.salt])?;
            chain.finalize()?;
            let stats = chain.scan().await?;
            ensure!(stats.factory.failed == 1, "{:?}", stats.factory);
            ensure!(stats.factory.flushed == 1, "{:?}", stats.factory);

            let failures: Vec<(Uuid, String, String)> = sqlx::query_as(
                "SELECT address_id, token, reason FROM flush_failures ORDER BY log_index",
            )
            .fetch_all(chain.pool())
            .await?;
            ensure!(failures.len() == 1);
            ensure!(failures[0].0 == blocked.id);
            ensure!(failures[0].1 == format!("{:#x}", chain.token));
            // `BlockedForwarder(address)` revert data, as the factory copied it.
            ensure!(failures[0].2.len() == 2 + 2 * (4 + 32), "{}", failures[0].2);
            ensure!(chain.state(blocked_deposit).await? == "credited");
            ensure!(chain.state(open_deposit).await? == "swept");

            // The failed forwarder still holds its deposit, so the books balance.
            ensure!(
                chain
                    .reconciler()
                    .check(CheckName::CustodyBalance)
                    .await?
                    .is_empty()
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn factory_events_for_unknown_pairs_are_ignored() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let known = chain.seed_address(5).await?;
            // Someone funds and flushes forwarders the service never issued: another salt of our
            // treasury, and our salt for another treasury.
            let stranger = B256::repeat_byte(0xee);
            let stranger_forwarder = forwarder_address(
                chain.factory,
                chain.implementation,
                FIXTURE_TREASURY,
                stranger,
            );
            let other_treasury = Address::repeat_byte(0x42);
            let foreign = forwarder_address(
                chain.factory,
                chain.implementation,
                other_treasury,
                known.salt,
            );
            chain.pay(stranger_forwarder)?;
            chain.pay(foreign)?;
            chain.flush(&[stranger])?;
            chain.flush_to(other_treasury, &[known.salt])?;
            chain.finalize()?;
            let stats = chain.scan().await?;
            ensure!(
                stats.factory == db::FactoryCommit::default(),
                "{:?}",
                stats.factory
            );
            ensure!(chain.count("SELECT count(*) FROM flushed").await? == 0);
            ensure!(chain.count("SELECT count(*) FROM deposits").await? == 0);
            ensure!(
                chain
                    .count("SELECT count(*) FROM addresses WHERE deployed_block IS NOT NULL")
                    .await?
                    == 0
            );

            // The pair, not the forwarder alone, decides: an event naming a known forwarder with
            // another treasury is not the known address's.
            let spoofed = |event| FactoryLog {
                tx_hash: B256::repeat_byte(0x11),
                log_index: 0,
                block_number: 1,
                block_hash: B256::repeat_byte(0x22),
                event,
            };
            let commit = db::commit_factory_logs(
                chain.pool(),
                CHAIN_ID,
                &[
                    spoofed(FactoryEvent::ForwarderCreated(DecodedForwarderCreated {
                        salt: known.salt,
                        forwarder: known.forwarder,
                        treasury: other_treasury,
                    })),
                    FactoryLog {
                        log_index: 1,
                        ..spoofed(FactoryEvent::Flushed(DecodedFlushed {
                            salt: known.salt,
                            forwarder: known.forwarder,
                            token: chain.token,
                            treasury: other_treasury,
                            amount: U256::from(AMOUNT),
                        }))
                    },
                    FactoryLog {
                        log_index: 2,
                        ..spoofed(FactoryEvent::FlushFailed(DecodedFlushFailed {
                            salt: stranger,
                            forwarder: stranger_forwarder,
                            token: chain.token,
                            reason: Bytes::new(),
                        }))
                    },
                ],
            )
            .await?;
            ensure!(commit == db::FactoryCommit::default(), "{commit:?}");
            ensure!(chain.count("SELECT count(*) FROM flush_failures").await? == 0);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_forwarder_mismatch_freezes_crediting_on_the_chain() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let first = chain.seed_address(6).await?;
            let second = chain.seed_address(7).await?;
            chain.pay(first.forwarder)?;
            chain.pay(second.forwarder)?;
            chain.finalize()?;
            chain.scan().await?;
            let first_deposit = chain.deposit_of(&first).await?;
            let second_deposit = chain.deposit_of(&second).await?;
            sqlx::query("UPDATE deposits SET final_at = now()")
                .execute(chain.pool())
                .await?;
            ensure!(
                chain
                    .reconciler()
                    .check(CheckName::CustodyBalance)
                    .await?
                    .is_empty()
            );

            // The ledger loses track of part of a deposit: the forwarder holds more than it says.
            sqlx::query("UPDATE deposits SET amount_atomic = amount_atomic - 1 WHERE id = $1")
                .bind(first_deposit)
                .execute(chain.pool())
                .await?;
            let findings = chain.reconciler().check(CheckName::CustodyBalance).await?;
            ensure!(findings.len() == 1, "{findings:?}");
            ensure!(findings[0].subjects["address_id"] == first.id.to_string());
            ensure!(findings[0].expected["deposits_atomic"] == json!((AMOUNT - 1).to_string()));
            ensure!(findings[0].observed["balance_atomic"] == json!(AMOUNT.to_string()));
            ensure!(chain_is_blocked(chain.pool(), CHAIN_ID).await?);

            // Crediting stops: the pump runs no step for the chain's deposits...
            let calls = Arc::new(AtomicUsize::new(0));
            let step = || Box::new(CountingStep(Arc::clone(&calls))) as Box<dyn Step>;
            let pump = Pump::new(
                chain.pool().clone(),
                Arc::default(),
                Arc::new(StepSet::new(step(), step())),
                PumpConfig::default(),
            )?;
            let RunOnceResult::Applied { deposit_id } = pump.run_once().await? else {
                anyhow::bail!("the pump claimed no deposit");
            };
            ensure!(deposit_id == first_deposit || deposit_id == second_deposit);
            ensure!(calls.load(Ordering::SeqCst) == 0);
            let reason: Option<String> = sqlx::query_scalar(
                "SELECT evidence->>'reason' FROM transitions WHERE deposit_id = $1 \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(deposit_id)
            .fetch_one(chain.pool())
            .await?;
            ensure!(reason.as_deref() == Some("chain_frozen"));

            // ...and the scanner records nothing new until an operator lifts the freeze.
            let third = chain.seed_address(8).await?;
            chain.pay(third.forwarder)?;
            chain.finalize()?;
            ensure!(chain.scan().await.is_err(), "frozen chain accepted a scan");
            ensure!(chain.count("SELECT count(*) FROM deposits").await? == 2);
            Ok(())
        })
    })
    .await
}

struct CountingStep(Arc<AtomicUsize>);

#[async_trait]
impl Step for CountingStep {
    async fn run(&self, _deposit: &Deposit) -> StepResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        StepResult::new(StepOutcome::Advance, json!({"outcome": "advance"}))
    }
}

/// One Anvil chain with the factory and a token, and a migrated database whose route uses them.
struct Chain<'a> {
    anvil: Anvil,
    database: &'a TestDatabase,
    factory: Address,
    implementation: Address,
    token: Address,
    route: RouteFile,
    routes: ChainRoutes,
    reader: FinalizedReader,
    account_id: Uuid,
}

struct SeededAddress {
    id: Uuid,
    salt: B256,
    forwarder: Address,
}

impl<'a> Chain<'a> {
    async fn start(database: &'a TestDatabase, anvil: Anvil) -> Result<Self> {
        let factory = forge_create(
            &anvil.rpc_url,
            "src/ForwarderFactory.sol:ForwarderFactory",
            &[],
        )?;
        let token = forge_create(
            &anvil.rpc_url,
            "test/mocks/MockTokens.sol:SelectiveRevertingToken",
            &[],
        )?;
        let implementation =
            Address::from_str(call(&anvil.rpc_url, factory, "implementation()(address)")?.trim())?;
        let mut route: RouteFile =
            serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
        route.chain.chain_id = CHAIN_ID;
        route.livemode = false;
        route.chain.contracts.forwarder_factory = factory;
        route.chain.contracts.implementation = implementation;
        route.asset.contract = token;
        let routes = chain_routes(&RouteSet::new(vec![route.clone()]).map_err(anyhow::Error::msg)?)
            .into_iter()
            .next()
            .context("one chain")?;
        let account = seed::create_account(
            &database.app_pool,
            &NewAccount {
                livemode: false,
                ..NewAccount::named("sweeps")
            },
        )
        .await?;
        let reader = FinalizedReader::new(Arc::new(EvmClient::new(&anvil.rpc_url)?));
        Ok(Self {
            anvil,
            database,
            factory,
            implementation,
            token,
            route,
            routes,
            reader,
            account_id: account.id,
        })
    }

    fn pool(&self) -> &PgPool {
        &self.database.app_pool
    }

    async fn seed_address(&self, number: u8) -> Result<SeededAddress> {
        let customer = seed::create_customer(
            self.pool(),
            &NewCustomer {
                id: Uuid::new_v4(),
                account_id: self.account_id,
                livemode: false,
                client_reference_id: format!("customer-{number}"),
                paused_scopes: Vec::new(),
            },
        )
        .await?;
        let salt = B256::repeat_byte(number);
        let forwarder =
            forwarder_address(self.factory, self.implementation, FIXTURE_TREASURY, salt);
        let address = seed::insert_address(
            self.pool(),
            &NewAddress {
                id: Uuid::new_v4(),
                customer_id: customer.id,
                chain_id: CHAIN_ID,
                route: self.route.route.clone(),
                salt,
                address: forwarder,
            },
        )
        .await?;
        Ok(SeededAddress {
            id: address.id,
            salt,
            forwarder,
        })
    }

    /// A payer sends the route's token to `forwarder`.
    fn pay(&self, forwarder: Address) -> Result<()> {
        self.send(
            self.token,
            ANVIL_PRIVATE_KEY,
            "mint(address,uint256)",
            &[&format!("{forwarder:#x}"), &AMOUNT.to_string()],
        )
    }

    /// Anvil's second account calls the public `flush` for the route's treasury.
    fn flush(&self, salts: &[B256]) -> Result<()> {
        self.flush_to(FIXTURE_TREASURY, salts)
    }

    fn flush_to(&self, treasury: Address, salts: &[B256]) -> Result<()> {
        let salts = salts
            .iter()
            .map(|salt| format!("{salt:#x}"))
            .collect::<Vec<_>>()
            .join(",");
        self.send(
            self.factory,
            SWEEPER_KEY,
            "flush(address,bytes32[],address)",
            &[
                &format!("{treasury:#x}"),
                &format!("[{salts}]"),
                &format!("{:#x}", self.token),
            ],
        )
    }

    fn send(&self, to: Address, key: &str, signature: &str, args: &[&str]) -> Result<()> {
        let key = format!("0x{key}");
        let to = format!("{to:#x}");
        let mut arguments = vec![
            "send",
            "--rpc-url",
            &self.anvil.rpc_url,
            "--private-key",
            &key,
            &to,
            signature,
        ];
        arguments.extend_from_slice(args);
        run_checked("cast", &arguments, None)?;
        Ok(())
    }

    /// Mines past the finality depth, so every earlier block is `finalized`.
    fn finalize(&self) -> Result<()> {
        self.anvil.mine(FINALITY_DEPTH + 1)
    }

    async fn scan(&self) -> Result<topup::scanner::ScanStats> {
        Ok(coverage_once(self.pool(), &self.reader, &self.reader, &self.routes, 1).await?)
    }

    async fn deposit_of(&self, address: &SeededAddress) -> Result<Uuid> {
        Ok(
            sqlx::query_scalar("SELECT id FROM deposits WHERE address_id = $1")
                .bind(address.id)
                .fetch_one(self.pool())
                .await?,
        )
    }

    /// Marks a deposit credited, and final when `is_final`, as the pump and finality watch do.
    async fn credit(&self, deposit: Uuid, is_final: bool) -> Result<()> {
        sqlx::query(
            "UPDATE deposits SET state = 'credited', \
             final_at = CASE WHEN $2 THEN now() END WHERE id = $1",
        )
        .bind(deposit)
        .bind(is_final)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    async fn state(&self, deposit: Uuid) -> Result<String> {
        Ok(
            sqlx::query_scalar("SELECT state FROM deposits WHERE id = $1")
                .bind(deposit)
                .fetch_one(self.pool())
                .await?,
        )
    }

    async fn count(&self, query: &'static str) -> Result<i64> {
        Ok(sqlx::query_scalar(query).fetch_one(self.pool()).await?)
    }

    fn reconciler(&self) -> Reconciler {
        let chain: Arc<dyn ReconciliationChain> =
            Arc::new(FinalizedReader::new(Arc::clone(self.reader.client())));
        Reconciler::with_dependencies(
            self.pool().clone(),
            Arc::new(RouteSet::new(vec![self.route.clone()]).expect("route loads")),
            BTreeMap::from([(CHAIN_ID, chain)]),
        )
    }
}

fn call(rpc_url: &str, contract: Address, signature: &str) -> Result<String> {
    let contract = format!("{contract:#x}");
    let output = run_checked(
        "cast",
        &["call", "--rpc-url", rpc_url, &contract, signature],
        None,
    )?;
    Ok(String::from_utf8(output.stdout)?)
}

async fn run<F>(test: F) -> Result<()>
where
    F: for<'a> FnOnce(&'a Chain<'a>) -> support::TestFuture<'a> + 'static,
{
    let Some(anvil) = Anvil::start_if_available(&[]).await? else {
        return Ok(());
    };
    support::with_database(|database| {
        Box::pin(async move {
            let chain = Chain::start(database, anvil).await?;
            test(&chain).await
        })
    })
    .await
}
