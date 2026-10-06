//! Checked integer money arithmetic and documented rounding rules.

use std::str::FromStr;

use alloy_primitives::{U256, U512};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The fixed number of decimal places used by scaled prices.
pub const PRICE_SCALE: u8 = 8;

/// An amount in the asset's smallest on-chain unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct AtomicAmount(U256);

impl AtomicAmount {
    /// Creates an atomic amount.
    #[must_use]
    pub const fn new(value: U256) -> Self {
        Self(value)
    }

    /// Returns the underlying unsigned integer.
    #[must_use]
    pub const fn value(self) -> U256 {
        self.0
    }

    /// Adds two atomic amounts, returning `None` on overflow.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// Subtracts two atomic amounts, returning `None` on underflow.
    #[must_use]
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }
}

impl Serialize for AtomicAmount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for AtomicAmount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        U256::from_str(&value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// An amount in the destination product's minor unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MinorAmount(u64);

impl MinorAmount {
    /// Creates a minor-unit amount.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying unsigned integer.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Adds two minor-unit amounts, returning `None` on overflow.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// Subtracts two minor-unit amounts, returning `None` on underflow.
    #[must_use]
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }
}

/// A price stored as an integer with a fixed decimal scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "ScaledPriceRepr", into = "ScaledPriceRepr")]
pub struct ScaledPrice {
    value: u64,
    scale: u8,
}

impl ScaledPrice {
    /// Creates a non-zero price at the required eight-decimal scale.
    pub fn new(value: u64, scale: u8) -> Result<Self, MoneyError> {
        if value == 0 {
            return Err(MoneyError::ZeroPrice);
        }
        if scale != PRICE_SCALE {
            return Err(MoneyError::InvalidPriceScale { scale });
        }
        Ok(Self { value, scale })
    }

    /// Returns the scaled integer value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.value
    }

    /// Returns the number of decimal places.
    #[must_use]
    pub const fn scale(self) -> u8 {
        self.scale
    }
}

#[derive(Deserialize, Serialize)]
struct ScaledPriceRepr {
    value: u64,
    scale: u8,
}

impl TryFrom<ScaledPriceRepr> for ScaledPrice {
    type Error = MoneyError;

    fn try_from(value: ScaledPriceRepr) -> Result<Self, Self::Error> {
        Self::new(value.value, value.scale)
    }
}

impl From<ScaledPrice> for ScaledPriceRepr {
    fn from(value: ScaledPrice) -> Self {
        Self {
            value: value.value,
            scale: value.scale,
        }
    }
}

/// Basis points, constrained to the inclusive range 0 through 10,000.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(try_from = "u16", into = "u16")]
pub struct Bps(u16);

impl Bps {
    /// Creates a validated basis-point value.
    pub fn new(value: u16) -> Result<Self, MoneyError> {
        if value > 10_000 {
            return Err(MoneyError::InvalidBps { value });
        }
        Ok(Self(value))
    }

