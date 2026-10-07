from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote

import httpx

from ...client import AuthenticatedClient, Client
from ...types import Response, UNSET
from ... import errors

from ...models.submit_quote_transaction_request import SubmitQuoteTransactionRequest
from ...models.transaction_submission import TransactionSubmission
from ...types import UNSET, Unset
from typing import cast


def _get_kwargs(
    id: str,
    *,
    body: SubmitQuoteTransactionRequest,
    client_secret: str | Unset = UNSET,
) -> dict[str, Any]:
    headers: dict[str, Any] = {}

    params: dict[str, Any] = {}

    params["client_secret"] = client_secret

    params = {k: v for k, v in params.items() if v is not UNSET and v is not None}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/quotes/{id}/transactions".format(
            id=quote(str(id), safe=""),
        ),
        "params": params,
    }

    _kwargs["json"] = body.to_dict()

    headers["Content-Type"] = "application/json"

    _kwargs["headers"] = headers
    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> TransactionSubmission | None:
    if response.status_code == 202:
        response_202 = TransactionSubmission.from_dict(response.json())

        return response_202

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[TransactionSubmission]:
    return Response(
        status_code=HTTPStatus(response.status_code),
        content=response.content,
        headers=response.headers,
        parsed=_parse_response(client=client, response=response),
    )


def sync_detailed(
    id: str,
    *,
    client: AuthenticatedClient,
    body: SubmitQuoteTransactionRequest,
    client_secret: str | Unset = UNSET,
) -> Response[TransactionSubmission]:
    """Submit a quote transaction hint. The stored quote selects the chain and recipient.

    Args:
        id (str):
        client_secret (str | Unset):
        body (SubmitQuoteTransactionRequest): A quote's chain is derived from its stored terms.
            Example: {'transaction_hash':
            '0x7d3c1e5a9b2f4d6c8e0a1b3d5f7c9e2a4b6d8f0c1e3a5b7d9f1c3e5a7b9d1f3e'}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[TransactionSubmission]
    """

    kwargs = _get_kwargs(
        id=id,
        body=body,
        client_secret=client_secret,
    )

    response = client.get_httpx_client().request(
        **kwargs,
    )

    return _build_response(client=client, response=response)


def sync(
    id: str,
    *,
    client: AuthenticatedClient,
    body: SubmitQuoteTransactionRequest,
    client_secret: str | Unset = UNSET,
) -> TransactionSubmission | None:
    """Submit a quote transaction hint. The stored quote selects the chain and recipient.

    Args:
        id (str):
        client_secret (str | Unset):
        body (SubmitQuoteTransactionRequest): A quote's chain is derived from its stored terms.
            Example: {'transaction_hash':
            '0x7d3c1e5a9b2f4d6c8e0a1b3d5f7c9e2a4b6d8f0c1e3a5b7d9f1c3e5a7b9d1f3e'}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        TransactionSubmission
    """

    return sync_detailed(
        id=id,
        client=client,
        body=body,
        client_secret=client_secret,
    ).parsed


async def asyncio_detailed(
    id: str,
    *,
    client: AuthenticatedClient,
    body: SubmitQuoteTransactionRequest,
    client_secret: str | Unset = UNSET,
) -> Response[TransactionSubmission]:
    """Submit a quote transaction hint. The stored quote selects the chain and recipient.

    Args:
        id (str):
        client_secret (str | Unset):
        body (SubmitQuoteTransactionRequest): A quote's chain is derived from its stored terms.
            Example: {'transaction_hash':
            '0x7d3c1e5a9b2f4d6c8e0a1b3d5f7c9e2a4b6d8f0c1e3a5b7d9f1c3e5a7b9d1f3e'}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[TransactionSubmission]
    """

    kwargs = _get_kwargs(
        id=id,
        body=body,
        client_secret=client_secret,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    id: str,
    *,
    client: AuthenticatedClient,
    body: SubmitQuoteTransactionRequest,
    client_secret: str | Unset = UNSET,
) -> TransactionSubmission | None:
    """Submit a quote transaction hint. The stored quote selects the chain and recipient.

    Args:
        id (str):
        client_secret (str | Unset):
        body (SubmitQuoteTransactionRequest): A quote's chain is derived from its stored terms.
            Example: {'transaction_hash':
            '0x7d3c1e5a9b2f4d6c8e0a1b3d5f7c9e2a4b6d8f0c1e3a5b7d9f1c3e5a7b9d1f3e'}.

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        TransactionSubmission
    """

    return (
        await asyncio_detailed(
            id=id,
            client=client,
            body=body,
            client_secret=client_secret,
        )
    ).parsed
