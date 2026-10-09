from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset


T = TypeVar("T", bound="DepositAddressAsset")


@_attrs_define
class DepositAddressAsset:
    """A token a deposit address takes on one network.

    Attributes:
        asset (str): Asset code, such as `usdc`.
        contract (str): ERC-20 contract address.
        decimals (int): ERC-20 decimal count.
        payment_uri (str): EIP-681 ERC-20 transfer URI carrying the token, chain, and address, and no amount: the
            payer chooses it.
    """

    asset: str
    contract: str
    decimals: int
    payment_uri: str
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        asset = self.asset

        contract = self.contract

        decimals = self.decimals

        payment_uri = self.payment_uri

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "asset": asset,
                "contract": contract,
                "decimals": decimals,
                "payment_uri": payment_uri,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        asset = d.pop("asset")

        contract = d.pop("contract")

        decimals = d.pop("decimals")

        payment_uri = d.pop("payment_uri")

        deposit_address_asset = cls(
            asset=asset,
            contract=contract,
            decimals=decimals,
            payment_uri=payment_uri,
        )

        deposit_address_asset.additional_properties = d
        return deposit_address_asset

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
