//! A reorganization that changes the transfer at a credited deposit's receipt position, on
//! PostgreSQL with the pump and scripted providers (architecture §7): a contract-mediated transfer
//! re-executed against other state pays another amount, or another issued address, from the same
//! transaction and receipt position. The old deposit is reversed at finality, and the transfer now
//! at the position is a new deposit, credited once, whichever of the scanners and the finality
//! watch reads it first, and each deposit names the other (`replaces`, `replaced_by`).

mod support;

use std::sync::{Arc, Mutex};

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use topup::db::{self, NewDeposit};
use topup::finality::{FinalityWatch, WatchStats};
use topup::pump::{Pump, PumpConfig, RunOnceResult, StepSet};
use topup::routes::RouteSet;
use topup::scanner::{ChainRoutes, chain_routes, coverage_once};
use topup::steps::confirm::ConfirmStep;
use topup::steps::screen::{ScreenRoute, ScreenStep};
use topup_adapters::chain::evm::{
    ChainError, ChainReader, FactoryLog, FinalizedHead, ReceiptLookup, TransferLog,
};
use topup_adapters::pricing::{Observation, PriceError, PriceSource};
use topup_adapters::risk::SanctionsSource;
use topup_core::deposit::{DepositState, RejectReason};
use topup_core::identity::{
    credited_event_id, deposit_id, deposit_revision_id, event_id, reversed_event_id,
};
use topup_core::money::{AtomicAmount, PRICE_SCALE, ScaledPrice};
use topup_core::route::{ChainHeads, Confirmations, RouteFile};
use topup_core::screening::{SanctionsResult, SanctionsVerdict};
use topup_core::valuation::{SourceId, UnixSeconds};
use uuid::Uuid;

use support::TestDatabase;
use support::seed::{self, NewAccount, NewAddress};

const CHAIN_ID: u64 = 31_337;
const TOKEN: Address = Address::repeat_byte(0x55);
const OTHER_TOKEN: Address = Address::repeat_byte(0x56);
const ROUTER: Address = Address::repeat_byte(0x66);
const PAYER: Address = Address::repeat_byte(0x77);
const TX: B256 = B256::repeat_byte(0x0a);
const BLOCK_TIME: DateTime<Utc> = DateTime::UNIX_EPOCH;

#[tokio::test]
async fn a_changed_amount_reverses_the_deposit_and_credits_the_new_transfer_once() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let old = chain.credit_first(chain.recipient).await?;

            // The router's transaction is re-included one block later against other state: the
            // same receipt position now pays 90.
            let _new = chain.reorg(transfer(chain.recipient, 11, 0xbb, 90));

            // The finalized scanner and the reconciler read the new transfer while the old deposit
            // still holds the position: nothing is recorded.
            ensure!(chain.finalized_scan().await? == 0);

            // At finality the watch reverses the old deposit and records the new transfer.
            let stats = chain.watch().await?;
            ensure!(stats.reversed == 1, "{stats:?}");
            let successor = chain.successor(chain.recipient_id, 90).await?;
            chain.settle().await?;
            chain
                .assert_replaced(old, successor, chain.recipient_id, 90)
                .await?;

            // Read again, the transfer is a duplicate of the new deposit.
            ensure!(chain.watch().await?.reversed == 0);
            chain
                .assert_replaced(old, successor, chain.recipient_id, 90)
                .await
        })
    })
    .await
}

#[tokio::test]
async fn a_recipient_moved_to_another_issued_address_is_credited_there_once() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let old = chain.credit_first(chain.recipient).await?;
            let _new = chain.reorg(transfer(chain.other, 11, 0xbb, 100));

            // The watch reaches finality before the scanners reach the new block.
            let stats = chain.watch().await?;
            ensure!(stats.reversed == 1, "{stats:?}");
            let successor = chain.successor(chain.other_id, 100).await?;
            ensure!(chain.finalized_scan().await? == 0);
            chain.settle().await?;
            chain
                .assert_replaced(old, successor, chain.other_id, 100)
                .await
        })
    })
    .await
}

