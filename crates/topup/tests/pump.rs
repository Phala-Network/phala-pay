//! PostgreSQL integration tests for concurrent deposit pumps.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::{Barrier, Semaphore};
use tokio_util::sync::CancellationToken;
use topup::db::{self, EventObject, NewDeposit, OutboxEvent, StoredValuation, TransitionEffects};
use topup::jitter::JitterSource;
use topup::pump::{
    AgeAlertConfig, AgeAlerter, Pump, PumpConfig, RunOnceResult, Step, StepResult, StepSet,
};
use topup::routes::RouteSet;
use topup::steps::confirm::ConfirmStep;
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FinalizedHead, ReceiptLookup, TransferLog,
};
use topup_adapters::pricing::{Observation, PriceError, PriceSource};
use topup_core::deposit::{DepositState, RetryError, StepOutcome, WaitReason};
use topup_core::identity::deposit_id;
use topup_core::money::{AtomicAmount, MinorAmount, PRICE_SCALE, ScaledPrice};
use topup_core::route::{ChainHeads, Confirmations, RouteFile};
use topup_core::valuation::{SourceId, UnixSeconds};
use uuid::Uuid;

use support::seed::{self, NewAccount, NewAddress};
use support::with_database;

#[tokio::test]
async fn two_pumps_racing_on_one_deposit_apply_exactly_one_transition() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 1).await?;
            let id = insert_deposit(&context.app_pool, seed, 1).await?;
            let control = Arc::new(StepControl::default());
            let steps = blocking_steps(Arc::clone(&control), StepOutcome::Advance);
            let pump = test_pump(&context.app_pool, steps, PumpConfig::default(), 0)?;

            let first_pump = pump.clone();
            let first = tokio::spawn(async move { first_pump.run_once().await });
            control.wait_started().await?;
            let second = pump.run_once().await?;
            ensure!(second == RunOnceResult::Idle);
            control.release();
            ensure!(
                first.await.context("first pump task")??
                    == RunOnceResult::Applied { deposit_id: id }
            );
            ensure!(transition_count(&context.app_pool, id).await? == 1);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn late_step_result_is_stale_after_the_lease_is_reclaimed() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 2).await?;
            let id = insert_deposit(&context.app_pool, seed, 2).await?;
            let control = Arc::new(StepControl::default());
            let slow = test_pump(
                &context.app_pool,
                blocking_steps(Arc::clone(&control), StepOutcome::Advance),
                PumpConfig::default(),
                0,
            )?;
            let slow_task = tokio::spawn(async move { slow.run_once().await });
            control.wait_started().await?;

            sqlx::query(
                "UPDATE deposits SET lease_until = now() - interval '1 second' WHERE id = $1",
            )
            .bind(id)
            .execute(&context.app_pool)
            .await?;
            let fast = test_pump(
                &context.app_pool,
                static_steps(StepOutcome::Advance),
                PumpConfig::default(),
                0,
            )?;
            ensure!(fast.run_once().await? == RunOnceResult::Applied { deposit_id: id });
            control.release();
            ensure!(
                slow_task.await.context("slow pump task")??
                    == RunOnceResult::Stale { deposit_id: id }
            );
            ensure!(transition_count(&context.app_pool, id).await? == 1);
            let stored = db::get_deposit(&context.app_pool, id)
                .await?
                .context("deposit must exist")?;
            ensure!(stored.state == DepositState::Confirmed);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_dead_workers_lease_expires_and_the_deposit_is_reclaimed() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 3).await?;
            let id = insert_deposit(&context.app_pool, seed, 3).await?;
            let dead_worker_lease = Uuid::new_v4();
            let claimed = db::claim_deposit(&context.app_pool, dead_worker_lease)
                .await?
                .context("dead worker should claim the deposit")?;
            ensure!(claimed.id == id);

            let second = test_pump(
                &context.app_pool,
                static_steps(StepOutcome::Wait {
                    reason: WaitReason::Paused,
                }),
                PumpConfig {
                    wait_interval: StdDuration::from_secs(5),
                    ..PumpConfig::default()
                },
                0,
            )?;
            ensure!(second.run_once().await? == RunOnceResult::Idle);

            sqlx::query(
                "UPDATE deposits SET lease_until = now() - interval '1 second' WHERE id = $1",
            )
            .bind(id)
            .execute(&context.app_pool)
            .await?;
            ensure!(second.run_once().await? == RunOnceResult::Applied { deposit_id: id });
            ensure!(transition_count(&context.app_pool, id).await? == 1);
            ensure!(
                db::get_deposit(&context.app_pool, id)
                    .await?
                    .context("deposit must exist")?
                    .attempt
                    == 0
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn step_evidence_and_events_commit_with_the_transition() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 9).await?;
            let id = insert_deposit(&context.app_pool, seed, 13).await?;
            let event_id = Uuid::new_v4();
            let result = StepResult {
                outcome: StepOutcome::Advance,
                evidence: json!({"provider": "test", "confirmed": true}),
                events: vec![OutboxEvent {
                    id: event_id,
                    event_type: "deposit.credited".to_owned(),
                    account_id: seed.account_id,
                    livemode: true,
                    object: EventObject::Deposit(id),
                    next_attempt_at: Utc::now(),
                    actor: topup::db::SYSTEM_ACTOR.to_owned(),
                    request: None,
                    signing_key_version: None,
                }],
                effects: TransitionEffects {
                    mark_final: false,
                    dual_verified: false,
                    sanctions_hit: false,
                    canonical_evidence: None,
                    valuation: Some(StoredValuation {
                        valuation_at: Utc::now(),
                        price_scaled: 12_345_678,
                        price_source: "spot".to_owned(),
                        credit_minor: MinorAmount::new(1_234),
                        quote: json!({"primary": {"source": "test"}}),
                    }),
                    lock_consumption: None,
                },
            };
            let pump = test_pump(
                &context.app_pool,
                result_steps(result),
                PumpConfig::default(),
                0,
            )?;

            ensure!(pump.run_once().await? == RunOnceResult::Applied { deposit_id: id });
            let evidence: serde_json::Value =
                sqlx::query("SELECT evidence FROM transitions WHERE deposit_id = $1")
                    .bind(id)
                    .fetch_one(&context.app_pool)
                    .await?
                    .try_get(0)?;
            ensure!(evidence == json!({"provider": "test", "confirmed": true}));
            let event_count: i64 = sqlx::query("SELECT count(*) FROM events WHERE id = $1")
                .bind(event_id)
                .fetch_one(&context.app_pool)
                .await?
                .try_get(0)?;
            ensure!(event_count == 1);
            let stored = db::get_deposit(&context.app_pool, id)
                .await?
                .context("deposit")?;
            ensure!(stored.state == DepositState::Confirmed);
            ensure!(stored.price_scaled == Some(12_345_678));
            ensure!(stored.price_source.as_deref() == Some("spot"));
            ensure!(stored.credit_minor == Some(MinorAmount::new(1_234)));
            ensure!(stored.quote == Some(json!({"primary": {"source": "test"}})));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn two_pumps_consume_one_rate_lock_only_once() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 14).await?;
            let first_id = insert_deposit(&context.app_pool, seed, 14).await?;
            let second_id = insert_deposit(&context.app_pool, seed, 15).await?;
            sqlx::query(
                r#"
                UPDATE quotes
                SET route = 'phala-cloud-ethereum-pha-usd', amount_atomic = 1000,
                    price_scaled = 9000000, credit_minor = 777,
                    expires_at = now() + interval '15 minutes', status = 'open',
                    exposure_reserved = true, closed_at = NULL
                WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)
                "#,
            )
            .bind(seed.address_id)
            .execute(&context.app_pool)
            .await?;
            let first = db::get_deposit(&context.app_pool, first_id)
                .await?
                .context("first deposit")?;
            let second = db::get_deposit(&context.app_pool, second_id)
                .await?
                .context("second deposit")?;
            let logs = vec![
                transfer_log(&first, evm_address(14)),
                transfer_log(&second, evm_address(14)),
            ];
            let pump = test_pump(
                &context.app_pool,
                confirm_steps(
                    &context.app_pool,
                    logs,
                    Some(Arc::new(Barrier::new(4))),
                ),
                PumpConfig::default(),
                0,
            )?;
            let (first, second) = tokio::join!(pump.run_once(), pump.run_once());
            let results = [first?, second?];
            ensure!(
                results
                    .iter()
                    .filter(|result| matches!(result, RunOnceResult::Applied { .. }))
                    .count()
                    == 1
            );
            ensure!(
                results
                    .iter()
                    .filter(|result| matches!(result, RunOnceResult::Contended { .. }))
                    .count()
                    == 1
            );
            let consumed_by: Uuid =
                sqlx::query_scalar("SELECT consumed_by FROM quotes WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)")
                    .bind(seed.address_id)
                    .fetch_one(&context.app_pool)
                    .await?;
            ensure!(consumed_by == first_id || consumed_by == second_id);
            let confirmed: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM deposits WHERE id IN ($1, $2) AND state = 'confirmed'",
            )
            .bind(first_id)
            .bind(second_id)
            .fetch_one(&context.app_pool)
            .await?;
            ensure!(confirmed == 1);
            let locked: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM deposits WHERE id IN ($1, $2) AND price_source = 'lock' AND credit_minor = 777",
            )
            .bind(first_id)
            .bind(second_id)
            .fetch_one(&context.app_pool)
            .await?;
            ensure!(locked == 1);
            let (lock_status, reserved): (String, bool) = sqlx::query_as(
                "SELECT status, exposure_reserved FROM quotes WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)",
            )
            .bind(seed.address_id)
            .fetch_one(&context.app_pool)
            .await?;
            ensure!(lock_status == "consumed" && !reserved);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn confirm_uses_lock_only_within_amount_and_time_tolerance() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let cases = [
                ("within", 19_u8, 1_010_u64, 900_i64, "lock", 777_u64, true),
                ("under", 20, 989, 900, "spot", 98, false),
                ("over", 21, 1_011, 900, "spot", 101, false),
                ("late", 22, 1_000, -1, "spot", 100, false),
            ];

            for (name, number, amount, expiry_seconds, source, credit, consumed) in cases {
                let seed = seed_account(&context.app_pool, number).await?;
                let deposit_id = insert_deposit(&context.app_pool, seed, number).await?;
                sqlx::query("UPDATE deposits SET amount_atomic = $2::text::numeric WHERE id = $1")
                    .bind(deposit_id)
                    .bind(amount.to_string())
                    .execute(&context.app_pool)
                    .await?;
                // Chain block times are whole seconds; pin one and derive the expiry from
                // it so the window check does not depend on when the second ticks over.
                let block_time = DateTime::from_timestamp(Utc::now().timestamp(), 0)
                    .context("whole-second block time")?;
                sqlx::query("UPDATE deposits SET block_time = $2 WHERE id = $1")
                    .bind(deposit_id)
                    .bind(block_time)
                    .execute(&context.app_pool)
                    .await?;
                let expires_at = block_time + Duration::seconds(expiry_seconds);
                sqlx::query(
                    r#"
                    UPDATE quotes
                    SET route = 'phala-cloud-ethereum-pha-usd', amount_atomic = 1000,
                        price_scaled = 9000000, credit_minor = 777, expires_at = $2,
                        status = 'open', exposure_reserved = true, closed_at = NULL
                    WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)
                    "#,
                )
                .bind(seed.address_id)
                .bind(expires_at)
                .execute(&context.app_pool)
                .await?;

                let deposit = db::get_deposit(&context.app_pool, deposit_id)
                    .await?
                    .context("rate-lock deposit")?;
                let pump = test_pump(
                    &context.app_pool,
                    confirm_steps(
                        &context.app_pool,
                        vec![transfer_log(&deposit, evm_address(number))],
                        None,
                    ),
                    PumpConfig::default(),
                    0,
                )?;
                ensure!(pump.run_once().await? == RunOnceResult::Applied { deposit_id });

                let stored = db::get_deposit(&context.app_pool, deposit_id)
                    .await?
                    .context("confirmed rate-lock deposit")?;
                ensure!(stored.price_source.as_deref() == Some(source), "{name}");
                ensure!(
                    stored.credit_minor == Some(MinorAmount::new(credit)),
                    "{name}"
                );
                let (lock_status, reserved): (String, bool) = sqlx::query_as(
                    "SELECT status, exposure_reserved FROM quotes WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)",
                )
                .bind(seed.address_id)
                .fetch_one(&context.app_pool)
                .await?;
                ensure!(
                    lock_status == if consumed { "consumed" } else { "open" },
                    "{name}"
                );
                ensure!(reserved != consumed, "{name}");

                // The confirmed deposit stays claimable; move it out of the queue so the next
                // case's pump cannot claim it ahead of that case's deposit on a slow setup.
                sqlx::query(
                    "UPDATE deposits SET next_attempt_at = now() + interval '1 day' WHERE id = $1",
                )
                .bind(deposit_id)
                .execute(&context.app_pool)
                .await?;
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn in_window_payment_finalized_after_the_window_never_emits_expired() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 23).await?;
            let deposit_id = insert_deposit(&context.app_pool, seed, 23).await?;
            let expires_at = Utc::now() - Duration::seconds(1);
            sqlx::query("UPDATE deposits SET block_time = $2 WHERE id = $1")
                .bind(deposit_id)
                .bind(expires_at - Duration::seconds(1))
                .execute(&context.app_pool)
                .await?;
            sqlx::query(
                r#"
                UPDATE quotes
                SET route = 'phala-cloud-ethereum-pha-usd', amount_atomic = 1000,
                    price_scaled = 9000000, credit_minor = 777, expires_at = $2,
                    status = 'open', exposure_reserved = true, closed_at = NULL
                WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)
                "#,
            )
            .bind(seed.address_id)
            .bind(expires_at)
            .execute(&context.app_pool)
            .await?;

            // The scanner has committed through a finalized block past the window, and with it
            // the in-window payment, which still awaits the confirm step.
            sqlx::query(
                "INSERT INTO cursors (chain_id, scanned_block, scanned_block_time) VALUES (1, 0, $1)",
            )
            .bind(Utc::now())
            .execute(&context.app_pool)
            .await?;
            ensure!(topup::locks::expire_once(&context.app_pool, &RouteSet::default()).await? == 0);
            let deposit = db::get_deposit(&context.app_pool, deposit_id)
                .await?
                .context("expiry-race deposit")?;
            let pump = test_pump(
                &context.app_pool,
                confirm_steps(
                    &context.app_pool,
                    vec![transfer_log(&deposit, evm_address(23))],
                    None,
                ),
                PumpConfig::default(),
                0,
            )?;
            ensure!(pump.run_once().await? == RunOnceResult::Applied { deposit_id });

            let stored = db::get_deposit(&context.app_pool, deposit_id)
                .await?
                .context("confirmed expiry-race deposit")?;
            ensure!(stored.price_source.as_deref() == Some("lock"));
            ensure!(stored.credit_minor == Some(MinorAmount::new(777)));
            let (lock_status, reserved): (String, bool) = sqlx::query_as(
                "SELECT status, exposure_reserved FROM quotes WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)",
            )
            .bind(seed.address_id)
            .fetch_one(&context.app_pool)
            .await?;
            ensure!(lock_status == "consumed" && !reserved);
            ensure!(topup::locks::expire_once(&context.app_pool, &RouteSet::default()).await? == 0);
            let expired_events: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM events WHERE type = 'quote.expired'",
            )
            .fetch_one(&context.app_pool)
            .await?;
            ensure!(expired_events == 0);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn cancelled_lock_payment_is_credited_at_spot() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 18).await?;
            sqlx::query(
                r#"
                UPDATE quotes
                SET route = 'phala-cloud-ethereum-pha-usd', amount_atomic = 1000,
                    price_scaled = 9000000, credit_minor = 777,
                    expires_at = now() + interval '15 minutes', status = 'cancelled',
                    closed_at = now()
                WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)
                "#,
            )
            .bind(seed.address_id)
            .execute(&context.app_pool)
            .await?;
            let deposit_id = insert_deposit(&context.app_pool, seed, 18).await?;
            let deposit = db::get_deposit(&context.app_pool, deposit_id)
                .await?
                .context("cancelled-lock deposit")?;
            let pump = test_pump(
                &context.app_pool,
                confirm_steps(
                    &context.app_pool,
                    vec![transfer_log(&deposit, evm_address(18))],
                    None,
                ),
                PumpConfig::default(),
                0,
            )?;
            ensure!(pump.run_once().await? == RunOnceResult::Applied { deposit_id });
            let stored = db::get_deposit(&context.app_pool, deposit_id)
                .await?
                .context("confirmed cancelled-lock deposit")?;
            ensure!(stored.price_source.as_deref() == Some("spot"));
            ensure!(stored.credit_minor == Some(MinorAmount::new(100)));
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn step_timeout_is_persisted_as_a_retry() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 7).await?;
            let id = insert_deposit(&context.app_pool, seed, 10).await?;
            let steps = StepSet::new(
                Box::new(SlowStep),
                Box::new(StaticStep(step_result(StepOutcome::Advance))),
            );
            let pump = test_pump(
                &context.app_pool,
                steps,
                PumpConfig {
                    step_timeout: StdDuration::from_millis(10),
                    ..PumpConfig::default()
                },
                u64::MAX,
            )?;
            ensure!(pump.run_once().await? == RunOnceResult::Applied { deposit_id: id });
            let stored = db::get_deposit(&context.app_pool, id)
                .await?
                .context("deposit must exist")?;
            ensure!(stored.state == DepositState::Detected && stored.attempt == 1);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn retry_wait_and_advance_maintain_attempt_and_schedule_contracts() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 4).await?;

            let retry_id = insert_deposit(&context.app_pool, seed, 4).await?;
            sqlx::query("UPDATE deposits SET attempt = 2 WHERE id = $1")
                .bind(retry_id)
                .execute(&context.app_pool)
                .await?;
            let before_retry = Utc::now();
            let retry_pump = test_pump(
                &context.app_pool,
                static_steps(StepOutcome::Retry {
                    error: RetryError::Transient,
                }),
                PumpConfig::default(),
                0,
            )?;
            retry_pump.run_once().await?;
            let retry = db::get_deposit(&context.app_pool, retry_id)
                .await?
                .context("retry deposit")?;
            ensure!(retry.attempt == 3 && retry.state == DepositState::Detected);
            ensure!(retry.next_attempt_at >= before_retry + Duration::seconds(120));
            ensure!(retry.next_attempt_at <= Utc::now() + Duration::seconds(121));

            let wait_id = insert_deposit(&context.app_pool, seed, 5).await?;
            sqlx::query("UPDATE deposits SET attempt = 2 WHERE id = $1")
                .bind(wait_id)
                .execute(&context.app_pool)
                .await?;
            let before_wait = Utc::now();
            let wait_pump = test_pump(
                &context.app_pool,
                static_steps(StepOutcome::Wait {
                    reason: WaitReason::Paused,
                }),
                PumpConfig {
                    wait_interval: StdDuration::from_secs(5),
                    ..PumpConfig::default()
                },
                0,
            )?;
            wait_pump.run_once().await?;
            let wait = db::get_deposit(&context.app_pool, wait_id)
                .await?
                .context("wait deposit")?;
            ensure!(wait.attempt == 2 && wait.state == DepositState::Detected);
            ensure!(wait.next_attempt_at >= before_wait + Duration::seconds(5));
            ensure!(wait.next_attempt_at <= Utc::now() + Duration::seconds(6));

            let advance_id = insert_deposit(&context.app_pool, seed, 6).await?;
            sqlx::query("UPDATE deposits SET attempt = 2 WHERE id = $1")
                .bind(advance_id)
                .execute(&context.app_pool)
                .await?;
            let advance_pump = test_pump(
                &context.app_pool,
                static_steps(StepOutcome::Advance),
                PumpConfig::default(),
                0,
            )?;
            advance_pump.run_once().await?;
            let advance = db::get_deposit(&context.app_pool, advance_id)
                .await?
                .context("advance deposit")?;
            ensure!(advance.attempt == 0 && advance.state == DepositState::Confirmed);
            ensure!(advance.next_attempt_at <= Utc::now());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn graceful_shutdown_finishes_in_flight_work_and_claims_nothing_else() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 5).await?;
            let first_id = insert_deposit(&context.app_pool, seed, 7).await?;
            let second_id = insert_deposit(&context.app_pool, seed, 8).await?;
            let control = Arc::new(StepControl::default());
            let pump = test_pump(
                &context.app_pool,
                blocking_steps(
                    Arc::clone(&control),
                    StepOutcome::Wait {
                        reason: WaitReason::Paused,
                    },
                ),
                PumpConfig {
                    wait_interval: StdDuration::from_secs(5),
                    idle_poll_interval: StdDuration::from_millis(10),
                    ..PumpConfig::default()
                },
                0,
            )?;
            let cancellation = CancellationToken::new();
            let worker_cancellation = cancellation.clone();
            let worker = tokio::spawn(async move {
                pump.run(worker_cancellation).await;
            });
            control.wait_started().await?;
            cancellation.cancel();
            control.release();
            tokio::time::timeout(StdDuration::from_secs(2), worker)
                .await
                .context("pump did not stop")?
                .context("pump task failed")?;

            ensure!(total_transition_count(&context.app_pool).await? == 1);
            let claimed = control.claimed_ids();
            ensure!(claimed.len() == 1);
            let claimed_id = claimed[0];
            ensure!(claimed_id == first_id || claimed_id == second_id);
            let untouched_id = if claimed_id == first_id {
                second_id
            } else {
                first_id
            };
            let untouched = db::get_deposit(&context.app_pool, untouched_id)
                .await?
                .context("untouched deposit")?;
            ensure!(untouched.lease_token.is_none());
            ensure!(transition_count(&context.app_pool, untouched_id).await? == 0);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn age_alert_uses_the_route_threshold_of_the_current_state() -> Result<()> {
    with_database(|context| {
        Box::pin(async move {
            let seed = seed_account(&context.app_pool, 6).await?;
            let id = insert_deposit(&context.app_pool, seed, 9).await?;
            sqlx::query(
                "UPDATE deposits SET created_at = now() - interval '2 hours' WHERE id = $1",
            )
            .bind(id)
            .execute(&context.app_pool)
            .await?;
            let yaml = include_str!("fixtures/phala-cloud-pha.yaml");
            let mut route: RouteFile = serde_saphyr::from_str(yaml)?;
            route.alerts.stuck_after_s.detected = 1;
            route.alerts.stuck_after_s.confirmed = 3 * 60 * 60;
            let config = AgeAlertConfig::from_routes(&[route])?;
            let alerter =
                AgeAlerter::new(context.app_pool.clone(), config, StdDuration::from_secs(60));
            ensure!(alerter.scan_once().await? == 1);

            sqlx::query("UPDATE deposits SET state = 'confirmed' WHERE id = $1")
                .bind(id)
                .execute(&context.app_pool)
                .await?;
            ensure!(alerter.scan_once().await? == 0);
            Ok(())
        })
    })
    .await
}

