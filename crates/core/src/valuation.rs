//! Pure price validation and deposit valuation rules.

use std::error::Error;
use std::fmt;

use alloy_primitives::{Address, U512};

use crate::money::{AtomicAmount, Bps, CreditError, MinorAmount, PRICE_SCALE, ScaledPrice, credit};
use crate::route::{AssetConfig, PricingConfig, UNIT_DECIMALS};

const BASIS_POINTS: u128 = 10_000;
const PRICE_SCALE_FACTOR: u64 = 100_000_000;

/// A price-provider identifier.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(String);

impl SourceId {
    /// Creates a source identifier.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the source identifier as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A Unix timestamp measured in whole seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixSeconds(u64);

impl UnixSeconds {
    /// Creates a Unix timestamp.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the timestamp as seconds since the Unix epoch.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// A timestamped asset-price observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// Provider that produced the observation.
    pub source: SourceId,
    /// Observed price at the service price scale.
    pub price: ScaledPrice,
    /// Provider observation time.
    pub observed_at: UnixSeconds,
}

/// A timestamped USDT/USD foreign-exchange observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FxObservation {
    /// Provider that produced the observation.
    pub source: SourceId,
    /// Observed USDT/USD rate at the service price scale.
    pub rate: ScaledPrice,
    /// Provider observation time.
    pub observed_at: UnixSeconds,
}

/// Price freshness and divergence limits derived from route pricing configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValuationPolicy {
    /// Maximum accepted observation age in seconds.
    pub max_age_s: u64,
    /// Maximum asset-price divergence in basis points.
    pub max_deviation_bps: Bps,
    /// Maximum USDT/USD divergence from one dollar in basis points.
    pub max_fx_deviation_bps: Bps,
}

impl From<&PricingConfig> for ValuationPolicy {
    fn from(config: &PricingConfig) -> Self {
        Self {
            max_age_s: config.max_age_s,
            max_deviation_bps: config.max_deviation_bps,
            max_fx_deviation_bps: config.max_fx_deviation_bps.unwrap_or_default(),
        }
    }
}

/// What values a deposit: its route's asset, and the terms that govern it (the paid quote's, or
/// those of the payment settings the deposit is bound to; design payment-settings §6).
#[derive(Clone, Copy, Debug)]
pub struct RouteValuation<'route> {
    /// Deposited asset settings.
    pub asset: &'route AssetConfig,
    /// The minimum credit, in cents, of a deposit valued at spot.
    pub min_credit_minor: u64,
    /// The two-sided tolerance of a quote's payment.
    pub lock_tolerance_bps: Bps,
}

/// A rate lock considered for one observed deposit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockTerms {
    /// Asset for which the lock was created.
    pub asset: Address,
    /// Exact atomic amount quoted by the lock.
    pub amount: AtomicAmount,
    /// Locked asset price.
    pub price: ScaledPrice,
    /// Frozen destination credit shown when the lock was created.
    pub credit_minor: MinorAmount,
    /// Last accepted block timestamp.
    pub expires_at: UnixSeconds,
    /// Timestamp of the deposit's block.
    pub block_time: UnixSeconds,
}

/// Price source selected for a deposit valuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValuationSource {
    /// Current validated spot price.
    Spot,
    /// Eligible quote-first rate lock.
    Lock,
}

/// Completed deposit valuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Valuation {
    /// Price used to compute credit.
    pub price: ScaledPrice,
    /// Whether spot or lock pricing was selected.
    pub source: ValuationSource,
    /// Destination credit in minor units.
    pub credit_minor: MinorAmount,
}

