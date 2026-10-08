//! Serde schemas and pure validation for attested chain and route files.

use alloy_primitives::{Address, address, keccak256};
use serde::{Deserialize, Serialize};

use crate::money::{AtomicAmount, Bps};

/// A resolved route: the route file with every default applied.
///
/// Route files are parsed as [`RouteSpec`], which names only the values that differ per route
/// or environment; every other value is a documented code default here, attested with the image
/// digest. Serializing a route writes the resolved [`RouteSpec`], every field explicit, which
/// parses back to the same route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RouteSpec", into = "RouteSpec")]
pub struct RouteFile {
    /// Chain-specific settings.
    pub chain: ChainConfig,
    /// Stable route name.
    pub route: String,
    /// Attested route version.
    pub version: u64,
    /// Whether the route moves real value: its chain is a mainnet. Test-mode keys use only test
    /// routes, live-mode keys only live ones (design D9).
    pub livemode: bool,
    /// Asset settings.
    pub asset: AssetConfig,
    /// Price-source and freshness settings.
    pub pricing: PricingConfig,
    /// Deposit screening settings.
    pub screening: ScreeningConfig,
    /// The operator's default and bounds for each account's terms on the route
    /// (docs/design/payment-settings.md §5).
    pub merchant: MerchantBounds,
    /// State-age alert thresholds.
    pub alerts: AlertsConfig,
}

impl RouteFile {
    /// Validates cross-field constraints required before a route is enabled.
    pub fn validate(&self) -> Result<(), RouteError> {
        self.validate_with_template_addresses(false)
    }

    /// Validates a deployment template while allowing zero factory and implementation
    /// placeholders.
    ///
    /// Asset and sanctions-oracle addresses remain subject to normal non-zero validation.
    pub fn validate_template(&self) -> Result<(), RouteError> {
        self.validate_with_template_addresses(true)
    }

    fn validate_with_template_addresses(
        &self,
        allow_template_addresses: bool,
    ) -> Result<(), RouteError> {
        if !allow_template_addresses {
            validate_address(
                "chain.forwarder_factory",
                self.chain.contracts.forwarder_factory,
            )?;
            validate_address("chain.implementation", self.chain.contracts.implementation)?;
        }
        validate_address("asset.contract", self.asset.contract)?;
        validate_address("chain.sanctions_oracle", self.screening.sanctions_oracle)?;
        self.chain
            .confirmations
            .validate_for_chain(self.chain.chain_id)?;
        validate_livemode(self.livemode, self.chain.chain_id)?;
        validate_slug("asset.symbol", &self.asset.symbol)?;
        validate_decimals("asset.decimals", self.asset.decimals)?;
        self.pricing
            .validate(self.chain.chain_id, &self.asset.symbol, self.livemode)?;
        if self.asset.quote_amount_decimals > self.asset.decimals {
            return Err(RouteError::validation(
                "asset.quote_amount_decimals",
                "must be at most asset.decimals",
            ));
        }
        self.merchant.validate()?;
        validate_positive(
            "alerts.stuck_after_s.detected",
            self.alerts.stuck_after_s.detected,
        )?;
        validate_positive(
            "alerts.stuck_after_s.confirmed",
            self.alerts.stuck_after_s.confirmed,
        )?;
        Ok(())
    }
}

/// Chain-specific configuration of a resolved route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainConfig {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Confirmation a transfer's block must reach before it is credited.
    pub confirmations: Confirmations,
    /// Independent RPC provider identifiers.
    pub rpc_providers: Vec<String>,
    /// Forwarder contract addresses.
    pub contracts: ChainContracts,
}

/// The family of a chain, which decides the confirmation values it accepts (design D1).
///
/// A chain joins a family only through a reviewed code change, because the credit rule depends on
/// how the chain reorganizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainFamily {
    /// Ethereum L1 proof-of-stake (mainnet, testnets, and Anvil's L1 simulation): a depth or
    /// `finalized`.
    EthereumL1,
    /// OP-stack L2: a depth on the sequencer's unsafe head, `safe` (derived from data posted to
    /// L1), or `finalized`.
    OpStack,
}

impl ChainFamily {
    /// The reviewed family of `chain_id`, if any. A chain outside every family is credited only at
    /// `finalized`.
    #[must_use]
    pub const fn of(chain_id: u64) -> Option<Self> {
        match chain_id {
            // Mainnet, Sepolia, Holesky, Hoodi, and Anvil.
            1 | 11_155_111 | 17_000 | 560_048 | 31_337 => Some(Self::EthereumL1),
            // OP Mainnet, Base, Base Sepolia, OP Sepolia.
            10 | 8_453 | 84_532 | 11_155_420 => Some(Self::OpStack),
            _ => None,
        }
    }

    /// The family's default confirmation.
    #[must_use]
    pub const fn default_confirmations(self) -> Confirmations {
        match self {
            Self::EthereumL1 => Confirmations::Depth(DEFAULT_ETHEREUM_CONFIRMATION_DEPTH),
            Self::OpStack => Confirmations::Depth(DEFAULT_OP_STACK_CONFIRMATION_DEPTH),
        }
    }

    /// The family's block time in seconds: Ethereum's 12-second slot, an OP-stack chain's
    /// 2-second L2 block.
    #[must_use]
    pub const fn block_seconds(self) -> u64 {
        match self {
            Self::EthereumL1 => ETHEREUM_BLOCK_SECONDS,
            Self::OpStack => OP_STACK_BLOCK_SECONDS,
        }
    }
}

