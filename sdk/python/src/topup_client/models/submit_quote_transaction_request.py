from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset


T = TypeVar("T", bound="SubmitQuoteTransactionRequest")


@_attrs_define
class SubmitQuoteTransactionRequest:
    """A quote's chain is derived from its stored terms.

    Example:
        {'transaction_hash': '0x7d3c1e5a9b2f4d6c8e0a1b3d5f7c9e2a4b6d8f0c1e3a5b7d9f1c3e5a7b9d1f3e'}

    Attributes:
        transaction_hash (str): Transaction hash, exactly 32 hexadecimal bytes prefixed by `0x`.
    """

    transaction_hash: str
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        transaction_hash = self.transaction_hash

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "transaction_hash": transaction_hash,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        transaction_hash = d.pop("transaction_hash")

        submit_quote_transaction_request = cls(
            transaction_hash=transaction_hash,
        )

        submit_quote_transaction_request.additional_properties = d
        return submit_quote_transaction_request

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