#[derive(Clone)]
struct StaticStep(StepResult);

#[async_trait]
impl Step for StaticStep {
    async fn run(&self, _deposit: &db::Deposit) -> StepResult {
        self.0.clone()
    }
}

struct SlowStep;

#[async_trait]
impl Step for SlowStep {
    async fn run(&self, _deposit: &db::Deposit) -> StepResult {
        tokio::time::sleep(StdDuration::from_secs(5)).await;
        step_result(StepOutcome::Advance)
    }
}

struct StepControl {
    started: Semaphore,
    release: Semaphore,
    claimed: Mutex<Vec<Uuid>>,
}

impl Default for StepControl {
    fn default() -> Self {
        Self {
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            claimed: Mutex::new(Vec::new()),
        }
    }
}

impl StepControl {
    async fn wait_started(&self) -> Result<()> {
        self.started
            .acquire()
            .await
            .context("started semaphore closed")?
            .forget();
        Ok(())
    }

    fn release(&self) {
        self.release.add_permits(1);
    }

    fn claimed_ids(&self) -> Vec<Uuid> {
        self.claimed.lock().expect("claimed lock poisoned").clone()
    }
}

struct BlockingStep {
    control: Arc<StepControl>,
    outcome: StepOutcome,
}

#[async_trait]
impl Step for BlockingStep {
    async fn run(&self, deposit: &db::Deposit) -> StepResult {
        self.control
            .claimed
            .lock()
            .expect("claimed lock poisoned")
            .push(deposit.id);
        self.control.started.add_permits(1);
        if let Ok(permit) = self.control.release.acquire().await {
            permit.forget();
        }
        step_result(self.outcome)
    }
}

