//! Verified OFAC SDN snapshots and audited operator supplements. All decisions are local.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use tokio_util::sync::CancellationToken;
use topup_adapters::risk::oracle::SanctionsSource;
use topup_core::screening::{SanctionsProvenance, SanctionsResult, SanctionsVerdict};
use uuid::Uuid;

use crate::audit::{self, Actor, Entry};
use crate::refunds::{DestinationScreener, DestinationScreening};
use crate::routes::RouteSet;

const DOWNLOAD: &str = "https://sanctionslistservice.ofac.treas.gov/api/download/SDN.XML";
const PREVIEW: &str = "https://sanctionslistservice.ofac.treas.gov/api/PublicationPreview/SdnList";
const HOSTS: [&str; 2] = [
    "sanctionslistservice.ofac.treas.gov",
    "wc2h-sls-prod-public-published.s3.us-gov-west-1.amazonaws.com",
];
/// Refresh immediately at startup, then hourly.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
const STALE_ALERT_SECONDS: i64 = 6 * 3600;
const MAX_XML_BYTES: usize = 64 * 1024 * 1024;

/// Only sanctions configuration knob. Example: `max_staleness: 24h`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SanctionsConfig {
    /// Longest acceptable age of a successful official-hash verification.
    pub max_staleness: String,
}
impl Default for SanctionsConfig {
    fn default() -> Self {
        Self {
            max_staleness: "24h".to_owned(),
        }
    }
}
impl SanctionsConfig {
    /// Parses a positive integer followed by `s`, `m`, `h`, or `d`.
    pub fn duration(&self) -> Result<Duration, String> {
        let (value, multiplier) = match self.max_staleness.chars().last() {
            Some('s') => (self.max_staleness.strip_suffix('s'), 1),
            Some('m') => (self.max_staleness.strip_suffix('m'), 60),
            Some('h') => (self.max_staleness.strip_suffix('h'), 3600),
            Some('d') => (self.max_staleness.strip_suffix('d'), 86400),
            _ => (None, 1),
        };
        let seconds = value
            .and_then(|v| v.parse::<u64>().ok())
            .and_then(|v| v.checked_mul(multiplier))
            .filter(|v| *v > 0 && i64::try_from(*v).is_ok());
        seconds.map(Duration::from_secs).ok_or_else(|| {
            "sanctions.max_staleness requires a positive integer and s/m/h/d suffix".to_owned()
        })
    }
}

/// Snapshot provenance exposed to the admin daily report.
#[derive(Clone, Debug, FromRow, Serialize, utoipa::ToSchema)]
pub struct Snapshot {
    /// Snapshot identifier.
    pub id: Uuid,
    /// OFAC publication date.
    pub publish_date: NaiveDate,
    /// Exact SHA-256 bytes, encoded as hex in screening evidence.
    pub sha256: String,
    /// SDN entities, independently counted during parsing.
    pub record_count: i64,
    /// Digital currency identifiers, including non-EVM values.
    pub address_count: i64,
    /// Latest successful official-hash verification.
    pub verified_at: DateTime<Utc>,
}
/// Latest activated snapshot; historical snapshots remain intact.
pub async fn active(pool: &PgPool) -> Result<Option<Snapshot>, sqlx::Error> {
    sqlx::query_as("SELECT id,publish_date,encode(sha256,'hex') AS sha256,record_count,address_count,verified_at FROM sanctions_list_snapshots WHERE source='ofac_sdn' AND activated_at IS NOT NULL ORDER BY activated_at DESC,id DESC LIMIT 1")
        .fetch_optional(pool).await
}

/// Pure deny-first rules. Failed reads cannot turn a positive hit into a clear answer.
#[must_use]
pub fn verdict(
    snapshot_hit: bool,
    manual_hit: bool,
    snapshot_fresh: bool,
    reads_ok: bool,
) -> SanctionsVerdict {
    if snapshot_hit || manual_hit {
        SanctionsVerdict::Sanctioned
    } else if snapshot_fresh && reads_ok {
        SanctionsVerdict::Clear
    } else {
        SanctionsVerdict::Uncertain
    }
}

