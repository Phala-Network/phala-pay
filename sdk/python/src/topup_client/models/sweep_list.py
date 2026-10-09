from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.sweep_list_object import check_sweep_list_object
from ..models.sweep_list_object import SweepListObject
from typing import cast

if TYPE_CHECKING:
    from ..models.sweep import Sweep


T = TypeVar("T", bound="SweepList")


@_attrs_define
class SweepList:
    """A page of sweeps, newest first (<https://docs.stripe.com/api/pagination>).

    Example:
        {'data': [{'address': '0x2f3e91325b2288bce392711f85f5359661062a91', 'amount_atomic': '25000000', 'asset':
            'usdc', 'block_number': 21000420, 'chain_id': 1, 'created': 1790560800, 'forwarder':
            'fwd_5c7e9a1b3d2f44c6e8a0b2d4f6c8e0a2', 'id': 'sw_1e3c5a7b9d0f42e4c6a8b0d2f4e6a8c0', 'livemode': False,
            'log_index': 7, 'object': 'sweep', 'token': '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'treasury':
            '0x936c1991f8da9a919fa11b557a3514719f5a4504', 'tx_hash':
            '0x4b6d8f0a2c4e6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a6c'}], 'has_more': False, 'object': 'list',
            'url': '/v1/sweeps'}

    Attributes:
        data (list[Sweep]): The sweeps.
        has_more (bool): Whether more sweeps follow in the direction of this page.
        object_ (SweepListObject): Always `list`.
        url (str): The list's path, `/v1/sweeps`.
    """

    data: list[Sweep]
    has_more: bool
    object_: SweepListObject
    url: str
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.sweep import Sweep  # noqa: PLC0415

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
        from ..models.sweep import Sweep  # noqa: PLC0415

        d = dict(src_dict)
        data = []
        _data = d.pop("data")
        for data_item_data in _data:
            data_item = Sweep.from_dict(data_item_data)

            data.append(data_item)

        has_more = d.pop("has_more")

        object_ = check_sweep_list_object(d.pop("object"))

        url = d.pop("url")

        sweep_list = cls(
            data=data,
            has_more=has_more,
            object_=object_,
            url=url,
        )

        sweep_list.additional_properties = d
        return sweep_list

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
