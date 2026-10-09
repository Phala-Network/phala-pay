from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..types import UNSET, Unset
from typing import cast

if TYPE_CHECKING:
    from ..models.payment_settings_chain import PaymentSettingsChain


T = TypeVar("T", bound="UpdatePaymentSettingsRequest")


@_attrs_define
class UpdatePaymentSettingsRequest:
    """`POST /v1/payment_settings` body. A parameter not sent is unchanged; `chains`, when sent,
    replaces the whole list. Writes are last-write-wins.

        Example:
            {'chains': [{'assets': [{'asset': 'usdc', 'quote_spread_bps': 0}, {'asset': 'usdt'}], 'chain_id': 1,
                'confirmations': '12'}]}

        Attributes:
            chains (list[PaymentSettingsChain] | None | Unset): The chains to accept, replacing the list: each chain and
                asset of the key's mode once. An
                element's term not sent resets to the operator's default. `[]` accepts nothing.
            quote_creations_per_customer_per_minute (int | None | Unset): One customer's quote creations in a rolling
                minute, from 1 to the operator's maximum;
                `null` restores the default.
    """

    chains: list[PaymentSettingsChain] | None | Unset = UNSET
    quote_creations_per_customer_per_minute: int | None | Unset = UNSET

    def to_dict(self) -> dict[str, Any]:
        from ..models.payment_settings_chain import PaymentSettingsChain  # noqa: PLC0415

        chains: list[dict[str, Any]] | None | Unset
        if isinstance(self.chains, Unset):
            chains = UNSET
        elif isinstance(self.chains, list):
            chains = []
            for chains_type_0_item_data in self.chains:
                chains_type_0_item = chains_type_0_item_data.to_dict()
                chains.append(chains_type_0_item)

        else:
            chains = self.chains

        quote_creations_per_customer_per_minute: int | None | Unset
        if isinstance(self.quote_creations_per_customer_per_minute, Unset):
            quote_creations_per_customer_per_minute = UNSET
        else:
            quote_creations_per_customer_per_minute = self.quote_creations_per_customer_per_minute

        field_dict: dict[str, Any] = {}

        field_dict.update({})
        if chains is not UNSET:
            field_dict["chains"] = chains
        if quote_creations_per_customer_per_minute is not UNSET:
            field_dict["quote_creations_per_customer_per_minute"] = (
                quote_creations_per_customer_per_minute
            )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.payment_settings_chain import PaymentSettingsChain  # noqa: PLC0415

        d = dict(src_dict)

        def _parse_chains(data: object) -> list[PaymentSettingsChain] | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            try:
                if not isinstance(data, list):
                    raise TypeError()
                chains_type_0 = []
                _chains_type_0 = data
                for chains_type_0_item_data in _chains_type_0:
                    chains_type_0_item = PaymentSettingsChain.from_dict(chains_type_0_item_data)

                    chains_type_0.append(chains_type_0_item)

                return chains_type_0
            except (TypeError, ValueError, AttributeError, KeyError):
                pass
            return cast(list[PaymentSettingsChain] | None | Unset, data)

        chains = _parse_chains(d.pop("chains", UNSET))

        def _parse_quote_creations_per_customer_per_minute(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        quote_creations_per_customer_per_minute = _parse_quote_creations_per_customer_per_minute(
            d.pop("quote_creations_per_customer_per_minute", UNSET)
        )

        update_payment_settings_request = cls(
            chains=chains,
            quote_creations_per_customer_per_minute=quote_creations_per_customer_per_minute,
        )

        return update_payment_settings_request
