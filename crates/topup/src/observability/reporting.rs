//! Optional Sentry reporting: error and alert events, and Crons check-ins.
//!
//! Reporting is enabled only when `SENTRY_DSN` is set and non-empty. Without it no client is
//! bound, the tracing layer is not installed, and check-ins return before doing any work.
//!
//! Events carry what the production JSON log line carries (the same INFO ceiling and silenced
//! transport targets), minus [`SCRUBBED_FIELDS`]. There is no HTTP integration, so no request
//! body, header, URL, or client IP reaches Sentry, and `send_default_pii` stays off.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sentry::integrations::tracing::{EventFilter, SentryLayer};
use sentry::protocol::{
    Breadcrumb, Context, Event, MonitorCheckIn, MonitorCheckInStatus, MonitorConfig,
    MonitorIntervalUnit, MonitorSchedule,
};
use sentry::types::Dsn;
use sentry::{ClientInitGuard, ClientOptions, Envelope, Hub};
use tracing::{Level, Metadata, Subscriber};
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;

/// Tracing field that marks a WARN line as an alert event; its value is the alert name.
///
/// Every other `tags.*` field of an alert line is a grouping dimension, so keep them
/// low-cardinality (route, state, check, chain), never a deposit or event id.
const ALERT_FIELD: &str = "tags.alert";
/// Tracing fields removed from events and breadcrumbs: a product's end-user identifier.
const SCRUBBED_FIELDS: [&str; 1] = ["account_id"];
/// Context in which the tracing integration stores event fields.
const TRACING_FIELDS_CONTEXT: &str = "Rust Tracing Fields";
const RUNBOOKS: &str = "https://github.com/Phala-Network/phala-pay/blob/main/deploy/runbooks/";
/// Minimum interval between two events of the same issue; loops retry every few seconds.
const EVENT_REPEAT_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Direct signer/proxy retries share the business probes' hourly reminder ceiling.
const INTERNAL_FAILURE_REPEAT_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Minimum interval between two check-ins of one monitor, below the Crons limit of six a minute.
const CHECK_IN_INTERVAL: Duration = Duration::from_secs(60);

/// Invalid `SENTRY_DSN`; the value is never included.
#[derive(Debug, thiserror::Error)]
#[error("SENTRY_DSN is not a valid Sentry DSN")]
pub struct ReportingError;

/// The source commit the binary was built from (the image's `SOURCE_COMMIT` build argument), the
/// Sentry release; unset in a build without it.
const SOURCE_COMMIT: Option<&str> = option_env!("SOURCE_COMMIT");

/// Starts Sentry reporting from `SENTRY_DSN`; returns `None`, binding nothing, when it is unset
/// or empty.
///
/// `deployment` is the configuration's environment and whether the service is read-only
/// ([`deployment_environment`]); the release is the source commit. Keep the guard alive until
/// exit: dropping it flushes queued events.
pub fn init_reporting(
    deployment: Option<(&str, bool)>,
) -> Result<Option<ClientInitGuard>, ReportingError> {
    let options = client_options(
        std::env::var("SENTRY_DSN").ok().as_deref(),
        SOURCE_COMMIT,
        deployment.map(|(environment, read_only)| deployment_environment(environment, read_only)),
    )?;
    Ok(options.map(sentry::init))
}

/// The configured environment, or `<environment>-restore` for a read-only restore instance
/// (`deploy/RESTORE.md`), so a restore or drill instance never reports as the live environment.
fn deployment_environment(environment: &str, read_only: bool) -> String {
    if read_only {
        format!("{environment}-restore")
    } else {
        environment.to_owned()
    }
}

/// Production preflight: require a nonempty DSN without exposing its value.
pub fn require_sentry_dsn(dsn: Option<&str>) -> Result<(), ReportingError> {
    let dsn = dsn
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(ReportingError)?;
    dsn.parse::<Dsn>().map(|_| ()).map_err(|_| ReportingError)
}

