from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote

import httpx

from ...client import AuthenticatedClient, Client
from ...types import Response, UNSET
from ... import errors

from ...models.create_quote_request import CreateQuoteRequest
from ...models.error_response import ErrorResponse
from ...models.quote import Quote
from ...types import UNSET, Unset
from typing import cast


def _get_kwargs(
    *,
    body: CreateQuoteRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}
    if not isinstance(idempotency_key, Unset):
        headers["Idempotency-Key"] = idempotency_key

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/quotes",
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> ErrorResponse | Quote | None:
    if response.status_code == 200:
        response_200 = Quote.from_dict(response.json())

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
) -> Response[ErrorResponse | Quote]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient,
    body: CreateQuoteRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[ErrorResponse | Quote]:
    """Quotes `amount` cents payable in `asset` on `chain_id`: a locked price, the exact token amount,
    and a single-use address, valid until `expires_at`, on the terms your payment settings set for
    the asset; the quote keeps them for good. An asset your settings do not accept is
    `asset_not_accepted`.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateQuoteRequest): `POST /v1/quotes` body. Example: {'amount': 2500, 'asset':
            'PHA', 'chain_id': 1, 'client_reference_id': 'team-42', 'currency': 'usd', 'metadata':
            {'order_id': 'ord_1001'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[ErrorResponse | Quote]
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
    body: CreateQuoteRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> ErrorResponse | Quote | None:
    """Quotes `amount` cents payable in `asset` on `chain_id`: a locked price, the exact token amount,
    and a single-use address, valid until `expires_at`, on the terms your payment settings set for
    the asset; the quote keeps them for good. An asset your settings do not accept is
    `asset_not_accepted`.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateQuoteRequest): `POST /v1/quotes` body. Example: {'amount': 2500, 'asset':
            'PHA', 'chain_id': 1, 'client_reference_id': 'team-42', 'currency': 'usd', 'metadata':
            {'order_id': 'ord_1001'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        ErrorResponse | Quote
    """

    return sync_detailed(
        client=client,
        body=body,
        idempotency_key=idempotency_key,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient,
    body: CreateQuoteRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[ErrorResponse | Quote]:
    """Quotes `amount` cents payable in `asset` on `chain_id`: a locked price, the exact token amount,
    and a single-use address, valid until `expires_at`, on the terms your payment settings set for
    the asset; the quote keeps them for good. An asset your settings do not accept is
    `asset_not_accepted`.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateQuoteRequest): `POST /v1/quotes` body. Example: {'amount': 2500, 'asset':
            'PHA', 'chain_id': 1, 'client_reference_id': 'team-42', 'currency': 'usd', 'metadata':
            {'order_id': 'ord_1001'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[ErrorResponse | Quote]
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
    body: CreateQuoteRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> ErrorResponse | Quote | None:
    """Quotes `amount` cents payable in `asset` on `chain_id`: a locked price, the exact token amount,
    and a single-use address, valid until `expires_at`, on the terms your payment settings set for
    the asset; the quote keeps them for good. An asset your settings do not accept is
    `asset_not_accepted`.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateQuoteRequest): `POST /v1/quotes` body. Example: {'amount': 2500, 'asset':
            'PHA', 'chain_id': 1, 'client_reference_id': 'team-42', 'currency': 'usd', 'metadata':
            {'order_id': 'ord_1001'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        ErrorResponse | Quote
    """

    return (
        await asyncio_detailed(
            client=client,
            body=body,
            idempotency_key=idempotency_key,
        )
    ).parsed