#[tokio::test]
async fn an_unchanged_re_inclusion_keeps_its_deposit() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let old = chain.credit_first(chain.recipient).await?;
            let new = chain.reorg(transfer(chain.recipient, 11, 0xbb, 100));

            ensure!(chain.finalized_scan().await? == 0);
            let stats = chain.watch().await?;
            ensure!(stats.finalized == 1 && stats.reversed == 0, "{stats:?}");
            chain.settle().await?;

            let deposit = chain.deposit(old).await?;
            ensure!(deposit.state == DepositState::Credited);
            ensure!(deposit.final_at.is_some());
            ensure!(
                deposit.block_hash == new.block_hash,
                "the evidence follows the new block"
            );
            ensure!(chain.count("SELECT count(*) FROM deposits").await? == 1);
            ensure!(chain.events("deposit.credited").await? == vec![credited_event_id(old)]);
            ensure!(chain.events("deposit.reversed").await?.is_empty());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_successor_of_an_unsupported_token_is_rejected_once() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let old = chain.credit_first(chain.recipient).await?;
            // The swap now pays out another token, which has no route.
            let _new = chain.reorg(TransferLog {
                token: OTHER_TOKEN,
                ..transfer(chain.recipient, 11, 0xbb, 100)
            });

            ensure!(chain.watch().await?.reversed == 1);
            let successor = deposit_revision_id(CHAIN_ID, TX, 0, 1);
            let rejected = chain.deposit(successor).await?;
            ensure!(rejected.state == DepositState::Rejected);
            ensure!(rejected.reason == Some(RejectReason::UnsupportedAsset));
            ensure!(rejected.asset_contract == OTHER_TOKEN && rejected.route.is_none());

            // Read again by the scanners, the transfer is a duplicate.
            ensure!(chain.finalized_scan().await? == 0);
            chain.settle().await?;
            ensure!(chain.count("SELECT count(*) FROM deposits").await? == 2);
            ensure!(
                chain.events("deposit.rejected").await?
                    == vec![event_id("deposit.rejected", successor)]
            );
            ensure!(chain.events("deposit.reversed").await? == vec![reversed_event_id(old)]);
            ensure!(chain.events("deposit.credited").await? == vec![credited_event_id(old)]);
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_transfer_moved_to_an_address_not_issued_leaves_no_successor() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            let old = chain.credit_first(chain.recipient).await?;
            let stale = transfer(chain.recipient, 10, 0xaa, 100);
            chain.reorg(transfer(Address::repeat_byte(0x99), 11, 0xbb, 100));

            ensure!(chain.watch().await?.reversed == 1);
            let evidence = chain.reversal_evidence(old).await?;
            ensure!(
                evidence["result"] == "transfer_changed_at_finality",
                "{evidence}"
            );
            ensure!(evidence.get("successor_deposit_id").is_none(), "{evidence}");

            // A provider still serving the orphaned block gives the per-block scan the old
            // transfer: the position is final, so the read is stale and records nothing.
            ensure!(chain.fast_scan(&stale).await? == 0);
            ensure!(chain.finalized_scan().await? == 0);
            ensure!(chain.watch().await?.reversed == 0);
            chain.settle().await?;
            ensure!(chain.count("SELECT count(*) FROM deposits").await? == 1);
            ensure!(chain.deposit(old).await?.state == DepositState::Reversed);
            ensure!(chain.snapshot("deposit.reversed", old).await?["replaced_by"].is_null());
            Ok(())
        })
    })
    .await
}

#[tokio::test]
async fn a_successor_that_still_pays_the_quote_takes_it_over() -> Result<()> {
    run(|chain| {
        Box::pin(async move {
            chain.open_quote(100).await?;
            let old = chain.credit_first(chain.recipient).await?;
            ensure!(chain.quote_consumed_by().await? == ("consumed".to_owned(), Some(old)));
            // The quote's window closes before the deposit is final.
            chain.close_quote_window().await?;

            // The router pays the same amount to the same address, from another pool.
            chain.reorg(TransferLog {
                from: Address::repeat_byte(0x67),
                ..transfer(chain.recipient, 11, 0xbb, 100)
            });
            ensure!(chain.watch().await?.reversed == 1);
            let successor = chain.successor(chain.recipient_id, 100).await?;
            ensure!(chain.quote_consumed_by().await? == ("consumed".to_owned(), Some(successor)));
            chain.settle().await?;

            chain
                .assert_replaced(old, successor, chain.recipient_id, 100)
                .await?;
            ensure!(chain.deposit(successor).await?.price_source.as_deref() == Some("lock"));
            ensure!(chain.quote_consumed_by().await? == ("consumed".to_owned(), Some(successor)));
            ensure!(chain.events("quote.expired").await?.is_empty());
            Ok(())
        })
    })
    .await
}