/// Builds the client options, or `None` when `dsn` is unset or empty.
fn client_options(
    dsn: Option<&str>,
    commit: Option<&str>,
    environment: Option<String>,
) -> Result<Option<ClientOptions>, ReportingError> {
    let Some(dsn) = dsn.map(str::trim).filter(|dsn| !dsn.is_empty()) else {
        return Ok(None);
    };
    let dsn = dsn.parse::<Dsn>().map_err(|_| ReportingError)?;
    let throttle = EventThrottle::default();
    let mut options = ClientOptions::new()
        .send_default_pii(false)
        .before_send(move |event| throttle.admit(prepare_event(event)))
        .before_breadcrumb(|breadcrumb| Some(scrub_breadcrumb(breadcrumb)));
    options.dsn = Some(dsn);
    options.release = commit_release(commit).map(|release| Cow::Owned(release.to_owned()));
    options.environment = environment.map(Cow::Owned);
    Ok(Some(options))
}

/// A full hexadecimal commit id; anything else (an unset or malformed build argument) is no
/// release.
fn commit_release(commit: Option<&str>) -> Option<&str> {
    commit
        .filter(|commit| commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// The Sentry tracing layer when reporting is enabled.
///
/// ERROR lines and WARN lines carrying [`ALERT_FIELD`] become events; other INFO and WARN lines
/// become breadcrumbs. Spans are not sent, so span fields never reach Sentry.
pub(crate) fn tracing_layer<S>() -> Option<SentryLayer<S>>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    Hub::current().client().map(|_| {
        sentry::integrations::tracing::layer()
            .event_filter(event_filter)
            .span_filter(|_| false)
    })
}

fn event_filter(metadata: &Metadata<'_>) -> EventFilter {
    match *metadata.level() {
        Level::ERROR => EventFilter::Event,
        Level::WARN if metadata.fields().field(ALERT_FIELD).is_some() => EventFilter::Event,
        Level::WARN | Level::INFO => EventFilter::Breadcrumb,
        _ => EventFilter::Ignore,
    }
}

/// Scrubs fields, and gives alert events a stable fingerprint and their runbook.
fn prepare_event(mut event: Event<'static>) -> Event<'static> {
    if let Some(Context::Other(fields)) = event.contexts.get_mut(TRACING_FIELDS_CONTEXT) {
        for field in SCRUBBED_FIELDS {
            fields.remove(field);
        }
    }
    if let Some(alert) = event.tags.get("alert").cloned() {
        let fingerprint = ["topup-alert".to_owned(), alert.clone()]
            .into_iter()
            .chain(
                event
                    .tags
                    .iter()
                    .filter(|(name, _)| name.as_str() != "alert")
                    .map(|(name, value)| format!("{name}={value}")),
            )
            .map(Cow::Owned)
            .collect::<Vec<_>>();
        event.fingerprint = Cow::Owned(fingerprint);
        let runbook = format!("{RUNBOOKS}{}", runbook(&alert, &event.tags));
        event.tags.insert("runbook".to_owned(), runbook);
    }
    event
}

fn scrub_breadcrumb(mut breadcrumb: Breadcrumb) -> Breadcrumb {
    for field in SCRUBBED_FIELDS {
        breadcrumb.data.remove(field);
    }
    breadcrumb
}

/// The runbook for an alert, as indexed in `deploy/runbooks/README.md`.
fn runbook(alert: &str, tags: &BTreeMap<String, String>) -> &'static str {
    let tag = |name: &str| tags.get(name).map(String::as_str);
    match alert {
        "TopupOutboxBacklog" | "TopupOutboxStalled" | "TopupOutboxInternalFailure" => {
            "outbox-backlog.md"
        }
        "TopupHeartbeatStale"
        | "TopupTreasuryProgressAge"
        | "TopupRefundProgressAge"
        | "TopupCertificateExpiry"
        | "TopupCertificateProbeFailed"
        | "TopupBusinessProbeFailed" => "business-health.md",
        "TopupContractCodeMismatch"
        | "TopupFinalizedCheckpointConflict"
        | "TopupUnverifiedEvidenceMismatch" => "chain-frozen.md",
        "TopupRpcEndpointUnavailable" => "rpc-health.md",
        "TopupRpcDisagreement" | "TopupSanctionsHold" => "provider-disagreement.md",
        "TopupDepositReversalUnproven" => "deposit-reversed.md",
        "TopupDepositStateAgeExceeded" => match tag("state") {
            Some("detected" | "confirmed") => "provider-disagreement.md",
            _ => "README.md#alert-and-symptom-index",
        },
        // Both checks freeze the chain they fail on.
        "TopupReconciliationMismatch"
            if matches!(tag("check"), Some("address_derivation" | "custody_balance")) =>
        {
            "chain-frozen.md"
        }
        "TopupReconciliationMismatch" => "reconciliation-mismatch.md",
        "TopupAddressCapacity" => "address-capacity.md",
        "TopupLockExpiryFailing" => "lock-expiry-worker-failure.md",
        "TopupLockExposureNearCap" => "lock-exposure-near-cap.md",
        "TopupUnsupportedInflows" => "rejected-funds-at-treasury.md",
        "TopupSanctionsListStale"
        | "TopupSanctionsListVerifyFailed"
        | "TopupSanctionsRescreenFailed"
        | "TopupRefundDestinationSanctioned" => "sanctions-list.md",
        "TopupTreasurySanctioned" => "treasury-change.md#sanctioned-treasury",
        "TopupDepositReversed" | "TopupDepositPendingAfterReorg" => "deposit-reversed.md",
        _ => "README.md#alert-and-symptom-index",
    }
}

/// Sends at most one event per issue every [`EVENT_REPEAT_INTERVAL`], or hourly for direct
/// internal webhook failures. Business probes additionally manage their own transition state.
///
/// A failing loop logs the same error every few seconds; the first event opens or reopens the
/// issue, and the repeats would only spend the project's quota.
/// This plan ignores per-key rate limits and the org quota is shared, so the SDK must filter here.
#[derive(Default)]
struct EventThrottle(Mutex<BTreeMap<String, Instant>>);

impl EventThrottle {
    fn admit(&self, event: Event<'static>) -> Option<Event<'static>> {
        self.admit_at(event, Instant::now())
    }

    fn admit_at(&self, event: Event<'static>, now: Instant) -> Option<Event<'static>> {
        let interval =
            if event.tags.get("alert").map(String::as_str) == Some("TopupOutboxInternalFailure") {
                INTERNAL_FAILURE_REPEAT_INTERVAL
            } else {
                EVENT_REPEAT_INTERVAL
            };
        let key = if event
            .fingerprint
            .first()
            .is_some_and(|part| part == "topup-alert")
        {
            event.fingerprint.join("\u{1f}")
        } else {
            format!(
                "{}\u{1f}{}",
                event.logger.as_deref().unwrap_or_default(),
                event.message.as_deref().unwrap_or_default()
            )
        };
        let Ok(mut sent) = self.0.lock() else {
            return Some(event);
        };
        sent.retain(|_, at| now.saturating_duration_since(*at) < INTERNAL_FAILURE_REPEAT_INTERVAL);
        if sent
            .get(&key)
            .is_some_and(|at| now.saturating_duration_since(*at) < interval)
        {
            return None;
        }
        sent.insert(key, now);
        Some(event)
    }
}

/// A Sentry Crons monitor for one periodic job, created or updated by its check-ins.
#[derive(Debug)]
pub struct CronMonitor {
    slug: String,
    config: MonitorConfig,
    last_check_in: Mutex<Option<Instant>>,
}

impl CronMonitor {
    /// Hourly official sanctions publication verification.
    pub fn sanctions() -> Self {
        let mut monitor = Self::new(
            "topup-sanctions-refresh".to_owned(),
            MonitorSchedule::Interval {
                value: 1,
                unit: MonitorIntervalUnit::Hour,
            },
            5,
        );
        monitor.config.failure_issue_threshold = Some(6);
        monitor
    }
    /// Fast discovery: one completed round per minute, with a two-minute missed-check-in margin.
    #[must_use]
    pub fn fast_scanner(chain_id: u64) -> Self {
        let mut monitor = Self::heartbeat(format!("topup-fast-scanner-{chain_id}"), 2);
        monitor.config.failure_issue_threshold = Some(3);
        monitor
    }
    /// Dual coverage: one round per ten minutes, with a two-minute missed-check-in margin.
    #[must_use]
    pub fn coverage_scanner(chain_id: u64) -> Self {
        Self::new(
            format!("topup-coverage-scanner-{chain_id}"),
            MonitorSchedule::Interval {
                value: 10,
                unit: MonitorIntervalUnit::Minute,
            },
            2,
        )
    }

    /// Polls of the finality watch.
    #[must_use]
    pub fn finality_watch() -> Self {
        Self::heartbeat("topup-finality-watch".to_owned(), 5)
    }

    /// Iterations of one deposit pump; a step may run for four minutes.
    #[must_use]
    pub fn pump(instance: &str) -> Self {
        Self::heartbeat(format!("topup-pump-{instance}"), 5)
    }

    /// Polls of one webhook delivery worker.
    #[must_use]
    pub fn outbox(instance: &str) -> Self {
        Self::heartbeat(format!("topup-outbox-{instance}"), 5)
    }

    /// Successful rate-lock expiry scans.
    #[must_use]
    pub fn lock_expiry() -> Self {
        Self::heartbeat("topup-lock-expiry".to_owned(), 5)
    }

    /// Freshness of the WAL-G success marker; three stale observations in a row open an issue,
    /// so a restart's first minute does not.
    #[must_use]
    pub fn backup() -> Self {
        let mut monitor = Self::heartbeat("topup-backup".to_owned(), 2);
        monitor.config.failure_issue_threshold = Some(3);
        monitor
    }

    /// Reconciliation rounds, one every `every`.
    #[must_use]
    pub fn reconciler(every: Duration) -> Self {
        let minutes = every.as_secs().div_ceil(60).max(1);
        Self::new(
            "topup-reconciler".to_owned(),
            MonitorSchedule::Interval {
                value: minutes,
                unit: MonitorIntervalUnit::Minute,
            },
            minutes,
        )
    }

    fn heartbeat(slug: String, margin_minutes: u64) -> Self {
        Self::new(
            slug,
            MonitorSchedule::Interval {
                value: 1,
                unit: MonitorIntervalUnit::Minute,
            },
            margin_minutes,
        )
    }

    fn new(slug: String, schedule: MonitorSchedule, margin_minutes: u64) -> Self {
        Self {
            slug,
            config: MonitorConfig {
                schedule,
                checkin_margin: Some(margin_minutes),
                max_runtime: None,
                timezone: None,
                failure_issue_threshold: None,
                recovery_threshold: None,
            },
            last_check_in: Mutex::new(None),
        }
    }

    /// The monitor slug.
    #[must_use]
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// Sends an `ok` or `error` check-in carrying the monitor configuration, at most once every
    /// [`CHECK_IN_INTERVAL`]; does nothing while reporting is disabled.
    pub fn check_in(&self, healthy: bool) {
        let Some(client) = Hub::current().client() else {
            return;
        };
        let now = Instant::now();
        let Ok(mut last) = self.last_check_in.lock() else {
            return;
        };
        if last.is_some_and(|at| now.duration_since(at) < CHECK_IN_INTERVAL) {
            return;
        }
        *last = Some(now);
        let mut envelope = Envelope::new();
        envelope.add_item(MonitorCheckIn {
            check_in_id: Uuid::new_v4(),
            monitor_slug: self.slug.clone(),
            status: if healthy {
                MonitorCheckInStatus::Ok
            } else {
                MonitorCheckInStatus::Error
            },
            environment: client.options().environment.as_deref().map(str::to_owned),
            duration: None,
            monitor_config: Some(self.config.clone()),
        });
        client.send_envelope(envelope);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sentry::protocol::{EnvelopeItem, MonitorCheckInStatus, Value};
    use sentry::test::{with_captured_envelopes_options, with_captured_events_options};

    use super::{CronMonitor, client_options, deployment_environment, runbook};
    use crate::observability::log_subscriber;

    const TEST_DSN: &str = "https://public@sentry.invalid/1";

    fn options() -> sentry::ClientOptions {
        client_options(Some(TEST_DSN), None, None)
            .expect("test DSN is valid")
            .expect("test DSN enables reporting")
    }

    #[test]
    fn unset_or_empty_dsn_disables_reporting_and_every_hook() {
        for dsn in [None, Some(""), Some("  ")] {
            assert!(client_options(dsn, None, None).expect("disabled").is_none());
        }
        assert!(client_options(Some("not a dsn"), None, None).is_err());
        assert!(sentry::Hub::current().client().is_none());
        assert!(super::tracing_layer::<tracing_subscriber::Registry>().is_none());
        // Without a client this returns before building a check-in.
        CronMonitor::lock_expiry().check_in(true);
    }

    #[test]
    fn synthetic_business_alerts_use_production_fingerprints_and_runbooks() {
        let alerts = [
            "TopupOutboxBacklog",
            "TopupOutboxStalled",
            "TopupOutboxInternalFailure",
            "TopupHeartbeatStale",
            "TopupTreasuryProgressAge",
            "TopupRefundProgressAge",
            "TopupCertificateExpiry",
            "TopupCertificateProbeFailed",
            "TopupBusinessProbeFailed",
            "TopupAddressCapacity",
        ];
        let events = with_captured_events_options(
            || {
                tracing::subscriber::with_default(log_subscriber(std::io::sink), || {
                    for alert in alerts {
                        crate::observability::emit_alert(alert, "synthetic", "critical", 1, 0);
                    }
                });
            },
            options(),
        );
        assert_eq!(events.len(), alerts.len());
        for (event, alert) in events.iter().zip(alerts) {
            assert_eq!(event.tags["alert"], alert);
            assert_eq!(event.fingerprint[0], "topup-alert");
            let expected = if alert.starts_with("TopupOutbox") {
                "outbox-backlog.md"
            } else if alert == "TopupAddressCapacity" {
                "address-capacity.md"
            } else {
                "business-health.md"
            };
            assert!(event.tags["runbook"].ends_with(expected));
        }
    }

    #[test]
    fn direct_internal_failures_are_limited_to_hourly_per_component() {
        let throttle = super::EventThrottle::default();
        let start = std::time::Instant::now();
        let alert = |component: &str| {
            super::prepare_event(sentry::protocol::Event {
                tags: std::collections::BTreeMap::from([
                    ("alert".to_owned(), "TopupOutboxInternalFailure".to_owned()),
                    ("component".to_owned(), component.to_owned()),
                ]),
                ..Default::default()
            })
        };
        assert!(throttle.admit_at(alert("signing_failed"), start).is_some());
        assert!(
            throttle
                .admit_at(
                    alert("signing_failed"),
                    start + std::time::Duration::from_secs(600)
                )
                .is_none()
        );
        assert!(
            throttle
                .admit_at(
                    alert("signing_failed"),
                    start + std::time::Duration::from_secs(3599)
                )
                .is_none()
        );
        assert!(
            throttle
                .admit_at(
                    alert("proxy_connect_failed"),
                    start + std::time::Duration::from_secs(3599)
                )
                .is_some()
        );
        assert!(
            throttle
                .admit_at(
                    alert("signing_failed"),
                    start + std::time::Duration::from_secs(3600)
                )
                .is_some()
        );
        let other = || sentry::protocol::Event {
            message: Some("database operation failed".to_owned()),
            ..Default::default()
        };
        assert!(throttle.admit_at(other(), start).is_some());
        assert!(
            throttle
                .admit_at(other(), start + std::time::Duration::from_secs(600))
                .is_some()
        );
    }

    #[test]
    fn production_preflight_requires_a_present_valid_dsn() {
        for dsn in [None, Some(""), Some("  "), Some("not a dsn")] {
            assert!(super::require_sentry_dsn(dsn).is_err());
        }
        assert!(super::require_sentry_dsn(Some(TEST_DSN)).is_ok());
    }

    #[test]
    fn release_is_the_source_commit() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let options = client_options(Some(TEST_DSN), Some(commit), None)
            .expect("valid")
            .expect("enabled");
        assert_eq!(options.release.as_deref(), Some(commit));
        for local in [None, Some(""), Some("dev")] {
            let options = client_options(Some(TEST_DSN), local, None)
                .expect("valid")
                .expect("enabled");
            assert_eq!(options.release, None);
        }
    }

    #[test]
    fn a_read_only_restore_instance_reports_under_its_own_environment() {
        assert_eq!(deployment_environment("staging", true), "staging-restore");
        assert_eq!(deployment_environment("staging", false), "staging");
        let options = client_options(
            Some(TEST_DSN),
            None,
            Some(deployment_environment("production", true)),
        )
        .expect("valid")
        .expect("enabled");
        assert_eq!(options.environment.as_deref(), Some("production-restore"));
    }

    #[test]
    fn production_log_lines_map_to_events_with_alert_fingerprints_and_runbooks() {
        let events = with_captured_events_options(
            || {
                tracing::subscriber::with_default(log_subscriber(std::io::sink), || {
                    tracing::info!("routine line becomes a breadcrumb");
                    tracing::warn!(
                        account_id = "customer-1",
                        "plain warning stays a breadcrumb"
                    );
                    tracing::warn!(
                        target: "reqwest::connect",
                        url = "https://user:rpc-secret@rpc.example",
                        "transport warning"
                    );
                    for deposit in ["first", "second"] {
                        tracing::warn!(
                            tags.alert = "TopupDepositStateAgeExceeded",
                            tags.route = "route-a",
                            tags.state = "confirmed",
                            deposit_id = deposit,
                            account_id = "customer-1",
                            "deposit has exceeded its state-age threshold"
                        );
                    }
                    tracing::error!(check = "missing_deposit", "reconciliation check failed");
                    tracing::error!(check = "missing_deposit", "reconciliation check failed");
                });
            },
            options(),
        );

        assert_eq!(events.len(), 2, "{events:#?}");
        let alert = &events[0];
        assert_eq!(
            alert.fingerprint.as_ref(),
            [
                "topup-alert",
                "TopupDepositStateAgeExceeded",
                "route=route-a",
                "state=confirmed"
            ]
        );
        assert_eq!(
            alert.tags.get("runbook").map(String::as_str),
            Some(
                "https://github.com/Phala-Network/phala-pay/blob/main/deploy/runbooks/\
                 provider-disagreement.md"
            )
        );
        let rendered = format!("{events:?}");
        assert!(rendered.contains("first"), "{rendered}");
        assert!(
            !rendered.contains("second"),
            "repeat alert was not throttled"
        );
        assert!(!rendered.contains("customer-1"), "{rendered}");
        assert!(!rendered.contains("rpc-secret"), "{rendered}");
        assert!(rendered.contains("routine line becomes a breadcrumb"));
        assert_eq!(
            events[1].message.as_deref(),
            Some("reconciliation check failed")
        );
        assert!(events[1].fingerprint.as_ref() == ["{{ default }}"]);
    }

    #[test]
    fn scanner_monitors_report_separate_per_chain_fast_and_coverage_cadences() {
        let envelopes = with_captured_envelopes_options(
            || {
                CronMonitor::fast_scanner(1).check_in(true);
                CronMonitor::coverage_scanner(1).check_in(false);
                CronMonitor::coverage_scanner(8453).check_in(true);
            },
            options().environment("staging"),
        );
        let checks = envelopes
            .iter()
            .flat_map(|e| e.items())
            .filter_map(|item| match item {
                EnvelopeItem::MonitorCheckIn(check) => Some(check),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(checks.len(), 3);
        for (check, (slug, minutes, status)) in checks.iter().zip([
            ("topup-fast-scanner-1", 1, MonitorCheckInStatus::Ok),
            ("topup-coverage-scanner-1", 10, MonitorCheckInStatus::Error),
            ("topup-coverage-scanner-8453", 10, MonitorCheckInStatus::Ok),
        ]) {
            assert_eq!(check.monitor_slug, slug);
            assert_eq!(check.status, status);
            let config = serde_json::to_value(&check.monitor_config).unwrap();
            assert_eq!(
                config["schedule"],
                serde_json::json!({"type":"interval","value":minutes,"unit":"minute"})
            );
            assert_eq!(config["checkin_margin"], 2);
        }
    }

    #[test]
    fn check_ins_carry_the_monitor_config_and_are_rate_limited() {
        let envelopes = with_captured_envelopes_options(
            || {
                let monitor = CronMonitor::backup();
                monitor.check_in(false);
                monitor.check_in(true);
            },
            options().environment("staging"),
        );
        let check_ins = envelopes
            .iter()
            .flat_map(|envelope| envelope.items())
            .filter_map(|item| match item {
                EnvelopeItem::MonitorCheckIn(check_in) => Some(check_in),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(check_ins.len(), 1);
        let check_in = check_ins[0];
        assert_eq!(check_in.monitor_slug, "topup-backup");
        assert_eq!(check_in.status, MonitorCheckInStatus::Error);
        assert_eq!(check_in.environment.as_deref(), Some("staging"));
        let config = serde_json::to_value(&check_in.monitor_config).expect("config serializes");
        assert_eq!(
            config,
            serde_json::json!({
                "schedule": {"type": "interval", "value": 1, "unit": "minute"},
                "checkin_margin": 2,
                "failure_issue_threshold": 3,
            })
        );
    }

    #[test]
    fn every_runbook_link_names_a_committed_runbook() {
        let runbooks = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/runbooks");
        let tags = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect()
        };
        for (alert, pairs) in [
            ("TopupDepositStateAgeExceeded", &[("state", "detected")][..]),
            ("TopupDepositStateAgeExceeded", &[("state", "confirmed")]),
            (
                "TopupReconciliationMismatch",
                &[("check", "address_derivation")],
            ),
            (
                "TopupReconciliationMismatch",
                &[("check", "custody_balance")],
            ),
            (
                "TopupReconciliationMismatch",
                &[("check", "credit_recomputation")],
            ),
            ("TopupLockExpiryFailing", &[]),
            ("TopupLockExposureNearCap", &[]),
            ("TopupUnsupportedInflows", &[]),
            ("TopupTreasurySanctioned", &[]),
            ("TopupDepositReversed", &[]),
            ("UnknownAlert", &[]),
        ] {
            let path = runbook(alert, &tags(pairs));
            let file = path.split_once('#').map_or(path, |(file, _)| file);
            assert!(runbooks.join(file).is_file(), "{alert}: {path}");
        }
    }

    #[test]
    fn scrubbed_fields_leave_breadcrumbs() {
        let mut breadcrumb = sentry::Breadcrumb::default();
        breadcrumb
            .data
            .insert("account_id".to_owned(), Value::from("customer-1"));
        breadcrumb
            .data
            .insert("deposit_id".to_owned(), Value::from("kept"));
        let breadcrumb = super::scrub_breadcrumb(breadcrumb);
        assert!(!breadcrumb.data.contains_key("account_id"));
        assert!(breadcrumb.data.contains_key("deposit_id"));
    }
}
