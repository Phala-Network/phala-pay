//! Fast credit and reversal on Anvil and PostgreSQL (docs/design/multi-tenant.md §4, §16 PR 1):
//! a route crediting at depth 2, reorganized with `anvil_reorg`; and the same on an OP-stack chain
//! id, crediting at the family's depth on the sequencer's unsafe head while `safe` and `finalized`
//! lag behind.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use sqlx::{PgPool, Row};
use topup::api::{AppState, ClientReadLimiter, PublicOrigin, VerificationKey};
use topup::audit::Actor;
use topup::db;
use topup::deposit_addresses;
use topup::finality::FinalityWatch;
use topup::pump::{Pump, PumpConfig, RunOnceResult, StepSet};
use topup::routes::RouteSet;
use topup::scanner::{ChainRoutes, chain_routes, coverage_once, fast_once};
use topup::steps::confirm::ConfirmStep;
use topup::steps::screen::{ScreenRoute, ScreenStep};
use topup::tenancy::Scope;
use topup_adapters::attestation::DstackAttestor;
use topup_adapters::chain::evm::{
    ChainError, ChainReader, EvmClient, FinalizedHead, FinalizedReader, ReceiptLookup, TransferLog,
};
use topup_adapters::pricing::{Observation, PriceError, PriceSource};
use topup_adapters::risk::oracle::SanctionsSource;
use topup_core::deposit::DepositState;
use topup_core::identity::{credited_event_id, deposit_id, reversed_event_id};
use topup_core::money::{AtomicAmount, PRICE_SCALE, ScaledPrice};
use topup_core::route::{ChainFamily, ChainHeads, Confirmations, RouteFile};
use topup_core::screening::{SanctionsResult, SanctionsVerdict};
use topup_core::valuation::{SourceId, UnixSeconds};
use tracing_test::traced_test;
use uuid::Uuid;

use support::chain::{ANVIL_PRIVATE_KEY, Anvil, CHAIN_ID, forge_create, run_checked};
use support::seed::{self, NewAccount, NewAddress};
use support::{TEST_ORIGIN, TestDatabase, public_key_base64};

/// Anvil's second and third default accounts: the payer, and a sender whose transaction shifts
/// the payer's log within a re-mined block.
const PAYER_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const PAYER: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
const OTHER_KEY: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const OTHER: &str = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";
const AMOUNT: u64 = 1_000;
/// Anvil's default 32-slot epochs keep `finalized` 64 blocks and `safe` 32 blocks behind the head,
/// so a reorg of a few blocks stays above both.
const FINALITY_DEPTH: u64 = 64;
/// Base Sepolia's chain id: an OP-stack chain (design D1).
const BASE_SEPOLIA: u64 = 84_532;

/// The chain a scenario runs on and the confirmation its route credits at.
#[derive(Clone, Copy)]
struct Network {
    chain_id: u64,
    confirmations: Confirmations,
}

/// Anvil's Ethereum L1 chain id at depth 2.
const L1: Network = Network {
    chain_id: CHAIN_ID,
    confirmations: Confirmations::Depth(2),
};

/// An OP-stack chain id at the family's default, a depth on the sequencer's unsafe head.
const OP_STACK: Network = Network {
    chain_id: BASE_SEPOLIA,
    confirmations: ChainFamily::OpStack.default_confirmations(),
};