struct FixedJitter(u64);

impl JitterSource for FixedJitter {
    fn next_u64(&self) -> u64 {
        self.0
    }
}

fn static_steps(outcome: StepOutcome) -> StepSet {
    result_steps(step_result(outcome))
}

fn result_steps(result: StepResult) -> StepSet {
    StepSet::new(
        Box::new(StaticStep(result.clone())),
        Box::new(StaticStep(result)),
    )
}

fn step_result(outcome: StepOutcome) -> StepResult {
    StepResult::new(outcome, json!({"source": "test"}))
}

fn blocking_steps(control: Arc<StepControl>, outcome: StepOutcome) -> StepSet {
    StepSet::new(
        Box::new(BlockingStep { control, outcome }),
        Box::new(StaticStep(step_result(outcome))),
    )
}

fn test_pump(pool: &PgPool, steps: StepSet, config: PumpConfig, jitter: u64) -> Result<Pump> {
    Pump::with_jitter(
        pool.clone(),
        Arc::default(),
        Arc::new(steps),
        config,
        Arc::new(FixedJitter(jitter)),
    )
    .map_err(Into::into)
}

#[derive(Clone)]
struct ConfirmChain {
    logs: Arc<Vec<TransferLog>>,
    barrier: Option<Arc<Barrier>>,
}