/// A transfer of the router's transaction at receipt position 0.
fn transfer(to: Address, block_number: u64, hash_byte: u8, amount: u64) -> TransferLog {
    TransferLog {
        tx_hash: TX,
        receipt_log_index: 0,
        log_index: 3,
        block_number,
        block_hash: B256::repeat_byte(hash_byte),
        block_time: BLOCK_TIME,
        tx_from: PAYER,
        tx_nonce: 7,
        token: TOKEN,
        from: ROUTER,
        to,
        amount: AtomicAmount::new(U256::from(amount)),
    }
}

/// Both providers: one canonical chain with at most the router's transfer on it.
#[derive(Clone, Default)]
struct ScriptedChain(Arc<Mutex<ChainState>>);

#[derive(Default)]
struct ChainState {
    head: u64,
    finalized: u64,
    transfer: Option<TransferLog>,
}

impl ScriptedChain {
    fn set(&self, head: u64, finalized: u64, transfer: TransferLog) {
        let mut state = self.0.lock().expect("chain state");
        *state = ChainState {
            head,
            finalized,
            transfer: Some(transfer),
        };
    }

    fn state<T>(&self, read: impl FnOnce(&ChainState) -> T) -> T {
        read(&self.0.lock().expect("chain state"))
    }
}

impl ChainReader for ScriptedChain {
    async fn header(&self, number: u64) -> Result<(B256, DateTime<Utc>), ChainError> {
        Ok((
            self.state(|state| {
                state
                    .transfer
                    .as_ref()
                    .filter(|log| log.block_number == number)
                    .map_or(B256::repeat_byte(0x55), |log| log.block_hash)
            }),
            BLOCK_TIME,
        ))
    }
    async fn finalized_head(&self) -> Result<FinalizedHead, ChainError> {
        Ok(FinalizedHead {
            number: self.state(|state| state.finalized),
            time: BLOCK_TIME,
        })
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

    async fn factory_logs(
        &self,
        _factory: Address,
        _forwarders: &[Address],
        _from_block: u64,
        _to_block: u64,
    ) -> Result<Vec<FactoryLog>, ChainError> {
        Ok(Vec::new())
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

    async fn receipt_transfer(
        &self,
        tx_hash: B256,
        receipt_log_index: u64,
    ) -> Result<ReceiptLookup, ChainError> {
        Ok(self.state(|state| match &state.transfer {
            Some(log) if log.tx_hash == tx_hash => ReceiptLookup::Included {
                block_number: log.block_number,
                block_hash: log.block_hash,
                block_time: log.block_time,
                status: true,
                tx_from: log.tx_from,
                tx_nonce: log.tx_nonce,
                transfer: (log.receipt_log_index == receipt_log_index)
                    .then(|| Box::new(log.clone())),
            },
            _ => ReceiptLookup::Missing,
        }))
    }

    async fn nonce_at(&self, _account: Address, _block: u64) -> Result<u64, ChainError> {
        Ok(8)
    }
}

async fn run<S>(scenario: S) -> Result<()>
where
    S: for<'a> FnOnce(&'a Scenario) -> support::TestFuture<'a>,
{
    let Some(database) = TestDatabase::create().await? else {
        return Ok(());
    };
    let result = async {
        let scenario_chain = Scenario::setup(&database).await?;
        scenario(&scenario_chain).await
    }
    .await;
    let cleanup = database.cleanup().await;
    result.and(cleanup)
}

struct Scenario {
    pool: PgPool,
    chain: ScriptedChain,
    route: RouteFile,
    routes: ChainRoutes,
    pump: Pump,
    watch: FinalityWatch,
    recipient: Address,
    recipient_id: Uuid,
    other: Address,
    other_id: Uuid,
}

impl Scenario {
    async fn setup(database: &TestDatabase) -> Result<Self> {
        let pool = database.app_pool.clone();
        let (account, customer) = seed::create_account_and_customer(
            &pool,
            &NewAccount {
                livemode: false,
                webhook_url: "https://product.test/webhooks".to_owned(),
                ..NewAccount::named("reorg")
            },
            "workspace-reorg",
        )
        .await?;
        seed::accept_assets(&pool, account.id, false, CHAIN_ID, &["pha"]).await?;
        let mut route: RouteFile = serde_saphyr::from_str(
            &include_str!("fixtures/phala-cloud-pha.yaml")
                .replace("chain_id: 1", &format!("chain_id: {CHAIN_ID}"))
                .replace("livemode: true", "livemode: false")
                .replace(
                    "0x6c5bA91642F10282b576d91922Ae6448C9d52f4E",
                    &format!("{TOKEN:#x}"),
                ),
        )?;
        route.chain.confirmations = Confirmations::Depth(2);
        route.asset.decimals = 0;
        route.asset.quote_amount_decimals = 0;
        route.merchant.min_amount = topup_core::route::Bounded::at(1);
        route.merchant.min_deposit_atomic =
            topup_core::route::Bounded::at(AtomicAmount::new(U256::ZERO));
        route.validate()?;
        let issue = |byte: u8| {
            let pool = pool.clone();
            let route = route.route.clone();
            async move {
                let address = NewAddress {
                    id: Uuid::new_v4(),
                    customer_id: customer.id,
                    chain_id: CHAIN_ID,
                    route,
                    salt: B256::repeat_byte(byte),
                    address: Address::repeat_byte(byte),
                };
                seed::insert_address(&pool, &address).await?;
                Ok::<_, anyhow::Error>((address.address, address.id))
            }
        };
        let (recipient, recipient_id) = issue(0x5a).await?;
        let (other, other_id) = issue(0x5b).await?;

        let route_set = Arc::new(RouteSet::new(vec![route.clone()]).map_err(anyhow::Error::msg)?);
        let routes = chain_routes(&route_set)
            .into_iter()
            .next()
            .context("one chain route")?;
        let chain = ScriptedChain::default();
        let now = u64::try_from(Utc::now().timestamp())?;
        let price = |source: &str, value: u64| -> Result<Arc<dyn PriceSource>> {
            Ok(Arc::new(FixedPrice(Observation {
                source: SourceId::new(source),
                price: ScaledPrice::new(value, PRICE_SCALE).context("fixture price")?,
                observed_at: UnixSeconds::new(now),
            })))
        };
        let confirm = ConfirmStep::single(
            pool.clone(),
            route.clone(),
            chain.clone(),
            chain.clone(),
            price("kraken", 10_000_000)?,
            Some(price("binance", 10_000_000)?),
            Some(price("kraken", 100_000_000)?),
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
            route_set,
            CHAIN_ID,
            chain.clone(),
            chain.clone(),
        )
        .with_pump(Arc::new(pump.clone()));
        Ok(Self {
            pool,
            chain,
            route,
            routes,
            pump,
            watch,
            recipient,
            recipient_id,
            other,
            other_id,
        })
    }

