"""Strict-mypy regression coverage for the documented pins-based webhook call."""

from fastapi import Request

from phala_pay import PhalaPay


def verify_delivery(raw_body: bytes, request: Request) -> None:
    with PhalaPay.from_env() as pay:
        pay.webhooks.construct_event(raw_body, request.headers)