impl ChainReader for ConfirmChain {
    async fn factory_logs(
        &self,
        _factory: Address,
        _forwarders: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<topup_adapters::chain::evm::FactoryLog>, ChainError> {
        panic!("the confirm step never reads factory events")
    }

    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
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
        panic!("confirm must locate transfers by receipt identity")
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
        tx_hash: B256,
        receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        Ok(self
            .logs
            .iter()
            .find(|log| log.tx_hash == tx_hash && log.receipt_log_index == receipt_log_index)
            .cloned()
            .map_or(ReceiptLookup::Missing, |log| ReceiptLookup::Included {
                block_number: log.block_number,
                block_hash: log.block_hash,
                block_time: log.block_time,
                status: true,
                tx_from: log.tx_from,
                tx_nonce: log.tx_nonce,
                transfer: Some(Box::new(log)),
            }))
    }

    async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
        panic!("the confirm step never reads nonces")
    }
}

struct FixedPrice(Observation);

#[async_trait]
impl PriceSource for FixedPrice {
    async fn observe(&self) -> Result<Observation, PriceError> {
        Ok(self.0.clone())
    }
}

fn confirm_steps(pool: &PgPool, logs: Vec<TransferLog>, barrier: Option<Arc<Barrier>>) -> StepSet {
    let chain = ConfirmChain {
        logs: Arc::new(logs),
        barrier,
    };
    let observed_at = UnixSeconds::new(
        u64::try_from(Utc::now().timestamp()).expect("current timestamp must be non-negative"),
    );
    let price = |source: &str, value| {
        Arc::new(FixedPrice(Observation {
            source: SourceId::new(source),
            price: ScaledPrice::new(value, PRICE_SCALE).expect("test price"),
            observed_at,
        })) as Arc<dyn PriceSource>
    };
    let confirm = ConfirmStep::single(
        pool.clone(),
        confirmation_route(),
        chain.clone(),
        chain,
        price("primary", 10_000_000),
        Some(price("check", 10_000_000)),
        Some(price("fx", 100_000_000)),
    );
    static_steps(StepOutcome::Wait {
        reason: WaitReason::Paused,
    })
    .with_detected(Box::new(confirm))
}