/// Decision-time local screener shared by deposits, treasury changes and refund destinations.
#[derive(Clone)]
pub struct ListScreener {
    pool: PgPool,
    max_staleness: Duration,
}
impl ListScreener {
    /// Composes a local source with the validated freshness limit.
    #[must_use]
    pub fn new(pool: PgPool, max_staleness: Duration) -> Self {
        Self {
            pool,
            max_staleness,
        }
    }
    /// Screens without chain restrictions or a historical-block pin.
    pub async fn check(&self, address: Address, purpose: &'static str) -> SanctionsResult {
        let now = Utc::now();
        // Each source is one statement, so its hit and provenance share an MVCC snapshot.
        let snapshot: Result<Option<(Uuid, NaiveDate, Vec<u8>, DateTime<Utc>, bool)>, sqlx::Error> = sqlx::query_as(
            "SELECT s.id,s.publish_date,s.sha256,s.verified_at,EXISTS(SELECT 1 FROM sanctions_list_addresses a WHERE a.snapshot_id=s.id AND a.evm_address=$1) FROM sanctions_list_snapshots s WHERE s.source='ofac_sdn' AND s.activated_at IS NOT NULL ORDER BY s.activated_at DESC,s.id DESC LIMIT 1")
            .bind(address.as_slice()).fetch_optional(&self.pool).await;
        let manual: Result<bool, sqlx::Error> = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sanctions_manual_entries WHERE evm_address=$1 AND removed_at IS NULL)")
            .bind(address.as_slice()).fetch_one(&self.pool).await;
        let reads_ok = snapshot.is_ok() && manual.is_ok();
        let manual_hit = manual.unwrap_or(false);
        let mut evidence = SanctionsProvenance {
            snapshot_id: None,
            sha256: None,
            publish_date: None,
            verified_at: None,
            manual_hit,
            screened_at: now.timestamp(),
        };
        let (hit, fresh) = match snapshot {
            Ok(Some((id, date, sha, verified, hit))) => {
                evidence.snapshot_id = Some(id);
                evidence.sha256 = B256::try_from(sha.as_slice()).ok();
                evidence.publish_date = Some(date.to_string());
                evidence.verified_at = Some(verified.timestamp());
                let age = now.signed_duration_since(verified).num_seconds();
                (
                    hit,
                    evidence.sha256.is_some()
                        && age >= 0
                        && chrono::Duration::from_std(self.max_staleness)
                            .is_ok_and(|limit| now.signed_duration_since(verified) <= limit),
                )
            }
            _ => (false, false),
        };
        let verdict = verdict(hit, manual_hit, fresh, reads_ok);
        crate::observability::sanctions_metrics::screen(purpose, verdict);
        SanctionsResult {
            verdict,
            provenance: Some(evidence),
        }
    }
}
#[async_trait]
impl SanctionsSource for ListScreener {
    async fn sanctions(&self, address: Address, _block_number: u64) -> SanctionsResult {
        self.check(address, "deposit").await
    }
}
#[async_trait]
impl DestinationScreener for ListScreener {
    async fn screen(
        &self,
        _route: &topup_core::route::RouteFile,
        destination: Address,
    ) -> DestinationScreening {
        match self.check(destination, "destination").await.verdict {
            SanctionsVerdict::Sanctioned => DestinationScreening::Sanctioned,
            SanctionsVerdict::Clear => DestinationScreening::Clear,
            SanctionsVerdict::Uncertain => DestinationScreening::Unavailable,
        }
    }
}

#[derive(Deserialize)]
struct SdnList {
    #[serde(rename = "publshInformation")]
    publication: Publication,
    #[serde(rename = "sdnEntry", default)]
    entries: Vec<SdnEntry>,
}
#[derive(Deserialize)]
struct Publication {
    #[serde(rename = "Publish_Date")]
    date: String,
    #[serde(rename = "Record_Count")]
    count: i64,
}
#[derive(Deserialize)]
struct SdnEntry {
    uid: i64,
    #[serde(rename = "idList")]
    ids: Option<IdList>,
}
#[derive(Deserialize)]
struct IdList {
    #[serde(rename = "id", default)]
    ids: Vec<SdnId>,
}
#[derive(Deserialize)]
struct SdnId {
    #[serde(rename = "idType")]
    kind: String,
    #[serde(rename = "idNumber")]
    value: String,
}
#[derive(Debug)]
struct CurrencyAddress {
    uid: i64,
    kind: String,
    raw: String,
    evm: Option<Address>,
}
#[derive(Debug)]
struct Parsed {
    date: NaiveDate,
    count: i64,
    addresses: Vec<CurrencyAddress>,
    parse_errors: u64,
}

