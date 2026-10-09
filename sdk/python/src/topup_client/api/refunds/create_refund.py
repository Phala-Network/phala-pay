from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote

import httpx

from ...client import AuthenticatedClient, Client
from ...types import Response, UNSET
from ... import errors

from ...models.create_refund_request import CreateRefundRequest
from ...models.error_response import ErrorResponse
from ...models.refund import Refund
from ...types import UNSET, Unset
from typing import cast


def _get_kwargs(
    *,
    body: CreateRefundRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}
    if not isinstance(idempotency_key, Unset):
        headers["Idempotency-Key"] = idempotency_key

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/refunds",
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> ErrorResponse | Refund | None:
    if response.status_code == 200:
        response_200 = Refund.from_dict(response.json())

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
) -> Response[ErrorResponse | Refund]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    *,
    client: AuthenticatedClient,
    body: CreateRefundRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[ErrorResponse | Refund]:
    """Creates a `pending` refund of a final deposit (design D5): a rejected deposit other than a
    sanctioned or dust one, or a credited one. The amount, the unrefunded remainder by default, is
    reserved until the refund is canceled or fails. The destination must pass sanctions screening
    (`400 destination_sanctioned`). The merchant then pays it from the refund's `treasury` and
    attaches the transaction with `mark_paid`.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateRefundRequest): `POST /v1/refunds` body. Example: {'amount_atomic':
            '25000000', 'deposit': 'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516', 'destination_address':
            '0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a', 'metadata': {'ticket': 'support-311'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[ErrorResponse | Refund]
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
    body: CreateRefundRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> ErrorResponse | Refund | None:
    """Creates a `pending` refund of a final deposit (design D5): a rejected deposit other than a
    sanctioned or dust one, or a credited one. The amount, the unrefunded remainder by default, is
    reserved until the refund is canceled or fails. The destination must pass sanctions screening
    (`400 destination_sanctioned`). The merchant then pays it from the refund's `treasury` and
    attaches the transaction with `mark_paid`.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateRefundRequest): `POST /v1/refunds` body. Example: {'amount_atomic':
            '25000000', 'deposit': 'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516', 'destination_address':
            '0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a', 'metadata': {'ticket': 'support-311'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        ErrorResponse | Refund
    """

    return sync_detailed(
        client=client,
        body=body,
        idempotency_key=idempotency_key,
    ).parsed


async def asyncio_detailed(
    *,
    client: AuthenticatedClient,
    body: CreateRefundRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> Response[ErrorResponse | Refund]:
    """Creates a `pending` refund of a final deposit (design D5): a rejected deposit other than a
    sanctioned or dust one, or a credited one. The amount, the unrefunded remainder by default, is
    reserved until the refund is canceled or fails. The destination must pass sanctions screening
    (`400 destination_sanctioned`). The merchant then pays it from the refund's `treasury` and
    attaches the transaction with `mark_paid`.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateRefundRequest): `POST /v1/refunds` body. Example: {'amount_atomic':
            '25000000', 'deposit': 'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516', 'destination_address':
            '0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a', 'metadata': {'ticket': 'support-311'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[ErrorResponse | Refund]
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
    body: CreateRefundRequest,
    idempotency_key: None | str | Unset = UNSET,
) -> ErrorResponse | Refund | None:
    """Creates a `pending` refund of a final deposit (design D5): a rejected deposit other than a
    sanctioned or dust one, or a credited one. The amount, the unrefunded remainder by default, is
    reserved until the refund is canceled or fails. The destination must pass sanctions screening
    (`400 destination_sanctioned`). The merchant then pays it from the refund's `treasury` and
    attaches the transaction with `mark_paid`.

    Args:
        idempotency_key (None | str | Unset):
        body (CreateRefundRequest): `POST /v1/refunds` body. Example: {'amount_atomic':
            '25000000', 'deposit': 'dep_8a1f4e2b6c3d49e0a7b5c1d2e3f40516', 'destination_address':
            '0x1775c1326aa633546b0b5634ae2bef0ba7cbfc9a', 'metadata': {'ticket': 'support-311'}}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        ErrorResponse | Refund
    """

    return (
        await asyncio_detailed(
            client=client,
            body=body,
            idempotency_key=idempotency_key,
        )
    ).parsed
