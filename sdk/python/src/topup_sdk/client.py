"""Retrying wrapper over the generated `topup_client` package, authenticated with an API key.

Every request sends `Authorization: Bearer ppay_sk_…` (a secret key) or `ppay_rk_…` (a restricted
key, which holds only the permissions it was created with); the key selects the account and the
mode. Run production servers with a restricted key and keep secret keys for administration.
Every operation exposed here is idempotent on the service side, so the wrapper retries transport
failures, transient statuses, and `409 idempotency_key_in_use`:

- `create_quote`: sends an `Idempotency-Key` (generated unless given) and reuses it on every
  retry, so a retry returns the quote the first attempt created.
- `cancel_quote`: canceling a canceled quote returns it unchanged.
- `create_refund`: sends an `Idempotency-Key` like `create_quote`.
- `create_deposit_address`: returns the customer's active address, so a repeat returns the same
  one; `rotate_deposit_address` sends an `Idempotency-Key` like `create_quote`.
- `mark_refund_paid`: attaching the same transaction again returns the refund.
- `cancel_refund`: canceling a canceled refund returns it unchanged.
- `update_quote`, `update_deposit`, `update_refund`: merging the same `metadata` again leaves the
  object as the first attempt did.
- Every other `POST` (keys, webhook endpoints, treasuries, the account) sends one
  `Idempotency-Key` for all its attempts, so a retry returns the first attempt's response.

`metadata` is Stripe's: up to 50 string pairs, keys of up to 40 characters without square
brackets, values of up to 500 characters. On an update a key set to `""` is unset and
`metadata=""` unsets every key. A quote's metadata is copied to the deposit that pays it. Do not
store sensitive information in it.

Every quote and deposit address is recomputed before it is returned from the pins the merchant
configures itself: `account` (its `acct_` id), `forwarder` (the `(factory, implementation)` pair of
the attested deployment), and `treasuries` (its own treasury per chain, as it proved them). An open
quote's address is derived from the pinned treasury of its chain, the account, the customer, and
the quote id; every network of an active deposit address from its chain's pinned treasury and salt
inputs. The response's `treasury` is never trusted: a compromised service could return another
treasury with its matching address. A mismatch, a response naming another treasury, or a chain
without a pinned treasury raises `AddressMismatchError` rather than return an address the merchant
did not derive.

In live mode the check is mandatory and fails closed: without `account`, `forwarder`, and
`treasuries`, every address check raises. In test mode, without `forwarder` nothing is checked,
and without `treasuries` the response's treasury is used with an `UnpinnedTreasuryWarning`.

`Quote.payment` reports a transfer as soon as it is seen on chain. It is display only, and a reorg
can remove it: credit comes from the `deposit.credited` webhook (or `list_deposits`). A deposit is
credited at its route's confirmations, before finality, while the account's credit that is not
final stays within its `max_unfinalized_credit` (past it, once final); a reorg before finality
reverses it (`deposit.reversed`).
"""

from __future__ import annotations

import math
import random
import time
import uuid
import warnings
from collections.abc import Callable, Iterator, Mapping, Sequence
from email.utils import parsedate_to_datetime
from functools import partial
from typing import Any, Literal, TypeVar
from weakref import ReferenceType

import httpx

from topup_client import AuthenticatedClient
from topup_client.api.account import (
    get_account,
    pause_account,
    resume_account,
    roll_webhook_key,
)
from topup_client.api.api_keys import (
    create_api_key,
    get_api_key,
    list_api_keys,
    revoke_api_key,
    roll_api_key,
)
from topup_client.api.attestation import get_attestation
from topup_client.api.balance import get_balance
from topup_client.api.config import get_config
from topup_client.api.deposit_addresses import (
    create_deposit_address,
    get_deposit_address,
    list_deposit_addresses,
    rotate_deposit_address,
    update_deposit_address,
)
from topup_client.api.deposits import get_deposit, list_deposits, update_deposit
from topup_client.api.events import get_event, list_events, resend_event
from topup_client.api.forwarders import list_forwarders
from topup_client.api.payment_settings import get_payment_settings, update_payment_settings
from topup_client.api.quotes import (
    cancel_quote,
    create_quote,
    get_quote,
    list_quotes,
    update_quote,
)
from topup_client.api.refunds import (
    cancel_refund,
    create_refund,
    get_refund,
    list_refunds,
    mark_refund_paid,
    update_refund,
)
from topup_client.api.sweeps import list_sweeps
from topup_client.api.treasuries import (
    cancel_treasury,
    create_treasury,
    create_treasury_challenge,
    get_treasury,
    list_treasuries,
    pause_treasury,
    resume_treasury,
)
from topup_client.api.webhook_endpoints import (
    create_webhook_endpoint,
    delete_webhook_endpoint,
    get_webhook_endpoint,
    list_webhook_endpoints,
    test_webhook_endpoint,
    update_webhook_endpoint,
)
from topup_client.models import (
    AccountObject,
    AccountSelfPauseRequest,
    ApiKeyList,
    ApiKeyObject,
    AttestationResponse,
    Balance,
    Config,
    CreateApiKeyRequest,
    CreateDepositAddressRequest,
    CreateQuoteRequest,
    CreateRefundRequest,
    CreateTreasuryChallengeRequest,
    CreateTreasuryRequest,
    CreateWebhookEndpointRequest,
    DeletedWebhookEndpoint,
    Deposit,
    DepositAddress,
    DepositAddressList,
    DepositList,
    EventList,
    EventObjectResponse,
    Forwarder,
    ForwarderList,
    MarkRefundPaidRequest,
    MetadataClear,
    MetadataParamType0,
    PaymentSettingsChain,
    PaymentSettingsObject,
    Quote,
    QuoteList,
    Refund,
    RefundList,
    ResendEventRequest,
    RollApiKeyRequest,
    RollWebhookKeyRequest,
    Sweep,
    SweepList,
    Treasury,
    TreasuryChallenge,
    TreasuryList,
    UpdateMetadataRequest,
    UpdatePaymentSettingsRequest,
    UpdateWebhookEndpointRequest,
    WebhookEndpointList,
    WebhookEndpointObject,
)
from topup_client.types import UNSET, Response, Unset

from ._origin import normalize_origin
from ._secrets import Secret, protect, redact
from ._transport import REQUEST_STATE, BorrowedTransport, HTTPClient, RequestState
from .addresses import deposit_address, quote_address, same_address
from .attestation import verify_attestation_binding
from .errors import (
    AddressMismatchError,
    ApiError,
    ConfigurationError,
    ResponseValidationError,
    TransportError,
    UnpinnedTreasuryWarning,
)
from .signing import sf_string

T = TypeVar("T")

# A `POST` retried with its `Idempotency-Key` after a `500` gets the saved `500` back, marked
# `Idempotent-Replayed`, which is not retried again.
RETRYABLE_STATUSES = frozenset({429, 500, 502, 503, 504})

