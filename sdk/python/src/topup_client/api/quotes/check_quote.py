from http import HTTPStatus
from typing import Any, cast
from urllib.parse import quote

import httpx

from ...client import AuthenticatedClient, Client
from ...types import Response, UNSET
from ... import errors

from ...models.quote_check import QuoteCheck
from ...types import UNSET, Unset
from typing import cast


def _get_kwargs(
    id: str,
    *,
    client_secret: str | Unset = UNSET,
) -> dict[str, Any]:

    params: dict[str, Any] = {}

    params["client_secret"] = client_secret

    params = {k: v for k, v in params.items() if v is not UNSET and v is not None}

    _kwargs: dict[str, Any] = {
        "method": "post",
        "url": "/v1/quotes/{id}/check".format(
            id=quote(str(id), safe=""),
        ),
        "params": params,
    }

    return _kwargs


def _parse_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> QuoteCheck | None:
    if response.status_code == 202:
        response_202 = QuoteCheck.from_dict(response.json())

        return response_202

    if client.raise_on_unexpected_status:
        raise errors.UnexpectedStatus(response.status_code, response.content)
    else:
        return None


def _build_response(
    *, client: AuthenticatedClient | Client, response: httpx.Response
) -> Response[QuoteCheck]:
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
    client_secret: str | Unset = UNSET,
) -> Response[QuoteCheck]:
    """Request immediate read-only discovery for an open, unexpired quote's issued address.

    Args:
        id (str):
        client_secret (str | Unset):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[QuoteCheck]
    """

    kwargs = _get_kwargs(
        id=id,
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
    client_secret: str | Unset = UNSET,
) -> QuoteCheck | None:
    """Request immediate read-only discovery for an open, unexpired quote's issued address.

    Args:
        id (str):
        client_secret (str | Unset):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        QuoteCheck
    """

    return sync_detailed(
        id=id,
        client=client,
        client_secret=client_secret,
    ).parsed


async def asyncio_detailed(
    id: str,
    *,
    client: AuthenticatedClient,
    client_secret: str | Unset = UNSET,
) -> Response[QuoteCheck]:
    """Request immediate read-only discovery for an open, unexpired quote's issued address.

    Args:
        id (str):
        client_secret (str | Unset):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        Response[QuoteCheck]
    """

    kwargs = _get_kwargs(
        id=id,
        client_secret=client_secret,
    )

    response = await client.get_async_httpx_client().request(**kwargs)

    return _build_response(client=client, response=response)


async def asyncio(
    id: str,
    *,
    client: AuthenticatedClient,
    client_secret: str | Unset = UNSET,
) -> QuoteCheck | None:
    """Request immediate read-only discovery for an open, unexpired quote's issued address.

    Args:
        id (str):
        client_secret (str | Unset):

    Raises:
        errors.UnexpectedStatus: If the server returns an undocumented status code and Client.raise_on_unexpected_status is True.
        httpx.TimeoutException: If the request takes longer than Client.timeout.

    Returns:
        QuoteCheck
    """

    return (
        await asyncio_detailed(
            id=id,
            client=client,
            client_secret=client_secret,
        )
    ).parsed
