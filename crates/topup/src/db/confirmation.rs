//! Durable confirmation read admission shared by the pump and finality watcher.
use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, Row};
use topup_core::route::Confirmations;
use uuid::Uuid;

/// Fixed normal Depth wake-up offsets after the estimated depth anchor.
pub const DEPTH_OFFSETS: [i64; 6] = [0, 4, 12, 28, 60, 124];

/// Fixed normal probe positions; the first position always runs immediately.
#[must_use]
pub fn offsets(policy: Confirmations) -> Vec<i64> {
    match policy {
        Confirmations::Depth(_) => DEPTH_OFFSETS.to_vec(),
        Confirmations::Safe => vec![0, 384, 768],
        Confirmations::Finalized => (0..7).map(|i| i * 384).collect(),
    }
}

/// The worker admitted to a chain-evidence due time.
#[derive(Clone, Copy, Debug)]
pub enum Reader {
    /// A normal or slow-lane head probe; receipt admission happens within this same lease.
    Confirm(Confirmations),
    /// An unresolved recheck or the first post-confirmation finality check.
    Watcher,
}

/// Read ownership and budget reserved before network calls.
#[derive(Clone, Copy, Debug)]
pub struct ReadClaim {
    /// Current record version; read-result writes must compare it and the lease token.
    pub version: DateTime<Utc>,
    /// Position already consumed from the normal schedule (one based).
    pub head_checks: i32,
    /// Fixed normal deadline, once the first valid pair of heads has completed.
    pub deadline: Option<DateTime<Utc>>,
    /// Immutable slow-lane entry, when this is a slow head probe.
    pub first_slow_at: Option<DateTime<Utc>>,
    /// Whether this claim reserved a head read; a crashed normal probe can instead enter S.
    pub probe: bool,
}

/// Acquires the same lease and due-time CAS for either worker. A pump can already own its
/// processing lease; advancing the shared due still prevents it from reading twice. No chain
/// lock or database transaction remains held while a caller performs RPC.
pub async fn claim_read(
    pool: &PgPool,
    id: Uuid,
    token: Uuid,
    reader: Reader,
    now: DateTime<Utc>,
) -> Result<Option<ReadClaim>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT state,deposit_finality_pending(deposits) AS watcher_eligible,updated_at,confirm_head_checks,confirm_receipt_checks, \
         confirm_deadline_at,first_slow_at,first_unresolved_at FROM deposits \
         WHERE id=$1 AND (lease_until IS NULL OR lease_until <= $3 OR lease_token=$2) \
         AND (finality_check_at IS NULL OR finality_check_at <= $3) FOR UPDATE SKIP LOCKED",
    )
    .bind(id)
    .bind(token)
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let state: String = row.try_get("state")?;
    let watcher_eligible: bool = row.try_get("watcher_eligible")?;
    let mut heads: i32 = row.try_get("confirm_head_checks")?;
    let receipts: i32 = row.try_get("confirm_receipt_checks")?;
    let deadline: Option<DateTime<Utc>> = row.try_get("confirm_deadline_at")?;
    let first_slow: Option<DateTime<Utc>> = row.try_get("first_slow_at")?;
    let unresolved: Option<DateTime<Utc>> = row.try_get("first_unresolved_at")?;
    let version: DateTime<Utc> = row.try_get("updated_at")?;
    let mut probe = false;
    let mut interrupted = false;
    let next_due = match reader {
        Reader::Confirm(policy) if state == "detected" && unresolved.is_none() && receipts == 0 => {
            if first_slow.is_some() {
                probe = true;
                None
            } else {
                let times = offsets(policy);
                let limit = i32::try_from(times.len()).expect("bounded normal schedule");
                if heads >= limit || (heads > 0 && deadline.is_none()) {
                    // The previous admitted read never committed a valid height-only result.
                    // Its consumed position is retained; uncertainty belongs to S, not L.
                    interrupted = true;
                    None
                } else {
                    let position = match deadline {
                        Some(end) => {
                            let anchor = end - Duration::seconds(times[times.len() - 1]);
                            times
                                .iter()
                                .enumerate()
                                .skip(1)
                                .filter(|(_, offset)| anchor + Duration::seconds(**offset) <= now)
                                .map(|(i, _)| i)
                                .max()
                                .unwrap_or(1)
                                .max(usize::try_from(heads).expect("nonnegative normal counter"))
                        }
                        None => 0,
                    };
                    heads = i32::try_from(position + 1).expect("bounded normal position");
                    probe = true;
                    deadline.and_then(|end| {
                        times.get(position + 1).map(|offset| {
                            end - Duration::seconds(times[times.len() - 1])
                                + Duration::seconds(*offset)
                        })
                    })
                }
            }
        }
        Reader::Watcher if watcher_eligible => None,
        _ => return Ok(None),
    };
    sqlx::query(
        "UPDATE deposits SET lease_token=$2,lease_until=$3 + interval '5 minutes', \
         confirm_head_checks=$4, \
         first_unresolved_at=CASE WHEN $6 THEN COALESCE(first_unresolved_at,$3) ELSE first_unresolved_at END, \
         finality_due_at=CASE WHEN $7 THEN COALESCE(finality_due_at,$3) ELSE finality_due_at END, \
         finality_check_at=COALESCE($5,finality_next_check_at( \
             COALESCE(first_unresolved_at,first_slow_at,finality_due_at,$3),$3)) \
         , next_attempt_at=CASE WHEN state='detected' AND $7 THEN \
             finality_next_check_at(COALESCE(first_unresolved_at,finality_due_at,$3),$3) ELSE next_attempt_at END \
         WHERE id=$1",
    )
    .bind(id).bind(token).bind(now).bind(heads).bind(next_due).bind(interrupted)
    .bind(matches!(reader, Reader::Watcher)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(ReadClaim {
        version,
        head_checks: heads,
        deadline,
        first_slow_at: first_slow,
        probe,
    }))
}

