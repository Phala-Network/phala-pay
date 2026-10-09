from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.refund_object import check_refund_object
from ..models.refund_object import RefundObject
from ..types import UNSET, Unset
from typing import cast

if TYPE_CHECKING:
    from ..models.deposit import Deposit
    from ..models.refund_metadata import RefundMetadata


T = TypeVar("T", bound="Refund")


@_attrs_define
class Refund:
    """A refund of (part of) a deposit to the customer, which the merchant pays from the treasury of
    the deposit's address and attaches with `mark_paid` (design D5).

        Example:
            {'amount_atomic': '25000000', 'created': 1790557200, 'deposit': 'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516',
                'destination_address': '0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a', 'failure_reason': None, 'id':
                're_3c9e7a1b5d2f4a6c8e0b1d3f5a7c9e02', 'livemode': False, 'metadata': {'ticket': 'support-311'}, 'object':
                'refund', 'receipt_log_index': 0, 'status': 'pending', 'transaction_hash':
                '0x4b6d8f0a2c4e6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a6c', 'treasury':
                '0x936c1991f8da9a919fa11b557a3514719f5a4504'}

        Attributes:
            amount_atomic (str): Token amount in base units, as a decimal string.
            created (int): Request time, Unix seconds.
            deposit (Deposit | str): A deposit id, or the deposit with `expand[]`.
            destination_address (str): Destination address.
            id (str): `re_` id.
            livemode (bool): Whether the refund was requested with a live key.
            metadata (RefundMetadata): Your key/value pairs ([metadata](https://docs.stripe.com/api/metadata)); `{}` when
                none.
            object_ (RefundObject): Always `refund`.
            status (str): `pending` (awaiting payment, or its transaction's finality), `succeeded` (the transfer is
                final), `failed` (the attached transaction does not pay the refund; see
                `failure_reason`), or `canceled` (only before a transaction is attached). A refund marked
                paid stays `pending`, reserving its amount of the deposit, until it is `succeeded` or
                `failed`.
            treasury (str): The treasury the refund must be paid from: the one the deposit's address pays, which may
                differ from the account's current treasury.
            failure_reason (None | str | Unset): Why the refund failed: `transaction_failed`, `transfer_not_found`,
                `sender_mismatch`,
                `destination_mismatch`, `amount_mismatch`, or `transfer_already_used`.
                Historical refunds may retain `transaction_dropped` or `transaction_not_found`; missing
                transactions now remain pending with their reservation and alert after 24 hours.
                New values may be added.
            receipt_log_index (int | None | Unset): Position of the paying `Transfer` log among the logs of the
                transaction's receipt: as named
                when marked paid, or found at verification.
            transaction_hash (None | str | Unset): The attached refund transaction, once marked paid.
    """

    amount_atomic: str
    created: int
    deposit: Deposit | str
    destination_address: str
    id: str
    livemode: bool
    metadata: RefundMetadata
    object_: RefundObject
    status: str
    treasury: str
    failure_reason: None | str | Unset = UNSET
    receipt_log_index: int | None | Unset = UNSET
    transaction_hash: None | str | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.deposit import Deposit  # noqa: PLC0415
        from ..models.refund_metadata import RefundMetadata  # noqa: PLC0415

        amount_atomic = self.amount_atomic

        created = self.created

        deposit: dict[str, Any] | str
        if isinstance(self.deposit, Deposit):
            deposit = self.deposit.to_dict()
        else:
            deposit = self.deposit

        destination_address = self.destination_address

        id = self.id

        livemode = self.livemode

        metadata = self.metadata.to_dict()

        object_: str = self.object_

        status = self.status

        treasury = self.treasury

        failure_reason: None | str | Unset
        if isinstance(self.failure_reason, Unset):
            failure_reason = UNSET
        else:
            failure_reason = self.failure_reason

        receipt_log_index: int | None | Unset
        if isinstance(self.receipt_log_index, Unset):
            receipt_log_index = UNSET
        else:
            receipt_log_index = self.receipt_log_index

        transaction_hash: None | str | Unset
        if isinstance(self.transaction_hash, Unset):
            transaction_hash = UNSET
        else:
            transaction_hash = self.transaction_hash

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "amount_atomic": amount_atomic,
                "created": created,
                "deposit": deposit,
                "destination_address": destination_address,
                "id": id,
                "livemode": livemode,
                "metadata": metadata,
                "object": object_,
                "status": status,
                "treasury": treasury,
            }
        )
        if failure_reason is not UNSET:
            field_dict["failure_reason"] = failure_reason
        if receipt_log_index is not UNSET:
            field_dict["receipt_log_index"] = receipt_log_index
        if transaction_hash is not UNSET:
            field_dict["transaction_hash"] = transaction_hash

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.deposit import Deposit  # noqa: PLC0415
        from ..models.refund_metadata import RefundMetadata  # noqa: PLC0415

        d = dict(src_dict)
        amount_atomic = d.pop("amount_atomic")

        created = d.pop("created")

        def _parse_deposit(data: object) -> Deposit | str:
            try:
                if not isinstance(data, dict):
                    raise TypeError()
                componentsschemas_expandable_deposit_type_1 = Deposit.from_dict(data)

                return componentsschemas_expandable_deposit_type_1
            except (TypeError, ValueError, AttributeError, KeyError):
                pass
            return cast(Deposit | str, data)

        deposit = _parse_deposit(d.pop("deposit"))

        destination_address = d.pop("destination_address")

        id = d.pop("id")

        livemode = d.pop("livemode")

        metadata = RefundMetadata.from_dict(d.pop("metadata"))

        object_ = check_refund_object(d.pop("object"))

        status = d.pop("status")

        treasury = d.pop("treasury")

        def _parse_failure_reason(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        failure_reason = _parse_failure_reason(d.pop("failure_reason", UNSET))

        def _parse_receipt_log_index(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        receipt_log_index = _parse_receipt_log_index(d.pop("receipt_log_index", UNSET))

        def _parse_transaction_hash(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        transaction_hash = _parse_transaction_hash(d.pop("transaction_hash", UNSET))

        refund = cls(
            amount_atomic=amount_atomic,
            created=created,
            deposit=deposit,
            destination_address=destination_address,
            id=id,
            livemode=livemode,
            metadata=metadata,
            object_=object_,
            status=status,
            treasury=treasury,
            failure_reason=failure_reason,
            receipt_log_index=receipt_log_index,
            transaction_hash=transaction_hash,
        )

        refund.additional_properties = d
        return refund

    @property
    def additional_keys(self) -> list[str]:
        return list(self.additional_properties.keys())

    def __getitem__(self, key: str) -> Any:
        return self.additional_properties[key]

    def __setitem__(self, key: str, value: Any) -> None:
        self.additional_properties[key] = value

    def __delitem__(self, key: str) -> None:
        del self.additional_properties[key]

    def __contains__(self, key: str) -> bool:
        return key in self.additional_properties
