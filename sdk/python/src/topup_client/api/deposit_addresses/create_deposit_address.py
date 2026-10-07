from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote

import httpx

from ...client import AuthenticatedClient, Client
from ...types import Response, UNSET
from ... import errors

from ...models.create_deposit_address_request import CreateDepositAddressRequest
from ...models.deposit_address import DepositAddress
from ...models.error_response import ErrorResponse
from ...types import UNSET, Unset
from typing import cast


def _get_kwargs(
    *,
    body: CreateDepositAddressRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}
    if not isinstance(idempotency_key, Unset):
        headers["Idempotency-Key"] = idempotency_key

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/deposit_addresses",
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> DepositAddress | ErrorResponse | None:
    if response.status_code == 200:
        response_200 = DepositAddress.from_dict(response.json())

        return response_200

    if response.status_code == 400:
        response_400 = ErrorResponse.from_dict(response.json())

        return response_400

    if response.status_code == 401:
        response_401 = ErrorResponse.from_dict(response.json())

        return response_401

    if response.status_code == 403:
        response_403 = ErrorResponse.from_dict(response.json())

        return response_403

    if response.status_code == 409:
        response_409 = ErrorResponse.from_dict(response.json())

        return response_409

    if response.status_code == 422:
        response_422 = ErrorResponse.from_dict(response.json())

        return response_422

    if response.status_code == 429:
        response_429 = ErrorResponse.from_dict(response.json())

        return response_429

    if response.status_code == 503:
        response_503 = ErrorResponse.from_dict(response.json())

        return response_503

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[DepositAddress | ErrorResponse]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient,
    body: CreateDepositAddressRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[DepositAddress | ErrorResponse]:
    """Returns the customer's active deposit address, one address for every token your payment
    settings accept on every network of the key's mode where you have a treasury (an account that
    accepts nothing gets `asset_not_accepted`), issuing it if the customer has
    none: the same request always returns the same address until it is rotated. It also adds the
    address's network on a chain supported, or given a treasury, since it was issued, and replaces
    a chain's network whose treasury changed.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateDepositAddressRequest): `POST /v1/deposit_addresses` body. Example:
            {'client_reference_id': 'team-42', 'metadata': {'plan': 'pro'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[DepositAddress | ErrorResponse]
    """

    kwargs = _get_kwargs(
        body=body,
        idempotency_key=idempotency_key,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


def sync(
    *,
    client: AuthenticatedClient,
    body: CreateDepositAddressRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> DepositAddress | ErrorResponse | None:
    """Returns the customer's active deposit address, one address for every token your payment
    settings accept on every network of the key's mode where you have a treasury (an account that
    accepts nothing gets `asset_not_accepted`), issuing it if the customer has
    none: the same request always returns the same address until it is rotated. It also adds the
    address's network on a chain supported, or given a treasury, since it was issued, and replaces
    a chain's network whose treasury changed.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateDepositAddressRequest): `POST /v1/deposit_addresses` body. Example:
            {'client_reference_id': 'team-42', 'metadata': {'plan': 'pro'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        DepositAddress | ErrorResponse
    """

    return sync_detailed(
        client=client,
        body=body,
        idempotency_key=idempotency_key,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient,
    body: CreateDepositAddressRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[DepositAddress | ErrorResponse]:
    """Returns the customer's active deposit address, one address for every token your payment
    settings accept on every network of the key's mode where you have a treasury (an account that
    accepts nothing gets `asset_not_accepted`), issuing it if the customer has
    none: the same request always returns the same address until it is rotated. It also adds the
    address's network on a chain supported, or given a treasury, since it was issued, and replaces
    a chain's network whose treasury changed.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateDepositAddressRequest): `POST /v1/deposit_addresses` body. Example:
            {'client_reference_id': 'team-42', 'metadata': {'plan': 'pro'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[DepositAddress | ErrorResponse]
    """

    kwargs = _get_kwargs(
        body=body,
        idempotency_key=idempotency_key,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    *,
    client: AuthenticatedClient,
    body: CreateDepositAddressRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> DepositAddress | ErrorResponse | None:
    """Returns the customer's active deposit address, one address for every token your payment
    settings accept on every network of the key's mode where you have a treasury (an account that
    accepts nothing gets `asset_not_accepted`), issuing it if the customer has
    none: the same request always returns the same address until it is rotated. It also adds the
    address's network on a chain supported, or given a treasury, since it was issued, and replaces
    a chain's network whose treasury changed.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateDepositAddressRequest): `POST /v1/deposit_addresses` body. Example:
            {'client_reference_id': 'team-42', 'metadata': {'plan': 'pro'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        DepositAddress | ErrorResponse
    """

    return (
        await asyncio_detailed(
            client=client,
            body=body,
            idempotency_key=idempotency_key,
        )
    ).parsed
