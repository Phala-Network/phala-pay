"""`PhalaPay`: the Phala Pay merchant API as Stripe-style resources over `TopupClient`."""

from __future__ import annotations

import math
import os
from collections.abc import Callable, Iterator, Mapping, Sequence
from pathlib import Path
from typing import Any, TypedDict, Unpack, cast
from weakref import ref

import httpx

from topup_client.models import (
    AccountObject,
    ApiKeyObject,
    Balance,
    Config,
    DeletedWebhookEndpoint,
    Deposit,
    DepositAddress,
    EventObjectResponse,
    Forwarder,
    PaymentSettingsObject,
    Quote,
    Refund,
    Sweep,
    Treasury,
    TreasuryChallenge,
    WebhookEndpointObject,
)
from topup_client.types import UNSET, Unset
from topup_sdk import TopupClient, export_account, sign_treasury_challenge
from topup_sdk._origin import normalize_origin
from topup_sdk.client import LIVE_KEY_PREFIXES, Metadata

from ._errors import ConfigurationError, ResponseValidationError
from ._pins import Pins, PinsError, key_livemode, parse_pins
from ._types import (
    DepositAddressStatus,
    DepositStatus,
    EventType,
    QuoteStatus,
    RefundStatus,
    TreasuryStatus,
)
from ._webhook import SignatureVerificationError, Webhook


class RequestOptions(TypedDict, total=False):
    request_deadline: float | None
    upgrade_tolerance: bool | None


def _request_options(options: RequestOptions) -> RequestOptions:
    for key in options:
        if key not in {"request_deadline", "upgrade_tolerance"}:
            raise TypeError(f"unexpected keyword argument {key!r}")
    return options


def _iterate(
    page_fn: Callable[..., dict[str, Any]], starting_after: str | None, **filters: Any
) -> Iterator[Any]:
    seen: set[str] = set()
    if starting_after is not None:
        seen.add(starting_after)
    while True:
        listed = page_fn(starting_after=starting_after, **filters)
        ids = {item.id for item in listed["data"]}
        if seen.intersection(ids):
            raise ResponseValidationError("repeated page cursor")
        seen.update(ids)
        yield from listed["data"]
        if not listed["has_more"]:
            return
        starting_after = listed["data"][-1].id


