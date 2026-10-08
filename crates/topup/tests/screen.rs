//! PostgreSQL and Anvil integration coverage for the C5 screen step.

mod support;

use std::process::Command;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};
use topup::db::{self, NewDeposit};
use topup::pump::{Pump, PumpConfig, RunOnceResult, Step, StepResult, StepSet};
use topup::steps::screen::{ScreenRoute, ScreenStep};
use topup_adapters::chain::evm::EvmClient;
use topup_adapters::risk::oracle::{SanctionsOracle, SanctionsSource};
use topup_core::deposit::{DepositState, RejectReason, RetryError, StepOutcome, WaitReason};
use topup_core::identity::{credited_event_id, deposit_id, event_id};
use topup_core::money::AtomicAmount;
use topup_core::route::{Bounded, RouteFile};
use topup_core::screening::{SanctionsResult, SanctionsVerdict};
use uuid::Uuid;

use support::chain::{ANVIL_PRIVATE_KEY, Anvil, forge_create};
use support::seed::{self, NewAccount, NewAddress};
use support::with_database;

struct MockSanctionsSource {
    sanctioned: Address,
}

#[async_trait]
impl SanctionsSource for MockSanctionsSource {
    async fn sanctions(&self, address: Address, _block_number: u64) -> SanctionsResult {
        let answer = if address == self.sanctioned {
            SanctionsVerdict::Sanctioned
        } else {
            SanctionsVerdict::Clear
        };
        topup_core::screening::SanctionsResult::new(answer)
    }
}