fn confirmation_route() -> RouteFile {
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))
            .expect("route fixture");
    route.asset.contract = evm_address(200);
    route.asset.decimals = 2;
    route.asset.quote_amount_decimals = 2;
    route.merchant.min_amount = topup_core::route::Bounded::at(1);
    route
}

fn transfer_log(deposit: &db::Deposit, recipient: Address) -> TransferLog {
    TransferLog {
        tx_hash: deposit.tx_hash,
        log_index: deposit.log_index,
        receipt_log_index: deposit.log_index,
        tx_from: alloy_primitives::Address::ZERO,
        tx_nonce: 0,
        block_number: deposit.block_number,
        block_hash: deposit.block_hash,
        block_time: deposit.block_time,
        token: deposit.asset_contract,
        from: deposit.from_address,
        to: recipient,
        amount: deposit.amount_atomic,
    }
}

#[derive(Clone, Copy)]
struct Seed {
    account_id: Uuid,
    address_id: Uuid,
}

async fn seed_account(pool: &PgPool, number: u8) -> Result<Seed> {
    let (account, customer) = seed::create_account_and_customer(
        pool,
        &NewAccount {
            webhook_url: format!("https://product-{number}.test/webhooks"),
            ..NewAccount::named(&format!("product-{number}"))
        },
        &format!("workspace-{number}"),
    )
    .await?;
    seed::accept_assets(pool, account.id, true, 1, &["pha"]).await?;
    let address = NewAddress {
        id: Uuid::new_v4(),
        customer_id: customer.id,
        chain_id: 1,
        route: "phala-cloud-ethereum-pha-usd".to_owned(),
        salt: b256(number),
        address: evm_address(number),
    };
    seed::insert_address(pool, &address).await?;
    Ok(Seed {
        account_id: account.id,
        address_id: address.id,
    })
}