class PhalaPay:
    """A client for one account and mode, authenticated with its API key.

        pay = PhalaPay(
            api_base="https://pay.example.com",
            api_key=os.environ["PHALA_PAY_KEY"],
            account="acct_…",
            forwarder=(FACTORY, IMPLEMENTATION),
            treasuries={1: "0x…your treasury on Ethereum"},
        )
        quote = pay.quotes.create(client_reference_id="team-42", amount=2500,
                                  chain_id=11155111, asset="pha")
        return {"client_secret": quote.client_secret, "expected_address": quote.address}

    `api_key` is a restricted key (`ppay_rk_…`, recommended for a production server) or a secret
    key (`ppay_sk_…`, for administration); the key selects the account and the mode. Every quote
    and deposit address is recomputed before it is returned from pins you configure yourself:
    `forwarder`, the `(factory, implementation)` pair pinned from the attested deployment;
    `treasuries` (`{chain_id: treasury}`), your own treasury per chain; and `account` (`acct_…`).
    The service's `treasury` is never trusted, and an address you cannot derive raises
    `AddressMismatchError` (fail closed). A live key requires all three pins: without them every
    address check raises. In test mode `account` is read from `GET /v1/account` when not given,
    and without `treasuries` the service's treasury is used with an `UnpinnedTreasuryWarning`.

    Requests that fail with a transport error, `429` (after its `Retry-After`), or `5xx` are
    retried with backoff; `POST`s reuse one `Idempotency-Key` across retries, so a retry never
    creates a second object, and a response the service saved for the key (even a `500`) is
    returned as it was, not retried. A failed request raises `ApiError` with the service's
    `code`, `param`, `doc_url`, and `request_id`.
    """

    def __init__(  # noqa: PLR0912, PLR0915
        self,
        api_key: str,
        *legacy: object,
        pins: Pins | str | None = None,
        api_base: str | None = None,
        timeout: float = 15.0,
        max_attempts: int = 4,
        request_deadline: float | None = None,
        upgrade_tolerance: bool = False,
        transport: httpx.BaseTransport | None = None,
        **legacy_kwargs: object,
    ) -> None:
        if legacy or legacy_kwargs:
            if pins is not None:
                raise ConfigurationError(
                    "pins cannot be combined with legacy constructor arguments"
                )
            if legacy:
                if len(legacy) != 1 or not isinstance(legacy[0], str):
                    raise ConfigurationError("legacy constructor requires api_base and api_key")
                api_key, api_base = legacy[0], api_key
            if api_base is None:
                raise ConfigurationError("legacy constructor requires api_base")
            forwarder = legacy_kwargs.pop("forwarder", None)
            treasuries = legacy_kwargs.pop("treasuries", None)
            account = legacy_kwargs.pop("account", None)
            if legacy_kwargs:
                raise ConfigurationError("unknown constructor argument")
            if api_key.startswith(LIVE_KEY_PREFIXES) and not (account and forwarder and treasuries):
                raise ConfigurationError(
                    "live mode requires pinned account, forwarder and treasuries"
                )
            self._pins = None
        else:
            if pins is None:
                raise ConfigurationError("pins is required")
            try:
                self._pins = parse_pins(pins) if isinstance(pins, str) else pins
            except (PinsError, ValueError) as error:
                raise ConfigurationError("invalid pins") from error
            if not isinstance(self._pins, Pins):
                raise ConfigurationError("invalid pins")
            if self._pins.livemode != key_livemode(api_key):
                raise ConfigurationError("API key mode does not match pins")
            if api_base is None:
                api_base = self._pins.api_base
            elif normalize_origin(api_base, test=not self._pins.livemode) != self._pins.api_base:
                raise ConfigurationError("api_base does not match pins")
            forwarder = (self._pins.factory, self._pins.implementation)
            treasuries = self._pins.treasuries
            account = self._pins.account
        self._client = TopupClient(
            api_base,
            api_key,
            account=cast(str | None, account),
            forwarder=cast(tuple[str, str] | None, forwarder),
            treasuries=cast(Mapping[int, str] | None, treasuries),
            timeout=timeout,
            max_attempts=max_attempts,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            transport=transport,
        )
        self.account = AccountResource(self._client)
        self.payment_settings = PaymentSettingsResource(self._client)
        self.config = ConfigResource(self._client)
        self.quotes = Quotes(self._client)
        self.deposits = Deposits(self._client)
        self.deposit_addresses = DepositAddresses(self._client)
        self.refunds = Refunds(self._client)
        self.balance = BalanceResource(self._client)
        self.sweeps = Sweeps(self._client)
        self.forwarders = Forwarders(self._client)
        self.treasuries = Treasuries(self._client)
        self.api_keys = ApiKeys(self._client)
        self.webhook_endpoints = WebhookEndpoints(self._client)
        self.events = Events(self._client)
        self.webhooks: Any = _BoundWebhook(self._pins) if self._pins else Webhook

    @classmethod
    def from_env(
        cls, env: Mapping[str, str] | None = None, *, api_base: str | None = None
    ) -> PhalaPay:
        values = os.environ if env is None else env
        try:
            key, encoded = values["PHALA_PAY_API_KEY"], values["PHALA_PAY_PINS"]
        except KeyError as exc:
            raise ConfigurationError(f"missing {exc.args[0]}") from None
        try:
            parsed = parse_pins(encoded)
        except (PinsError, ValueError) as error:
            raise ConfigurationError("invalid pins") from error
        return cls(key, pins=parsed, api_base=api_base)

    @property
    def pins(self) -> Pins:
        if self._pins is None:
            raise ConfigurationError("pins unavailable on legacy client")
        return self._pins

    @property
    def livemode(self) -> bool:
        return self._client.livemode

    def checkout_params(self, quote: Quote) -> dict[str, str]:
        issued = self._client._issued_quotes.get(id(quote))
        if (
            not isinstance(quote, Quote)
            or quote.status != "open"
            or not isinstance(quote.client_secret, str)
            or not quote.client_secret
            or issued is None
            or issued[0]() is not quote
            or issued[1] != quote.client_secret
            or self._pins is None
        ):
            raise ResponseValidationError(
                "quote must be an open quote created by this client with a client secret"
            )
        self._client._verify_identity(quote)
        self._client._checked(quote)
        return {
            "clientSecret": quote.client_secret,
            "expectedAddress": quote.address,
            "apiBase": str(self._client._client.get_httpx_client().base_url).rstrip("/"),
        }

    def export_account(self, directory: str | Path) -> dict[str, int]:
        """Writes every object of the key's account and mode to `directory`, one JSON file per
        resource, and returns the counts (design §13). `forwarders.json` keeps funds sweepable
        without Phala Pay."""
        return export_account(self._client, directory)

    def close(self) -> None:
        self._client.close()

    def __enter__(self) -> PhalaPay:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class AccountResource:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def retrieve(
        self,
        **options: Unpack[RequestOptions],
    ) -> AccountObject:
        """The key's account, in the key's mode."""
        return self._client.get_account(
            **_request_options(options),
        )

    def pause_quotes(
        self,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> AccountObject:
        """Stops issuing quotes, deposit addresses, and networks in both modes, for an
        emergency; existing addresses keep being credited."""
        return self._client.pause_quotes(
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def resume_quotes(
        self,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> AccountObject:
        """Lifts your own `quotes` pause; an operator's pause stays."""
        return self._client.resume_quotes(
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def roll_webhook_key(
        self,
        *,
        expires_in: int = 172_800,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> AccountObject:
        """Rolls this mode's webhook signing key; the old one signs beside it for `expires_in`
        seconds: 48 hours (the default) to 7 days live, `0` to 7 days in test mode."""
        return self._client.roll_webhook_key(
            expires_in=expires_in,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )


class PaymentSettingsResource:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def retrieve(
        self,
        **options: Unpack[RequestOptions],
    ) -> PaymentSettingsObject:
        """What the account accepts in the key's mode and on what terms, with the operator's
        catalog and bounds in `available`; a new account accepts nothing."""
        return self._client.get_payment_settings(
            **_request_options(options),
        )

    def update(
        self,
        *,
        chains: Sequence[Mapping[str, Any]] | Unset = UNSET,
        quote_creations_per_customer_per_minute: int | Unset | None = UNSET,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> PaymentSettingsObject:
        """Sets the chains and assets the account accepts, replacing the list when given, with a
        per-chain `confirmations` and per-asset terms within the operator's bounds; a parameter not
        given is unchanged. Send the complete configuration after a service restore."""
        return self._client.update_payment_settings(
            chains=chains,
            quote_creations_per_customer_per_minute=quote_creations_per_customer_per_minute,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )


class ConfigResource:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def retrieve(
        self,
        **options: Unpack[RequestOptions],
    ) -> Config:
        """The payable assets, limits, quote terms, and confirmations, for the product's UI."""
        return self._client.get_config(
            **_request_options(options),
        )


class Quotes:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def create(
        self,
        *,
        client_reference_id: str,
        amount: int,
        chain_id: int,
        asset: str,
        currency: str = "usd",
        idempotency_key: str | None = None,
        metadata: Mapping[str, str] | None = None,
        **options: Unpack[RequestOptions],
    ) -> Quote:
        """Quotes `amount` cents for the customer `client_reference_id`, payable in `asset` on
        `chain_id`.

        Only this response carries `client_secret`, the value the payer's browser needs, beside
        the recomputed `address` to pass as `<Checkout expectedAddress>`. Pass an
        `idempotency_key` of your own (for example your order id) to resume a checkout: for 24
        hours a repeat of the same request replays the first response, the same quote and the
        same `client_secret` included.

        `metadata` is Stripe's: up to 50 string pairs for your own use, such as your order id,
        copied to the deposit that pays the quote. Do not store sensitive information in it.
        """
        quote = self._client.create_quote(
            client_reference_id,
            amount,
            chain_id=chain_id,
            asset=asset,
            currency=currency,
            idempotency_key=idempotency_key,
            metadata=metadata,
            **_request_options(options),
        )
        issued_quotes = self._client._issued_quotes
        quote_key = id(quote)
        issued_quotes[quote_key] = (
            ref(quote, lambda _: issued_quotes.pop(quote_key, None)),
            quote.client_secret if isinstance(quote.client_secret, str) else None,
        )
        return quote

    def retrieve(
        self,
        quote_id: str,
        *,
        expand: Sequence[str] | None = None,
        **options: Unpack[RequestOptions],
    ) -> Quote:
        return self._client.get_quote(
            quote_id,
            expand=None if expand is None else list(expand),
            **_request_options(options),
        )

    def list(
        self,
        *,
        client_reference_id: str | None = None,
        status: QuoteStatus | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Iterator[Quote]:
        """Yields every matching quote, newest first, fetching pages as it goes."""
        return _iterate(
            self.list_page,
            starting_after,
            client_reference_id=client_reference_id,
            status=status,
            **_request_options(options),
            limit=limit,
        )

    def list_page(
        self,
        *,
        client_reference_id: str | None = None,
        status: QuoteStatus | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_quotes_page(
            client_reference_id=client_reference_id,
            status=status,
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )

    def update(
        self,
        quote_id: str,
        *,
        metadata: Metadata | None = None,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Quote:
        """Merges `metadata` into the quote's: a key set to `""` is unset, and `metadata=""`
        unsets every key."""
        return self._client.update_quote(
            quote_id,
            metadata=metadata,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def cancel(
        self,
        quote_id: str,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Quote:
        """Cancels an open quote no payment has reached; repeating it returns the canceled quote."""
        return self._client.cancel_quote(
            quote_id,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )


class Deposits:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    # Before `list`, whose name would shadow the builtin in later annotations.
    def retrieve(
        self,
        deposit_id: str,
        *,
        expand: Sequence[str] | None = None,
        **options: Unpack[RequestOptions],
    ) -> Deposit:
        return self._client.get_deposit(
            deposit_id,
            expand=None if expand is None else list(expand),
            **_request_options(options),
        )

    def update(
        self,
        deposit_id: str,
        *,
        metadata: Metadata | None = None,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Deposit:
        """Merges `metadata` into the deposit's, which started as a copy of its quote's or its
        deposit address's; theirs are left unchanged."""
        return self._client.update_deposit(
            deposit_id,
            metadata=metadata,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def list(
        self,
        *,
        client_reference_id: str | None = None,
        quote: str | None = None,
        deposit_address: str | None = None,
        status: DepositStatus | None = None,
        tx_hash: str | None = None,
        created_gt: int | None = None,
        created_gte: int | None = None,
        created_lt: int | None = None,
        created_lte: int | None = None,
        expand: Sequence[str] | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Iterator[Deposit]:
        """Yields every matching deposit, newest first, fetching pages as it goes. `created_*`
        are Unix seconds (Stripe's `created[gt|gte|lt|lte]`)."""
        return _iterate(
            self.list_page,
            starting_after,
            client_reference_id=client_reference_id,
            quote=quote,
            deposit_address=deposit_address,
            status=status,
            tx_hash=tx_hash,
            created_gt=created_gt,
            created_gte=created_gte,
            created_lt=created_lt,
            created_lte=created_lte,
            expand=None if expand is None else list(expand),
            **_request_options(options),
            limit=limit,
        )

    def list_page(
        self,
        *,
        client_reference_id: str | None = None,
        quote: str | None = None,
        deposit_address: str | None = None,
        status: DepositStatus | None = None,
        tx_hash: str | None = None,
        created_gt: int | None = None,
        created_gte: int | None = None,
        created_lt: int | None = None,
        created_lte: int | None = None,
        expand: Sequence[str] | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_deposits_page(
            client_reference_id=client_reference_id,
            quote=quote,
            deposit_address=deposit_address,
            status=status,
            tx_hash=tx_hash,
            created_gt=created_gt,
            created_gte=created_gte,
            created_lt=created_lt,
            created_lte=created_lte,
            expand=None if expand is None else list(expand),
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )


class DepositAddresses:
    """A customer's persistent deposit address: one address for every supported token on every
    supported network. Show it like a bank account number; any amount of a supported token sent
    to it is credited at the market rate when it arrives. `networks` lists each chain's address
    (the same wherever the treasury is the same; `address` is it when all agree) and tokens, and
    `payments` the transfers seen and recorded in the last 24 hours.

    `topup_sdk.deposit_address(...)` recomputes any version offline from its salt inputs and a
    network's treasury.
    """

    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def create(
        self,
        *,
        client_reference_id: str,
        metadata: Metadata | None = None,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> DepositAddress:
        """Returns the customer's active address, recomputed; the same call keeps returning it
        until it is rotated, and adds a network supported since. Each response carries a new
        `client_secret` for the customer's page (`<DepositAddress clientSecret>`) to follow
        payments. `metadata` is merged into the address's; each deposit to it starts with a
        copy, and a rotation carries it to the next address."""
        return self._client.create_deposit_address(
            client_reference_id,
            metadata=metadata,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def update(
        self,
        deposit_address_id: str,
        *,
        metadata: Metadata | None = None,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> DepositAddress:
        """Merges `metadata` into the address's (a key set to `""` is unset, `""` unsets all)."""
        return self._client.update_deposit_address(
            deposit_address_id,
            metadata=metadata,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def retrieve(
        self,
        deposit_address_id: str,
        **options: Unpack[RequestOptions],
    ) -> DepositAddress:
        return self._client.get_deposit_address(
            deposit_address_id,
            **_request_options(options),
        )

    def list(
        self,
        *,
        client_reference_id: str | None = None,
        status: DepositAddressStatus | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Iterator[DepositAddress]:
        """Yields every matching deposit address, newest first, fetching pages as it goes."""
        return _iterate(
            self.list_page,
            starting_after,
            client_reference_id=client_reference_id,
            status=status,
            **_request_options(options),
            limit=limit,
        )

    def list_page(
        self,
        *,
        client_reference_id: str | None = None,
        status: DepositAddressStatus | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_deposit_addresses_page(
            client_reference_id=client_reference_id,
            status=status,
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )

    def rotate(
        self,
        deposit_address_id: str,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> DepositAddress:
        """Retires the address and returns the customer's new one, a new address on every
        network; the retired address is still credited, so stop showing it rather than telling
        the customer it is invalid."""
        return self._client.rotate_deposit_address(
            deposit_address_id,
            idempotency_key=idempotency_key,
            **_request_options(options),
        )


class Refunds:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def create(
        self,
        *,
        deposit: str,
        destination_address: str,
        amount_atomic: int | None = None,
        idempotency_key: str | None = None,
        metadata: Mapping[str, str] | None = None,
        **options: Unpack[RequestOptions],
    ) -> Refund:
        """Creates a pending refund of a final `deposit` (its unrefunded remainder unless
        `amount_atomic` is given) to an address the customer controls; pay it from its
        `treasury`, then call `mark_paid`."""
        return self._client.create_refund(
            deposit,
            destination_address,
            amount_atomic,
            idempotency_key=idempotency_key,
            metadata=metadata,
            **_request_options(options),
        )

    def mark_paid(
        self,
        refund_id: str,
        *,
        transaction_hash: str,
        receipt_log_index: int | None = None,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Refund:
        """Attaches the transaction that pays the refund; it is verified at finality. From then
        on the refund cannot be canceled: it stays pending until it succeeds, or fails when the
        transaction does not pay it or is proven dropped."""
        return self._client.mark_refund_paid(
            refund_id,
            transaction_hash,
            receipt_log_index=receipt_log_index,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def cancel(
        self,
        refund_id: str,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Refund:
        """Cancels a pending refund."""
        return self._client.cancel_refund(
            refund_id,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def retrieve(
        self,
        refund_id: str,
        *,
        expand: Sequence[str] | None = None,
        **options: Unpack[RequestOptions],
    ) -> Refund:
        return self._client.get_refund(
            refund_id,
            expand=None if expand is None else list(expand),
            **_request_options(options),
        )

    def update(
        self,
        refund_id: str,
        *,
        metadata: Metadata | None = None,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Refund:
        """Merges `metadata` into the refund's."""
        return self._client.update_refund(
            refund_id,
            metadata=metadata,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def list(
        self,
        *,
        deposit: str | None = None,
        status: RefundStatus | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Iterator[Refund]:
        """Yields every matching refund, newest first, fetching pages as it goes."""
        return _iterate(
            self.list_page,
            starting_after,
            deposit=deposit,
            status=status,
            **_request_options(options),
            limit=limit,
        )

    def list_page(
        self,
        *,
        deposit: str | None = None,
        status: RefundStatus | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_refunds_page(
            deposit=deposit,
            status=status,
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )


class BalanceResource:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def retrieve(
        self,
        **options: Unpack[RequestOptions],
    ) -> Balance:
        """What the account's forwarders hold, per chain and token, and the final part of it."""
        return self._client.get_balance(
            **_request_options(options),
        )


class Sweeps:
    """Sweeps: finalized `Flushed` events that moved a forwarder's balance to its treasury.
    Build the `flush` calls offline with `topup_sdk.flush_transactions` from
    `forwarders.list(sweepable=token)`, and for a Safe treasury write them with
    `topup_sdk.safe_batch`."""

    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def list(
        self,
        *,
        chain_id: int | None = None,
        forwarder: str | None = None,
        token: str | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Iterator[Sweep]:
        """Yields the sweeps, newest first, fetching pages as it goes."""
        return _iterate(
            self.list_page,
            starting_after,
            chain_id=chain_id,
            forwarder=forwarder,
            token=token,
            **_request_options(options),
            limit=limit,
        )

    def list_page(
        self,
        *,
        chain_id: int | None = None,
        forwarder: str | None = None,
        token: str | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_sweeps_page(
            chain_id=chain_id,
            forwarder=forwarder,
            token=token,
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )


class Forwarders:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def list(
        self,
        *,
        chain_id: int | None = None,
        quote: str | None = None,
        deposit_address: str | None = None,
        sweepable: str | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Iterator[Forwarder]:
        """Yields the account's forwarders with their `(factory, salt, treasury)`: all of them, or
        those of one `quote` or `deposit_address`; with `sweepable` (a token contract), only those
        safe to sweep of it."""
        return _iterate(
            self.list_page,
            starting_after,
            chain_id=chain_id,
            quote=quote,
            deposit_address=deposit_address,
            sweepable=sweepable,
            **_request_options(options),
            limit=limit,
        )

    def list_page(
        self,
        *,
        chain_id: int | None = None,
        quote: str | None = None,
        deposit_address: str | None = None,
        sweepable: str | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_forwarders_page(
            chain_id=chain_id,
            quote=quote,
            deposit_address=deposit_address,
            sweepable=sweepable,
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )


class Treasuries:
    """Treasuries, proven through the API (design D10): an EOA signs the challenge with
    `set_eoa`; a Safe's owners sign it as a Safe message and `create` submits it
    (docs/integration.md, "Set a Safe as treasury")."""

    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def challenge(
        self,
        *,
        chain_id: int,
        address: str,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> TreasuryChallenge:
        """The EIP-4361 message that proves `address` as the treasury of `chain_id`."""
        return self._client.create_treasury_challenge(
            chain_id,
            address,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def create(
        self,
        *,
        chain_id: int,
        message: str,
        signature: str,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Treasury:
        """Submits a signed challenge `message`."""
        return self._client.create_treasury(
            chain_id,
            message,
            signature,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def set_eoa(self, *, chain_id: int, address: str, private_key: str | bytes) -> Treasury:
        """Proves the EOA `address` as the treasury of `chain_id`: requests a challenge, signs it
        with `private_key` (which must be `address`'s; needs `phala-pay[eoa]`), and submits it."""
        challenge = self.challenge(chain_id=chain_id, address=address)
        signature = sign_treasury_challenge(challenge.message, private_key, address=address)
        return self.create(chain_id=chain_id, message=challenge.message, signature=signature)

    def retrieve(
        self,
        treasury_id: str,
        **options: Unpack[RequestOptions],
    ) -> Treasury:
        return self._client.get_treasury(
            treasury_id,
            **_request_options(options),
        )

    def list(
        self,
        *,
        chain_id: int | None = None,
        status: TreasuryStatus | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> list[Treasury]:
        """The treasuries of this mode, newest first."""
        return list(
            _iterate(
                self.list_page,
                starting_after,
                chain_id=chain_id,
                status=status,
                **_request_options(options),
                limit=limit,
            )
        )

    def list_page(
        self,
        *,
        chain_id: int | None = None,
        status: TreasuryStatus | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_treasuries_page(
            chain_id=chain_id,
            status=status,
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )

    def cancel(
        self,
        treasury_id: str,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Treasury:
        """Cancels a pending treasury change before it applies."""
        return self._client.cancel_treasury(
            treasury_id,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def pause(
        self,
        treasury_id: str,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Treasury:
        """Pauses crediting of deposits to every forwarder over the treasury, for an incident
        such as a compromised former treasury: they stay `pending` until `resume`."""
        return self._client.pause_treasury(
            treasury_id,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def resume(
        self,
        treasury_id: str,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Treasury:
        """Lifts your crediting pause; an operator's pause stays."""
        return self._client.resume_treasury(
            treasury_id,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )


class ApiKeys:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def create(
        self,
        *,
        name: str = "",
        permissions: Sequence[str] | None = None,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> ApiKeyObject:
        """A new key of this mode; its `secret` is in this response only. With `permissions`
        (such as `["quotes.write", "deposit_addresses.write", "deposits.read", "events.read",
        "refunds.read", "account.read"]`) it is a restricted key, `ppay_rk_…`, which never
        manages keys, treasuries, webhook endpoints, webhook keys, or account settings: run
        production with one and keep the secret key offline."""
        return self._client.create_api_key(
            name=name,
            permissions=permissions,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def retrieve(
        self,
        api_key_id: str,
        **options: Unpack[RequestOptions],
    ) -> ApiKeyObject:
        return self._client.get_api_key(
            api_key_id,
            **_request_options(options),
        )

    def list(
        self,
        *,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> list[ApiKeyObject]:
        """This mode's keys, without their secrets."""
        return list(
            _iterate(
                self.list_page,
                starting_after,
                **_request_options(options),
                limit=limit,
            )
        )

    def list_page(
        self,
        *,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_api_keys_page(
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )

    def roll(
        self,
        api_key_id: str,
        *,
        expires_in: int = 0,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> ApiKeyObject:
        """A replacement key of the same kind and permissions with its `secret`; the old one
        works for `expires_in` seconds."""
        return self._client.roll_api_key(
            api_key_id,
            expires_in=expires_in,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def revoke(
        self,
        api_key_id: str,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> ApiKeyObject:
        return self._client.revoke_api_key(
            api_key_id,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )


class WebhookEndpoints:
    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def create(
        self,
        *,
        url: str,
        enabled_events: list[str],
        description: str | None = None,
        metadata: Mapping[str, str] | None = None,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> WebhookEndpointObject:
        return self._client.create_webhook_endpoint(
            url,
            enabled_events,
            description=description,
            metadata=metadata,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def retrieve(
        self,
        endpoint_id: str,
        **options: Unpack[RequestOptions],
    ) -> WebhookEndpointObject:
        return self._client.get_webhook_endpoint(
            endpoint_id,
            **_request_options(options),
        )

    def update(
        self,
        endpoint_id: str,
        *,
        url: str | None = None,
        enabled_events: list[str] | None = None,
        description: str | None = None,
        disabled: bool | None = None,
        metadata: Metadata | None = None,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> WebhookEndpointObject:
        return self._client.update_webhook_endpoint(
            endpoint_id,
            url=url,
            enabled_events=enabled_events,
            description=description,
            disabled=disabled,
            metadata=metadata,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def delete(
        self,
        endpoint_id: str,
        **options: Unpack[RequestOptions],
    ) -> DeletedWebhookEndpoint:
        return self._client.delete_webhook_endpoint(
            endpoint_id,
            **_request_options(options),
        )

    def test(
        self,
        endpoint_id: str,
        *,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> EventObjectResponse:
        """Sends a test event to the endpoint."""
        return self._client.test_webhook_endpoint(
            endpoint_id,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def list(
        self,
        *,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Iterator[WebhookEndpointObject]:
        return _iterate(
            self.list_page,
            starting_after,
            **_request_options(options),
            limit=limit,
        )

    def list_page(
        self,
        *,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_webhook_endpoints_page(
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )


class Events:
    """Events of this mode: every webhook ever sent, and the account's audit log."""

    def __init__(self, client: TopupClient) -> None:
        self._client = client

    def retrieve(
        self,
        event_id: str,
        **options: Unpack[RequestOptions],
    ) -> EventObjectResponse:
        return self._client.get_event(
            event_id,
            **_request_options(options),
        )

    def resend(
        self,
        event_id: str,
        *,
        webhook_endpoint: str,
        idempotency_key: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> EventObjectResponse:
        """Delivers the event again to one enabled endpoint."""
        return self._client.resend_event(
            event_id,
            webhook_endpoint=webhook_endpoint,
            **_request_options(options),
            idempotency_key=idempotency_key,
        )

    def list(
        self,
        *,
        type: EventType | str | None = None,
        types: Sequence[EventType | str] | None = None,
        delivery_success: bool | None = None,
        created_gt: int | None = None,
        created_gte: int | None = None,
        created_lt: int | None = None,
        created_lte: int | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> Iterator[EventObjectResponse]:
        """Yields events, newest first. `type` filters by one type, such as `deposit.reversed`,
        or a group, `deposit.*`; `types` by up to 20. `delivery_success=False` yields the events
        a webhook endpoint has not received: its `pending_deliveries`, `oldest_pending_at`, and
        `last_attempt` say whether it is keeping up; resend the missed events once it is fixed.
        `data.object` is the object as it was when the event happened; `*.updated` events carry
        `data.previous_attributes`, and events caused by your requests their `request`."""
        return _iterate(
            self.list_page,
            starting_after,
            type=type,
            types=types,
            delivery_success=delivery_success,
            created_gt=created_gt,
            created_gte=created_gte,
            created_lt=created_lt,
            created_lte=created_lte,
            **_request_options(options),
            limit=limit,
        )

    def list_page(
        self,
        *,
        type: EventType | str | None = None,
        types: Sequence[EventType | str] | None = None,
        delivery_success: bool | None = None,
        created_gt: int | None = None,
        created_gte: int | None = None,
        created_lt: int | None = None,
        created_lte: int | None = None,
        limit: int = 100,
        starting_after: str | None = None,
        **options: Unpack[RequestOptions],
    ) -> dict[str, Any]:
        return self._client.list_events_page(
            type=type,
            types=types,
            delivery_success=delivery_success,
            created_gt=created_gt,
            created_gte=created_gte,
            created_lt=created_lt,
            created_lte=created_lte,
            **_request_options(options),
            limit=limit,
            starting_after=starting_after,
        )


class _BoundWebhook:
    def __init__(self, pins: Pins) -> None:
        self._pins = pins

    def construct_event(
        self, payload: bytes | str, headers: Mapping[str, str], *, tolerance: int | float = 300
    ) -> Any:
        if type(tolerance) not in (int, float) or not math.isfinite(tolerance) or tolerance < 0:
            raise ConfigurationError("tolerance must be finite and non-negative")
        if not isinstance(payload, bytes | str):
            raise SignatureVerificationError("webhook requires original bytes or exact UTF-8 text")
        try:
            event = Webhook.construct_event(
                payload,
                headers,
                [key for _, key in self._pins.webhook_keys],
                self._pins.account,
                expected_livemode=self._pins.livemode,
                tolerance=tolerance,
            )
            resource = event.data.object
            if (
                isinstance(resource, Deposit | Quote | Refund)
                and resource.livemode != self._pins.livemode
            ):
                raise SignatureVerificationError("webhook resource is for the other mode")
            return event
        except (ValueError, TypeError, KeyError, AttributeError):
            pass
        raise SignatureVerificationError("malformed webhook envelope or resource")