/// The confirmation a transfer's block must reach before it is credited (design D1).
///
/// Written in a route file as a positive integer (a depth: the block and the blocks on top of it,
/// `head - block + 1`), `safe`, or `finalized`. A block at or below `finalized` always qualifies,
/// so `finalized` credits only final deposits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ConfirmationsRepr", into = "ConfirmationsRepr")]
pub enum Confirmations {
    /// At least this many blocks, the transfer's own included, on provider heads (`latest`).
    Depth(u64),
    /// The provider's `safe` block is at or past the transfer's block.
    Safe,
    /// The provider's `finalized` block is at or past the transfer's block.
    Finalized,
}

/// A provider's heads read for one confirmation check. `latest` and `safe` are read only when the
/// confirmation needs them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainHeads {
    /// The `latest` block number, when read.
    pub latest: Option<u64>,
    /// The `safe` block number, when read.
    pub safe: Option<u64>,
    /// The `finalized` block number.
    pub finalized: u64,
}

impl Confirmations {
    /// Whether the provider heads must include `latest`.
    #[must_use]
    pub const fn needs_latest(self) -> bool {
        matches!(self, Self::Depth(_))
    }

    /// Whether the provider heads must include `safe`.
    #[must_use]
    pub const fn needs_safe(self) -> bool {
        matches!(self, Self::Safe)
    }

    /// The highest block that has reached this confirmation on a provider with `heads`: every
    /// block at or below it qualifies. A missing head counts as not reached.
    #[must_use]
    pub fn horizon(self, heads: ChainHeads) -> u64 {
        let reached = match self {
            // head - block + 1 >= n  <=>  block <= head + 1 - n
            Self::Depth(depth) => heads
                .latest
                .and_then(|latest| latest.checked_add(1)?.checked_sub(depth)),
            Self::Safe => heads.safe,
            Self::Finalized => None,
        };
        reached.map_or(heads.finalized, |block| block.max(heads.finalized))
    }

    /// Whether `block` has reached this confirmation on a provider with `heads`.
    #[must_use]
    pub fn reached(self, block: u64, heads: ChainHeads) -> bool {
        block <= self.horizon(heads)
    }

    /// The stricter of two confirmations of one chain (design D1): any depth < `safe` <
    /// `finalized`, and the deeper of two depths. `safe` outranks every depth because the sequencer
    /// can rewrite the unsafe head on its own however deep a block is in it, but not a block derived
    /// from data posted to L1, which only an L1 reorganization reaching that data can change.
    #[must_use]
    pub fn stricter(self, other: Self) -> Self {
        match (self, other) {
            (Self::Finalized, _) | (_, Self::Finalized) => Self::Finalized,
            (Self::Safe, _) | (_, Self::Safe) => Self::Safe,
            (Self::Depth(left), Self::Depth(right)) => Self::Depth(left.max(right)),
        }
    }

    /// Parses an account policy's value: a depth of 1 to 999 999 blocks as decimal digits,
    /// `safe`, or `finalized`, as `GET /v1/config` reports a route's.
    #[must_use]
    pub fn parse_policy(value: &str) -> Option<Self> {
        match value {
            "safe" => Some(Self::Safe),
            "finalized" => Some(Self::Finalized),
            depth
                if (1..=6).contains(&depth.len())
                    && depth.bytes().all(|byte| byte.is_ascii_digit())
                    && !depth.starts_with('0') =>
            {
                depth.parse().ok().map(Self::Depth)
            }
            _ => None,
        }
    }

    /// The policy value of [`Confirmations::parse_policy`].
    #[must_use]
    pub fn policy_value(self) -> String {
        match self {
            Self::Depth(depth) => depth.to_string(),
            Self::Safe => "safe".to_owned(),
            Self::Finalized => "finalized".to_owned(),
        }
    }

    /// Typical seconds from paying to the `deposit.credited` event on chain `chain_id`: at a
    /// depth, half a block waiting for inclusion, the remaining blocks, then a block of polling
    /// and delivery (`depth` blocks and a half: 30 s at depth 2 on Ethereum, 7 s at depth 3 on
    /// an OP-stack chain); for `safe` and `finalized`, the typical delay of those tags.
    #[must_use]
    pub const fn typical_credit_seconds(self, chain_id: u64) -> u64 {
        let block = match ChainFamily::of(chain_id) {
            Some(family) => family.block_seconds(),
            None => ETHEREUM_BLOCK_SECONDS,
        };
        match self {
            Self::Depth(depth) => depth.saturating_mul(block).saturating_add(block / 2),
            Self::Safe => TYPICAL_SAFE_SECONDS,
            Self::Finalized => TYPICAL_FINALIZED_SECONDS,
        }
    }

