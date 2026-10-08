//! Pure sanctions, deposit-bound, and pause-scope screening rules.

use std::collections::BTreeSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::deposit::{RejectReason, RetryError, StepOutcome, WaitReason};
use crate::money::AtomicAmount;

/// Decision-time sanctions verdict from verified lists.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SanctionsVerdict {
    /// An active snapshot or manual entry names the address, even if stale.
    Sanctioned,
    /// No hit, a fresh active snapshot, and a successful manual-list read.
    Clear,
    /// No usable fresh snapshot or a failed list read; hold and retry.
    Uncertain,
}

/// Evidence of the verified snapshot used at decision time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SanctionsProvenance {
    /// Active snapshot identifier, absent before initial verification.
    pub snapshot_id: Option<uuid::Uuid>,
    /// SHA-256 of the exact publication bytes.
    pub sha256: Option<alloy_primitives::B256>,
    /// OFAC publication date in ISO format.
    pub publish_date: Option<String>,
    /// Last successful verification, Unix seconds.
    pub verified_at: Option<i64>,
    /// Whether an active operator entry matched.
    pub manual_hit: bool,
    /// Decision time, Unix seconds.
    pub screened_at: i64,
}

/// A single verdict with optional provenance (test and deprecated oracle sources omit it).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SanctionsResult {
    /// The list verdict.
    pub verdict: SanctionsVerdict,
    /// Verified-list provenance.
    pub provenance: Option<SanctionsProvenance>,
}

impl SanctionsResult {
    /// Creates a verdict without snapshot evidence, for injectable sources.
    #[must_use]
    pub const fn new(verdict: SanctionsVerdict) -> Self {
        Self {
            verdict,
            provenance: None,
        }
    }
}

/// Inclusive per-deposit amount bounds, from the terms that govern the deposit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bounds {
    /// The minimum accepted atomic amount.
    pub min_atomic: AtomicAmount,
    /// The maximum accepted atomic amount.
    pub max_atomic: AtomicAmount,
}

/// A runtime operation that can be paused independently.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseScope {
    /// Creating new rate quotes.
    Quotes,
    /// Starting new product settlements.
    Settlement,
    /// Processing refund requests.
    Refunds,
}

impl PauseScope {
    /// Returns the stable text code used by APIs and persistence.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Quotes => "quotes",
            Self::Settlement => "settlement",
            Self::Refunds => "refunds",
        }
    }
}

impl FromStr for PauseScope {
    type Err = ParsePauseScopeError;

    fn from_str(code: &str) -> Result<Self, Self::Err> {
        match code {
            "quotes" => Ok(Self::Quotes),
            "settlement" => Ok(Self::Settlement),
            "refunds" => Ok(Self::Refunds),
            _ => Err(ParsePauseScopeError {
                code: code.to_owned(),
            }),
        }
    }
}

/// A set of independently paused runtime operations.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PauseScopes(BTreeSet<PauseScope>);

impl PauseScopes {
    /// Parses a set from stable text codes, rejecting every unknown code.
    pub fn from_codes<I, S>(codes: I) -> Result<Self, ParsePauseScopeError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let scopes = codes
            .into_iter()
            .map(|code| PauseScope::from_str(code.as_ref()))
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(Self(scopes))
    }

    /// Returns whether the supplied operation is paused.
    #[must_use]
    pub fn contains(&self, scope: PauseScope) -> bool {
        self.0.contains(&scope)
    }
}

/// An unknown pause-scope text code.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("unknown pause scope `{code}`")]
pub struct ParsePauseScopeError {
    code: String,
}

impl ParsePauseScopeError {
    /// Returns the rejected text code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }
}

