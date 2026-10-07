//! PostgreSQL-backed reconciliation checks, repairs, freezes, and cancellation behavior.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use topup::db::{self, Deposit, NewDeposit};
use topup::pump::{Pump, PumpConfig, RunOnceResult, Step, StepResult, StepSet};
use topup::reconciler::{
    CheckName, Reconciler, ReconciliationChain, ReconciliationError, ReconciliationReport,
    frozen_chains, hold_lease_owner_lock,
};
use topup_adapters::chain::evm::{ChainError, ChainReader, FinalizedHead, TransferLog};
use topup_core::deposit::{DepositState, StepOutcome};
use topup_core::identity::deposit_id;
use topup_core::money::AtomicAmount;
use topup_core::route::RouteFile;
use uuid::Uuid;

use support::TestDatabase;
use support::seed::{self, NewAccount, NewAddress, NewCustomer};

const CHAIN_ID: u64 = 31_337;

#[derive(Default)]
struct MockChain {
    finalized: AtomicU64,
    derivation_delay: StdDuration,
    derivation_started: Notify,
    fail_derivation: AtomicBool,
    fail_balances: AtomicBool,
    balances: Mutex<BTreeMap<Address, U256>>,
    balance_reads: Mutex<Vec<(u64, Vec<Address>)>>,
    derived: Mutex<BTreeMap<B256, Address>>,
    derivation_reads: Mutex<Vec<Vec<B256>>>,
}

impl MockChain {
    fn at(finalized: u64) -> Self {
        Self {
            finalized: AtomicU64::new(finalized),
            ..Self::default()
        }
    }

    fn derive(&self, seeds: &[&Seed]) {
        let mut derived = self.derived.lock().unwrap();
        for seed in seeds {
            derived.insert(seed.salt, seed.address);
        }
    }
}

#[async_trait]
impl ReconciliationChain for MockChain {
    async fn token_balances_pinned(
        &self,
        _token: Address,
        addresses: &[Address],
        hash: B256,
    ) -> Result<Vec<U256>, ReconciliationError> {
        if hash != B256::repeat_byte(24) {
            return Err(ReconciliationError::Invariant(
                "fixture custody pin changed",
            ));
        }
        let block = self.finalized.load(Ordering::SeqCst).min(140);
        self.balance_reads
            .lock()
            .unwrap()
            .push((block, addresses.to_vec()));
        if self.fail_balances.load(Ordering::SeqCst) {
            return Err(ReconciliationError::Chain(
                "balance endpoint unavailable".into(),
            ));
        }
        let balances = self.balances.lock().unwrap();
        Ok(addresses
            .iter()
            .map(|address| balances.get(address).copied().unwrap_or(U256::ZERO))
            .collect())
    }

    async fn factory_addresses(
        &self,
        _factory: Address,
        _treasury: Address,
        salts: &[B256],
    ) -> Result<Vec<Address>, ReconciliationError> {
        self.derivation_started.notify_one();
        tokio::time::sleep(self.derivation_delay).await;
        if self.fail_derivation.load(Ordering::SeqCst) {
            return Err(ReconciliationError::Chain("addressOf timed out".to_owned()));
        }
        self.derivation_reads.lock().unwrap().push(salts.to_vec());
        let derived = self.derived.lock().unwrap();
        Ok(salts
            .iter()
            .map(|salt| derived.get(salt).copied().unwrap_or(Address::ZERO))
            .collect())
    }
}

struct Seed {
    address_id: Uuid,
    salt: B256,
    address: Address,
}