/// A price-validation or deposit-valuation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValuationError {
    /// A required observation was older than the policy limit or from the future.
    Stale {
        /// Provider whose observation failed freshness validation.
        source: SourceId,
    },
    /// Primary and normalized check prices exceeded the allowed ratio deviation.
    Divergent {
        /// Actual deviation, rounded up to the next whole basis point.
        deviation_bps: u128,
    },
    /// The USDT/USD rate exceeded its allowed deviation from one dollar.
    FxDepeg,
    /// The USDT-quoted check price did not include its required USDT/USD observation.
    FxMissing,
    /// A stablecoin reference rate exceeded its allowed deviation from one dollar.
    Depeg,
    /// Spot-priced credit was below the route minimum.
    BelowMinimum,
    /// Checked fixed-point arithmetic could not represent the result.
    ArithmeticOutOfRange,
    /// The shared credit calculation rejected the amount or scale.
    Credit(CreditError),
}

impl fmt::Display for ValuationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale { source } => write!(formatter, "stale price observation from {source}"),
            Self::Divergent { deviation_bps } => {
                write!(
                    formatter,
                    "price observations diverge by {deviation_bps} basis points"
                )
            }
            Self::FxDepeg => formatter.write_str("USDT/USD rate is outside its allowed range"),
            Self::FxMissing => formatter.write_str("USDT/USD observation is required"),
            Self::Depeg => {
                formatter.write_str("stablecoin reference rate is outside its peg range")
            }
            Self::BelowMinimum => {
                formatter.write_str("spot-priced credit is below the route minimum")
            }
            Self::ArithmeticOutOfRange => {
                formatter.write_str("valuation arithmetic is out of range")
            }
            Self::Credit(error) => write!(formatter, "credit calculation failed: {error}"),
        }
    }
}

