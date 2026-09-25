# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Process entry point for the Selkies streaming server.

Builds the centralized stream server, registers the WebRTC and WebSockets
services, switches to the configured mode, and runs the asyncio loop until a
signal or fatal error unwinds it. Signal handling routes SIGTERM/SIGHUP
through main-task cancellation so a service-manager stop tears down the same
way Ctrl-C does. `selkies --version` prints the package version and exits.
"""

import sys

from . import __version__

# Before the settings import parses argv: --version needs neither a
# configuration nor the native extensions.
if "--version" in sys.argv[1:]:
    print(f"selkies {__version__}")
    sys.exit(0)

import os
import signal
import asyncio
import logging
from importlib.metadata import PackageNotFoundError, version

from .settings import settings, socket_dir
from .display_utils import cursor_size_for_dpi, restore_dpi, set_cursor_size
from .webrtc_mode import WebRTCService
from .websockets_mode import DataStreamingServer
from .stream_server import CentralizedStreamServer
from .webcam import stop_shared_webcam
from . import audit


logger = logging.getLogger("main")


def _startup_summary() -> str:
    """The one line that says what this server came up as.

    Transport, capture backend, encoder and rate, how the desktop is sized,
    and which of audio, gamepads, and access control are on: the facts a log
    reader needs before any client line makes sense.
    """
    try:
        release = version("selkies")
    except PackageNotFoundError:
        release = "unknown"
    if settings.wayland[0]:
        host = settings.wayland_host_display
        backend = f"Wayland (host compositor '{host}')" if host else "Wayland"
    else:
        backend = f"X11 ({os.environ.get('DISPLAY') or 'no DISPLAY'})"
    fps_lo, fps_hi = settings.framerate
    fps = f"{fps_lo}" if fps_lo == fps_hi else f"{fps_lo}-{fps_hi}"
    if settings.manual_resolution[0]:
        size = f"fixed {int(settings.manual_width or 0)}x{int(settings.manual_height or 0)}"
    elif settings.enable_resize[0]:
        size = "sized by the client"
    else:
        size = "desktop size kept"
    if settings.master_token:
        access = "token"
    elif settings.enable_basic_auth[0]:
        access = "basic auth"
    else:
        access = "open"
    return (
        f"Selkies {release} starting: {settings.mode} transport, {backend} capture, "
        f"encoder {settings.encoder} at {fps} fps, {size}, "
        f"audio {'on' if settings.audio_enabled[0] else 'off'}, "
        f"gamepads {'on' if settings.gamepad_enabled[0] else 'off'}, access {access}."
    )


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
    way Ctrl-C does: canceling the main task raises CancelledError through the
    server loop, so the streaming service is stopped, the unix socket is removed, and
    the disconnect hooks run. Without this SIGTERM is fatal by default, and as
    container PID 1 it is ignored outright until SIGKILL.

    The first signal wins: later ones are absorbed while the teardown runs, since
    canceling the main task again would raise CancelledError at an await inside
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
    """Build the stream server, register its services, and run until canceled.

    Publishes the resolved gamepad and webcam socket directories to the
    environment first, so the LD_PRELOAD interposers in app processes (which
    read `SELKIES_JS_SOCKET_PATH` and `SELKIES_WEBCAM_SOCKET_PATH`) use the
    same directories selkies does however the settings were supplied. The
    virtual webcam outlives mode switches (applications hold `/dev/videoN`
    open across them), so it is released only when the server exits. The
    encoders this host serves are resolved once here, off the loop since the
    hardware probe opens the GPU, before either transport publishes a menu. On
    X11 the desktop's density is settled here as well, before either transport
    can take a page, so the first page's DPI is compared against what the
    desktop has rather than against unity.
    """
    _install_shutdown_signal_handlers()

    os.environ["SELKIES_JS_SOCKET_PATH"] = socket_dir(settings.js_socket_path)
    os.environ["SELKIES_WEBCAM_SOCKET_PATH"] = socket_dir(settings.webcam_socket_path)

    if settings.computer_use_bind:
        try:
            from pixelflux import start_computer_use
            start_computer_use(settings.computer_use_bind)
        except Exception as e:
            logger.warning(f"Computer-Use server not started: {e}")

    await wait_for_app_ready(settings.app_ready_file, settings.app_wait_ready[0])

    if not settings.wayland[0]:
        startup_dpi = await restore_dpi(int(float(getattr(settings, "scaling_dpi", "96") or 96)),
                                        settings.was_provided("scaling_dpi"))
        if settings.cursor_size > 0:
            await set_cursor_size(cursor_size_for_dpi(startup_dpi, settings.cursor_size))

    await asyncio.to_thread(settings.resolve_encoder_backends)
    server = CentralizedStreamServer(settings)

    server.register_service("webrtc", WebRTCService(server))
    server.register_service("websockets", DataStreamingServer(server))

    logger.info(_startup_summary())
    await server.switch_to_mode(settings.mode)

    try:
        await server.run()
    finally:
        await stop_shared_webcam()
        await audit.close()


def main() -> None:
    """Entry point for command-line execution.

    Runs under uvloop when installed, else the stock loop: uvloop makes the
    whole loop (timers, callbacks, socket I/O) markedly faster, which lifts
    the pure-Python WebRTC SCTP data-channel throughput and keeps large
    transfers from stalling input. `uvloop.run` owns how its loop is
    installed per interpreter, so no event-loop policy API (removed in
    Python 3.16) is touched here.
    """
    try:
        import uvloop
        runner = uvloop.run
    except ImportError:
        runner = asyncio.run
    try:
        runner(run())
    except KeyboardInterrupt:
        logger.info("Server stopped by user")
    except asyncio.CancelledError:
        logger.info("Server stopped by signal")
    except Exception as e:
        logger.error(f"Error in main: {e}", exc_info=True)
        sys.exit(1)


if __name__ == "__main__":
    main()
