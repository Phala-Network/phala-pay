use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Stable reconciliation check identifier used by findings and alerts.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckName {
    /// Stored credit disagreed with deterministic recomputation.
    CreditRecomputation,
    /// A deposit was not linked to a later confirmed flush.
    MissingFlushLink,
    /// Custody balances or persisted flush totals disagreed with chain state.
    CustodyBalance,
    /// The factory-derived address disagreed with stored address data.
    AddressDerivation,
}

impl CheckName {
    /// Every check, in metric registration order.
    pub const ALL: [Self; 4] = [
        Self::CreditRecomputation,
        Self::MissingFlushLink,
        Self::CustodyBalance,
        Self::AddressDerivation,
    ];

    /// Returns the stable metric label value.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::CreditRecomputation => "credit_recomputation",
            Self::MissingFlushLink => "missing_flush_link",
            Self::CustodyBalance => "custody_balance",
            Self::AddressDerivation => "address_derivation",
        }
    }
}

/// One typed reconciliation observation and its repair outcome.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    /// Stable identifier derived from the complete finding fingerprint.
    pub id: Uuid,
    /// Reconciliation check which produced the finding.
    pub check: CheckName,
    /// Stable subject identifiers such as chain, address, or deposit ids.
    pub subjects: BTreeMap<String, String>,
    /// Expected state encoded without lossy numeric conversions.
    pub expected: Value,
    /// Observed state encoded without lossy numeric conversions.
    pub observed: Value,
    /// Whether the exact safe repair allowed by the specification was applied.
    pub repair_applied: bool,
    /// Whether post-restore reconciliation must prevent service resumption.
    pub incomplete: bool,
    pub(crate) fingerprint: String,
}

impl Finding {
    pub(crate) fn new(
        check: CheckName,
        subjects: BTreeMap<String, String>,
        expected: Value,
        observed: Value,
        repair_applied: bool,
        incomplete: bool,
    ) -> Result<Self, serde_json::Error> {
        let material = serde_json::to_vec(&(
            check,
            &subjects,
            &expected,
            &observed,
            repair_applied,
            incomplete,
        ))?;
        let fingerprint = hex::encode(Sha256::digest(material));
        let id = Uuid::new_v5(&Uuid::NAMESPACE_OID, fingerprint.as_bytes());
        Ok(Self {
            id,
            check,
            subjects,
            expected,
            observed,
            repair_applied,
            incomplete,
            fingerprint,
        })
    }
}

/// Aggregate result of one reconciliation pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReconciliationReport {
    /// Findings observed during this pass, including already-deduplicated durable findings.
    pub findings: Vec<Finding>,
    /// Checks which could not complete; every other check still ran.
    pub failed_checks: Vec<CheckName>,
    /// The error of each check in `failed_checks`, in the same order.
    pub check_errors: Vec<String>,
    /// Whether a post-restore caller must keep the service stopped.
    ///
    /// Only the post-restore product lookups set this flag; alert-only findings never do.
    pub incomplete: bool,
}

impl ReconciliationReport {
    /// Returns whether every check completed.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.failed_checks.is_empty()
    }
}