    /// Checks this confirmation against the family of `chain_id` (design D1): a depth or
    /// `finalized` on Ethereum L1; a depth, `safe`, or `finalized` on OP-stack; only `finalized`
    /// on a chain of no reviewed family.
    ///
    /// # Errors
    ///
    /// Returns the constraint the value fails, for the field `chain.confirmations`.
    pub fn validate_for_chain(self, chain_id: u64) -> Result<(), RouteError> {
        const FIELD: &str = "chain.confirmations";
        match (self, ChainFamily::of(chain_id)) {
            (Self::Depth(0), _) => Err(RouteError::validation(FIELD, "a depth must be at least 1")),
            (Self::Finalized, _)
            | (Self::Depth(_), Some(_))
            | (Self::Safe, Some(ChainFamily::OpStack)) => Ok(()),
            (Self::Safe, Some(ChainFamily::EthereumL1)) => Err(RouteError::validation(
                FIELD,
                "an Ethereum L1 chain accepts a depth or `finalized`",
            )),
            (Self::Depth(_) | Self::Safe, None) => Err(RouteError::validation(
                FIELD,
                format!(
                    "chain {chain_id} has no reviewed chain family; only `finalized` is accepted"
                ),
            )),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ConfirmationsRepr {
    Depth(u64),
    Tag(ConfirmationTag),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfirmationTag {
    Safe,
    Finalized,
}

impl TryFrom<ConfirmationsRepr> for Confirmations {
    type Error = RouteError;

    fn try_from(repr: ConfirmationsRepr) -> Result<Self, Self::Error> {
        match repr {
            ConfirmationsRepr::Depth(0) => Err(RouteError::validation(
                "chain.confirmations",
                "a depth must be at least 1",
            )),
            ConfirmationsRepr::Depth(depth) => Ok(Self::Depth(depth)),
            ConfirmationsRepr::Tag(ConfirmationTag::Safe) => Ok(Self::Safe),
            ConfirmationsRepr::Tag(ConfirmationTag::Finalized) => Ok(Self::Finalized),
        }
    }
}

impl From<Confirmations> for ConfirmationsRepr {
    fn from(confirmations: Confirmations) -> Self {
        match confirmations {
            Confirmations::Depth(depth) => Self::Depth(depth),
            Confirmations::Safe => Self::Tag(ConfirmationTag::Safe),
            Confirmations::Finalized => Self::Tag(ConfirmationTag::Finalized),
        }
    }
}

/// Contract addresses required for deterministic deposits. The treasury a forwarder pays is not
/// the route's: it is the account's treasury of the chain, set through the API (design D10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainContracts {
    /// Forwarder factory address.
    pub forwarder_factory: Address,
    /// Immutable EIP-1167 forwarder implementation address.
    pub implementation: Address,
}

/// Deposited asset configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    /// Lowercase asset code the API names the asset by, such as `pha`.
    pub symbol: String,
    /// ERC-20 contract address.
    pub contract: Address,
    /// ERC-20 decimal count.
    pub decimals: u8,
    /// Token decimals a quote's amount is rounded up to, at most `decimals`. The operator's alone:
    /// rounding up at a low precision costs the payer more than any spread (design
    /// payment-settings §3).
    pub quote_amount_decimals: u8,
}

/// Canonical price configuration.
pub type PricingConfig = crate::price::PriceConfig;

/// Explicit route valuation mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingMode {
    /// Validate a primary asset/USD rate against an independent market and FX leg.
    #[serde(rename = "volatile")]
    Spot,
    /// Credit at one dollar after validating the primary rate as a depeg guard.
    Stablecoin,
}

/// Screening thresholds and oracle address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreeningConfig {
    /// Sanctions oracle contract address.
    pub sanctions_oracle: Address,
}

/// An operator's default for one merchant parameter and the inclusive bounds an account's value
/// must stay within.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounded<T> {
    /// The value of an account that sets none.
    pub default: T,
    /// The lowest value an account may set.
    pub min: T,
    /// The highest value an account may set.
    pub max: T,
}

impl<T: Copy + Ord> Bounded<T> {
    /// `value` as the default and both bounds: a term no account can change.
    #[must_use]
    pub const fn at(value: T) -> Self {
        Self {
            default: value,
            min: value,
            max: value,
        }
    }

    /// Whether an account may set `value`.
    #[must_use]
    pub fn contains(&self, value: T) -> bool {
        self.min <= value && value <= self.max
    }

    /// `value` moved within the bounds, for a value set before the operator tightened them.
    #[must_use]
    pub fn clamp(&self, value: T) -> T {
        value.clamp(self.min, self.max)
    }

    fn validate(&self, field: &'static str) -> Result<(), RouteError> {
        if self.min > self.max || !self.contains(self.default) {
            return Err(RouteError::validation(
                field,
                "must have min <= default <= max",
            ));
        }
        Ok(())
    }
}

/// The operator's defaults and hard bounds for each account's terms on one route (design
/// payment-settings §5). An account's payment settings choose within them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MerchantBounds {
    /// A quote's payment window, in seconds.
    pub quote_ttl_seconds: Bounded<u64>,
    /// A quote's spread below spot, in basis points.
    pub quote_spread_bps: Bounded<Bps>,
    /// The two-sided tolerance of a quote's payment, in basis points.
    pub quote_tolerance_bps: Bounded<Bps>,
    /// The minimum credit of a quote or a deposit, in cents.
    pub min_amount: Bounded<u64>,
    /// The minimum creditable deposit, in base units.
    pub min_deposit_atomic: Bounded<AtomicAmount>,
    /// The maximum creditable deposit, in base units.
    pub max_deposit_atomic: Bounded<AtomicAmount>,
    /// The refund dust floor, in base units.
    pub min_refund_atomic: Bounded<AtomicAmount>,
}

