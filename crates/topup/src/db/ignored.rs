//! Exact finalized accounting, bounded diagnostic samples, and daily spam counts.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use topup_adapters::chain::evm::{RawTransfer, TransferRules};
use uuid::Uuid;

use super::{
    ScanAddress,
    types::{parse_address, parse_b256, to_i64},
};

/// Already-recorded groups must not be promoted to merchant deposits after route changes.
pub(crate) async fn historical_groups(
    pool: &PgPool,
    chain: u64,
    ids: &[Uuid],
    from: u64,
    to: u64,
) -> Result<BTreeSet<(B256, Address, Address)>, sqlx::Error> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT t.block_hash,t.token,a.address FROM ignored_transfer_totals t \
         JOIN addresses a ON a.id=t.address_id WHERE t.chain_id=$1 AND t.address_id=ANY($2) \
         AND t.block_number BETWEEN $3 AND $4",
    )
    .bind(to_i64(chain, "chain")?)
    .bind(ids)
    .bind(to_i64(from, "from")?)
    .bind(to_i64(to, "to")?)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(block, token, address)| {
            Ok((
                parse_b256(&block)?,
                parse_address(&token)?,
                parse_address(&address)?,
            ))
        })
        .collect()
}

/// Called under the coverage chain lock, in the same transaction as per-address progress.
/// A replay compares immutable group contents instead of adding the amount/count twice.
pub(crate) async fn record_in(
    tx: &mut Transaction<'_, Postgres>,
    chain: u64,
    logs: &BTreeMap<(B256, u64), RawTransfer>,
    addresses: &BTreeMap<Address, ScanAddress>,
    rules: &TransferRules,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let chain = to_i64(chain, "chain")?;
    let ids: Vec<_> = addresses.values().map(|a| a.id).collect();
    let min = logs.values().map(|l| l.block_number).min().unwrap_or(0);
    let max = logs.values().map(|l| l.block_number).max().unwrap_or(0);
    // Legacy/provisional deposit rows already own their amounts. Never count them twice,
    // reversed rows own no balance; their canonical ignored successors still need accounting.
    let existing: Vec<(String, i64)> = sqlx::query_as(
        "SELECT block_hash,log_index FROM deposits WHERE chain_id=$1 AND address_id=ANY($2) \
         AND block_number BETWEEN $3 AND $4 AND state <> 'reversed'",
    )
    .bind(chain)
    .bind(&ids)
    .bind(to_i64(min, "min")?)
    .bind(to_i64(max, "max")?)
    .fetch_all(&mut **tx)
    .await?;
    let existing: BTreeSet<_> = existing
        .into_iter()
        .map(|(hash, index)| {
            Ok((
                parse_b256(&hash)?,
                u64::try_from(index)
                    .map_err(|_| sqlx::Error::Protocol("negative log index".into()))?,
            ))
        })
        .collect::<Result<_, sqlx::Error>>()?;
    let mut groups = BTreeMap::<_, Vec<&RawTransfer>>::new();
    for (key, log) in logs {
        if existing.contains(key) {
            continue;
        }
        let address = addresses
            .get(&log.to)
            .ok_or_else(|| sqlx::Error::Protocol("untracked ignored recipient".into()))?;
        if log.block_number < address.created_block {
            continue;
        }
        groups
            .entry((address.id, log.token, log.block_hash, log.block_number))
            .or_default()
            .push(log);
    }
    for ((address_id, token, block_hash, block_number), logs) in groups {
        let token = format!("{token:#x}");
        let block_hash = format!("{block_hash:#x}");
        let count = i64::try_from(logs.len())
            .map_err(|_| sqlx::Error::Protocol("ignored count overflow".into()))?;
        // Numeric addition in PostgreSQL avoids narrowing a sum of multiple uint256 events.
        let amounts: Vec<_> = logs.iter().map(|l| l.amount.value().to_string()).collect();
        let encoded =
            serde_json::to_vec(&logs).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let digest = hex::encode(Sha256::digest(encoded));
        let id = Uuid::new_v4();
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO ignored_transfer_totals \
             (id,chain_id,address_id,token,block_number,block_hash,amount_atomic,event_count,evidence_hash,observed_day,created_at) \
             VALUES ($1,$2,$3,$4,$5,$6,(SELECT sum(v::numeric) FROM unnest($7::text[]) v),$8,$9,$10,$11) \
             ON CONFLICT (address_id,token,block_hash) DO NOTHING RETURNING id",
        ).bind(id).bind(chain).bind(address_id).bind(&token).bind(to_i64(block_number, "block")?)
            .bind(&block_hash).bind(amounts).bind(count).bind(&digest).bind(now.date_naive()).bind(now)
            .fetch_optional(&mut **tx).await?;
        if inserted.is_none() {
            let matches: bool = sqlx::query_scalar(
                "SELECT event_count=$4 AND evidence_hash=$5 AND block_number=$6 \
                 FROM ignored_transfer_totals WHERE address_id=$1 AND token=$2 AND block_hash=$3",
            )
            .bind(address_id)
            .bind(&token)
            .bind(&block_hash)
            .bind(count)
            .bind(digest)
            .bind(to_i64(block_number, "block")?)
            .fetch_one(&mut **tx)
            .await?;
            if !matches {
                return Err(sqlx::Error::Protocol(
                    "finalized ignored evidence changed".into(),
                ));
            }
            continue;
        }
        sqlx::query("INSERT INTO ignored_transfer_daily (chain_id,address_id,day,event_count) VALUES ($1,$2,$3,$4) \
            ON CONFLICT (chain_id,day,address_id) DO UPDATE SET event_count=ignored_transfer_daily.event_count+EXCLUDED.event_count")
            .bind(chain).bind(address_id).bind(now.date_naive()).bind(count).execute(&mut **tx).await?;
        let kept: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ignored_transfer_samples WHERE address_id=$1 AND token=$2 AND observed_day=$3",
        ).bind(address_id).bind(&token).bind(now.date_naive()).fetch_one(&mut **tx).await?;
        let remaining = usize::try_from(20_i64.saturating_sub(kept).max(0)).unwrap_or(0);
        for log in logs.into_iter().take(remaining) {
            sqlx::query("INSERT INTO ignored_transfer_samples \
                (total_id,chain_id,address_id,token,observed_day,block_hash,log_index,tx_hash,from_address,amount_atomic,reason,created_at) \
                VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10::text::numeric,$11,$12) ON CONFLICT DO NOTHING")
                .bind(id).bind(chain).bind(address_id).bind(&token).bind(now.date_naive()).bind(&block_hash)
                .bind(to_i64(log.log_index,"log_index")?).bind(format!("{:#x}",log.tx_hash))
                .bind(format!("{:#x}",log.from)).bind(log.amount.value().to_string())
                .bind(rules.ignored_reason(log).unwrap_or("previously_ignored")).bind(now)
                .execute(&mut **tx).await?;
        }
    }
    // Bound operational history and each cleanup transaction. Accounting totals are immutable.
    sqlx::query("DELETE FROM ignored_transfer_samples WHERE (chain_id,block_hash,log_index) IN \
        (SELECT chain_id,block_hash,log_index FROM ignored_transfer_samples WHERE created_at < $1 - interval '30 days' LIMIT 1000)")
        .bind(now).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM ignored_transfer_daily WHERE (chain_id,day,address_id) IN \
        (SELECT chain_id,day,address_id FROM ignored_transfer_daily WHERE day < $1::date - 30 LIMIT 1000)")
        .bind(now.date_naive()).execute(&mut **tx).await?;
    Ok(())
}
