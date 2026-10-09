from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.quote_list_object import check_quote_list_object
from ..models.quote_list_object import QuoteListObject
from typing import cast

if TYPE_CHECKING:
    from ..models.quote import Quote


T = TypeVar("T", bound="QuoteList")


@_attrs_define
class QuoteList:
    """A page of quotes, newest first (<https://docs.stripe.com/api/pagination>).

    Example:
        {'data': [{'address': '0x2f3e91325b2288bce392711f85f5359661062a91', 'amount': 2500, 'amount_atomic': '25000000',
            'asset': 'usdc', 'chain_id': 1, 'client_reference_id': 'team-42', 'client_secret':
            'qt_5f1c0b6a2d9e4f3a8b7c6d5e4f3a2b10_secret_9f8e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7c6b5a4938271605f4e3d2c1b0a',
            'created': 1790553600, 'currency': 'usd', 'deposit': None, 'exchange_rate': '1.00000000', 'expires_at':
            1790554500, 'id': 'qt_5f1c0b6a2d9e4f3a8b7c6d5e4f3a2b10', 'livemode': False, 'metadata': {'order_id':
            'ord_1001'}, 'object': 'quote', 'payment': {'amount_atomic': '25000000', 'asset': 'usdc', 'chain_id': 1,
            'confirmations': 1, 'deposit': 'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516', 'estimated_final_at': 1790554572,
            'matches_quote': True, 'status': 'seen', 'tx_hash':
            '0x7d3c1e5a9b2f4d6c8e0a1b3d5f7c9e2a4b6d8f0c1e3a5b7d9f1c3e5a7b9d1f3e'}, 'payment_uri': 'ethereum:0xa0b86991c6218b
            36c1d19d4a2e9eb0ce3606eb48@1/transfer?address=0x2f3e91325b2288bce392711f85f5359661062a91&uint256=25000000',
            'status': 'open', 'terms': {'confirmations': '12', 'max_deposit_atomic': '10000000000', 'min_amount': 100,
            'min_deposit_atomic': '0', 'min_refund_atomic': '1000000', 'quote_amount_decimals': 4, 'quote_spread_bps': 0,
            'quote_tolerance_bps': 100, 'quote_ttl_seconds': 900}, 'treasury':
            '0x936c1991f8da9a919fa11b557a3514719f5a4504'}], 'has_more': False, 'object': 'list', 'url': '/v1/quotes'}

    Attributes:
        data (list[Quote]): The quotes.
        has_more (bool): Whether more quotes follow in the direction of this page.
        object_ (QuoteListObject): Always `list`.
        url (str): The list's path, `/v1/quotes`.
    """

    data: list[Quote]
    has_more: bool
    object_: QuoteListObject
    url: str
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.quote import Quote  # noqa: PLC0415

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
        from ..models.quote import Quote  # noqa: PLC0415

        d = dict(src_dict)
        data = []
        _data = d.pop("data")
        for data_item_data in _data:
            data_item = Quote.from_dict(data_item_data)

            data.append(data_item)

        has_more = d.pop("has_more")

        object_ = check_quote_list_object(d.pop("object"))

        url = d.pop("url")

        quote_list = cls(
            data=data,
            has_more=has_more,
            object_=object_,
            url=url,
        )

        quote_list.additional_properties = d
        return quote_list

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