#[tokio::test]
async fn a_depth_one_reorg_before_credit_changes_nothing() -> Result<()> {
    run(&[], |chain| {
        Box::pin(async move {
            let tx = chain.pay(AMOUNT)?;
            // Depth 1: not credited yet.
            ensure!(chain.scan().await? == 0, "a depth-1 transfer was recorded");
            let raw = chain.raw_transaction(tx)?;
            chain.reorg(1, &[(&raw, 0)])?;
            let (_, reorged_hash) = chain.receipt_block(tx)?.context("re-mined receipt")?;
            chain.anvil.mine(1)?;

            ensure!(chain.scan().await? == 1);
            chain.settle().await?;
            let deposit = chain.deposit(tx).await?;
            ensure!(deposit.state == DepositState::Credited);
            ensure!(
                deposit.block_hash == reorged_hash,
                "evidence is the re-mined block"
            );
            ensure!(chain.count("SELECT count(*) FROM deposits").await? == 1);
            ensure!(chain.events("deposit.credited").await? == vec![credited_event_id(deposit.id)]);
            ensure!(chain.events("deposit.reversed").await?.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_re_included_transaction_keeps_its_deposit_and_is_followed_not_reversed() -> Result<()> {
    run(&[], |chain| {
        Box::pin(async move {
            let tx = chain.pay(AMOUNT)?;
            chain.anvil.mine(1)?;
            ensure!(chain.scan().await? == 1);
            chain.settle().await?;
            let credited = chain.deposit(tx).await?;
            ensure!(credited.state == DepositState::Credited);
            ensure!(credited.final_at.is_none(), "a depth-2 credit is not final");

            // The transaction moves one block later, behind another sender's transfer in the
            // same block, so its block-wide log index changes and its receipt position does not.
            let raw = chain.raw_transaction(tx)?;
            let shift = chain.other_transfer_raw()?;
            chain.reorg(2, &[(&shift, 1), (&raw, 1)])?;
            let (block_number, block_hash) =
                chain.receipt_block(tx)?.context("re-included receipt")?;
            ensure!(block_number == credited.block_number + 1);

            // Nothing is read before the recorded block is final.
            let stats = chain.watch().await?;
            ensure!(stats.watched == 0, "{stats:?}");

            // At finality one receipt per provider shows the transfer in its new, final block.
            chain.anvil.mine(FINALITY_DEPTH + 2)?;
            let stats = chain.watch().await?;
            ensure!(stats.finalized == 1 && stats.reversed == 0, "{stats:?}");
            let final_deposit = chain.deposit(tx).await?;
            ensure!(final_deposit.id == credited.id);
            ensure!(final_deposit.final_at.is_some());
            ensure!(final_deposit.state == DepositState::Credited);
            ensure!(
                final_deposit.block_number == block_number
                    && final_deposit.block_hash == block_hash
            );
            ensure!(final_deposit.receipt_log_index == 0);
            ensure!(
                final_deposit.log_index != credited.log_index,
                "the re-mined block puts another log first"
            );
            ensure!(chain.count("SELECT count(*) FROM deposits").await? == 1);
            ensure!(chain.events("deposit.reversed").await?.is_empty());
            ensure!(chain.events("deposit.credited").await?.len() == 1);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_transaction_replaced_with_the_same_nonce_is_reversed_once() -> Result<()> {
    run(&[], |chain| {
        Box::pin(async move {
            let nonce = chain.payer_nonce()?;
            let tx = chain.pay(AMOUNT)?;
            chain.anvil.mine(1)?;
            ensure!(chain.scan().await? == 1);
            chain.settle().await?;
            let credited = chain.deposit(tx).await?;
            ensure!(credited.state == DepositState::Credited);
            ensure!(credited.price_source.as_deref() == Some("lock"));
            ensure!(chain.quote_status().await? == ("consumed".to_owned(), Some(credited.id)));

            // The API refunds only final deposits; a pending refund of this one is written
            // directly, to show a reversal cancels it.
            let refund = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO refunds (id, account_id, livemode, chain_id, deposit_id, \
                 amount_atomic, destination_address, status) \
                 SELECT $1, account_id, livemode, chain_id, id, 1, $3, 'pending' \
                 FROM deposits WHERE id = $2",
            )
            .bind(refund)
            .bind(credited.id)
            .bind(OTHER.to_lowercase())
            .execute(&chain.pool)
            .await?;

            // The payer's nonce is spent on another transaction instead.
            let replacement = chain.payer_replacement(nonce)?;
            chain.reorg(2, &[(&replacement, 0)])?;
            ensure!(chain.receipt_block(tx)?.is_none());

            // Not final yet: the deposit is not read, and it may still come back.
            let stats = chain.watch().await?;
            ensure!(stats.watched == 0, "{stats:?}");
            ensure!(chain.deposit(tx).await?.state == DepositState::Credited);

            sqlx::query(
                "UPDATE quotes SET expires_at=now()-interval '1 second' WHERE consumed_by=$1",
            )
            .bind(credited.id)
            .execute(&chain.pool)
            .await?;
            chain.anvil.mine(FINALITY_DEPTH + 2)?;
            chain.record_known_replacement(&replacement).await?;
            let stats = chain.watch().await?;
            ensure!(stats.reversed == 1, "{stats:?}");
            let reversed = chain.deposit(tx).await?;
            ensure!(reversed.state == DepositState::Reversed);
            ensure!(reversed.final_at.is_none());
            ensure!(
                chain.events("deposit.reversed").await? == vec![reversed_event_id(credited.id)]
            );
            // Its snapshot takes the whole credit back: the deposit nets to zero, whichever of
            // `deposit.credited` and `deposit.reversed` a merchant receives first.
            let snapshot: Value = sqlx::query_scalar(
                "SELECT data -> 'object' FROM events WHERE type = 'deposit.reversed'",
            )
            .fetch_one(&chain.pool)
            .await?;
            ensure!(
                snapshot["status"] == "reversed"
                    && snapshot["amount"].as_u64().is_some_and(|amount| amount > 0)
                    && snapshot["amount_reversed"] == snapshot["amount"]
                    && snapshot["amount_refunded"] == 0,
                "{snapshot}"
            );
            let evidence: Value = sqlx::query_scalar(
                "SELECT evidence FROM transitions \
                 WHERE deposit_id = $1 AND from_state = 'credited' AND to_state = 'reversed'",
            )
            .bind(credited.id)
            .fetch_one(&chain.pool)
            .await?;
            ensure!(evidence["result"] == "known_finalized_replacement");
            let refund_status: String =
                sqlx::query_scalar("SELECT status FROM refunds WHERE id = $1")
                    .bind(refund)
                    .fetch_one(&chain.pool)
                    .await?;
            ensure!(refund_status == "canceled");
            // Even past wall-clock expiry, reversal restores open and the reservation;
            // only complete dual coverage may close the quote afterward.
            ensure!(chain.quote_status().await? == ("open".to_owned(), None));
            let reserved:bool=sqlx::query_scalar("SELECT exposure_reserved FROM quotes WHERE id=(SELECT quote_id FROM addresses WHERE id=$1)").bind(chain.address_id).fetch_one(&chain.pool).await?;
            ensure!(reserved);
            ensure!(chain.events("quote.expired").await?.is_empty());
            chain.anvil.mine(80)?;
            chain.anvil.mine(1)?;
            run_checked("cast", &["rpc", "evm_increaseTime", "60", "--rpc-url", &chain.anvil.rpc_url], None)?;
            chain.anvil.mine(FINALITY_DEPTH + 2)?;
            coverage_once(&chain.pool,&chain.reader,&chain.reader,&chain.routes,1).await?;
            ensure!(topup::locks::expire_once(&chain.pool,&RouteSet::new(vec![chain.route.clone()]).unwrap()).await?==1);
            ensure!(chain.events("quote.expired").await?.len()==1);

            // Terminal: later passes and pumps leave it alone, and the event stays single.
            let again = chain.watch().await?;
            ensure!(again.finalized == 1 && again.reversed == 0, "{again:?}");
            ensure!(chain.watch().await?.watched == 0);
            chain.settle().await?;
            ensure!(chain.pump.run_once().await? == RunOnceResult::Idle);
            ensure!(chain.events("deposit.reversed").await?.len() == 1);
            Ok(())
        })
    })
    .await
}

/// The payer's view reports the credit of the one payment it shows, never a sum, and a fresh read
/// after that payment is reversed shows the payment still standing.
#[tokio::test]
async fn the_payers_view_credits_the_shown_payment_and_drops_a_reversed_one() -> Result<()> {
    run(&[], |chain| {
        Box::pin(async move {
            // Two partial payments, each credited at spot: the payer's, then another sender's,
            // with gas to spare for when it is re-mined without the first one before it.
            let nonce = chain.payer_nonce()?;
            let first = chain.pay(300)?;
            let second = send(
                &chain.anvil,
                OTHER_KEY,
                &[
                    "--gas-limit",
                    "100000",
                    &format!("{:#x}", chain.token),
                    "transfer(address,uint256)",
                    &format!("{:#x}", chain.address),
                    "400",
                ],
            )?;
            let second: B256 = serde_json::from_slice::<Value>(&second.stdout)?["transactionHash"]
                .as_str()
                .context("transaction hash")?
                .parse()?;
            chain.anvil.mine(1)?;
            ensure!(chain.scan().await? == 2);
            chain.settle().await?;
            let credit = |deposit: &db::Deposit| deposit.credit_minor.map(|credit| credit.value());
            let (first_deposit, second_deposit) =
                (chain.deposit(first).await?, chain.deposit(second).await?);
            for deposit in [&first_deposit, &second_deposit] {
                ensure!(deposit.state == DepositState::Credited);
                ensure!(deposit.price_source.as_deref() == Some("spot"));
            }
            let (first_credit, second_credit) = (credit(&first_deposit), credit(&second_deposit));
            ensure!(
                first_credit.is_some() && second_credit.is_some() && first_credit != second_credit
            );
            let view = chain.client_quote().await?;
            ensure!(
                view["payment_status"] == "credited"
                    && view["amount"] == 90
                    && view["amount_credited"].as_u64() == first_credit
                    && view["typical_credit_seconds"] == 30,
                "{view}"
            );

            // The payer's nonce is spent on another transaction; the other sender's payment is
            // re-mined. At finality the first deposit is reversed and the second one stands.
            let raw = chain.raw_transaction(second)?;
            let replacement = chain.payer_replacement(nonce)?;
            chain.reorg(3, &[(&replacement, 0), (&raw, 1)])?;
            ensure!(chain.receipt_block(first)?.is_none());
            ensure!(chain.receipt_block(second)?.is_some());
            chain.anvil.mine(FINALITY_DEPTH + 2)?;
            chain.record_known_replacement(&replacement).await?;
            let stats = chain.watch().await?;
            ensure!(stats.reversed == 1 && stats.finalized == 2, "{stats:?}");
            ensure!(chain.deposit(first).await?.state == DepositState::Reversed);
            ensure!(chain.deposit(second).await?.state == DepositState::Credited);
            let view = chain.client_quote().await?;
            ensure!(
                view["payment_status"] == "credited"
                    && view["amount_credited"].as_u64() == second_credit,
                "{view}"
            );
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn payments_to_active_and_retired_deposit_addresses_are_credited_at_spot() -> Result<()> {
    run(&[], |chain| {
        Box::pin(async move {
            let (account, customer) = seed::create_account_and_customer(
                &chain.pool,
                &NewAccount {
                    livemode: false,
                    ..NewAccount::named("deposit-address-merchant")
                },
                "team-da",
            )
            .await?;
            seed::accept_routes(&chain.pool, account.id, false, &[&chain.route]).await?;
            seed::set_treasury(
                &chain.pool,
                account.id,
                false,
                chain.route.chain.chain_id,
                seed::FIXTURE_TREASURY,
            )
            .await?;
            let chains = [deposit_addresses::ChainContracts::of(&chain.route)];
            let (retired, created) =
                deposit_addresses::create(&chain.pool, &account, &customer, &chains, None).await?;
            ensure!(created);
            let active = deposit_addresses::rotate(
                &chain.pool,
                &account,
                Scope::new(account.id, false),
                &Actor::system("test"),
                retired.id,
                &chains,
            )
            .await?;
            let [retired_network] = retired.networks.as_slice() else {
                bail!("one network: {:?}", retired.networks);
            };
            let [active_network] = active.networks.as_slice() else {
                bail!("one network: {:?}", active.networks);
            };

            let to_retired = chain.pay_to(retired_network.address, AMOUNT)?;
            let to_active = chain.pay_to(active_network.address, AMOUNT)?;
            chain.anvil.mine(1)?;
            // The per-block scan covers deposit addresses, active and retired, like every address.
            ensure!(chain.scan().await? == 2);
            chain.settle().await?;
            for (tx, network) in [(to_retired, retired_network), (to_active, active_network)] {
                let deposit = chain.deposit(tx).await?;
                ensure!(
                    deposit.state == DepositState::Credited,
                    "{:?}",
                    deposit.state
                );
                ensure!(deposit.price_source.as_deref() == Some("spot"));
                ensure!(deposit.address_id == network.address_id);
                ensure!(deposit.customer_id == customer.id);
                ensure!(deposit.account_id == account.id && !deposit.livemode);
                ensure!(
                    chain
                        .events("deposit.credited")
                        .await?
                        .contains(&credited_event_id(deposit.id))
                );
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_treasury_change_moves_the_chains_address_and_the_old_address_still_credits() -> Result<()>
{
    run(&[], |chain| {
        Box::pin(async move {
            let (account, customer) = seed::create_account_and_customer(
                &chain.pool,
                &NewAccount {
                    livemode: false,
                    ..NewAccount::named("treasury-change-merchant")
                },
                "team-tc",
            )
            .await?;
            seed::accept_routes(&chain.pool, account.id, false, &[&chain.route]).await?;
            let chain_id = chain.route.chain.chain_id;
            seed::set_treasury(
                &chain.pool,
                account.id,
                false,
                chain_id,
                seed::FIXTURE_TREASURY,
            )
            .await?;
            let chains = [deposit_addresses::ChainContracts::of(&chain.route)];
            let (before, _) =
                deposit_addresses::create(&chain.pool, &account, &customer, &chains, None).await?;
            let [old_network] = before.networks.as_slice() else {
                bail!("one network: {:?}", before.networks);
            };

            // The change applies through the time-lock worker's path: the chain's network of
            // every deposit address moves to a forwarder over the new treasury.
            let new_treasury = Address::repeat_byte(0x7e);
            seed::schedule_treasury(
                &chain.pool,
                account.id,
                false,
                chain_id,
                new_treasury,
                Utc::now(),
            )
            .await?;
            let routes = RouteSet::new(vec![chain.route.clone()]).map_err(anyhow::Error::msg)?;
            ensure!(
                topup::treasuries::apply_due(
                    &chain.pool,
                    &routes,
                    &support::ClearScreener,
                    Utc::now()
                )
                .await?
                    == 1
            );
            let after =
                deposit_addresses::get(&chain.pool, Scope::new(account.id, false), before.id)
                    .await?
                    .context("deposit address")?;
            let [new_network] = after.networks.as_slice() else {
                bail!("one network: {:?}", after.networks);
            };
            ensure!(new_network.treasury == new_treasury);
            ensure!(new_network.address != old_network.address);

            // Both are watched: a payment to the superseded address is still credited, and its
            // funds reach the old treasury, its forwarder's clone argument.
            let to_old = chain.pay_to(old_network.address, AMOUNT)?;
            let to_new = chain.pay_to(new_network.address, AMOUNT)?;
            chain.anvil.mine(1)?;
            ensure!(chain.scan().await? == 2);
            chain.settle().await?;
            for (tx, network) in [(to_old, old_network), (to_new, new_network)] {
                let deposit = chain.deposit(tx).await?;
                ensure!(
                    deposit.state == DepositState::Credited,
                    "{:?}",
                    deposit.state
                );
                ensure!(deposit.address_id == network.address_id);
                let address = db::get_address(&chain.pool, network.address_id)
                    .await?
                    .context("address row")?;
                ensure!(address.treasury == network.treasury);
            }
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_lagging_provider_b_delays_the_credit_until_it_reaches_the_depth() -> Result<()> {
    let lag = Arc::new(AtomicU64::new(1));
    let lagging = Arc::clone(&lag);
    run_with(
        L1,
        &[],
        move |rpc_url| {
            Ok(LaggingReader {
                inner: reader(rpc_url)?,
                lag: Arc::clone(&lagging),
            })
        },
        |chain| {
            Box::pin(async move {
                let tx = chain.pay(AMOUNT)?;
                chain.anvil.mine(1)?;
                ensure!(chain.scan().await? == 1);
                let id = chain.deposit(tx).await?.id;
                ensure!(chain.pump.run_once().await? == RunOnceResult::Applied { deposit_id: id });
                let waiting = chain.deposit(tx).await?;
                ensure!(waiting.state == DepositState::Detected);
                let evidence: Value = sqlx::query_scalar(
                    "SELECT evidence FROM transitions WHERE deposit_id = $1 \
                     ORDER BY created_at DESC LIMIT 1",
                )
                .bind(id)
                .fetch_one(&chain.pool)
                .await?;
                ensure!(evidence["result"] == "not_confirmed", "{evidence}");
                // Retried within a few seconds, not a full wait interval.
                let delay = waiting.next_attempt_at - Utc::now();
                ensure!(delay <= chrono::Duration::seconds(3), "{delay}");

                lag.store(0, Ordering::SeqCst);
                sqlx::query("UPDATE deposits SET next_attempt_at = now() WHERE id = $1")
                    .bind(id)
                    .execute(&chain.pool)
                    .await?;
                chain.settle().await?;
                ensure!(chain.deposit(tx).await?.state == DepositState::Credited);
                Ok(())
            })
        },
    )
    .await
}

#[tokio::test]
async fn an_op_stack_unsafe_head_reorg_reverses_the_credit_and_credits_the_replacement()
-> Result<()> {
    run_on(OP_STACK, &[], |chain| {
        Box::pin(async move {
            let Confirmations::Depth(depth) = OP_STACK.confirmations else {
                bail!("the OP-stack default is a depth");
            };
            let nonce = chain.payer_nonce()?;
            let tx = chain.pay(AMOUNT)?;
            chain.anvil.mine(depth - 2)?;
            ensure!(
                chain.scan().await? == 0,
                "recorded one block short of the depth"
            );
            chain.anvil.mine(1)?;
            ensure!(chain.scan().await? == 1);
            chain.settle().await?;
            let credited = chain.deposit(tx).await?;
            ensure!(credited.state == DepositState::Credited);
            ensure!(credited.final_at.is_none());
            chain
                .assert_above_safe_and_finalized(credited.block_number)
                .await?;

            // The unsafe blocks from the payment's on are replaced: the payer's nonce now pays the
            // address from another transaction, in the payment's block.
            let replacement = chain.payer_replacement_to(nonce, chain.address, AMOUNT)?;
            chain.reorg(depth, &[(&replacement, 0)])?;
            ensure!(chain.receipt_block(tx)?.is_none());
            let (replacement_block, _) = chain
                .receipt_block(replacement_hash(&replacement)?)?
                .context("replacement receipt")?;
            ensure!(replacement_block == credited.block_number);
            chain.anvil.mine(1)?;
            ensure!(chain.scan().await? == 0, "below the head loop's cursor");
            let stats = chain.watch().await?;
            ensure!(stats.watched == 0, "{stats:?}");
            ensure!(chain.deposit(tx).await?.state == DepositState::Credited);

            chain.anvil.mine(FINALITY_DEPTH + depth)?;
            chain.record_known_replacement(&replacement).await?;
            let stats = chain.watch().await?;
            ensure!(stats.reversed == 1, "{stats:?}");
            ensure!(chain.deposit(tx).await?.state == DepositState::Reversed);
            ensure!(
                chain.events("deposit.reversed").await? == vec![reversed_event_id(credited.id)]
            );

            let stats =
                coverage_once(&chain.pool, &chain.reader, &chain.reader, &chain.routes, 1).await?;
            ensure!(
                stats.inserted == 0,
                "known replacement must be idempotent: {stats:?}"
            );
            chain.settle().await?;
            let successor = replacement_hash(&replacement)?;
            ensure!(chain.deposit(successor).await?.state == DepositState::Credited);
            ensure!(
                chain.events("deposit.credited").await?.len() == 2
                    && chain.events("deposit.reversed").await?.len() == 1
            );
            // Coverage already recorded finalized evidence for the successor.
            let stats = chain.watch().await?;
            ensure!(stats.watched == 0 && stats.reversed == 0, "{stats:?}");
            ensure!(chain.deposit(successor).await?.final_at.is_some());
            ensure!(chain.events("deposit.reversed").await?.len() == 1);
            Ok(())
        })
    })
    .await
}

/// The residual risk of crediting before finality, the same on both families: a reorganization
/// that removes the payment without spending the payer's nonce (on OP-stack, verifiers replacing
/// unsafe blocks the batcher never posted) proves nothing dropped, as the transaction could still
/// be included again. Past finality the watch keeps the deposit credited and not final, so it holds
/// its share of the account's cap, and alerts once the block is an hour old; it does not reverse.
/// Anvil's clock starts two hours back, so the payment's block is past that hour.
#[tokio::test]
#[traced_test]
async fn an_op_stack_payment_removed_without_its_nonce_spent_stays_pending_and_alerts() -> Result<()>
{
    let genesis = (Utc::now() - chrono::Duration::hours(2))
        .timestamp()
        .to_string();
    run_on(OP_STACK, &["--timestamp", &genesis], |chain| {
        Box::pin(async move {
            let Confirmations::Depth(depth) = OP_STACK.confirmations else {
                bail!("the OP-stack default is a depth");
            };
            let tx = chain.pay(AMOUNT)?;
            chain.anvil.mine(depth - 1)?;
            ensure!(chain.scan().await? == 1);
            chain.settle().await?;
            let credited = chain.deposit(tx).await?;
            ensure!(credited.state == DepositState::Credited);
            let credit = credited
                .credit_minor
                .context("a credited deposit has its credit")?
                .value();
            chain
                .assert_above_safe_and_finalized(credited.block_number)
                .await?;
            let exposure = || async {
                Ok::<_, anyhow::Error>(
                    db::unfinalized_credit(
                        &chain.pool,
                        credited.account_id,
                        credited.livemode,
                        Uuid::nil(),
                    )
                    .await?
                    .credited,
                )
            };
            ensure!(exposure().await? == credit);

            // The unsafe blocks from the payment's on are replaced by empty ones: the transaction
            // is gone, and the payer's nonce is not spent.
            let nonce = chain.payer_nonce()?;
            chain.reorg(depth, &[])?;
            ensure!(chain.receipt_block(tx)?.is_none());
            ensure!(
                chain.payer_nonce()? == nonce - 1,
                "the payer's nonce is unspent"
            );

            chain.anvil.mine(FINALITY_DEPTH + depth)?;
            let finalized = chain.reader.finalized_head().await?.number;
            ensure!(
                finalized > credited.block_number,
                "past the payment's height"
            );
            let stats = chain.watch().await?;
            ensure!(
                stats.watched == 1 && stats.reversed == 0 && stats.finalized == 0,
                "{stats:?}"
            );
            let pending = chain.deposit(tx).await?;
            ensure!(pending.state == DepositState::Credited && pending.final_at.is_none());
            ensure!(chain.events("deposit.reversed").await?.is_empty());
            ensure!(
                exposure().await? == credit,
                "the credit still holds the cap"
            );
            ensure!(logs_contain("TopupDepositPendingAfterReorg"));
            Ok(())
        })
    })
    .await
}

/// The hash of a signed raw transaction.
fn replacement_hash(raw: &str) -> Result<B256> {
    let bytes = alloy_primitives::hex::decode(raw).context("raw transaction hex")?;
    Ok(alloy_primitives::keccak256(bytes))
}

#[tokio::test]
async fn a_newly_issued_address_is_scanned_from_the_next_block_without_a_gap() -> Result<()> {
    run(&[], |chain| {
        Box::pin(async move {
            chain.head_scan().await?;
            chain.anvil.mine(2)?;
            chain.head_scan().await?;
            // An address issued now, with no open quote, is paid in the next block.
            let (address_id, address) =
                issue_address(&chain.pool, chain.chain_id, chain.customer_id, 0x6b).await?;
            let tx = chain.pay_to(address, AMOUNT)?;
            chain.anvil.mine(1)?;
            let third = chain.head_scan().await?;
            ensure!(third.inserted == 1, "{third:?}");
            let deposit = chain.deposit(tx).await?;
            ensure!(deposit.address_id == address_id);
            ensure!(deposit.state == DepositState::Detected);
            Ok(())
        })
    })
    .await
}

async fn run<S>(anvil_args: &[&str], scenario: S) -> Result<()>
where
    S: for<'a> FnOnce(&'a FastChain) -> support::TestFuture<'a>,
{
    run_on(L1, anvil_args, scenario).await
}

async fn run_on<S>(network: Network, anvil_args: &[&str], scenario: S) -> Result<()>
where
    S: for<'a> FnOnce(&'a FastChain) -> support::TestFuture<'a>,
{
    run_with(network, anvil_args, reader, scenario).await
}

async fn run_with<B, F, S>(
    network: Network,
    anvil_args: &[&str],
    secondary: F,
    scenario: S,
) -> Result<()>
where
    B: ChainReader + Send + Sync + 'static,
    F: Fn(&str) -> Result<B>,
    S: for<'a> FnOnce(&'a FastChain) -> support::TestFuture<'a>,
{
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let Some(anvil) = Anvil::start_on_chain_if_available(network.chain_id, anvil_args).await?
    else {
        database.cleanup().await?;
        return Ok(());
    };
    let result = async {
        let chain = FastChain::setup(&database, network, anvil, secondary).await?;
        scenario(&chain).await
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

struct FastChain {
    anvil: Anvil,
    chain_id: u64,
    pool: PgPool,
    token: Address,
    customer_id: Uuid,
    address: Address,
    address_id: Uuid,
    route: RouteFile,
    routes: ChainRoutes,
    reader: FinalizedReader,
    pump: Pump,
    watch: FinalityWatch,
    api: axum::Router,
    client_reads: Arc<ClientReadLimiter>,
}

impl FastChain {
    async fn setup<B, F>(
        database: &TestDatabase,
        network: Network,
        anvil: Anvil,
        secondary: F,
    ) -> Result<Self>
    where
        B: ChainReader + Send + Sync + 'static,
        F: Fn(&str) -> Result<B>,
    {
        let pool = database.app_pool.clone();
        // History below `finalized` for the finalized scanner's first cursor.
        anvil.mine(2 * FINALITY_DEPTH)?;
        let token = forge_create(&anvil.rpc_url, "test/mocks/MockTokens.sol:MockERC20", &[])?;
        for (account, amount) in [(PAYER, 1_000_000_u64), (OTHER, 1_000_000)] {
            send(
                &anvil,
                ANVIL_PRIVATE_KEY,
                &[
                    &format!("{token:#x}"),
                    "mint(address,uint256)",
                    account,
                    &amount.to_string(),
                ],
            )?;
        }
        anvil.mine(4)?;

        let chain_id = network.chain_id;
        let (customer_id, address_id, address) = seed_address(&pool, chain_id).await?;
        let mut route: RouteFile = serde_saphyr::from_str(
            &include_str!("fixtures/phala-cloud-pha.yaml")
                .replace("chain_id: 1", &format!("chain_id: {chain_id}"))
                .replace("livemode: true", "livemode: false")
                .replace(
                    "0x6c5bA91642F10282b576d91922Ae6448C9d52f4E",
                    &format!("{token:#x}"),
                ),
        )?;
        if matches!(chain_id, 8453 | 84532) {
            route.pricing.sequencer_uptime = Some(topup_core::price::Sequencer {
                feed: "BASE_SEQUENCER_UPTIME".into(),
                grace_s: 3600,
            });
        }
        route.chain.confirmations = network.confirmations;
        route.asset.decimals = 2;
        route.asset.quote_amount_decimals = 2;
        route.merchant.min_amount = topup_core::route::Bounded::at(1);
        route.merchant.min_deposit_atomic =
            topup_core::route::Bounded::at(AtomicAmount::new(U256::ZERO));
        route.validate()?;
        let route_set = Arc::new(RouteSet::new(vec![route.clone()]).map_err(anyhow::Error::msg)?);
        let routes = chain_routes(&route_set)
            .into_iter()
            .next()
            .context("one chain route")?;

        let now = u64::try_from(Utc::now().timestamp())?;
        let price = |source: &str, value: u64| -> Arc<dyn PriceSource> {
            Arc::new(FixedPrice(Observation {
                source: SourceId::new(source),
                price: ScaledPrice::new(value, PRICE_SCALE).expect("fixture price"),
                observed_at: UnixSeconds::new(now),
            }))
        };
        let confirm = ConfirmStep::single(
            pool.clone(),
            route.clone(),
            reader(&anvil.rpc_url)?,
            secondary(&anvil.rpc_url)?,
            price("kraken", 10_000_000),
            Some(price("binance", 10_000_000)),
            Some(price("kraken", 100_000_000)),
        );
        let screen = ScreenStep::new(
            pool.clone(),
            [ScreenRoute::new(route.clone(), Arc::new(ClearSanctions))],
        )?;
        let pump = Pump::new(
            pool.clone(),
            Arc::clone(&route_set),
            Arc::new(StepSet::new(Box::new(confirm), Box::new(screen))),
            PumpConfig::default(),
        )?;
        let watch = FinalityWatch::single(
            pool.clone(),
            Arc::clone(&route_set),
            chain_id,
            reader(&anvil.rpc_url)?,
            secondary(&anvil.rpc_url)?,
        );
        let client_reads = Arc::new(ClientReadLimiter::default());
        let api = topup::api::router(AppState {
            pool: pool.clone(),
            routes: route_set,
            maintenance_keys: Vec::new(),
            admin_key: VerificationKey::from_base64(
                "admin/v1".to_owned(),
                &public_key_base64(&ed25519_dalek::SigningKey::from_bytes(&[49; 32])),
            )
            .map_err(anyhow::Error::msg)?,
            public_origin: PublicOrigin::parse(TEST_ORIGIN)?,
            attestor: Arc::new(DstackAttestor::new()),
            rate_lock_quotes: Arc::new(topup::locks::UnavailableQuoteProvider),
            client_reads: Arc::clone(&client_reads),
            rate_limits: Arc::default(),
            hint_limits: Arc::default(),
            transaction_hints: Arc::default(),
            screening: Arc::new(topup::refunds::UnavailableDestinationScreener),
            sanctions_rescreen: Arc::default(),
            contract_signatures: Arc::new(topup::treasuries::UnavailableContractSignatures),
        })
        .0;
        let reader = reader(&anvil.rpc_url)?;
        // The finalized scanner's first pass writes the cursor the fast scan starts above.
        coverage_once(&pool, &reader, &reader, &routes, 1).await?;
        let chain = Self {
            anvil,
            chain_id,
            pool,
            token,
            customer_id,
            address,
            address_id,
            route,
            routes,
            reader,
            pump,
            watch,
            api,
            client_reads,
        };
        // Every address is a quote's; the fast scan watches open quotes.
        chain.open_quote().await?;
        Ok(chain)
    }

    /// Transfers `amount` from the payer to the quote's address and waits for its receipt.
    fn pay(&self, amount: u64) -> Result<B256> {
        self.pay_to(self.address, amount)
    }

    /// Transfers `amount` from the payer to `to` and waits for its receipt.
    fn pay_to(&self, to: Address, amount: u64) -> Result<B256> {
        let output = send(
            &self.anvil,
            PAYER_KEY,
            &[
                &format!("{:#x}", self.token),
                "transfer(address,uint256)",
                &format!("{to:#x}"),
                &amount.to_string(),
            ],
        )?;
        let receipt: Value = serde_json::from_slice(&output.stdout)?;
        receipt["transactionHash"]
            .as_str()
            .context("transaction hash")?
            .parse()
            .context("parse transaction hash")
    }

    fn raw_transaction(&self, tx: B256) -> Result<String> {
        let output = rpc(
            &self.anvil,
            "eth_getRawTransactionByHash",
            &[&format!("{tx:#x}")],
        )?;
        Ok(String::from_utf8(output.stdout)?
            .trim()
            .trim_matches('"')
            .to_owned())
    }

    /// Another sender's token transfer, tipped above the payer's so it comes first in a block.
    fn other_transfer_raw(&self) -> Result<String> {
        let output = run_checked(
            "cast",
            &[
                "mktx",
                "--rpc-url",
                &self.anvil.rpc_url,
                "--private-key",
                OTHER_KEY,
                "--gas-price",
                "100gwei",
                "--priority-gas-price",
                "50gwei",
                &format!("{:#x}", self.token),
                "transfer(address,uint256)",
                OTHER,
                "1",
            ],
            None,
        )?;
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    /// A payer transaction with `nonce` that pays someone else.
    fn payer_replacement(&self, nonce: u64) -> Result<String> {
        self.payer_replacement_to(nonce, OTHER.parse()?, 1)
    }

    /// A payer transaction with `nonce` that pays `amount` to `to`. Its gas limit is fixed, not
    /// estimated on the current state, where `to` may already hold the replaced payment.
    fn payer_replacement_to(&self, nonce: u64, to: Address, amount: u64) -> Result<String> {
        let output = run_checked(
            "cast",
            &[
                "mktx",
                "--rpc-url",
                &self.anvil.rpc_url,
                "--private-key",
                PAYER_KEY,
                "--gas-limit",
                "100000",
                "--nonce",
                &nonce.to_string(),
                &format!("{:#x}", self.token),
                "transfer(address,uint256)",
                &format!("{to:#x}"),
                &amount.to_string(),
            ],
            None,
        )?;
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn payer_nonce(&self) -> Result<u64> {
        let output = run_checked(
            "cast",
            &["nonce", "--rpc-url", &self.anvil.rpc_url, PAYER],
            None,
        )?;
        Ok(String::from_utf8(output.stdout)?.trim().parse()?)
    }

    /// Replaces the last `depth` blocks with as many new blocks, carrying `transactions` at their
    /// block offsets; transactions of the replaced blocks are dropped.
    fn reorg(&self, depth: u64, transactions: &[(&str, u64)]) -> Result<()> {
        let pairs = serde_json::to_string(
            &transactions
                .iter()
                .map(|(raw, offset)| serde_json::json!([raw, offset]))
                .collect::<Vec<_>>(),
        )?;
        rpc(&self.anvil, "anvil_reorg", &[&depth.to_string(), &pairs])?;
        Ok(())
    }

    fn receipt_block(&self, tx: B256) -> Result<Option<(u64, B256)>> {
        let output = rpc(
            &self.anvil,
            "eth_getTransactionReceipt",
            &[&format!("{tx:#x}")],
        )?;
        let receipt: Value = serde_json::from_slice(&output.stdout)?;
        if receipt.is_null() {
            return Ok(None);
        }
        let number = receipt["blockNumber"].as_str().context("block number")?;
        let number = u64::from_str_radix(number.trim_start_matches("0x"), 16)?;
        let hash = receipt["blockHash"]
            .as_str()
            .context("block hash")?
            .parse()?;
        Ok(Some((number, hash)))
    }

    /// Checks that `block` is above provider A's `safe` and `finalized` heads: on the unsafe head.
    async fn assert_above_safe_and_finalized(&self, block: u64) -> Result<()> {
        let safe = self
            .reader
            .confirmation_heads(Confirmations::Safe)
            .await?
            .safe
            .context("the safe head")?;
        let finalized = self.reader.finalized_head().await?.number;
        ensure!(
            safe < block && finalized < block,
            "block {block} is not above safe {safe} and finalized {finalized}"
        );
        Ok(())
    }

    /// One per-block scan, as the head loop runs on each new head.
    async fn head_scan(&self) -> Result<db::ScanCommit> {
        Ok(fast_once(&self.pool, &self.reader, &self.routes).await?)
    }

    /// Deposits one per-block scan records at the route's depth.
    async fn scan(&self) -> Result<u64> {
        Ok(self.head_scan().await?.inserted)
    }

    /// Runs the pump until nothing is due.
    async fn settle(&self) -> Result<()> {
        for _ in 0..10 {
            if self.pump.run_once().await? == RunOnceResult::Idle {
                return Ok(());
            }
        }
        bail!("the pump did not settle")
    }

    /// Simulates fast discovery of a positive replacement already known to the service.
    async fn record_known_replacement(&self, raw: &str) -> Result<()> {
        let hash = replacement_hash(raw)?;
        let transfer = self
            .reader
            .receipt_transfer(hash, 0)
            .await?
            .transfer()
            .context("replacement transfer")?
            .clone();
        let address_id = if transfer.to == self.address {
            self.address_id
        } else {
            let row = seed::insert_address(
                &self.pool,
                &NewAddress {
                    id: Uuid::new_v4(),
                    customer_id: self.customer_id,
                    chain_id: self.chain_id,
                    route: self.route.route.clone(),
                    salt: B256::repeat_byte(0x71),
                    address: transfer.to,
                },
            )
            .await?;
            row.id
        };
        let mut tx = self.pool.begin().await?;
        db::insert_scanned_deposit_in(
            &mut tx,
            &db::NewDeposit {
                chain_id: self.chain_id,
                tx_hash: hash,
                receipt_log_index: 0,
                log_index: transfer.log_index,
                block_number: transfer.block_number,
                block_hash: transfer.block_hash,
                block_time: transfer.block_time,
                address_id,
                route: Some(self.route.route.clone()),
                route_version: Some(self.route.version),
                asset_contract: transfer.token,
                from_address: transfer.from,
                amount_atomic: transfer.amount,
                state: DepositState::Detected,
                reason: None,
                next_attempt_at: Utc::now(),
                tx_from: transfer.tx_from,
                tx_nonce: transfer.tx_nonce,
                is_final: false,
            },
            db::Evidence::Confirmed,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn watch(&self) -> Result<topup::finality::WatchStats> {
        let verify = FinalizedReader::new(Arc::new(EvmClient::new(&self.anvil.rpc_url)?));
        topup::checkpoint::advance(&self.pool, self.chain_id, &self.reader, &verify).await?;
        Ok(self.watch.watch_once(self.chain_id).await?)
    }

    async fn deposit(&self, tx: B256) -> Result<db::Deposit> {
        db::get_deposit(&self.pool, deposit_id(self.chain_id, tx, 0))
            .await?
            .context("deposit by receipt position")
    }

    async fn count(&self, query: &'static str) -> Result<i64> {
        Ok(sqlx::query_scalar(query).fetch_one(&self.pool).await?)
    }

    async fn events(&self, event_type: &str) -> Result<Vec<Uuid>> {
        Ok(
            sqlx::query_scalar("SELECT id FROM events WHERE type = $1 ORDER BY id")
                .bind(event_type)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// Opens a quote on the address for exactly [`AMOUNT`], its window an hour long.
    async fn open_quote(&self) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE quotes
            SET route = $2, amount_atomic = $3::text::numeric, price_scaled = 9000000,
                expires_at = now() + interval '1 hour', credit_minor = 90, status = 'open',
                exposure_reserved = true, closed_at = NULL, terms = $4
            WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)
            "#,
        )
        .bind(self.address_id)
        .bind(&self.route.route)
        .bind(AMOUNT.to_string())
        .bind(sqlx::types::Json(topup::payment_config::Terms::defaults(
            &self.route,
        )))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The payer's view of the address's quote, read with a client secret issued for it.
    async fn client_quote(&self) -> Result<Value> {
        let quote: Uuid = sqlx::query_scalar("SELECT quote_id FROM addresses WHERE id = $1")
            .bind(self.address_id)
            .fetch_one(&self.pool)
            .await?;
        let secret = support::issue_client_secret(&self.pool, &self.client_reads, quote).await?;
        support::client_quote(&self.api, &secret).await
    }

    async fn quote_status(&self) -> Result<(String, Option<Uuid>)> {
        let row = sqlx::query(
            "SELECT quote.status, quote.consumed_by, quote.exposure_reserved FROM quotes AS quote \
             JOIN addresses AS address ON address.quote_id = quote.id WHERE address.id = $1",
        )
        .bind(self.address_id)
        .fetch_one(&self.pool)
        .await?;
        let status: String = row.try_get("status")?;
        let reserved: bool = row.try_get("exposure_reserved")?;
        ensure!(
            reserved == (status == "open"),
            "an open quote reserves its exposure"
        );
        Ok((status, row.try_get("consumed_by")?))
    }
}

fn send(anvil: &Anvil, key: &str, call: &[&str]) -> Result<std::process::Output> {
    let mut arguments = vec![
        "send",
        "--json",
        "--rpc-url",
        &anvil.rpc_url,
        "--private-key",
        key,
    ];
    arguments.extend_from_slice(call);
    run_checked("cast", &arguments, None)
}

fn rpc(anvil: &Anvil, method: &str, params: &[&str]) -> Result<std::process::Output> {
    let mut arguments = vec!["rpc", "--rpc-url", &anvil.rpc_url, method];
    arguments.extend_from_slice(params);
    run_checked("cast", &arguments, None)
}

fn reader(rpc_url: &str) -> Result<FinalizedReader> {
    Ok(FinalizedReader::new(Arc::new(EvmClient::new(rpc_url)?)))
}

async fn seed_address(pool: &PgPool, chain_id: u64) -> Result<(Uuid, Uuid, Address)> {
    let (account, customer) = seed::create_account_and_customer(
        pool,
        &NewAccount {
            livemode: false,
            webhook_url: "https://product.test/webhooks".to_owned(),
            ..NewAccount::named("phala-cloud")
        },
        "workspace-fast",
    )
    .await?;
    seed::accept_assets(pool, account.id, false, chain_id, &["pha"]).await?;
    let (address_id, address) = issue_address(pool, chain_id, customer.id, 0x5a).await?;
    Ok((customer.id, address_id, address))
}

/// Issues the customer's address `0x<byte>…`, created at the current finalized cursor.
async fn issue_address(
    pool: &PgPool,
    chain_id: u64,
    customer_id: Uuid,
    byte: u8,
) -> Result<(Uuid, Address)> {
    let address = Address::repeat_byte(byte);
    let address_id = Uuid::new_v4();
    seed::insert_address(
        pool,
        &NewAddress {
            id: address_id,
            customer_id,
            chain_id,
            route: "phala-cloud-ethereum-pha-usd".to_owned(),
            salt: B256::repeat_byte(byte),
            address,
        },
    )
    .await?;
    Ok((address_id, address))
}

/// Provider B, `lag` blocks behind provider A's head.
struct LaggingReader {
    inner: FinalizedReader,
    lag: Arc<AtomicU64>,
}

impl ChainReader for LaggingReader {
    async fn factory_logs(
        &self,
        factory: Address,
        forwarders: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<topup_adapters::chain::evm::FactoryLog>, ChainError> {
        self.inner
            .factory_logs(factory, forwarders, from_block, to_block)
            .await
    }

    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        self.inner.finalized_head().await
    }

    async fn confirmation_heads(
        &self,
        confirmations: Confirmations,
    ) -> Result<ChainHeads, ChainError> {
        let heads = self.inner.confirmation_heads(confirmations).await?;
        let lag = self.lag.load(Ordering::SeqCst);
        Ok(ChainHeads {
            latest: heads.latest.map(|latest| latest.saturating_sub(lag)),
            ..heads
        })
    }

    async fn transfer_logs_to(
        &self,
        addresses: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<TransferLog>, ChainError> {
        self.inner
            .transfer_logs_to(addresses, from_block, to_block)
            .await
    }

    async fn receipt_transfer(
        &self,
        tx_hash: B256,
        receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        self.inner
            .receipt_transfer(tx_hash, receipt_log_index)
            .await
    }

    async fn nonce_at(&self, account: Address, block: u64) -> Result<u64, ChainError> {
        self.inner.nonce_at(account, block).await
    }
}

struct FixedPrice(Observation);

#[async_trait]
impl PriceSource for FixedPrice {
    async fn observe(&self) -> Result<Observation, PriceError> {
        Ok(self.0.clone())
    }
}

struct ClearSanctions;

#[async_trait]
impl SanctionsSource for ClearSanctions {
    async fn sanctions(&self, _address: Address, _block_number: u64) -> SanctionsResult {
        topup_core::screening::SanctionsResult::new(SanctionsVerdict::Clear)
    }
}
