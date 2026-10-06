"""Thread-safe single-flight TTL caching and a bounded LRU for the product."""

from __future__ import annotations

import logging
import threading
import time
from collections.abc import Callable
from concurrent.futures import Future, ThreadPoolExecutor, wait
from dataclasses import dataclass
from http import HTTPStatus

from topup_sdk.errors import TopupError, TransportError

from .transport import operation_deadline


@dataclass(frozen=True)
class Failure:
    """Public response details, without an exception, traceback, or SDK constructor."""

    status: HTTPStatus
    code: str
    retry_after: str | None
    message: str
    log_level: int = logging.WARNING
    log_exc_info: bool = False
    source_type: str | None = None


class CachedFailureError(TopupError):
    """A fresh control-flow signal carrying a cached failure value to the HTTP boundary."""

    def __init__(self, failure: Failure) -> None:
        super().__init__(failure.message)
        self.failure = failure


class SingleFlightTTL[T]:
    """Share one refresh, retain stale values on failure, and back off from completion.

    Synchronous readers wait for the leader. With an executor, warm readers return
    immediately and cold readers wait only for their own deadline.
    """

    def __init__(
        self,
        *,
        clock: Callable[[], float],
        ttl: float,
        negative_ttl: float,
        failure: Callable[[Exception], Failure],
        executor: ThreadPoolExecutor | None = None,
        incomplete: Callable[[T], bool] = lambda _: False,
        inclusive: bool = False,
        cache_errors: tuple[type[Exception], ...],
    ) -> None:
        self._clock = clock
        self._ttl = ttl
        self._negative_ttl = negative_ttl
        self._failure = failure
        self._executor = executor
        self._incomplete = incomplete
        self._inclusive = inclusive
        self._cache_errors = cache_errors
        self._changed = threading.Condition()
        self._value: T | None = None
        self._stored_at = 0.0
        self._expires = 0.0
        self._error: Failure | None = None
        self._refreshing = False
        self._future: Future[T | Failure] | None = None

    def get(
        self,
        fetch: Callable[[], T],
        *,
        timeout: float | None = None,
        stale: Callable[[T, float], T] = lambda value, _: value,
    ) -> T:
        leader = False
        with self._changed:
            if self._future is not None and self._future.done():
                finished, self._future = self._future, None
                # Surface unexpected executor failures to the next HTTP reader, even
                # when the refresh was started by a reader receiving a stale value.
                finished.result()
            while self._refreshing and self._executor is None:
                remaining = self._remaining(None)
                self._changed.wait(timeout=remaining)
            now = self._clock()
            fresh = now < self._expires or (
                self._inclusive and self._error is None and now == self._expires
            )
            if fresh:
                if self._value is not None:
                    return stale(self._value, now - self._stored_at)
                if self._error is not None:
                    raise CachedFailureError(self._error)
            if not self._refreshing:
                self._refreshing = True
                if self._executor is not None:
                    try:
                        self._future = self._executor.submit(self._refresh, fetch)
                    except RuntimeError:
                        self._refreshing = False
                        raise
                else:
                    leader = True
            if self._value is not None and not leader:
                return stale(self._value, now - self._stored_at)
            pending = self._future
        if leader:
            return self._unwrap(self._refresh(fetch))
        if pending is None:
            raise TransportError("unavailable")
        try:
            result = pending.result(timeout=self._remaining(timeout))
        except TimeoutError as error:
            raise TransportError("timeout") from error
        finally:
            with self._changed:
                if pending.done() and self._future is pending:
                    self._future = None
        return self._unwrap(result)

    @staticmethod
    def _remaining(timeout: float | None) -> float | None:
        deadline = operation_deadline.get()
        if deadline is None:
            return timeout
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TransportError("timeout")
        return remaining if timeout is None else min(timeout, remaining)

    @staticmethod
    def _unwrap(value: T | Failure) -> T:
        if isinstance(value, Failure):
            raise CachedFailureError(value)
        return value

    def _refresh(self, fetch: Callable[[], T]) -> T | Failure:
        try:
            value = fetch()
        except self._cache_errors as error:
            with self._changed:
                self._error = self._failure(error)
                self._expires = self._clock() + self._negative_ttl
                if self._value is not None:
                    return self._value
                return self._error
        else:
            with self._changed:
                incomplete = self._incomplete(value)
                self._expires = self._clock() + (self._negative_ttl if incomplete else self._ttl)
                self._error = None
                if incomplete and self._value is not None:
                    return self._value
                self._value = value
                self._stored_at = self._clock()
                return value
        finally:
            with self._changed:
                self._refreshing = False
                self._changed.notify_all()

    def drain(self) -> None:
        """Wait for the currently submitted refresh, including its cache publication."""
        with self._changed:
            pending = self._future
        if pending is not None:
            wait([pending])