impl MerchantBounds {
    fn validate(&self) -> Result<(), RouteError> {
        self.quote_ttl_seconds
            .validate("merchant.quote_ttl_seconds")?;
        self.quote_spread_bps
            .validate("merchant.quote_spread_bps")?;
        self.quote_tolerance_bps
            .validate("merchant.quote_tolerance_bps")?;
        self.min_amount.validate("merchant.min_amount")?;
        self.min_deposit_atomic
            .validate("merchant.min_deposit_atomic")?;
        self.max_deposit_atomic
            .validate("merchant.max_deposit_atomic")?;
        self.min_refund_atomic
            .validate("merchant.min_refund_atomic")?;
        if self.quote_ttl_seconds.min < QUOTE_TTL_SECONDS_FLOOR
            || self.quote_ttl_seconds.max > QUOTE_TTL_SECONDS_CEILING
        {
            return Err(RouteError::validation(
                "merchant.quote_ttl_seconds",
                format!(
                    "bounds must lie within {QUOTE_TTL_SECONDS_FLOOR} to \
                     {QUOTE_TTL_SECONDS_CEILING} seconds"
                ),
            ));
        }
        if self.quote_spread_bps.max.value() > QUOTE_SPREAD_BPS_CEILING {
            return Err(RouteError::validation(
                "merchant.quote_spread_bps",
                format!("max must be at most {QUOTE_SPREAD_BPS_CEILING}"),
            ));
        }
        if self.quote_tolerance_bps.max.value() > QUOTE_TOLERANCE_BPS_CEILING {
            return Err(RouteError::validation(
                "merchant.quote_tolerance_bps",
                format!("max must be at most {QUOTE_TOLERANCE_BPS_CEILING}"),
            ));
        }
        if self.min_amount.min == 0 {
            return Err(RouteError::validation(
                "merchant.min_amount",
                "min must be at least 1",
            ));
        }
        if self.min_deposit_atomic.default > self.max_deposit_atomic.default {
            return Err(RouteError::validation(
                "merchant.min_deposit_atomic",
                "default must not exceed merchant.max_deposit_atomic's default",
            ));
        }
        if self.min_refund_atomic.max > self.max_deposit_atomic.max {
            return Err(RouteError::validation(
                "merchant.min_refund_atomic",
                "max must not exceed merchant.max_deposit_atomic's max",
            ));
        }
        Ok(())
    }
}

/// Operational alert thresholds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertsConfig {
    /// Maximum age per active deposit state.
    pub stuck_after_s: StuckAfterConfig,
}

/// Maximum ages for active deposit states, in seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StuckAfterConfig {
    /// Detected-state threshold.
    pub detected: u64,
    /// Confirmed-state threshold.
    pub confirmed: u64,
}

/// A route file as written: the values that differ per route or environment, plus optional
/// overrides of the code defaults below. See `docs/architecture.md` §14 for each default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// Stable route name.
    pub route: String,
    /// Attested route version.
    pub version: u64,
    /// Whether the route is live (a mainnet) or test (a testnet); checked against the chain.
    pub livemode: bool,
    /// Chain settings.
    pub chain: ChainSpec,
    /// Deposited asset.
    pub asset: AssetSpec,
    /// Price sources.
    pub price: PricingConfig,
    /// The default and bounds of each account's terms.
    pub merchant: MerchantSpec,
    /// Alert threshold overrides.
    #[serde(default, skip_serializing_if = "AlertsSpec::is_empty")]
    pub alerts: AlertsSpec,
}

/// Chain settings of a route file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainSpec {
    /// EVM chain identifier.
    pub chain_id: u64,
    /// Forwarder factory address.
    pub forwarder_factory: Address,
    /// Confirmation required before crediting; default [`ChainFamily::default_confirmations`], or
    /// `finalized` for a chain outside every family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmations: Option<Confirmations>,
    /// Forwarder implementation; default the factory's first `CREATE` ([`factory_implementation`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<Address>,
    /// Deprecated in N: parsed for N-1 rollback only; unused for screening. Default [`default_sanctions_oracle`] for the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sanctions_oracle: Option<Address>,
}

/// Deposited asset of a route file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetSpec {
    /// Lowercase asset code, such as `pha`.
    pub symbol: String,
    /// ERC-20 contract address.
    pub contract: Address,
    /// ERC-20 decimal count, attested because credit math depends on it.
    pub decimals: u8,
    /// Token decimals a quote's amount is rounded up to; default [`DEFAULT_QUOTE_AMOUNT_DECIMALS`]
    /// or `decimals` if fewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote_amount_decimals: Option<u8>,
}

/// One merchant parameter of a route file: the operator's default and bounds, each left out for
/// its code default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundedSpec<T> {
    /// The value of an account that sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<T>,
    /// The lowest value an account may set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<T>,
    /// The highest value an account may set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<T>,
}

impl<T: Copy> BoundedSpec<T> {
    fn resolve(
        self,
        field: &'static str,
        default: Option<T>,
        min: impl FnOnce(T) -> T,
        max: impl FnOnce(T) -> T,
    ) -> Result<Bounded<T>, RouteError> {
        let default = self
            .default
            .or(default)
            .ok_or_else(|| RouteError::validation(field, "default is required"))?;
        Ok(Bounded {
            default,
            min: self.min.unwrap_or_else(|| min(default)),
            max: self.max.unwrap_or_else(|| max(default)),
        })
    }
}

impl<T> From<Bounded<T>> for BoundedSpec<T> {
    fn from(bounded: Bounded<T>) -> Self {
        Self {
            default: Some(bounded.default),
            min: Some(bounded.min),
            max: Some(bounded.max),
        }
    }
}

