from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.deposit_address_list_object import check_deposit_address_list_object
from ..models.deposit_address_list_object import DepositAddressListObject
from typing import cast

if TYPE_CHECKING:
    from ..models.deposit_address import DepositAddress


T = TypeVar("T", bound="DepositAddressList")


@_attrs_define
class DepositAddressList:
    """A page of deposit addresses, newest first (<https://docs.stripe.com/api/pagination>).

    Example:
        {'data': [{'address': '0x0f45147a02e4c9d91aff20024e22095536fd5053', 'client_reference_id': 'team-42',
            'client_secret':
            'da_7b2e9c4a1f6d48b3a5c0e2d4f6a8b1c3_secret_0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9',
            'created': 1790553600, 'id': 'da_7b2e9c4a1f6d48b3a5c0e2d4f6a8b1c3', 'livemode': False, 'metadata': {'plan':
            'pro'}, 'networks': [{'address': '0x0f45147a02e4c9d91aff20024e22095536fd5053', 'assets': [{'asset': 'usdc',
            'contract': '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'decimals': 6, 'payment_uri': 'ethereum:0xa0b86991c621
            8b36c1d19d4a2e9eb0ce3606eb48@1/transfer?address=0x0f45147a02e4c9d91aff20024e22095536fd5053'}], 'chain_id': 1,
            'treasury': '0x936c1991f8da9a919fa11b557a3514719f5a4504'}], 'object': 'deposit_address', 'payments': [],
            'retired_at': None, 'salt': '0x4e9767dd0c2ab5b953a305c3f10dc1e0d1f7c9d3cbab8463509d2edb06ca4b52', 'status':
            'active', 'version': 1}], 'has_more': False, 'object': 'list', 'url': '/v1/deposit_addresses'}

    Attributes:
        data (list[DepositAddress]): The deposit addresses.
        has_more (bool): Whether more addresses follow in the direction of this page.
        object_ (DepositAddressListObject): Always `list`.
        url (str): The list's path, `/v1/deposit_addresses`.
    """

    data: list[DepositAddress]
    has_more: bool
    object_: DepositAddressListObject
    url: str
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.deposit_address import DepositAddress  # noqa: PLC0415

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
        from ..models.deposit_address import DepositAddress  # noqa: PLC0415

        d = dict(src_dict)
        data = []
        _data = d.pop("data")
        for data_item_data in _data:
            data_item = DepositAddress.from_dict(data_item_data)

            data.append(data_item)

        has_more = d.pop("has_more")

        object_ = check_deposit_address_list_object(d.pop("object"))

        url = d.pop("url")

        deposit_address_list = cls(
            data=data,
            has_more=has_more,
            object_=object_,
            url=url,
        )

        deposit_address_list.additional_properties = d
        return deposit_address_list

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