async fn insert_deposit(pool: &PgPool, seed: Seed, number: u8) -> Result<Uuid> {
    let deposit = NewDeposit {
        chain_id: 1,
        tx_hash: b256(number),
        log_index: 0,
        receipt_log_index: 0,
        tx_from: alloy_primitives::Address::ZERO,
        tx_nonce: 0,
        is_final: true,
        block_number: 100 + u64::from(number),
        block_hash: b256(number.wrapping_add(1)),
        block_time: Utc::now(),
        address_id: seed.address_id,
        route: Some("phala-cloud-ethereum-pha-usd".to_owned()),
        route_version: Some(1),
        asset_contract: evm_address(200),
        from_address: evm_address(number.wrapping_add(100)),
        amount_atomic: AtomicAmount::new(U256::from(1_000)),
        state: DepositState::Detected,
        reason: None,
        next_attempt_at: Utc::now() - Duration::seconds(1),
    };
    let id = deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
    ensure!(db::insert_deposit(pool, &deposit).await?);
    Ok(id)
}

async fn transition_count(pool: &PgPool, deposit_id: Uuid) -> Result<i64> {
    Ok(
        sqlx::query("SELECT count(*) FROM transitions WHERE deposit_id = $1")
            .bind(deposit_id)
            .fetch_one(pool)
            .await?
            .try_get(0)?,
    )
}

async fn total_transition_count(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query("SELECT count(*) FROM transitions")
        .fetch_one(pool)
        .await?
        .try_get(0)?)
}

fn evm_address(byte: u8) -> Address {
    Address::from([byte; 20])
}

fn b256(byte: u8) -> B256 {
    B256::from([byte; 32])
}
