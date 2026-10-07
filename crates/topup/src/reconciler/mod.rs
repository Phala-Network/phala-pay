//! Periodic custody and credit reconciliation.
//!
//! Regular ledger checks run each round; scheduled dual custody runs at most once per hour,
//! independently per chain and token route. Per-deposit failures are
//! recorded as findings; a check that cannot complete is reported in
//! [`ReconciliationReport::failed_checks`] and withholds the round heartbeat.
//!
//! Chain reads stay proportional to what changed: the service's rounds run only after the
//! agreed checkpoint advances, reuse the head the scanner published, verify each stored
//! `(address, salt, treasury)` against the factory with a bounded LRU cache, and read balances only of
//! forwarders that hold unswept funds by the ledger.

mod chain;
pub(crate) mod store;
mod types;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alloy_primitives::Address;
use serde_json::json;
use sqlx::PgPool;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use topup_adapters::chain::evm::FinalizedReader;
use topup_core::money::{PRICE_SCALE, ScaledPrice, credit};
use topup_core::route::{RouteFile, UNIT_DECIMALS};
use uuid::Uuid;

use crate::db::{self, ApplyTransitionError};
use crate::routes::RouteSet;
use crate::scanner::FinalizedHeads;

pub use chain::ReconciliationChain;
pub use store::{LeaseOwnerLock, chain_is_blocked, frozen_chains, hold_lease_owner_lock};
pub use types::{CheckName, Finding, ReconciliationReport};

/// Order in which a round runs its checks; derivation runs first so a freeze lands early.
const REGULAR_CHECKS: [CheckName; 4] = [
    CheckName::AddressDerivation,
    CheckName::CreditRecomputation,
    CheckName::MissingFlushLink,
    CheckName::CustodyBalance,
];

