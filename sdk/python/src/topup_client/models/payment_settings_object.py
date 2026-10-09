from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.payment_settings_object_object import check_payment_settings_object_object
from ..models.payment_settings_object_object import PaymentSettingsObjectObject
from typing import cast

if TYPE_CHECKING:
    from ..models.available_chain import AvailableChain
    from ..models.payment_settings_chain import PaymentSettingsChain


T = TypeVar("T", bound="PaymentSettingsObject")


@_attrs_define
class PaymentSettingsObject:
    """Your payment settings in the key's mode (`GET /v1/payment_settings`): what you accept and on
    what terms, chosen from the operator's catalog within its bounds
    (docs/design/payment-settings.md).

        Example:
            {'available': [{'assets': [{'accepted': True, 'asset': 'usdc', 'contract':
                '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'decimals': 6, 'enabled': True, 'max_deposit_atomic': {'default':
                '10000000000', 'max': '10000000000', 'min': '0'}, 'min_amount': {'default': 100, 'max': 18446744073709551615,
                'min': 100}, 'min_deposit_atomic': {'default': '0', 'max':
                '115792089237316195423570985008687907853269984665640564039457584007913129639935', 'min': '0'},
                'min_refund_atomic': {'default': '1000000', 'max': '1000000', 'min': '1000000'}, 'pricing': 'stablecoin',
                'quote_amount_decimals': 4, 'quote_spread_bps': {'default': 0, 'max': 500, 'min': 0}, 'quote_tolerance_bps':
                {'default': 100, 'max': 500, 'min': 0}, 'quote_ttl_seconds': {'default': 900, 'max': 3600, 'min': 30}},
                {'accepted': True, 'asset': 'usdt', 'contract': '0xdac17f958d2ee523a2206206994597c13d831ec7', 'decimals': 6,
                'enabled': True, 'max_deposit_atomic': {'default': '10000000000', 'max': '10000000000', 'min': '0'},
                'min_amount': {'default': 100, 'max': 18446744073709551615, 'min': 100}, 'min_deposit_atomic': {'default': '0',
                'max': '115792089237316195423570985008687907853269984665640564039457584007913129639935', 'min': '0'},
                'min_refund_atomic': {'default': '1000000', 'max': '1000000', 'min': '1000000'}, 'pricing': 'stablecoin',
                'quote_amount_decimals': 4, 'quote_spread_bps': {'default': 0, 'max': 500, 'min': 0}, 'quote_tolerance_bps':
                {'default': 100, 'max': 500, 'min': 0}, 'quote_ttl_seconds': {'default': 900, 'max': 3600, 'min': 30}}],
                'chain_id': 1, 'confirmations': {'default': '2', 'floor': '2'}, 'status': 'active'}], 'chains': [{'assets':
                [{'asset': 'usdc', 'quote_spread_bps': 0}, {'asset': 'usdt'}], 'chain_id': 1, 'confirmations': '12'}],
                'livemode': False, 'object': 'payment_settings', 'quote_creations_per_customer_per_minute': None, 'revision':
                'psrev_5b0e4f1a9c3d4e7f8a2b6c1d0e9f8a7b', 'status': 'configured', 'updated': 1790467200}

        Attributes:
            available (list[AvailableChain]): The operator's catalog of the mode: every chain and asset you may accept, with
                its
                defaults and bounds, and whether you accept it.
            chains (list[PaymentSettingsChain]): The chains you accept, each with its accepted assets. A chain or asset not
                listed is not
                accepted.
            livemode (bool): The mode of the key that reads it.
            object_ (PaymentSettingsObjectObject): Always `payment_settings`.
            quote_creations_per_customer_per_minute (int | None): One customer's quote creations in a rolling minute; `null`
                for the default.
            revision (str): The current revision, `psrev_…`: unique and never reused, with no order.
            status (str): `unconfigured` (accepts nothing: never configured), `configured`, or `held` (after a
                restore of the service, until you reconfirm with `POST /v1/payment_settings`; nothing is
                accepted meanwhile, and payments recorded wait).
            updated (int): When the current revision was written, Unix seconds.
    """

    available: list[AvailableChain]
    chains: list[PaymentSettingsChain]
    livemode: bool
    object_: PaymentSettingsObjectObject
    quote_creations_per_customer_per_minute: int | None
    revision: str
    status: str
    updated: int
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.available_chain import AvailableChain  # noqa: PLC0415
        from ..models.payment_settings_chain import PaymentSettingsChain  # noqa: PLC0415

        available = []
        for available_item_data in self.available:
            available_item = available_item_data.to_dict()
            available.append(available_item)

        chains = []
        for chains_item_data in self.chains:
            chains_item = chains_item_data.to_dict()
            chains.append(chains_item)

        livemode = self.livemode

        object_: str = self.object_

        quote_creations_per_customer_per_minute: int | None
        quote_creations_per_customer_per_minute = self.quote_creations_per_customer_per_minute

        revision = self.revision

        status = self.status

        updated = self.updated

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "available": available,
                "chains": chains,
                "livemode": livemode,
                "object": object_,
                "quote_creations_per_customer_per_minute": quote_creations_per_customer_per_minute,
                "revision": revision,
                "status": status,
                "updated": updated,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.available_chain import AvailableChain  # noqa: PLC0415
        from ..models.payment_settings_chain import PaymentSettingsChain  # noqa: PLC0415

        d = dict(src_dict)
        available = []
        _available = d.pop("available")
        for available_item_data in _available:
            available_item = AvailableChain.from_dict(available_item_data)

            available.append(available_item)

        chains = []
        _chains = d.pop("chains")
        for chains_item_data in _chains:
            chains_item = PaymentSettingsChain.from_dict(chains_item_data)

            chains.append(chains_item)

        livemode = d.pop("livemode")

        object_ = check_payment_settings_object_object(d.pop("object"))

        def _parse_quote_creations_per_customer_per_minute(data: object) -> int | None:
            if data is None:
                return data
            return cast(int | None, data)

        quote_creations_per_customer_per_minute = _parse_quote_creations_per_customer_per_minute(
            d.pop("quote_creations_per_customer_per_minute")
        )

        revision = d.pop("revision")

        status = d.pop("status")

        updated = d.pop("updated")

        payment_settings_object = cls(
            available=available,
            chains=chains,
            livemode=livemode,
            object_=object_,
            quote_creations_per_customer_per_minute=quote_creations_per_customer_per_minute,
            revision=revision,
            status=status,
            updated=updated,
        )

        payment_settings_object.additional_properties = d
        return payment_settings_object

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