/// The merchant section of a route file: each account's terms on the route, as the operator's
/// default and bounds (design payment-settings §5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MerchantSpec {
    /// A quote's payment window in seconds: default [`DEFAULT_QUOTE_WINDOW_S`], bounds 30 to 3600.
    #[serde(default)]
    pub quote_ttl_seconds: BoundedSpec<u64>,
    /// Spread below spot: default [`DEFAULT_QUOTE_SPREAD_BPS`], bounds 0 to 500 (or the default).
    #[serde(default)]
    pub quote_spread_bps: BoundedSpec<Bps>,
    /// Two-sided payment tolerance: default [`DEFAULT_QUOTE_TOLERANCE_BPS`], bounds 0 to 500 (or
    /// the default).
    #[serde(default)]
    pub quote_tolerance_bps: BoundedSpec<Bps>,
    /// Minimum credit in cents: the default is required; an account may raise it.
    pub min_amount: BoundedSpec<u64>,
    /// Minimum creditable deposit in base units: default 0; an account may raise it.
    #[serde(default)]
    pub min_deposit_atomic: BoundedSpec<AtomicAmount>,
    /// Maximum creditable deposit in base units: the default is required; an account may lower it.
    pub max_deposit_atomic: BoundedSpec<AtomicAmount>,
    /// Refund dust floor in base units: the default is required, and an account keeps it unless
    /// the operator sets bounds.
    pub min_refund_atomic: BoundedSpec<AtomicAmount>,
}

/// Alert threshold overrides.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertsSpec {
    /// Maximum age per active deposit state.
    #[serde(default, skip_serializing_if = "StuckAfterSpec::is_empty")]
    pub stuck_after_s: StuckAfterSpec,
}

impl AlertsSpec {
    fn is_empty(&self) -> bool {
        self.stuck_after_s.is_empty()
    }
}

/// Maximum ages per active deposit state, in seconds; each defaults to `DEFAULT_STUCK_AFTER_*`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StuckAfterSpec {
    /// Detected-state threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected: Option<u64>,
    /// Confirmed-state threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<u64>,
}

impl StuckAfterSpec {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Decimals of the unit credit is counted in: USD cents, the API's `amount`, on every route.
pub const UNIT_DECIMALS: u8 = 2;
/// Two blocks on Ethereum L1: depth-1 reorgs are routine, deeper ones were not observed (design
/// D1), and a reversal is recoverable.
pub const DEFAULT_ETHEREUM_CONFIRMATION_DEPTH: u64 = 2;
/// Three blocks on an OP-stack chain's unsafe head, 4 s after inclusion: a product-risk choice
/// (design D1, owner decision of 2026-10-02). Base reports a single reorged L2 block ever and none
/// after batching to L1; the account cap bounds what a deeper reorganization exposes.
pub const DEFAULT_OP_STACK_CONFIRMATION_DEPTH: u64 = 3;
/// Ethereum L1's slot time.
pub const ETHEREUM_BLOCK_SECONDS: u64 = 12;
/// An OP-stack chain's L2 block time (OP Mainnet, Base, and their testnets).
pub const OP_STACK_BLOCK_SECONDS: u64 = 2;
/// Typical delay of an OP-stack `safe` head behind the sequencer: a few L1 batch intervals (Base
/// documents about 2 minutes to batch inclusion; chains that batch less often take longer).
pub const TYPICAL_SAFE_SECONDS: u64 = 300;
/// Typical Ethereum delay from inclusion to the `finalized` tag: a block in epoch `n` is final
/// once the checkpoint of epoch `n + 1` finalizes, 64 to 95 slots of 12 s.
pub const TYPICAL_FINALIZED_SECONDS: u64 = 900;
/// Provider ids of a route that names none: the configuration's `provider-a` and `provider-b`.
pub const DEFAULT_RPC_PROVIDERS: [&str; 2] = ["provider-a", "provider-b"];
/// A 15-minute payment window.
pub const DEFAULT_QUOTE_WINDOW_S: u64 = 900;
/// Quotes are priced 0.5% below spot.
pub const DEFAULT_QUOTE_SPREAD_BPS: u16 = 50;
/// 1% absorbs wallet rounding without accepting a real underpayment.
pub const DEFAULT_QUOTE_TOLERANCE_BPS: u16 = 100;
/// Four token decimals keep the amount to pay readable and typeable; rounding up overpays by less
/// than 0.0001 token.
pub const DEFAULT_QUOTE_AMOUNT_DECIMALS: u8 = 4;
/// The shortest payment window an operator may allow, and the default lower bound: a quote shorter
/// than this cannot be paid.
pub const QUOTE_TTL_SECONDS_FLOOR: u64 = 30;
/// The longest payment window an operator may allow: a day.
pub const QUOTE_TTL_SECONDS_CEILING: u64 = 86_400;
/// The default upper bound of a quote's payment window: an hour.
pub const DEFAULT_QUOTE_TTL_SECONDS_MAX: u64 = 3_600;
/// The highest spread an operator may allow: 50%, leaving the locked price representable.
pub const QUOTE_SPREAD_BPS_CEILING: u16 = 5_000;
/// The highest tolerance an operator may allow: 10%, far from the 100% at which a zero payment
/// would match a quote.
pub const QUOTE_TOLERANCE_BPS_CEILING: u16 = 1_000;
/// The default upper bound of a quote's spread and of its tolerance: 5%.
pub const DEFAULT_QUOTE_BPS_MAX: u16 = 500;
/// Detected deposits normally confirm within minutes.
pub const DEFAULT_STUCK_AFTER_DETECTED_S: u64 = 1_800;
/// Confirmed deposits normally credit within minutes.
pub const DEFAULT_STUCK_AFTER_CONFIRMED_S: u64 = 1_800;

const CHAINALYSIS_ORACLE: Address = address!("0x40C57923924B5c5c5455c48D93317139ADDaC8fb");
const CHAINALYSIS_ORACLE_BASE: Address = address!("0x3A91A31cB3dC49b4db9Ce721F50a9D076c8D739B");

/// The Chainalysis sanctions oracle published for `chain_id`, if any
/// (<https://go.chainalysis.com/chainalysis-oracle-docs.html>).
#[must_use]
pub const fn default_sanctions_oracle(chain_id: u64) -> Option<Address> {
    match chain_id {
        1 | 10 | 56 | 137 | 250 | 42_161 | 42_220 | 43_114 => Some(CHAINALYSIS_ORACLE),
        8_453 => Some(CHAINALYSIS_ORACLE_BASE),
        _ => None,
    }
}

/// The forwarder implementation `ForwarderFactory` creates in its constructor: the factory's
/// first `CREATE`, at nonce 1 (EIP-161), `keccak256(rlp([factory, 1]))[12..]`.
#[must_use]
pub fn factory_implementation(factory: Address) -> Address {
    let mut preimage = Vec::with_capacity(23);
    // RLP list of 22 payload bytes: a 20-byte string (0x94 prefix) and the single byte 0x01.
    preimage.extend_from_slice(&[0xd6, 0x94]);
    preimage.extend_from_slice(factory.as_slice());
    preimage.push(0x01);
    Address::from_word(keccak256(preimage))
}

impl TryFrom<RouteSpec> for RouteFile {
    type Error = RouteError;

