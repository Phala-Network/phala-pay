//! Pure refund rules: which deposits are refundable, and which transfer log pays a refund
//! (design D5).

use std::collections::BTreeSet;

use alloy_primitives::{Address, U256};

use crate::deposit::{DepositState, RejectReason};
use crate::money::AtomicAmount;

/// Deposit and route facts needed to decide whether a deposit may be refunded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefundDeposit {
    /// Current deposit state.
    pub state: DepositState,
    /// Stable rejection reason, when the deposit is rejected.
    pub reason: Option<RejectReason>,
    /// Deposited token amount.
    pub amount: AtomicAmount,
    /// Route minimum below which a deposit is non-refundable dust.
    pub min_refund: AtomicAmount,
}

/// Stable reason a deposit cannot be refunded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefundIneligible {
    /// Sanctioned funds require the compliance process, not a refund.
    Sanctioned,
    /// The deposit is below the route's refundable dust floor.
    Dust,
    /// A deposit still in the settlement pipeline, or reversed, is not refunded.
    NotRejected,
    /// No section 15 refundable case matched the supplied facts.
    NoRefundableCase,
}

/// Applies the section 15 refund policy to persisted deposit and route facts.
///
/// Every refundable case is at or above the dust floor and not sanctioned: a deposit rejected for
/// a wrong token (`unsupported_asset`, or `asset_not_accepted` for a routed token the account's
/// payment settings do not accept), a below-minimum credit (`below_minimum`), or out-of-bounds
/// amounts; and a credited or swept deposit the merchant chooses to refund. The merchant pays
/// every refund from its own treasury.
pub fn refund_eligibility(deposit: RefundDeposit) -> Result<(), RefundIneligible> {
    if deposit.reason == Some(RejectReason::Sanctioned) {
        return Err(RefundIneligible::Sanctioned);
    }
    if deposit.amount < deposit.min_refund {
        return Err(RefundIneligible::Dust);
    }
    if matches!(deposit.state, DepositState::Credited | DepositState::Swept) {
        return Ok(());
    }
    if deposit.state != DepositState::Rejected {
        return Err(RefundIneligible::NotRejected);
    }
    if deposit.reason.is_some() {
        return Ok(());
    }
    Err(RefundIneligible::NoRefundableCase)
}

/// The transfer that pays a refund: the deposit's token, from the treasury of the deposit's own
/// address (not the account's current treasury), to the destination, for exactly the amount.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedRefund {
    /// The deposit's token contract.
    pub token: Address,
    /// The treasury the deposit's forwarder pays.
    pub treasury: Address,
    /// The refund's destination.
    pub destination: Address,
    /// The refund's amount in base units.
    pub amount: U256,
}

/// One ERC-20 `Transfer` log of a finalized refund transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefundTransfer {
    /// Position of the log among the logs of the transaction's receipt (0 for the first). Unlike
    /// the block-wide log index, it survives the transaction's re-inclusion in another block, as
    /// a deposit's identity does.
    pub receipt_log_index: u64,
    /// Token contract that emitted the log.
    pub token: Address,
    /// Transfer sender.
    pub from: Address,
    /// Transfer recipient.
    pub to: Address,
    /// Transferred base units.
    pub amount: U256,
}

/// Stable reason a finalized refund transaction does not pay the refund.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefundFailure {
    /// The transaction reverted.
    TransactionFailed,
    /// No transfer of the deposit's token is at the named log, or in the transaction.
    TransferNotFound,
    /// The transfer is not from the treasury of the deposit's address.
    SenderMismatch,
    /// The transfer is not to the refund's destination.
    DestinationMismatch,
    /// The transfer is not of the refund's amount.
    AmountMismatch,
    /// The matching transfer already pays another refund.
    TransferAlreadyUsed,
    /// Legacy failure code retained for historical objects. Current workers require a finalized
    /// paying receipt to resolve attached refunds; nonce changes alone do not prove replacement.
    TransactionDropped,
    /// Legacy absence-timeout failure code. Current workers alert and keep the reservation.
    TransactionNotFound,
}

impl RefundFailure {
    /// Returns the stable `failure_reason` code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::TransactionFailed => "transaction_failed",
            Self::TransferNotFound => "transfer_not_found",
            Self::SenderMismatch => "sender_mismatch",
            Self::DestinationMismatch => "destination_mismatch",
            Self::AmountMismatch => "amount_mismatch",
            Self::TransferAlreadyUsed => "transfer_already_used",
            Self::TransactionDropped => "transaction_dropped",
            Self::TransactionNotFound => "transaction_not_found",
        }
    }
}

