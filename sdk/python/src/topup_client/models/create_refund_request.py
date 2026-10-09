from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.metadata_clear import check_metadata_clear
from ..models.metadata_clear import MetadataClear
from ..types import UNSET, Unset
from typing import cast

if TYPE_CHECKING:
    from ..models.metadata_param_type_0 import MetadataParamType0


T = TypeVar("T", bound="CreateRefundRequest")


@_attrs_define
class CreateRefundRequest:
    """`POST /v1/refunds` body.

    Example:
        {'amount_atomic': '25000000', 'deposit': 'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516', 'destination_address':
            '0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a', 'metadata': {'ticket': 'support-311'}}

    Attributes:
        deposit (str): `dep_` id of the deposit to refund.
        destination_address (str): Address the customer controls; never default it to the sender, which may be an
            exchange.
        amount_atomic (None | str | Unset): Amount in base units, as a decimal string; the unrefunded remainder when
            absent.
        metadata (MetadataClear | MetadataParamType0 | Unset): A `metadata` parameter: an object of string values, where
            `""` unsets the key, or `""` to
            unset every key.
    """

    deposit: str
    destination_address: str
    amount_atomic: None | str | Unset = UNSET
    metadata: MetadataClear | MetadataParamType0 | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        from ..models.metadata_param_type_0 import MetadataParamType0  # noqa: PLC0415

        deposit = self.deposit

        destination_address = self.destination_address

        amount_atomic: None | str | Unset
        if isinstance(self.amount_atomic, Unset):
            amount_atomic = UNSET
        else:
            amount_atomic = self.amount_atomic

        metadata: dict[str, Any] | str | Unset
        if isinstance(self.metadata, Unset):
            metadata = UNSET
        elif isinstance(self.metadata, MetadataParamType0):
            metadata = self.metadata.to_dict()
        else:
            metadata = self.metadata

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "deposit": deposit,
                "destination_address": destination_address,
            }
        )
        if amount_atomic is not UNSET:
            field_dict["amount_atomic"] = amount_atomic
        if metadata is not UNSET:
            field_dict["metadata"] = metadata

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.metadata_param_type_0 import MetadataParamType0  # noqa: PLC0415

        d = dict(src_dict)
        deposit = d.pop("deposit")

        destination_address = d.pop("destination_address")

        def _parse_amount_atomic(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        amount_atomic = _parse_amount_atomic(d.pop("amount_atomic", UNSET))

        def _parse_metadata(data: object) -> MetadataClear | MetadataParamType0 | Unset:
            if isinstance(data, Unset):
                return data
            try:
                if not isinstance(data, dict):
                    raise TypeError()
                componentsschemas_metadata_param_type_0 = MetadataParamType0.from_dict(data)

                return componentsschemas_metadata_param_type_0
            except (TypeError, ValueError, AttributeError, KeyError):
                pass
            if not isinstance(data, str):
                raise TypeError()
            componentsschemas_metadata_param_type_1 = check_metadata_clear(data)

            return componentsschemas_metadata_param_type_1

        metadata = _parse_metadata(d.pop("metadata", UNSET))

        create_refund_request = cls(
            deposit=deposit,
            destination_address=destination_address,
            amount_atomic=amount_atomic,
            metadata=metadata,
        )

        return create_refund_request
