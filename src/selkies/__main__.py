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
from .stream_server import CentralisedStreamServer


logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

if __name__ == "__main__" and __package__ is None:
    current_script_dir = os.path.dirname(os.path.abspath(__file__))
    package_container_dir = os.path.dirname(current_script_dir)
    if package_container_dir not in sys.path:
        sys.path.insert(0, package_container_dir)


async def run():
    """
    Main entry point for the Selkies streaming server.
    """
    # Create the centralised server with settings
    server = CentralisedStreamServer(settings)

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
    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        logger.info("Server stopped by user")
    except Exception as e:
        logger.error(f"Error in main: {e}", exc_info=True)
        sys.exit(1)


if __name__ == "__main__":
    main()