/// Picks the log of a finalized transaction that pays `expected`, and returns its receipt
/// position.
///
/// `named` is the log the merchant named, if any; otherwise any matching log qualifies. A log in
/// `used` pays another refund already. When nothing matches, the reason describes the first
/// transfer of the deposit's token that was considered, checked in the order sender, destination,
/// amount.
pub fn match_refund_transfer(
    expected: &ExpectedRefund,
    succeeded: bool,
    transfers: &[RefundTransfer],
    named: Option<u64>,
    used: &BTreeSet<u64>,
) -> Result<u64, RefundFailure> {
    if !succeeded {
        return Err(RefundFailure::TransactionFailed);
    }
    let mut already_used = false;
    let mut first_mismatch = None;
    for transfer in transfers.iter().filter(|transfer| {
        transfer.token == expected.token
            && named.is_none_or(|index| transfer.receipt_log_index == index)
    }) {
        match mismatch(expected, transfer) {
            None if !used.contains(&transfer.receipt_log_index) => {
                return Ok(transfer.receipt_log_index);
            }
            None => already_used = true,
            Some(reason) => {
                first_mismatch.get_or_insert(reason);
            }
        }
    }
    Err(if already_used {
        RefundFailure::TransferAlreadyUsed
    } else {
        first_mismatch.unwrap_or(RefundFailure::TransferNotFound)
    })
}

fn mismatch(expected: &ExpectedRefund, transfer: &RefundTransfer) -> Option<RefundFailure> {
    if transfer.from != expected.treasury {
        Some(RefundFailure::SenderMismatch)
    } else if transfer.to != expected.destination {
        Some(RefundFailure::DestinationMismatch)
    } else if transfer.amount != expected.amount {
        Some(RefundFailure::AmountMismatch)
    } else {
        None
    }
}

