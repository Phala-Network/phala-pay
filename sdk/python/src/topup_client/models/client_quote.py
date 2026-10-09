from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.client_quote_object import check_client_quote_object
from ..models.client_quote_object import ClientQuoteObject
from ..types import UNSET, Unset
from typing import cast


T = TypeVar("T", bound="ClientQuote")


@_attrs_define
class ClientQuote:
    """The public view of a quote, read with its `client_secret` and without a signature, for the
    payer's checkout page. It has no account or internal fields.

        Example:
            {'address': '0x2f3e91325b2288bce392711f85f5359661062a91', 'amount': 2500, 'amount_atomic': '25000000',
                'amount_credited': None, 'asset': 'usdc', 'chain_id': 1, 'confirmations': 1, 'currency': 'usd', 'decimals': 6,
                'expires_at': 1790554500, 'id': 'qt_5f1c0b6a2d9e4f3a8b7c6d5e4f3a2b10', 'livemode': False, 'object': 'quote',
                'payment_status': 'seen', 'payment_uri': 'ethereum:0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48@1/transfer?address
                =0x2f3e91325b2288bce392711f85f5359661062a91&uint256=25000000', 'status': 'open', 'typical_credit_seconds': 30}

        Attributes:
            address (str): Single-use forwarder address to pay.
            amount (int): Credit in the currency's minor unit.
            amount_atomic (str): The exact token amount to pay, in base units, as a decimal string.
            amount_credited (int | None): While `credited`: the credit of the payment in the currency's minor unit, the
                credited
                deposit's `amount`. It differs from `amount` for a payment valued at spot (another amount,
                or paid late); otherwise `null`.
            asset (str): Asset code.
            chain_id (int): EVM chain identifier.
            confirmations (int | None): While `seen`: blocks on top of and including the payment's block; otherwise `null`.
            currency (str): `usd`.
            decimals (int): The token's decimals, to display `amount_atomic`.
            expires_at (int): End of the payment window, Unix seconds.
            id (str): `qt_` id.
            livemode (bool): Whether the quote is in live mode; a test-mode page should say so.
            object_ (ClientQuoteObject): Always `quote`.
            payment_status (str): Progress of the payment shown on the page; display only, never a reason to deliver
                anything: `none`; `seen` (in a block, below the route's confirmation, and may still
                disappear); `confirming` (at the route's confirmation, being valued and screened);
                `credited`; `rejected` (not credited; the payer should contact the merchant's support); or
                `reversed` (credited, then its transaction left the chain before finality: the payment did
                not happen, and the credit is taken back).
            payment_uri (str): EIP-681 URI carrying the token, chain, address, and amount.
            status (str): `open`, `complete`, `expired`, or `canceled`, as on `Quote`; hide the address once
                `expires_at` has passed.
            typical_credit_seconds (int): Typical time from payment to credit, in seconds, at the confirmation the quote's
                payments
                are credited at, as `GET /v1/config` reports it.
            cancel_requested_at (int | None | Unset): Deferred cancellation request time, Unix seconds.
    """

    address: str
    amount: int
    amount_atomic: str
    amount_credited: int | None
    asset: str
    chain_id: int
    confirmations: int | None
    currency: str
    decimals: int
    expires_at: int
    id: str
    livemode: bool
    object_: ClientQuoteObject
    payment_status: str
    payment_uri: str
    status: str
    typical_credit_seconds: int
    cancel_requested_at: int | None | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        address = self.address

        amount = self.amount

        amount_atomic = self.amount_atomic

        amount_credited: int | None
        amount_credited = self.amount_credited

        asset = self.asset

        chain_id = self.chain_id

        confirmations: int | None
        confirmations = self.confirmations

        currency = self.currency

        decimals = self.decimals

        expires_at = self.expires_at

        id = self.id

        livemode = self.livemode

        object_: str = self.object_

        payment_status = self.payment_status

        payment_uri = self.payment_uri

        status = self.status

        typical_credit_seconds = self.typical_credit_seconds

        cancel_requested_at: int | None | Unset
        if isinstance(self.cancel_requested_at, Unset):
            cancel_requested_at = UNSET
        else:
            cancel_requested_at = self.cancel_requested_at

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "address": address,
                "amount": amount,
                "amount_atomic": amount_atomic,
                "amount_credited": amount_credited,
                "asset": asset,
                "chain_id": chain_id,
                "confirmations": confirmations,
                "currency": currency,
                "decimals": decimals,
                "expires_at": expires_at,
                "id": id,
                "livemode": livemode,
                "object": object_,
                "payment_status": payment_status,
                "payment_uri": payment_uri,
                "status": status,
                "typical_credit_seconds": typical_credit_seconds,
            }
        )
        if cancel_requested_at is not UNSET:
            field_dict["cancel_requested_at"] = cancel_requested_at

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        d = dict(src_dict)
        address = d.pop("address")

        amount = d.pop("amount")

        amount_atomic = d.pop("amount_atomic")

        def _parse_amount_credited(data: object) -> int | None:
            if data is None:
                return data
            return cast(int | None, data)

        amount_credited = _parse_amount_credited(d.pop("amount_credited"))

        asset = d.pop("asset")

        chain_id = d.pop("chain_id")

        def _parse_confirmations(data: object) -> int | None:
            if data is None:
                return data
            return cast(int | None, data)

        confirmations = _parse_confirmations(d.pop("confirmations"))

        currency = d.pop("currency")

        decimals = d.pop("decimals")

        expires_at = d.pop("expires_at")

        id = d.pop("id")

        livemode = d.pop("livemode")

        object_ = check_client_quote_object(d.pop("object"))

        payment_status = d.pop("payment_status")

        payment_uri = d.pop("payment_uri")

        status = d.pop("status")

        typical_credit_seconds = d.pop("typical_credit_seconds")

        def _parse_cancel_requested_at(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        cancel_requested_at = _parse_cancel_requested_at(d.pop("cancel_requested_at", UNSET))

        client_quote = cls(
            address=address,
            amount=amount,
            amount_atomic=amount_atomic,
            amount_credited=amount_credited,
            asset=asset,
            chain_id=chain_id,
            confirmations=confirmations,
            currency=currency,
            decimals=decimals,
            expires_at=expires_at,
            id=id,
            livemode=livemode,
            object_=object_,
            payment_status=payment_status,
            payment_uri=payment_uri,
            status=status,
            typical_credit_seconds=typical_credit_seconds,
            cancel_requested_at=cancel_requested_at,
        )

        client_quote.additional_properties = d
        return client_quote

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
