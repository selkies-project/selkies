# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Process entry point for the Selkies streaming server.

Builds the centralized stream server, registers the WebRTC and WebSockets
services, switches to the configured mode, and runs the asyncio loop until a
signal or fatal error unwinds it. Signal handling routes SIGTERM/SIGHUP
through main-task cancellation so a service-manager stop tears down the same
way Ctrl-C does.
"""

import os
import sys
import signal
import asyncio
import logging

from .settings import settings
from .webrtc_mode import WebRTCService
from .selkies import DataStreamingServer
from .stream_server import CentralizedStreamServer


logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


async def wait_for_app_ready(ready_file: str, app_wait_ready: bool = False) -> None:
    """Wait for the streaming app's ready signal.

    Returns immediately unless `app_wait_ready` is set, else polls until a
    sidecar creates `ready_file`.
    """
    if app_wait_ready:
        logger.info(f"Waiting for streaming app ready file: {ready_file}")
    while app_wait_ready and not os.path.exists(ready_file):
        await asyncio.sleep(0.2)


def _install_shutdown_signal_handlers() -> None:
    """Make a service-manager stop (systemd, `docker stop`, `kill`) unwind the same
    way Ctrl-C does: cancelling the main task raises CancelledError through the
    server loop, so the streaming service is stopped, the unix socket is removed and
    the disconnect hooks run. Without this SIGTERM is fatal by default, and as
    container PID 1 it is ignored outright until SIGKILL.

    The first signal wins: later ones are absorbed while the teardown runs, since
    cancelling the main task again would raise CancelledError at an await inside
    the cleanup path and leave the rest of it (listener shutdown, unix-socket
    removal) undone. The handlers stay installed so an impatient orchestrator's
    repeat SIGTERM cannot fall through to the default fatal disposition either.
    """
    loop = asyncio.get_running_loop()
    main_task = asyncio.current_task()
    if main_task is None:
        return
    shutting_down = False

    def _request_shutdown(signal_name: str) -> None:
        nonlocal shutting_down
        if shutting_down:
            logger.info("Ignoring %s: shutdown already in progress", signal_name)
            return
        shutting_down = True
        logger.info("Received %s, shutting down", signal_name)
        main_task.cancel()

    for signal_name in ("SIGTERM", "SIGHUP"):
        sig = getattr(signal, signal_name, None)
        if sig is None:
            continue
        try:
            loop.add_signal_handler(sig, _request_shutdown, signal_name)
        except (NotImplementedError, RuntimeError, ValueError):
            logger.debug("Cannot install a %s handler on this platform", signal_name)


async def run() -> None:
    """Build the stream server, register its services, and run until cancelled."""
    _install_shutdown_signal_handlers()

    # Publish the resolved gamepad-socket directory so the LD_PRELOAD interposer in
    # app processes (which reads SELKIES_JS_SOCKET_PATH) writes/reads sockets in the
    # same directory selkies uses, regardless of how the setting was configured.
    os.environ["SELKIES_JS_SOCKET_PATH"] = settings.js_socket_path

    if settings.computer_use_bind:
        try:
            from pixelflux import start_computer_use
            start_computer_use(settings.computer_use_bind)
        except Exception as e:
            logger.warning(f"Computer-Use server not started: {e}")

    await wait_for_app_ready(settings.app_ready_file, settings.app_wait_ready[0])

    server = CentralizedStreamServer(settings)

    server.register_service("webrtc", WebRTCService(server))
    server.register_service("websockets", DataStreamingServer(server))

    logger.info(f"Initiating server with {settings.mode} mode")
    await server.switch_to_mode(settings.mode)

    await server.run()


def main() -> None:
    """Entry point for command-line execution."""
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
    except asyncio.CancelledError:
        logger.info("Server stopped by signal")
    except Exception as e:
        logger.error(f"Error in main: {e}", exc_info=True)
        sys.exit(1)


if __name__ == "__main__":
    main()