/// Reconciliation failure which prevents one check or one subject from completing.
#[derive(Debug, thiserror::Error)]
pub enum ReconciliationError {
    /// Runtime configuration is invalid or incomplete.
    #[error("{0}")]
    Configuration(String),
    /// A chain adapter failed.
    #[error("{0}")]
    Chain(String),
    /// PostgreSQL failed an operation.
    #[error("{0}")]
    Database(#[from] sqlx::Error),
    /// A finding could not be encoded.
    #[error("{0}")]
    Encode(#[from] serde_json::Error),
    /// Durable data violated an internal invariant.
    #[error("{0}")]
    Invariant(&'static str),
    /// The lease-owner lock is held in a conflicting mode by another process.
    #[error("{0}")]
    LeaseOwnerLock(&'static str),
}

impl ReconciliationError {
    /// Returns a stable category safe to persist in findings.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Configuration(_) => "configuration",
            Self::Chain(_) => "chain_unavailable",
            Self::Database(_) => "database",
            Self::Encode(_) => "encode",
            Self::Invariant(message) => message,
            Self::LeaseOwnerLock(_) => "lease_owner_lock_held",
        }
    }
}

impl From<topup_adapters::chain::evm::ChainError> for ReconciliationError {
    fn from(error: topup_adapters::chain::evm::ChainError) -> Self {
        Self::Chain(error.to_string())
    }
}

impl From<ApplyTransitionError> for ReconciliationError {
    fn from(error: ApplyTransitionError) -> Self {
        match error {
            ApplyTransitionError::Database(error) => Self::Database(error),
            ApplyTransitionError::InvalidInput(message) => Self::Invariant(message),
        }
    }
}

/// Finalized heads read once per chain and shared by every check in a round.
type RoundHeads = BTreeMap<u64, u64>;

/// A stored address the factory derived identically: `(chain, row, salt, treasury, address)`.
type VerifiedDerivation = (u64, Uuid, alloy_primitives::B256, Address, Address);

/// Bounded LRU cache. A changed row has a different key; eviction re-verifies old addresses.
#[derive(Default)]
struct VerifiedCache {
    keys: BTreeMap<VerifiedDerivation, u128>,
    recency: BTreeSet<(u128, VerifiedDerivation)>,
    clock: u128,
}
impl VerifiedCache {
    fn contains(&mut self, key: &VerifiedDerivation) -> bool {
        if self.keys.contains_key(key) {
            self.insert(*key);
            true
        } else {
            false
        }
    }
    fn insert(&mut self, key: VerifiedDerivation) {
        self.clock = self.clock.saturating_add(1);
        if let Some(old) = self.keys.insert(key, self.clock) {
            self.recency.remove(&(old, key));
        }
        self.recency.insert((self.clock, key));
        while self.keys.len() > db::ADDRESS_PAGE_SIZE * 4 {
            if let Some((_, old)) = self.recency.pop_first() {
                self.keys.remove(&old);
            }
        }
    }
}

/// Runs every §13 check against configured routes and dependencies.
pub struct Reconciler {
    pool: PgPool,
    routes: Arc<RouteSet>,
    chains: BTreeMap<u64, Arc<dyn ReconciliationChain>>,
    verifiers: BTreeMap<u64, Arc<dyn ReconciliationChain>>,
    /// Derivations already confirmed on chain; a changed row is a new key and is read again.
    verified: Mutex<VerifiedCache>,
}

impl Reconciler {
    /// Builds production reconciliation dependencies on each chain's read and verify endpoints.
    pub fn from_routes(pool: PgPool, routes: Arc<RouteSet>) -> Result<Self, ReconciliationError> {
        let mut chains = BTreeMap::<u64, Arc<dyn ReconciliationChain>>::new();
        for chain_id in routes.chain_ids() {
            let client = routes.provider(chain_id, 0).map_err(|error| {
                ReconciliationError::Configuration(format!("reconciler chain {chain_id}: {error}"))
            })?;
            chains.insert(chain_id, Arc::new(FinalizedReader::new(Arc::clone(client))));
        }
        let mut verifiers = BTreeMap::<u64, Arc<dyn ReconciliationChain>>::new();
        for chain in routes.chain_ids() {
            verifiers.insert(
                chain,
                Arc::new(FinalizedReader::new(
                    routes
                        .provider(chain, 1)
                        .map_err(|e| ReconciliationError::Configuration(e.to_string()))?
                        .clone(),
                )),
            );
        }
        Ok(Self {
            pool,
            routes,
            chains,
            verifiers,
            verified: Mutex::default(),
        })
    }

    /// Builds a reconciler with explicit dependencies for integration tests.
    #[must_use]
    pub fn with_dependencies(
        pool: PgPool,
        routes: Arc<RouteSet>,
        chains: BTreeMap<u64, Arc<dyn ReconciliationChain>>,
    ) -> Self {
        Self {
            pool,
            routes,
            verifiers: chains.clone(),
            chains,
            verified: Mutex::default(),
        }
    }

    /// Inject independent verification reads for custody disagreement tests.
    pub fn with_verifiers(
        mut self,
        verifiers: BTreeMap<u64, Arc<dyn ReconciliationChain>>,
    ) -> Self {
        self.verifiers = verifiers;
        self
    }

    /// Runs all regular reconciliation checks once.
    ///
    /// Check failures are reported in [`ReconciliationReport::failed_checks`]; the other checks
    /// still run. The `Result` is kept for callers of the library entry point.
    pub async fn run_once(&self) -> Result<ReconciliationReport, ReconciliationError> {
        crate::rpc_runtime::ensure_checkpoints(&self.pool, &self.routes)
            .await
            .map_err(ReconciliationError::Chain)?;
        Ok(self.run_checks(false).await)
    }

    /// Runs the post-restore round: every regular check, on the restored ledger alone.
    ///
    /// The service is authoritative for its credits, so a restore asks the product nothing. The
    /// round refuses to run while any process holds the [`LeaseOwnerLock`], so no pump works on
    /// the restored ledger meanwhile.
    pub async fn post_restore_once(&self) -> Result<ReconciliationReport, ReconciliationError> {
        let lock = store::exclusive_lease_owner_lock(&self.pool).await?;
        let result = async {
            crate::rpc_runtime::ensure_checkpoints(&self.pool, &self.routes)
                .await
                .map_err(ReconciliationError::Chain)?;
            Ok(self.run_checks(true).await)
        }
        .await;
        if let Err(error) = lock.release().await {
            tracing::warn!(%error, "failed to release the post-restore lease-owner lock");
        }
        result
    }

    /// Runs one bounded check page without persisting its findings. Repeated calls resume
    /// the durable cursor; completed row passes wrap so old rows are checked again.
    ///
    /// Safe repairs and freezes still apply: `missing_flush_link` writes the
    /// ledger, and `address_derivation` and `custody_balance` freeze a chain, exactly as a full
    /// round does.
    pub async fn check(&self, check: CheckName) -> Result<Vec<Finding>, ReconciliationError> {
        crate::rpc_runtime::ensure_checkpoints(&self.pool, &self.routes)
            .await
            .map_err(ReconciliationError::Chain)?;
        let mut findings = Vec::new();
        self.run_check(check, &mut RoundHeads::new(), &mut findings)
            .await?;
        Ok(findings)
    }

    async fn run_checks(&self, post_restore: bool) -> ReconciliationReport {
        self.run_checks_at(post_restore, RoundHeads::new(), true)
            .await
    }

    /// Runs a round with finalized heads already known for some chains; the others are read.
    async fn run_checks_at(
        &self,
        post_restore: bool,
        mut heads: RoundHeads,
        custody_due: bool,
    ) -> ReconciliationReport {
        let mut report = ReconciliationReport::default();
        for check in REGULAR_CHECKS {
            if check == CheckName::CustodyBalance && !custody_due {
                continue;
            }
            let mut findings = Vec::new();
            let mut result = async {
                if post_restore {
                    self.reset_check_cursor(check).await?;
                }
                loop {
                    self.run_check(check, &mut heads, &mut findings).await?;
                    if !post_restore || !self.check_pending(check).await? {
                        break;
                    }
                }
                Ok::<(), ReconciliationError>(())
            }
            .await;
            for finding in &findings {
                match store::persist_finding(&self.pool, finding).await {
                    Ok(inserted) => log_finding(finding, inserted),
                    Err(error) => result = result.and(Err(error)),
                }
            }
            if let Err(error) = result {
                tracing::error!(check = check.code(), %error, "reconciliation check failed");
                report.failed_checks.push(check);
                report.check_errors.push(error.to_string());
            }
            report.findings.extend(findings);
        }
        report.incomplete = report.findings.iter().any(|finding| finding.incomplete);
        if report.succeeded() {
            tracing::info!(
                findings = report.findings.len(),
                post_restore,
                "reconciler heartbeat"
            );
        } else {
            let failed = report
                .failed_checks
                .iter()
                .map(|check| check.code())
                .collect::<Vec<_>>();
            tracing::error!(?failed, post_restore, "reconciliation round incomplete");
        }
        report
    }

    async fn run_check(
        &self,
        check: CheckName,
        heads: &mut RoundHeads,
        findings: &mut Vec<Finding>,
    ) -> Result<(), ReconciliationError> {
        match check {
            CheckName::AddressDerivation => self.address_derivation(findings).await,
            CheckName::CreditRecomputation => self.credit_recomputation(findings).await,
            CheckName::MissingFlushLink => self.missing_flush_links(findings).await,
            CheckName::CustodyBalance => self.custody_balances(heads, findings).await,
        }
    }

    fn work_names(&self, check: CheckName) -> Vec<String> {
        match check {
            CheckName::AddressDerivation => vec!["derivation".into()],
            CheckName::CreditRecomputation => vec!["credit".into()],
            CheckName::MissingFlushLink => vec!["flush".into()],
            CheckName::CustodyBalance => self
                .latest_asset_routes()
                .iter()
                .map(|r| format!("custody:{:#x}", r.asset.contract))
                .collect(),
        }
    }
    async fn reset_check_cursor(&self, check: CheckName) -> Result<(), ReconciliationError> {
        let mut chains = self.chain_keys()?;
        chains.push(0);
        sqlx::query(
            "UPDATE reconciliation_work_cursors SET last_id=NULL \
             WHERE check_name=ANY($1) AND chain_id=ANY($2)",
        )
        .bind(self.work_names(check))
        .bind(chains)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    async fn check_pending(&self, check: CheckName) -> Result<bool, ReconciliationError> {
        let mut chains = self.chain_keys()?;
        chains.push(0);
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 \
             FROM reconciliation_work_cursors \
             WHERE check_name=ANY($1) \
             AND chain_id=ANY($2) \
             AND last_id IS NOT NULL)",
        )
        .bind(self.work_names(check))
        .bind(chains)
        .fetch_one(&self.pool)
        .await?)
    }
    fn chain_keys(&self) -> Result<Vec<i64>, ReconciliationError> {
        self.chains
            .keys()
            .map(|id| {
                i64::try_from(*id).map_err(|_| ReconciliationError::Invariant("chain overflow"))
            })
            .collect()
    }
    async fn work_pending(&self) -> Result<bool, ReconciliationError> {
        for check in REGULAR_CHECKS {
            if self.check_pending(check).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Runs a round every `every` in which the agreed checkpoint, as `heads` publishes it,
    /// advanced on some chain since the last complete round, until cancellation. A round without
    /// an advance would read the same finalized state, so it is skipped and reported healthy.
    pub async fn run_loop(
        &self,
        every: Duration,
        heads: FinalizedHeads,
        cancellation: CancellationToken,
    ) {
        let monitor = crate::observability::CronMonitor::reconciler(every);
        let mut ticks = interval(every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut reconciled: Option<RoundHeads> = None;
        let custody_rounds = Duration::from_secs(3_600)
            .as_nanos()
            .div_ceil(every.as_nanos())
            .max(1);
        let mut round = 0_u128;
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = ticks.tick() => {}
            }
            let custody_due = round.is_multiple_of(custody_rounds);
            round = round.wrapping_add(1);
            let published = self
                .chains
                .keys()
                .filter_map(|chain_id| Some((*chain_id, heads.get(*chain_id)?.number)))
                .collect::<RoundHeads>();
            if !published.is_empty() && reconciled.as_ref() == Some(&published) {
                match self.work_pending().await {
                    Ok(false) => {
                        monitor.check_in(true);
                        continue;
                    }
                    Ok(true) => {}
                    Err(error) => {
                        tracing::warn!(%error,"could not read reconciliation work cursors");
                    }
                }
            }
            tokio::select! {
                () = cancellation.cancelled() => return,
                report = self.run_checks_at(false, published.clone(), custody_due) => {
                    monitor.check_in(report.succeeded());
                    if report.succeeded() {
                        reconciled = Some(published);
                    }
                    crate::observability::record_reconciliation(
                        report
                            .failed_checks
                            .iter()
                            .map(|check| check.code().to_owned())
                            .zip(report.check_errors)
                            .collect(),
                    );
                }
            }
        }
    }

    /// Verifies every stored `(salt, treasury)` with the on-chain factory and freezes mismatching
    /// chains.
    async fn address_derivation(
        &self,
        findings: &mut Vec<Finding>,
    ) -> Result<(), ReconciliationError> {
        let mut failure = None;
        for (chain_id, factory) in self.chain_factories()? {
            if let Err(error) = self
                .address_derivation_for_chain(chain_id, factory, findings)
                .await
            {
                tracing::error!(chain_id, %error, "address derivation check failed for chain");
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    async fn address_derivation_for_chain(
        &self,
        chain_id: u64,
        factory: Address,
        findings: &mut Vec<Finding>,
    ) -> Result<(), ReconciliationError> {
        let chain = self.chain(chain_id)?;
        let key = |address: &db::Address| {
            (
                chain_id,
                address.id,
                address.salt,
                address.treasury,
                address.address,
            )
        };
        let mut by_treasury = BTreeMap::<Address, Vec<db::Address>>::new();
        let cursor = store::work_cursor(
            &self.pool,
            "derivation",
            i64::try_from(chain_id)
                .map_err(|_| ReconciliationError::Invariant("chain overflow"))?,
        )
        .await?;
        let (stored, more) = db::chain_address_page(&self.pool, chain_id, cursor).await?;
        let last = stored.last().map(|a| a.id);
        {
            let mut verified = self.verified.lock().unwrap_or_else(PoisonError::into_inner);
            for address in stored {
                if verified.contains(&key(&address)) {
                    continue;
                }
                by_treasury
                    .entry(address.treasury)
                    .or_default()
                    .push(address);
            }
        }
        for (treasury, addresses) in by_treasury {
            let salts = addresses
                .iter()
                .map(|address| address.salt)
                .collect::<Vec<_>>();
            let derived = chain.factory_addresses(factory, treasury, &salts).await?;
            if derived.len() != addresses.len() {
                return Err(ReconciliationError::Invariant(
                    "addressOf response length did not match address count",
                ));
            }
            for (stored, observed) in addresses.iter().zip(derived) {
                if stored.address == observed {
                    self.verified
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(key(stored));
                    continue;
                }
                store::block_chain(
                    &self.pool,
                    chain_id,
                    CheckName::AddressDerivation.code(),
                    "factory addressOf(treasury, salt) disagrees with stored address",
                )
                .await?;
                findings.push(Finding::new(
                    CheckName::AddressDerivation,
                    subjects([
                        ("chain_id", chain_id.to_string()),
                        ("address_id", stored.id.to_string()),
                        ("salt", format!("{:#x}", stored.salt)),
                        ("treasury", format!("{treasury:#x}")),
                    ]),
                    json!({"address": format!("{observed:#x}")}),
                    json!({"address": format!("{:#x}", stored.address)}),
                    false,
                    false,
                )?);
            }
        }
        store::save_work_cursor(
            &self.pool,
            "derivation",
            i64::try_from(chain_id)
                .map_err(|_| ReconciliationError::Invariant("chain overflow"))?,
            if more { last } else { None },
        )
        .await?;
        Ok(())
    }

    /// Recomputes credits from their recorded valuation evidence.
    async fn credit_recomputation(
        &self,
        findings: &mut Vec<Finding>,
    ) -> Result<(), ReconciliationError> {
        let cursor = store::work_cursor(&self.pool, "credit", 0).await?;
        let mut ids = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT id FROM deposits
            WHERE credit_minor IS NOT NULL AND price_scaled IS NOT NULL
              AND route IS NOT NULL AND route_version IS NOT NULL
              AND id >= $1 AND ($2::uuid IS NULL OR id <> $2)
            ORDER BY id LIMIT 1001
            "#,
        )
        .bind(cursor.unwrap_or(Uuid::nil()))
        .bind(cursor)
        .fetch_all(&self.pool)
        .await?;
        let more = ids.len() > db::ADDRESS_PAGE_SIZE;
        ids.truncate(db::ADDRESS_PAGE_SIZE);
        let last = ids.last().copied();
        let routes = self.route_index();
        let mut deposits = db::deposits_by_ids(&self.pool, &ids)
            .await?
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let address_ids = deposits
            .values()
            .filter_map(|d| {
                d.as_ref()
                    .ok()
                    .filter(|d| d.price_source.as_deref() == Some("lock"))
                    .map(|d| d.address_id)
            })
            .collect::<Vec<_>>();
        let quote_credits = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT a.id,q.credit_minor::text \
             FROM addresses a \
             JOIN quotes q ON q.id=a.quote_id \
             WHERE a.id=ANY($1)",
        )
        .bind(address_ids)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        for id in ids {
            let result = match deposits.remove(&id) {
                Some(Ok(deposit)) => Self::recompute_credit(&deposit, &quote_credits, &routes),
                Some(Err(error)) => Err(error.into()),
                None => Err(ReconciliationError::Invariant("listed deposit disappeared")),
            };
            match result {
                Ok(Some(finding)) => findings.push(finding),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(deposit_id = %crate::ids::format(crate::ids::DEPOSIT, id), %error, "credit recomputation failed");
                    findings.push(Finding::new(
                        CheckName::CreditRecomputation,
                        subjects([("deposit_id", id.to_string())]),
                        json!({"credit_minor": "recomputed"}),
                        json!({"error": error.code()}),
                        false,
                        false,
                    )?);
                }
            }
        }
        store::save_work_cursor(&self.pool, "credit", 0, if more { last } else { None }).await?;
        Ok(())
    }

    fn recompute_credit(
        deposit: &db::Deposit,
        quote_credits: &BTreeMap<Uuid, String>,
        routes: &BTreeMap<(String, u64), &RouteFile>,
    ) -> Result<Option<Finding>, ReconciliationError> {
        let route_name = deposit
            .route
            .as_ref()
            .ok_or(ReconciliationError::Invariant(
                "valued deposit has no route",
            ))?;
        let route_version = deposit.route_version.ok_or(ReconciliationError::Invariant(
            "valued deposit has no route version",
        ))?;
        let deposit_subjects = || {
            subjects([
                ("deposit_id", deposit.id.to_string()),
                ("address_id", deposit.address_id.to_string()),
                ("chain_id", deposit.chain_id.to_string()),
            ])
        };
        let Some(route) = routes.get(&(route_name.clone(), route_version)) else {
            return Ok(Some(Finding::new(
                CheckName::CreditRecomputation,
                deposit_subjects(),
                json!({"credit_minor": "recomputed"}),
                json!({
                    "error": "route_version_unavailable",
                    "route": route_name,
                    "route_version": route_version,
                }),
                false,
                false,
            )?));
        };
        let stored = deposit.credit_minor.ok_or(ReconciliationError::Invariant(
            "listed deposit has no stored credit",
        ))?;
        let expected = if deposit.price_source.as_deref() == Some("lock") {
            quote_credits
                .get(&deposit.address_id)
                .ok_or(ReconciliationError::Invariant(
                    "lock-priced deposit has no rate lock",
                ))?
                .parse::<u64>()
                .map_err(|_| ReconciliationError::Invariant("rate-lock credit is invalid"))?
        } else {
            let price = ScaledPrice::new(
                deposit.price_scaled.ok_or(ReconciliationError::Invariant(
                    "listed deposit has no stored price",
                ))?,
                PRICE_SCALE,
            )
            .map_err(|_| ReconciliationError::Invariant("stored price is invalid"))?;
            credit(
                deposit.amount_atomic,
                price,
                route.asset.decimals,
                UNIT_DECIMALS,
            )
            .map_err(|_| ReconciliationError::Invariant("stored credit cannot be recomputed"))?
            .value()
        };
        if expected == stored.value() {
            return Ok(None);
        }
        Ok(Some(Finding::new(
            CheckName::CreditRecomputation,
            deposit_subjects(),
            json!({"credit_minor": expected.to_string()}),
            json!({"credit_minor": stored.value().to_string()}),
            false,
            false,
        )?))
    }

    /// Sweeps every final credited deposit that an indexed finalized `Flushed` event after it
    /// covers, with the rule the scanner and the finality watch apply.
    async fn missing_flush_links(
        &self,
        findings: &mut Vec<Finding>,
    ) -> Result<(), ReconciliationError> {
        let cursor = store::work_cursor(&self.pool, "flush", 0).await?;
        let mut ids = sqlx::query_scalar::<_, Uuid>(
            "SELECT id \
             FROM deposits \
             WHERE state='credited' \
             AND final_at IS NOT NULL \
             AND id >= $1 \
             AND ($2::uuid IS NULL OR id <> $2) \
             ORDER BY id \
             LIMIT 1001",
        )
        .bind(cursor.unwrap_or(Uuid::nil()))
        .bind(cursor)
        .fetch_all(&self.pool)
        .await?;
        let more = ids.len() > db::ADDRESS_PAGE_SIZE;
        ids.truncate(db::ADDRESS_PAGE_SIZE);
        let last = ids.last().copied();
        let mut transaction = self.pool.begin().await?;
        let mut swept = Vec::new();
        for id in ids {
            swept.extend(db::mark_swept(&mut transaction, Some(id), &[]).await?);
        }
        transaction.commit().await?;
        for deposit_id in swept {
            findings.push(Finding::new(
                CheckName::MissingFlushLink,
                subjects([("deposit_id", deposit_id.to_string())]),
                json!({"state": "swept"}),
                json!({"state": "credited"}),
                true,
                false,
            )?);
        }
        store::save_work_cursor(&self.pool, "flush", 0, if more { last } else { None }).await?;
        Ok(())
    }

    /// Checks, per forwarder, that its finalized balance is its deposits minus its sweeps, and
    /// freezes the chain on any mismatch (design §13).
    async fn custody_balances(
        &self,
        heads: &mut RoundHeads,
        findings: &mut Vec<Finding>,
    ) -> Result<(), ReconciliationError> {
        let mut failure = None;
        for route in self.latest_asset_routes() {
            if let Err(error) = self.custody_for_route(route, heads, findings).await {
                tracing::error!(
                    chain_id = route.chain.chain_id,
                    route = %route.route,
                    %error,
                    "custody balance check failed for route"
                );
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Compares, at one finalized block, every active forwarder's balance of the route's token
    /// with its deposits minus its finalized `Flushed` amounts.
    ///
    /// The block is the lower of the finalized head and the scanner cursor, below which every
    /// transfer and every factory event of a watched address is indexed. Anyone can flush a
    /// forwarder at any time, and only finalized events are indexed, so both sides describe the
    /// same finalized state; a mismatch means the ledger is wrong, and crediting on the chain
    /// stops until an operator lifts the freeze.
    async fn custody_for_route(
        &self,
        route: &RouteFile,
        _heads: &mut RoundHeads,
        findings: &mut Vec<Finding>,
    ) -> Result<(), ReconciliationError> {
        let chain_id = route.chain.chain_id;
        let token = route.asset.contract;
        let chain = Arc::clone(self.chain(chain_id)?);
        let Some(checkpoint) = db::chain_reads::checkpoint(&self.pool, chain_id).await? else {
            return Ok(());
        };
        let Some(coverage) = db::chain_reads::coverage(&self.pool, chain_id).await? else {
            return Ok(());
        };
        let block = checkpoint.number.min(coverage.number);
        let hash = if block == coverage.number {
            coverage.hash
        } else {
            checkpoint.hash
        };
        let check = format!("custody:{token:#x}");
        let chain_key = i64::try_from(chain_id)
            .map_err(|_| ReconciliationError::Invariant("chain overflow"))?;
        let cursor = store::work_cursor(&self.pool, &check, chain_key).await?;
        let (page, more) = db::scan_address_page(&self.pool, chain_id, cursor).await?;
        let last = page.last().map(|a| a.id);
        let mut ids = Vec::new();
        for address in &page {
            let caught: bool = sqlx::query_scalar::<_, Option<bool>>(
                "SELECT dual_covered_through=$2 FROM addresses WHERE id=$1",
            )
            .bind(address.id)
            .bind(
                i64::try_from(coverage.number)
                    .map_err(|_| ReconciliationError::Invariant("coverage overflow"))?,
            )
            .fetch_one(&self.pool)
            .await?
            .unwrap_or(false);
            if caught {
                ids.push(address.id);
            }
        }
        let ledgers = store::forwarder_ledgers(&self.pool, chain_id, token, block, &ids).await?;
        if ledgers.is_empty() {
            store::save_work_cursor(
                &self.pool,
                &check,
                chain_key,
                if more { last } else { None },
            )
            .await?;
            return Ok(());
        }
        let physical = ledgers
            .iter()
            .map(|ledger| ledger.address)
            .collect::<Vec<_>>();
        let verifier = self
            .verifiers
            .get(&chain_id)
            .ok_or(ReconciliationError::Invariant("custody verifier missing"))?;
        let (balances, verified) = tokio::try_join!(
            chain.token_balances_pinned(token, &physical, hash),
            verifier.token_balances_pinned(token, &physical, hash)
        )?;
        if balances.len() != ledgers.len() {
            return Err(ReconciliationError::Invariant(
                "balance response length did not match address count",
            ));
        }
        if verified != balances {
            return Err(ReconciliationError::Chain(
                "dual custody evidence disagreed".into(),
            ));
        }
        for (ledger, observed) in ledgers.iter().zip(balances) {
            if ledger.deposits.checked_sub(ledger.flushed) == Some(observed) {
                continue;
            }
            store::block_chain(
                &self.pool,
                chain_id,
                CheckName::CustodyBalance.code(),
                "a forwarder balance disagrees with its deposits minus its sweeps",
            )
            .await?;
            findings.push(Finding::new(
                CheckName::CustodyBalance,
                subjects([
                    ("chain_id", chain_id.to_string()),
                    ("address_id", ledger.address_id.to_string()),
                    ("token", format!("{token:#x}")),
                    ("block", block.to_string()),
                ]),
                json!({
                    "deposits_atomic": ledger.deposits.to_string(),
                    "flushed_atomic": ledger.flushed.to_string(),
                }),
                json!({"balance_atomic": observed.to_string()}),
                false,
                false,
            )?);
        }
        store::save_work_cursor(
            &self.pool,
            &check,
            chain_key,
            if more { last } else { None },
        )
        .await?;
        Ok(())
    }

    fn chain(&self, chain_id: u64) -> Result<&Arc<dyn ReconciliationChain>, ReconciliationError> {
        self.chains.get(&chain_id).ok_or_else(|| {
            ReconciliationError::Configuration(format!(
                "no reconciliation client for chain {chain_id}"
            ))
        })
    }

    fn route_index(&self) -> BTreeMap<(String, u64), &RouteFile> {
        self.routes
            .routes()
            .iter()
            .map(|route| ((route.route.clone(), route.version), route))
            .collect()
    }

    fn latest_asset_routes(&self) -> Vec<&RouteFile> {
        self.routes.current().collect()
    }

    /// Returns each chain's factory; every route of a chain must name the same one.
    fn chain_factories(&self) -> Result<Vec<(u64, Address)>, ReconciliationError> {
        let mut factories = BTreeMap::new();
        for route in self.routes.routes() {
            let factory = route.chain.contracts.forwarder_factory;
            match factories.insert(route.chain.chain_id, factory) {
                Some(existing) if existing != factory => {
                    return Err(ReconciliationError::Configuration(format!(
                        "routes disagree on the factory for chain {}",
                        route.chain.chain_id
                    )));
                }
                Some(_) | None => {}
            }
        }
        Ok(factories.into_iter().collect())
    }
}

fn log_finding(finding: &Finding, inserted: bool) {
    if !inserted {
        tracing::debug!(
            check = finding.check.code(),
            "reconciliation finding already recorded"
        );
    } else if finding.repair_applied {
        tracing::info!(
            check = finding.check.code(),
            subjects = ?finding.subjects,
            "reconciliation repair applied"
        );
    } else {
        tracing::warn!(
            tags.alert = "TopupReconciliationMismatch",
            tags.check = finding.check.code(),
            check = finding.check.code(),
            subjects = ?finding.subjects,
            expected = %finding.expected,
            observed = %finding.observed,
            "reconciliation mismatch"
        );
    }
}

fn subjects<const N: usize>(pairs: [(&str, String); N]) -> BTreeMap<String, String> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

#[cfg(test)]
mod scale_tests {
    use super::*;

    #[test]
    fn verified_derivations_use_bounded_lru_and_reverify_changed_rows() {
        let mut cache = VerifiedCache::default();
        let key = |id| {
            (
                1,
                Uuid::from_u128(id),
                alloy_primitives::B256::ZERO,
                Address::ZERO,
                Address::ZERO,
            )
        };
        cache.insert(key(0));
        for id in 1..200_000 {
            // Keep one old entry hot; eviction must retain it while retiring cold entries.
            assert!(cache.contains(&key(0)));
            cache.insert(key(id));
            assert!(cache.keys.len() <= 4_000);
            assert!(cache.recency.len() <= 4_000);
        }
        assert!(cache.contains(&key(0)));
        assert!(!cache.contains(&key(1)));
        let mut changed = key(0);
        changed.2 = alloy_primitives::B256::from([1; 32]);
        assert!(!cache.contains(&changed));
    }
}