#[tokio::test]
async fn post_restore_refuses_to_preempt_a_running_lease_owner() -> Result<()> {
    with_database(|pool| async move {
        let route = route()?;
        let seed = seed_identity(&pool, &route, 45).await?;
        let deposit_id = seed_deposit(
            &pool,
            &route,
            &seed,
            DepositSeed::new(45, DepositState::Credited),
        )
        .await?;
        let lease_token = Uuid::new_v4();
        sqlx::query(
            "UPDATE deposits SET lease_token = $2, lease_until = now() + interval '5 minutes' WHERE id = $1",
        )
        .bind(deposit_id)
        .bind(lease_token)
        .execute(&pool)
        .await?;
        let chain = Arc::new(MockChain::at(150));
        chain.derive(&[&seed]);
        let reconciler = reconciler(&pool, route, chain)?;

        let service = hold_lease_owner_lock(&pool).await?;
        let second_service = hold_lease_owner_lock(&pool).await?;
        let refused = reconciler.post_restore_once().await;
        ensure!(matches!(
            refused,
            Err(ReconciliationError::LeaseOwnerLock(_))
        ));
        ensure!(deposit(&pool, deposit_id).await?.lease_token == Some(lease_token));
        second_service.release().await?;
        service.release().await?;

        let report = reconciler.post_restore_once().await?;
        ensure!(!report.incomplete, "restore must complete: {report:?}");
        // The round asks the product nothing and leaves the deposit to its own lease.
        ensure!(deposit(&pool, deposit_id).await?.lease_token == Some(lease_token));
        hold_lease_owner_lock(&pool).await?.release().await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn losing_the_lease_owner_connection_stops_guarded_pumps() -> Result<()> {
    with_database(|pool| async move {
        let shutdown = CancellationToken::new();
        let lock = hold_lease_owner_lock(&pool).await?;
        let watch = tokio::spawn(lock.watch(StdDuration::from_millis(50), shutdown.clone()));
        let calls = Arc::new(AtomicUsize::new(0));
        let step = || Box::new(AdvanceStep(Arc::clone(&calls))) as Box<dyn Step>;
        let pump = Pump::new(
            pool.clone(),
            Arc::default(),
            Arc::new(StepSet::new(step(), step())),
            PumpConfig::default(),
        )?;
        let pump_shutdown = shutdown.child_token();
        let pump_task = tokio::spawn(async move { pump.run(pump_shutdown).await });

        let terminated: bool = sqlx::query_scalar(
            r#"
            SELECT pg_terminate_backend(pid) FROM pg_locks
            WHERE locktype = 'advisory' AND mode = 'ShareLock'
              AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
            "#,
        )
        .fetch_one(&pool)
        .await?;
        ensure!(terminated);
        let watched = tokio::time::timeout(StdDuration::from_secs(10), watch).await??;
        ensure!(watched.is_err());
        ensure!(shutdown.is_cancelled());
        tokio::time::timeout(StdDuration::from_secs(10), pump_task).await??;
        Ok(())
    })
    .await
}

/// Shutdown retains the lock until tasks end, even if its backend dies after cancellation.
#[tokio::test]
async fn losing_the_lock_connection_during_shutdown_does_not_hang_cleanup() -> Result<()> {
    with_database(|pool| async move {
        let shutdown = CancellationToken::new();
        let lock = hold_lease_owner_lock(&pool).await?;
        let watch = tokio::spawn(lock.watch(StdDuration::from_millis(50), shutdown.clone()));
        shutdown.cancel();
        let lock = tokio::time::timeout(StdDuration::from_secs(10), watch).await???;
        let terminated: bool = sqlx::query_scalar(
            "SELECT pg_terminate_backend(pid) FROM pg_locks \
             WHERE locktype = 'advisory' AND mode = 'ShareLock' \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .fetch_one(&pool)
        .await?;
        ensure!(terminated);
        ensure!(
            tokio::time::timeout(StdDuration::from_secs(10), lock.release())
                .await?
                .is_err()
        );
        tokio::time::timeout(StdDuration::from_secs(10), pool.close()).await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn checks_are_independent_and_a_failed_round_recovers() -> Result<()> {
    with_database(|pool| async move {
        let route = route()?;
        let seed = seed_identity(&pool, &route, 61).await?;
        let mispriced_id = seed_deposit(
            &pool,
            &route,
            &seed,
            DepositSeed::new(61, DepositState::Confirmed).credit(101),
        )
        .await?;
        let historical_id = seed_deposit(
            &pool,
            &route,
            &seed,
            DepositSeed::new(62, DepositState::Confirmed),
        )
        .await?;
        sqlx::query("UPDATE deposits SET route_version = 99 WHERE id = $1")
            .bind(historical_id)
            .execute(&pool)
            .await?;
        scanned_through(&pool, 150).await?;
        let chain = Arc::new(MockChain::at(150));
        chain.derive(&[&seed]);
        chain.fail_derivation.store(true, Ordering::SeqCst);
        let reconciler = Reconciler::with_dependencies(
            pool.clone(),
            route_set(route)?,
            BTreeMap::from([(CHAIN_ID, Arc::clone(&chain) as Arc<dyn ReconciliationChain>)]),
        );

        let failed = reconciler.run_once().await?;
        ensure!(failed.failed_checks == [CheckName::AddressDerivation]);
        ensure!(failed.findings.iter().any(|finding| {
            finding.subjects.get("deposit_id") == Some(&mispriced_id.to_string())
                && finding.expected["credit_minor"] == json!("100")
        }));
        ensure!(failed.findings.iter().any(|finding| {
            finding.subjects.get("deposit_id") == Some(&historical_id.to_string())
                && finding.observed["error"] == json!("route_version_unavailable")
        }));
        ensure!(has_check(&failed, CheckName::CustodyBalance));

        chain.fail_derivation.store(false, Ordering::SeqCst);
        let recovered = reconciler.run_once().await?;
        ensure!(recovered.succeeded());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn custody_is_checked_per_forwarder_at_the_indexed_finalized_block() -> Result<()> {
    with_database(|pool| async move {
        let route = route()?;
        let token = route.asset.contract;
        let swept = seed_identity(&pool, &route, 71).await?;
        let unsettled = seed_identity(&pool, &route, 72).await?;
        let idle = seed_identity(&pool, &route, 73).await?;
        // Swept to zero by its ledger: its balance is not read.
        let emptied = seed_identity(&pool, &route, 74).await?;
        seed_deposit(
            &pool,
            &route,
            &emptied,
            DepositSeed::new(75, DepositState::Swept)
                .block(100)
                .amount(250),
        )
        .await?;
        seed_flushed(&pool, &route, &emptied, 120, 4, 250).await?;
        seed_deposit(
            &pool,
            &route,
            &swept,
            DepositSeed::new(71, DepositState::Swept)
                .block(100)
                .amount(1_000),
        )
        .await?;
        // Above the checked block: neither its balance nor its deposit counts yet.
        seed_deposit(
            &pool,
            &route,
            &swept,
            DepositSeed::new(72, DepositState::Detected)
                .block(145)
                .amount(500),
        )
        .await?;
        seed_flushed(&pool, &route, &swept, 120, 3, 400).await?;
        // A reversed deposit is not in the balance.
        let reversed = seed_deposit(
            &pool,
            &route,
            &swept,
            DepositSeed::new(74, DepositState::Credited)
                .block(110)
                .amount(9),
        )
        .await?;
        sqlx::query("UPDATE deposits SET state = 'reversed', final_at = NULL WHERE id = $1")
            .bind(reversed)
            .execute(&pool)
            .await?;
        // The finality watch has not settled this deposit, so its forwarder waits a round.
        let pending = seed_deposit(
            &pool,
            &route,
            &unsettled,
            DepositSeed::new(73, DepositState::Credited)
                .block(100)
                .amount(700),
        )
        .await?;
        sqlx::query("UPDATE deposits SET final_at = NULL WHERE id = $1")
            .bind(pending)
            .execute(&pool)
            .await?;

        let chain = Arc::new(MockChain::at(150));
        chain.derive(&[&swept, &unsettled, &idle, &emptied]);
        chain
            .balances
            .lock()
            .unwrap()
            .insert(swept.address, U256::from(600_u64));
        let reconciler = reconciler(&pool, route.clone(), chain.clone())?;

        // Nothing is compared before the scanner has indexed a range.
        ensure!(
            reconciler
                .check(CheckName::CustodyBalance)
                .await?
                .is_empty()
        );
        ensure!(chain.balance_reads.lock().unwrap().is_empty());

        // The scanner lags the finalized head: the check reads at the scanner's block, where every
        // transfer and factory event is indexed, and only forwarders whose settled ledger holds
        // unswept funds.
        scanned_through(&pool, 140).await?;
        // Finality advances independently beyond coverage. This distinct hash is deliberately
        // rejected by the chain fixture: custody at it would compare unindexed state.
        db::chain_reads::advance_checkpoint(
            &pool,
            CHAIN_ID,
            db::chain_reads::Boundary {
                number: 150,
                hash: B256::repeat_byte(25),
                time: Utc::now(),
            },
        )
        .await?;
        ensure!(
            reconciler
                .check(CheckName::CustodyBalance)
                .await?
                .is_empty()
        );
        ensure!(
            chain.balance_reads.lock().unwrap().as_slice()
                == [(140, vec![swept.address]), (140, vec![swept.address])]
        );
        ensure!(
            frozen_chains(&pool, &*route_set(route.clone())?)
                .await?
                .is_empty()
        );

        // A forwarder holding more than its deposits minus its sweeps freezes the chain.
        chain
            .balances
            .lock()
            .unwrap()
            .insert(swept.address, U256::from(601_u64));
        let findings = reconciler.check(CheckName::CustodyBalance).await?;
        ensure!(findings.len() == 1, "unexpected findings: {findings:?}");
        ensure!(findings[0].subjects["address_id"] == swept.address_id.to_string());
        ensure!(findings[0].subjects["token"] == format!("{token:#x}"));
        ensure!(findings[0].expected["deposits_atomic"] == json!("1000"));
        ensure!(findings[0].expected["flushed_atomic"] == json!("400"));
        ensure!(findings[0].observed["balance_atomic"] == json!("601"));
        ensure!(frozen_chains(&pool, &*route_set(route)?).await? == BTreeSet::from([CHAIN_ID]));
        let block: String = sqlx::query_scalar(
            "SELECT check_name FROM reconciliation_blocks WHERE block_key = 'chain:31337'",
        )
        .fetch_one(&pool)
        .await?;
        ensure!(block == "custody_balance");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn custody_requires_full_dual_balance_agreement_even_when_read_matches() -> Result<()> {
    with_database(|pool| async move {
        let route = route()?;
        let first = seed_identity(&pool, &route, 81).await?;
        let second = seed_identity(&pool, &route, 82).await?;
        seed_deposit(
            &pool,
            &route,
            &first,
            DepositSeed::new(81, DepositState::Credited)
                .block(100)
                .amount(100),
        )
        .await?;
        seed_deposit(
            &pool,
            &route,
            &second,
            DepositSeed::new(82, DepositState::Credited)
                .block(100)
                .amount(200),
        )
        .await?;
        scanned_through(&pool, 140).await?;
        let read = Arc::new(MockChain::at(150));
        let verify = Arc::new(MockChain::at(150));
        for reader in [&read, &verify] {
            reader.balances.lock().unwrap().extend([
                (first.address, U256::from(100)),
                (second.address, U256::from(200)),
            ]);
        }
        let check = reconciler(&pool, route.clone(), read.clone())?.with_verifiers(BTreeMap::from(
            [(CHAIN_ID, verify.clone() as Arc<dyn ReconciliationChain>)],
        ));
        // The first entry agrees; the second does not. A matching read vector cannot clear it.
        verify
            .balances
            .lock()
            .unwrap()
            .insert(second.address, U256::from(201));
        ensure!(
            check.check(CheckName::CustodyBalance).await.is_err(),
            "read-only agreement reported clean custody"
        );
        ensure!(
            frozen_chains(&pool, &*route_set(route.clone())?)
                .await?
                .is_empty()
        );
        verify.fail_balances.store(true, Ordering::SeqCst);
        ensure!(
            check.check(CheckName::CustodyBalance).await.is_err(),
            "an unavailable verifier reported clean custody"
        );
        ensure!(
            frozen_chains(&pool, &*route_set(route.clone())?)
                .await?
                .is_empty()
        );
        verify.fail_balances.store(false, Ordering::SeqCst);
        verify
            .balances
            .lock()
            .unwrap()
            .insert(second.address, U256::from(200));
        ensure!(check.check(CheckName::CustodyBalance).await?.is_empty());
        ensure!(
            verify.balance_reads.lock().unwrap().len() == 3,
            "a clean ledger skipped independent balances"
        );
        read.balances
            .lock()
            .unwrap()
            .insert(first.address, U256::from(101));
        ensure!(check.check(CheckName::CustodyBalance).await.is_err());
        ensure!(
            frozen_chains(&pool, &*route_set(route.clone())?)
                .await?
                .is_empty()
        );
        verify
            .balances
            .lock()
            .unwrap()
            .insert(first.address, U256::from(101));
        ensure!(check.check(CheckName::CustodyBalance).await?.len() == 1);
        ensure!(frozen_chains(&pool, &*route_set(route)?).await? == BTreeSet::from([CHAIN_ID]));
        Ok(())
    })
    .await
}

/// Derivation reads are cached only while the stored derivation inputs remain unchanged.
#[tokio::test]
async fn each_stored_derivation_is_read_once_until_its_row_changes() -> Result<()> {
    with_database(|pool| async move {
        let route = route()?;
        let seed = seed_identity(&pool, &route, 92).await?;
        let chain = Arc::new(MockChain::at(0));
        chain.derive(&[&seed]);
        let reconciler = reconciler(&pool, route.clone(), chain.clone())?;
        ensure!(
            reconciler
                .check(CheckName::AddressDerivation)
                .await?
                .is_empty()
        );
        ensure!(
            reconciler
                .check(CheckName::AddressDerivation)
                .await?
                .is_empty()
        );
        ensure!(chain.derivation_reads.lock().unwrap().as_slice() == [vec![seed.salt]]);

        // A changed row is read again, and a mismatch still freezes the chain.
        sqlx::query("UPDATE addresses SET address = $2 WHERE id = $1")
            .bind(seed.address_id)
            .bind(format!("{:#x}", Address::repeat_byte(0x93)))
            .execute(&pool)
            .await?;
        let findings = reconciler.check(CheckName::AddressDerivation).await?;
        ensure!(findings.len() == 1, "unexpected findings: {findings:?}");
        ensure!(chain.derivation_reads.lock().unwrap().len() == 2);
        ensure!(frozen_chains(&pool, &*route_set(route)?).await? == BTreeSet::from([CHAIN_ID]));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn only_chain_checks_freeze_and_findings_are_idempotent() -> Result<()> {
    with_database(|pool| async move {
        let route = route()?;
        let seed = seed_identity(&pool, &route, 91).await?;
        seed_deposit(
            &pool,
            &route,
            &seed,
            DepositSeed::new(91, DepositState::Confirmed).credit(101),
        )
        .await?;
        let chain = Arc::new(MockChain::at(0));
        chain
            .derived
            .lock()
            .unwrap()
            .insert(seed.salt, Address::from([99; 20]));
        let reconciler = Reconciler::with_dependencies(
            pool.clone(),
            route_set(route)?,
            BTreeMap::from([(CHAIN_ID, Arc::clone(&chain) as Arc<dyn ReconciliationChain>)]),
        );
        let first = reconciler.run_once().await?;
        ensure!(has_check(&first, CheckName::CreditRecomputation));
        ensure!(has_check(&first, CheckName::AddressDerivation));
        ensure!(!first.incomplete);
        let count_after_first =
            count(&pool, "SELECT count(*) FROM reconciliation_findings").await?;
        let audit_after_first = count(
            &pool,
            "SELECT count(*) FROM audit WHERE action = 'reconciliation_mismatch'",
        )
        .await?;
        let _ = reconciler.run_once().await?;
        ensure!(
            count(&pool, "SELECT count(*) FROM reconciliation_findings").await?
                == count_after_first
        );
        ensure!(
            count(
                &pool,
                "SELECT count(*) FROM audit WHERE action = 'reconciliation_mismatch'",
            )
            .await?
                == audit_after_first
        );
        let scopes: Vec<String> =
            sqlx::query_scalar("SELECT scope FROM reconciliation_blocks ORDER BY scope")
                .fetch_all(&pool)
                .await?;
        // A credit mismatch is reported; only a chain check freezes, as nothing is sent.
        ensure!(scopes == ["chain"]);

        // Lifting is manual: a lifted block whose mismatch still reproduces is written again by
        // the next round.
        sqlx::query("DELETE FROM reconciliation_blocks")
            .execute(&pool)
            .await?;
        let _ = reconciler.run_once().await?;
        let scopes: Vec<String> =
            sqlx::query_scalar("SELECT scope FROM reconciliation_blocks ORDER BY scope")
                .fetch_all(&pool)
                .await?;
        // A credit mismatch is reported; only a chain check freezes, as nothing is sent.
        ensure!(scopes == ["chain"]);
        Ok(())
    })
    .await
}

struct AdvanceStep(Arc<AtomicUsize>);

#[async_trait]
impl Step for AdvanceStep {
    async fn run(&self, _deposit: &Deposit) -> StepResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        StepResult::new(StepOutcome::Advance, json!({"outcome": "advance"}))
    }
}

struct UnreachableReader;

impl ChainReader for UnreachableReader {
    async fn factory_logs(
        &self,
        _factory: Address,
        _forwarders: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<topup_adapters::chain::evm::FactoryLog>, ChainError> {
        Err(ChainError::Rpc("unreachable"))
    }

    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Err(ChainError::Rpc("unreachable"))
    }

    async fn transfer_logs_to(
        &self,
        _addresses: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        Err(ChainError::Rpc("unreachable"))
    }

    async fn confirmation_heads(
        &self,
        _confirmations: topup_core::route::Confirmations,
    ) -> Result<topup_core::route::ChainHeads, ChainError> {
        Err(ChainError::Rpc("unreachable"))
    }

    async fn receipt_transfer(
        &self,
        _tx_hash: B256,
        _receipt_log_index: u64,
    ) -> Result<topup_adapters::chain::evm::ReceiptLookup, ChainError> {
        Err(ChainError::Rpc("unreachable"))
    }

    async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
        Err(ChainError::Rpc("unreachable"))
    }
}

#[tokio::test]
async fn frozen_chain_gates_startup_pumps_and_scanner() -> Result<()> {
    with_database(|pool| async move {
        let route = route()?;
        let seed = seed_identity(&pool, &route, 101).await?;
        let deposit_id = seed_deposit(
            &pool,
            &route,
            &seed,
            DepositSeed::new(101, DepositState::Detected),
        )
        .await?;
        ensure!(frozen_chains(&pool, &*route_set(route.clone())?).await?.is_empty());
        sqlx::query(
            r#"
            INSERT INTO reconciliation_blocks (block_key, scope, chain_id, check_name, reason)
            VALUES ('chain:31337', 'chain', 31337, 'address_derivation', 'test freeze')
            "#,
        )
        .execute(&pool)
        .await?;
        ensure!(
            frozen_chains(&pool, &*route_set(route.clone())?).await?
                == BTreeSet::from([CHAIN_ID])
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let step = || Box::new(AdvanceStep(Arc::clone(&calls))) as Box<dyn Step>;
        let pump = Pump::new(
            pool.clone(),
            Arc::default(),
            Arc::new(StepSet::new(step(), step())),
            PumpConfig::default(),
        )?;
        ensure!(pump.run_once().await? == RunOnceResult::Applied { deposit_id });
        ensure!(calls.load(Ordering::SeqCst) == 0);
        let waiting = deposit(&pool, deposit_id).await?;
        ensure!(waiting.state == DepositState::Detected);
        ensure!(waiting.next_attempt_at > Utc::now());
        let reason: Option<String> = sqlx::query_scalar(
            "SELECT evidence->>'reason' FROM transitions WHERE deposit_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(deposit_id)
        .fetch_one(&pool)
        .await?;
        ensure!(reason.as_deref() == Some("chain_frozen"));

        let scanner_routes = topup::scanner::chain_routes(&*route_set(route)?);
        ensure!(topup::scanner::coverage_once(&pool, &UnreachableReader, &UnreachableReader, &scanner_routes[0], 1).await.is_err());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn application_role_cannot_rewrite_findings_or_blocks() -> Result<()> {
    with_database(|pool| async move {
        let checks = [
            ("reconciliation_findings", "SELECT", true),
            ("reconciliation_findings", "INSERT", true),
            ("reconciliation_findings", "UPDATE", false),
            ("reconciliation_findings", "DELETE", false),
            ("reconciliation_findings", "TRUNCATE", false),
            ("reconciliation_blocks", "INSERT", true),
            ("reconciliation_blocks", "UPDATE", false),
            // The admin lift endpoint deletes a block; nothing may rewrite one.
            ("reconciliation_blocks", "DELETE", true),
            ("reconciliation_blocks", "TRUNCATE", false),
            ("reconciliation_deposit_cursors", "UPDATE", true),
            ("reconciliation_deposit_cursors", "DELETE", false),
            // Finalized chain facts are recorded once and never rewritten.
            ("flushed", "INSERT", true),
            ("flushed", "UPDATE", false),
            ("flushed", "DELETE", false),
            ("flush_failures", "INSERT", true),
            ("flush_failures", "UPDATE", false),
            ("flush_failures", "DELETE", false),
        ];
        for (table, privilege, expected) in checks {
            let granted: bool =
                sqlx::query_scalar("SELECT has_table_privilege('topup_app', $1, $2)")
                    .bind(table)
                    .bind(privilege)
                    .fetch_one(&pool)
                    .await?;
            ensure!(
                granted == expected,
                "topup_app {privilege} on {table} should be {expected}"
            );
        }
        sqlx::query(
            r#"
            INSERT INTO reconciliation_blocks (block_key, scope, chain_id, check_name, reason)
            VALUES ('chain:31337', 'chain', 31337, 'address_derivation', 'test freeze')
            "#,
        )
        .execute(&pool)
        .await?;
        let denied = sqlx::query("UPDATE reconciliation_blocks SET chain_id = 1")
            .execute(&pool)
            .await
            .err()
            .and_then(|error| {
                error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .map(|code| code.into_owned())
            });
        ensure!(
            denied.as_deref() == Some("42501"),
            "rewriting a block must be denied"
        );
        ensure!(frozen_chains(&pool, &*route_set(route()?)?).await? == BTreeSet::from([CHAIN_ID]));
        Ok(())
    })
    .await
}

#[tokio::test]
async fn loop_respects_cancellation() -> Result<()> {
    with_database(|pool| async move {
        let route = route()?;
        let seed = seed_identity(&pool, &route, 82).await?;
        let chain = Arc::new(MockChain {
            derivation_delay: StdDuration::from_secs(10),
            ..MockChain::default()
        });
        chain.derive(&[&seed]);
        let reconciler = Arc::new(Reconciler::with_dependencies(
            pool.clone(),
            route_set(route)?,
            BTreeMap::from([(CHAIN_ID, Arc::clone(&chain) as Arc<dyn ReconciliationChain>)]),
        ));
        let cancellation = CancellationToken::new();
        let task = tokio::spawn({
            let reconciler = Arc::clone(&reconciler);
            let cancellation = cancellation.clone();
            async move {
                reconciler
                    .run_loop(
                        StdDuration::from_secs(60),
                        topup::scanner::FinalizedHeads::default(),
                        cancellation,
                    )
                    .await;
            }
        });
        tokio::time::timeout(
            StdDuration::from_secs(1),
            chain.derivation_started.notified(),
        )
        .await?;
        cancellation.cancel();
        tokio::time::timeout(StdDuration::from_secs(1), task).await??;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn loop_publishes_failed_checks_for_the_daily_report() -> Result<()> {
    with_database(|pool| async move {
        let route = route()?;
        let seed = seed_identity(&pool, &route, 81).await?;
        let chain = Arc::new(MockChain::at(150));
        chain.derive(&[&seed]);
        chain.fail_derivation.store(true, Ordering::SeqCst);
        let reconciler = Reconciler::with_dependencies(
            pool.clone(),
            route_set(route)?,
            BTreeMap::from([(CHAIN_ID, Arc::clone(&chain) as Arc<dyn ReconciliationChain>)]),
        );
        let cancellation = CancellationToken::new();
        // The first tick is immediate; no other test completes a loop round in this binary.
        let round = async {
            while !topup::observability::reconciliation().is_some_and(|status| status.failed_checks.iter().any(|(check, _)| check == "address_derivation")) {
                tokio::time::sleep(StdDuration::from_millis(20)).await;
            }
        };
        tokio::select! {
            () = reconciler.run_loop(StdDuration::from_secs(3_600), topup::scanner::FinalizedHeads::default(), cancellation.clone()) => {}
            result = tokio::time::timeout(StdDuration::from_secs(10), round) => result?,
        }
        cancellation.cancel();
        let status = topup::observability::reconciliation().context("round status")?;
        ensure!(
            status.failed_checks
                == [(
                    "address_derivation".to_owned(),
                    "addressOf timed out".to_owned()
                )],
            "{status:?}"
        );
        Ok(())
    })
    .await
}

async fn with_database<F, Fut>(test: F) -> Result<()>
where
    F: FnOnce(PgPool) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let Some(context) = TestDatabase::create().await? else {
        return Ok(());
    };
    seed::initialize_dual_chain(&context.app_pool, CHAIN_ID).await?;
    let result = test(context.app_pool.clone()).await;
    let cleanup = context.cleanup().await;
    result.and(cleanup)
}

fn route() -> Result<RouteFile> {
    let mut route: RouteFile =
        serde_saphyr::from_str(include_str!("fixtures/phala-cloud-pha.yaml"))?;
    route.chain.chain_id = CHAIN_ID;
    route.livemode = false;
    route.chain.rpc_providers = vec![
        "http://127.0.0.1:8546".to_owned(),
        "http://localhost:8546".to_owned(),
    ];
    route.asset.contract = Address::from([200; 20]);
    Ok(route)
}

fn reconciler(pool: &PgPool, route: RouteFile, chain: Arc<MockChain>) -> Result<Reconciler> {
    Ok(Reconciler::with_dependencies(
        pool.clone(),
        route_set(route)?,
        BTreeMap::from([(CHAIN_ID, chain as Arc<dyn ReconciliationChain>)]),
    ))
}

fn has_check(report: &ReconciliationReport, check: CheckName) -> bool {
    report
        .findings
        .iter()
        .any(|finding| finding.check == check && !finding.repair_applied && !finding.incomplete)
}

/// Records that the scanner has backfilled every address and committed through `block`.
async fn scanned_through(pool: &PgPool, block: u64) -> Result<()> {
    let ids = db::list_scan_addresses(pool, CHAIN_ID)
        .await?
        .into_iter()
        .map(|address| address.id)
        .collect::<Vec<_>>();
    let boundary = topup::db::chain_reads::Boundary {
        number: block,
        hash: B256::repeat_byte(24),
        time: Utc::now(),
    };
    topup::db::chain_reads::advance_checkpoint(pool, CHAIN_ID, boundary).await?;
    topup::db::chain_reads::initialize_coverage(pool, CHAIN_ID, boundary).await?;
    sqlx::query("UPDATE chain_coverage SET through_block=$2,through_hash=$3,through_time=$4 WHERE chain_id=$1").bind(i64::try_from(CHAIN_ID)?).bind(i64::try_from(block)?).bind(format!("{:#x}",boundary.hash)).bind(boundary.time).execute(pool).await?;
    sqlx::query("UPDATE addresses SET backfilled=true,dual_covered_through=$2 WHERE id=ANY($1)")
        .bind(ids)
        .bind(i64::try_from(block)?)
        .execute(pool)
        .await?;
    Ok(())
}

async fn count(pool: &PgPool, query: &'static str) -> Result<i64> {
    Ok(sqlx::query_scalar(query).fetch_one(pool).await?)
}

async fn deposit(pool: &PgPool, id: Uuid) -> Result<Deposit> {
    db::get_deposit(pool, id)
        .await?
        .context("deposit must exist")
}

async fn seed_account(pool: &PgPool) -> Result<Uuid> {
    let existing =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM accounts WHERE name = 'reconciler'")
            .fetch_optional(pool)
            .await?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let account = seed::create_account(
        pool,
        &NewAccount {
            livemode: false,
            webhook_url: "http://product.test/webhooks".to_owned(),
            ..NewAccount::named("reconciler")
        },
    )
    .await?;
    Ok(account.id)
}

async fn seed_identity(pool: &PgPool, route: &RouteFile, number: u8) -> Result<Seed> {
    let account_id = seed_account(pool).await?;
    let customer = seed::create_customer(
        pool,
        &NewCustomer {
            id: Uuid::new_v4(),
            account_id,
            livemode: false,
            client_reference_id: format!("workspace-{number}"),
            paused_scopes: Vec::new(),
        },
    )
    .await?;
    let address_id = Uuid::new_v4();
    let salt = B256::from([number; 32]);
    let address = Address::from([number; 20]);
    seed::insert_address(
        pool,
        &NewAddress {
            id: address_id,
            customer_id: customer.id,
            chain_id: route.chain.chain_id,
            route: route.route.clone(),
            salt,
            address,
        },
    )
    .await?;
    Ok(Seed {
        address_id,
        salt,
        address,
    })
}

struct DepositSeed {
    number: u8,
    state: DepositState,
    block_number: u64,
    amount: U256,
    credit_minor: u64,
}

impl DepositSeed {
    fn new(number: u8, state: DepositState) -> Self {
        Self {
            number,
            state,
            block_number: 100,
            amount: U256::from(10_u64).pow(U256::from(18_u8)),
            credit_minor: 100,
        }
    }

    fn block(self, block_number: u64) -> Self {
        Self {
            block_number,
            ..self
        }
    }

    fn amount(self, amount: u64) -> Self {
        Self {
            amount: U256::from(amount),
            ..self
        }
    }

    fn credit(self, credit_minor: u64) -> Self {
        Self {
            credit_minor,
            ..self
        }
    }
}

async fn seed_deposit(
    pool: &PgPool,
    route: &RouteFile,
    seed: &Seed,
    fixture: DepositSeed,
) -> Result<Uuid> {
    let number = fixture.number;
    let deposit = NewDeposit {
        chain_id: route.chain.chain_id,
        tx_hash: B256::from([number; 32]),
        log_index: u64::from(number),
        receipt_log_index: u64::from(number),
        tx_from: alloy_primitives::Address::ZERO,
        tx_nonce: 0,
        is_final: true,
        block_number: fixture.block_number,
        block_hash: B256::from([number.wrapping_add(1); 32]),
        block_time: Utc::now(),
        address_id: seed.address_id,
        route: Some(route.route.clone()),
        route_version: Some(route.version),
        asset_contract: route.asset.contract,
        from_address: Address::from([201; 20]),
        amount_atomic: AtomicAmount::new(fixture.amount),
        state: fixture.state,
        reason: None,
        next_attempt_at: Utc::now() - Duration::seconds(1),
    };
    let id = deposit_id(deposit.chain_id, deposit.tx_hash, deposit.log_index);
    ensure!(db::insert_deposit(pool, &deposit).await?);
    sqlx::query(
        r#"
        UPDATE deposits
        SET valuation_at = $2, price_scaled = 100000000,
            price_source = 'spot', credit_minor = $3::text::numeric
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(Utc::now())
    .bind(fixture.credit_minor.to_string())
    .execute(pool)
    .await?;
    Ok(id)
}

/// Records a finalized `Flushed` event for the seed's forwarder, as the scanner indexes one.
async fn seed_flushed(
    pool: &PgPool,
    route: &RouteFile,
    seed: &Seed,
    block_number: u64,
    log_index: u64,
    amount: u64,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO flushed
            (chain_id, tx_hash, log_index, address_id, token, treasury, amount_atomic,
             block_number, block_hash)
        SELECT chain_id, $2, $3, id, $4, treasury, $5::text::numeric, $6, $7
        FROM addresses WHERE id = $1
        "#,
    )
    .bind(seed.address_id)
    .bind(format!("{:#x}", B256::from([23; 32])))
    .bind(i64::try_from(log_index)?)
    .bind(format!("{:#x}", route.asset.contract))
    .bind(amount.to_string())
    .bind(i64::try_from(block_number)?)
    .bind(format!("{:#x}", B256::from([24; 32])))
    .execute(pool)
    .await?;
    Ok(())
}

fn route_set(route: RouteFile) -> Result<Arc<topup::routes::RouteSet>> {
    topup::routes::RouteSet::new(vec![route])
        .map(Arc::new)
        .map_err(anyhow::Error::msg)
}

/// Production queries and entry points on 200k addresses and 200k valued deposits. Only a
/// bounded page is resident per round, progress survives new Reconciler instances, and a
/// payment to the old cancelled quote on the final page is still recorded.
#[tokio::test]
async fn scale_rounds_are_bounded_and_eventually_cover_old_addresses_and_deposits() -> Result<()> {
    support::with_database(|database| Box::pin(async move {
        let pool=database.app_pool.clone();
        seed::initialize_dual_chain(&pool, CHAIN_ID).await?;
        let route=route()?;
        let seed=seed_identity(&pool,&route,1).await?;
        let original=seed_deposit(&pool,&route,&seed,DepositSeed::new(2,DepositState::Credited).block(3).credit(101)).await?;
        // Clone valid fixture rows in 10k-row transactions, preserving all current constraints.
        // Deterministic IDs put the old fixture last, so coverage requires every keyset page.
        for start in (1..200_000_i64).step_by(10_000) {
            let end=(start+9_999).min(199_999);
            sqlx::query("INSERT INTO quotes SELECT copy.* FROM quotes q CROSS JOIN generate_series($1::bigint,$2::bigint) g CROSS JOIN LATERAL jsonb_populate_record(NULL::quotes,to_jsonb(q)||jsonb_build_object('id',lpad(to_hex(g),32,'0')::uuid,'idempotency_key',NULL,'client_secret_hash',NULL)) copy WHERE q.id=(SELECT quote_id FROM addresses WHERE id=$3)")
                .bind(start).bind(end).bind(seed.address_id).execute(&pool).await?;
            sqlx::query("INSERT INTO addresses SELECT copy.* FROM addresses a CROSS JOIN generate_series($1::bigint,$2::bigint) g CROSS JOIN LATERAL jsonb_populate_record(NULL::addresses,to_jsonb(a)||jsonb_build_object('id',lpad(to_hex(g),32,'0')::uuid,'quote_id',lpad(to_hex(g),32,'0')::uuid,'address','0x'||lpad(to_hex(g),40,'0'),'salt','0x'||lpad(to_hex(g),64,'0'),'backfilled',true)) copy WHERE a.id=$3")
                .bind(start).bind(end).bind(seed.address_id).execute(&pool).await?;
            sqlx::query("INSERT INTO deposits SELECT copy.* FROM deposits d CROSS JOIN generate_series($1::bigint,$2::bigint) g CROSS JOIN LATERAL jsonb_populate_record(NULL::deposits,to_jsonb(d)||jsonb_build_object('id',lpad(to_hex(g),32,'0')::uuid,'address_id',lpad(to_hex(g),32,'0')::uuid,'tx_hash','0x'||lpad(to_hex(g),64,'0'),'credit_minor',100)) copy WHERE d.id=$3")
                .bind(start).bind(end).bind(original).execute(&pool).await?;
        }
        sqlx::query("UPDATE addresses SET backfilled=true WHERE id=$1").bind(seed.address_id).execute(&pool).await?;
        sqlx::query("ANALYZE addresses").execute(&database.owner_pool).await?;
        sqlx::query("ANALYZE deposits").execute(&database.owner_pool).await?;
        let before=std::time::Instant::now();
        let all=db::list_scan_addresses(&pool,CHAIN_ID).await?;
        let old_time=before.elapsed();
        let old_bytes=all.len()*std::mem::size_of::<db::ScanAddress>();
        ensure!(all.len()==200_000);
        drop(all);
        let after=std::time::Instant::now();
        let (page,more)=db::scan_address_page(&pool,CHAIN_ID,None).await?;
        let page_time=after.elapsed();
        let page_bytes=page.len()*std::mem::size_of::<db::ScanAddress>();
        ensure!(page.len()==1_000 && more);
        ensure!(page_bytes*200==old_bytes);
        let address_plan=sqlx::query_scalar::<_,String>("EXPLAIN (ANALYZE,BUFFERS) SELECT id,address,created_block,backfilled,backfilled_through FROM addresses WHERE chain_id=$1 AND id >= $2 ORDER BY id LIMIT 1001")
            .bind(i64::try_from(CHAIN_ID)?).bind(Uuid::from_u128(100_000)).fetch_all(&pool).await?.join("\n");
        let credit_plan=sqlx::query_scalar::<_,String>("EXPLAIN (ANALYZE,BUFFERS) SELECT id FROM deposits WHERE credit_minor IS NOT NULL AND price_scaled IS NOT NULL AND route IS NOT NULL AND route_version IS NOT NULL AND id > $1 ORDER BY id LIMIT 1001")
            .bind(Uuid::from_u128(100_000)).fetch_all(&pool).await?.join("\n");
        ensure!((address_plan.contains("addresses_chain_page_idx") || address_plan.contains("addresses_pkey")) && !address_plan.contains("Sort"),"{address_plan}");
        ensure!((credit_plan.contains("deposits_credit_page_idx") || credit_plan.contains("deposits_pkey")) && !credit_plan.contains("Sort"),"{credit_plan}");
        println!("SCALE addresses=200000 deposits=200000 old_address_load={old_time:?} page_load={page_time:?} old_address_bytes={old_bytes} page_bytes={page_bytes}\nADDRESS PLAN\n{address_plan}\nCREDIT PLAN\n{credit_plan}");

        let chain=Arc::new(MockChain::at(5));
        let mut max_credit_round=StdDuration::ZERO;
        let mut findings=Vec::new();
        for round in 0..200 {
            // New instances exercise durable progress rather than in-process indices.
            let reconciler=reconciler(&pool,route.clone(),chain.clone())?;
            let started=std::time::Instant::now();
            findings.extend(reconciler.check(CheckName::CreditRecomputation).await?);
            max_credit_round=max_credit_round.max(started.elapsed());
            ensure!(started.elapsed()<StdDuration::from_secs(10),"unbounded credit round {round}");
            let position:Option<Uuid>=sqlx::query_scalar("SELECT last_id FROM reconciliation_work_cursors WHERE check_name='credit' AND chain_id=0").fetch_one(&pool).await?;
            ensure!(position.is_some()==(round<199),"credit cursor failed to wrap on round {round}");
        }
        ensure!(findings.len()==1 && findings[0].subjects["deposit_id"]==original.to_string(),"old credit was skipped: {findings:?}");
        println!("SCALE credit_round_max={max_credit_round:?} old_work_per_round=200000 new_work_per_round=1000");

        let (first_page,_)=db::scan_address_page(&pool,CHAIN_ID,None).await?;
        for address in first_page {chain.balances.lock().unwrap().insert(address.address,U256::from(10_u64).pow(U256::from(18_u8)));}
        let custody_started=std::time::Instant::now();
        ensure!(reconciler(&pool,route.clone(),chain.clone())?.check(CheckName::CustodyBalance).await?.is_empty());
        ensure!(chain.balance_reads.lock().unwrap().iter().all(|(_,a)|a.len()<=1_000));
        ensure!(custody_started.elapsed()<StdDuration::from_secs(5));
        println!("SCALE custody_round={:?}",custody_started.elapsed());
        Ok(())
    })).await
}

#[tokio::test]
async fn post_restore_ignores_a_completed_sweeps_stale_page_cursor() -> Result<()> {
    with_database(|pool| async move {
        let route=route()?;
        let seed=seed_identity(&pool,&route,1).await?;
        scanned_through(&pool,5).await?;
        let chain=Arc::new(MockChain::at(5));
        chain.derive(&[&seed]);
        sqlx::query("INSERT INTO reconciliation_deposit_cursors(chain_id,next_block) VALUES($1,6)").bind(i64::try_from(CHAIN_ID)?).execute(&pool).await?;
        // Crash after advancing the block cursor but before clearing the previous sweep.
        sqlx::query("INSERT INTO scan_address_sweeps(chain_id,lane,epoch,anchor,from_block,through_block,last_id) VALUES($1,'missing',0,0,0,5,$2)").bind(i64::try_from(CHAIN_ID)?).bind(seed.address_id).execute(&pool).await?;
        let reconciler=reconciler(&pool,route,chain)?;
        let report=tokio::time::timeout(StdDuration::from_secs(5),reconciler.post_restore_once()).await??;
        ensure!(report.succeeded() && !report.incomplete,"{report:?}");
        Ok(())
    }).await
}