/// Reserves the sole full confirmation read before sending any receipt RPC. It shares the
/// head claim's token and record version, and is never refunded after failure or a crash.
pub async fn reserve_receipt(
    pool: &PgPool,
    id: Uuid,
    token: Uuid,
    version: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "UPDATE deposits SET confirm_receipt_checks=confirm_receipt_checks+1 \
         WHERE id=$1 AND lease_token=$2 AND updated_at=$3 AND state='detected' \
         AND confirm_receipt_checks=0",
    )
    .bind(id)
    .bind(token)
    .bind(version)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Commits the first normal deadline after its heads finish, or enters L after the last
/// successful height-only probe. A slow probe retains its already-reserved segmented due.
#[allow(clippy::too_many_arguments)]
pub async fn finish_probe(
    pool: &PgPool,
    id: Uuid,
    token: Uuid,
    claim: ReadClaim,
    policy: Confirmations,
    anchor: DateTime<Utc>,
    ready: bool,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let times = offsets(policy);
    let last = times[times.len() - 1];
    let end = claim.deadline.unwrap_or(anchor + Duration::seconds(last));
    let position = usize::try_from(claim.head_checks).expect("reserved nonnegative counter");
    let enter_slow = !ready && claim.first_slow_at.is_none() && position >= times.len();
    let next = if claim.first_slow_at.is_some() {
        None
    } else if enter_slow || ready {
        Some(now + Duration::seconds(60))
    } else {
        times
            .get(position)
            .map(|offset| end - Duration::seconds(last) + Duration::seconds(*offset))
    };
    Ok(sqlx::query(
        "UPDATE deposits SET confirm_deadline_at=COALESCE(confirm_deadline_at,$4), \
         first_slow_at=CASE WHEN $5 THEN COALESCE(first_slow_at,$6) ELSE first_slow_at END, \
         finality_check_at=COALESCE($7,finality_check_at) \
         WHERE id=$1 AND lease_token=$2 AND updated_at=$3 AND state='detected'",
    )
    .bind(id)
    .bind(token)
    .bind(claim.version)
    .bind(end)
    .bind(enter_slow)
    .bind(now)
    .bind(next)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Releases chain-read ownership while preserving the already reserved due time.
pub async fn release_read(pool: &PgPool, id: Uuid, token: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE deposits SET lease_token=NULL,lease_until=NULL WHERE id=$1 AND lease_token=$2",
    )
    .bind(id)
    .bind(token)
    .execute(pool)
    .await?;
    Ok(())
}