fn parse(bytes: &[u8]) -> Result<Parsed, RefreshError> {
    // Reject incomplete documents, extra roots and DTDs before deserializing the known schema.
    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut depth = 0_u64;
    let mut roots = 0_u64;
    loop {
        use quick_xml::events::Event;
        match reader.read_event().map_err(|_| RefreshError::Parse)? {
            Event::Start(e) => {
                if depth == 0 {
                    roots = roots.saturating_add(1);
                    if e.local_name().as_ref() != b"sdnList" {
                        return Err(RefreshError::Parse);
                    }
                }
                depth = depth.checked_add(1).ok_or(RefreshError::Parse)?;
            }
            Event::End(_) => {
                depth = depth.checked_sub(1).ok_or(RefreshError::Parse)?;
            }
            Event::Empty(_) if depth == 0 => return Err(RefreshError::Parse),
            Event::DocType(_) => return Err(RefreshError::Parse),
            Event::Text(e) if depth == 0 && !e.as_ref().iter().all(u8::is_ascii_whitespace) => {
                return Err(RefreshError::Parse);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 || roots != 1 {
        return Err(RefreshError::Parse);
    }
    let document: SdnList = quick_xml::de::from_reader(bytes).map_err(|_| RefreshError::Parse)?;
    if document.publication.count <= 0
        || usize::try_from(document.publication.count).ok() != Some(document.entries.len())
    {
        return Err(RefreshError::Parse);
    }
    let date = NaiveDate::parse_from_str(document.publication.date.trim(), "%m/%d/%Y")
        .map_err(|_| RefreshError::Parse)?;
    let mut addresses = BTreeMap::new();
    let mut parse_errors = 0_u64;
    let mut uids = BTreeSet::new();
    for entry in document.entries {
        if entry.uid <= 0 || !uids.insert(entry.uid) {
            return Err(RefreshError::Parse);
        }
        for id in entry.ids.into_iter().flat_map(|ids| ids.ids) {
            if !id.kind.starts_with("Digital Currency Address - ") {
                continue;
            }
            let evm = id.value.trim().parse::<Address>().ok();
            if id.value.trim().starts_with("0x") && evm.is_none() {
                parse_errors = parse_errors.saturating_add(1);
            }
            addresses
                .entry((entry.uid, id.value.clone()))
                .or_insert(CurrencyAddress {
                    uid: entry.uid,
                    kind: id.kind,
                    raw: id.value,
                    evm,
                });
        }
    }
    Ok(Parsed {
        date,
        count: document.publication.count,
        addresses: addresses.into_values().collect(),
        parse_errors,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewFile {
    file_name: String,
    hash_codes: BTreeMap<String, String>,
    last_updated: String,
}

/// Bounded, redacted refresh failures; never log signed download URLs or response bodies.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// Network, HTTP or publication-preview contract failed.
    #[error("sanctions publication fetch failed")]
    Fetch,
    /// Exact bytes did not match the official SHA-256.
    #[error("sanctions publication hash mismatch")]
    Hash,
    /// XML schema, record count or publication date is invalid.
    #[error("sanctions publication validation failed")]
    Parse,
    /// Local persistence failed.
    #[error("sanctions snapshot persistence failed")]
    Database(#[from] sqlx::Error),
}
impl RefreshError {
    fn code(&self) -> &'static str {
        match self {
            Self::Fetch | Self::Database(_) => "fetch_error",
            Self::Hash => "hash_mismatch",
            Self::Parse => "parse_error",
        }
    }
}
/// Successful refresh outcome.
#[derive(Debug, PartialEq)]
pub enum Refresh {
    /// Same published hash; verification advanced.
    Unchanged,
    /// A new fully verified snapshot activated.
    Activated,
}

/// Official endpoints are fixed; tests inject localhost endpoints privately.
pub struct Refresher {
    pool: PgPool,
    client: reqwest::Client,
    preview: String,
    download: String,
}
impl Refresher {
    /// Builds the TLS client with an exact host allowlist and bounded redirect chain.
    pub fn new(pool: PgPool) -> Result<Self, RefreshError> {
        #[cfg(feature = "test-support")]
        if let Ok(origin) = std::env::var("TOPUP_TEST_SLS_ORIGIN") {
            return Self::fixture(pool, &origin);
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let url = attempt.url();
                if attempt.previous().len() >= 5
                    || url.scheme() != "https"
                    || !url.host_str().is_some_and(|host| HOSTS.contains(&host))
                {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .map_err(|_| RefreshError::Fetch)?;
        Ok(Self {
            pool,
            client,
            preview: PREVIEW.to_owned(),
            download: DOWNLOAD.to_owned(),
        })
    }
    /// Loopback-only HTTP fixture, excluded from production builds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture(pool: PgPool, origin: &str) -> Result<Self, RefreshError> {
        let url = url::Url::parse(origin).map_err(|_| RefreshError::Fetch)?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(RefreshError::Fetch);
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| RefreshError::Fetch)?;
        let origin = origin.trim_end_matches('/');
        Ok(Self {
            pool,
            client,
            preview: format!("{origin}/api/PublicationPreview/SdnList"),
            download: format!("{origin}/api/download/SDN.XML"),
        })
    }
    async fn bytes(
        &self,
        request: reqwest::RequestBuilder,
        limit: usize,
    ) -> Result<Vec<u8>, RefreshError> {
        let response = request.send().await.map_err(|_| RefreshError::Fetch)?;
        if !response.status().is_success() {
            return Err(RefreshError::Fetch);
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| RefreshError::Fetch)?;
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|len| len > limit)
            {
                return Err(RefreshError::Fetch);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    /// Revalidates the current publication, retaining the previous snapshot on every failure.
    pub async fn refresh(&self) -> Result<Refresh, RefreshError> {
        let outcome = self.refresh_inner().await;
        let code = match &outcome {
            Ok(Refresh::Unchanged) => "unchanged",
            Ok(Refresh::Activated) => "activated",
            Err(error) => error.code(),
        };
        crate::observability::sanctions_metrics::refresh(code);
        if matches!(outcome, Err(RefreshError::Hash | RefreshError::Parse)) {
            tracing::error!(
                tags.alert = "TopupSanctionsListVerifyFailed",
                result = code,
                "OFAC verification failed; previous evidence retained"
            );
        }
        outcome
    }
    async fn refresh_inner(&self) -> Result<Refresh, RefreshError> {
        let preview = self
            .bytes(self.client.post(&self.preview), 1024 * 1024)
            .await?;
        let files: Vec<PreviewFile> =
            serde_json::from_slice(&preview).map_err(|_| RefreshError::Fetch)?;
        let mut matching = files.into_iter().filter(|f| f.file_name == "SDN.XML");
        let file = matching.next().ok_or(RefreshError::Fetch)?;
        if matching.next().is_some() || file.last_updated.trim().is_empty() {
            return Err(RefreshError::Fetch);
        }
        let sha = file
            .hash_codes
            .get("SHA-256")
            .and_then(|value| hex::decode(value).ok())
            .filter(|v| v.len() == 32)
            .ok_or(RefreshError::Fetch)?;
        let previous = active(&self.pool).await?;
        let parsed = if previous
            .as_ref()
            .is_some_and(|s| s.sha256 == hex::encode(&sha))
        {
            None
        } else {
            let bytes = self
                .bytes(self.client.get(&self.download), MAX_XML_BYTES)
                .await?;
            if Sha256::digest(&bytes).as_slice() != sha {
                return Err(RefreshError::Hash);
            }
            Some(parse(&bytes)?)
        };
        // Serialize activations and recheck the active snapshot after the network read.
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('sanctions-list-refresh',0))")
            .execute(&mut *tx)
            .await?;
        let current: Option<(Uuid, Vec<u8>, NaiveDate)> = sqlx::query_as("SELECT id,sha256,publish_date FROM sanctions_list_snapshots WHERE source='ofac_sdn' AND activated_at IS NOT NULL ORDER BY activated_at DESC,id DESC LIMIT 1 FOR UPDATE").fetch_optional(&mut *tx).await?;
        if let Some((id, hash, _)) = &current
            && *hash == sha
        {
            sqlx::query(
                "UPDATE sanctions_list_snapshots SET verified_at=clock_timestamp() WHERE id=$1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(Refresh::Unchanged);
        }
        let parsed = parsed.ok_or(RefreshError::Fetch)?;
        if current
            .as_ref()
            .is_some_and(|(_, _, date)| parsed.date < *date)
        {
            return Err(RefreshError::Parse);
        }
        let old: Vec<Vec<u8>> = if let Some((id, _, _)) = &current {
            sqlx::query_scalar("SELECT DISTINCT evm_address FROM sanctions_list_addresses WHERE snapshot_id=$1 AND evm_address IS NOT NULL").bind(id).fetch_all(&mut *tx).await?
        } else {
            Vec::new()
        };
        let id = Uuid::new_v4();
        // A hash previously seen can become active again only if publication monotonicity permits.
        let id: Uuid = sqlx::query_scalar("INSERT INTO sanctions_list_snapshots(id,source,publish_date,sha256,record_count,address_count,fetched_at,activated_at,verified_at) VALUES($1,'ofac_sdn',$2,$3,$4,$5,clock_timestamp(),clock_timestamp(),clock_timestamp()) ON CONFLICT(source,sha256) DO UPDATE SET activated_at=clock_timestamp(),verified_at=clock_timestamp() RETURNING id")
            .bind(id).bind(parsed.date).bind(&sha).bind(parsed.count).bind(i64::try_from(parsed.addresses.len()).map_err(|_| RefreshError::Parse)?).fetch_one(&mut *tx).await?;
        for address in &parsed.addresses {
            sqlx::query("INSERT INTO sanctions_list_addresses(snapshot_id,sdn_uid,id_type,raw_value,evm_address) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
                .bind(id).bind(address.uid).bind(&address.kind).bind(&address.raw).bind(address.evm.map(|a| a.to_vec())).execute(&mut *tx).await?;
        }
        let old: BTreeSet<_> = old.into_iter().collect();
        let new: BTreeSet<_> = parsed
            .addresses
            .iter()
            .filter_map(|a| a.evm.map(|a| a.to_vec()))
            .collect();
        let changes = serde_json::json!({"sha256":hex::encode(&sha),"added":new.difference(&old).count(),"removed":old.difference(&new).count()}).to_string();
        audit::insert(
            &mut *tx,
            &Entry {
                account_id: None,
                actor: &Actor::system("sanctions_refresh"),
                action: "sanctions.snapshot_activated",
                subject: &id.to_string(),
                reason: &changes,
            },
        )
        .await?;
        tx.commit().await?;
        crate::observability::sanctions_metrics::parse_errors(parsed.parse_errors);
        tracing::info!(snapshot_id=%id, sha256=%hex::encode(&sha), added=new.difference(&old).count(), removed=old.difference(&new).count(), "verified sanctions snapshot activated");
        Ok(Refresh::Activated)
    }
    /// Cancellation-aware hourly refresh, freshness alerts and activation re-screening.
    pub async fn run(
        self,
        routes: Arc<RouteSet>,
        screening: Arc<ListScreener>,
        cancellation: CancellationToken,
    ) {
        let cron = crate::observability::CronMonitor::sanctions();
        let mut pending_rescreen = true;
        let mut interval = tokio::time::interval(REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { biased; _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
            let outcome = tokio::select! { biased; _ = cancellation.cancelled() => break, result = self.refresh() => result };
            cron.check_in(outcome.is_ok());
            if let Err(error) = &outcome {
                tracing::warn!(
                    result = error.code(),
                    "sanctions refresh failed; retaining previous verification time"
                );
            }
            if matches!(outcome, Ok(Refresh::Activated)) {
                pending_rescreen = true;
            }
            if pending_rescreen && outcome.is_ok() {
                match rescreen(&self.pool, &routes, &*screening).await {
                    Ok(()) => pending_rescreen = false,
                    Err(_) => tracing::error!(
                        "sanctions activation re-screen failed; retry on next refresh"
                    ),
                }
            }
            match active(&self.pool).await {
                Ok(Some(snapshot)) => {
                    let age = Utc::now()
                        .signed_duration_since(snapshot.verified_at)
                        .num_seconds()
                        .max(0);
                    crate::observability::sanctions_metrics::snapshot(&snapshot, age, &self.pool)
                        .await;
                    if age > STALE_ALERT_SECONDS {
                        tracing::error!(
                            tags.alert = "TopupSanctionsListStale",
                            age_seconds = age,
                            "OFAC verification older than six hours; inspect sanctions runbook"
                        );
                    }
                }
                _ => tracing::error!(
                    tags.alert = "TopupSanctionsListStale",
                    "no readable active sanctions snapshot; negative decisions hold"
                ),
            }
        }
    }
}

/// Forces current and pending treasuries, and pending refund destinations, through existing paths.
pub async fn rescreen(
    pool: &PgPool,
    routes: &RouteSet,
    screening: &dyn DestinationScreener,
) -> Result<(), sqlx::Error> {
    // Existing daily worker consumes these in bounded batches; force current treasuries due.
    sqlx::query("UPDATE treasuries SET screened_at='epoch' WHERE applied_at IS NOT NULL AND replaced_at IS NULL").execute(pool).await?;
    crate::treasuries::rescreen_due(pool, routes, screening, Utc::now())
        .await
        .map_err(|_| sqlx::Error::Protocol("treasury rescreen failed".into()))?;
    crate::treasuries::rescreen_pending(pool, routes, screening)
        .await
        .map_err(|_| sqlx::Error::Protocol("pending treasury rescreen failed".into()))?;
    let destinations: Vec<(Uuid, String, i64, bool)> = sqlx::query_as("SELECT r.id,r.destination_address,d.chain_id,r.livemode FROM refunds r JOIN deposits d ON d.id=r.deposit_id WHERE r.status='pending'").fetch_all(pool).await?;
    for (id, destination, chain, livemode) in destinations {
        let address = destination
            .parse()
            .map_err(|_| sqlx::Error::Protocol("invalid refund destination".into()))?;
        let Some(route) = routes.routes().iter().find(|r| {
            i64::try_from(r.chain.chain_id).ok() == Some(chain) && r.livemode == livemode
        }) else {
            continue;
        };
        if screening.screen(route, address).await == DestinationScreening::Sanctioned {
            // The service does not move refunds. Mark the hit for the operator and retain reservation;
            // finality verification remains the existing ledger path.
            audit::insert(
                pool,
                &Entry {
                    account_id: None,
                    actor: &Actor::system("sanctions_refresh"),
                    action: "sanctions.refund_destination_hit",
                    subject: &id.to_string(),
                    reason: "active sanctions list names a pending refund destination",
                },
            )
            .await?;
            tracing::error!(
                tags.alert = "TopupRefundDestinationSanctioned",
                "active sanctions list names a pending refund destination; operator review required"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_database;
    use axum::{
        Json, Router,
        extract::State,
        routing::{get, post},
    };
    use std::sync::Mutex;
    const XML: &str = include_str!("../tests/fixtures/sdn.xml");
    #[test]
    fn xml_retains_all_currencies_and_normalizes_across_tags() {
        let parsed = parse(XML.as_bytes()).unwrap();
        assert_eq!(parsed.count, 2);
        assert_eq!(parsed.addresses.len(), 5);
        assert_eq!(parsed.parse_errors, 1);
        let evm: Vec<_> = parsed.addresses.iter().filter_map(|a| a.evm).collect();
        assert_eq!(
            evm,
            vec![
                Address::repeat_byte(0xaa),
                Address::repeat_byte(0xbb),
                Address::repeat_byte(0xdd)
            ]
        );
        assert!(
            parsed
                .addresses
                .iter()
                .any(|a| a.raw == "bc1fixture" && a.evm.is_none())
        );
        assert!(parse(XML.replace("<Record_Count>2", "<Record_Count>3").as_bytes()).is_err());
        assert!(parse(XML.replace("</sdnList>", "").as_bytes()).is_err());
        assert!(parse(format!("{XML}<sdnList/>").as_bytes()).is_err());
        assert!(parse(XML.replace("<uid>200", "<uid>100").as_bytes()).is_err());
    }
    #[test]
    fn deny_first_verdict_truth_table() {
        for snapshot_hit in [false, true] {
            for manual_hit in [false, true] {
                for snapshot_fresh in [false, true] {
                    for reads_ok in [false, true] {
                        let expected = match (snapshot_hit, manual_hit, snapshot_fresh, reads_ok) {
                            (true, _, _, _) | (_, true, _, _) => SanctionsVerdict::Sanctioned,
                            (false, false, true, true) => SanctionsVerdict::Clear,
                            _ => SanctionsVerdict::Uncertain,
                        };
                        assert_eq!(
                            verdict(snapshot_hit, manual_hit, snapshot_fresh, reads_ok),
                            expected,
                            "{snapshot_hit}/{manual_hit}/{snapshot_fresh}/{reads_ok}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn freshness_configuration_is_positive_and_bounded() {
        assert_eq!(
            SanctionsConfig::default().duration().unwrap(),
            Duration::from_secs(86400)
        );
        for value in ["0h", "-1h", "24", "NaN", "18446744073709551615d"] {
            assert!(
                SanctionsConfig {
                    max_staleness: value.into()
                }
                .duration()
                .is_err()
            );
        }
    }
    #[derive(Clone)]
    struct PublicationState {
        xml: String,
        sha: String,
        malformed: bool,
    }
    async fn preview(State(state): State<Arc<Mutex<PublicationState>>>) -> Json<serde_json::Value> {
        let state = state.lock().unwrap();
        if state.malformed {
            return Json(serde_json::json!({"changed_contract":true}));
        }
        Json(
            serde_json::json!([{"fileName":"SDN.XML","hashCodes":{"SHA-256":state.sha},"lastUpdated":"2026-10-05T00:00:00Z"}]),
        )
    }
    async fn download(State(state): State<Arc<Mutex<PublicationState>>>) -> String {
        state.lock().unwrap().xml.clone()
    }
    struct Fixture {
        state: Arc<Mutex<PublicationState>>,
        origin: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Fixture {
        async fn new() -> Self {
            let state = Arc::new(Mutex::new(PublicationState {
                xml: XML.into(),
                sha: hex::encode(Sha256::digest(XML.as_bytes())),
                malformed: false,
            }));
            let app = Router::new()
                .route("/api/PublicationPreview/SdnList", post(preview))
                .route("/api/download/SDN.XML", get(download))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                state,
                origin,
                task,
            }
        }
        fn publish(&self, xml: String) {
            let mut state = self.state.lock().unwrap();
            state.sha = hex::encode(Sha256::digest(xml.as_bytes()));
            state.xml = xml;
            state.malformed = false;
        }
    }
    #[tokio::test]
    async fn fixture_refresh_is_atomic_and_failure_preserves_verification() -> anyhow::Result<()> {
        with_database(|db|Box::pin(async move {
            let fixture=Fixture::new().await;
            let refresh=Refresher::fixture(db.app_pool.clone(),&fixture.origin)?;
            let source=ListScreener::new(db.app_pool.clone(),Duration::from_secs(86400));
            assert_eq!(source.check(Address::repeat_byte(0x11),"deposit").await.verdict,SanctionsVerdict::Uncertain);
            assert_eq!(refresh.refresh().await?,Refresh::Activated);
            let first=active(&db.app_pool).await?.unwrap();
            assert_eq!(source.check(Address::repeat_byte(0xaa),"deposit").await.verdict,SanctionsVerdict::Sanctioned);
            assert_eq!(source.check(Address::repeat_byte(0x11),"deposit").await.verdict,SanctionsVerdict::Clear);
            fixture.state.lock().unwrap().sha="00".repeat(32);
            assert!(matches!(refresh.refresh().await,Err(RefreshError::Hash)));
            let unchanged=active(&db.app_pool).await?.unwrap();
            assert_eq!((unchanged.id,unchanged.verified_at),(first.id,first.verified_at));
            fixture.publish(XML.replace("<Record_Count>2","<Record_Count>3"));
            assert!(matches!(refresh.refresh().await,Err(RefreshError::Parse)));
            assert_eq!(active(&db.app_pool).await?.unwrap().verified_at,first.verified_at);
            fixture.publish(XML.replace("10/05/2026","10/04/2026"));
            assert!(matches!(refresh.refresh().await,Err(RefreshError::Parse)));
            fixture.state.lock().unwrap().malformed=true;
            assert!(matches!(refresh.refresh().await,Err(RefreshError::Fetch)));
            assert_eq!(active(&db.app_pool).await?.unwrap().verified_at,first.verified_at);
            fixture.publish(XML.into());
            assert_eq!(refresh.refresh().await?,Refresh::Unchanged);
            let same=active(&db.app_pool).await?.unwrap();
            assert_eq!(same.id,first.id);assert!(same.verified_at>first.verified_at);
            assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM sanctions_list_snapshots").fetch_one(&db.app_pool).await?,1);
            let next=XML.replace("10/05/2026","10/06/2026").replace("0xAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAa","0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
            fixture.publish(next);
            // Block a concurrent activation on the transaction lock: readers see the complete old set.
            let mut lock=db.app_pool.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('sanctions-list-refresh',0))").execute(&mut *lock).await?;
            let other=Refresher::fixture(db.app_pool.clone(),&fixture.origin)?;
            let task=tokio::spawn(async move {other.refresh().await});
            assert_eq!(source.check(Address::repeat_byte(0xaa),"deposit").await.verdict,SanctionsVerdict::Sanctioned);
            assert_eq!(source.check(Address::repeat_byte(0xee),"deposit").await.verdict,SanctionsVerdict::Clear);
            lock.commit().await?;
            assert_eq!(task.await??,Refresh::Activated);
            assert_eq!(source.check(Address::repeat_byte(0xaa),"deposit").await.verdict,SanctionsVerdict::Clear);
            assert_eq!(source.check(Address::repeat_byte(0xee),"deposit").await.verdict,SanctionsVerdict::Sanctioned);
            sqlx::query("UPDATE sanctions_list_snapshots SET verified_at=now()-interval '25 hours'").execute(&db.app_pool).await?;
            assert_eq!(source.check(Address::repeat_byte(0xee),"deposit").await.verdict,SanctionsVerdict::Sanctioned);
            assert_eq!(source.check(Address::repeat_byte(0x11),"deposit").await.verdict,SanctionsVerdict::Uncertain);
            // Manual deny also wins without any active snapshot.
            sqlx::query("INSERT INTO sanctions_manual_entries(evm_address,reason,source_ref,created_by) VALUES($1,'fixture','UK:fixture','fixture')").bind(Address::repeat_byte(0x11).as_slice()).execute(&db.app_pool).await?;
            sqlx::query("UPDATE sanctions_list_snapshots SET activated_at=NULL").execute(&db.owner_pool).await?;
            assert_eq!(source.check(Address::repeat_byte(0x11),"destination").await.verdict,SanctionsVerdict::Sanctioned);
            assert_eq!(source.check(Address::repeat_byte(0x22),"destination").await.verdict,SanctionsVerdict::Uncertain);
            // Fault injection: a failed manual-list read holds a fresh negative answer.
            sqlx::query("UPDATE sanctions_list_snapshots SET activated_at=now(),verified_at=now() WHERE id=$1").bind(first.id).execute(&db.owner_pool).await?;
            sqlx::query("REVOKE SELECT ON sanctions_manual_entries FROM topup_app").execute(&db.owner_pool).await?;
            assert_eq!(source.check(Address::repeat_byte(0x22),"deposit").await.verdict,SanctionsVerdict::Uncertain);
            assert_eq!(source.check(Address::repeat_byte(0xaa),"deposit").await.verdict,SanctionsVerdict::Sanctioned);
            sqlx::query("GRANT SELECT ON sanctions_manual_entries TO topup_app").execute(&db.owner_pool).await?;
            sqlx::query("REVOKE SELECT ON sanctions_list_snapshots FROM topup_app").execute(&db.owner_pool).await?;
            assert_eq!(source.check(Address::repeat_byte(0x11),"deposit").await.verdict,SanctionsVerdict::Sanctioned);
            assert_eq!(source.check(Address::repeat_byte(0x22),"deposit").await.verdict,SanctionsVerdict::Uncertain);
            Ok(())
        })).await
    }
}
