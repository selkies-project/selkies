# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""
Best-effort audit-event emitter for clipboard and file-transfer events.

Selkies sessions can carry data between the remote browser and the
server desktop in three channels: bidirectional clipboard, file
upload (browser to server) and file download (server to browser).
Deployments that need a tamper-evident record of those transfers,
without forcing the operator to maintain a Selkies code patch, can
configure the ``audit_webhook_url`` setting.

When the URL is set, Selkies POSTs a small JSON object to it on each
clipboard and file event. Only metadata is sent (event type, size,
mime type, timestamp); the actual content is never logged.

The emitter is intentionally non-blocking and best-effort:

* failures (DNS, connection refused, timeout, HTTP >= 400) emit a
  warning and are otherwise dropped, so a misconfigured or down
  collector cannot stall the streaming pipeline;
* events are scheduled on the running event loop and the originating
  coroutine returns immediately;
* when no event loop is available (called too early during startup)
  the event is silently dropped with a debug log line.

The audit channel is opt-in and disabled by default - the empty URL
short-circuits the module-level :func:`emit` to a no-op.
"""

from __future__ import annotations

import asyncio
import logging
from datetime import datetime, timezone
from typing import Any, Optional

import aiohttp

logger = logging.getLogger("audit")


class AuditClient:
    """Fire-and-forget JSON-over-HTTP audit-event sink.

    The client owns a single :class:`aiohttp.ClientSession` that is
    created lazily on the first event and reused for subsequent
    POSTs. Close it via :meth:`close` during shutdown if you care
    about clean asyncio resource teardown.
    """

    def __init__(
        self,
        url: str = "",
        token: str = "",
        timeout_seconds: float = 2.0,
    ):
        self.url = url.strip()
        self.token = token.strip()
        # HTTPS only: the optional Bearer token and the event metadata must
        # never traverse cleartext. A non-HTTPS URL disables the channel.
        if self.url and not self.url.lower().startswith("https://"):
            logger.warning(
                "audit_webhook_url is not https:// (%s) - audit channel disabled", self.url
            )
            self.url = ""
        # Lower bound 100 ms: avoids accidental zero-timeout misconfiguration
        # which aiohttp would interpret as "never time out".
        self.timeout = aiohttp.ClientTimeout(total=max(0.1, float(timeout_seconds)))
        self._session: Optional[aiohttp.ClientSession] = None
        # Strong refs to in-flight POST tasks. loop.create_task keeps no
        # reference of its own, so without this the GC can cancel a task
        # mid-flight and silently drop the event.
        self._background_tasks: "set[asyncio.Task]" = set()

    @property
    def enabled(self) -> bool:
        """True if a webhook URL is configured and the client will POST."""
        return bool(self.url)

    async def _ensure_session(self) -> aiohttp.ClientSession:
        if self._session is None or self._session.closed:
            headers = {"User-Agent": "selkies-audit/1.0"}
            if self.token:
                headers["Authorization"] = f"Bearer {self.token}"
            self._session = aiohttp.ClientSession(
                timeout=self.timeout,
                headers=headers,
            )
        return self._session

    async def _post(self, payload: dict) -> None:
        try:
            session = await self._ensure_session()
            async with session.post(self.url, json=payload) as resp:
                if resp.status >= 400:
                    # Bounded read: a misbehaving collector must not be able to
                    # make us buffer a huge error body into memory.
                    body = (await resp.content.read(2048)).decode("utf-8", "replace")
                    logger.warning(
                        "audit webhook returned %s for event=%s: %s",
                        resp.status,
                        payload.get("event"),
                        body,
                    )
        except asyncio.TimeoutError:
            logger.warning(
                "audit webhook timeout for event=%s url=%s",
                payload.get("event"),
                self.url,
            )
        except aiohttp.ClientError as exc:
            logger.warning(
                "audit webhook client error for event=%s: %s",
                payload.get("event"),
                exc,
            )
        except Exception as exc:  # noqa: BLE001
            # Last-resort guard: the audit channel must never crash the
            # streaming session, regardless of what aiohttp throws.
            logger.warning(
                "audit webhook unexpected error for event=%s: %s",
                payload.get("event"),
                exc,
            )

    def emit(self, event: str, **fields: Any) -> None:
        """Schedule an audit event and return immediately.

        :param event: short identifier such as ``"clipboard.send"`` or
            ``"file.upload"``. Stable across releases.
        :param fields: extra JSON-serializable metadata. Must NOT
            include the payload contents - sizes and mime types only.
        """
        if not self.enabled:
            return
        payload: dict = {
            "event": event,
            "ts": datetime.now(timezone.utc).isoformat(),
        }
        payload.update(fields)
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            # No event loop yet - happens if a call site fires during
            # synchronous import-time code. Drop the event quietly.
            logger.debug("no running event loop, dropping audit event %s", event)
            return
        task = loop.create_task(self._post(payload))
        self._background_tasks.add(task)
        task.add_done_callback(self._background_tasks.discard)

    async def close(self) -> None:
        """Drain in-flight POSTs and close the underlying HTTP session."""
        if self._background_tasks:
            await asyncio.gather(*self._background_tasks, return_exceptions=True)
        if self._session is not None and not self._session.closed:
            await self._session.close()
        self._session = None


# Module-level singleton. Wired up by the streaming-mode bootstrap code
# (selkies.py / webrtc_mode.py / stream_server.py) from parsed settings.
_default: Optional[AuditClient] = None


def configure(
    url: str = "",
    token: str = "",
    timeout_seconds: float = 2.0,
) -> None:
    """(Re)initialize the module-level :class:`AuditClient`.

    Safe to call multiple times. If a previous instance had an open
    session, its teardown is scheduled on the running loop so reconfiguring
    does not leak the connector. Call sites must use :func:`emit` rather
    than caching the client.
    """
    global _default
    old = _default
    _default = AuditClient(
        url=url, token=token, timeout_seconds=timeout_seconds
    )
    if old is not None and old._session is not None:
        try:
            asyncio.get_running_loop().create_task(old.close())
        except RuntimeError:
            # No running loop (e.g. configured before startup); the old
            # session was never opened, so there is nothing to close.
            pass


def emit(event: str, **fields: Any) -> None:
    """Module-level convenience wrapper around the configured client.

    Safe to call before :func:`configure` - in that case the event is
    silently dropped. This decouples call sites from startup ordering.
    """
    if _default is None:
        return
    _default.emit(event, **fields)


def is_enabled() -> bool:
    """True if the module-level client is configured with a URL."""
    return _default is not None and _default.enabled


async def close() -> None:
    """Tear down the module-level client. Idempotent."""
    global _default
    if _default is not None:
        await _default.close()
        _default = None
