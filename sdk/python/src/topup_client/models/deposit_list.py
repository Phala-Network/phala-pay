from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.deposit_list_object import check_deposit_list_object
from ..models.deposit_list_object import DepositListObject
from typing import cast

if TYPE_CHECKING:
    from ..models.deposit import Deposit


T = TypeVar("T", bound="DepositList")


@_attrs_define
class DepositList:
    """A page of a list, newest first (<https://docs.stripe.com/api/pagination>).

    Example:
        {'data': [{'address': '0x2f3e91325b2288bce392711f85f5359661062a91', 'amount': 2500, 'amount_atomic': '25000000',
            'amount_refunded': 0, 'amount_refunded_atomic': '0', 'amount_reversed': 0, 'asset': 'usdc', 'asset_contract':
            '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'block_hash':
            '0x9a1c3e5b7d0f2a4c6e8b0d2f4a6c8e0b2d4f6a8c0e2b4d6f8a0c2e4b6d8f0a2c', 'block_number': 21000000, 'block_time':
            1790553612, 'chain_id': 1, 'client_reference_id': 'team-42', 'created': 1790553624, 'currency': 'usd',
            'deposit_address': None, 'exchange_rate': '1.00000000', 'final': True, 'final_at': 1790554572, 'from_address':
            '0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a', 'id': 'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516', 'livemode': False,
            'log_index': 212, 'metadata': {'order_id': 'ord_1001'}, 'object': 'deposit', 'price_source': 'quote', 'quote':
            'qt_5f1c0b6a2d9e4f3a8b7c6d5e4f3a2b10', 'receipt_log_index': 0, 'refunded': False, 'rejection_reason': None,
            'replaced_by': None, 'replaces': None, 'revision': 0, 'status': 'credited', 'swept': False, 'tx_hash':
            '0x7d3c1e5a9b2f4d6c8e0a1b3d5f7c9e2a4b6d8f0c1e3a5b7d9f1c3e5a7b9d1f3e', 'valued_at': 1790553630}], 'has_more':
            False, 'object': 'list', 'url': '/v1/deposits'}

    Attributes:
        data (list[Deposit]): The deposits.
        has_more (bool): Whether more deposits follow in the direction of this page.
        object_ (DepositListObject): Always `list`.
        url (str): The list's path, `/v1/deposits`.
    """

    data: list[Deposit]
    has_more: bool
    object_: DepositListObject
    url: str
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.deposit import Deposit  # noqa: PLC0415

        data = []
        for data_item_data in self.data:
            data_item = data_item_data.to_dict()
            data.append(data_item)

        has_more = self.has_more

        object_: str = self.object_

        url = self.url

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "data": data,
                "has_more": has_more,
                "object": object_,
                "url": url,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.deposit import Deposit  # noqa: PLC0415

        d = dict(src_dict)
        data = []
        _data = d.pop("data")
        for data_item_data in _data:
            data_item = Deposit.from_dict(data_item_data)

            data.append(data_item)

        has_more = d.pop("has_more")

        object_ = check_deposit_list_object(d.pop("object"))

        url = d.pop("url")

        deposit_list = cls(
            data=data,
            has_more=has_more,
            object_=object_,
            url=url,
        )

        deposit_list.additional_properties = d
        return deposit_list

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
