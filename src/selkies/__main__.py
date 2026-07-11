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


logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


async def wait_for_app_ready(ready_file, app_wait_ready=False):
    """Wait for the streaming app's ready signal: returns immediately unless
    app_wait_ready is set, else polls until a sidecar creates ready_file."""
    if app_wait_ready:
        logger.info(f"Waiting for streaming app ready file: {ready_file}")
    while app_wait_ready and not os.path.exists(ready_file):
        await asyncio.sleep(0.2)


async def run():
    """
    Main entry point for the Selkies streaming server.
    """
    # Publish the resolved gamepad-socket directory so the LD_PRELOAD interposer in
    # app processes (which reads SELKIES_JS_SOCKET_PATH) writes/reads sockets in the
    # same directory selkies uses, regardless of how the setting was configured.
    os.environ["SELKIES_JS_SOCKET_PATH"] = settings.js_socket_path

    await wait_for_app_ready(settings.app_ready_file, settings.app_wait_ready[0])

    # Create the centralised server with settings
    server = CentralizedStreamServer(settings)

    # Register services
    server.register_service("webrtc", WebRTCService(server))
    server.register_service("websockets", DataStreamingServer(server))

    # Switch to the configured mode
    logger.info(f"Initiating server with {settings.mode} mode")
    await server.switch_to_mode(settings.mode)

    await server.run()


def main():
    """
    Entry point for command-line execution.
    """
    # uvloop makes the whole asyncio loop (timers, callbacks, socket I/O) markedly
    # faster, which directly lifts the pure-Python WebRTC SCTP data-channel
    # throughput and keeps large transfers from stalling input. Optional: fall
    # back to the stock loop if it isn't installed.
    try:
        import uvloop
        uvloop.install()
    except ImportError:
        pass
    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        logger.info("Server stopped by user")
    except Exception as e:
        logger.error(f"Error in main: {e}", exc_info=True)
        sys.exit(1)


if __name__ == "__main__":
    main()
