from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..types import UNSET, Unset
from typing import cast


T = TypeVar("T", bound="MarkRefundPaidRequest")


@_attrs_define
class MarkRefundPaidRequest:
    """`POST /v1/refunds/{id}/mark_paid` body: the merchant's refund transaction.

    Example:
        {'receipt_log_index': 0, 'transaction_hash':
            '0x4b6d8f0a2c4e6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a6c'}

    Attributes:
        transaction_hash (str): Hash of the transaction that pays the refund from the treasury of the deposit's address.
        original_signed_transaction (None | str | Unset): Optional 0x-prefixed EIP-2718 signed original transaction,
            with a replacement candidate.
            Its keccak256 must equal transaction_hash and its signature must recover a sender. When
            the original's sender/nonce were not captured from both providers before eviction, these
            locally verified fields allow replacement proof with no additional RPC. Raw bytes are not
            stored. Legacy, EIP-2930, EIP-1559, EIP-4844 and EIP-7702 envelopes are supported.
        receipt_log_index (int | None | Unset): Position of the `Transfer` log that pays the refund among the logs of
            the transaction's
            receipt (0 for the first), not the block-wide `logIndex`, which changes if the transaction
            is re-included in another block; any matching log when absent.
        replacement_transaction_hash (None | str | Unset): Optional replacement candidate for the original
            transaction_hash. The original remains
            reserved until both providers finalize this distinct transaction and prove the same
            sender and nonce as the original. Nonce changes or missing receipts alone prove nothing.
            At most eight automatic recovery passes per epoch. Before exhaustion or 24 hours,
            identical re-POSTs are idempotent no-ops. After both, an explicit re-POST renews the same
            candidate (or supplies another), at most once per 24 hours per attachment; never automatic.
    """

    transaction_hash: str
    original_signed_transaction: None | str | Unset = UNSET
    receipt_log_index: int | None | Unset = UNSET
    replacement_transaction_hash: None | str | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        transaction_hash = self.transaction_hash

        original_signed_transaction: None | str | Unset
        if isinstance(self.original_signed_transaction, Unset):
            original_signed_transaction = UNSET
        else:
            original_signed_transaction = self.original_signed_transaction

        receipt_log_index: int | None | Unset
        if isinstance(self.receipt_log_index, Unset):
            receipt_log_index = UNSET
        else:
            receipt_log_index = self.receipt_log_index

        replacement_transaction_hash: None | str | Unset
        if isinstance(self.replacement_transaction_hash, Unset):
            replacement_transaction_hash = UNSET
        else:
            replacement_transaction_hash = self.replacement_transaction_hash

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "transaction_hash": transaction_hash,
            }
        )
        if original_signed_transaction is not UNSET:
            field_dict["original_signed_transaction"] = original_signed_transaction
        if receipt_log_index is not UNSET:
            field_dict["receipt_log_index"] = receipt_log_index
        if replacement_transaction_hash is not UNSET:
            field_dict["replacement_transaction_hash"] = replacement_transaction_hash

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        transaction_hash = d.pop("transaction_hash")

        def _parse_original_signed_transaction(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        original_signed_transaction = _parse_original_signed_transaction(
            d.pop("original_signed_transaction", UNSET)
        )

        def _parse_receipt_log_index(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        receipt_log_index = _parse_receipt_log_index(d.pop("receipt_log_index", UNSET))

        def _parse_replacement_transaction_hash(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        replacement_transaction_hash = _parse_replacement_transaction_hash(
            d.pop("replacement_transaction_hash", UNSET)
        )

        mark_refund_paid_request = cls(
            transaction_hash=transaction_hash,
            original_signed_transaction=original_signed_transaction,
            receipt_log_index=receipt_log_index,
            replacement_transaction_hash=replacement_transaction_hash,
        )

        return mark_refund_paid_request