impl Error for ValuationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Credit(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CreditError> for ValuationError {
    fn from(error: CreditError) -> Self {
        Self::Credit(error)
    }
}

/// Validates primary USD and USDT-quoted check observations and returns the primary price.
///
/// The FX leg is freshness-checked, guarded against a USDT depeg, and multiplied into the check
/// price with nearest, ties-to-even rounding at the service's eight-decimal price scale.
pub fn validate_spot(
    primary: &Observation,
    check: &Observation,
    fx: Option<&FxObservation>,
    now: UnixSeconds,
    policy: ValuationPolicy,
) -> Result<ScaledPrice, ValuationError> {
    ensure_fresh(&primary.source, primary.observed_at, now, policy.max_age_s)?;
    ensure_fresh(&check.source, check.observed_at, now, policy.max_age_s)?;

    let fx = fx.ok_or(ValuationError::FxMissing)?;
    ensure_fresh(&fx.source, fx.observed_at, now, policy.max_age_s)?;
    if !within_deviation(
        PRICE_SCALE_FACTOR,
        fx.rate.value(),
        policy.max_fx_deviation_bps,
    )? {
        return Err(ValuationError::FxDepeg);
    }

    let normalized_check = multiply_scaled(check.price, fx.rate)?;
    if !within_deviation(
        primary.price.value(),
        normalized_check.value(),
        policy.max_deviation_bps,
    )? {
        return Err(ValuationError::Divergent {
            deviation_bps: deviation_bps_ceil(primary.price.value(), normalized_check.value())?,
        });
    }

    Ok(primary.price)
}

/// Returns the fixed one-dollar stablecoin price after validating a fresh reference rate.
pub fn stablecoin_price(
    reference: &Observation,
    now: UnixSeconds,
    policy: ValuationPolicy,
) -> Result<ScaledPrice, ValuationError> {
    ensure_fresh(
        &reference.source,
        reference.observed_at,
        now,
        policy.max_age_s,
    )?;
    if !within_deviation(
        PRICE_SCALE_FACTOR,
        reference.price.value(),
        policy.max_deviation_bps,
    )? {
        return Err(ValuationError::Depeg);
    }
    ScaledPrice::new(PRICE_SCALE_FACTOR, PRICE_SCALE)
        .map_err(|_| ValuationError::ArithmeticOutOfRange)
}

/// Returns frozen lock credit when eligible, otherwise computes and validates spot credit.
pub fn value_deposit(
    amount: AtomicAmount,
    spot_price: ScaledPrice,
    route: &RouteValuation<'_>,
    lock: Option<&LockTerms>,
) -> Result<Valuation, ValuationError> {
    if let Some(terms) = lock.filter(|terms| lock_applies(amount, route, terms)) {
        return Ok(Valuation {
            price: terms.price,
            source: ValuationSource::Lock,
            credit_minor: terms.credit_minor,
        });
    }

    let credit_minor = credit(amount, spot_price, route.asset.decimals, UNIT_DECIMALS)?;
    if credit_minor.value() < route.min_credit_minor {
        return Err(ValuationError::BelowMinimum);
    }
    Ok(Valuation {
        price: spot_price,
        source: ValuationSource::Spot,
        credit_minor,
    })
}

fn ensure_fresh(
    source: &SourceId,
    observed_at: UnixSeconds,
    now: UnixSeconds,
    max_age_s: u64,
) -> Result<(), ValuationError> {
    let age = now
        .0
        .checked_sub(observed_at.0)
        .ok_or_else(|| ValuationError::Stale {
            source: source.clone(),
        })?;
    if age > max_age_s {
        return Err(ValuationError::Stale {
            source: source.clone(),
        });
    }
    Ok(())
}

fn within_deviation(reference: u64, candidate: u64, maximum: Bps) -> Result<bool, ValuationError> {
    let difference = reference.abs_diff(candidate);
    let left = u128::from(difference)
        .checked_mul(BASIS_POINTS)
        .ok_or(ValuationError::ArithmeticOutOfRange)?;
    let right = u128::from(reference)
        .checked_mul(u128::from(maximum.value()))
        .ok_or(ValuationError::ArithmeticOutOfRange)?;
    Ok(left <= right)
}

fn deviation_bps_ceil(reference: u64, candidate: u64) -> Result<u128, ValuationError> {
    let numerator = u128::from(reference.abs_diff(candidate))
        .checked_mul(BASIS_POINTS)
        .ok_or(ValuationError::ArithmeticOutOfRange)?;
    let denominator = u128::from(reference);
    let quotient = numerator
        .checked_div(denominator)
        .ok_or(ValuationError::ArithmeticOutOfRange)?;
    let remainder = numerator
        .checked_rem(denominator)
        .ok_or(ValuationError::ArithmeticOutOfRange)?;
    if remainder == 0 {
        Ok(quotient)
    } else {
        quotient
            .checked_add(1)
            .ok_or(ValuationError::ArithmeticOutOfRange)
    }
}

fn multiply_scaled(first: ScaledPrice, second: ScaledPrice) -> Result<ScaledPrice, ValuationError> {
    let numerator = u128::from(first.value())
        .checked_mul(u128::from(second.value()))
        .ok_or(ValuationError::ArithmeticOutOfRange)?;
    let denominator = u128::from(PRICE_SCALE_FACTOR);
    let quotient = numerator
        .checked_div(denominator)
        .ok_or(ValuationError::ArithmeticOutOfRange)?;
    let remainder = numerator
        .checked_rem(denominator)
        .ok_or(ValuationError::ArithmeticOutOfRange)?;
    let twice_remainder = remainder
        .checked_mul(2)
        .ok_or(ValuationError::ArithmeticOutOfRange)?;
    let quotient_is_odd = quotient
        .checked_rem(2)
        .ok_or(ValuationError::ArithmeticOutOfRange)?
        == 1;
    let round_up =
        twice_remainder > denominator || (twice_remainder == denominator && quotient_is_odd);
    let rounded = if round_up {
        quotient
            .checked_add(1)
            .ok_or(ValuationError::ArithmeticOutOfRange)?
    } else {
        quotient
    };
    let value = u64::try_from(rounded).map_err(|_| ValuationError::ArithmeticOutOfRange)?;
    ScaledPrice::new(value, PRICE_SCALE).map_err(|_| ValuationError::ArithmeticOutOfRange)
}

/// Whether a payment of `amount` is a valid payment of `lock`: its asset, in a block at or before
/// its expiry, and within its two-sided tolerance.
#[must_use]
pub fn lock_applies(amount: AtomicAmount, route: &RouteValuation<'_>, lock: &LockTerms) -> bool {
    lock.block_time <= lock.expires_at
        && lock.asset == route.asset.contract
        && amount_within_tolerance(amount, lock.amount, route.lock_tolerance_bps)
}

/// Whether `actual` is within `tolerance` of `locked`: `|actual − locked| × 10 000 ≤ locked × bps`.
#[must_use]
pub fn amount_within_tolerance(actual: AtomicAmount, locked: AtomicAmount, tolerance: Bps) -> bool {
    let difference = if actual >= locked {
        actual.checked_sub(locked)
    } else {
        locked.checked_sub(actual)
    };
    let Some(difference) = difference else {
        return false;
    };
    let left = U512::from(difference.value()).checked_mul(U512::from(10_000_u16));
    let right = U512::from(locked.value()).checked_mul(U512::from(tolerance.value()));
    matches!((left, right), (Some(left), Some(right)) if left <= right)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, U256};
    use proptest::prelude::*;

    use super::*;
    use crate::route::{PricingConfig, PricingMode};

    fn price(value: u64) -> ScaledPrice {
        ScaledPrice::new(value, PRICE_SCALE).expect("test price is valid")
    }

    fn bps(value: u16) -> Bps {
        Bps::new(value).expect("test basis points are valid")
    }

    fn observation(source: &str, value: u64, observed_at: u64) -> Observation {
        Observation {
            source: SourceId::new(source),
            price: price(value),
            observed_at: UnixSeconds::new(observed_at),
        }
    }

    fn fx(value: u64, observed_at: u64) -> FxObservation {
        FxObservation {
            source: SourceId::new("fx"),
            rate: price(value),
            observed_at: UnixSeconds::new(observed_at),
        }
    }

    fn policy(max_age_s: u64, deviation: u16, fx_deviation: u16) -> ValuationPolicy {
        ValuationPolicy {
            max_age_s,
            max_deviation_bps: bps(deviation),
            max_fx_deviation_bps: bps(fx_deviation),
        }
    }

    struct TestRoute {
        asset: AssetConfig,
        min_credit_minor: u64,
        tolerance_bps: Bps,
    }

    impl TestRoute {
        fn new(asset: Address, min_credit_minor: u64, tolerance_bps: u16) -> Self {
            Self {
                asset: AssetConfig {
                    symbol: "pha".to_owned(),
                    contract: asset,
                    decimals: 18,
                    quote_amount_decimals: 4,
                },
                min_credit_minor,
                tolerance_bps: bps(tolerance_bps),
            }
        }

        fn valuation(&self) -> RouteValuation<'_> {
            RouteValuation {
                asset: &self.asset,
                min_credit_minor: self.min_credit_minor,
                lock_tolerance_bps: self.tolerance_bps,
            }
        }
    }

    #[test]
    fn spot_boundaries_and_direction_are_table_driven() {
        struct Case {
            name: &'static str,
            primary: u64,
            check: u64,
            now: u64,
            observed_at: u64,
            maximum_bps: u16,
            expected: Result<u64, ValuationError>,
        }

        let cases = [
            Case {
                name: "age exactly max_age",
                primary: 100_000_000,
                check: 100_000_000,
                now: 220,
                observed_at: 100,
                maximum_bps: 100,
                expected: Ok(100_000_000),
            },
            Case {
                name: "positive deviation exactly at bound",
                primary: 100_000_000,
                check: 101_000_000,
                now: 100,
                observed_at: 100,
                maximum_bps: 100,
                expected: Ok(100_000_000),
            },
            Case {
                name: "negative deviation exactly at bound",
                primary: 100_000_000,
                check: 99_000_000,
                now: 100,
                observed_at: 100,
                maximum_bps: 100,
                expected: Ok(100_000_000),
            },
            Case {
                name: "primary denominator rejects larger check",
                primary: 100_000_000,
                check: 110_000_000,
                now: 100,
                observed_at: 100,
                maximum_bps: 950,
                expected: Err(ValuationError::Divergent {
                    deviation_bps: 1_000,
                }),
            },
            Case {
                name: "swapped primary denominator accepts same gap",
                primary: 110_000_000,
                check: 100_000_000,
                now: 100,
                observed_at: 100,
                maximum_bps: 950,
                expected: Ok(110_000_000),
            },
        ];

        for case in cases {
            let primary = observation("primary", case.primary, case.observed_at);
            let check = observation("check", case.check, case.observed_at);
            let fx = fx(PRICE_SCALE_FACTOR, case.observed_at);
            let actual = validate_spot(
                &primary,
                &check,
                Some(&fx),
                UnixSeconds::new(case.now),
                policy(120, case.maximum_bps, 50),
            )
            .map(ScaledPrice::value);
            assert_eq!(actual, case.expected, "{}", case.name);
        }
    }

    #[test]
    fn spot_errors_are_table_driven() {
        enum FxCase {
            Missing,
            Present { value: u64, observed_at: u64 },
        }

        struct Case {
            name: &'static str,
            primary_at: u64,
            check_at: u64,
            check_price: u64,
            fx: FxCase,
            expected: ValuationError,
        }

        let cases = [
            Case {
                name: "stale primary",
                primary_at: 79,
                check_at: 100,
                check_price: 100_000_000,
                fx: FxCase::Present {
                    value: PRICE_SCALE_FACTOR,
                    observed_at: 100,
                },
                expected: ValuationError::Stale {
                    source: SourceId::new("primary"),
                },
            },
            Case {
                name: "stale check",
                primary_at: 100,
                check_at: 79,
                check_price: 100_000_000,
                fx: FxCase::Present {
                    value: PRICE_SCALE_FACTOR,
                    observed_at: 100,
                },
                expected: ValuationError::Stale {
                    source: SourceId::new("check"),
                },
            },
            Case {
                name: "stale fx",
                primary_at: 100,
                check_at: 100,
                check_price: 100_000_000,
                fx: FxCase::Present {
                    value: PRICE_SCALE_FACTOR,
                    observed_at: 79,
                },
                expected: ValuationError::Stale {
                    source: SourceId::new("fx"),
                },
            },
            Case {
                name: "missing fx",
                primary_at: 100,
                check_at: 100,
                check_price: 100_000_000,
                fx: FxCase::Missing,
                expected: ValuationError::FxMissing,
            },
            Case {
                name: "fx depeg",
                primary_at: 100,
                check_at: 100,
                check_price: 100_000_000,
                fx: FxCase::Present {
                    value: 100_500_001,
                    observed_at: 100,
                },
                expected: ValuationError::FxDepeg,
            },
            Case {
                name: "divergent price",
                primary_at: 100,
                check_at: 100,
                check_price: 101_000_001,
                fx: FxCase::Present {
                    value: PRICE_SCALE_FACTOR,
                    observed_at: 100,
                },
                expected: ValuationError::Divergent { deviation_bps: 101 },
            },
        ];

        for case in cases {
            let primary = observation("primary", 100_000_000, case.primary_at);
            let check = observation("check", case.check_price, case.check_at);
            let fx = match case.fx {
                FxCase::Missing => None,
                FxCase::Present { value, observed_at } => Some(fx(value, observed_at)),
            };
            assert_eq!(
                validate_spot(
                    &primary,
                    &check,
                    fx.as_ref(),
                    UnixSeconds::new(100),
                    policy(20, 100, 50),
                ),
                Err(case.expected),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn fx_exactly_at_each_bound_is_accepted() {
        for rate in [99_500_000, 100_500_000] {
            let primary = observation("primary", rate, 100);
            let check = observation("check", 100_000_000, 100);
            assert_eq!(
                validate_spot(
                    &primary,
                    &check,
                    Some(&fx(rate, 100)),
                    UnixSeconds::new(100),
                    policy(20, 0, 50),
                ),
                Ok(price(rate))
            );
        }
    }

    #[test]
    fn fx_normalization_uses_half_even_rounding() {
        assert_eq!(
            multiply_scaled(price(50_000_000), price(3)).unwrap(),
            price(2)
        );
        assert_eq!(
            multiply_scaled(price(50_000_000), price(5)).unwrap(),
            price(2)
        );
        assert_eq!(
            multiply_scaled(price(50_000_000), price(7)).unwrap(),
            price(4)
        );
    }

    #[test]
    fn stablecoin_boundaries_and_errors_are_table_driven() {
        struct Case {
            name: &'static str,
            rate: u64,
            observed_at: u64,
            expected: Result<u64, ValuationError>,
        }

        let cases = [
            Case {
                name: "lower bound",
                rate: 99_000_000,
                observed_at: 100,
                expected: Ok(PRICE_SCALE_FACTOR),
            },
            Case {
                name: "upper bound",
                rate: 101_000_000,
                observed_at: 100,
                expected: Ok(PRICE_SCALE_FACTOR),
            },
            Case {
                name: "depeg",
                rate: 101_000_001,
                observed_at: 100,
                expected: Err(ValuationError::Depeg),
            },
            Case {
                name: "stale reference",
                rate: 100_000_000,
                observed_at: 79,
                expected: Err(ValuationError::Stale {
                    source: SourceId::new("reference"),
                }),
            },
        ];

        for case in cases {
            let reference = observation("reference", case.rate, case.observed_at);
            let actual = stablecoin_price(&reference, UnixSeconds::new(100), policy(20, 100, 50))
                .map(ScaledPrice::value);
            assert_eq!(actual, case.expected, "{}", case.name);
        }
    }

    #[test]
    fn future_observations_are_rejected() {
        let primary = observation("primary", 100_000_000, 101);
        let check = observation("check", 100_000_000, 100);
        assert_eq!(
            validate_spot(
                &primary,
                &check,
                Some(&fx(PRICE_SCALE_FACTOR, 100)),
                UnixSeconds::new(100),
                policy(20, 100, 50),
            ),
            Err(ValuationError::Stale {
                source: SourceId::new("primary")
            })
        );
    }

    #[test]
    fn deposit_valuation_selects_lock_only_when_all_conditions_hold() {
        let asset = Address::from([1_u8; 20]);
        let route = TestRoute::new(asset, 0, 100);
        let amount = AtomicAmount::new(U256::from(1_000_u64));
        let base = LockTerms {
            asset,
            amount,
            price: price(50_000_000),
            credit_minor: MinorAmount::new(5_000),
            expires_at: UnixSeconds::new(200),
            block_time: UnixSeconds::new(200),
        };
        let cases = [
            ("all conditions", base, ValuationSource::Lock),
            (
                "late",
                LockTerms {
                    block_time: UnixSeconds::new(201),
                    ..base
                },
                ValuationSource::Spot,
            ),
            (
                "wrong asset",
                LockTerms {
                    asset: Address::from([2_u8; 20]),
                    ..base
                },
                ValuationSource::Spot,
            ),
            (
                "outside amount tolerance",
                LockTerms {
                    amount: AtomicAmount::new(U256::from(989_u64)),
                    ..base
                },
                ValuationSource::Spot,
            ),
        ];

        for (name, lock, expected) in cases {
            let valuation =
                value_deposit(amount, price(40_000_000), &route.valuation(), Some(&lock))
                    .expect("bounded valuation succeeds");
            assert_eq!(valuation.source, expected, "{name}");
        }
    }

    #[test]
    fn amount_exactly_at_lock_tolerance_is_accepted() {
        let asset = Address::from([1_u8; 20]);
        let mut route = TestRoute::new(asset, 0, 100);
        route.asset.decimals = 2;
        let lock = LockTerms {
            asset,
            amount: AtomicAmount::new(U256::from(1_000_u64)),
            price: price(50_000_000),
            credit_minor: MinorAmount::new(1_234),
            expires_at: UnixSeconds::new(200),
            block_time: UnixSeconds::new(200),
        };

        for amount in [990_u64, 1_010_u64] {
            let valuation = value_deposit(
                AtomicAmount::new(U256::from(amount)),
                price(40_000_000),
                &route.valuation(),
                Some(&lock),
            )
            .expect("boundary amount values successfully");
            assert_eq!(valuation.source, ValuationSource::Lock);
            assert_eq!(valuation.price, lock.price);
            assert_eq!(valuation.credit_minor, lock.credit_minor);
        }
    }

    #[test]
    fn amount_outside_lock_tolerance_falls_back_to_spot() {
        let asset = Address::from([1_u8; 20]);
        let mut route = TestRoute::new(asset, 0, 100);
        route.asset.decimals = 2;
        let lock = LockTerms {
            asset,
            amount: AtomicAmount::new(U256::from(1_000_u64)),
            price: price(50_000_000),
            credit_minor: MinorAmount::new(1_234),
            expires_at: UnixSeconds::new(200),
            block_time: UnixSeconds::new(200),
        };

        let valuation = value_deposit(
            AtomicAmount::new(U256::from(989_u64)),
            price(40_000_000),
            &route.valuation(),
            Some(&lock),
        )
        .expect("outside-tolerance amount uses spot valuation");
        assert_eq!(valuation.source, ValuationSource::Spot);
        assert_eq!(valuation.price, price(40_000_000));
        assert_eq!(valuation.credit_minor, MinorAmount::new(395));
    }

    #[test]
    fn below_minimum_is_rejected() {
        let route = TestRoute::new(Address::from([1_u8; 20]), 5_251, 100);
        assert_eq!(
            value_deposit(
                AtomicAmount::new(U256::from(1_000_u128 * 10_u128.pow(18))),
                price(5_250_000),
                &route.valuation(),
                None,
            ),
            Err(ValuationError::BelowMinimum)
        );
    }

    #[test]
    fn lock_credit_is_not_subject_to_spot_minimum() {
        let asset = Address::from([1_u8; 20]);
        let route = TestRoute::new(asset, 10_000, 0);
        let amount = AtomicAmount::new(U256::from(1_000_u64));
        let lock = LockTerms {
            asset,
            amount,
            price: price(50_000_000),
            credit_minor: MinorAmount::new(1),
            expires_at: UnixSeconds::new(200),
            block_time: UnixSeconds::new(200),
        };

        assert_eq!(
            value_deposit(amount, price(40_000_000), &route.valuation(), Some(&lock)),
            Ok(Valuation {
                price: lock.price,
                source: ValuationSource::Lock,
                credit_minor: lock.credit_minor,
            })
        );
    }

    #[test]
    fn golden_pha_credit_example() {
        let route = TestRoute::new(Address::from([1_u8; 20]), 100, 100);
        // 1,000 PHA * USD 0.0525/PHA = USD 52.50 = 5,250 minor units.
        let amount = AtomicAmount::new(U256::from(1_000_u128 * 10_u128.pow(18)));
        let valuation = value_deposit(amount, price(5_250_000), &route.valuation(), None)
            .expect("golden valuation succeeds");
        assert_eq!(valuation.credit_minor, MinorAmount::new(5_250));
        assert_eq!(valuation.source, ValuationSource::Spot);
    }

    #[test]
    fn valuation_policy_reuses_route_pricing_limits() {
        let pricing = PricingConfig {
            mode: PricingMode::Spot,
            primary: vec![],
            check: vec![],
            fx: vec![],
            sources: vec![],
            sequencer_uptime: None,
            allow_unclear_sources: true,
            peg_band_bps: bps(100),
            max_age_s: 120,
            max_deviation_bps: bps(100),
            max_fx_deviation_bps: Some(bps(50)),
        };

        assert_eq!(
            ValuationPolicy::from(&pricing),
            ValuationPolicy {
                max_age_s: 120,
                max_deviation_bps: bps(100),
                max_fx_deviation_bps: bps(50),
            }
        );
    }

    #[test]
    fn arithmetic_and_credit_range_errors_are_reported() {
        assert_eq!(
            multiply_scaled(price(u64::MAX), price(u64::MAX)),
            Err(ValuationError::ArithmeticOutOfRange)
        );

        let mut route = TestRoute::new(Address::from([1_u8; 20]), 0, 100);
        route.asset.decimals = 2;
        assert_eq!(
            value_deposit(
                AtomicAmount::new(U256::MAX),
                price(u64::MAX),
                &route.valuation(),
                None,
            ),
            Err(ValuationError::Credit(CreditError::OutOfRange))
        );
    }

    proptest! {
        #[test]
        fn accepted_spot_always_satisfies_ratio_bound(
            primary_value in 1_u64..1_000_000_000_000,
            check_value in 1_u64..1_000_000_000_000,
            maximum in 0_u16..=10_000,
        ) {
            let primary = observation("primary", primary_value, 100);
            let check = observation("check", check_value, 100);
            let maximum_bps = bps(maximum);
            let result = validate_spot(
                &primary,
                &check,
                Some(&fx(PRICE_SCALE_FACTOR, 100)),
                UnixSeconds::new(100),
                ValuationPolicy {
                    max_age_s: 0,
                    max_deviation_bps: maximum_bps,
                    max_fx_deviation_bps: bps(0),
                },
            );
            let difference = primary_value.abs_diff(check_value);
            let expected_accept = u128::from(difference) * 10_000
                <= u128::from(primary_value) * u128::from(maximum);
            prop_assert_eq!(result.is_ok(), expected_accept);
            if let Ok(returned) = result {
                prop_assert_eq!(returned, primary.price);
            }
        }

        #[test]
        fn fx_normalization_is_monotone(
            check_value in 100_000_000_u64..1_000_000_000,
            first_fx in 1_u64..1_000_000_000,
            extra in 0_u64..1_000_000_000,
        ) {
            let second_fx = first_fx + extra;
            let first = multiply_scaled(price(check_value), price(first_fx))
                .expect("bounded normalized price is representable");
            let second = multiply_scaled(price(check_value), price(second_fx))
                .expect("bounded normalized price is representable");
            prop_assert!(first <= second);
        }

        #[test]
        fn lock_selection_is_total_and_matches_all_three_conditions(
            actual in 1_u64..1_000_000_000,
            locked in 1_u64..1_000_000_000,
            tolerance in 0_u16..=10_000,
            has_lock in any::<bool>(),
            on_time in any::<bool>(),
            asset_matches in any::<bool>(),
        ) {
            let route_asset = Address::from([1_u8; 20]);
            let route = TestRoute::new(route_asset, 0, tolerance);
            let lock = LockTerms {
                asset: if asset_matches { route_asset } else { Address::from([2_u8; 20]) },
                amount: AtomicAmount::new(U256::from(locked)),
                price: price(2),
                credit_minor: MinorAmount::new(777),
                expires_at: UnixSeconds::new(100),
                block_time: UnixSeconds::new(if on_time { 100 } else { 101 }),
            };
            let amount = AtomicAmount::new(U256::from(actual));
            let selected_lock = has_lock.then_some(&lock);
            let result = value_deposit(amount, price(1), &route.valuation(), selected_lock);
            let difference = actual.abs_diff(locked);
            let within_tolerance = u128::from(difference) * 10_000
                <= u128::from(locked) * u128::from(tolerance);
            let expected_lock = has_lock
                && on_time
                && asset_matches
                && within_tolerance;
            let valuation = result.expect("bounded lock selection always values the deposit");
            prop_assert_eq!(valuation.source == ValuationSource::Lock, expected_lock);
            if expected_lock {
                prop_assert_eq!(valuation.price, lock.price);
                prop_assert_eq!(valuation.credit_minor, lock.credit_minor);
            }
        }
    }
}