    /// Returns the underlying basis-point value.
    #[must_use]
    pub const fn value(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for Bps {
    type Error = MoneyError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Bps> for u16 {
    fn from(value: Bps) -> Self {
        value.0
    }
}

/// Errors constructing constrained money types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    /// A price of zero cannot be used for inverse quote calculations.
    #[error("price must be greater than zero")]
    ZeroPrice,
    /// A price scale other than eight was supplied.
    #[error("price scale must be {PRICE_SCALE}, got {scale}")]
    InvalidPriceScale {
        /// The rejected scale.
        scale: u8,
    },
    /// A basis-point value exceeded 10,000.
    #[error("basis points must be at most 10000, got {value}")]
    InvalidBps {
        /// The rejected value.
        value: u16,
    },
    /// A positive price rounded to zero at the configured scale.
    #[error("price_not_representable: locked price rounds to zero at the configured scale")]
    PriceNotRepresentable,
    /// An intermediate value exceeded the arithmetic representation.
    #[error("money arithmetic is out of range")]
    ArithmeticOutOfRange,
}

/// Errors computing destination credit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    /// A decimal exponent could not be represented in the 512-bit intermediate.
    #[error("scale_out_of_range: decimal scale is out of range")]
    ScaleOutOfRange,
    /// A quote attempted division by zero.
    #[error("division_by_zero: price must be non-zero")]
    DivisionByZero,
    /// The floored result does not fit into a `u64` minor amount.
    #[error("out_of_range: money amount cannot be represented")]
    OutOfRange,
}

/// Computes destination credit and rounds down toward zero.
///
/// The calculation uses a 512-bit intermediate. For a non-negative exponent it computes
/// `floor(amount * price / 10^exp)`; for a negative exponent it computes
/// `amount * price * 10^(-exp)`. The final value is rejected when it does not fit in `u64`.
pub fn credit(
    amount: AtomicAmount,
    price: ScaledPrice,
    asset_decimals: u8,
    unit_decimals: u8,
) -> Result<MinorAmount, CreditError> {
    let exponent = decimal_exponent(asset_decimals, price.scale, unit_decimals);
    let product = U512::from(amount.0)
        .checked_mul(U512::from(price.value))
        .ok_or(CreditError::OutOfRange)?;
    let result = if exponent >= 0 {
        let divisor = power_of_ten(exponent.unsigned_abs())?;
        product
            .checked_div(divisor)
            .ok_or(CreditError::DivisionByZero)?
    } else {
        product
            .checked_mul(power_of_ten(exponent.unsigned_abs())?)
            .ok_or(CreditError::OutOfRange)?
    };
    u64::try_from(result)
        .map(MinorAmount)
        .map_err(|_| CreditError::OutOfRange)
}

/// Applies a spread to a spot price and rounds the quotient to nearest, ties to even.
///
/// The returned price is never greater than `spot`; a positive spread cannot increase the price.
pub fn lock_price(spot: ScaledPrice, spread: Bps) -> Result<ScaledPrice, MoneyError> {
    let basis = 10_000_u128;
    let numerator = u128::from(spot.value)
        .checked_mul(basis)
        .ok_or(MoneyError::ArithmeticOutOfRange)?;
    let denominator = basis
        .checked_add(u128::from(spread.0))
        .ok_or(MoneyError::ArithmeticOutOfRange)?;
    let quotient = numerator
        .checked_div(denominator)
        .ok_or(MoneyError::ArithmeticOutOfRange)?;
    let remainder = numerator
        .checked_rem(denominator)
        .ok_or(MoneyError::ArithmeticOutOfRange)?;
    let twice_remainder = remainder
        .checked_mul(2)
        .ok_or(MoneyError::ArithmeticOutOfRange)?;
    let quotient_is_odd = quotient
        .checked_rem(2)
        .ok_or(MoneyError::ArithmeticOutOfRange)?
        == 1;
    let round_up =
        twice_remainder > denominator || (twice_remainder == denominator && quotient_is_odd);
    let rounded = if round_up {
        quotient
            .checked_add(1)
            .ok_or(MoneyError::ArithmeticOutOfRange)?
    } else {
        quotient
    };
    let value = u64::try_from(rounded).map_err(|_| MoneyError::ArithmeticOutOfRange)?;
    match ScaledPrice::new(value, spot.scale) {
        Err(MoneyError::ZeroPrice) => Err(MoneyError::PriceNotRepresentable),
        result => result,
    }
}

/// Returns the smallest atomic amount whose floored credit reaches `target`.
///
/// This is the inverse of [`credit`]. Division rounds up, and scale, division, conversion, or final
/// credit representation failures are returned explicitly.
pub fn tokens_for_credit(
    target: MinorAmount,
    price: ScaledPrice,
    asset_decimals: u8,
    unit_decimals: u8,
) -> Result<AtomicAmount, CreditError> {
    let exponent = decimal_exponent(asset_decimals, price.scale, unit_decimals);
    let target_value = U512::from(target.0);
    let price_value = U512::from(price.value);
    let (numerator, denominator) = if exponent >= 0 {
        (
            target_value
                .checked_mul(power_of_ten(exponent.unsigned_abs())?)
                .ok_or(CreditError::ScaleOutOfRange)?,
            price_value,
        )
    } else {
        (
            target_value,
            price_value
                .checked_mul(power_of_ten(exponent.unsigned_abs())?)
                .ok_or(CreditError::ScaleOutOfRange)?,
        )
    };
    let quotient = numerator
        .checked_div(denominator)
        .ok_or(CreditError::DivisionByZero)?;
    let remainder = numerator
        .checked_rem(denominator)
        .ok_or(CreditError::DivisionByZero)?;
    let rounded = if remainder.is_zero() {
        quotient
    } else {
        quotient
            .checked_add(U512::from(1_u8))
            .ok_or(CreditError::OutOfRange)?
    };
    let (value, overflow) = U256::overflowing_from_limbs_slice(rounded.as_limbs());
    if overflow {
        return Err(CreditError::OutOfRange);
    }
    let amount = AtomicAmount(value);
    let achieved = credit(amount, price, asset_decimals, unit_decimals)?;
    if achieved < target {
        return Err(CreditError::OutOfRange);
    }
    Ok(amount)
}

/// Rounds `amount` up to a multiple of `10^(asset_decimals − shown_decimals)`, so the token amount
/// has at most `shown_decimals` digits after the point and is never less than `amount`.
///
/// `shown_decimals ≥ asset_decimals` returns `amount` unchanged.
pub fn round_up_to_decimals(
    amount: AtomicAmount,
    asset_decimals: u8,
    shown_decimals: u8,
) -> Result<AtomicAmount, CreditError> {
    let Some(dropped) = asset_decimals.checked_sub(shown_decimals) else {
        return Ok(amount);
    };
    let step = power_of_ten(u16::from(dropped))?;
    let value = U512::from(amount.0);
    let remainder = value.checked_rem(step).ok_or(CreditError::DivisionByZero)?;
    if remainder.is_zero() {
        return Ok(amount);
    }
    let rounded = value
        .checked_sub(remainder)
        .and_then(|floor| floor.checked_add(step))
        .ok_or(CreditError::OutOfRange)?;
    let (value, overflow) = U256::overflowing_from_limbs_slice(rounded.as_limbs());
    if overflow {
        return Err(CreditError::OutOfRange);
    }
    Ok(AtomicAmount(value))
}

fn decimal_exponent(asset_decimals: u8, price_scale: u8, unit_decimals: u8) -> i16 {
    i16::from(asset_decimals)
        .checked_add(i16::from(price_scale))
        .and_then(|value| value.checked_sub(i16::from(unit_decimals)))
        .unwrap_or_default()
}

fn power_of_ten(exponent: u16) -> Result<U512, CreditError> {
    U512::from(10_u8)
        .checked_pow(U512::from(exponent))
        .ok_or(CreditError::ScaleOutOfRange)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn price(value: u64) -> ScaledPrice {
        ScaledPrice::new(value, PRICE_SCALE).expect("test prices are valid")
    }

    fn bps(value: u16) -> Bps {
        Bps::new(value).expect("test basis points are valid")
    }

    proptest! {
        #[test]
        fn credit_is_monotone_in_amount(first in 0_u64..1_000_000_000_000, extra in 0_u64..1_000_000_000_000, price_value in 1_u64..1_000_000_000) {
            let first = U256::from(first);
            let second = first + U256::from(extra);
            let first_credit = credit(AtomicAmount::new(first), price(price_value), 6, 2)
                .expect("generated credit is representable");
            let second_credit = credit(AtomicAmount::new(second), price(price_value), 6, 2)
                .expect("generated credit is representable");
            prop_assert!(first_credit <= second_credit);
        }

        #[test]
        fn credit_is_monotone_in_price(amount in 0_u64..1_000_000_000_000, first_price in 1_u64..1_000_000_000, extra in 0_u64..1_000_000_000) {
            let second_price = first_price + extra;
            let first_credit = credit(AtomicAmount::new(U256::from(amount)), price(first_price), 6, 2)
                .expect("generated credit is representable");
            let second_credit = credit(AtomicAmount::new(U256::from(amount)), price(second_price), 6, 2)
                .expect("generated credit is representable");
            prop_assert!(first_credit <= second_credit);
        }

        #[test]
        fn splitting_loses_at_most_n_minus_one(parts in proptest::collection::vec(0_u64..1_000_000_000_000_u64, 1..20), price_value in 1_u64..1_000_000_000_u64) {
            let total_atomic: u128 = parts.iter().map(|part| u128::from(*part)).sum();
            let whole = credit(AtomicAmount::new(U256::from(total_atomic)), price(price_value), 6, 2)
                .expect("bounded input fits")
                .value();
            let split: u64 = parts.iter().map(|part| {
                credit(AtomicAmount::new(U256::from(*part)), price(price_value), 6, 2)
                    .expect("bounded part fits")
                    .value()
            }).sum();
            let loss = whole - split;
            prop_assert!(loss <= u64::try_from(parts.len() - 1).expect("test vector length fits"));
        }

        #[test]
        fn tokens_round_up_to_the_minimal_amount(target in 1_u64..1_000_000_000, price_value in 1_u64..1_000_000_000) {
            let target = MinorAmount::new(target);
            let locked_price = price(price_value);
            let tokens = tokens_for_credit(target, locked_price, 6, 2)
                .expect("generated inverse quote is representable");
            let actual = credit(tokens, locked_price, 6, 2).expect("inverse result fits");
            let previous = tokens
                .checked_sub(AtomicAmount::new(U256::from(1_u8)))
                .expect("positive target needs at least one atomic unit");
            let previous_credit = credit(previous, locked_price, 6, 2)
                .expect("previous credit remains representable");
            prop_assert!(actual >= target);
            prop_assert!(previous_credit < target);
        }

        #[test]
        fn rounding_up_keeps_the_shown_decimals_and_never_lowers(amount in any::<u128>(), asset_decimals in 0_u8..=36, shown_decimals in 0_u8..=36) {
            let amount = AtomicAmount::new(U256::from(amount));
            let rounded = round_up_to_decimals(amount, asset_decimals, shown_decimals)
                .expect("a u128 amount rounds within U256");
            let step = U256::from(10_u8).pow(U256::from(asset_decimals.saturating_sub(shown_decimals)));
            prop_assert!(rounded >= amount);
            prop_assert!(rounded.value() % step == U256::ZERO);
            prop_assert!(rounded.value() - amount.value() < step);
        }

        #[test]
        fn positive_spread_never_increases_price(value in 2_u64..=u64::MAX, spread in 1_u16..=10_000_u16) {
            let spot = price(value);
            let locked = lock_price(spot, bps(spread))
                .expect("generated lock price is representable");
            prop_assert!(locked <= spot);
        }
    }

    #[test]
    fn negative_exponent_multiplies_instead_of_dividing() {
        assert_eq!(
            credit(AtomicAmount::new(U256::from(3_u8)), price(2), 2, 12),
            Ok(MinorAmount::new(600))
        );
    }

    #[test]
    fn overflowing_credit_is_rejected() {
        assert_eq!(
            credit(AtomicAmount::new(U256::MAX), price(u64::MAX), 0, 0),
            Err(CreditError::OutOfRange)
        );
    }

    #[test]
    fn credit_uses_a_u512_intermediate_before_scaling_down() {
        let amount = AtomicAmount::new(U256::MAX);
        let scaled_price = price(u64::MAX);
        let product = U512::from(U256::MAX) * U512::from(u64::MAX);
        assert!(product > U512::from(U256::MAX));

        let expected = product / U512::from(10_u8).pow(U512::from(85_u8));
        assert_eq!(
            credit(amount, scaled_price, 77, 0),
            Ok(MinorAmount::new(
                u64::try_from(expected).expect("scaled result fits")
            ))
        );
    }

    #[test]
    fn reverse_quote_is_minimal_by_one_atomic_unit() {
        let target = MinorAmount::new(1_234);
        let scaled_price = price(25_000_000);
        let tokens = tokens_for_credit(target, scaled_price, 6, 2).expect("quote fits");
        let previous = tokens
            .checked_sub(AtomicAmount::new(U256::from(1_u8)))
            .expect("quote is positive");

        assert!(credit(tokens, scaled_price, 6, 2).unwrap() >= target);
        assert!(credit(previous, scaled_price, 6, 2).unwrap() < target);
    }

    #[test]
    fn reverse_quote_handles_a_negative_exponent() {
        let target = MinorAmount::new(601);
        let scaled_price = price(2);
        let tokens = tokens_for_credit(target, scaled_price, 2, 12).expect("quote fits");

        assert_eq!(tokens, AtomicAmount::new(U256::from(4_u8)));
        assert_eq!(
            credit(tokens, scaled_price, 2, 12),
            Ok(MinorAmount::new(800))
        );
        assert_eq!(
            credit(AtomicAmount::new(U256::from(3_u8)), scaled_price, 2, 12),
            Ok(MinorAmount::new(600))
        );
    }

    #[test]
    fn reverse_quote_rejects_unrepresentable_result_credit() {
        assert_eq!(
            tokens_for_credit(MinorAmount::new(u64::MAX), price(1_u64 << 63), 0, 8,),
            Err(CreditError::OutOfRange)
        );
    }

    #[test]
    fn reverse_quote_rejects_unrepresentable_scale() {
        assert_eq!(
            tokens_for_credit(MinorAmount::new(1), price(1), u8::MAX, 0),
            Err(CreditError::ScaleOutOfRange)
        );
    }

    #[test]
    fn rounding_up_shortens_an_eighteen_decimal_amount() {
        let amount = AtomicAmount::new(U256::from(273_918_494_456_300_549_947_u128));
        assert_eq!(
            round_up_to_decimals(amount, 18, 4),
            Ok(AtomicAmount::new(U256::from(
                273_918_500_000_000_000_000_u128
            )))
        );
        assert_eq!(
            round_up_to_decimals(AtomicAmount::new(U256::MAX), 18, 4),
            Err(CreditError::OutOfRange)
        );
    }

    #[test]
    fn lock_price_rejects_a_zero_scaled_result() {
        assert_eq!(
            lock_price(price(1), bps(10_000)),
            Err(MoneyError::PriceNotRepresentable)
        );
    }

    #[test]
    fn lock_price_uses_half_even_rounding() {
        assert_eq!(
            lock_price(price(20_001), bps(10_000)).unwrap().value(),
            10_000
        );
        assert_eq!(
            lock_price(price(20_003), bps(10_000)).unwrap().value(),
            10_002
        );
    }

    #[test]
    fn bps_deserialization_enforces_bounds_and_round_trips() {
        let zero: Bps = serde_json::from_str("0").expect("zero bps is valid");
        let maximum: Bps = serde_json::from_str("10000").expect("maximum bps is valid");
        assert_eq!(zero, bps(0));
        assert_eq!(maximum, bps(10_000));
        assert_eq!(serde_json::to_string(&zero).unwrap(), "0");
        assert_eq!(serde_json::to_string(&maximum).unwrap(), "10000");
        assert!(serde_json::from_str::<Bps>("10001").is_err());
    }

    #[test]
    fn scaled_price_deserialization_enforces_invariants_and_round_trips() {
        let scaled_price = price(u64::MAX);
        let json = serde_json::to_string(&scaled_price).expect("price serializes");
        assert_eq!(
            serde_json::from_str::<ScaledPrice>(&json).unwrap(),
            scaled_price
        );
        assert!(serde_json::from_str::<ScaledPrice>(r#"{"value":0,"scale":8}"#).is_err());
        assert!(serde_json::from_str::<ScaledPrice>(r#"{"value":1,"scale":7}"#).is_err());
    }
}
