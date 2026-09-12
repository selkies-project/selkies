# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Audit trail of clipboard and file transfers, POSTed to an operator's webhook.

Every transfer the server carries — clipboard content in either direction,
a file upload, a file download — is one JSON object on `audit_webhook_url`:
its `event`, an RFC 3339 `ts`, and metadata (byte size, MIME type or file
name), never the content. Events queue in order and one task delivers them
over a single keep-alive connection, so a transfer pays an enqueue and
nothing else; a collector that is slow or down loses what overflows the
queue rather than stalling a session, and each outage is logged once.
Without a URL every call is a no-op.
"""

import asyncio
import logging
import time
from datetime import datetime, timezone
from typing import Any, Optional

import aiohttp

from .settings import settings

logger = logging.getLogger("audit")

QUEUE_BOUND = 1024

_queue: Optional[asyncio.Queue] = None
_sender: Optional[asyncio.Task] = None
_overflowing = False
_closing = False


def emit(event: str, **fields: Any) -> None:
    """Queue one event and return at once."""
    global _queue, _sender, _overflowing
    if not settings.audit_webhook_url:
        return
    if _queue is None:
        _queue = asyncio.Queue(QUEUE_BOUND)
        _sender = asyncio.get_running_loop().create_task(_deliver(_queue))
    try:
        _queue.put_nowait({"event": event, "ts": time.time(), **fields})
        _overflowing = False
    except asyncio.QueueFull:
        if not _overflowing:
            logger.warning("Audit webhook queue full (%d events); dropping events until it drains", QUEUE_BOUND)
            _overflowing = True


async def _deliver(queue: asyncio.Queue) -> None:
    """Send queued events one by one; an outage is logged once, its end too."""
    headers = {}
    if settings.audit_webhook_token:
        headers["Authorization"] = f"Bearer {settings.audit_webhook_token}"
    timeout = aiohttp.ClientTimeout(total=settings.audit_webhook_timeout)
    failing = False
    async with aiohttp.ClientSession(headers=headers, timeout=timeout,
                                     connector=aiohttp.TCPConnector(limit=1)) as session:
        while not _closing:
            payload = await queue.get()
            payload["ts"] = datetime.fromtimestamp(payload["ts"], timezone.utc).isoformat(
                timespec="milliseconds").replace("+00:00", "Z")
            try:
                async with session.post(settings.audit_webhook_url, json=payload) as response:
                    failure = f"HTTP {response.status}" if response.status >= 400 else ""
            except (aiohttp.ClientError, asyncio.TimeoutError, OSError) as exc:
                failure = str(exc) or type(exc).__name__
            finally:
                queue.task_done()
            if failure and not failing:
                logger.warning("Audit webhook %s failed: %s; events are dropped until it answers",
                               settings.audit_webhook_url, failure)
            elif failing and not failure:
                logger.info("Audit webhook delivering again")
            failing = bool(failure)


async def close() -> None:
    """Deliver what is queued, within one request timeout, and stop the sender."""
    global _queue, _sender, _closing
    if _sender is None:
        return
    # The flag ends the loop at an event boundary, so shutting down never means
    # interrupting a request in flight; the cancel only breaks the idle wait.
    _closing = True
    try:
        await asyncio.wait_for(_queue.join(), settings.audit_webhook_timeout)
    except asyncio.TimeoutError:
        pass
    _sender.cancel()
    try:
        await _sender
    except asyncio.CancelledError:
        pass
    _queue = _sender = None
    _closing = False