#[tokio::test]
async fn postgres_pump_persists_screening_transitions_pauses_and_outbox() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool).await?;
            let block_number = 123;
            let sanctioned = Address::repeat_byte(0x22);
            let clear_id = insert_confirmed(
                &context.app_pool,
                seed,
                1,
                Address::repeat_byte(0x11),
                block_number,
            )
            .await?;
            let sanctioned_id =
                insert_confirmed(&context.app_pool, seed, 2, sanctioned, block_number).await?;
            let step = mock_screen_step(&context.app_pool, sanctioned)?;
            let clear = db::get_deposit(&context.app_pool, clear_id)
                .await?
                .context("clear deposit")?;

            set_route_pauses(&context.app_pool, &["settlement"]).await?;
            let route_wait = step.run(&clear).await;
            ensure!(
                route_wait.outcome
                    == StepOutcome::Wait {
                        reason: WaitReason::Paused,
                    }
            );
            ensure!(route_wait.evidence["pause_scopes"]["customer"] == serde_json::json!([]));
            ensure!(route_wait.evidence["pause_scopes"]["account"] == serde_json::json!([]));
            ensure!(
                route_wait.evidence["pause_scopes"]["route"] == serde_json::json!(["settlement"])
            );

            set_route_pauses(&context.app_pool, &[]).await?;
            let resumed = step.run(&clear).await;
            ensure!(resumed.outcome == StepOutcome::Advance);

            set_route_pauses(&context.app_pool, &["refunds"]).await?;
            let unrelated_pause = step.run(&clear).await;
            ensure!(unrelated_pause.outcome == StepOutcome::Advance);
            ensure!(
                unrelated_pause.evidence["pause_scopes"]["route"] == serde_json::json!(["refunds"])
            );
            set_route_pauses(&context.app_pool, &[]).await?;

            let pump = Pump::new(
                context.app_pool.clone(),
                Arc::default(),
                Arc::new(wait_steps().with_confirmed(Box::new(step))),
                PumpConfig::default(),
            )?;
            let mut applied = Vec::new();
            for _ in 0..2 {
                let RunOnceResult::Applied { deposit_id } = pump.run_once().await? else {
                    anyhow::bail!("screen pump did not apply a due deposit");
                };
                applied.push(deposit_id);
            }
            applied.sort_unstable();
            let mut expected = vec![clear_id, sanctioned_id];
            expected.sort_unstable();
            ensure!(applied == expected);

            let clear = db::get_deposit(&context.app_pool, clear_id)
                .await?
                .context("clear deposit after pump")?;
            ensure!(clear.state == DepositState::Credited);
            let rejected = db::get_deposit(&context.app_pool, sanctioned_id)
                .await?
                .context("rejected deposit after pump")?;
            ensure!(rejected.state == DepositState::Rejected);
            ensure!(rejected.reason == Some(RejectReason::Sanctioned));

            let clear_evidence = transition_evidence(&context.app_pool, clear_id).await?;
            ensure!(clear_evidence["sanctions"] == "clear");
            ensure!(clear_evidence.get("block_number").is_none());
            ensure!(clear_evidence["pause_scopes"]["route"] == serde_json::json!([]));
            let rejected_evidence = transition_evidence(&context.app_pool, sanctioned_id).await?;
            ensure!(rejected_evidence["sanctions"] == "sanctioned");
            ensure!(rejected_evidence["oracle"] == format!("{:#x}", Address::repeat_byte(9)));

            // Each event names its account, mode, and deposit; its data, the deposit's API
            // representation, is rendered in the transaction that records the transition.
            for (deposit, event_type, id) in [
                (
                    sanctioned_id,
                    "deposit.rejected",
                    event_id("deposit.rejected", sanctioned_id),
                ),
                (clear_id, "deposit.credited", credited_event_id(clear_id)),
            ] {
                let event = sqlx::query(
                    "SELECT id, type AS event_type, account_id, livemode, object_type, \
                     data AS payload FROM events WHERE object_id = $1",
                )
                .bind(deposit)
                .fetch_one(&context.app_pool)
                .await?;
                ensure!(event.try_get::<Uuid, _>("id")? == id);
                ensure!(event.try_get::<String, _>("event_type")? == event_type);
                ensure!(event.try_get::<Uuid, _>("account_id")? == seed.account_id);
                ensure!(event.try_get::<bool, _>("livemode")?);
                ensure!(
                    event
                        .try_get::<Option<String>, _>("object_type")?
                        .as_deref()
                        == Some("deposit")
                );
                // The deposit as the transition left it, rendered with it.
                let payload = event.try_get::<Value, _>("payload")?;
                let status = if event_type == "deposit.rejected" {
                    "rejected"
                } else {
                    "credited"
                };
                ensure!(payload["object"]["status"] == status, "{payload}");
                ensure!(payload["object"]["object"] == "deposit", "{payload}");
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn anvil_oracle_uses_current_canonical_pins_and_maps_live_results() -> Result<()> {
    let Some(anvil) = Anvil::start_if_available(&[]).await? else {
        return Ok(());
    };
    let rpc_url = anvil.rpc_url.clone();
    let oracle = forge_create(
        &rpc_url,
        "test/mocks/MockSanctionsOracle.sol:MockSanctionsOracle",
        &[],
    )?;
    let account = Address::repeat_byte(0x22);
    let recorded_block = current_block(&rpc_url)?;
    anvil.mine(2)?;
    let source = Arc::new(SanctionsOracle::new(
        client(&rpc_url, StdDuration::from_secs(2))?,
        client(&rpc_url, StdDuration::from_secs(2))?,
        oracle,
    )?);
    let before = source.sanctions(account, recorded_block).await;
    ensure!(before.verdict == SanctionsVerdict::Clear);
    ensure!(before.verdict == SanctionsVerdict::Clear);

    set_sanctioned(&rpc_url, oracle, account, true)?;
    let latest_block = current_block(&rpc_url)?;
    anvil.mine(2)?;
    ensure!(latest_block > recorded_block);
    let historical = source.sanctions(account, recorded_block).await;
    let latest = source.sanctions(account, latest_block).await;
    ensure!(historical.verdict == SanctionsVerdict::Sanctioned);
    ensure!(historical.verdict == SanctionsVerdict::Sanctioned);
    ensure!(latest.verdict == SanctionsVerdict::Sanctioned);
    ensure!(latest.verdict == SanctionsVerdict::Sanctioned);

    with_database(|context| {
        let rpc_url = rpc_url.clone();
        let source = Arc::<SanctionsOracle>::clone(&source);
        Box::pin(async move {
            let seed = seed_account(&context.app_pool).await?;
            let old_id =
                insert_confirmed(&context.app_pool, seed, 3, account, recorded_block).await?;
            let latest_id =
                insert_confirmed(&context.app_pool, seed, 4, account, latest_block).await?;
            let step = ScreenStep::new(
                context.app_pool.clone(),
                [ScreenRoute::new(screen_route("screen", oracle), source)],
            )?;

            let old_deposit = db::get_deposit(&context.app_pool, old_id)
                .await?
                .context("historical deposit")?;
            let old_result = step.run(&old_deposit).await;
            ensure!(old_result.outcome == StepOutcome::Reject(RejectReason::Sanctioned));
            ensure!(old_result.evidence["sanctions"] == "sanctioned");

            let latest_deposit = db::get_deposit(&context.app_pool, latest_id)
                .await?
                .context("latest deposit")?;
            let rejected = step.run(&latest_deposit).await;
            ensure!(rejected.outcome == StepOutcome::Reject(RejectReason::Sanctioned));
            ensure!(rejected.events.len() == 1);
            ensure!(rejected.events[0].event_type == "deposit.rejected");
            ensure!(rejected.evidence["sanctions"] == "sanctioned");
            ensure!(rejected.evidence["sanctions"] == "sanctioned");

            let down_source = Arc::new(SanctionsOracle::new(
                client(&rpc_url, StdDuration::from_millis(200))?,
                client("http://127.0.0.1:1", StdDuration::from_millis(200))?,
                oracle,
            )?);
            let down_step = ScreenStep::new(
                context.app_pool.clone(),
                [ScreenRoute::new(screen_route("down", oracle), down_source)],
            )?;
            let mut down_deposit = latest_deposit;
            down_deposit.route = Some("down".to_owned());
            down_deposit.from_address = Address::repeat_byte(0x11);
            let unavailable = down_step.run(&down_deposit).await;
            ensure!(
                unavailable.outcome
                    == StepOutcome::Retry {
                        error: RetryError::SanctionsInconclusive,
                    }
            );
            ensure!(unavailable.evidence["sanctions_hold"] == true);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn credit_past_the_unfinalized_cap_waits_for_finality() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let pool = &context.app_pool;
            let seed = seed_account(pool).await?;
            // Each deposit credits 37 cents; the account takes 50 before finality.
            sqlx::query("UPDATE accounts SET max_unfinalized_credit = 50 WHERE id = $1")
                .bind(seed.account_id)
                .execute(pool)
                .await?;
            let first = insert_confirmed(pool, seed, 1, Address::repeat_byte(0x11), 123).await?;
            let second = insert_confirmed(pool, seed, 2, Address::repeat_byte(0x12), 123).await?;
            sqlx::query("UPDATE deposits SET final_at = NULL")
                .execute(pool)
                .await?;
            let step = mock_screen_step(pool, Address::repeat_byte(0x22))?;
            let state = |id| async move {
                Ok::<_, anyhow::Error>(db::get_deposit(pool, id).await?.context("deposit")?.state)
            };

            let pump = Pump::new(
                pool.clone(),
                Arc::default(),
                Arc::new(wait_steps().with_confirmed(Box::new(step))),
                PumpConfig::default(),
            )?;

            // While crediting of the addresses' treasury is paused, deposits stay pending and
            // take none of the cap.
            let (livemode, treasury): (bool, String) =
                sqlx::query_as("SELECT livemode, treasury FROM addresses WHERE id = $1")
                    .bind(seed.address_id)
                    .fetch_one(pool)
                    .await?;
            seed::set_treasury(pool, seed.account_id, livemode, 31_337, treasury.parse()?).await?;
            let set_paused = |paused: &'static str| async move {
                sqlx::query(
                    "UPDATE treasuries SET crediting_paused_by = $2::text[] \
                     WHERE account_id = $1 AND replaced_at IS NULL",
                )
                .bind(seed.account_id)
                .bind(paused)
                .execute(pool)
                .await?;
                sqlx::query("UPDATE deposits SET next_attempt_at = now() - interval '1 hour'")
                    .execute(pool)
                    .await?;
                anyhow::Ok(())
            };
            set_paused("{merchant}").await?;
            for _ in 0..2 {
                ensure!(matches!(
                    pump.run_once().await?,
                    RunOnceResult::Applied { .. }
                ));
            }
            ensure!(state(first).await? == DepositState::Confirmed);
            ensure!(state(second).await? == DepositState::Confirmed);
            let exposure =
                db::unfinalized_credit(pool, seed.account_id, livemode, Uuid::nil()).await?;
            ensure!(exposure.credited == 0 && exposure.cap == 50, "{exposure:?}");
            set_paused("{}").await?;

            // Resumed, the first fits under the cap and is credited before finality.
            ensure!(pump.run_once().await? == RunOnceResult::Applied { deposit_id: first });
            ensure!(state(first).await? == DepositState::Credited);

            // The second would take the unfinalized credit to 74: it waits, still confirmed.
            let step = mock_screen_step(pool, Address::repeat_byte(0x22))?;
            let capped = step
                .run(&db::get_deposit(pool, second).await?.context("second")?)
                .await;
            ensure!(
                capped.outcome
                    == StepOutcome::Wait {
                        reason: WaitReason::UnfinalizedCreditCap,
                    }
            );
            ensure!(capped.evidence["unfinalized_credit_minor"] == 37);
            ensure!(capped.evidence["max_unfinalized_credit"] == 50);

            // A step that checked the cap before a concurrent credit still cannot pass it: the
            // commit re-checks under the account's lock and the deposit waits instead.
            let racing = Pump::new(
                pool.clone(),
                Arc::default(),
                Arc::new(wait_steps().with_confirmed(Box::new(AlwaysCredit))),
                PumpConfig::default(),
            )?;
            ensure!(racing.run_once().await? == RunOnceResult::Applied { deposit_id: second });
            ensure!(state(second).await? == DepositState::Confirmed);
            let evidence: Value = sqlx::query_scalar(
                "SELECT evidence FROM transitions WHERE deposit_id = $1 ORDER BY created_at DESC \
                 LIMIT 1",
            )
            .bind(second)
            .fetch_one(pool)
            .await?;
            ensure!(evidence["reason"] == "unfinalized_credit_cap", "{evidence}");
            let due: bool = sqlx::query_scalar(
                "SELECT next_attempt_at > now() + interval '30 seconds' FROM deposits \
                 WHERE id = $1",
            )
            .bind(second)
            .fetch_one(pool)
            .await?;
            ensure!(due, "a capped deposit is retried after the wait interval");
            ensure!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM events WHERE type = 'deposit.credited' \
                     AND object_id = $1"
                )
                .bind(second)
                .fetch_one(pool)
                .await?
                    == 0
            );

            // Once final it is credited whatever the unfinalized credit.
            sqlx::query(
                "UPDATE deposits SET final_at = now(), next_attempt_at = now() - interval '1 hour' \
                 WHERE id = $1",
            )
            .bind(second)
            .execute(pool)
            .await?;
            ensure!(pump.run_once().await? == RunOnceResult::Applied { deposit_id: second });
            ensure!(state(second).await? == DepositState::Credited);
            Ok(())
        })
    })
    .await
}