    /// The router pays 100 to `to` in block 10; the fast scanner records it at depth 2 and the
    /// pump credits it before finality. Returns the deposit id.
    async fn credit_first(&self, to: Address) -> Result<Uuid> {
        let first = transfer(to, 10, 0xaa, 100);
        self.chain.set(12, 5, first.clone());
        db::initialize_cursor(&self.pool, CHAIN_ID, 5, BLOCK_TIME).await?;
        let fast = db::commit_confirmed_scan(&self.pool, CHAIN_ID, &[self.deposit_of(&first)?], 10)
            .await?;
        ensure!(fast.inserted == 1);
        self.settle().await?;
        let id = deposit_id(CHAIN_ID, TX, 0);
        let credited = self.deposit(id).await?;
        ensure!(credited.state == DepositState::Credited);
        ensure!(credited.final_at.is_none(), "a depth-2 credit is not final");
        Ok(id)
    }

    /// Replaces the canonical chain: `transfer` is the router's transaction's transfer at the same
    /// receipt position, and its block is final.
    fn reorg(&self, transfer: TransferLog) -> TransferLog {
        self.chain.set(30, 20, transfer.clone());
        transfer
    }

    /// The chain's finalized scanner pass; returns the deposits it recorded.
    async fn finalized_scan(&self) -> Result<u64> {
        topup::checkpoint::advance(&self.pool, CHAIN_ID, &self.chain, &self.chain).await?;
        Ok(
            coverage_once(&self.pool, &self.chain, &self.chain, &self.routes, 1)
                .await?
                .inserted,
        )
    }

