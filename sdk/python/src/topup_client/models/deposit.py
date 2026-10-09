from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.deposit_object import check_deposit_object
from ..models.deposit_object import DepositObject
from ..types import UNSET, Unset
from typing import cast

if TYPE_CHECKING:
    from ..models.deposit_admin import DepositAdmin
    from ..models.deposit_metadata import DepositMetadata
    from ..models.quote import Quote


T = TypeVar("T", bound="Deposit")


@_attrs_define
class Deposit:
    """A transfer to a quote's address or a deposit address at the route's confirmation: valued,
    screened, and credited, or rejected; `reversed` if its transaction left the chain before
    finality.

    Every `deposit.*` event carries the whole deposit, with cumulative amounts, so the customer's
    balance can be recomputed from the latest snapshot whatever order events arrive in: the
    deposit contributes `amount - amount_refunded - amount_reversed` cents while its `status` is
    `credited` or `reversed`, and nothing while it is `pending` or `rejected`. `status` only moves
    forward (`pending`, then `credited` or `rejected`, then possibly `reversed`) and
    `amount_refunded` only grows, so of two snapshots the later one has the later status or, for
    the same status, the larger `amount_refunded`.

        Example:
            {'address': '0x2f3e91325b2288bce392711f85f5359661062a91', 'amount': 2500, 'amount_atomic': '25000000',
                'amount_refunded': 0, 'amount_refunded_atomic': '0', 'amount_reversed': 0, 'asset': 'usdc', 'asset_contract':
                '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'block_hash':
                '0x9a1c3e5b7d0f2a4c6e8b0d2f4a6c8e0b2d4f6a8c0e2b4d6f8a0c2e4b6d8f0a2c', 'block_number': 21000000, 'block_time':
                1790553612, 'chain_id': 1, 'client_reference_id': 'team-42', 'created': 1790553624, 'currency': 'usd',
                'deposit_address': None, 'exchange_rate': '1.00000000', 'final': True, 'final_at': 1790554572, 'from_address':
                '0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a', 'id': 'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516', 'livemode': False,
                'log_index': 212, 'metadata': {'order_id': 'ord_1001'}, 'object': 'deposit', 'price_source': 'quote', 'quote':
                'qt_5f1c0b6a2d9e4f3a8b7c6d5e4f3a2b10', 'receipt_log_index': 0, 'refunded': False, 'rejection_reason': None,
                'replaced_by': None, 'replaces': None, 'revision': 0, 'status': 'credited', 'swept': False, 'tx_hash':
                '0x7d3c1e5a9b2f4d6c8e0a1b3d5f7c9e2a4b6d8f0c1e3a5b7d9f1c3e5a7b9d1f3e', 'valued_at': 1790553630}

        Attributes:
            address (str): Receiving forwarder address.
            amount_atomic (str): Token amount in base units, as a decimal string.
            amount_refunded (int): Cents of `amount` its succeeded refunds take back: `amount` times
                `amount_refunded_atomic`
                over `amount_atomic`, rounded down, so it never exceeds the refunded share, and all of
                `amount` once fully refunded. Computed from the cumulative refunded amount, it only grows.
                `0` without `amount`.
            amount_refunded_atomic (str): Refunded token amount in base units, as a decimal string: the sum of succeeded
                refunds.
            amount_reversed (int): Cents of `amount` the reversal takes back: `amount` once `status` is `reversed` (a
                deposit is refunded only once final, and a final deposit is never reversed, so a reversed
                deposit has no refunds); `0` otherwise.
            asset_contract (str): Token contract address.
            block_hash (str): Hash of the block the transfer is in; it changes if the transaction is re-included.
            block_number (int): Number of the block the transfer is in; it changes if the transaction is re-included.
            block_time (int): Time of the block the transfer is in, Unix seconds.
            chain_id (int): EVM chain identifier.
            client_reference_id (str): Your identifier of the customer the deposit is credited to.
            created (int): Detection time, Unix seconds.
            currency (str): `usd`.
            final (bool): Whether the deposit's block is final on both providers: a final deposit can no longer be
                reversed, and only a final deposit can be refunded. A deposit is credited at the route's
                confirmation, before it is final (`GET /v1/config` `typical_finality_seconds`).
            from_address (str): Sender of the transfer.
            id (str): `dep_` and the hex of the deposit's deterministic UUID,
                `uuid_v5(DEPOSIT_NAMESPACE, "{chain_id}:{tx_hash}:{receipt_log_index}")`, where
                `receipt_log_index` is the transfer's position among its transaction's receipt logs. A
                deposit recorded at a position after the deposit there was reversed (`replaces`) has
                another id, `uuid_v5(DEPOSIT_NAMESPACE, "{chain_id}:{tx_hash}:{receipt_log_index}:{revision}")`.
            livemode (bool): Whether the deposit is on a live-mode route.
            log_index (int): Block-wide log index of the transfer; it changes if the transaction is re-included.
            metadata (DepositMetadata): Your key/value pairs ([metadata](https://docs.stripe.com/api/metadata)): a copy of
                the
                quote's or the deposit address's when the deposit is recorded, independent of it
                afterwards; `{}` when none.
            object_ (DepositObject): Always `deposit`.
            receipt_log_index (int): Position of the transfer among its transaction's receipt logs: with `chain_id`,
                `tx_hash`,
                and `revision`, what `id` is derived from.
            refunded (bool): Whether the deposit is fully refunded.
            revision (int): How many deposits at the same receipt position were reversed before this one: `0`, or
                the revision of the deposit it `replaces` plus one.
            status (str): `pending` (recorded at the route's confirmation and being valued and screened, or held
                while the account's or customer's `settlement` is paused), `credited`, `rejected` (see
                `rejection_reason`), or `reversed` (its transaction is not in the final chain: its credit is
                taken back, `amount_reversed`). New values may be added.
            swept (bool): Whether a finalized `Flushed` event after the deposit moved its forwarder's balance of
                its token to the treasury (`GET /v1/sweeps`), whoever sent the flush.
            tx_hash (str): Transaction hash.
            admin (DepositAdmin | None | Unset):
            amount (int | None | Unset): Credit in the currency's minor unit (cents), once valued.
            asset (None | str | Unset): Asset code; `null` for a token without a route.
            deposit_address (None | str | Unset): The deposit address that received the transfer, `da_…`, on `chain_id` at
                `address`; `null`
                for a quote's address. Payments to a deposit address, active or retired, are credited at
                spot.
            exchange_rate (None | str | Unset): USD per token, a decimal string with 8 places, once valued.
            final_at (int | None | Unset): When the finality watch found the deposit's block final on both providers, Unix
                seconds;
                `null` until `final`.
            price_source (None | str | Unset): `quote` (the quoted price) or `spot`, once valued.
            quote (None | Quote | str | Unset):
            rejection_reason (None | str | Unset): Why the deposit was rejected: `unsupported_asset`, `asset_not_accepted`
                (a routed asset
                the payment settings the deposit is bound to do not accept), `below_minimum`,
                `out_of_bounds`, `out_of_range`, or `sanctioned`.
            replaced_by (None | str | Unset): The deposit that took this reversed deposit's place, `dep_…` (its `replaces`
                is this one);
                `null` otherwise, or when that deposit is in another account or mode.
            replaces (None | str | Unset): The reversed deposit whose place this one took, `dep_…`: its transaction was re-
                included
                before finality against other state (a router or swap output), and the transfer at the same
                position in the final chain is this deposit. `null` otherwise, or when that deposit is in
                another account or mode.
            valued_at (int | None | Unset): Valuation time, Unix seconds.
    """

    address: str
    amount_atomic: str
    amount_refunded: int
    amount_refunded_atomic: str
    amount_reversed: int
    asset_contract: str
    block_hash: str
    block_number: int
    block_time: int
    chain_id: int
    client_reference_id: str
    created: int
    currency: str
    final: bool
    from_address: str
    id: str
    livemode: bool
    log_index: int
    metadata: DepositMetadata
    object_: DepositObject
    receipt_log_index: int
    refunded: bool
    revision: int
    status: str
    swept: bool
    tx_hash: str
    admin: DepositAdmin | None | Unset = UNSET
    amount: int | None | Unset = UNSET
    asset: None | str | Unset = UNSET
    deposit_address: None | str | Unset = UNSET
    exchange_rate: None | str | Unset = UNSET
    final_at: int | None | Unset = UNSET
    price_source: None | str | Unset = UNSET
    quote: None | Quote | str | Unset = UNSET
    rejection_reason: None | str | Unset = UNSET
    replaced_by: None | str | Unset = UNSET
    replaces: None | str | Unset = UNSET
    valued_at: int | None | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.deposit_admin import DepositAdmin  # noqa: PLC0415
        from ..models.deposit_metadata import DepositMetadata  # noqa: PLC0415
        from ..models.quote import Quote  # noqa: PLC0415

        address = self.address

        amount_atomic = self.amount_atomic

        amount_refunded = self.amount_refunded

        amount_refunded_atomic = self.amount_refunded_atomic

        amount_reversed = self.amount_reversed

        asset_contract = self.asset_contract

        block_hash = self.block_hash

        block_number = self.block_number

        block_time = self.block_time

        chain_id = self.chain_id

        client_reference_id = self.client_reference_id

        created = self.created

        currency = self.currency

        final = self.final

        from_address = self.from_address

        id = self.id

        livemode = self.livemode

        log_index = self.log_index

        metadata = self.metadata.to_dict()

        object_: str = self.object_

        receipt_log_index = self.receipt_log_index

        refunded = self.refunded

        revision = self.revision

        status = self.status

        swept = self.swept

        tx_hash = self.tx_hash

        admin: dict[str, Any] | None | Unset
        if isinstance(self.admin, Unset):
            admin = UNSET
        elif isinstance(self.admin, DepositAdmin):
            admin = self.admin.to_dict()
        else:
            admin = self.admin

        amount: int | None | Unset
        if isinstance(self.amount, Unset):
            amount = UNSET
        else:
            amount = self.amount

        asset: None | str | Unset
        if isinstance(self.asset, Unset):
            asset = UNSET
        else:
            asset = self.asset

        deposit_address: None | str | Unset
        if isinstance(self.deposit_address, Unset):
            deposit_address = UNSET
        else:
            deposit_address = self.deposit_address

        exchange_rate: None | str | Unset
        if isinstance(self.exchange_rate, Unset):
            exchange_rate = UNSET
        else:
            exchange_rate = self.exchange_rate

        final_at: int | None | Unset
        if isinstance(self.final_at, Unset):
            final_at = UNSET
        else:
            final_at = self.final_at

        price_source: None | str | Unset
        if isinstance(self.price_source, Unset):
            price_source = UNSET
        else:
            price_source = self.price_source

        quote: dict[str, Any] | None | str | Unset
        if isinstance(self.quote, Unset):
            quote = UNSET
        elif isinstance(self.quote, Quote):
            quote = self.quote.to_dict()
        else:
            quote = self.quote

        rejection_reason: None | str | Unset
        if isinstance(self.rejection_reason, Unset):
            rejection_reason = UNSET
        else:
            rejection_reason = self.rejection_reason

        replaced_by: None | str | Unset
        if isinstance(self.replaced_by, Unset):
            replaced_by = UNSET
        else:
            replaced_by = self.replaced_by

        replaces: None | str | Unset
        if isinstance(self.replaces, Unset):
            replaces = UNSET
        else:
            replaces = self.replaces

        valued_at: int | None | Unset
        if isinstance(self.valued_at, Unset):
            valued_at = UNSET
        else:
            valued_at = self.valued_at

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "address": address,
                "amount_atomic": amount_atomic,
                "amount_refunded": amount_refunded,
                "amount_refunded_atomic": amount_refunded_atomic,
                "amount_reversed": amount_reversed,
                "asset_contract": asset_contract,
                "block_hash": block_hash,
                "block_number": block_number,
                "block_time": block_time,
                "chain_id": chain_id,
                "client_reference_id": client_reference_id,
                "created": created,
                "currency": currency,
                "final": final,
                "from_address": from_address,
                "id": id,
                "livemode": livemode,
                "log_index": log_index,
                "metadata": metadata,
                "object": object_,
                "receipt_log_index": receipt_log_index,
                "refunded": refunded,
                "revision": revision,
                "status": status,
                "swept": swept,
                "tx_hash": tx_hash,
            }
        )
        if admin is not UNSET:
            field_dict["admin"] = admin
        if amount is not UNSET:
            field_dict["amount"] = amount
        if asset is not UNSET:
            field_dict["asset"] = asset
        if deposit_address is not UNSET:
            field_dict["deposit_address"] = deposit_address
        if exchange_rate is not UNSET:
            field_dict["exchange_rate"] = exchange_rate
        if final_at is not UNSET:
            field_dict["final_at"] = final_at
        if price_source is not UNSET:
            field_dict["price_source"] = price_source
        if quote is not UNSET:
            field_dict["quote"] = quote
        if rejection_reason is not UNSET:
            field_dict["rejection_reason"] = rejection_reason
        if replaced_by is not UNSET:
            field_dict["replaced_by"] = replaced_by
        if replaces is not UNSET:
            field_dict["replaces"] = replaces
        if valued_at is not UNSET:
            field_dict["valued_at"] = valued_at

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.deposit_admin import DepositAdmin  # noqa: PLC0415
        from ..models.deposit_metadata import DepositMetadata  # noqa: PLC0415
        from ..models.quote import Quote  # noqa: PLC0415

        d = dict(src_dict)
        address = d.pop("address")

        amount_atomic = d.pop("amount_atomic")

        amount_refunded = d.pop("amount_refunded")

        amount_refunded_atomic = d.pop("amount_refunded_atomic")

        amount_reversed = d.pop("amount_reversed")

        asset_contract = d.pop("asset_contract")

        block_hash = d.pop("block_hash")

        block_number = d.pop("block_number")

        block_time = d.pop("block_time")

        chain_id = d.pop("chain_id")

        client_reference_id = d.pop("client_reference_id")

        created = d.pop("created")

        currency = d.pop("currency")

        final = d.pop("final")

        from_address = d.pop("from_address")

        id = d.pop("id")

        livemode = d.pop("livemode")

        log_index = d.pop("log_index")

        metadata = DepositMetadata.from_dict(d.pop("metadata"))

        object_ = check_deposit_object(d.pop("object"))

        receipt_log_index = d.pop("receipt_log_index")

        refunded = d.pop("refunded")

        revision = d.pop("revision")

        status = d.pop("status")

        swept = d.pop("swept")

        tx_hash = d.pop("tx_hash")

        def _parse_admin(data: object) -> DepositAdmin | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            try:
                if not isinstance(data, dict):
                    raise TypeError()
                admin_type_0 = DepositAdmin.from_dict(data)

                return admin_type_0
            except (TypeError, ValueError, AttributeError, KeyError):
                pass
            return cast(DepositAdmin | None | Unset, data)

        admin = _parse_admin(d.pop("admin", UNSET))

        def _parse_amount(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        amount = _parse_amount(d.pop("amount", UNSET))

        def _parse_asset(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        asset = _parse_asset(d.pop("asset", UNSET))

        def _parse_deposit_address(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        deposit_address = _parse_deposit_address(d.pop("deposit_address", UNSET))

        def _parse_exchange_rate(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        exchange_rate = _parse_exchange_rate(d.pop("exchange_rate", UNSET))

        def _parse_final_at(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        final_at = _parse_final_at(d.pop("final_at", UNSET))

        def _parse_price_source(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        price_source = _parse_price_source(d.pop("price_source", UNSET))

        def _parse_quote(data: object) -> None | Quote | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            try:
                if not isinstance(data, dict):
                    raise TypeError()
                componentsschemas_expandable_quote_type_1 = Quote.from_dict(data)

                return componentsschemas_expandable_quote_type_1
            except (TypeError, ValueError, AttributeError, KeyError):
                pass
            return cast(None | Quote | str | Unset, data)

        quote = _parse_quote(d.pop("quote", UNSET))

        def _parse_rejection_reason(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        rejection_reason = _parse_rejection_reason(d.pop("rejection_reason", UNSET))

        def _parse_replaced_by(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        replaced_by = _parse_replaced_by(d.pop("replaced_by", UNSET))

        def _parse_replaces(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        replaces = _parse_replaces(d.pop("replaces", UNSET))

        def _parse_valued_at(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        valued_at = _parse_valued_at(d.pop("valued_at", UNSET))

        deposit = cls(
            address=address,
            amount_atomic=amount_atomic,
            amount_refunded=amount_refunded,
            amount_refunded_atomic=amount_refunded_atomic,
            amount_reversed=amount_reversed,
            asset_contract=asset_contract,
            block_hash=block_hash,
            block_number=block_number,
            block_time=block_time,
            chain_id=chain_id,
            client_reference_id=client_reference_id,
            created=created,
            currency=currency,
            final=final,
            from_address=from_address,
            id=id,
            livemode=livemode,
            log_index=log_index,
            metadata=metadata,
            object_=object_,
            receipt_log_index=receipt_log_index,
            refunded=refunded,
            revision=revision,
            status=status,
            swept=swept,
            tx_hash=tx_hash,
            admin=admin,
            amount=amount,
            asset=asset,
            deposit_address=deposit_address,
            exchange_rate=exchange_rate,
            final_at=final_at,
            price_source=price_source,
            quote=quote,
            rejection_reason=rejection_reason,
            replaced_by=replaced_by,
            replaces=replaces,
            valued_at=valued_at,
        )

        deposit.additional_properties = d
        return deposit

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