    fn try_from(spec: RouteSpec) -> Result<Self, Self::Error> {
        let chain_id = spec.chain.chain_id;
        let sanctions_oracle = spec
            .chain
            .sanctions_oracle
            .or_else(|| default_sanctions_oracle(chain_id))
            .ok_or_else(|| {
                RouteError::validation(
                    "chain.sanctions_oracle",
                    format!("is required for chain {chain_id}, which has no Chainalysis oracle"),
                )
            })?;
        let stuck = spec.alerts.stuck_after_s;
        let confirmations = spec.chain.confirmations.unwrap_or_else(|| {
            ChainFamily::of(chain_id).map_or(Confirmations::Finalized, |family| {
                family.default_confirmations()
            })
        });
        Ok(Self {
            chain: ChainConfig {
                chain_id,
                confirmations,
                rpc_providers: vec!["read".to_owned(), "verify".to_owned()],
                contracts: ChainContracts {
                    forwarder_factory: spec.chain.forwarder_factory,
                    implementation: spec
                        .chain
                        .implementation
                        .unwrap_or_else(|| factory_implementation(spec.chain.forwarder_factory)),
                },
            },
            route: spec.route,
            version: spec.version,
            asset: AssetConfig {
                symbol: spec.asset.symbol,
                contract: spec.asset.contract,
                decimals: spec.asset.decimals,
                quote_amount_decimals: spec
                    .asset
                    .quote_amount_decimals
                    .unwrap_or(DEFAULT_QUOTE_AMOUNT_DECIMALS.min(spec.asset.decimals)),
            },
            livemode: spec.livemode,
            pricing: spec.price,
            screening: ScreeningConfig { sanctions_oracle },
            merchant: merchant_bounds(spec.merchant)?,
            alerts: AlertsConfig {
                stuck_after_s: StuckAfterConfig {
                    detected: stuck.detected.unwrap_or(DEFAULT_STUCK_AFTER_DETECTED_S),
                    confirmed: stuck.confirmed.unwrap_or(DEFAULT_STUCK_AFTER_CONFIRMED_S),
                },
            },
        })
    }
}

impl From<RouteFile> for RouteSpec {
    /// The resolved route with every field explicit.
    fn from(route: RouteFile) -> Self {
        Self {
            route: route.route,
            version: route.version,
            livemode: route.livemode,
            chain: ChainSpec {
                chain_id: route.chain.chain_id,
                forwarder_factory: route.chain.contracts.forwarder_factory,
                confirmations: Some(route.chain.confirmations),
                implementation: Some(route.chain.contracts.implementation),
                sanctions_oracle: Some(route.screening.sanctions_oracle),
            },
            asset: AssetSpec {
                symbol: route.asset.symbol,
                contract: route.asset.contract,
                decimals: route.asset.decimals,
                quote_amount_decimals: Some(route.asset.quote_amount_decimals),
            },
            price: route.pricing,
            merchant: MerchantSpec {
                quote_ttl_seconds: route.merchant.quote_ttl_seconds.into(),
                quote_spread_bps: route.merchant.quote_spread_bps.into(),
                quote_tolerance_bps: route.merchant.quote_tolerance_bps.into(),
                min_amount: route.merchant.min_amount.into(),
                min_deposit_atomic: route.merchant.min_deposit_atomic.into(),
                max_deposit_atomic: route.merchant.max_deposit_atomic.into(),
                min_refund_atomic: route.merchant.min_refund_atomic.into(),
            },
            alerts: AlertsSpec {
                stuck_after_s: StuckAfterSpec {
                    detected: Some(route.alerts.stuck_after_s.detected),
                    confirmed: Some(route.alerts.stuck_after_s.confirmed),
                },
            },
        }
    }
}

/// The merchant section with every code default filled in; [`MerchantBounds::validate`] checks
/// the result.
fn merchant_bounds(spec: MerchantSpec) -> Result<MerchantBounds, RouteError> {
    let bps_max = bps("merchant", DEFAULT_QUOTE_BPS_MAX)?;
    Ok(MerchantBounds {
        quote_ttl_seconds: spec.quote_ttl_seconds.resolve(
            "merchant.quote_ttl_seconds",
            Some(DEFAULT_QUOTE_WINDOW_S),
            |_| QUOTE_TTL_SECONDS_FLOOR,
            |_| DEFAULT_QUOTE_TTL_SECONDS_MAX,
        )?,
        quote_spread_bps: spec.quote_spread_bps.resolve(
            "merchant.quote_spread_bps",
            Some(bps("merchant.quote_spread_bps", DEFAULT_QUOTE_SPREAD_BPS)?),
            |_| Bps::default(),
            |default| default.max(bps_max),
        )?,
        quote_tolerance_bps: spec.quote_tolerance_bps.resolve(
            "merchant.quote_tolerance_bps",
            Some(bps(
                "merchant.quote_tolerance_bps",
                DEFAULT_QUOTE_TOLERANCE_BPS,
            )?),
            |_| Bps::default(),
            |default| default.max(bps_max),
        )?,
        min_amount: spec.min_amount.resolve(
            "merchant.min_amount",
            None,
            std::convert::identity,
            |_| u64::MAX,
        )?,
        min_deposit_atomic: spec.min_deposit_atomic.resolve(
            "merchant.min_deposit_atomic",
            Some(AtomicAmount::default()),
            std::convert::identity,
            |_| AtomicAmount::new(alloy_primitives::U256::MAX),
        )?,
        max_deposit_atomic: spec.max_deposit_atomic.resolve(
            "merchant.max_deposit_atomic",
            None,
            |_| AtomicAmount::default(),
            std::convert::identity,
        )?,
        min_refund_atomic: spec.min_refund_atomic.resolve(
            "merchant.min_refund_atomic",
            None,
            std::convert::identity,
            std::convert::identity,
        )?,
    })
}

fn bps(field: &'static str, value: u16) -> Result<Bps, RouteError> {
    Bps::new(value).map_err(|error| RouteError::validation(field, error.to_string()))
}

/// Route validation failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// A parsed field violated a domain constraint.
    #[error("invalid route field `{field}`: {message}")]
    Validation {
        /// Dotted path to the invalid field.
        field: &'static str,
        /// Human-readable constraint failure.
        message: String,
    },
}