/// Credits every confirmed deposit without checking the cap, as a step that checked it before a
/// concurrent credit committed.
struct AlwaysCredit;

#[async_trait]
impl Step for AlwaysCredit {
    async fn run(&self, _deposit: &db::Deposit) -> StepResult {
        StepResult::new(
            StepOutcome::Advance,
            serde_json::json!({"outcome": "advance"}),
        )
    }
}

fn mock_screen_step(pool: &PgPool, sanctioned: Address) -> Result<ScreenStep> {
    Ok(ScreenStep::new(
        pool.clone(),
        [ScreenRoute::new(
            screen_route("screen", Address::repeat_byte(9)),
            Arc::new(MockSanctionsSource { sanctioned }),
        )],
    )?)
}

/// Version 1 of route `name` on the Anvil chain, whose deposits are bounded to 10 to 20 base units
/// on the operator's defaults, screened by `oracle`.
fn screen_route(name: &str, oracle: Address) -> RouteFile {
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))
            .expect("route fixture");
    route.route = name.to_owned();
    route.version = 1;
    route.chain.chain_id = 31_337;
    route.asset.contract = Address::repeat_byte(0x55);
    route.screening.sanctions_oracle = oracle;
    route.merchant.min_deposit_atomic = Bounded::at(AtomicAmount::new(U256::from(10)));
    route.merchant.max_deposit_atomic = Bounded::at(AtomicAmount::new(U256::from(20)));
    route.merchant.min_refund_atomic = Bounded::at(AtomicAmount::new(U256::from(10)));
    route
}

