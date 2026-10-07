use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use topup_core::deposit::DepositState;
use topup_core::route::{RouteFile, StuckAfterConfig};
use uuid::Uuid;

/// Route-version-indexed thresholds for state-age alerts.
#[derive(Clone, Debug)]
pub struct AgeAlertConfig {
    thresholds: BTreeMap<(String, u64), (StuckAfterConfig, u64)>,
}

impl AgeAlertConfig {
    /// Builds alert thresholds from validated route files.
    pub fn from_routes(routes: &[RouteFile]) -> Result<Self, AgeAlertConfigError> {
        let mut thresholds = BTreeMap::new();
        for route in routes {
            let key = (route.route.clone(), route.version);
            if thresholds
                .insert(
                    key.clone(),
                    (
                        route.alerts.stuck_after_s.clone(),
                        route
                            .chain
                            .confirmations
                            .typical_credit_seconds(route.chain.chain_id),
                    ),
                )
                .is_some()
            {
                return Err(AgeAlertConfigError {
                    route: key.0,
                    version: key.1,
                });
            }
        }
        Ok(Self { thresholds })
    }

    fn threshold(
        &self,
        route: &str,
        version: u64,
        state: DepositState,
        sanctions_hold: bool,
    ) -> Option<u64> {
        let (stuck_after, confirmation_window) =
            self.thresholds.get(&(route.to_owned(), version))?;
        if sanctions_hold && state == DepositState::Confirmed {
            return Some(*confirmation_window);
        }
        match state {
            DepositState::Detected => Some(stuck_after.detected),
            DepositState::Confirmed => Some(stuck_after.confirmed),
            // A credited deposit waits for its merchant's sweep, which has no deadline.
            DepositState::Credited
            | DepositState::Swept
            | DepositState::Rejected
            | DepositState::Reversed => None,
        }
    }
}

/// Duplicate route/version alert configuration.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("duplicate age alert configuration for route `{route}` version {version}")]
pub struct AgeAlertConfigError {
    route: String,
    version: u64,
}

/// Periodically finds deposits older than their route's state threshold.
///
/// Every scan logs every overdue deposit; Sentry groups the alerts by route and state, and the
/// reporting throttle drops the repeats.
pub struct AgeAlerter {
    pool: PgPool,
    config: AgeAlertConfig,
    scan_interval: Duration,
}

impl AgeAlerter {
    /// Creates a periodic state-age alerter.
    #[must_use]
    pub const fn new(pool: PgPool, config: AgeAlertConfig, scan_interval: Duration) -> Self {
        Self {
            pool,
            config,
            scan_interval,
        }
    }

    /// Scans until cancellation, logging failures without stopping the task.
    pub async fn run(&self, cancellation: CancellationToken) {
        let mut ticker = interval(self.scan_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = ticker.tick() => {
                    if let Err(error) = self.scan_once().await {
                        tracing::error!(%error, "deposit age alert scan failed");
                    }
                }
            }
        }
    }

    /// Performs one state-age scan and returns the number of alerts emitted.
    pub async fn scan_once(&self) -> Result<u64, sqlx::Error> {
        let rows = sqlx::query_as::<_, DepositAgeRow>(
            r#"
            SELECT
                deposit.id,
                deposit.route,
                deposit.route_version,
                deposit.state,
                COALESCE(
                    (
                        SELECT max(transition.created_at)
                        FROM transitions AS transition
                        WHERE transition.deposit_id = deposit.id
                          AND transition.from_state <> transition.to_state
                          AND transition.to_state = deposit.state
                    ),
                    deposit.created_at
                ) AS entered_at,
                COALESCE((SELECT transition.evidence->>'sanctions_hold' = 'true'
                          FROM transitions AS transition WHERE transition.deposit_id=deposit.id
                          ORDER BY transition.created_at DESC, transition.id DESC LIMIT 1), false) AS sanctions_hold
            FROM deposits AS deposit
            WHERE deposit.state IN ('detected', 'confirmed')
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        let now = Utc::now();
        let mut alert_count = 0_u64;
        for row in rows {
            let Some(route) = row.route.as_deref() else {
                continue;
            };
            let Some(version) = row
                .route_version
                .and_then(|value| u64::try_from(value).ok())
            else {
                continue;
            };
            let Some(state) = parse_active_state(&row.state) else {
                continue;
            };
            let Some(threshold) = self
                .config
                .threshold(route, version, state, row.sanctions_hold)
            else {
                continue;
            };
            let age_seconds = now.signed_duration_since(row.entered_at).num_seconds();
            let Ok(threshold_seconds) = i64::try_from(threshold) else {
                continue;
            };
            if age_seconds > threshold_seconds {
                tracing::warn!(
                    tags.alert = if row.sanctions_hold { "TopupSanctionsHold" } else { "TopupDepositStateAgeExceeded" },
                    tags.route = route,
                    tags.state = state_code(state),
                    deposit_id = %crate::ids::format(crate::ids::DEPOSIT, row.id),
                    route,
                    route_version = version,
                    state = ?state,
                    age_seconds,
                    threshold_seconds,
                    "deposit has exceeded its state-age threshold"
                );
                alert_count = alert_count.saturating_add(1);
            }
        }
        Ok(alert_count)
    }
}

const fn state_code(state: DepositState) -> &'static str {
    match state {
        DepositState::Detected => "detected",
        DepositState::Confirmed => "confirmed",
        DepositState::Credited => "credited",
        DepositState::Swept => "swept",
        DepositState::Rejected => "rejected",
        DepositState::Reversed => "reversed",
    }
}

#[derive(FromRow)]
struct DepositAgeRow {
    id: Uuid,
    route: Option<String>,
    route_version: Option<i64>,
    state: String,
    entered_at: DateTime<Utc>,
    sanctions_hold: bool,
}

fn parse_active_state(state: &str) -> Option<DepositState> {
    match state {
        "detected" => Some(DepositState::Detected),
        "confirmed" => Some(DepositState::Confirmed),
        "credited" => Some(DepositState::Credited),
        "swept" | "rejected" => None,
        _ => None,
    }
}
