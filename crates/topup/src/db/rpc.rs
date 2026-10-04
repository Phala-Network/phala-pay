//! Durable RPC safety state and atomic complete-window commits.
use super::{FactoryCommit, NewDeposit, ScanCommit, types::to_i64};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use topup_adapters::chain::evm::{
    FactoryLog,
    group::{Failure, HeadAnchor, WatermarkStore},
    window::{WindowProof, WindowRequest},
};
use uuid::Uuid;

/// Store bound to the accepted public configuration digest.
pub struct RpcState {
    /// Runtime application pool.
    pub pool: PgPool,
    /// Digest of public configuration, never secrets.
    pub config_digest: String,
}
#[async_trait]
impl WatermarkStore for RpcState {
    async fn blocked(&self, chain: u64) -> Result<(), Failure> {
        let row =
            sqlx::query("SELECT frozen,awaiting_anchor FROM rpc_chain_state WHERE chain_id=$1")
                .bind(i64::try_from(chain).map_err(|_| Failure::Persistence)?)
                .fetch_optional(&self.pool)
                .await
                .map_err(|_| Failure::Persistence)?;
        if let Some(row) = row {
            if row
                .try_get::<bool, _>("frozen")
                .map_err(|_| Failure::Persistence)?
            {
                return Err(Failure::Fork);
            }
            if row
                .try_get::<bool, _>("awaiting_anchor")
                .map_err(|_| Failure::Persistence)?
            {
                return Err(Failure::Unavailable);
            }
        }
        Ok(())
    }
    async fn freeze(&self, chain: u64) -> Result<(), Failure> {
        freeze(&self.pool, chain, "finalized hash conflict")
            .await
            .map_err(|_| Failure::Persistence)
    }
    async fn reorg(&self, chain: u64, group: &str, from: u64, to: u64) -> Result<(), Failure> {
        sqlx::query("INSERT INTO rpc_reorg_ranges(chain_id,group_id,epoch,from_block,to_block) VALUES($1,$2,COALESCE((SELECT epoch FROM rpc_chain_state WHERE chain_id=$1),0),$3,$4)")
            .bind(i64::try_from(chain).map_err(|_|Failure::Persistence)?).bind(group)
            .bind(i64::try_from(from).map_err(|_|Failure::Persistence)?).bind(i64::try_from(to).map_err(|_|Failure::Persistence)?)
            .execute(&self.pool).await.map_err(|_|Failure::Persistence)?;
        Ok(())
    }
    async fn load(
        &self,
        chain: u64,
        group: &str,
        tag: &str,
    ) -> Result<Option<HeadAnchor>, Failure> {
        let chain = i64::try_from(chain).map_err(|_| Failure::Persistence)?;
        let frozen: bool = sqlx::query_scalar(
            "SELECT COALESCE((SELECT frozen FROM rpc_chain_state WHERE chain_id=$1),false)",
        )
        .bind(chain)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| Failure::Persistence)?;
        if frozen {
            return Err(Failure::Fork);
        }
        let row=sqlx::query("SELECT number,hash,parent_hash FROM rpc_watermarks WHERE chain_id=$1 AND group_id=$2 AND tag=$3 AND epoch=COALESCE((SELECT epoch FROM rpc_chain_state WHERE chain_id=$1),0)")
            .bind(chain).bind(group).bind(tag).fetch_optional(&self.pool).await.map_err(|_|Failure::Persistence)?;
        row.map(|r| {
            Ok(HeadAnchor {
                number: u64::try_from(
                    r.try_get::<i64, _>("number")
                        .map_err(|_| Failure::Persistence)?,
                )
                .map_err(|_| Failure::Persistence)?,
                hash: r.try_get("hash").map_err(|_| Failure::Persistence)?,
                parent_hash: r.try_get("parent_hash").map_err(|_| Failure::Persistence)?,
            })
        })
        .transpose()
    }
    async fn accept(
        &self,
        chain: u64,
        group: &str,
        tag: &str,
        member: &str,
        head: &HeadAnchor,
    ) -> Result<(), Failure> {
        let chain = i64::try_from(chain).map_err(|_| Failure::Persistence)?;
        let number = i64::try_from(head.number).map_err(|_| Failure::Persistence)?;
        let mut tx = self.pool.begin().await.map_err(|_| Failure::Persistence)?;
        guard_state(
            &mut tx,
            u64::try_from(chain).map_err(|_| Failure::Persistence)?,
            tag != "cursor",
        )
        .await
        .map_err(|_| Failure::Fork)?;
        let changed=sqlx::query("INSERT INTO rpc_watermarks(chain_id,group_id,tag,number,hash,parent_hash,member_id,config_digest,epoch) VALUES($1,$2,$3,$4,$5,$6,$7,$8,COALESCE((SELECT epoch FROM rpc_chain_state WHERE chain_id=$1),0)) ON CONFLICT(chain_id,group_id,tag,epoch) DO UPDATE SET number=EXCLUDED.number,hash=EXCLUDED.hash,parent_hash=EXCLUDED.parent_hash,member_id=EXCLUDED.member_id,config_digest=EXCLUDED.config_digest,accepted_at=now() WHERE rpc_watermarks.number<=EXCLUDED.number AND ($3<>'finalized' OR rpc_watermarks.number<>EXCLUDED.number OR rpc_watermarks.hash=EXCLUDED.hash)")
            .bind(chain).bind(group).bind(tag).bind(number).bind(&head.hash).bind(&head.parent_hash).bind(member).bind(&self.config_digest).execute(&mut *tx).await.map_err(|_|Failure::Persistence)?.rows_affected();
        if changed == 0 {
            return Err(Failure::Stale);
        }
        tx.commit().await.map_err(|_| Failure::Persistence)?;
        Ok(())
    }
}
/// Canonical digest of public configuration or immutable window selectors.
pub fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
/// Frozen safety state is checked by crediting and all cursor writers.
pub async fn freeze(pool: &PgPool, chain: u64, reason: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO rpc_chain_state(chain_id,frozen,reason) VALUES($1,true,$2) ON CONFLICT(chain_id) DO UPDATE SET frozen=true,reason=EXCLUDED.reason")
        .bind(to_i64(chain,"rpc chain")?).bind(reason).execute(pool).await?;
    tracing::warn!(
        tags.alert = "TopupRpcChainFrozen",
        tags.chain = chain,
        "RPC chain frozen; audited recovery required"
    );
    Ok(())
}
/// Persisted provenance written in the same transaction as deposits and progress.
pub async fn coverage_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain: u64,
    proof: &WindowProof,
) -> Result<(), sqlx::Error> {
    let mut request = proof.request.clone();
    request.exclude_member = None;
    let value = serde_json::to_value(&request).map_err(|e| sqlx::Error::Encode(e.into()))?;
    let hash = digest(&value.to_string());
    sqlx::query("INSERT INTO rpc_window_reviews(chain_id,group_id,from_block,to_block,request,request_digest,answering_member,end_hash,epoch) VALUES($1,$2,$3,$4,$5,$6,$7,$8,COALESCE((SELECT epoch FROM rpc_chain_state WHERE chain_id=$1),0)) ON CONFLICT(chain_id,group_id,request_digest,epoch) DO NOTHING")
        .bind(to_i64(chain,"review chain")?).bind(&proof.group).bind(to_i64(request.from,"review from")?).bind(to_i64(request.to,"review to")?).bind(value).bind(hash).bind(&proof.member).bind(&proof.end_hash).execute(&mut **tx).await?;
    Ok(())
}
/// Progress to commit only after all reads in the numeric window succeed.
#[derive(Default)]
pub struct WindowProgress {
    /// Finalized scanner cursor and its known block time.
    pub scanned: Option<(u64, Option<DateTime<Utc>>)>,
    /// Completed address backfills.
    pub backfilled: Vec<Uuid>,
    /// Partial address backfill progress.
    pub through: Option<(Vec<Uuid>, u64)>,
    /// Reconciler compare-and-swap cursor (old, next).
    pub reconciliation: Option<(Option<u64>, u64)>,
    /// Independently reviewed durable coverage id.
    pub reviewed: Option<Uuid>,
}
/// Serializes progress commits against durable freeze/recovery using the chain state row.
pub async fn guard_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain: u64,
) -> Result<(), sqlx::Error> {
    guard_state(tx, chain, true).await
}
async fn guard_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    chain: u64,
    requires_anchor: bool,
) -> Result<(), sqlx::Error> {
    let chain = to_i64(chain, "RPC chain")?;
    sqlx::query("INSERT INTO rpc_chain_state(chain_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(chain)
        .execute(&mut **tx)
        .await?;
    let row = sqlx::query(
        "SELECT frozen,awaiting_anchor FROM rpc_chain_state WHERE chain_id=$1 FOR UPDATE",
    )
    .bind(chain)
    .fetch_one(&mut **tx)
    .await?;
    if row.try_get::<bool, _>("frozen")?
        || (requires_anchor && row.try_get::<bool, _>("awaiting_anchor")?)
    {
        return Err(sqlx::Error::Protocol(
            "RPC chain frozen or awaiting anchor".to_owned(),
        ));
    }
    Ok(())
}
/// Atomically commits all complete-window evidence and coverage with its derived progress.
pub async fn commit_window(
    pool: &PgPool,
    chain: u64,
    deposits: &[NewDeposit],
    factory: &[FactoryLog],
    proof: Option<&WindowProof>,
    progress: WindowProgress,
) -> Result<(ScanCommit, FactoryCommit), sqlx::Error> {
    let mut tx = pool.begin().await?;
    guard_in(&mut tx, chain).await?;
    let (scanned, time) = progress.scanned.map_or((None, None), |(n, t)| (Some(n), t));
    let scan = super::scanner::commit_scan_in(
        &mut tx,
        chain,
        deposits,
        &progress.backfilled,
        scanned,
        time,
    )
    .await?;
    let events = super::sweeps::commit_factory_logs_in(&mut tx, chain, factory).await?;
    if let Some(proof) = proof {
        coverage_in(&mut tx, chain, proof).await?;
    }
    if let Some((ids, through)) = progress.through {
        sqlx::query("UPDATE addresses SET backfilled_through=GREATEST(COALESCE(backfilled_through,0),$2) WHERE id=ANY($1) AND NOT backfilled")
            .bind(ids).bind(to_i64(through,"backfill through")?).execute(&mut *tx).await?;
    }
    if let Some((old, next)) = progress.reconciliation {
        let old = old
            .map(|n| to_i64(n, "reconciliation cursor"))
            .transpose()?;
        let n=sqlx::query("INSERT INTO reconciliation_deposit_cursors(chain_id,next_block) VALUES($1,$2) ON CONFLICT(chain_id) DO UPDATE SET next_block=EXCLUDED.next_block WHERE reconciliation_deposit_cursors.next_block IS NOT DISTINCT FROM $3")
            .bind(to_i64(chain,"cursor chain")?).bind(to_i64(next,"cursor next")?).bind(old).execute(&mut *tx).await?.rows_affected();
        if n == 0 {
            return Err(sqlx::Error::Protocol(
                "reconciliation cursor changed".to_owned(),
            ));
        }
    }
    if let (Some(id), Some(proof)) = (progress.reviewed, proof) {
        let old =
            sqlx::query("SELECT end_hash,request FROM rpc_window_reviews WHERE id=$1 AND epoch=COALESCE((SELECT epoch FROM rpc_chain_state WHERE chain_id=$2),0) FOR UPDATE")
                .bind(id)
                .bind(to_i64(chain,"review chain")?)
                .fetch_one(&mut *tx)
                .await?;
        let old_request: WindowRequest = serde_json::from_value(old.try_get("request")?)
            .map_err(|e| sqlx::Error::Decode(e.into()))?;
        if old_request.finalized && old.try_get::<String, _>("end_hash")? != proof.end_hash {
            tx.rollback().await?;
            freeze(pool, chain, "historical finalized window hash conflict").await?;
            return Err(sqlx::Error::Protocol(
                "historical finalized window hash conflict".to_owned(),
            ));
        }

        sqlx::query("UPDATE rpc_window_reviews SET replayed_at=now() WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE rpc_window_reviews SET reviewed_at=now(),reviewed_by=$2 WHERE id=$1 AND answering_member<>$2")
            .bind(id).bind(&proof.member).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok((scan, events))
}
/// Old coverage stays reviewable regardless of the moving finalized tail.
pub async fn due_reviews(
    pool: &PgPool,
    chain: u64,
) -> Result<Vec<(Uuid, WindowRequest, String)>, sqlx::Error> {
    let rows=sqlx::query("SELECT id,request,answering_member FROM rpc_window_reviews WHERE chain_id=$1 AND epoch=COALESCE((SELECT epoch FROM rpc_chain_state WHERE chain_id=$1),0) AND reviewed_at IS NULL ORDER BY COALESCE(replayed_at,created_at),from_block,id LIMIT 16")
        .bind(to_i64(chain,"review chain")?).fetch_all(pool).await?;
    rows.into_iter()
        .map(|r| {
            Ok((
                r.try_get("id")?,
                serde_json::from_value(r.try_get("request")?)
                    .map_err(|e| sqlx::Error::Decode(e.into()))?,
                r.try_get("answering_member")?,
            ))
        })
        .collect()
}
/// Audited recovery preserves old evidence and repairs every cursor-derived address floor.
pub async fn recover(
    pool: &PgPool,
    chain: u64,
    anchor: &HeadAnchor,
    actor: &str,
    reason: &str,
) -> Result<(), sqlx::Error> {
    if reason.trim().is_empty()
        || actor.trim().is_empty()
        || reason.len() > 1024
        || actor.len() > 128
    {
        return Err(sqlx::Error::Protocol(
            "RPC recovery reason required".to_owned(),
        ));
    }
    let chain = to_i64(chain, "recovery chain")?;
    let number = to_i64(anchor.number, "recovery anchor")?;
    let mut tx = pool.begin().await?;
    let epoch:i64=sqlx::query_scalar("UPDATE rpc_chain_state SET epoch=epoch+1,frozen=true,recovery_pending=true,reason=$2 WHERE chain_id=$1 AND NOT recovery_pending RETURNING epoch").bind(chain).bind(reason).fetch_one(&mut *tx).await?;
    let before = sqlx::query(
        "SELECT id,created_block,backfilled,backfilled_through FROM addresses WHERE chain_id=$1",
    )
    .bind(chain)
    .fetch_all(&mut *tx)
    .await?;
    let originals:serde_json::Value=sqlx::query_scalar("SELECT jsonb_build_object('cursors',(SELECT jsonb_agg(to_jsonb(c)) FROM cursors c WHERE chain_id=$1),'watermarks',(SELECT jsonb_agg(to_jsonb(w)) FROM rpc_watermarks w WHERE chain_id=$1),'reconciliation',(SELECT jsonb_agg(to_jsonb(r)) FROM reconciliation_deposit_cursors r WHERE chain_id=$1),'pending',(SELECT jsonb_agg(to_jsonb(p)) FROM pending_transfers p WHERE chain_id=$1))").bind(chain).fetch_one(&mut *tx).await?;
    let evidence=before.iter().map(|r|Ok(json!({"id":r.try_get::<Uuid,_>("id")?,"created_block":r.try_get::<i64,_>("created_block")?,"backfilled":r.try_get::<bool,_>("backfilled")?,"backfilled_through":r.try_get::<Option<i64>,_>("backfilled_through")?}))).collect::<Result<Vec<_>,sqlx::Error>>()?;
    sqlx::query(
        "INSERT INTO rpc_recoveries(chain_id,epoch,evidence,actor,reason) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(chain)
    .bind(epoch)
    .bind(json!({"anchor":anchor,"addresses":evidence,"progress":originals}))
    .bind(actor)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    // Rebase every suspect issuance to genesis conservatively: a height-only cursor is not proof
    // that an address's original creation height was correct. Preserve originals in the audit.
    sqlx::query("UPDATE addresses SET created_block=0,backfilled=false,backfilled_through=NULL WHERE chain_id=$1").bind(chain).execute(&mut *tx).await?;
    sqlx::query("UPDATE cursors SET scanned_block=$2,scanned_block_time=NULL,confirmed_block=NULL WHERE chain_id=$1")
        .bind(chain)
        .bind(number)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM pending_transfers WHERE chain_id=$1")
        .bind(chain)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM reconciliation_deposit_cursors WHERE chain_id=$1")
        .bind(chain)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
/// Shared persistence handle for adapter clients.
pub fn state(pool: &PgPool, config_digest: String) -> Arc<RpcState> {
    Arc::new(RpcState {
        pool: pool.clone(),
        config_digest,
    })
}

/// Head evidence and display rows commit together with the confirmed cursor and coverage.
pub async fn commit_head_window(
    pool: &PgPool,
    chain: u64,
    range: (u64, u64),
    deposits: &[NewDeposit],
    horizon: Option<u64>,
    pending: &[super::NewPendingTransfer],
    proof: Option<&WindowProof>,
) -> Result<(ScanCommit, super::HeadCommit), sqlx::Error> {
    commit_head_page(
        pool, chain, range, deposits, horizon, pending, proof, None, true,
    )
    .await
}

/// Commits one address page; only the last page advances chain-wide confirmation/reorg progress.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn commit_head_page(
    pool: &PgPool,
    chain: u64,
    range: (u64, u64),
    deposits: &[NewDeposit],
    horizon: Option<u64>,
    pending: &[super::NewPendingTransfer],
    proof: Option<&WindowProof>,
    addresses: Option<&[Uuid]>,
    complete: bool,
) -> Result<(ScanCommit, super::HeadCommit), sqlx::Error> {
    let mut tx = pool.begin().await?;
    guard_in(&mut tx, chain).await?;
    let scan = if let Some(horizon) = horizon {
        super::scanner::commit_confirmed_scan_in(
            &mut tx,
            chain,
            deposits,
            if complete { horizon } else { 0 },
        )
        .await?
    } else {
        ScanCommit {
            inserted: 0,
            unsupported_inserted: 0,
            inserted_positions: Vec::new(),
        }
    };
    let head =
        super::pending::commit_head_page_in(&mut tx, chain, range.0, range.1, pending, addresses)
            .await?;
    if let Some(proof) = proof {
        coverage_in(&mut tx, chain, proof).await?;
    }
    if complete {
        sqlx::query(
            "UPDATE rpc_reorg_ranges SET replayed_through=LEAST(to_block,$2) \
             WHERE chain_id=$1 \
             AND epoch=COALESCE((SELECT epoch \
             FROM rpc_chain_state \
             WHERE chain_id=$1),0) \
             AND COALESCE(replayed_through+1,from_block)>=$3 \
             AND COALESCE(replayed_through+1,from_block)<=$2 \
             AND COALESCE(replayed_through,from_block-1)<to_block",
        )
        .bind(to_i64(chain, "reorg chain")?)
        .bind(to_i64(range.1, "reorg through")?)
        .bind(to_i64(range.0, "reorg from")?)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok((scan, head))
}

static HEALTH_METRICS: std::sync::Mutex<Vec<prometheus::proto::MetricFamily>> =
    std::sync::Mutex::new(Vec::new());
/// Last successful database safety/replay gauges; a failed refresh preserves the snapshot.
pub fn metrics() -> Vec<prometheus::proto::MetricFamily> {
    HEALTH_METRICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
/// Refreshes durable gauges without database I/O in the metrics HTTP handler.
pub async fn refresh_metrics(pool: &PgPool) -> Result<(), sqlx::Error> {
    use prometheus::{IntGaugeVec, Opts, core::Collector};
    let gauge = |name, help, labels: &[&str]| {
        IntGaugeVec::new(Opts::new(name, help), labels)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))
    };
    let frozen = gauge(
        "topup_rpc_chain_frozen",
        "Durable fork safety freeze.",
        &["chain_id"],
    )?;
    let awaiting = gauge(
        "topup_rpc_chain_awaiting_anchor",
        "Chain waiting for A/B cursor hash agreement.",
        &["chain_id"],
    )?;
    let epoch = gauge(
        "topup_rpc_chain_epoch",
        "Audited recovery epoch.",
        &["chain_id"],
    )?;
    for row in sqlx::query("SELECT chain_id, frozen, awaiting_anchor, epoch FROM rpc_chain_state")
        .fetch_all(pool)
        .await?
    {
        let chain = row.try_get::<i64, _>("chain_id")?.to_string();
        frozen
            .with_label_values(&[&chain])
            .set(i64::from(row.try_get::<bool, _>("frozen")?));
        awaiting
            .with_label_values(&[&chain])
            .set(i64::from(row.try_get::<bool, _>("awaiting_anchor")?));
        epoch
            .with_label_values(&[&chain])
            .set(row.try_get("epoch")?);
    }
    let heads = gauge(
        "topup_rpc_group_head",
        "Accepted persisted head height.",
        &["chain_id", "group", "tag", "epoch"],
    )?;
    let rows = sqlx::query(r#"
        SELECT chain_id, group_id, tag, number, epoch
        FROM rpc_watermarks w
        WHERE epoch = COALESCE((SELECT epoch FROM rpc_chain_state s WHERE s.chain_id = w.chain_id), 0)
        "#).fetch_all(pool).await?;
    for row in rows {
        let chain = row.try_get::<i64, _>("chain_id")?.to_string();
        let group: String = row.try_get("group_id")?;
        let tag: String = row.try_get("tag")?;
        let epoch = row.try_get::<i64, _>("epoch")?.to_string();
        heads
            .with_label_values(&[&chain, &group, &tag, &epoch])
            .set(row.try_get("number")?);
    }
    let review = gauge(
        "topup_rpc_review_pending_windows",
        "Historical windows requiring independent review.",
        &["chain_id"],
    )?;
    let reorg = gauge(
        "topup_rpc_reorg_pending_ranges",
        "Nonfinal reorg ranges awaiting complete replay.",
        &["chain_id"],
    )?;
    // Include zero rows after recovery so an on-demand reader can distinguish clear from absent.
    let rows = sqlx::query(r#"
        SELECT s.chain_id,
            (SELECT count(*) FROM rpc_window_reviews w
             WHERE w.chain_id = s.chain_id AND w.epoch = s.epoch AND w.reviewed_at IS NULL) AS reviews,
            (SELECT count(*) FROM rpc_reorg_ranges r
             WHERE r.chain_id = s.chain_id AND r.epoch = s.epoch
               AND COALESCE(r.replayed_through, r.from_block - 1) < r.to_block) AS reorgs
        FROM rpc_chain_state s
        "#).fetch_all(pool).await?;
    for row in rows {
        let chain = row.try_get::<i64, _>("chain_id")?.to_string();
        review
            .with_label_values(&[&chain])
            .set(row.try_get("reviews")?);
        reorg
            .with_label_values(&[&chain])
            .set(row.try_get("reorgs")?);
    }
    let refreshed = prometheus::IntGauge::new(
        "topup_rpc_metrics_refreshed_at_seconds",
        "Unix time of last successful durable metrics refresh.",
    )
    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    refreshed.set(Utc::now().timestamp());
    let out = frozen
        .collect()
        .into_iter()
        .chain(awaiting.collect())
        .chain(epoch.collect())
        .chain(heads.collect())
        .chain(review.collect())
        .chain(reorg.collect())
        .chain(refreshed.collect())
        .collect();
    *HEALTH_METRICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = out;
    Ok(())
}

/// Owner-only complete-window replay while the chain remains frozen. The caller holds the
/// exclusive lease-owner lock, so no runtime worker or API issues progress during recovery.
pub(crate) async fn recovery_window(
    pool: &PgPool,
    chain: u64,
    deposits: &[NewDeposit],
    factory: &[FactoryLog],
    proof: &WindowProof,
    ids: &[Uuid],
    through: u64,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let frozen: bool =
        sqlx::query_scalar("SELECT frozen FROM rpc_chain_state WHERE chain_id=$1 FOR UPDATE")
            .bind(to_i64(chain, "recovery chain")?)
            .fetch_one(&mut *tx)
            .await?;
    if !frozen {
        return Err(sqlx::Error::Protocol(
            "recovery replay requires a frozen chain".to_owned(),
        ));
    }
    super::scanner::commit_scan_in(&mut tx, chain, deposits, &[], None, None).await?;
    super::sweeps::commit_factory_logs_in(&mut tx, chain, factory).await?;
    coverage_in(&mut tx, chain, proof).await?;
    sqlx::query("UPDATE addresses SET backfilled_through=$2 WHERE id=ANY($1)")
        .bind(ids)
        .bind(to_i64(through, "recovery through")?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