impl RouteError {
    pub(crate) fn validation(field: &'static str, message: impl Into<String>) -> Self {
        Self::Validation {
            field,
            message: message.into(),
        }
    }
}

fn validate_address(field: &'static str, address: Address) -> Result<(), RouteError> {
    if address.is_zero() {
        return Err(RouteError::validation(
            field,
            "must not be the zero address",
        ));
    }
    Ok(())
}

/// Whether `chain_id` is a test network: Ethereum's testnets, the OP-stack testnets, and local
/// development chains. A route on one is a test route; every other chain is live (design D9).
#[must_use]
pub const fn is_testnet(chain_id: u64) -> bool {
    matches!(
        chain_id,
        // Sepolia, Holesky, Hoodi, Base Sepolia, OP Sepolia, Anvil and Hardhat, and Geth dev.
        11_155_111 | 17_000 | 560_048 | 84_532 | 11_155_420 | 31_337 | 1_337
    )
}

fn validate_livemode(livemode: bool, chain_id: u64) -> Result<(), RouteError> {
    match (livemode, is_testnet(chain_id)) {
        (true, true) => Err(RouteError::validation(
            "livemode",
            format!("must be false: chain {chain_id} is a test network"),
        )),
        (false, false) => Err(RouteError::validation(
            "livemode",
            format!("must be true: chain {chain_id} is not a known test network"),
        )),
        _ => Ok(()),
    }
}

fn validate_slug(field: &'static str, value: &str) -> Result<(), RouteError> {
    let valid = value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if !valid {
        return Err(RouteError::validation(
            field,
            "must match ^[a-z0-9][a-z0-9-]{0,62}$",
        ));
    }
    Ok(())
}

fn validate_decimals(field: &'static str, decimals: u8) -> Result<(), RouteError> {
    if decimals > 36 {
        return Err(RouteError::validation(field, "must be at most 36"));
    }
    Ok(())
}