/// Screens a confirmed deposit for direct sanctions, amount bounds, and settlement pauses.
///
/// Checks are deliberately ordered as sanctions, bounds, then pause. A sanctions hit therefore
/// rejects even when the amount is out of bounds or settlement is paused, and an out-of-bounds
/// deposit rejects instead of waiting behind a pause. Only the `settlement` pause scope gates this
/// step because it holds a deposit before crediting; the other scopes govern their own
/// operations.
///
/// A positive hit rejects; an uncertain answer retries; only clear proceeds.
#[must_use]
pub fn screen(
    amount: AtomicAmount,
    sanctions: &SanctionsResult,
    bounds: &Bounds,
    customer_scopes: &PauseScopes,
    account_scopes: &PauseScopes,
) -> StepOutcome {
    match sanctions.verdict {
        SanctionsVerdict::Sanctioned => return StepOutcome::Reject(RejectReason::Sanctioned),
        SanctionsVerdict::Uncertain => {
            return StepOutcome::Retry {
                error: RetryError::SanctionsInconclusive,
            };
        }
        SanctionsVerdict::Clear => {}
    }

    if amount < bounds.min_atomic || amount > bounds.max_atomic {
        return StepOutcome::Reject(RejectReason::OutOfBounds);
    }

    if customer_scopes.contains(PauseScope::Settlement)
        || account_scopes.contains(PauseScope::Settlement)
    {
        return StepOutcome::Wait {
            reason: WaitReason::Paused,
        };
    }

    StepOutcome::Advance
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    fn amount(value: u32) -> AtomicAmount {
        AtomicAmount::new(U256::from(value))
    }

    fn sanctions(verdict: SanctionsVerdict) -> SanctionsResult {
        SanctionsResult::new(verdict)
    }

    fn bounds() -> Bounds {
        Bounds {
            min_atomic: amount(10),
            max_atomic: amount(20),
        }
    }

    fn scopes(codes: &[&str]) -> PauseScopes {
        PauseScopes::from_codes(codes.iter().copied()).expect("test scope codes must parse")
    }

    #[test]
    fn pause_scopes_parse_exact_codes_into_a_set() {
        let scopes = PauseScopes::from_codes(["quotes", "settlement", "refunds", "settlement"])
            .expect("documented scope codes must parse");

        assert!(scopes.contains(PauseScope::Quotes));
        assert!(scopes.contains(PauseScope::Settlement));
        assert!(scopes.contains(PauseScope::Refunds));
    }

    #[test]
    fn pause_scopes_reject_unknown_text_codes() {
        // `addresses` paused persistent-address issuance and `flush` the operator flusher; neither
        // exists any more.
        assert!(PauseScopes::from_codes(["addresses"]).is_err());
        assert!(PauseScopes::from_codes(["flush"]).is_err());
        let error = PauseScopes::from_codes(["settlement", "payments"])
            .expect_err("unknown scope must fail parsing");

        assert_eq!(error.code(), "payments");
        assert_eq!(error.to_string(), "unknown pause scope `payments`");
    }

    #[test]
    fn pause_scopes_serde_round_trip_uses_exact_codes() {
        let scopes = PauseScopes::from_codes(["refunds", "settlement", "quotes"])
            .expect("documented scope codes must parse");

        let json = serde_json::to_string(&scopes).expect("pause scopes must serialize");
        assert_eq!(json, r#"["quotes","settlement","refunds"]"#);
        assert_eq!(
            serde_json::from_str::<PauseScopes>(&json).expect("pause scopes must deserialize"),
            scopes
        );
    }

    #[test]
    fn pause_scopes_serde_rejects_unknown_codes() {
        let error = serde_json::from_str::<PauseScopes>(r#"["settlement","payments"]"#)
            .expect_err("unknown scope must fail deserialization");

        assert!(error.to_string().contains("unknown variant `payments`"));
    }

    #[test]
    fn sanctions_verdict_truth_table() {
        for (verdict, expected) in [
            (
                SanctionsVerdict::Sanctioned,
                StepOutcome::Reject(RejectReason::Sanctioned),
            ),
            (
                SanctionsVerdict::Uncertain,
                StepOutcome::Retry {
                    error: RetryError::SanctionsInconclusive,
                },
            ),
            (SanctionsVerdict::Clear, StepOutcome::Advance),
        ] {
            assert_eq!(
                screen(
                    amount(15),
                    &sanctions(verdict),
                    &bounds(),
                    &PauseScopes::default(),
                    &PauseScopes::default()
                ),
                expected
            );
        }
    }

    #[test]
    fn deposit_bounds_are_inclusive() {
        let sanctions = sanctions(SanctionsVerdict::Clear);
        let bounds = bounds();
        let no_pauses = PauseScopes::default();
        let cases = [
            (amount(10), StepOutcome::Advance),
            (amount(20), StepOutcome::Advance),
            (amount(9), StepOutcome::Reject(RejectReason::OutOfBounds)),
            (amount(21), StepOutcome::Reject(RejectReason::OutOfBounds)),
        ];

        for (amount, expected) in cases {
            assert_eq!(
                screen(amount, &sanctions, &bounds, &no_pauses, &no_pauses),
                expected
            );
        }
    }

    #[test]
    fn settlement_pause_on_customer_or_account_waits() {
        let sanctions = sanctions(SanctionsVerdict::Clear);
        let bounds = bounds();
        let active = PauseScopes::default();
        let paused = scopes(&["settlement"]);
        let wait = StepOutcome::Wait {
            reason: WaitReason::Paused,
        };
        let cases = [
            (&active, &active, StepOutcome::Advance),
            (&paused, &active, wait),
            (&active, &paused, wait),
            (&paused, &paused, wait),
        ];

        for (customer_scopes, account_scopes, expected) in cases {
            assert_eq!(
                screen(
                    amount(15),
                    &sanctions,
                    &bounds,
                    customer_scopes,
                    account_scopes,
                ),
                expected
            );
        }
    }

    #[test]
    fn non_settlement_pause_scopes_do_not_gate_screening() {
        let non_settlement = scopes(&["quotes", "refunds"]);

        assert_eq!(
            screen(
                amount(15),
                &sanctions(SanctionsVerdict::Clear),
                &bounds(),
                &non_settlement,
                &non_settlement,
            ),
            StepOutcome::Advance
        );
    }

    #[test]
    fn sanctions_are_evaluated_before_bounds_and_pause() {
        assert_eq!(
            screen(
                amount(9),
                &sanctions(SanctionsVerdict::Sanctioned),
                &bounds(),
                &scopes(&["settlement"]),
                &scopes(&["settlement"]),
            ),
            StepOutcome::Reject(RejectReason::Sanctioned)
        );
    }

    #[test]
    fn inconclusive_sanctions_are_evaluated_before_bounds_and_pause() {
        assert_eq!(
            screen(
                amount(9),
                &sanctions(SanctionsVerdict::Uncertain),
                &bounds(),
                &scopes(&["settlement"]),
                &scopes(&["settlement"]),
            ),
            StepOutcome::Retry {
                error: RetryError::SanctionsInconclusive,
            }
        );
    }

    #[test]
    fn bounds_are_evaluated_before_pause() {
        assert_eq!(
            screen(
                amount(21),
                &sanctions(SanctionsVerdict::Clear),
                &bounds(),
                &scopes(&["settlement"]),
                &scopes(&["settlement"]),
            ),
            StepOutcome::Reject(RejectReason::OutOfBounds)
        );
    }
}