API_KEY_PREFIXES = ("ppay_sk_test_", "ppay_sk_live_", "ppay_rk_test_", "ppay_rk_live_")
LIVE_KEY_PREFIXES = ("ppay_sk_live_", "ppay_rk_live_")

Metadata = Mapping[str, str] | Literal[""]
"""A `metadata` parameter: string pairs, where `""` unsets a key, or `""` to unset every key."""


class TopupClient:
    """Merchant API client authenticated with an API key: a secret key (`ppay_sk_test_…`,
    `ppay_sk_live_…`) or a restricted key (`ppay_rk_test_…`, `ppay_rk_live_…`).

    `account` (`acct_…`), `forwarder` (the `(factory, implementation)` pair pinned from the attested
    deployment, as the webhook keys are), and `treasuries` (`{chain_id: treasury}`, your own
    treasury per chain) are the pins every quote and deposit address is recomputed from before it
    is returned. A live key requires all three: an address check without them raises
    `AddressMismatchError`. In test mode `account` is read once from `GET /v1/account` when not
    given, and an unpinned treasury falls back to the response's with a warning.
    """

    def __init__(
        self,
        base_url: str,
        api_key: str,
        *,
        account: str | None = None,
        forwarder: tuple[str, str] | None = None,
        treasuries: Mapping[int, str] | None = None,
        timeout: float = 15.0,
        max_attempts: int = 4,
        request_deadline: float | None = None,
        upgrade_tolerance: bool = False,
        initial_backoff: float = 0.5,
        transport: httpx.BaseTransport | None = None,
        sleep: Callable[[float], None] = time.sleep,
        rng: Callable[[], float] = random.random,
        clock: Callable[[], float] = time.monotonic,
        wall_clock: Callable[[], float] = time.time,
    ) -> None:
        if type(max_attempts) is not int or not 1 <= max_attempts <= 10:
            raise ConfigurationError("max_attempts must be an integer from 1 to 10")
        for name, value in (
            ("timeout", timeout),
            ("request_deadline", 60.0 if request_deadline is None else request_deadline),
            ("initial_backoff", initial_backoff),
        ):
            if type(value) not in (int, float) or not math.isfinite(value) or value <= 0:
                raise ConfigurationError(f"{name} must be finite and positive")
        if not api_key.startswith(API_KEY_PREFIXES):
            raise ValueError(
                "an API key is a secret key, ppay_sk_test_… or ppay_sk_live_…, or a restricted "
                "key, ppay_rk_test_… or ppay_rk_live_…"
            )
        if treasuries is not None and forwarder is None:
            raise ValueError("pinning treasuries needs the forwarder to recompute addresses")
        self.livemode = api_key.startswith(LIVE_KEY_PREFIXES)
        """Whether the key is a live key, which requires every address pin."""
        self._issued_quotes: dict[int, tuple[ReferenceType[Quote], str | None]] = {}
        self._account = account
        self._account_pinned = account is not None
        self.forwarder = forwarder
        self.treasuries = None if treasuries is None else dict(treasuries)
        if type(upgrade_tolerance) is not bool:
            raise ConfigurationError("upgrade_tolerance must be a boolean")
        self._upgrade_tolerance = upgrade_tolerance
        self._request_deadline = request_deadline
        self._max_attempts = max_attempts
        self._initial_backoff = initial_backoff
        self._sleep = sleep
        self._rng = rng
        self._clock = clock
        self._wall_clock = wall_clock
        base_url = normalize_origin(base_url, test=not self.livemode)
        http = HTTPClient(
            attempt_timeout=timeout,
            clock=clock,
            key_factory=_idempotency_key,
            on_response=_raise_error_response,
            base_url=base_url,
            headers={"Authorization": Secret(f"Bearer {api_key}")},
            timeout=timeout,
            follow_redirects=False,
            transport=BorrowedTransport(transport) if transport is not None else None,
        )
        self._client = AuthenticatedClient(
            base_url=base_url, token=Secret(api_key), raise_on_unexpected_status=False
        ).set_httpx_client(http)

    def close(self) -> None:
        """Closes the underlying connection pool."""
        self._client.get_httpx_client().close()

    def __enter__(self) -> TopupClient:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def get_account(
        self,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> AccountObject:
        """Returns the key's account, in the key's mode."""
        return self._call(
            lambda: get_account.sync_detailed(client=self._client),
            AccountObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def roll_webhook_key(
        self,
        *,
        expires_in: int = 172_800,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> AccountObject:
        """Rolls this mode's webhook signing key: the next version signs from now on, and the
        current one keeps signing beside it for `expires_in` seconds: 48 hours (the default) to 7
        days in live mode, so a leaked key cannot cut off the key you pinned; test mode also
        accepts `0`, which stops it at once. The roll's `account.updated` notice is signed by the
        retiring key too. Pin the new public key from `attestation` before the old one expires.
        Retries reuse one `Idempotency-Key`, so they never roll twice."""
        body = RollWebhookKeyRequest(expires_in=expires_in)
        return self._call(
            lambda: roll_webhook_key.sync_detailed(client=self._client, body=body),
            AccountObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def get_payment_settings(
        self,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> PaymentSettingsObject:
        """The account's payment settings in the key's mode: the chains and assets it accepts and
        its terms on each, with the operator's catalog and bounds in `available`. A new account
        accepts nothing until it is configured."""
        return self._call(
            lambda: get_payment_settings.sync_detailed(client=self._client),
            PaymentSettingsObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def update_payment_settings(
        self,
        *,
        chains: Sequence[Mapping[str, Any]] | Unset = UNSET,
        quote_creations_per_customer_per_minute: int | Unset | None = UNSET,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> PaymentSettingsObject:
        """Updates the account's payment settings in the key's mode; a parameter not given is
        unchanged. `chains`, when given, replaces the whole list: each
        `{"chain_id", "confirmations"?, "assets": [{"asset", <terms>?}]}`, where a term not given
        takes the operator's default, and `[]` accepts nothing.
        `quote_creations_per_customer_per_minute=None` restores its default. Writes are
        last-write-wins; after a service restore, send the complete configuration to reconfirm
        it. Retries reuse one `Idempotency-Key`."""
        body = UpdatePaymentSettingsRequest(
            chains=UNSET
            if isinstance(chains, Unset)
            else [PaymentSettingsChain.from_dict(dict(chain)) for chain in chains],
            quote_creations_per_customer_per_minute=quote_creations_per_customer_per_minute,
        )
        return self._call(
            lambda: update_payment_settings.sync_detailed(client=self._client, body=body),
            PaymentSettingsObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def pause_quotes(
        self,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> AccountObject:
        """Pauses the account's `quotes` in both modes: no quote, deposit address, or network is
        issued until `resume_quotes`, for an emergency such as a leaked key during a treasury
        time-lock. Payments to existing addresses keep being credited."""
        body = AccountSelfPauseRequest(scopes=["quotes"])
        return self._call(
            lambda: pause_account.sync_detailed(client=self._client, body=body),
            AccountObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def resume_quotes(
        self,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> AccountObject:
        """Lifts your own `quotes` pause; an operator's pause stays in `paused_scopes`."""
        body = AccountSelfPauseRequest(scopes=["quotes"])
        return self._call(
            lambda: resume_account.sync_detailed(client=self._client, body=body),
            AccountObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def account_id(self) -> str:
        """The key's account id, `acct_…`, read once."""
        if self._account is None:
            self._account = self.get_account().id
        return self._account

    def get_config(
        self,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> Config:
        """Returns the payable assets, limits, and quote terms the product's UI shows."""
        return self._call(
            lambda: get_config.sync_detailed(client=self._client),
            Config,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def create_quote(
        self,
        client_reference_id: str,
        amount: int,
        *,
        chain_id: int,
        asset: str,
        currency: str = "usd",
        idempotency_key: str | None = None,
        metadata: Mapping[str, str] | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> Quote:
        """Quotes `amount` minor units (cents) for the customer `client_reference_id`, payable in
        `asset` on `chain_id`.

        Retries reuse one `Idempotency-Key`, so they return the quote the first attempt created;
        pass your own key to make a retry after a crash safe too. The deposit that pays the quote
        starts with a copy of its `metadata`.
        """
        body = CreateQuoteRequest(
            client_reference_id=client_reference_id,
            amount=amount,
            currency=currency,
            chain_id=chain_id,
            asset=asset,
            metadata=_metadata(metadata),
        )
        quote = self._call(
            lambda: create_quote.sync_detailed(client=self._client, body=body),
            Quote,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )
        return self._checked(quote)

    def get_quote(
        self,
        quote_id: str,
        *,
        expand: list[str] | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> Quote:
        """Returns a quote, for example to resume a checkout page."""
        quote = self._call(
            lambda: get_quote.sync_detailed(quote_id, client=self._client, expand=_unset(expand)),
            Quote,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )
        return self._checked(quote)

    def list_quotes(
        self,
        *,
        client_reference_id: str | None = None,
        status: str | None = None,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> Iterator[Quote]:
        """Yields the account's quotes, newest first, following every page. They are not
        recomputed when open; historical quote addresses are not payable."""
        return self._paginate(
            lambda cursor: self.list_quotes_page(
                client_reference_id=client_reference_id,
                status=status,
                request_deadline=request_deadline,
                upgrade_tolerance=upgrade_tolerance,
                limit=page_size,
                starting_after=cursor,
            ),
            starting_after=starting_after,
        )

    def update_quote(
        self,
        quote_id: str,
        *,
        metadata: Metadata | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Quote:
        """Merges `metadata` into the quote's, in any status."""
        body = UpdateMetadataRequest(metadata=_metadata(metadata))
        quote = self._call(
            lambda: update_quote.sync_detailed(quote_id, client=self._client, body=body),
            Quote,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )
        return self._checked(quote)

    def cancel_quote(
        self,
        quote_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Quote:
        """Cancels an open, unpaid quote; later payments to its address are credited at spot."""
        quote = self._call(
            lambda: cancel_quote.sync_detailed(quote_id, client=self._client),
            Quote,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )
        return self._checked(quote)

    def list_deposits(
        self,
        *,
        client_reference_id: str | None = None,
        quote: str | None = None,
        deposit_address: str | None = None,
        status: str | None = None,
        tx_hash: str | None = None,
        created_gt: int | None = None,
        created_gte: int | None = None,
        created_lt: int | None = None,
        created_lte: int | None = None,
        expand: list[str] | None = None,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> Iterator[Deposit]:
        """Yields the account's deposits matching the filters, newest first, following every page
        (Stripe's auto-pagination). `created_*` are Unix seconds; `expand` may name `data.quote`.
        A deposit is `final` once it can no longer be reversed."""
        return self._paginate(
            lambda cursor: self.list_deposits_page(
                client_reference_id=client_reference_id,
                quote=quote,
                deposit_address=deposit_address,
                status=status,
                tx_hash=tx_hash,
                created_gt=created_gt,
                created_gte=created_gte,
                created_lt=created_lt,
                created_lte=created_lte,
                expand=expand,
                request_deadline=request_deadline,
                upgrade_tolerance=upgrade_tolerance,
                limit=page_size,
                starting_after=cursor,
            ),
            starting_after=starting_after,
        )

    def get_deposit(
        self,
        deposit_id: str,
        *,
        expand: list[str] | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> Deposit:
        """Returns one of the product's deposits; `expand` may name `quote`."""
        return self._call(
            lambda: get_deposit.sync_detailed(
                deposit_id, client=self._client, expand=_unset(expand)
            ),
            Deposit,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def update_deposit(
        self,
        deposit_id: str,
        *,
        metadata: Metadata | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Deposit:
        """Merges `metadata` into the deposit's; the quote's is left unchanged."""
        body = UpdateMetadataRequest(metadata=_metadata(metadata))
        return self._call(
            lambda: update_deposit.sync_detailed(deposit_id, client=self._client, body=body),
            Deposit,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def create_deposit_address(
        self,
        client_reference_id: str,
        *,
        metadata: Metadata | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> DepositAddress:
        """Returns the customer's active deposit address, one address for every supported token on
        every supported network (`networks`), issuing it the first time. Any amount of a supported
        token sent to it is credited at spot when it arrives. `metadata` is merged into the
        address's, and each deposit to it starts with a copy."""
        body = CreateDepositAddressRequest(
            client_reference_id=client_reference_id,
            metadata=_metadata(metadata),
        )
        address = self._call(
            lambda: create_deposit_address.sync_detailed(client=self._client, body=body),
            DepositAddress,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )
        return self._checked_deposit_address(address)

    def get_deposit_address(
        self,
        deposit_address_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> DepositAddress:
        """Returns one deposit address, active or retired."""
        address = self._call(
            lambda: get_deposit_address.sync_detailed(deposit_address_id, client=self._client),
            DepositAddress,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )
        return self._checked_deposit_address(address)

    def list_deposit_addresses(
        self,
        *,
        client_reference_id: str | None = None,
        status: str | None = None,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> Iterator[DepositAddress]:
        """Yields the matching deposit addresses, newest first, following every page."""
        return self._paginate(
            lambda cursor: self.list_deposit_addresses_page(
                client_reference_id=client_reference_id,
                status=status,
                request_deadline=request_deadline,
                upgrade_tolerance=upgrade_tolerance,
                limit=page_size,
                starting_after=cursor,
            ),
            starting_after=starting_after,
        )

    def update_deposit_address(
        self,
        deposit_address_id: str,
        *,
        metadata: Metadata | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> DepositAddress:
        """Merges `metadata` into the deposit address's, active or retired; deposits already
        recorded keep their own copy."""
        body = UpdateMetadataRequest(metadata=_metadata(metadata))
        address = self._call(
            lambda: update_deposit_address.sync_detailed(
                deposit_address_id, client=self._client, body=body
            ),
            DepositAddress,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )
        return self._checked_deposit_address(address)

    def rotate_deposit_address(
        self,
        deposit_address_id: str,
        *,
        idempotency_key: str | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> DepositAddress:
        """Retires an active deposit address and returns the customer's new one. Payments to the
        retired address are still credited; stop showing it.

        Retries reuse one `Idempotency-Key`, so a retry never rotates twice.
        """
        address = self._call(
            lambda: rotate_deposit_address.sync_detailed(deposit_address_id, client=self._client),
            DepositAddress,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )
        return self._checked_deposit_address(address)

    def create_refund(
        self,
        deposit: str,
        destination_address: str,
        amount_atomic: int | None = None,
        *,
        idempotency_key: str | None = None,
        metadata: Mapping[str, str] | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> Refund:
        """Creates a pending refund of `deposit` (the unrefunded remainder unless `amount_atomic`
        is given) to an address the customer controls. Pay it from the refund's `treasury`, then
        attach the transaction with `mark_refund_paid`.

        Retries reuse one `Idempotency-Key`, as `create_quote` does.
        """
        body = CreateRefundRequest(
            deposit=deposit,
            destination_address=destination_address,
            amount_atomic=UNSET if amount_atomic is None else str(amount_atomic),
            metadata=_metadata(metadata),
        )
        return self._call(
            lambda: create_refund.sync_detailed(client=self._client, body=body),
            Refund,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def mark_refund_paid(
        self,
        refund_id: str,
        transaction_hash: str,
        *,
        receipt_log_index: int | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Refund:
        """Attaches the transaction that pays a pending refund; the service verifies it at
        finality. `receipt_log_index`, the paying `Transfer` log's position among the receipt's
        logs, names it when one transaction pays several refunds. A refund marked paid can no
        longer be canceled."""
        body = MarkRefundPaidRequest(
            transaction_hash=transaction_hash,
            receipt_log_index=UNSET if receipt_log_index is None else receipt_log_index,
        )
        return self._call(
            lambda: mark_refund_paid.sync_detailed(refund_id, client=self._client, body=body),
            Refund,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def cancel_refund(
        self,
        refund_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Refund:
        """Cancels a pending refund not yet marked paid and releases its reservation of the
        deposit."""
        return self._call(
            lambda: cancel_refund.sync_detailed(refund_id, client=self._client),
            Refund,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def list_refunds(
        self,
        *,
        deposit: str | None = None,
        status: str | None = None,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> Iterator[Refund]:
        """Yields the account's refunds, newest first, following every page."""
        return self._paginate(
            lambda cursor: self.list_refunds_page(
                deposit=deposit,
                status=status,
                request_deadline=request_deadline,
                upgrade_tolerance=upgrade_tolerance,
                limit=page_size,
                starting_after=cursor,
            ),
            starting_after=starting_after,
        )

    def get_refund(
        self,
        refund_id: str,
        *,
        expand: list[str] | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> Refund:
        """Returns one refund; `expand` may name `deposit`."""
        return self._call(
            lambda: get_refund.sync_detailed(refund_id, client=self._client, expand=_unset(expand)),
            Refund,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def update_refund(
        self,
        refund_id: str,
        *,
        metadata: Metadata | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Refund:
        """Merges `metadata` into the refund's."""
        body = UpdateMetadataRequest(metadata=_metadata(metadata))
        return self._call(
            lambda: update_refund.sync_detailed(refund_id, client=self._client, body=body),
            Refund,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def attestation(
        self,
        nonce: bytes,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> AttestationResponse:
        """Fetches attestation evidence binding `nonce` to the webhook keys of this key's account
        and mode.

        Raises `AttestationError` unless `report_data` binds `nonce`, the account, the mode, and
        every listed key. Verify the quote with the dstack verifier (`deploy/dstack-verifier.sh`),
        including that its report data is `report_data` zero-padded to 64 bytes, before pinning
        the keys.
        """
        response = self._call(
            lambda: get_attestation.sync_detailed(client=self._client, nonce=nonce.hex()),
            AttestationResponse,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )
        verify_attestation_binding(response, nonce)
        return response

    def list_api_keys(
        self,
        *,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> list[ApiKeyObject]:
        """The API keys of this key's account and mode, newest first, without their secrets,
        from every page."""
        return list(
            self._paginate(
                lambda cursor: self.list_api_keys_page(
                    request_deadline=request_deadline,
                    upgrade_tolerance=upgrade_tolerance,
                    limit=page_size,
                    starting_after=cursor,
                ),
                starting_after=starting_after,
            )
        )

    def create_api_key(
        self,
        *,
        name: str = "",
        permissions: Sequence[str] | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> ApiKeyObject:
        """Creates a key of this mode; its `secret` is in this response only. Without
        `permissions` it is a secret key; with them a restricted key (`ppay_rk_…`) holding only
        those permissions, such as `quotes.write` or `deposits.read`, where a `write` includes
        its `read`. A restricted key never manages keys, treasuries, webhook endpoints, webhook
        keys, or account settings: run production servers with one and keep secret keys
        offline. Needs a secret key."""
        body = (
            CreateApiKeyRequest(name=name)
            if permissions is None
            else CreateApiKeyRequest(name=name, type_="restricted", permissions=list(permissions))
        )
        return self._call(
            lambda: create_api_key.sync_detailed(client=self._client, body=body),
            ApiKeyObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def get_api_key(
        self,
        api_key_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> ApiKeyObject:
        """One API key of this mode, without its secret."""
        return self._call(
            lambda: get_api_key.sync_detailed(api_key_id, client=self._client),
            ApiKeyObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def roll_api_key(
        self,
        api_key_id: str,
        *,
        expires_in: int = 0,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> ApiKeyObject:
        """Replaces a key with a new one of the same kind and permissions, returned with its
        `secret`; the old key keeps working for `expires_in` seconds (at most 7 days; `0`
        revokes it at once)."""
        body = RollApiKeyRequest(expires_in=expires_in)
        return self._call(
            lambda: roll_api_key.sync_detailed(api_key_id, client=self._client, body=body),
            ApiKeyObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def revoke_api_key(
        self,
        api_key_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> ApiKeyObject:
        """Revokes a key at once."""
        return self._call(
            lambda: revoke_api_key.sync_detailed(api_key_id, client=self._client),
            ApiKeyObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def list_webhook_endpoints(
        self,
        *,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> Iterator[WebhookEndpointObject]:
        """Yields the webhook endpoints of this mode, newest first."""
        return self._paginate(
            lambda cursor: self.list_webhook_endpoints_page(
                request_deadline=request_deadline,
                upgrade_tolerance=upgrade_tolerance,
                limit=page_size,
                starting_after=cursor,
            ),
            starting_after=starting_after,
        )

    def create_webhook_endpoint(
        self,
        url: str,
        enabled_events: list[str],
        *,
        description: str | None = None,
        metadata: Mapping[str, str] | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> WebhookEndpointObject:
        """Registers `url` for `enabled_events` (`["*"]` for all). Account events (`account.*`,
        `api_key.*`, `webhook_endpoint.*`) reach every enabled endpoint whatever it lists."""
        body = CreateWebhookEndpointRequest(
            url=url,
            enabled_events=enabled_events,
            description=_unset(description),
            metadata=_metadata(metadata),
        )
        return self._call(
            lambda: create_webhook_endpoint.sync_detailed(client=self._client, body=body),
            WebhookEndpointObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def get_webhook_endpoint(
        self,
        endpoint_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> WebhookEndpointObject:
        """One webhook endpoint."""
        return self._call(
            lambda: get_webhook_endpoint.sync_detailed(endpoint_id, client=self._client),
            WebhookEndpointObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def update_webhook_endpoint(
        self,
        endpoint_id: str,
        *,
        url: str | None = None,
        enabled_events: list[str] | None = None,
        description: str | None = None,
        disabled: bool | None = None,
        metadata: Metadata | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> WebhookEndpointObject:
        """Updates the parameters given; `disabled=True` stops its deliveries. The endpoint is
        notified of its own change first."""
        body = UpdateWebhookEndpointRequest(
            url=_unset(url),
            enabled_events=_unset(enabled_events),
            description=_unset(description),
            disabled=_unset(disabled),
            metadata=_metadata(metadata),
        )
        return self._call(
            lambda: update_webhook_endpoint.sync_detailed(
                endpoint_id, client=self._client, body=body
            ),
            WebhookEndpointObject,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def delete_webhook_endpoint(
        self,
        endpoint_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> DeletedWebhookEndpoint:
        """Deletes an endpoint; it receives the notice of its own deletion."""
        return self._call(
            lambda: delete_webhook_endpoint.sync_detailed(endpoint_id, client=self._client),
            DeletedWebhookEndpoint,
            retryable=False,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def test_webhook_endpoint(
        self,
        endpoint_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> EventObjectResponse:
        """Sends a test event to one endpoint and returns it."""
        return self._call(
            lambda: test_webhook_endpoint.sync_detailed(endpoint_id, client=self._client),
            EventObjectResponse,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def list_events(
        self,
        *,
        type: str | None = None,
        types: Sequence[str] | None = None,
        delivery_success: bool | None = None,
        created_gt: int | None = None,
        created_gte: int | None = None,
        created_lt: int | None = None,
        created_lte: int | None = None,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> Iterator[EventObjectResponse]:
        """Yields the events of this mode, newest first: the account's audit log and every
        webhook ever sent. `type` filters by one event type, such as `deposit.credited`, or a
        group, `deposit.*`; `types` by up to 20. `delivery_success=False` yields the events a
        webhook endpoint has not received yet: resend them once it is fixed."""
        return self._paginate(
            lambda cursor: self.list_events_page(
                type=type,
                types=types,
                delivery_success=delivery_success,
                created_gt=created_gt,
                created_gte=created_gte,
                created_lt=created_lt,
                created_lte=created_lte,
                request_deadline=request_deadline,
                upgrade_tolerance=upgrade_tolerance,
                limit=page_size,
                starting_after=cursor,
            ),
            starting_after=starting_after,
        )

    def get_event(
        self,
        event_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> EventObjectResponse:
        """One event."""
        return self._call(
            lambda: get_event.sync_detailed(event_id, client=self._client),
            EventObjectResponse,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def resend_event(
        self,
        event_id: str,
        *,
        webhook_endpoint: str,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> EventObjectResponse:
        """Delivers an event again to one enabled endpoint, as `stripe events resend` does."""
        body = ResendEventRequest(webhook_endpoint=webhook_endpoint)
        return self._call(
            lambda: resend_event.sync_detailed(event_id, client=self._client, body=body),
            EventObjectResponse,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def create_treasury_challenge(
        self,
        chain_id: int,
        address: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> TreasuryChallenge:
        """Returns the EIP-4361 message that proves `address` as the treasury of `chain_id`: an
        EOA signs it with `topup_sdk.sign_treasury_challenge` (or any wallet's `personal_sign`),
        a Safe's owners sign it as a Safe message (docs/integration.md)."""
        body = CreateTreasuryChallengeRequest(chain_id=chain_id, address=address)
        return self._call(
            lambda: create_treasury_challenge.sync_detailed(client=self._client, body=body),
            TreasuryChallenge,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def create_treasury(
        self,
        chain_id: int,
        message: str,
        signature: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Treasury:
        """Submits a signed challenge, announced as `treasury.created`. The chain's first treasury
        applies at once; a later live change is `pending` for 48 hours, then applies
        (`treasury.updated`) unless canceled (`treasury.canceled`)."""
        body = CreateTreasuryRequest(chain_id=chain_id, message=message, signature=signature)
        return self._call(
            lambda: create_treasury.sync_detailed(client=self._client, body=body),
            Treasury,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def list_treasuries(
        self,
        *,
        chain_id: int | None = None,
        status: str | None = None,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> list[Treasury]:
        """The treasuries of this mode, newest first, from every page."""
        return list(
            self._paginate(
                lambda cursor: self.list_treasuries_page(
                    chain_id=chain_id,
                    status=status,
                    request_deadline=request_deadline,
                    upgrade_tolerance=upgrade_tolerance,
                    limit=page_size,
                    starting_after=cursor,
                ),
                starting_after=starting_after,
            )
        )

    def get_treasury(
        self,
        treasury_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> Treasury:
        """One treasury."""
        return self._call(
            lambda: get_treasury.sync_detailed(treasury_id, client=self._client),
            Treasury,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def cancel_treasury(
        self,
        treasury_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Treasury:
        """Cancels a pending treasury change before it applies."""
        return self._call(
            lambda: cancel_treasury.sync_detailed(treasury_id, client=self._client),
            Treasury,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def pause_treasury(
        self,
        treasury_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Treasury:
        """Pauses crediting of deposits to every forwarder over the treasury, for an incident such
        as a compromised former treasury: they stay `pending`, and no `deposit.credited` is sent,
        until `resume_treasury`. Announced as `treasury.updated`. Needs a secret key."""
        return self._call(
            lambda: pause_treasury.sync_detailed(treasury_id, client=self._client),
            Treasury,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def resume_treasury(
        self,
        treasury_id: str,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        idempotency_key: str | None = None,
    ) -> Treasury:
        """Lifts your crediting pause of the treasury; the deposits it held are credited. An
        operator's pause stays in `crediting_paused_by`."""
        return self._call(
            lambda: resume_treasury.sync_detailed(treasury_id, client=self._client),
            Treasury,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
            idempotency_key=idempotency_key,
        )

    def get_balance(
        self,
        *,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> Balance:
        """What the account's forwarders hold, per chain and token."""
        return self._call(
            lambda: get_balance.sync_detailed(client=self._client),
            Balance,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_sweeps(
        self,
        *,
        chain_id: int | None = None,
        forwarder: str | None = None,
        token: str | None = None,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> Iterator[Sweep]:
        """Yields the sweeps, finalized `Flushed` events of the account's forwarders, newest
        first."""
        return self._paginate(
            lambda cursor: self.list_sweeps_page(
                chain_id=chain_id,
                forwarder=forwarder,
                token=token,
                request_deadline=request_deadline,
                upgrade_tolerance=upgrade_tolerance,
                limit=page_size,
                starting_after=cursor,
            ),
            starting_after=starting_after,
        )

    def list_forwarders(
        self,
        *,
        chain_id: int | None = None,
        quote: str | None = None,
        deposit_address: str | None = None,
        sweepable: str | None = None,
        page_size: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> Iterator[Forwarder]:
        """Yields the account's forwarders with the `(factory, salt, treasury)` each address
        derives from: all of them, or those of one `quote` (`qt_…`) or one `deposit_address`
        (`da_…`, one per chain and treasury it has had). With `sweepable` (a token contract),
        only those with a final unswept balance of it that may be swept: pass them to
        `topup_sdk.flush_transaction`."""
        return self._paginate(
            lambda cursor: self.list_forwarders_page(
                chain_id=chain_id,
                quote=quote,
                deposit_address=deposit_address,
                sweepable=sweepable,
                request_deadline=request_deadline,
                upgrade_tolerance=upgrade_tolerance,
                limit=page_size,
                starting_after=cursor,
            ),
            starting_after=starting_after,
        )

    def list_quotes_page(
        self,
        *,
        client_reference_id: str | None = None,
        status: str | None = None,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_quotes.sync_detailed,
                client=self._client,
                client_reference_id=_unset(client_reference_id),
                status=_unset(status),
                limit=limit,
                starting_after=_unset(starting_after),
            ),
            QuoteList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_deposits_page(
        self,
        *,
        client_reference_id: str | None = None,
        quote: str | None = None,
        deposit_address: str | None = None,
        status: str | None = None,
        tx_hash: str | None = None,
        created_gt: int | None = None,
        created_gte: int | None = None,
        created_lt: int | None = None,
        created_lte: int | None = None,
        expand: list[str] | None = None,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_deposits.sync_detailed,
                client=self._client,
                client_reference_id=_unset(client_reference_id),
                quote=_unset(quote),
                deposit_address=_unset(deposit_address),
                status=_unset(status),
                tx_hash=_unset(tx_hash),
                createdgt=_unset(created_gt),
                createdgte=_unset(created_gte),
                createdlt=_unset(created_lt),
                createdlte=_unset(created_lte),
                limit=limit,
                starting_after=_unset(starting_after),
                expand=_unset(expand),
            ),
            DepositList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_deposit_addresses_page(
        self,
        *,
        client_reference_id: str | None = None,
        status: str | None = None,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_deposit_addresses.sync_detailed,
                client=self._client,
                client_reference_id=_unset(client_reference_id),
                status=_unset(status),
                limit=limit,
                starting_after=_unset(starting_after),
            ),
            DepositAddressList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_refunds_page(
        self,
        *,
        deposit: str | None = None,
        status: str | None = None,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_refunds.sync_detailed,
                client=self._client,
                deposit=_unset(deposit),
                status=_unset(status),
                limit=limit,
                starting_after=_unset(starting_after),
            ),
            RefundList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_api_keys_page(
        self,
        *,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_api_keys.sync_detailed,
                client=self._client,
                limit=limit,
                starting_after=_unset(starting_after),
            ),
            ApiKeyList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_webhook_endpoints_page(
        self,
        *,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_webhook_endpoints.sync_detailed,
                client=self._client,
                limit=limit,
                starting_after=_unset(starting_after),
            ),
            WebhookEndpointList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_events_page(
        self,
        *,
        type: str | None = None,
        types: Sequence[str] | None = None,
        delivery_success: bool | None = None,
        created_gt: int | None = None,
        created_gte: int | None = None,
        created_lt: int | None = None,
        created_lte: int | None = None,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_events.sync_detailed,
                client=self._client,
                type_=_unset(type),
                types=UNSET if types is None else list(types),
                delivery_success=_unset(delivery_success),
                createdgt=_unset(created_gt),
                createdgte=_unset(created_gte),
                createdlt=_unset(created_lt),
                createdlte=_unset(created_lte),
                limit=limit,
                starting_after=_unset(starting_after),
            ),
            EventList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_treasuries_page(
        self,
        *,
        chain_id: int | None = None,
        status: str | None = None,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_treasuries.sync_detailed,
                client=self._client,
                chain_id=_unset(chain_id),
                status=_unset(status),
                limit=limit,
                starting_after=_unset(starting_after),
            ),
            TreasuryList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_sweeps_page(
        self,
        *,
        chain_id: int | None = None,
        forwarder: str | None = None,
        token: str | None = None,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_sweeps.sync_detailed,
                client=self._client,
                chain_id=_unset(chain_id),
                forwarder=_unset(forwarder),
                token=_unset(token),
                limit=limit,
                starting_after=_unset(starting_after),
            ),
            SweepList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def list_forwarders_page(
        self,
        *,
        chain_id: int | None = None,
        quote: str | None = None,
        deposit_address: str | None = None,
        sweepable: str | None = None,
        limit: int = 100,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
        starting_after: str | None = None,
    ) -> dict[str, Any]:
        if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
            raise ConfigurationError("limit must be an integer from 1 to 100")
        return self._page(
            partial(
                list_forwarders.sync_detailed,
                client=self._client,
                chain_id=_unset(chain_id),
                quote=_unset(quote),
                deposit_address=_unset(deposit_address),
                sweepable=_unset(sweepable),
                limit=limit,
                starting_after=_unset(starting_after),
            ),
            ForwarderList,
            starting_after=starting_after,
            request_deadline=request_deadline,
            upgrade_tolerance=upgrade_tolerance,
        )

    def _page(
        self,
        operation: Callable[[], Response[Any]],
        kind: type[Any],
        *,
        starting_after: str | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> dict[str, Any]:
        listed = self._call(
            operation, kind, request_deadline=request_deadline, upgrade_tolerance=upgrade_tolerance
        )
        if type(listed.has_more) is not bool or not isinstance(listed.data, list):
            raise ResponseValidationError("malformed list page")
        if listed.has_more and not listed.data:
            raise ResponseValidationError("empty continuing page")
        ids = [item.id for item in listed.data]
        if any(not isinstance(cursor, str) or not cursor for cursor in ids):
            raise ResponseValidationError("invalid page cursor")
        if len(set(ids)) != len(ids) or starting_after in ids:
            raise ResponseValidationError("repeated page cursor")
        data = []
        for item in listed.data:
            if isinstance(item, Quote):
                self._checked(item)
            elif isinstance(item, DepositAddress):
                self._checked_deposit_address(item)
            data.append(item)
        return {"data": data, "has_more": listed.has_more}

    def _paginate(
        self,
        page: Callable[[str | None], dict[str, Any]],
        *,
        starting_after: str | None = None,
    ) -> Iterator[Any]:
        seen: set[str] = set()
        if starting_after is not None:
            seen.add(starting_after)
        while True:
            listed = page(starting_after)
            ids = {item.id for item in listed["data"]}
            if seen.intersection(ids):
                raise ResponseValidationError("repeated page cursor")
            seen.update(ids)
            yield from listed["data"]
            if not listed["has_more"]:
                return
            starting_after = listed["data"][-1].id

    def _checked(self, quote: Quote) -> Quote:
        """Raises unless an open quote's address is the one derived from the pinned forwarder
        and account over the pinned treasury of its chain, and the quote names that treasury."""
        if quote.status != "open":
            return quote
        where = f"quote {quote.id}"
        pins = self._pins(where)
        if pins is None:
            return quote
        factory, implementation, account = pins
        treasury = self._treasury(quote.chain_id, quote.treasury, where)
        derived = quote_address(
            factory,
            implementation,
            treasury,
            account=account,
            client_reference_id=quote.client_reference_id,
            quote_id=quote.id,
        )
        if not same_address(derived, quote.address):
            raise AddressMismatchError(f"quote {quote.id} has an address the account cannot derive")
        return quote

    def _checked_deposit_address(self, address: DepositAddress) -> DepositAddress:
        """Raises unless every network of an active deposit address is the address derived from
        the pinned forwarder and account over the pinned treasury of its chain, and the network
        names that treasury. A retired address may pay a treasury since replaced, so it is not
        checked."""
        if address.status != "active":
            return address
        pins = self._pins(f"deposit address {address.id}")
        if pins is None:
            return address
        factory, implementation, account = pins
        if not address.networks:
            raise AddressMismatchError("active deposit address has no networks")
        chains: set[int] = set()
        for network in address.networks:
            if network.chain_id in chains:
                raise ResponseValidationError("duplicate address network")
            chains.add(network.chain_id)
            where = f"deposit address {address.id} on chain {network.chain_id}"
            treasury = self._treasury(network.chain_id, network.treasury, where)
            derived = deposit_address(
                factory,
                implementation,
                treasury,
                account=account,
                livemode=address.livemode,
                client_reference_id=address.client_reference_id,
                version=address.version,
            )
            if not same_address(derived, network.address) or (
                isinstance(address.address, str) and not same_address(derived, address.address)
            ):
                raise AddressMismatchError(f"{where} is not one the account can derive")
        return address

    def _pins(self, where: str) -> tuple[str, str, str] | None:
        """The pinned `(factory, implementation, account)` an address is derived from; `None`
        to skip the check, only in test mode without a pinned forwarder. A live key fails
        closed without its pins: the account must be pinned too, not read from the service."""
        if self.livemode and (
            self.forwarder is None or self.treasuries is None or not self._account_pinned
        ):
            raise AddressMismatchError(
                f"{where}: live mode requires the pinned account, forwarder, and treasuries"
            )
        if self.forwarder is None:
            return None
        factory, implementation = self.forwarder
        return factory, implementation, self.account_id()

    def _treasury(self, chain_id: int, treasury: str, where: str) -> str:
        """The treasury an address of `chain_id` is derived from: the pinned one, never the
        response's, which must name it. Only in test mode without pinned treasuries is the
        response's used, with a warning."""
        if self.treasuries is None:
            warnings.warn(
                f"{where}: no pinned treasuries; the service's treasury {treasury} is trusted "
                "(test mode only)",
                UnpinnedTreasuryWarning,
                stacklevel=4,
            )
            return treasury
        pinned = self.treasuries.get(chain_id)
        if pinned is None or not same_address(pinned, treasury):
            raise AddressMismatchError(f"{where} pays a treasury that is not the pinned one")
        return pinned

    def _call(
        self,
        operation: Callable[[], Response[Any]],
        expected: type[T],
        *,
        retryable: bool = True,
        idempotency_key: str | None = None,
        request_deadline: float | None = None,
        upgrade_tolerance: bool | None = None,
    ) -> T:
        explicit_deadline = self._request_deadline if request_deadline is None else request_deadline
        budget = 60.0 if explicit_deadline is None else explicit_deadline
        if upgrade_tolerance is not None and type(upgrade_tolerance) is not bool:
            raise ConfigurationError("upgrade_tolerance must be a boolean")
        tolerate = self._upgrade_tolerance if upgrade_tolerance is None else upgrade_tolerance
        if type(budget) not in (int, float) or not math.isfinite(budget) or budget <= 0:
            raise ConfigurationError("request_deadline must be finite and positive")
        if idempotency_key is not None:
            _idempotency_key(idempotency_key)
        started = self._clock()
        # Explicit deadlines are hard limits; the upgrade budget starts with the logical request.
        upgrade_budget = 300.0 if explicit_deadline is None else min(300.0, explicit_deadline)
        state = RequestState(
            started + budget,
            idempotency_key,
            upgrade_deadline=started + upgrade_budget if tolerate else None,
        )
        token = REQUEST_STATE.set(state)
        try:
            return self._perform(operation, expected, state, retryable)
        finally:
            REQUEST_STATE.reset(token)

    def _perform(  # noqa: PLR0912, PLR0915
        self,
        operation: Callable[[], Response[Any]],
        expected: type[T],
        state: RequestState,
        retryable: bool,
    ) -> T:
        attempt = 0
        failure: ApiError | TransportError | ResponseValidationError = TransportError("timeout")
        while True:
            if self._clock() >= state.deadline:
                raise failure
            attempt += 1
            state.response = None
            try:
                response = operation()
            except ConfigurationError:
                raise
            except TransportError as error:
                failure = error
            except httpx.TimeoutException:
                failure = TransportError("timeout")
            except httpx.TransportError:
                failure = TransportError("network")
            except _ErrorResponseError as error:
                failure = _api_error(error.response, now=self._wall_clock())
            except (AttributeError, TypeError, ValueError, KeyError):
                raw = state.response
                failure = ResponseValidationError(
                    "malformed response",
                    status_code=None if raw is None else raw.status_code,
                    request_id=None if raw is None else raw.headers.get("request-id"),
                )
                # Raise outside the decoder exception context; it may contain a raw body.
            else:
                parsed = response.parsed
                if response.status_code == 200 and isinstance(parsed, expected):
                    try:
                        self._verify_identity(parsed)
                    except ResponseValidationError as validation:
                        validation.status_code = response.status_code
                        validation.request_id = (
                            redact(response.headers.get("request-id", "")) or None
                        )
                        raise
                    return protect(parsed)
                if 300 <= response.status_code < 400:
                    raise _unexpected(response.status_code, response.headers)
                raise ResponseValidationError(
                    "malformed response",
                    status_code=response.status_code,
                    request_id=response.headers.get("request-id"),
                )
            if isinstance(failure, ResponseValidationError):
                raise failure
            method = state.request.method if state.request is not None else "GET"
            raw = state.response
            replayed = (
                raw is not None and raw.headers.get("idempotent-replayed", "").lower() == "true"
            )
            can_retry = retryable and method in {"GET", "POST"} and not replayed
            if isinstance(failure, ApiError):
                can_retry = can_retry and (
                    failure.status_code in RETRYABLE_STATUSES
                    or (failure.status_code == 409 and failure.code == "idempotency_key_in_use")
                )
            upgrade_failure = (
                can_retry
                and state.upgrade_deadline is not None
                and (
                    isinstance(failure, TransportError)
                    or (isinstance(failure, ApiError) and failure.status_code in {502, 503, 504})
                )
            )
            if upgrade_failure and state.upgrade_deadline is not None:
                state.deadline = state.upgrade_deadline
            if not can_retry or (not upgrade_failure and attempt >= self._max_attempts):
                raise failure
            delay = min(
                10.0 if upgrade_failure else 5.0, self._initial_backoff * 2 ** min(attempt - 1, 10)
            )
            delay *= 0.5 + 0.5 * self._rng()
            minimum = (
                _seconds(raw.headers.get("retry-after"), now=self._wall_clock()) if raw else None
            )
            if minimum is not None:
                delay = max(delay, minimum)
            if self._clock() + delay >= state.deadline:
                raise failure
            self._sleep(delay)

    def _verify_identity(self, value: Any) -> None:
        if isinstance(value, Quote | Deposit | DepositAddress):
            for name in ("id", "status", "client_reference_id"):
                if not isinstance(getattr(value, name), str):
                    raise ResponseValidationError("malformed response resource")
        if hasattr(value, "livemode") and (
            type(value.livemode) is not bool or value.livemode != self.livemode
        ):
            raise ResponseValidationError("response mode does not match client")
        if isinstance(value, AccountObject) and self._account_pinned and value.id != self._account:
            raise ResponseValidationError("response account does not match pins")
        if self._account_pinned and hasattr(value, "account") and value.account != self._account:
            raise ResponseValidationError("response account does not match pins")
        if hasattr(value, "data") and isinstance(value.data, list):
            for item in value.data:
                self._verify_identity(item)
        if (
            isinstance(value, Quote | Deposit)
            and type(value.amount) is not int
            and value.amount is not None
        ):
            raise ResponseValidationError("invalid response amount")


class _ErrorResponseError(Exception):
    """Carries an error response past the generated client, which parses only the bodies the
    OpenAPI document describes."""

    def __init__(self, response: httpx.Response) -> None:
        super().__init__(response.status_code)
        self.response = response


def _raise_error_response(response: httpx.Response) -> None:
    """Raises on every error status before the generated client parses the body, so an error
    body is classified by the single retry loop instead of the generated decoder. Gateway
    bodies become transport failures in upgrade mode; other errors keep legacy parsing."""
    if response.status_code >= 400:
        response.read()
        raise _ErrorResponseError(response)


def _idempotency_key(key: str | None) -> str:
    """One `Idempotency-Key` for every attempt of a request, generated unless given."""
    # The IETF Idempotency-Key header is an RFC 8941 string; the key is its content.
    value = str(uuid.uuid4()) if key is None else key
    if (
        not isinstance(value, str)
        or not value
        or not value.isascii()
        or any(ord(c) < 32 or ord(c) > 126 for c in value)
    ):
        raise ConfigurationError("idempotency_key must be a nonempty printable ASCII string")
    if len(value) > 255:
        raise ConfigurationError("idempotency_key must be at most 255 characters")
    return sf_string(value)


def _unset[V](value: V | None) -> V | Unset:
    return UNSET if value is None else value


def _metadata(metadata: Metadata | None) -> MetadataParamType0 | MetadataClear | Unset:
    if metadata is None:
        return UNSET
    if isinstance(metadata, str):
        if metadata:
            raise ValueError('metadata is a mapping of strings, or "" to unset every key')
        return ""
    return MetadataParamType0.from_dict(dict(metadata))


def _api_error(response: httpx.Response, *, now: float | None = None) -> ApiError | TransportError:
    """The service's error object, read leniently: a missing or malformed field is `None`, and a
    body that is not an error object is `unexpected_response`. In upgrade mode, malformed
    gateway errors are transport failures without exposing the gateway's body."""
    try:
        body = response.json()
    except ValueError:
        body = None
    error = body.get("error") if isinstance(body, dict) else None
    state = REQUEST_STATE.get()
    gateway_failure = (
        state is not None
        and state.upgrade_deadline is not None
        and state.request is not None
        and state.request.method in {"GET", "POST"}
        and response.status_code in {502, 503, 504}
        and response.headers.get("idempotent-replayed", "").lower() != "true"
    )
    if gateway_failure and (
        not isinstance(error, dict)
        or not isinstance(error.get("code"), str)
        or not isinstance(error.get("message"), str)
        or any(
            error.get(field) is not None and not isinstance(error[field], str)
            for field in ("type", "param", "doc_url")
        )
    ):
        return TransportError("network")
    if not isinstance(error, dict) or not isinstance(error.get("code"), str):
        return _unexpected(response.status_code, response.headers, now=now)

    def text(key: str) -> str | None:
        value = error.get(key)
        return value if isinstance(value, str) else None

    return ApiError(
        response.status_code,
        error["code"],
        text("message") or "",
        error_type=text("type"),
        param=text("param"),
        doc_url=text("doc_url"),
        request_id=response.headers.get("request-id"),
        retry_after=_seconds(response.headers.get("retry-after"), now=now),
    )


def _unexpected(
    status_code: int, headers: Mapping[str, str], *, now: float | None = None
) -> ApiError:
    return ApiError(
        status_code,
        "unexpected_response",
        "undocumented response",
        request_id=headers.get("request-id"),
        retry_after=_seconds(headers.get("retry-after"), now=now),
    )


def _seconds(value: str | None, *, now: float | None = None) -> float | None:
    """A `Retry-After` of delay seconds; `None` when absent or not a number of seconds."""
    if value is None:
        return None
    if value.isascii() and value.isdigit():
        try:
            seconds = float(value)
        except OverflowError:
            return None
        return seconds if math.isfinite(seconds) else None
    try:
        return max(
            0.0, parsedate_to_datetime(value).timestamp() - (time.time() if now is None else now)
        )
    except (TypeError, ValueError, OverflowError):
        return None
