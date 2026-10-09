from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.refund_list_object import check_refund_list_object
from ..models.refund_list_object import RefundListObject
from typing import cast

if TYPE_CHECKING:
    from ..models.refund import Refund


T = TypeVar("T", bound="RefundList")


@_attrs_define
class RefundList:
    """A page of refunds, newest first (<https://docs.stripe.com/api/pagination>).

    Example:
        {'data': [{'amount_atomic': '25000000', 'created': 1790557200, 'deposit':
            'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516', 'destination_address': '0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a',
            'failure_reason': None, 'id': 're_3c9e7a1b5d2f4a6c8e0b1d3f5a7c9e02', 'livemode': False, 'metadata': {'ticket':
            'support-311'}, 'object': 'refund', 'receipt_log_index': 0, 'status': 'pending', 'transaction_hash':
            '0x4b6d8f0a2c4e6a8c0e2b4d6f8a0c2e4b6d8f0a2c4e6b8d0f2a4c6e8b0d2f4a6c', 'treasury':
            '0x936c1991f8da9a919fa11b557a3514719f5a4504'}], 'has_more': False, 'object': 'list', 'url': '/v1/refunds'}

    Attributes:
        data (list[Refund]): The refunds.
        has_more (bool): Whether more refunds follow in the direction of this page.
        object_ (RefundListObject): Always `list`.
        url (str): The list's path, `/v1/refunds`.
    """

    data: list[Refund]
    has_more: bool
    object_: RefundListObject
    url: str
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.refund import Refund  # noqa: PLC0415

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
        from ..models.refund import Refund  # noqa: PLC0415

        d = dict(src_dict)
        data = []
        _data = d.pop("data")
        for data_item_data in _data:
            data_item = Refund.from_dict(data_item_data)

            data.append(data_item)

        has_more = d.pop("has_more")

        object_ = check_refund_list_object(d.pop("object"))

        url = d.pop("url")

        refund_list = cls(
            data=data,
            has_more=has_more,
            object_=object_,
            url=url,
        )

        refund_list.additional_properties = d
        return refund_list

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
