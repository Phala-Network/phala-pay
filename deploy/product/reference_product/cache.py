"""Thread-safe single-flight TTL caching and a bounded LRU for the product."""

from __future__ import annotations

import threading
import time
from collections import OrderedDict
from collections.abc import Callable
from concurrent.futures import Future, ThreadPoolExecutor, wait

from topup_sdk.errors import TransportError

from .transport import operation_deadline


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
        failure: Callable[[Exception], Callable[[], Exception]],
        executor: ThreadPoolExecutor | None = None,
        incomplete: Callable[[T], bool] = lambda _: False,
        inclusive: bool = False,
    ) -> None:
        self._clock = clock
        self._ttl = ttl
        self._negative_ttl = negative_ttl
        self._failure = failure
        self._executor = executor
        self._incomplete = incomplete
        self._inclusive = inclusive
        self._changed = threading.Condition()
        self._value: T | None = None
        self._stored_at = 0.0
        self._expires = 0.0
        self._error: Callable[[], Exception] | None = None
        self._refreshing = False
        self._future: Future[T] | None = None

    def get(
        self,
        fetch: Callable[[], T],
        *,
        timeout: float | None = None,
        stale: Callable[[T, float], T] = lambda value, _: value,
    ) -> T:
        leader = False
        with self._changed:
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
                    raise self._error()
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
            return self._refresh(fetch)
        if pending is None:
            raise TransportError("unavailable")
        try:
            return pending.result(timeout=self._remaining(timeout))
        except TimeoutError as error:
            raise TransportError("timeout") from error
        except Exception as error:
            raise self._failure(error)() from None

    @staticmethod
    def _remaining(timeout: float | None) -> float | None:
        deadline = operation_deadline.get()
        if deadline is None:
            return timeout
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TransportError("timeout")
        return remaining if timeout is None else min(timeout, remaining)

    def _refresh(self, fetch: Callable[[], T]) -> T:
        try:
            value = fetch()
        except Exception as error:
            with self._changed:
                self._error = self._failure(error)
                self._expires = self._clock() + self._negative_ttl
                if self._value is not None:
                    return self._value
            raise
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


class ExpiringLRU[K, V]:
    """Small OrderedDict LRU: successful values persist, negative entries have a TTL.

    cachetools is not installed in the locked product environment. Callers serialize
    access with their existing lock; expiry is checked on lookup, without a full scan.
    """

    def __init__(self, maxsize: int, clock: Callable[[], float]) -> None:
        self._entries: OrderedDict[K, tuple[float | None, V]] = OrderedDict()
        self._maxsize = maxsize
        self._clock = clock

    def get(self, key: K) -> tuple[V] | None:
        entry = self._entries.get(key)
        if entry is None:
            return None
        expires, value = entry
        if expires is not None and self._clock() >= expires:
            del self._entries[key]
            return None
        self._entries.move_to_end(key)
        return (value,)

    def put(self, key: K, value: V, *, ttl: float | None = None) -> None:
        self._entries[key] = (None if ttl is None else self._clock() + ttl, value)
        self._entries.move_to_end(key)
        if len(self._entries) > self._maxsize:
            self._entries.popitem(last=False)

    def __len__(self) -> int:
        return len(self._entries)
