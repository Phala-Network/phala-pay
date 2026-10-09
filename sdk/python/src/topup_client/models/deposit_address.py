from __future__ import annotations

from collections.abc import Mapping
from typing import Any, TypeVar, BinaryIO, TextIO, TYPE_CHECKING, Generator

from attrs import define as _attrs_define
from attrs import field as _attrs_field

from ..types import UNSET, Unset

from ..models.deposit_address_object import check_deposit_address_object
from ..models.deposit_address_object import DepositAddressObject
from ..types import UNSET, Unset
from typing import cast

if TYPE_CHECKING:
    from ..models.deposit_address_metadata import DepositAddressMetadata
    from ..models.deposit_address_network import DepositAddressNetwork
    from ..models.payment import Payment


T = TypeVar("T", bound="DepositAddress")


@_attrs_define
class DepositAddress:
    """A customer's persistent deposit address, like a bank-transfer virtual account: one address for
    every supported token on every supported network. Any amount of a supported token sent to it
    is credited to the customer at the market (spot) price when it arrives. Rotation retires it and
    issues a new one on every network; a retired address is still credited.

        Example:
            {'address': '0x0f45147a02e4c9d91aff20024e22095536fd5053', 'client_reference_id': 'team-42', 'client_secret':
                'da_7b2e9c4a1f6d48b3a5c0e2d4f6a8b1c3_secret_0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9',
                'created': 1790553600, 'id': 'da_7b2e9c4a1f6d48b3a5c0e2d4f6a8b1c3', 'livemode': False, 'metadata': {'plan':
                'pro'}, 'networks': [{'address': '0x0f45147a02e4c9d91aff20024e22095536fd5053', 'assets': [{'asset': 'usdc',
                'contract': '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', 'decimals': 6, 'payment_uri': 'ethereum:0xa0b86991c621
                8b36c1d19d4a2e9eb0ce3606eb48@1/transfer?address=0x0f45147a02e4c9d91aff20024e22095536fd5053'}], 'chain_id': 1,
                'treasury': '0x936c1991f8da9a919fa11b557a3514719f5a4504'}], 'object': 'deposit_address', 'payments': [],
                'retired_at': None, 'salt': '0x4e9767dd0c2ab5b953a305c3f10dc1e0d1f7c9d3cbab8463509d2edb06ca4b52', 'status':
                'active', 'version': 1}

        Attributes:
            client_reference_id (str): Your identifier of the customer.
            created (int): Creation time, Unix seconds.
            id (str): `da_` id.
            livemode (bool): Whether the address is in live mode.
            metadata (DepositAddressMetadata): Your key/value pairs ([metadata](https://docs.stripe.com/api/metadata)); `{}`
                when none.
                Each deposit to the address starts with a copy.
            networks (list[DepositAddressNetwork]): The address on each supported network of the mode it was issued on, by
                `chain_id`, with
                the tokens it takes there.
            object_ (DepositAddressObject): Always `deposit_address`.
            payments (list[Payment]): Payments to the address in the last 24 hours, newest first, at most 10, as a quote's
                `payment`: `seen` in a block within about a block time of arriving, then `recorded` as a
                deposit. Display only; credit from `deposit.credited`.
            salt (str): CREATE2 salt, 32 bytes of hex; the same on every network.
            status (str): `active`, or `retired` by a rotation; payments to either are credited.
            version (int): The address's version among the customer's addresses, from 1. The salt is
                `keccak256(abi.encode(account, livemode, client_reference_id, "deposit_address", version))`,
                with the types `(string, bool, string, string, uint256)` and `account` your `acct_` id; it
                names no chain or asset. On each network the address is the factory's `CREATE2` over that
                network's treasury and the salt.
            address (None | str | Unset): The address shared by every network, when all of `networks` have the same one;
                `null`
                when a network's treasury differs, and so its address (see `networks`), or when there is
                no network.
            client_secret (None | str | Unset): Lets the customer's page read the address's public view,
                `ClientDepositAddress`, from
                `GET /v1/deposit_addresses/{id}?client_secret=…` without an API key, to show a payment as
                soon as it is seen. Returned only by `POST /v1/deposit_addresses` and `…/rotate`, each
                time a new one; only its hash is stored, and the newest 10 of an address stay valid. Give
                it only to the customer's page, and do not log it.
            retired_at (int | None | Unset): Retirement time, Unix seconds; `null` while active.
    """

    client_reference_id: str
    created: int
    id: str
    livemode: bool
    metadata: DepositAddressMetadata
    networks: list[DepositAddressNetwork]
    object_: DepositAddressObject
    payments: list[Payment]
    salt: str
    status: str
    version: int
    address: None | str | Unset = UNSET
    client_secret: None | str | Unset = UNSET
    retired_at: int | None | Unset = UNSET
    additional_properties: dict[str, Any] = _attrs_field(init=False, factory=dict)

    def to_dict(self) -> dict[str, Any]:
        from ..models.deposit_address_metadata import DepositAddressMetadata  # noqa: PLC0415
        from ..models.deposit_address_network import DepositAddressNetwork  # noqa: PLC0415
        from ..models.payment import Payment  # noqa: PLC0415

        client_reference_id = self.client_reference_id

        created = self.created

        id = self.id

        livemode = self.livemode

        metadata = self.metadata.to_dict()

        networks = []
        for networks_item_data in self.networks:
            networks_item = networks_item_data.to_dict()
            networks.append(networks_item)

        object_: str = self.object_

        payments = []
        for payments_item_data in self.payments:
            payments_item = payments_item_data.to_dict()
            payments.append(payments_item)

        salt = self.salt

        status = self.status

        version = self.version

        address: None | str | Unset
        if isinstance(self.address, Unset):
            address = UNSET
        else:
            address = self.address

        client_secret: None | str | Unset
        if isinstance(self.client_secret, Unset):
            client_secret = UNSET
        else:
            client_secret = self.client_secret

        retired_at: int | None | Unset
        if isinstance(self.retired_at, Unset):
            retired_at = UNSET
        else:
            retired_at = self.retired_at

        field_dict: dict[str, Any] = {}
        field_dict.update(self.additional_properties)
        field_dict.update(
            {
                "client_reference_id": client_reference_id,
                "created": created,
                "id": id,
                "livemode": livemode,
                "metadata": metadata,
                "networks": networks,
                "object": object_,
                "payments": payments,
                "salt": salt,
                "status": status,
                "version": version,
            }
        )
        if address is not UNSET:
            field_dict["address"] = address
        if client_secret is not UNSET:
            field_dict["client_secret"] = client_secret
        if retired_at is not UNSET:
            field_dict["retired_at"] = retired_at

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.deposit_address_metadata import DepositAddressMetadata  # noqa: PLC0415
        from ..models.deposit_address_network import DepositAddressNetwork  # noqa: PLC0415
        from ..models.payment import Payment  # noqa: PLC0415

        d = dict(src_dict)
        client_reference_id = d.pop("client_reference_id")

        created = d.pop("created")

        id = d.pop("id")

        livemode = d.pop("livemode")

        metadata = DepositAddressMetadata.from_dict(d.pop("metadata"))

        networks = []
        _networks = d.pop("networks")
        for networks_item_data in _networks:
            networks_item = DepositAddressNetwork.from_dict(networks_item_data)

            networks.append(networks_item)

        object_ = check_deposit_address_object(d.pop("object"))

        payments = []
        _payments = d.pop("payments")
        for payments_item_data in _payments:
            payments_item = Payment.from_dict(payments_item_data)

            payments.append(payments_item)

        salt = d.pop("salt")

        status = d.pop("status")

        version = d.pop("version")

        def _parse_address(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        address = _parse_address(d.pop("address", UNSET))

        def _parse_client_secret(data: object) -> None | str | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(None | str | Unset, data)

        client_secret = _parse_client_secret(d.pop("client_secret", UNSET))

        def _parse_retired_at(data: object) -> int | None | Unset:
            if data is None:
                return data
            if isinstance(data, Unset):
                return data
            return cast(int | None | Unset, data)

        retired_at = _parse_retired_at(d.pop("retired_at", UNSET))

        deposit_address = cls(
            client_reference_id=client_reference_id,
            created=created,
            id=id,
            livemode=livemode,
            metadata=metadata,
            networks=networks,
            object_=object_,
            payments=payments,
            salt=salt,
            status=status,
            version=version,
            address=address,
            client_secret=client_secret,
            retired_at=retired_at,
        )

        deposit_address.additional_properties = d
        return deposit_address

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