    /// The per-block scan's commit of `transfer`, read at the route's confirmation; returns the
    /// deposits it recorded.
    async fn fast_scan(&self, transfer: &TransferLog) -> Result<u64> {
        let deposit = self.deposit_of(transfer)?;
        Ok(
            db::commit_confirmed_scan(&self.pool, CHAIN_ID, &[deposit], transfer.block_number)
                .await?
                .inserted,
        )
    }

    fn deposit_of(&self, transfer: &TransferLog) -> Result<NewDeposit> {
        let address_id = if transfer.to == self.recipient {
            self.recipient_id
        } else if transfer.to == self.other {
            self.other_id
        } else {
            bail!("the transfer pays no issued address")
        };
        let routed = transfer.token == TOKEN;
        Ok(NewDeposit {
            chain_id: CHAIN_ID,
            tx_hash: transfer.tx_hash,
            receipt_log_index: transfer.receipt_log_index,
            log_index: transfer.log_index,
            block_number: transfer.block_number,
            block_hash: transfer.block_hash,
            block_time: transfer.block_time,
            address_id,
            route: routed.then(|| self.route.route.clone()),
            route_version: routed.then_some(self.route.version),
            asset_contract: transfer.token,
            from_address: transfer.from,
            amount_atomic: transfer.amount,
            state: if routed {
                DepositState::Detected
            } else {
                DepositState::Rejected
            },
            reason: (!routed).then_some(RejectReason::UnsupportedAsset),
            next_attempt_at: Utc::now(),
            tx_from: transfer.tx_from,
            tx_nonce: transfer.tx_nonce,
            is_final: false,
        })
    }

    async fn watch(&self) -> Result<WatchStats> {
        let head = self.chain.finalized_head().await?;
        let (hash, time) = self.chain.header(head.number).await?;
        db::chain_reads::advance_checkpoint(
            &self.pool,
            CHAIN_ID,
            db::chain_reads::Boundary {
                number: head.number,
                hash,
                time,
            },
        )
        .await?;
        let clock: DateTime<Utc> = sqlx::query_scalar(
            "SELECT GREATEST(now(),COALESCE(max(finality_check_at),now())) FROM deposits WHERE deposit_finality_pending(deposits)",
        ).fetch_one(&self.pool).await?;
        Ok(self.watch.watch_once_at(CHAIN_ID, clock).await?)
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
        bail!("the pump did not settle")
    }

    /// The deposit the watch recorded for the transfer now at the position: a new id, the next
    /// revision, to `address_id` for `amount`.
    async fn successor(&self, address_id: Uuid, amount: u64) -> Result<Uuid> {
        let id = deposit_revision_id(CHAIN_ID, TX, 0, 1);
        ensure!(id != deposit_id(CHAIN_ID, TX, 0));
        let deposit = self.deposit(id).await?;
        ensure!(deposit.state == DepositState::Confirmed);
        ensure!(deposit.address_id == address_id);
        ensure!(deposit.amount_atomic == AtomicAmount::new(U256::from(amount)));
        ensure!(deposit.block_number == 11 && deposit.receipt_log_index == 0);
        Ok(id)
    }