#[derive(Clone, Copy)]
struct Seed {
    account_id: Uuid,
    address_id: Uuid,
}

async fn seed_account(pool: &PgPool) -> Result<Seed> {
    let (account, customer) = seed::create_account_and_customer(
        pool,
        &NewAccount {
            webhook_url: "https://product.test/webhooks".to_owned(),
            ..NewAccount::named("product-c5")
        },
        "workspace-c5",
    )
    .await?;
    let address = NewAddress {
        id: Uuid::new_v4(),
        customer_id: customer.id,
        chain_id: 31_337,
        route: "screen".to_owned(),
        salt: B256::repeat_byte(0x33),
        address: Address::repeat_byte(0x44),
    };
    seed::insert_address(pool, &address).await?;
    let oracle = Address::repeat_byte(9);
    seed::accept_routes(
        pool,
        account.id,
        true,
        &[
            &screen_route("screen", oracle),
            &screen_route("down", oracle),
        ],
    )
    .await?;
    Ok(Seed {
        account_id: account.id,
        address_id: address.id,
    })
}

async fn insert_confirmed(
    pool: &PgPool,
    seed: Seed,
    number: u8,
    from_address: Address,
    block_number: u64,
) -> Result<Uuid> {
    let deposit = NewDeposit {
        chain_id: 31_337,
        tx_hash: B256::repeat_byte(number),
        log_index: 0,
        receipt_log_index: 0,
        tx_from: alloy_primitives::Address::ZERO,
        tx_nonce: 0,
        is_final: true,
        block_number,
        block_hash: B256::repeat_byte(number.wrapping_add(1)),
        block_time: Utc::now(),
        address_id: seed.address_id,
        route: Some("screen".to_owned()),
        route_version: Some(1),
        asset_contract: Address::repeat_byte(0x55),
        from_address,
        amount_atomic: AtomicAmount::new(U256::from(15)),
        state: DepositState::Confirmed,
        reason: None,
        next_attempt_at: Utc::now() - Duration::seconds(1),
    };
    let id = deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
    ensure!(db::insert_deposit(pool, &deposit).await?);
    // The confirm step stores the valuation the credit carries.
    sqlx::query(
        r#"
        UPDATE deposits
        SET valuation_at = '2026-09-28T00:00:00Z', price_scaled = 250000000,
            price_source = 'spot', credit_minor = 37
        WHERE id = $1
        "#,
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn set_route_pauses(pool: &PgPool, scopes: &[&str]) -> Result<()> {
    let scopes = scopes.iter().map(ToString::to_string).collect::<Vec<_>>();
    sqlx::query(
        r#"
        INSERT INTO route_pauses (route, paused_scopes)
        VALUES ('screen', $1)
        ON CONFLICT (route) DO UPDATE SET paused_scopes = EXCLUDED.paused_scopes
        "#,
    )
    .bind(scopes)
    .execute(pool)
    .await?;
    Ok(())
}

async fn transition_evidence(pool: &PgPool, deposit_id: Uuid) -> Result<Value> {
    Ok(
        sqlx::query("SELECT evidence FROM transitions WHERE deposit_id = $1")
            .bind(deposit_id)
            .fetch_one(pool)
            .await?
            .try_get("evidence")?,
    )
}

fn set_sanctioned(
    rpc_url: &str,
    oracle: Address,
    account: Address,
    sanctioned: bool,
) -> Result<()> {
    let output = Command::new("cast")
        .args([
            "send",
            "--rpc-url",
            rpc_url,
            "--private-key",
            ANVIL_PRIVATE_KEY,
            &format!("{oracle:#x}"),
            "setSanctioned(address,bool)",
            &format!("{account:#x}"),
            if sanctioned { "true" } else { "false" },
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "cast send failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn current_block(rpc_url: &str) -> Result<u64> {
    let output = Command::new("cast")
        .args(["block-number", "--rpc-url", rpc_url])
        .output()?;
    ensure!(output.status.success(), "cast block-number failed");
    String::from_utf8(output.stdout)?
        .trim()
        .parse()
        .context("cast returned an invalid block number")
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

fn client(rpc_url: &str, timeout: StdDuration) -> Result<Arc<EvmClient>> {
    Ok(Arc::new(EvmClient::with_timeout(rpc_url, timeout)?))
}

#[tokio::test]
#[tracing_test::traced_test]
async fn only_dual_clear_credits_and_inconclusive_screening_holds_retries_and_alerts() -> Result<()>
{
    struct Answers(SanctionsVerdict, SanctionsVerdict);
    #[async_trait]
    impl SanctionsSource for Answers {
        async fn sanctions(&self, _: Address, _block_number: u64) -> SanctionsResult {
            topup_core::screening::SanctionsResult::new(if self.0 == self.1 {
                self.0
            } else {
                SanctionsVerdict::Uncertain
            })
        }
    }
    with_database(|database| Box::pin(async move {
        let pool=&database.app_pool;
        let seed=seed_account(pool).await?;
        let route=screen_route("screen",Address::repeat_byte(9));
        let alerter=topup::pump::AgeAlerter::new(pool.clone(),topup::pump::AgeAlertConfig::from_routes(std::slice::from_ref(&route))?,StdDuration::from_secs(1));
        let mut number=0;let mut held=None;
        for a in [SanctionsVerdict::Clear,SanctionsVerdict::Sanctioned,SanctionsVerdict::Uncertain] {
            for b in [SanctionsVerdict::Clear,SanctionsVerdict::Sanctioned,SanctionsVerdict::Uncertain] {
                number+=1;
                let id=insert_confirmed(pool,seed,number,Address::repeat_byte(number),123).await?;
                let step=ScreenStep::new(pool.clone(),[ScreenRoute::new(route.clone(),Arc::new(Answers(a,b)))])?;
                let pump=Pump::new(pool.clone(),Arc::default(),Arc::new(wait_steps().with_confirmed(Box::new(step))),PumpConfig::default())?;
                let attempted_at=Utc::now();
                ensure!(pump.run_once().await?==RunOnceResult::Applied {deposit_id:id});
                let deposit=db::get_deposit(pool,id).await?.context("screened deposit")?;
                let expected=match (a,b) {
                    (SanctionsVerdict::Clear,SanctionsVerdict::Clear)=>DepositState::Credited,
                    (SanctionsVerdict::Sanctioned,SanctionsVerdict::Sanctioned)=>DepositState::Rejected,
                    _=>DepositState::Confirmed,
                };
                ensure!(deposit.state==expected,"{a:?}/{b:?} -> {:?}",deposit.state);
                let credits:i64=sqlx::query_scalar("SELECT count(*) FROM events WHERE object_id=$1 AND type='deposit.credited'").bind(id).fetch_one(pool).await?;
                ensure!(credits==i64::from(expected==DepositState::Credited));
                let evidence=transition_evidence(pool,id).await?;
                ensure!(evidence.get("block_hash").is_none());
                if expected==DepositState::Confirmed {
                    ensure!(deposit.attempt>0 && deposit.next_attempt_at>=attempted_at);
                    ensure!(evidence["sanctions_hold"]==true);
                    ensure!(sqlx::query_scalar::<_,bool>("SELECT sanctions_hit_at IS NULL FROM deposits WHERE id=$1").bind(id).fetch_one(pool).await?);
                    held=Some(id);
                    sqlx::query("UPDATE deposits SET next_attempt_at=now()+interval '1 hour' WHERE id=$1").bind(id).execute(pool).await?;
                }
            }
        }
        ensure!(alerter.scan_once().await?==0,"fresh sanctions holds must not alert yet");
        let held=held.context("inconclusive case")?;
        let window=i64::try_from(route.chain.confirmations.typical_credit_seconds(route.chain.chain_id))?;
        sqlx::query("UPDATE deposits SET created_at=now()-($2::bigint+1)*interval '1 second' WHERE id=$1").bind(held).bind(window).execute(&database.owner_pool).await?;
        ensure!(alerter.scan_once().await?==1);
        ensure!(logs_contain("TopupSanctionsHold"));
        let step=ScreenStep::new(pool.clone(),[ScreenRoute::new(route,Arc::new(Answers(SanctionsVerdict::Clear,SanctionsVerdict::Clear)))])?;
        let pump=Pump::new(pool.clone(),Arc::default(),Arc::new(wait_steps().with_confirmed(Box::new(step))),PumpConfig::default())?;
        sqlx::query("UPDATE deposits SET next_attempt_at=now()-interval '1 second' WHERE id=$1").bind(held).execute(pool).await?;
        ensure!(pump.run_once().await?==RunOnceResult::Applied {deposit_id:held});
        ensure!(db::get_deposit(pool,held).await?.unwrap().state==DepositState::Credited);
        Ok(())
    })).await
}

#[tokio::test]
async fn local_list_stale_holds_and_latest_hit_rejects_with_event() -> Result<()> {
    with_database(|db| Box::pin(async move {
        let seed=seed_account(&db.app_pool).await?;
        let sender=Address::repeat_byte(0xaa);
        let id=insert_confirmed(&db.app_pool,seed,80,sender,123).await?;
        let snapshot=Uuid::new_v4();
        sqlx::query("INSERT INTO sanctions_list_snapshots(id,source,publish_date,sha256,record_count,address_count,fetched_at,activated_at,verified_at) VALUES($1,'ofac_sdn','2026-10-05',$2,1,0,now(),now(),now()-interval '25 hours')")
            .bind(snapshot).bind(vec![1_u8;32]).execute(&db.app_pool).await?;
        let source=Arc::new(topup::sanctions::ListScreener::new(db.app_pool.clone(),StdDuration::from_secs(86400)));
        let step=ScreenStep::new(db.app_pool.clone(),[ScreenRoute::new(screen_route("screen",Address::repeat_byte(9)),source)])?;
        let pump=Pump::new(db.app_pool.clone(),Arc::default(),Arc::new(wait_steps().with_confirmed(Box::new(step))),PumpConfig::default())?;
        ensure!(pump.run_once().await?==RunOnceResult::Applied {deposit_id:id});
        let held=topup::db::get_deposit(&db.app_pool,id).await?.context("held deposit")?;
        ensure!(held.state==DepositState::Confirmed);
        let evidence=transition_evidence(&db.app_pool,id).await?;
        ensure!(evidence["sanctions_hold"]==true && evidence["sanctions"]=="uncertain");
        ensure!(evidence["provenance"]["snapshot_id"]==snapshot.to_string());
        // New activation at decision time sanctions a sender whose transfer block is old.
        let newest=Uuid::new_v4();
        sqlx::query("INSERT INTO sanctions_list_snapshots(id,source,publish_date,sha256,record_count,address_count,fetched_at,activated_at,verified_at) VALUES($1,'ofac_sdn','2026-10-06',$2,1,1,now(),now(),now())")
            .bind(newest).bind(vec![2_u8;32]).execute(&db.app_pool).await?;
        sqlx::query("INSERT INTO sanctions_list_addresses(snapshot_id,sdn_uid,id_type,raw_value,evm_address) VALUES($1,1,'Digital Currency Address - BSC',$2,$3)")
            .bind(newest).bind(format!("{sender:#x}")).bind(sender.as_slice()).execute(&db.app_pool).await?;
        sqlx::query("UPDATE deposits SET next_attempt_at=now() WHERE id=$1").bind(id).execute(&db.app_pool).await?;
        ensure!(pump.run_once().await?==RunOnceResult::Applied {deposit_id:id});
        ensure!(topup::db::get_deposit(&db.app_pool,id).await?.unwrap().state==DepositState::Rejected);
        ensure!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM events WHERE object_id=$1 AND type='deposit.rejected'").bind(id).fetch_one(&db.app_pool).await?==1);
        let evidence=transition_evidence(&db.app_pool,id).await?;
        ensure!(evidence["sanctions"]=="sanctioned" && evidence["provenance"]["snapshot_id"]==newest.to_string());
        ensure!(evidence.get("block_number").is_none());
        Ok(())
    })).await
}
