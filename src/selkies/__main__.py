# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import os
import sys
import asyncio
import logging

from .settings import settings
from .webrtc_mode import WebRTCService
from .selkies import DataStreamingServer
from .stream_server import CentralizedStreamServer
from . import audit as _audit


logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


async def run():
    """
    Main entry point for the Selkies streaming server.
    """
    # Configure the optional audit webhook (opt-in; empty URL is a no-op).
    # str-type settings are stored as plain strings, so they are read
    # directly (not via the [0] tuple accessor used for bool/range types).
    try:
        _audit_timeout = float(getattr(settings, "audit_webhook_timeout", "2.0") or "2.0")
    except (TypeError, ValueError):
        _audit_timeout = 2.0
    _audit.configure(
        url=getattr(settings, "audit_webhook_url", "") or "",
        token=getattr(settings, "audit_webhook_token", "") or "",
        timeout_seconds=_audit_timeout,
    )
    if getattr(settings, "audit_webhook_url", ""):
        logger.info("audit webhook enabled, target=%s", settings.audit_webhook_url)

    try:
        # Create the centralised server with settings
        server = CentralizedStreamServer(settings)

        # Register services
        server.register_service("webrtc", WebRTCService(server))
        server.register_service("websockets", DataStreamingServer(server))

        # Switch to the configured mode
        logger.info(f"Initiating server with {settings.mode} mode")
        await server.switch_to_mode(settings.mode)

        await server.run()
    finally:
        # Drain in-flight audit POSTs and close the aiohttp session so we do
        # not leak the connector / emit unclosed-session warnings on shutdown.
        await _audit.close()


def main():
    """
    Entry point for command-line execution.
    """
    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        logger.info("Server stopped by user")
    except Exception as e:
        logger.error(f"Error in main: {e}", exc_info=True)
        sys.exit(1)


if __name__ == "__main__":
    main()