fn validate_positive(field: &'static str, value: u64) -> Result<(), RouteError> {
    if value == 0 {
        return Err(RouteError::validation(field, "must be greater than zero"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_implementation_is_the_factorys_first_create() {
        // Staging's Sepolia factory and the implementation its constructor created.
        assert_eq!(
            factory_implementation(address!("0x2407bE5Be2b632F5b166872A49E4946a70CCa531")),
            address!("0x70B714508BFa441449DC09f790Ca03Baa5170360")
        );
        // The A1 deterministic test-vector deployment.
        assert_eq!(
            factory_implementation(address!("0xe8A9Ab1AbC7651A5b7C2ED5B662F2f80BF5C446d")),
            address!("0xfeb1871c9897251C74b39DFC74e577888290faE6")
        );
    }

    #[test]
    fn chain_defaults_cover_only_reviewed_chains() {
        assert_eq!(default_sanctions_oracle(1), Some(CHAINALYSIS_ORACLE));
        assert_eq!(
            default_sanctions_oracle(8_453),
            Some(CHAINALYSIS_ORACLE_BASE)
        );
        assert_eq!(default_sanctions_oracle(11_155_111), None);
    }

    #[test]
    fn livemode_matches_the_chain() {
        assert!(validate_livemode(true, 1).is_ok());
        assert!(validate_livemode(false, 11_155_111).is_ok());
        assert!(validate_livemode(false, 31_337).is_ok());
        assert!(
            validate_livemode(false, 1)
                .expect_err("a mainnet route is live")
                .to_string()
                .contains("must be true")
        );
        assert!(
            validate_livemode(true, 11_155_111)
                .expect_err("a testnet route is test")
                .to_string()
                .contains("must be false")
        );
    }

    #[test]
    fn confirmation_horizon_follows_the_chain_family_rule() {
        let heads = ChainHeads {
            latest: Some(100),
            safe: Some(90),
            finalized: 60,
        };
        // Depth 2: the head block and its parent's block count; block 99 has two.
        assert_eq!(Confirmations::Depth(2).horizon(heads), 99);
        assert!(Confirmations::Depth(2).reached(99, heads));
        assert!(!Confirmations::Depth(2).reached(100, heads));
        assert!(Confirmations::Depth(1).reached(100, heads));
        assert_eq!(Confirmations::Safe.horizon(heads), 90);
        assert_eq!(Confirmations::Finalized.horizon(heads), 60);
        // A block at or below finalized always qualifies, and an unread head never adds blocks.
        let lagging = ChainHeads {
            latest: None,
            safe: None,
            finalized: 60,
        };
        assert_eq!(Confirmations::Depth(2).horizon(lagging), 60);
        assert_eq!(Confirmations::Safe.horizon(lagging), 60);
        assert_eq!(
            Confirmations::Depth(200).horizon(ChainHeads {
                latest: Some(100),
                ..lagging
            }),
            60
        );
    }

    #[test]
    fn confirmations_parse_as_a_depth_or_a_tag_and_are_checked_per_family() {
        for (yaml, expected) in [
            ("2", Confirmations::Depth(2)),
            ("safe", Confirmations::Safe),
            ("finalized", Confirmations::Finalized),
        ] {
            let parsed: Confirmations = serde_json::from_str(&match yaml {
                "2" => "2".to_owned(),
                tag => format!("\"{tag}\""),
            })
            .expect("valid confirmations");
            assert_eq!(parsed, expected);
            let encoded = serde_json::to_string(&parsed).expect("serializes");
            assert_eq!(
                serde_json::from_str::<Confirmations>(&encoded).expect("round trip"),
                parsed
            );
        }
        assert!(serde_json::from_str::<Confirmations>("0").is_err());
        assert!(serde_json::from_str::<Confirmations>("\"latest\"").is_err());

        assert_eq!(
            ChainFamily::of(1).map(ChainFamily::default_confirmations),
            Some(Confirmations::Depth(2))
        );
        assert_eq!(
            ChainFamily::of(8_453).map(ChainFamily::default_confirmations),
            Some(Confirmations::Depth(3))
        );
        assert!(Confirmations::Depth(2).validate_for_chain(1).is_ok());
        assert!(Confirmations::Finalized.validate_for_chain(1).is_ok());
        assert!(Confirmations::Safe.validate_for_chain(1).is_err());
        assert!(Confirmations::Safe.validate_for_chain(8_453).is_ok());
        assert!(Confirmations::Depth(3).validate_for_chain(8_453).is_ok());
        assert!(Confirmations::Depth(0).validate_for_chain(8_453).is_err());
        assert!(Confirmations::Finalized.validate_for_chain(137).is_ok());
        assert!(Confirmations::Depth(2).validate_for_chain(137).is_err());
        assert!(Confirmations::Safe.validate_for_chain(137).is_err());
        assert_eq!(
            Confirmations::parse_policy("12"),
            Some(Confirmations::Depth(12))
        );
        assert_eq!(
            Confirmations::parse_policy("finalized"),
            Some(Confirmations::Finalized)
        );
        for invalid in ["0", "012", "1234567", "latest", "", "+3"] {
            assert_eq!(Confirmations::parse_policy(invalid), None, "{invalid}");
        }
        assert_eq!(Confirmations::Depth(12).policy_value(), "12");
        assert_eq!(Confirmations::Depth(2).typical_credit_seconds(1), 30);
        assert_eq!(Confirmations::Depth(3).typical_credit_seconds(8_453), 7);
        assert_eq!(Confirmations::Safe.typical_credit_seconds(8_453), 300);
        assert_eq!(Confirmations::Finalized.typical_credit_seconds(8_453), 900);
    }

    #[test]
    fn stricter_orders_any_depth_below_safe_below_finalized() {
        use Confirmations::{Depth, Finalized, Safe};
        assert_eq!(Depth(2).stricter(Depth(5)), Depth(5));
        assert_eq!(Depth(5).stricter(Depth(2)), Depth(5));
        // `safe` is derived from data posted to L1, which no depth on the unsafe head is.
        assert_eq!(Depth(999_999).stricter(Safe), Safe);
        assert_eq!(Safe.stricter(Depth(3)), Safe);
        assert_eq!(Safe.stricter(Finalized), Finalized);
        assert_eq!(Depth(3).stricter(Finalized), Finalized);
        for left in [Depth(1), Depth(3), Safe, Finalized] {
            for right in [Depth(1), Depth(3), Safe, Finalized] {
                assert_eq!(left.stricter(right), right.stricter(left));
            }
        }
    }
}
