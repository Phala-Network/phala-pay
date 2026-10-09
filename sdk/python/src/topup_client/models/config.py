from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.config_object import check_config_object
from ..models.config_object import ConfigObject
from typing import cast

if TYPE_CHECKING:
    from ..models.config_asset import ConfigAsset


T = TypeVar("T", bound="Config")


@_attrs_define
class Config:
    """What a product's UI reads instead of hardcoding: assets, limits, and quote terms.

    Example:
        {'assets': [{'asset': 'usdc', 'chain_id': 1, 'confirmations': '12', 'contract':
            '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'decimals': 6, 'max_deposit_atomic': '10000000000', 'min_amount':
            100, 'min_deposit_atomic': '0', 'min_refund_atomic': '1000000', 'pricing': 'stablecoin',
            'quote_amount_decimals': 4, 'quote_spread_bps': 0, 'quote_tolerance_bps': 100, 'quote_ttl_seconds': 900,
            'typical_credit_seconds': 150, 'typical_finality_seconds': 900}], 'currency': 'usd', 'livemode': False,
            'max_open_amount_per_account': 1000000, 'max_open_amount_per_customer': 500000, 'max_open_quotes': 100,
            'object': 'config', 'quote_creations_per_customer_per_minute': 10}

    Attributes:
        assets (list[ConfigAsset]): One entry per asset your payment settings accept on a chain where you have a
            treasury,
            with its terms: your effective payment config. Empty until you configure
            `POST /v1/payment_settings`.
        currency (str): Credit currency, `usd`.
        livemode (bool): The mode of the key that reads it: `assets` lists that mode's routes.
        max_open_amount_per_account (int): Cap on the credit of your open quotes in this mode, in cents. Test-mode
            quotes never
            count against live mode's cap.
        max_open_amount_per_customer (int): Cap on the credit of one customer's open quotes, in cents; no single quote
            can exceed it.
        max_open_quotes (int): Cap on the number of your open quotes in this mode.
        object_ (ConfigObject): Always `config`.
        quote_creations_per_customer_per_minute (int): One customer's quote creations in a rolling minute, from your
            payment settings.
    """

    assets: list[ConfigAsset]
    currency: str
    livemode: bool
    max_open_amount_per_account: int
    max_open_amount_per_customer: int
    max_open_quotes: int
    object_: ConfigObject
    quote_creations_per_customer_per_minute: int
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.config_asset import ConfigAsset  # noqa: PLC0415

        assets = []
        for assets_item_data in self.assets:
            assets_item = assets_item_data.to_dict()
            assets.append(assets_item)

        currency = self.currency

        livemode = self.livemode

        max_open_amount_per_account = self.max_open_amount_per_account

        max_open_amount_per_customer = self.max_open_amount_per_customer

        max_open_quotes = self.max_open_quotes

        object_: str = self.object_

        quote_creations_per_customer_per_minute = self.quote_creations_per_customer_per_minute

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "assets": assets,
                "currency": currency,
                "livemode": livemode,
                "max_open_amount_per_account": max_open_amount_per_account,
                "max_open_amount_per_customer": max_open_amount_per_customer,
                "max_open_quotes": max_open_quotes,
                "object": object_,
                "quote_creations_per_customer_per_minute": quote_creations_per_customer_per_minute,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.config_asset import ConfigAsset  # noqa: PLC0415

        d = dict(src_dict)
        assets = []
        _assets = d.pop("assets")
        for assets_item_data in _assets:
            assets_item = ConfigAsset.from_dict(assets_item_data)

            assets.append(assets_item)

        currency = d.pop("currency")

        livemode = d.pop("livemode")

        max_open_amount_per_account = d.pop("max_open_amount_per_account")

        max_open_amount_per_customer = d.pop("max_open_amount_per_customer")

        max_open_quotes = d.pop("max_open_quotes")

        object_ = check_config_object(d.pop("object"))

        quote_creations_per_customer_per_minute = d.pop("quote_creations_per_customer_per_minute")

        config = cls(
            assets=assets,
            currency=currency,
            livemode=livemode,
            max_open_amount_per_account=max_open_amount_per_account,
            max_open_amount_per_customer=max_open_amount_per_customer,
            max_open_quotes=max_open_quotes,
            object_=object_,
            quote_creations_per_customer_per_minute=quote_creations_per_customer_per_minute,
        )

        config.additional_properties = d
        return config

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