    /// The old deposit is reversed and names its successor; the successor is credited once, at
    /// finality, and the merchant saw exactly: the old credit, its reversal, the new credit.
    async fn assert_replaced(
        &self,
        old: Uuid,
        new: Uuid,
        address_id: Uuid,
        amount: u64,
    ) -> Result<()> {
        let reversed = self.deposit(old).await?;
        ensure!(reversed.state == DepositState::Reversed);
        let evidence = self.reversal_evidence(old).await?;
        ensure!(
            evidence["result"] == "transfer_changed_at_finality",
            "{evidence}"
        );
        ensure!(
            evidence["successor_deposit_id"] == new.to_string(),
            "{evidence}"
        );

        let credited = self.deposit(new).await?;
        ensure!(credited.state == DepositState::Credited);
        ensure!(credited.final_at.is_some());
        ensure!(credited.address_id == address_id);
        ensure!(credited.amount_atomic == AtomicAmount::new(U256::from(amount)));

        ensure!(self.count("SELECT count(*) FROM deposits").await? == 2);
        ensure!(
            self.count("SELECT count(*) FROM deposits WHERE state = 'credited'")
                .await?
                == 1
        );
        let mut credits = vec![credited_event_id(old), credited_event_id(new)];
        credits.sort();
        ensure!(self.events("deposit.credited").await? == credits);
        ensure!(self.events("deposit.reversed").await? == vec![reversed_event_id(old)]);

        // The webhook snapshots link the two deposits.
        let (old_id, new_id) = (public_id(old), public_id(new));
        let reversal = self.snapshot("deposit.reversed", old).await?;
        ensure!(reversal["replaced_by"] == new_id.as_str(), "{reversal}");
        ensure!(reversal["replaces"].is_null(), "{reversal}");
        let credit = self.snapshot("deposit.credited", new).await?;
        ensure!(credit["replaces"] == old_id.as_str(), "{credit}");
        ensure!(credit["replaced_by"].is_null(), "{credit}");
        Ok(())
    }

    async fn reversal_evidence(&self, id: Uuid) -> Result<serde_json::Value> {
        Ok(sqlx::query_scalar(
            "SELECT evidence FROM transitions WHERE deposit_id = $1 AND to_state = 'reversed'",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The deposit object of deposit `id`'s `event_type` event.
    async fn snapshot(&self, event_type: &str, id: Uuid) -> Result<serde_json::Value> {
        Ok(sqlx::query_scalar(
            "SELECT data->'object' FROM events WHERE type = $1 AND data->'object'->>'id' = $2",
        )
        .bind(event_type)
        .bind(public_id(id))
        .fetch_one(&self.pool)
        .await?)
    }

    /// Opens the recipient's quote for `amount`, its window an hour long.
    async fn open_quote(&self, amount: u64) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE quotes
            SET route = $2, amount_atomic = $3::text::numeric, price_scaled = 9000000,
                expires_at = now() + interval '1 hour', credit_minor = 900, status = 'open',
                exposure_reserved = true, closed_at = NULL, terms = $4
            WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)
            "#,
        )
        .bind(self.recipient_id)
        .bind(&self.route.route)
        .bind(amount.to_string())
        .bind(sqlx::types::Json(topup::payment_config::Terms::defaults(
            &self.route,
        )))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Moves the recipient's quote window into the past; the transfers stay inside it.
    async fn close_quote_window(&self) -> Result<()> {
        sqlx::query(
            "UPDATE quotes SET expires_at = now() - interval '1 minute' \
             WHERE id = (SELECT quote_id FROM addresses WHERE id = $1)",
        )
        .bind(self.recipient_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn quote_consumed_by(&self) -> Result<(String, Option<Uuid>)> {
        Ok(sqlx::query_as(
            "SELECT quote.status, quote.consumed_by FROM quotes AS quote \
             JOIN addresses AS address ON address.quote_id = quote.id WHERE address.id = $1",
        )
        .bind(self.recipient_id)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn deposit(&self, id: Uuid) -> Result<db::Deposit> {
        db::get_deposit(&self.pool, id)
            .await?
            .with_context(|| format!("deposit {id}"))
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
}

fn public_id(id: Uuid) -> String {
    topup::ids::format(topup::ids::DEPOSIT, id)
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