/// The part of a deposit's `credit` (cents) that its succeeded refunds take back: the credit
/// pro rata to the refunded share of the deposited tokens, rounded down.
///
/// It is computed from the cumulative refunded amount, not summed per refund, so it never
/// decreases as refunds succeed, it is never more than the refunded share (the merchant never
/// claws back more than it credited for the refunded tokens), and a full refund takes back the
/// whole credit. `refunded_atomic` above `amount_atomic` counts as a full refund.
#[must_use]
pub fn refunded_credit(credit: u64, amount_atomic: U256, refunded_atomic: U256) -> u64 {
    if amount_atomic.is_zero() || refunded_atomic >= amount_atomic {
        return if refunded_atomic.is_zero() { 0 } else { credit };
    }
    // credit < 2^64 and refunded < amount, so the quotient is below credit and fits.
    let share = U256::from(credit)
        .saturating_mul(refunded_atomic)
        .checked_div(amount_atomic)
        .unwrap_or_default();
    u64::try_from(share).unwrap_or(credit)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    fn amount(value: u64) -> AtomicAmount {
        AtomicAmount::new(U256::from(value))
    }

    fn deposit() -> RefundDeposit {
        RefundDeposit {
            state: DepositState::Detected,
            reason: None,
            amount: amount(100),
            min_refund: amount(10),
        }
    }

    fn rejected(reason: RejectReason) -> RefundDeposit {
        RefundDeposit {
            state: DepositState::Rejected,
            reason: Some(reason),
            ..deposit()
        }
    }

    #[test]
    fn refund_eligibility_table_covers_policy_boundaries() {
        let cases = [
            (
                "ordinary deposit",
                deposit(),
                Err(RefundIneligible::NotRejected),
            ),
            (
                "wrong token",
                rejected(RejectReason::UnsupportedAsset),
                Ok(()),
            ),
            ("out of bounds", rejected(RejectReason::OutOfBounds), Ok(())),
            (
                "an asset the account does not accept",
                rejected(RejectReason::AssetNotAccepted),
                Ok(()),
            ),
            (
                "sanctioned",
                rejected(RejectReason::Sanctioned),
                Err(RefundIneligible::Sanctioned),
            ),
            (
                "below minimum, one below the dust floor",
                RefundDeposit {
                    amount: amount(9),
                    ..rejected(RejectReason::BelowMinimum)
                },
                Err(RefundIneligible::Dust),
            ),
            (
                "below minimum, at the dust floor",
                RefundDeposit {
                    amount: amount(10),
                    ..rejected(RejectReason::BelowMinimum)
                },
                Ok(()),
            ),
            (
                "pending unsupported asset is not refundable",
                RefundDeposit {
                    reason: Some(RejectReason::UnsupportedAsset),
                    ..deposit()
                },
                Err(RefundIneligible::NotRejected),
            ),
            (
                "rejected without a reason matches no case",
                RefundDeposit {
                    state: DepositState::Rejected,
                    ..deposit()
                },
                Err(RefundIneligible::NoRefundableCase),
            ),
            (
                "credited value the product did not apply",
                RefundDeposit {
                    state: DepositState::Credited,
                    ..deposit()
                },
                Ok(()),
            ),
            (
                "swept credited value the product did not apply",
                RefundDeposit {
                    state: DepositState::Swept,
                    ..deposit()
                },
                Ok(()),
            ),
            (
                "credited dust",
                RefundDeposit {
                    state: DepositState::Credited,
                    amount: amount(9),
                    ..deposit()
                },
                Err(RefundIneligible::Dust),
            ),
        ];

        for (name, input, expected) in cases {
            assert_eq!(refund_eligibility(input), expected, "{name}");
        }
    }

    fn address(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    fn expected() -> ExpectedRefund {
        ExpectedRefund {
            token: address(0x70),
            treasury: address(0x7e),
            destination: address(0x44),
            amount: U256::from(100_u64),
        }
    }

    fn paying(receipt_log_index: u64) -> RefundTransfer {
        RefundTransfer {
            receipt_log_index,
            token: address(0x70),
            from: address(0x7e),
            to: address(0x44),
            amount: U256::from(100_u64),
        }
    }

    #[test]
    fn refund_transfer_matching_covers_every_failure() {
        let none = BTreeSet::new();
        let cases = [
            (
                "matching log",
                true,
                vec![paying(7)],
                None,
                none.clone(),
                Ok(7),
            ),
            (
                "reverted",
                false,
                vec![paying(7)],
                None,
                none.clone(),
                Err(RefundFailure::TransactionFailed),
            ),
            (
                "other token only",
                true,
                vec![RefundTransfer {
                    token: address(0x71),
                    ..paying(7)
                }],
                None,
                none.clone(),
                Err(RefundFailure::TransferNotFound),
            ),
            (
                "account's current treasury, not the address's",
                true,
                vec![RefundTransfer {
                    from: address(0x7f),
                    ..paying(7)
                }],
                None,
                none.clone(),
                Err(RefundFailure::SenderMismatch),
            ),
            (
                "wrong destination",
                true,
                vec![RefundTransfer {
                    to: address(0x45),
                    ..paying(7)
                }],
                None,
                none.clone(),
                Err(RefundFailure::DestinationMismatch),
            ),
            (
                "short by one",
                true,
                vec![RefundTransfer {
                    amount: U256::from(99_u64),
                    ..paying(7)
                }],
                None,
                none.clone(),
                Err(RefundFailure::AmountMismatch),
            ),
            (
                "more than the refund",
                true,
                vec![RefundTransfer {
                    amount: U256::from(101_u64),
                    ..paying(7)
                }],
                None,
                none.clone(),
                Err(RefundFailure::AmountMismatch),
            ),
            (
                "the only match pays another refund",
                true,
                vec![paying(7)],
                None,
                BTreeSet::from([7]),
                Err(RefundFailure::TransferAlreadyUsed),
            ),
            (
                "a second match is still free",
                true,
                vec![paying(7), paying(9)],
                None,
                BTreeSet::from([7]),
                Ok(9),
            ),
            (
                "the named log is another one",
                true,
                vec![paying(7)],
                Some(8),
                none.clone(),
                Err(RefundFailure::TransferNotFound),
            ),
            (
                "the named log mismatches though another matches",
                true,
                vec![
                    RefundTransfer {
                        amount: U256::from(5_u64),
                        ..paying(3)
                    },
                    paying(7),
                ],
                Some(3),
                none.clone(),
                Err(RefundFailure::AmountMismatch),
            ),
            (
                "a mismatch before the match",
                true,
                vec![
                    RefundTransfer {
                        to: address(0x45),
                        ..paying(3)
                    },
                    paying(7),
                ],
                None,
                none,
                Ok(7),
            ),
        ];
        for (name, succeeded, transfers, named, used, result) in cases {
            assert_eq!(
                match_refund_transfer(&expected(), succeeded, &transfers, named, &used),
                result,
                "{name}"
            );
        }
    }

    #[test]
    fn refunded_credit_is_pro_rata_rounded_down_and_whole_when_fully_refunded() {
        let cases = [
            ("nothing refunded", 2_500, 1_000, 0, 0),
            ("half", 2_500, 1_000, 500, 1_250),
            ("a third rounds down", 100, 3, 1, 33),
            ("two thirds rounds down", 100, 3, 2, 66),
            ("the whole deposit", 100, 3, 3, 100),
            ("more than the deposit", 100, 3, 4, 100),
            ("one base unit of many", 1, 1_000_000, 1, 0),
            ("no credit", 0, 1_000, 500, 0),
            ("the largest credit", u64::MAX, 7, 6, u64::MAX / 7 * 6),
        ];
        for (name, credit, amount, refunded, expected) in cases {
            assert_eq!(
                refunded_credit(credit, U256::from(amount), U256::from(refunded)),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn refunded_credit_never_over_claws_and_never_decreases() {
        let (credit, amount) = (997_u64, 13_u64);
        let mut previous = 0;
        for refunded in 0..=amount {
            let taken = refunded_credit(credit, U256::from(amount), U256::from(refunded));
            // Never more than the exact share: taken * amount <= credit * refunded.
            assert!(
                u128::from(taken) * u128::from(amount) <= u128::from(credit) * u128::from(refunded)
            );
            assert!(
                taken >= previous,
                "cumulative claw-back decreased at {refunded}"
            );
            previous = taken;
        }
        assert_eq!(previous, credit);
    }
}
