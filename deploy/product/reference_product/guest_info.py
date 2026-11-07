"""Public guest Info through the private Info-only service, never an attestation verdict."""

import threading
import time
from typing import Any

import httpx

# The frozen v0 Info contract contains only public metadata. Never forward an arbitrary RPC.
PUBLIC_FIELDS = frozenset(
    {
        "app_id",
        "instance_id",
        "app_cert",
        "tcb_info",
        "app_name",
        "device_id",
        "mr_aggregated",
        "os_image_hash",
        "key_provider_info",
        "compose_hash",
        "vm_config",
        "cloud_vendor",
        "cloud_product",
    }
)


class GuestInfo:
    """Coalesce socket calls and cache successes for 60 seconds, failures for five seconds."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._until = 0.0
        self._value: dict[str, Any] | None = None

    def get(self) -> dict[str, Any] | None:
        with self._lock:
            if time.monotonic() < self._until:
                return self._value
            try:
                with httpx.Client(timeout=15, trust_env=False) as client:
                    response = client.get("http://guest-info:8082/Info")
                    response.raise_for_status()
                    value = response.json()
                if not isinstance(value, dict) or not isinstance(value.get("vm_config"), str):
                    raise TypeError("invalid guest Info")
                self._value = {key: value[key] for key in PUBLIC_FIELDS if key in value}
            except (httpx.HTTPError, ValueError, TypeError):
                self._value = None
            self._until = time.monotonic() + (60 if self._value is not None else 5)
            return self._value
