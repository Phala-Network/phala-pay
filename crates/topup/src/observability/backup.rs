use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use super::reporting::CronMonitor;

/// Independent base-backup and WAL recovery evidence on the shared volume.
const BACKUP_MARKER_DIRECTORY: &str = "/run/topup-observability";
const CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// Sampling, segment switches, and uploads must fit inside the one-minute recovery target.
const BACKUP_MAX_AGE_S: u64 = 60;
const BASE_MAX_AGE_S: u64 = 48 * 3600;
const MAX_WAL_GAP_BYTES: u64 = 16 * 1024 * 1024;

/// Requires a recent base backup and fresh WAL progress, independently of PostgreSQL.
pub async fn monitor_backup(cancellation: CancellationToken) {
    let directory = Path::new(BACKUP_MARKER_DIRECTORY);
    let monitor = CronMonitor::backup();
    loop {
        super::capacity::observe().await;
        monitor.check_in(backup_healthy(directory, unix_now()));
        tokio::select! {
            () = cancellation.cancelled() => return,
            () = sleep(CHECK_INTERVAL) => {}
        }
    }
}

fn backup_timestamp(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn backup_healthy(directory: &Path, now: u64) -> bool {
    let progress = std::fs::read_to_string(directory.join("wal-progress")).unwrap_or_default();
    let values = progress
        .split_whitespace()
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>();
    let Ok(values) = values else { return false };
    let [checked_at, backlog_age, gap] = values.as_slice() else {
        return false;
    };
    recovery_healthy(
        backup_timestamp(&directory.join("last-base-backup-unix-seconds")),
        backup_timestamp(&directory.join("last-backup-unix-seconds")),
        *checked_at,
        *backlog_age,
        *gap,
        now,
    )
}

fn recovery_healthy(
    base: Option<u64>,
    wal: Option<u64>,
    checked_at: u64,
    backlog_age: u64,
    gap: u64,
    now: u64,
) -> bool {
    let fresh = |timestamp: Option<u64>, max_age| {
        timestamp.is_some_and(|timestamp| {
            timestamp > 0 && timestamp <= now && now - timestamp <= max_age
        })
    };
    fresh(base, BASE_MAX_AGE_S)
        && fresh(wal, BACKUP_MAX_AGE_S)
        && fresh(Some(checked_at), 45)
        && backlog_age <= BACKUP_MAX_AGE_S
        && gap <= MAX_WAL_GAP_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_requires_base_backup_and_current_recovery_progress() {
        let healthy = |base, wal, checked, backlog, gap| {
            recovery_healthy(base, wal, checked, backlog, gap, 200000)
        };
        assert!(
            !healthy(None, Some(200000), 200000, 0, 0),
            "WAL alone is not restorable"
        );
        assert!(healthy(Some(199900), Some(200000), 200000, 0, 0));
        assert!(
            !healthy(Some(199900), Some(199800), 200000, 0, 0),
            "old backlog upload is stale"
        );
        assert!(!healthy(Some(199900), Some(200000), 200000, 61, 0));
        assert!(!healthy(Some(199900), Some(200000), 200000, 0, 16777217));
        assert!(
            !healthy(Some(199900), Some(200000), 199900, 0, 0),
            "stopped monitor fails closed"
        );
        assert!(
            !healthy(Some(1), Some(200000), 200000, 0, 0),
            "old base backup fails"
        );
        assert!(
            !healthy(Some(200001), Some(200000), 200000, 0, 0),
            "future timestamps fail closed"
        );
    }
}
