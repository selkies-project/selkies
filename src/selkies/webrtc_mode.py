# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright 2019 Google LLC
#
#   Licensed under the Apache License, Version 2.0 (the "License");
#   you may not use this file except in compliance with the License.
#   You may obtain a copy of the License at
#
#        http://www.apache.org/licenses/LICENSE-2.0
#
#   Unless required by applicable law or agreed to in writing, software
#   distributed under the License is distributed on an "AS IS" BASIS,
#   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#   See the License for the specific language governing permissions and
#   limitations under the License.

"""WebRTC streaming mode: the service that pairs pixelflux capture with RTC peers.

This module hosts ``WebRTCService``, the WebRTC counterpart of the websockets
streaming engine. It owns one ``MediaPipelinePixel`` per display, the vendored
RTC stack (``RTCApp``), the internal signaling client, the input handler, and
the TURN/STUN credential monitors, and wires them together with callbacks on a
single asyncio event loop (blocking pixelflux/compositor queries are pushed to
threads via ``asyncio.to_thread``).

Feature parity with the websockets transport is a design requirement, so the
same policies are re-implemented against RTC primitives here: per-display
client-tunable video settings, the extended-desktop layout for a secondary
display (xrandr monitors on X11, compositor outputs on Wayland), the
all-consumers-paused capture stop, keyframe-request throttling, the DPI/scale
ladder, token reconciliation for live peers, and the shared virtual-microphone
control plane.
"""

import sys
import time
import json
import logging
import asyncio
import argparse

from aiohttp import web
from typing import Any, Awaitable, Callable, Dict, List, Optional, Tuple, Union

from .rtc import RTCApp, ClientType
from . import selkies as selkies_module
from .selkies import current_session_tokens, SCALING_DPI_MIN, SCALING_DPI_MAX
from .media_pipeline import (MediaPipelinePixel, RateControlMode,
                             ScreenCapture as PixelfluxScreenCapture)
from .webrtc.codecs import configure_multiopus
from .webrtc_signaling import WebRTCSignalingClient
from .signaling_server import WebRTCPeerManagement
from .input_handler import WebRTCInput
from .display_utils import (resize_display, set_dpi, set_cursor_size, parse_gpu_id,
                            compute_dual_layout, apply_extended_layout, get_new_res,
                            clear_selkies_monitors, clamp_primary_feedback,
                            MultiMonitorWindowManager,
                            wayland_output_id, wayland_reposition_primary,
                            session_screen_index,
                            parse_resize_dims, cursor_size_for_dpi, align_dims_16)
from .webrtc_utils import SystemMonitor, Metrics, GPUMonitor, get_rtc_configuration
from .settings import (settings, AppSettings, SETTING_DEFINITIONS,
                       build_client_settings_payload, sanitize_client_setting)
from types import SimpleNamespace
from .webrtc_utils import HMACRTCMonitor, RESTRTCMonitor, RTCConfigFileMonitor, CloudflareRTCMonitor
from .stream_server import BaseStreamingService, CentralizedStreamServer
from .audio_control import AudioControl

logger = logging.getLogger("webrtc")


def _selkies_is_aioice_frame_chain(exc: BaseException) -> bool:
    """Return True when the exception's traceback passes through the vendored
    ICE package (selkies.ice, the incorporated aioice), allowing asyncio's own
    event-loop frames."""
    tb = getattr(exc, "__traceback__", None)
    saw_ice = False
    while tb is not None:
        if "selkies/ice/" in tb.tb_frame.f_code.co_filename:
            saw_ice = True
        tb = tb.tb_next
    return saw_ice


class _AsyncioSendNoiseFilter(logging.Filter):
    """Drop asyncio's bare 'socket.send() raised exception.' warning: it fires
    per ICMP-unreachable UDP send, which is routine while ICE candidate pairs
    are probed or torn down, and carries no actionable detail."""

    def filter(self, record: logging.LogRecord) -> bool:
        return record.getMessage() != "socket.send() raised exception."


def _install_webrtc_teardown_noise_filters(loop: asyncio.AbstractEventLoop) -> None:
    """Demote KNOWN-benign aioice teardown noise; everything else surfaces
    unchanged. Two mechanisms, both installed once per loop/process:

    - loop exception handler: aioice STUN retry timers fire after their
      transport was closed (AttributeError on .sendto / the cascaded
      .call_exception_handler), and TURN send_data tasks that outlive the
      allocation end in never-retrieved TransactionTimeout. Both are pure
      teardown races on sessions already gone.
    - logging filter on the 'asyncio' logger for the bare per-datagram
      'socket.send() raised exception.' warning.
    """
    asyncio_logger = logging.getLogger("asyncio")
    if not any(isinstance(f, _AsyncioSendNoiseFilter) for f in asyncio_logger.filters):
        asyncio_logger.addFilter(_AsyncioSendNoiseFilter())

    if getattr(loop, "_selkies_webrtc_noise_filter", False):
        return
    loop._selkies_webrtc_noise_filter = True
    previous = loop.get_exception_handler()

    def handler(l: asyncio.AbstractEventLoop, context: Dict[str, Any]) -> None:
        exc = context.get("exception")
        if exc is not None:
            if isinstance(exc, AttributeError) and _selkies_is_aioice_frame_chain(exc) and (
                "'NoneType' object has no attribute 'sendto'" in str(exc)
                or "'NoneType' object has no attribute 'call_exception_handler'" in str(exc)
            ):
                logger.debug(f"aioice teardown race ignored: {exc}")
                return
            if exc.__class__.__name__ == "TransactionTimeout" and (
                _selkies_is_aioice_frame_chain(exc)
                or "selkies/ice/" in str(context.get("future", ""))
            ):
                logger.debug("aioice TURN transaction outlived its allocation; ignored.")
                return
        if previous is not None:
            previous(l, context)
        else:
            l.default_exception_handler(context)

    loop.set_exception_handler(handler)
logger.setLevel(logging.INFO)

# Cursor base size in points at 96 DPI (DPI changes scale from it): the
# cursor_size setting (SELKIES_CURSOR_SIZE / XCURSOR_SIZE) when explicit;
# None means "auto" (platform default), which disables every cursor-size
# override so a later DPI sync never stomps the compositor/DE choice.
CURSOR_SIZE: Optional[int] = settings.cursor_size if settings.cursor_size > 0 else None
# Same switch selkies.py uses (SELKIES_WAYLAND / --wayland):
# the input backend must match the capture backend, which gets the choice per
# capture via the CaptureSettings use_wayland field.
IS_WAYLAND: bool = bool(settings.wayland[0])

def get_server_settings() -> Dict[str, Any]:
    """The server-settings payload every client is greeted with."""
    return {"settings": build_client_settings_payload()}


class WebRTCService(BaseStreamingService):
    """The WebRTC streaming service run under the centralized stream server.

    Owns the whole WebRTC data path for a session: signaling, peer management,
    one media pipeline per display, input handling, monitoring, and the
    congestion-control/pacer loop. Mirrors the websockets service's policies
    (per-display settings, extended-desktop layout, capture pause rules) so the
    two transports stay in behavioral parity.

    Attributes:
        media_pipeline: The primary display's pipeline (also stored under the
            ``"primary"`` key of ``display_pipelines``).
        display_pipelines: One ``MediaPipelinePixel`` per connected display id.
        display_clients: Per-secondary-display registration state — requested
            dimensions, position, and the display's own copies of the
            client-tunable video settings.
        display_layouts: The computed extended-desktop layout the input
            handler offsets coordinates against.
        args: Mutable per-session snapshot of the client-tunable settings
            (seeded from ``SETTING_DEFINITIONS``); the primary display's
            authority, and the seed for joining secondaries.
    """

    def __init__(self, supervisor: CentralizedStreamServer) -> None:
        super().__init__("webrtc")
        self.settings: Optional[AppSettings] = settings
        self.tasks: List[asyncio.Task] = []
        self.shutdown_event = asyncio.Event()
        self._shutdown_called = False
        self.signaling_client: Optional[WebRTCSignalingClient] = None
        self.media_pipeline: Optional[MediaPipelinePixel] = None
        self.rtc_app: Optional[RTCApp] = None
        self.input_handler: Optional[WebRTCInput] = None
        self.system_monitor: Optional[SystemMonitor] = None
        self.gpu_monitor: Optional[GPUMonitor] = None
        self.metrics: Optional[Metrics] = None
        self.peer_id = 1
        self.args: Optional[SimpleNamespace] = None
        self.monitoring_utils_used: Dict[str, bool] = {}
        self.mon_hmac_turn: Optional[HMACRTCMonitor] = None
        self.mon_rest_api: Optional[RESTRTCMonitor] = None
        self.mon_rtc_config_file: Optional[RTCConfigFileMonitor] = None
        self.mon_cloudflare_turn: Optional[CloudflareRTCMonitor] = None
        self.peer_manager: Optional[WebRTCPeerManagement] = None
        self.supervisor = supervisor
        # Multi-display state (websockets-parity model): connected secondary
        # display clients, the computed extended-desktop layout the input
        # handler offsets against, and one media pipeline per display.
        self.display_clients: Dict[str, Dict[str, Any]] = {}
        self.display_layouts: Dict[str, Dict[str, int]] = {}
        self.display_pipelines: Dict[str, MediaPipelinePixel] = {}
        self._last_idr_request_times: Dict[str, float] = {}
        self._display_lock = asyncio.Lock()
        self._primary_dims: Optional[Tuple[int, int]] = None
        # Fallback pixelflux handle for Wayland output management when the
        # primary pipeline has no live capture module (any handle reaches the
        # shared compositor backend).
        self._wayland_ctl_module: Optional[Any] = None
        # Host-capture mode: how many outputs the host compositor can back displays
        # with; None until a query answers, and never populated when self-compositing
        # (outputs are minted on demand there).
        self._host_output_capacity: Optional[int] = None
        # Last (w, h) a client asked the primary to become. The realized size
        # may legitimately differ (CVT cell alignment widens the mode), so
        # idempotence must be judged against the request, not just the result.
        self._last_resize_request: Optional[Tuple[int, int]] = None
        # Multi-monitor WM swap (websockets parity): heavy DEs tile poorly across the
        # per-display regions, so swap to a minimal Openbox once a secondary joins.
        self._wm_swap = MultiMonitorWindowManager()
        # Primary-capture reconnect grace (websockets RECONNECT_GRACE_S parity): a
        # controller tab reload drops and re-adds its peer within a second or two,
        # so the primary capture stop is deferred by this window and cancelled if a
        # consumer reclaims it — viewers and display2 stream through the reload.
        self.RECONNECT_GRACE_S = 3.0
        self._primary_stop_grace_task: Optional[asyncio.Task] = None

        # Shared SelkiesVirtualMic control plane for the WebRTC mic path: one
        # sound-server control connection and one virtual-source module for the
        # service, provisioned once on the first mic packet (the data plane is
        # per-peer pcmflux playback into the 'input' sink). Idempotent with the
        # websockets 0x02 path — a source it already loaded is reused, never
        # double-loaded — and only unloaded here when this path loaded it.
        self._mic_control: Optional[AudioControl] = None
        self._mic_module_index: Optional[int] = None
        self._mic_module_owned = False
        self._mic_provisioned = False
        self._mic_provision_lock = asyncio.Lock()

        self._init_default_settings()

    def _init_default_settings(self) -> None:
        """Seed ``self.args`` from ``SETTING_DEFINITIONS`` and derive the manual
        startup geometry.

        Range settings pin to the single allowed value when the range is locked
        (min == max), otherwise take the definition's default; other types copy
        the configured value verbatim.
        """
        self.args = SimpleNamespace()
        try:
            for setting_def in SETTING_DEFINITIONS:
                name = setting_def["name"]
                stype = setting_def["type"]
                if stype == "bool":
                    value = getattr(self.settings, name)[0]
                elif stype == "range":
                    min, max = getattr(self.settings, name)
                    value = (
                        min
                        if min == max
                        else setting_def.get("meta", {}).get("default_value", 0)
                    )
                elif stype == "enum":
                    value = getattr(self.settings, name)
                elif stype in ("int", "float", "str", "list"):
                    value = getattr(self.settings, name)
                else:
                    continue
                setattr(self.args, name, value)
        except Exception as e:
            logger.error(f"Error initializing default settings: {e}", exc_info=True)

        # Initial display size: honor a configured manual resolution; otherwise leave
        # the display as-is (physical/preset displays stay untouched) — the first
        # client reconfigures it to its own size anyway. The resize itself runs in
        # initialize_components (async context; on Wayland there is no X server to
        # resize — the capture start sizes the compositor output from these dims).
        self._manual_dims: Optional[Tuple[int, int]] = None
        if getattr(self.args, "is_manual_resolution_mode", False):
            width = int(getattr(self.args, "manual_width", 0) or 0)
            height = int(getattr(self.args, "manual_height", 0) or 0)
            if width > 0 and height > 0:
                self._manual_dims = (width - (width % 2), height - (height % 2))

    async def initialize_components(self) -> None:
        """Build every component: metrics, signaling, the primary media
        pipeline, the RTC app, the input handler, and the monitors, then wire
        the peer manager with the fetched RTC configuration."""

        # Re-snapshot settings: this service is constructed once at boot, but a
        # live transport switch lands here with the settings singleton already
        # re-resolved for webrtc (encoder filter, rate-control default), and
        # the pipeline below must be built from those values, not the boot-time
        # copy.
        self._init_default_settings()

        # Metrics backs BOTH the Prometheus endpoint and the WebRTC CSV statistics,
        # so build it when either flag is on: CSV-only configs must not leave
        # self.metrics as None (session start dereferences it for the CSV file).
        if self.args.enable_metrics_http or self.args.enable_webrtc_statistics:
            webrtc_csv = self.args.enable_webrtc_statistics
            self.metrics = Metrics(using_webrtc_csv=webrtc_csv)

        # Init signaling client
        self.signaling_client = self.create_signaling_client()

        # Surround (>2ch) is carried as Chromium's multiopus codec; swap the offered
        # audio codec set before any peer connection builds its capabilities.
        if int(self.args.audio_channels) > 2:
            configure_multiopus(int(self.args.audio_channels))

        self.media_pipeline = MediaPipelinePixel(
            async_event_loop=asyncio.get_running_loop(),
            encoder=self.args.encoder,
            framerate=int(self.args.framerate),
            # kbps, as consumed by pixelflux.
            video_bitrate=int(self.args.video_bitrate),
            # enum with a wider server-side value_range, so an operator override
            # can arrive as an arbitrary numeric string
            audio_bitrate=int(float(self.args.audio_bitrate)),
            audio_channels=int(self.args.audio_channels),
            audio_enabled=self.args.audio_enabled,
            audio_device_name=self.args.audio_device_name,
            crf=int(self.args.video_crf),
            video_fullcolor=bool(self.args.video_fullcolor),
            use_cpu=bool(self.args.use_cpu),
            video_streaming_mode=bool(self.args.video_streaming_mode),
            use_paint_over_quality=bool(self.args.use_paint_over_quality),
            video_paintover_crf=int(self.args.video_paintover_crf),
            video_paintover_burst_frames=int(self.args.video_paintover_burst_frames),
        )
        if self._manual_dims:
            # The pipeline must capture the manual geometry, not its constructor
            # default: on X11 resize the screen to it now, and on Wayland these
            # dimensions ARE the resize (the capture start sizes the compositor
            # output from them).
            if not IS_WAYLAND:
                realized = await resize_display(f"{self._manual_dims[0]}x{self._manual_dims[1]}")
                if realized:
                    # Capture what the X server realized (CVT cell alignment can
                    # widen the mode), never a region the root may not cover.
                    self._manual_dims = realized
            self.media_pipeline.width, self.media_pipeline.height = self._manual_dims
        if self.args.enable_rate_control:
            self.media_pipeline.rc_mode = RateControlMode(self.args.rate_control_mode)
        else:
            # WS parity: with rate control disabled the engine runs CRF on both
            # transports.
            self.media_pipeline.rc_mode = RateControlMode.CRF

        # Fetch rtc configuration
        (
            stun_servers,
            turn_servers,
            rtc_config,
            self.monitoring_utils_used,
        ) = await get_rtc_configuration(self.args)
        self.rtc_app = RTCApp(
            async_event_loop=asyncio.get_running_loop(),
            encoder=self.args.encoder,
            stun_servers=stun_servers,
            turn_servers=turn_servers,
        )
        self.rtc_app.media_pipeline = self.media_pipeline
        self.rtc_app.provision_virtual_mic = self._provision_webrtc_virtual_mic
        self.display_pipelines["primary"] = self.media_pipeline

        # Input handler
        self.input_handler = WebRTCInput(
            rtc_app=self.rtc_app,
            uinput_mouse_socket_path=getattr(self.args, "uinput_mouse_socket", "") or "",
            # Same setting as the websockets service: the interposer sockets are
            # shared process-wide state, so both transports must agree on the path.
            js_socket_path_prefix=getattr(self.args, "js_socket_path", "/tmp"),
            enable_clipboard=self.args.enable_clipboard,
            enable_binary_clipboard="true"
            if self.args.enable_binary_clipboard
            else "false",
            enable_cursors=self.args.enable_cursors,
            cursor_size=self.args.cursor_size,
            cursor_scale=1.0,
            cursor_debug=self.args.debug_cursors,
            upload_dir=self.args.file_manager_path,
            is_wayland=IS_WAYLAND,
            app_wayland_display=(getattr(self.args, "app_wayland_display", "")
                                 or getattr(self.args, "wayland_host_display", "")),
            # Same setting as the websockets service: kernel gamepads are
            # process-wide, so both transports must resolve them identically.
            uinput_gamepad=getattr(self.args, "uinput_gamepad", "auto"),
            # Duck-typed layout source: send_x11_mouse offsets a secondary
            # display's coordinates by display_layouts[display_id].
            data_server_instance=self,
        )
        self.input_handler.initialize_upload_dir()
        if IS_WAYLAND:
            # Seed the compositor capture scale from the configured DPI so the
            # FIRST pipeline start honors it; handle_scaling updates it on later
            # client DPI syncs. The input handler owns the policy: a nested app
            # session keeps the output at scale 1.0 and takes its DPI as Xft
            # resources instead.
            self.media_pipeline.scale = await self.input_handler.realize_wayland_dpi(
                getattr(settings, "scaling_dpi", "96") or 96)

        # Initialize monitoring instances
        self.system_monitor = SystemMonitor()
        # Always on: gpu_stats reports nothing when no supported GPU/tool is present.
        # Keyed to the pipeline's render node so stats describe the encoding GPU.
        stats_gpu_id = parse_gpu_id(getattr(self.args, "gpu_id", ""))
        self.gpu_monitor = GPUMonitor(
            gpu_id=stats_gpu_id if (stats_gpu_id or 0) > 0 else 0,
            enabled=True,
            dri_node=getattr(self.args, "encode_dri", "") or "",
        )

        self.create_peer_manager(rtc_config)

    def create_signaling_client(self) -> WebRTCSignalingClient:
        """Create and configure signaling client."""
        using_https = self.args.enable_https
        using_basic_auth = self.args.enable_basic_auth
        ws_protocol = "wss:" if using_https else "ws:"

        prefix = self.settings.subfolder
        username = self.settings.basic_auth_user
        password = self.settings.basic_auth_password
        client = WebRTCSignalingClient(
            f"{ws_protocol}//127.0.0.1:{self.args.port}{prefix}/api/ws",
            enable_https=using_https,
            enable_basic_auth=using_basic_auth,
            basic_auth_user=username,
            basic_auth_password=password,
            server_token=getattr(self.settings, "master_token", None),
        )
        return client

    async def handle_signaling_error(self, error: Exception) -> None:
        """Handle signaling errors."""
        logger.error(f"Signaling client error: {error}. Closing the pipelines")
        await self.handle_signaling_disconnect()

    async def handle_signaling_disconnect(self) -> None:
        """Tear down every RTC connection once the signaling link drops."""
        logger.info("Signaling disconnected, cleaning up all resources")
        try:
            await self.rtc_app.stop_all_rtc_connections()
        except Exception as e:
            logger.error(
                f"Error during signaling disconnect cleanup: {e}", exc_info=True
            )

    async def handle_session_start(
        self, session_peer_id: str, client_type: str, client_token: Optional[str] = None,
        display_id: str = "primary", display_position: str = "right",
    ) -> None:
        """Start an RTC connection for a joining peer.

        A secondary display's controller is gated on the effective second-screen
        availability first: the published setting can lag a host-side change, so
        the capacity is re-read and a refusal closes the peer's signaling socket
        with a fatal verdict (a bare return would leave the page on
        "Connecting..." forever).

        Args:
            session_peer_id: The signaling peer id of the joining client.
            client_type: "controller" or a viewer/shared role name.
            client_token: Optional per-client auth token (governs input role).
            display_id: The display this peer consumes ("primary" or a
                secondary id).
            display_position: Where a joining secondary sits relative to the
                primary ("right", "left", "up", "down").
        """
        logger.info(
            f"starting session for client peer id: {session_peer_id} of type: {client_type} (display '{display_id}')"
        )
        try:
            if display_id != "primary" and client_type == "controller":
                # Authoritative gate (the published setting can lag a host-side
                # change): re-read the capacity, then refuse with the concrete
                # reason.
                await self._refresh_second_screen_capacity()
                available, reason = self._second_screen_availability()
                if not available:
                    logger.warning(
                        "Secondary display '%s' refused: %s", display_id, reason,
                    )
                    # Fatal verdict: a bare return leaves the signaling socket
                    # open and the page on "Connecting..." forever.
                    await self._close_peer_signaling_ws(
                        session_peer_id, 4000, reason.encode(),
                    )
                    return
                # Dimensions arrive through the client's first resize message.
                entry = self.display_clients.setdefault(display_id, {"width": 0, "height": 0})
                entry["position"] = display_position
                self._seed_display_settings(entry)
            await self.rtc_app.start_rtc_connection(session_peer_id, client_type, client_token, display_id)
            # Initialize stats location directory
            if self.args.enable_webrtc_statistics and self.metrics:
                await self.metrics.initialize_webrtc_csv_file(self.args.webrtc_statistics_dir)
            logger.info(f"started session for client peer id {session_peer_id}")
        except Exception as e:
            logger.error(
                f"Error starting session for client peer id {session_peer_id}: {e}",
                exc_info=True,
            )
            await self.rtc_app.stop_rtc_connection(session_peer_id, client_type)

    async def handle_session_end(self, session_peer_id: str, client_type: str) -> None:
        """Handle end of a session initiated by a client.
        Stops the RTC connection and media pipeline for the given session peer id.
        """
        try:
            if self.rtc_app:
                await self.rtc_app.stop_rtc_connection(session_peer_id, client_type)
            logger.info(
                f"session ended for client peer id {session_peer_id} of type {client_type}"
            )
        except Exception as e:
            logger.error(
                f"Error handling session end for {session_peer_id}: {e}", exc_info=True
            )

    def create_peer_manager(self, rtc_config: Any) -> None:
        """Build the signaling-side peer manager with the fetched RTC config
        and the TURN/STUN/sharing options the handshake needs."""
        options = argparse.Namespace(
            keepalive_timeout=30,
            rtc_config_file=self.args.rtc_config_json,
            turn_shared_secret=self.args.turn_shared_secret,
            rtc_config=rtc_config,
            turn_host=self.args.turn_host,
            turn_port=self.args.turn_port,
            turn_protocol=self.args.turn_protocol,
            turn_tls=self.args.turn_tls,
            turn_auth_header_name=self.args.turn_rest_username_auth_header,
            stun_host=self.args.stun_host,
            stun_port=self.args.stun_port,
            enable_sharing=self.args.enable_sharing,
            enable_shared=self.args.enable_shared,
            enable_player2=self.args.enable_player2,
            enable_player3=self.args.enable_player3,
            enable_player4=self.args.enable_player4,
        )
        self.peer_manager = WebRTCPeerManagement(options)
        self.peer_manager.on_client_presence = self.supervisor.set_clients_present

    def setup_callbacks(self) -> None:
        """Configure all application callbacks."""
        if not self.rtc_app or not self.media_pipeline or not self.input_handler:
            return

        # Signaling client callbacks
        self.signaling_client.on_error = self.handle_signaling_error
        self.signaling_client.on_disconnect = self.handle_signaling_disconnect
        self.signaling_client.on_session_start = self.handle_session_start
        self.signaling_client.on_session_end = self.handle_session_end
        self.signaling_client.on_sdp = self.rtc_app.set_sdp
        self.signaling_client.on_ice = self.rtc_app.set_ice

        self.media_pipeline.produce_data = self.rtc_app.consume_data
        # Resend cursor on pipeline (re)start: a slept/woken tab clears its cursor canvas.
        self.media_pipeline.on_pipeline_started = self.send_current_cursor

        # RTCApp callbacks
        self.rtc_app.request_idr_frame = self.request_idr_for_display
        self.rtc_app.start_display_media = self.start_display_media
        self.rtc_app.stop_display_media = self.stop_display_media
        self.rtc_app.on_sdp = self.signaling_client.send_sdp
        self.rtc_app.on_ice = self.signaling_client.send_ice
        self.rtc_app.on_data_open = self.handle_data_channel_open
        self.rtc_app.on_data_close = lambda: logger.info("Data channel closed")
        self.rtc_app.on_data_error = lambda e: logger.error(f"Data channel error: {e}")
        self.rtc_app.on_data_message = self.input_handler.on_message
        self.rtc_app.on_peer_gone = self.handle_peer_gone
        self.input_handler.on_request_keyframe = self.request_idr_for_display

        # Input handler callbacks
        self.input_handler.on_cursor_change = lambda data: (
            self.rtc_app.send_cursor_data(data)
        )
        # Cursors come from pixelflux on both backends (Wayland compositor /
        # X11 XFixes monitor); route them through the same transport callback,
        # capped at the input handler's DPI-scaled cursor size.
        self.media_pipeline.on_cursor_data = lambda data: (
            self.input_handler.on_cursor_change(data)
        )
        self.media_pipeline.get_cursor_size_cap = lambda: getattr(
            self.input_handler, "cursor_size_cap", 0
        )
        self.input_handler.on_video_encoder_bit_rate = self.handle_video_bitrate_change
        self.input_handler.on_audio_encoder_bit_rate = self.handle_audio_bitrate_change
        # Native-cursor capture toggles every display's capture (websockets parity:
        # its capture_cursor tunable is global across displays).
        self.input_handler.on_mouse_pointer_visible = self.handle_pointer_visible
        self.input_handler.on_clipboard_read = lambda d, t: (
            self.rtc_app.send_clipboard_data(d, t)
        )
        self.input_handler.on_set_fps = self.handle_fps_change
        self.input_handler.on_client_fps = lambda fps: (
            self.metrics.set_fps(fps) if self.metrics else None
        )
        self.input_handler.on_client_latency = lambda latency: (
            self.metrics.set_latency(latency) if self.metrics else None
        )
        self.input_handler.on_ping_response = lambda latency: (
            self.rtc_app.send_latency_time(latency)
        )
        self.input_handler.on_client_webrtc_stats = self.handle_client_werbtc_stats
        self.input_handler.on_update_settings = self.handle_update_settings
        self.input_handler.on_update_rate_control_mode = self.handle_rate_control_change
        self.input_handler.on_update_crf = self.handle_crf_change
        # Offers resolve their codec/SDP munging per display, so displays can run
        # different encoders and chroma formats (a live full-colour toggle has to
        # reach the profile every later offer advertises).
        self.rtc_app.get_encoder_for_display = self._encoder_for_display
        self.rtc_app.get_fullcolor_for_display = self._fullcolor_for_display
        self.rtc_app.get_use_cpu_for_display = self._use_cpu_for_display
        # Per-peer tab-visibility pause (data-channel STOP_VIDEO/START_VIDEO)
        # and the consumer-set re-check on peer departure.
        self.rtc_app.on_video_consumer_active = self.handle_video_consumer_active
        self.rtc_app.on_consumers_changed = self.handle_consumers_changed
        # Token updates (/api/tokens) must reconcile LIVE WebRTC peers too:
        # revocation closes them, mk handoffs push over the data channel.
        selkies_module.webrtc_reconcile_hook = self.reconcile_webrtc_peers

        # DPI scaling is independent of enable_resize, which gates only dynamic
        # resolution changes. The WebSocket transport applies scaling through the
        # SETTINGS payload regardless of the resize gate, so wire scaling here too.
        self.input_handler.on_scaling_ratio = self.handle_scaling
        # A secondary display's whole bring-up rides its resize message, so it must not be
        # gated; enable_resize gates only the primary's dynamic resolution (in on_resize_handler).
        self.input_handler.on_resize = self.on_resize_handler

        # Monitoring callbacks
        self.gpu_monitor.on_stats = self.handle_gpu_stats
        self.system_monitor.on_timer = self.handle_system_monitor

    def _second_screen_availability(self) -> Tuple[bool, str]:
        """Whether this session can actually attach a second display, and the
        reason when it cannot (websockets-mode parity).

        The admin flag gates first; past it, X11 and the self-composited
        Wayland backend mint another output on demand, while host capture is
        bounded by the host compositor's real output count (unknown until a
        pipeline start establishes the host session).

        Returns:
            A ``(available, reason)`` pair; ``reason`` is empty when available.
        """
        enabled, _ = self.settings.second_screen
        if not enabled:
            return False, "Second screens are disabled on this server."
        if not IS_WAYLAND or not (self.settings.wayland_host_display or "").strip():
            return True, ""
        capacity = self._host_output_capacity
        if capacity is None or capacity < 0:
            return False, "The host compositor's outputs are not known yet."
        if capacity < 2:
            return False, "The host compositor has a single output, so a second display has nothing to capture."
        return True, ""

    async def _refresh_second_screen_capacity(self) -> bool:
        """Host-capture mode only: re-read how many outputs the host exposes.

        Returns:
            True when the answer changed, i.e. the second-screen availability
            that clients were told may have flipped.
        """
        if not IS_WAYLAND or not (self.settings.wayland_host_display or "").strip():
            return False
        module = self._wayland_capture_handle()
        if module is None or not hasattr(module, "output_capacity"):
            return False
        try:
            capacity = int(await asyncio.to_thread(module.output_capacity))
        except Exception as e:
            logger.warning(f"Wayland output capacity query failed: {e}")
            return False
        changed = capacity != self._host_output_capacity
        self._host_output_capacity = capacity
        return changed

    def _server_settings_payload(self) -> Dict[str, Any]:
        """get_server_settings with second_screen published as EFFECTIVE
        availability — the admin flag AND the backend's real capacity — so
        dashboards never offer a second display the server would immediately
        refuse."""
        payload = get_server_settings()
        available, _ = self._second_screen_availability()
        entry = payload.get("settings", {}).get("second_screen")
        if isinstance(entry, dict) and entry.get("value") and not available:
            payload["settings"]["second_screen"] = dict(entry, value=False)
        # Terminal the apps panel launches in, chosen by the session's windowing
        # system (absent when none is installed: the client keeps its default).
        terminal = self.input_handler.app_terminal() if self.input_handler else None
        if terminal:
            payload["settings"]["app_terminal"] = {"value": terminal}
        return payload

    def handle_data_channel_open(self, channel: Optional[Any] = None) -> None:
        """Greet the peer that just joined: every display page and viewer needs
        the server settings for conditional UI, the current cursor, and the
        display roster. Sent on ITS channel when given; without one, falls back
        to broadcasting."""
        logger.info("opened peer data channel for user input to X11")
        server_settings_payload = self._server_settings_payload()
        if channel is not None:
            self.rtc_app.send_message_to_channel(
                channel, "server_settings", server_settings_payload
            )
            displays = ["primary"] + [d for d in self.display_clients.keys() if d != "primary"]
            self.rtc_app.send_message_to_channel(
                channel, "display_config_update", {"displays": displays}
            )
        else:
            self.rtc_app.send_media_data_over_channel(
                "server_settings", server_settings_payload
            )
            self._broadcast_display_config()
        self.send_current_cursor(channel)

    def send_current_cursor(self, channel: Optional[Any] = None) -> None:
        """Resend the current cursor (on channel open / video restart): to one
        peer's channel when given, otherwise to every connected peer.

        Idempotent; a slept/woken tab clears its cursor canvas and needs it back.
        """
        if not self.rtc_app:
            return
        cursor_data = None
        if self.input_handler:
            try:
                cursor_data = self.input_handler.get_current_cursor_data()
            except Exception as e:
                logger.warning(f"Failed to fetch current cursor data: {e}")
        # Fall back to the last cursor the app sent if a fresh one isn't available.
        if cursor_data is None:
            cursor_data = self.rtc_app.last_cursor_sent
        if not cursor_data:
            return
        try:
            if channel is not None:
                self.rtc_app.send_message_to_channel(channel, "cursor", cursor_data)
            else:
                self.rtc_app.send_cursor_data(cursor_data)
        except Exception as e:
            logger.warning(f"Failed to send current cursor to client: {e}")

    async def handle_pointer_visible(self, visible: bool) -> None:
        """Compose the cursor into the captured video, on every display's capture
        (the websockets capture_cursor tunable is likewise global)."""
        for pipeline in list(self.display_pipelines.values()):
            if pipeline is not None:
                await pipeline.set_pointer_visible(visible)

    async def handle_video_bitrate_change(self, bitrate: int, display_id: str = "primary") -> None:
        """Video bitrate change for the display whose page sent it; sanitized
        against the server's configured range like the SETTINGS path, so the
        opcode cannot bypass a locked/narrowed range."""
        sanitized = sanitize_client_setting("video_bitrate", bitrate, self.settings, logger)
        if sanitized is None or sanitized == self._display_setting(display_id, "video_bitrate"):
            return
        await self._apply_display_setting(display_id or "primary", "video_bitrate", sanitized)

    async def handle_audio_bitrate_change(self, bitrate: int) -> None:
        """Handle audio bitrate change request (bps; sanitized like SETTINGS)."""
        sanitized = sanitize_client_setting("audio_bitrate", bitrate, self.settings, logger)
        if sanitized is None or sanitized == getattr(self.args, "audio_bitrate", None):
            return
        if self.media_pipeline:
            await self.media_pipeline.set_audio_bitrate(int(sanitized))
        self.args.audio_bitrate = sanitized

    async def handle_fps_change(self, fps: int, display_id: str = "primary") -> None:
        """Framerate change for the display whose page sent it; sanitized against
        the server's configured range like the SETTINGS path."""
        sanitized = sanitize_client_setting("framerate", fps, self.settings, logger)
        if sanitized is None or sanitized == self._display_setting(display_id, "framerate"):
            return
        await self._apply_display_setting(display_id or "primary", "framerate", sanitized)

    async def handle_rate_control_change(self, mode: Any, display_id: str = "primary") -> None:
        """Rate-control switch for the display whose page sent it; honors the
        server's enable_rate_control lock like the SETTINGS path."""
        if self.args.enable_rate_control is False:
            logger.debug("Server has rate control disabled. Ignoring rate-control change.")
            return
        # Store the plain value: str(<str-Enum>) formats as the member name on
        # some supported Python versions, which would corrupt later comparisons.
        mode_str = mode.value if isinstance(mode, RateControlMode) else str(mode)
        await self._apply_display_setting(display_id or "primary", "rate_control_mode", mode_str)

    async def handle_crf_change(self, crf: int, display_id: str = "primary") -> None:
        """CRF change for the display whose page sent it; sanitized against the
        server's configured range like the SETTINGS path."""
        sanitized = sanitize_client_setting("video_crf", int(crf), self.settings, logger)
        if sanitized is None or sanitized == self._display_setting(display_id, "video_crf"):
            return
        await self._apply_display_setting(display_id or "primary", "video_crf", sanitized)

    async def handle_client_werbtc_stats(
        self, webrtc_stat_type: str, webrtc_stats: str
    ) -> None:
        """Ingest client-reported WebRTC stats, gated on the Metrics object
        itself (built for metrics-http AND/OR the CSV statistics flag) so
        CSV-only configs actually ingest the stats they enabled."""
        if self.metrics:
            await self.metrics.set_webrtc_stats(webrtc_stat_type, webrtc_stats)

    async def on_resize_handler(self, res: str, display_id: str = "primary") -> None:
        """Route a client resolution to its display: the primary resizes the real
        display directly while it is alone; once a secondary display is connected
        (or for any secondary), the resolution feeds the extended-desktop layout
        instead (websockets parity)."""
        display_id = display_id or "primary"
        if display_id == "primary" and not self.args.enable_resize:
            logger.warning(f"remote resizing disabled, skipping resize to {res}")
            return
        if display_id != "primary" or self.display_clients:
            locked_dims = self._server_locked_dims()
            if locked_dims is not None:
                # The layout path must honor the admin's manual-resolution lock the
                # way the single-display path does: every display follows the
                # server's geometry (websockets parity), so a second screen cannot
                # be the way a client escapes the lock. A locked size is the
                # server's own, so a client-side alignment toggle cannot alter it.
                logger.warning(
                    f"Client attempted to resize to {res} but server is in manual resolution mode. "
                    f"Using the configured {locked_dims[0]}x{locked_dims[1]} instead."
                )
                w, h = locked_dims
            else:
                dims = parse_resize_dims(res)
                if dims is None:
                    logger.error(f"Invalid resize request: {res}")
                    return
                w, h = dims
                if self._display_setting(display_id, "force_aligned_resolution"):
                    w, h = align_dims_16(w, h)
            if display_id == "primary":
                self._primary_dims = (w, h)
            else:
                entry = self.display_clients.get(display_id)
                if entry is None:
                    logger.warning(f"Resize for unknown display '{display_id}' ignored.")
                    return
                entry["width"], entry["height"] = w, h
            await self.reconfigure_displays()
            return
        self._primary_dims = None
        await self._resize_primary_display(res)

    def _server_locked_dims(self) -> Optional[Tuple[int, int]]:
        """The geometry an admin-configured manual-resolution lock pins the desktop
        to, or None when the server sets no lock. Derived on every read from the
        server settings (never from the client-writable args, whose manual trio is
        the client's own manual/auto toggle) and from the dimensions startup
        realized, so the lock cannot drift with client state."""
        server_is_manual, _ = self.settings.is_manual_resolution_mode
        if not server_is_manual:
            return None
        if self._manual_dims:
            return self._manual_dims
        # The settings layer guarantees positive manual dimensions while the lock
        # is on; its own defaults stand in if one is somehow unusable, so a locked
        # server never falls back to honoring the client's request.
        width = int(getattr(self.settings, "manual_width", 0) or 0)
        height = int(getattr(self.settings, "manual_height", 0) or 0)
        if width <= 0:
            width = 1024
        if height <= 0:
            height = 768
        return (width - (width % 2), height - (height % 2))

    async def _resize_primary_display(self, res: str) -> None:
        """Resize the single (primary-only) display to a client-requested
        resolution and keep the capture dimensions in sync with what was
        realized."""
        # Only an admin-configured manual-resolution lock (server config) blocks client
        # resizes, mirroring the WebSocket handler. The client's own manual/auto toggle
        # lives in self.args and must NOT gate here: in client manual mode the chosen
        # resolution is delivered through this same resize path.
        if self._server_locked_dims() is not None:
            logger.warning(
                f"Client attempted to resize to {res} but server is in manual resolution mode. Request ignored."
            )
            return
        try:
            dims = parse_resize_dims(res)
            if dims is None:
                logger.error(f"Invalid resize request: {res}. Ignoring")
                if self.media_pipeline:
                    self.media_pipeline.last_resize_success = False
                return
            target_w, target_h = dims
            if getattr(self.args, "force_aligned_resolution", False):
                target_w, target_h = align_dims_16(target_w, target_h)

            # Idempotent: clients re-assert their resolution on reconnects and
            # settings broadcasts; re-applying the current size would churn
            # RandR (X11) or restart the capture (Wayland) for nothing. The
            # last request is honored too: when the realized size differs from
            # it (CVT cell alignment), the same re-asserted request must not
            # read as "not applied yet" forever.
            if (
                self.media_pipeline
                and self.media_pipeline.last_resize_success
                and (
                    (self.media_pipeline.width == target_w
                     and self.media_pipeline.height == target_h)
                    or self._last_resize_request == (target_w, target_h)
                )
            ):
                logger.debug(f"Resolution already {target_w}x{target_h}; skipping re-apply.")
                return

            if IS_WAYLAND:
                # No X server to resize: the compositor output follows the capture
                # dimensions, so update them and restart capture (websockets parity).
                self.media_pipeline.width = target_w
                self.media_pipeline.height = target_h
                # A capture already running takes the size through a restart;
                # the first request of a session can land while the pipeline
                # is still bringing audio up, so the capture itself is what is
                # checked, not the whole pipeline. A capture not yet started
                # reads the new size when it starts.
                if self.media_pipeline.is_screen_capturing():
                    await self.media_pipeline.restart_screen_capture()
                    # The compositor is the authority on what it realized (it
                    # may even-mask or refuse the mode): reconcile and tell the
                    # client the corrected size.
                    await self._push_wayland_realized_geometry("primary", self.media_pipeline)
                self.media_pipeline.last_resize_success = True
                self._last_resize_request = (target_w, target_h)
                logger.info(
                    f"Wayland capture resized to {self.media_pipeline.width}x{self.media_pipeline.height}"
                    f" (requested {target_w}x{target_h})"
                )
                return

            realized = await resize_display(f"{target_w}x{target_h}")
            if realized:
                realized_w, realized_h = realized
                if (realized_w, realized_h) != (target_w, target_h):
                    logger.info(
                        f"resize_display realized {realized_w}x{realized_h} for request {target_w}x{target_h}"
                    )
                else:
                    logger.info(f"resize_display('{target_w}x{target_h}') reported success")
                # Pull the capture onto the new root NOW: the auto-adjust poll
                # trails ~30 frames behind, during which a grown desktop's new
                # bands (panel, window bottoms) stay out of frame. A zero-size
                # region update re-reads the live root synchronously and keeps
                # root-follow (websockets keep-path parity).
                capture_module = getattr(self.media_pipeline, "capture_module", None)
                if capture_module is not None:
                    try:
                        await asyncio.to_thread(
                            capture_module.update_capture_region, 0, 0, 0, 0
                        )
                    except Exception as e:
                        logger.warning(f"Capture re-follow after resize failed: {e}")
                self.media_pipeline.width = realized_w
                self.media_pipeline.height = realized_h
                self.media_pipeline.last_resize_success = True
                self._last_resize_request = (target_w, target_h)
                if self.rtc_app is not None:
                    # Wayland-branch parity: X snapping (xrandr mode pick) can
                    # realize a different size than requested, and the client's
                    # manual-mode bookkeeping must follow the realized one.
                    self.rtc_app.send_remote_resolution(f"{realized_w}x{realized_h}", "primary")
            else:
                logger.error(
                    f"resize_display('{target_w}x{target_h}') reported failure"
                )
                self.media_pipeline.last_resize_success = False

        except Exception as e:
            logger.error(
                f"Error during resize handling for '{res}': {e}", exc_info=True
            )
            if self.media_pipeline:
                self.media_pipeline.last_resize_success = False

    async def request_idr_for_display(self, display_id: str = "primary") -> None:
        """Schedule a dynamic IDR frame on the display's encoder, throttled by a
        per-display floor (websockets REQUEST_KEYFRAME parity).

        Any number of viewers share one encoder, and an unthrottled
        data-channel request or PLI storm would let a single client force
        keyframe bursts for every consumer. A request landing inside the floor
        is satisfied by the IDR the previous request already scheduled.
        """
        display_id = display_id or "primary"
        now = time.monotonic()
        if now - self._last_idr_request_times.get(display_id, 0.0) < 0.25:
            return
        self._last_idr_request_times[display_id] = now
        pipeline = self.display_pipelines.get(display_id)
        if pipeline is not None:
            await pipeline.dynamic_idr_frame()

    async def _provision_webrtc_virtual_mic(self) -> None:
        """Bring up the SelkiesVirtualMic once for the WebRTC transport (shared
        provisioning with the websockets 0x02 path). Called from the per-peer mic
        playback start on the first mic packet; the lock + flag make concurrent
        first-packet calls across peers provision exactly once, and the shared
        helper reuses a source the websockets path already loaded rather than
        double-loading it."""
        if self._mic_provisioned:
            return
        async with self._mic_provision_lock:
            if self._mic_provisioned:
                return
            if self._mic_control is None:
                self._mic_control = AudioControl("selkies-webrtc-mic")
            audio_device_name = getattr(self.media_pipeline, "audio_device_name", None)
            is_capturing = bool(getattr(self.media_pipeline, "_is_pcmflux_capturing", False))
            self._mic_module_index, self._mic_module_owned = (
                await self._mic_control.ensure_virtual_microphone(audio_device_name, is_capturing)
            )
            self._mic_provisioned = self._mic_module_index is not None

    async def _teardown_webrtc_virtual_mic(self) -> None:
        """Unload the virtual-source module (only if this path loaded it) and
        release the mic control connection on shutdown."""
        control = self._mic_control
        self._mic_control = None
        if control is None:
            return
        if self._mic_module_index is not None and self._mic_module_owned:
            logger.info(f"Unloading WebRTC virtual mic module {self._mic_module_index}.")
            await control.unload_module(self._mic_module_index)
        self._mic_module_index = None
        self._mic_module_owned = False
        self._mic_provisioned = False
        await control.aclose()

    async def start_display_media(self, display_id: str) -> None:
        """A display's consumer connected: the primary starts its pipeline right
        away; a secondary waits for its dimensions (the client's first resize
        message), which trigger the layout pass that creates its pipeline."""
        if display_id == "primary" and self.media_pipeline:
            # A consumer reclaimed the primary: cancel a pending grace stop and
            # (re)start. start_media_pipeline is idempotent, so a controller tab
            # reload that reconnects inside the grace reuses the still-warm
            # capture with no black restart.
            self._cancel_primary_stop_grace()
            await self.media_pipeline.start_media_pipeline()
            # A Wayland start only enqueues a compositor command, so its real outcome
            # is read back here (the same barrier the secondaries use). A pipeline that
            # believes it is running with no live capture would leave the page waiting
            # on frames that never arrive, so it is stopped and logged truthfully; the
            # next consumer / reconnect retries. This also surfaces the host-death case
            # (reap_dead_host flips is_capturing to False).
            if (IS_WAYLAND and self.media_pipeline.is_media_pipeline_running()
                    and not await self._wayland_capture_live("primary", self.media_pipeline)):
                last_error = self._wayland_capture_last_error(self.media_pipeline, "primary")
                logger.error(
                    "Primary Wayland capture is not live after start"
                    + (f": {last_error}." if last_error else "."))
                await self.media_pipeline.stop_media_pipeline()
                return
            caveat = (self._wayland_capture_last_error(self.media_pipeline, "primary")
                      if IS_WAYLAND else None)
            if caveat:
                logger.warning(f"Primary Wayland capture started with a caveat: {caveat}")
            # The pipeline start is what establishes the host session in
            # host-capture mode, so the host's output count (and with it the
            # second-screen availability already announced on channel open) can
            # first become known here.
            if await self._refresh_second_screen_capacity() and self.rtc_app:
                self.rtc_app.send_media_data_over_channel(
                    "server_settings", self._server_settings_payload()
                )

    async def stop_display_media(self, display_id: str) -> None:
        """Release a display's pipeline: the primary's stop is deferred by a
        reconnect grace (a controller tab reload reconnects within a second or
        two and reuses the warm capture, and viewers/display2 keep streaming
        throughout — websockets _teardown_if_unclaimed parity); a secondary is
        fully unregistered and the desktop re-laid-out at once."""
        if display_id == "primary":
            self._schedule_primary_stop_grace()
            return
        async with self._display_lock:
            pipeline = self.display_pipelines.pop(display_id, None)
            self.display_clients.pop(display_id, None)
            self.display_layouts.pop(display_id, None)
            if pipeline is not None:
                await pipeline.stop_media_pipeline()
        await self.reconfigure_displays()

    def _cancel_primary_stop_grace(self) -> None:
        """Drop a pending deferred primary-capture stop: a consumer reclaimed
        the display before the grace elapsed."""
        task = self._primary_stop_grace_task
        self._primary_stop_grace_task = None
        if task is not None and not task.done():
            task.cancel()

    def _schedule_primary_stop_grace(self) -> None:
        """Stop the primary capture after RECONNECT_GRACE_S unless a consumer
        reclaims it first. A page reload drops and re-adds its peer within the
        window, so tearing the capture down immediately would black out a
        reconnecting controller (and stall the audio fan-out the viewers share)
        for no reason; if nobody reclaims the primary, the stop runs after the
        grace. At most one grace is pending at a time."""
        if self._primary_stop_grace_task is not None and not self._primary_stop_grace_task.done():
            return
        if self.media_pipeline is None or not self.media_pipeline.is_media_pipeline_running():
            return

        async def _stop_after_grace() -> None:
            try:
                await asyncio.sleep(self.RECONNECT_GRACE_S)
            except asyncio.CancelledError:
                return
            self._primary_stop_grace_task = None
            # A consumer that reconnected during the grace cancelled this task;
            # re-check under no lock is enough (the same event loop serializes
            # start/stop with this coroutine's resume).
            if self._primary_display_has_consumer():
                logger.info("Primary reclaimed within the grace; capture kept.")
                return
            if self.media_pipeline is not None:
                logger.info("Primary unclaimed after the grace; stopping its capture.")
                await self.media_pipeline.stop_media_pipeline()

        self._primary_stop_grace_task = asyncio.create_task(_stop_after_grace())

    def _primary_display_has_consumer(self) -> bool:
        """Whether any peer (controller or viewer) still consumes the primary."""
        if self.rtc_app is None:
            return False
        return any(
            (p.get("display_id") or "primary") == "primary"
            for p in self.rtc_app.peer_connections.values()
        )

    def _wayland_capture_handle(self) -> Optional[Any]:
        """A pixelflux handle for compositor output management (any ScreenCapture
        reaches the shared Wayland backend); prefers the primary pipeline's live
        capture module."""
        module = getattr(self.media_pipeline, "capture_module", None)
        if module is not None:
            return module
        if PixelfluxScreenCapture is None:
            return None
        if self._wayland_ctl_module is None:
            self._wayland_ctl_module = PixelfluxScreenCapture()
        return self._wayland_ctl_module

    async def _destroy_wayland_secondary_outputs(self, keep_oid: Optional[int] = None) -> None:
        """Retire every secondary compositor output except `keep_oid` (the
        primary, output 0, always persists)."""
        module = self._wayland_capture_handle()
        if module is None:
            return
        try:
            for out in await asyncio.to_thread(module.list_outputs):
                if out[0] != 0 and out[0] != keep_oid:
                    await asyncio.to_thread(module.destroy_output, out[0])
        except Exception as e:
            logger.warning(f"Wayland output teardown failed: {e}")

    async def _apply_wayland_extension(self, did: str, layouts: Dict[str, Dict[str, int]]) -> bool:
        """Realize the extended layout as compositor outputs, BEFORE the
        secondary's pipeline binds a capture — the Wayland counterpart of
        apply_extended_layout.

        The primary (output 0) MOVES to its layout offset ('left'/'up' place it
        off-origin); a secondary reposition is a destroy + recreate (its
        capture rebinds on the pipeline restart that follows), destroyed before
        the primary moves so the rectangles never overlap.

        Returns:
            False when the output cannot be created or the primary cannot move
            (the caller drops the display).
        """
        module = self._wayland_capture_handle()
        if module is None:
            return False
        oid = wayland_output_id(did)
        s = layouts[did]
        scale = float(getattr(self.media_pipeline, "scale", 1.0) or 1.0)
        try:
            outputs = {o[0]: o for o in await asyncio.to_thread(module.list_outputs)}
        except Exception as e:
            logger.error(f"Wayland list_outputs failed: {e}")
            outputs = {}
        await self._destroy_wayland_secondary_outputs(keep_oid=oid)
        existing = outputs.get(oid)
        if existing is not None and (existing[1], existing[2]) != (s["x"], s["y"]):
            logger.info(f"Wayland output {oid} moves to +{s['x']}+{s['y']}; recreating it.")
            await asyncio.to_thread(module.destroy_output, oid)
            existing = None
        p = layouts.get("primary") or {"x": 0, "y": 0}
        existing0 = outputs.get(0)
        current0 = (existing0[1], existing0[2]) if existing0 is not None else (0, 0)
        if (p["x"], p["y"]) != current0:
            if not await wayland_reposition_primary(module, p["x"], p["y"]):
                return False
        if existing is not None:
            return True
        try:
            created = bool(await asyncio.to_thread(
                module.create_output, oid, s["w"], s["h"], s["x"], s["y"], scale))
        except Exception as e:
            logger.error(f"Wayland create_output {oid} failed: {e}")
            return False
        if created and self.input_handler:
            # Which of the session's own screens a capture drives just changed.
            self.input_handler.resync_session_screens()
        return created

    async def _wayland_capture_live(self, did: str, pipeline: MediaPipelinePixel) -> bool:
        """Whether the display's capture really runs in the compositor. The
        geometry read is a barrier: it is answered only after the queued capture
        start finished, so is_capturing is authoritative afterwards."""
        module = getattr(pipeline, "capture_module", None)
        if module is None:
            return False
        try:
            await asyncio.to_thread(module.get_realized_geometry, wayland_output_id(did))
            return bool(module.is_capturing)
        except Exception:
            return False

    def _wayland_capture_last_error(
        self, pipeline: Optional[MediaPipelinePixel], did: str
    ) -> Optional[str]:
        """The reason a display's Wayland capture failed, or a caveat a live one came up
        with (encoder fell back to CPU, host connect refused), or None. Read straight from
        ``capture_state`` (no barrier); the caller ensures ordering. None on an older
        pixelflux without the readback."""
        module = getattr(pipeline, "capture_module", None) if pipeline is not None else None
        getter = getattr(module, "capture_state", None) if module is not None else None
        if getter is None:
            return None
        try:
            _state, last_error = getter(wayland_output_id(did))
            return last_error
        except Exception:
            return None

    async def _realized_wayland_dims(self, did: str) -> Optional[Tuple[int, int]]:
        """The ``(width, height)`` the compositor currently has for this
        display's output, or None when it cannot be read."""
        if not IS_WAYLAND:
            return None
        pipeline = (self.media_pipeline if did == "primary"
                    else self.display_pipelines.get(did))
        module = getattr(pipeline, "capture_module", None)
        if module is None or not hasattr(module, "get_realized_geometry"):
            return None
        try:
            geom = await asyncio.to_thread(
                module.get_realized_geometry, wayland_output_id(did))
        except Exception as e:
            logger.warning(f"Wayland realized-geometry read failed for '{did}': {e}")
            return None
        if geom is None:
            logger.warning(f"Wayland realized-geometry read for '{did}' timed out; size unknown.")
            return None
        w, h, _scale = geom
        return (w, h) if w > 0 and h > 0 else None

    async def _push_wayland_realized_geometry(
        self, did: str, pipeline: Optional[MediaPipelinePixel]
    ) -> None:
        """Read what the pixelflux compositor actually realized on this
        display's output after a capture (re)start (it may even-mask dimensions
        or keep the old mode on a GBM allocation failure), fold it into the
        pipeline/layout state the input math offsets against, and push the
        corrected size to the clients over the existing system resolution
        message — the WR counterpart of the WS realized clamp + broadcast. The
        stream itself re-negotiates through the encoder (the track's intrinsic
        size IS the realized resolution); this closes the control-plane loop.
        The read is also a barrier: the compositor answers only after the
        queued capture (re)start finished."""
        if not IS_WAYLAND or pipeline is None:
            return
        module = getattr(pipeline, "capture_module", None)
        if module is None or not hasattr(module, "get_realized_geometry"):
            return
        try:
            geom = await asyncio.to_thread(
                module.get_realized_geometry, wayland_output_id(did))
        except Exception as e:
            logger.warning(f"Wayland realized-geometry read failed for '{did}': {e}")
            return
        if geom is None:
            # A timeout is unknown geometry, not "nothing to reconcile": leave the
            # prior state and pushed size untouched.
            logger.warning(f"Wayland realized-geometry read for '{did}' timed out; state left unreconciled.")
            return
        w, h, scale = geom
        if w <= 0 or h <= 0:
            return
        pipeline.width, pipeline.height = w, h
        if did == "primary":
            if self._primary_dims is not None:
                self._primary_dims = (w, h)
        else:
            entry = self.display_clients.get(did)
            if entry is not None:
                entry["width"], entry["height"] = w, h
        layout = self.display_layouts.get(did)
        if layout is not None:
            layout["w"], layout["h"] = w, h
        logger.info(f"Wayland realized geometry for '{did}': {w}x{h} @ scale {scale}")
        if self.rtc_app is not None:
            # Unconditional (idempotent, WS-broadcast parity): the client's own
            # request may have been snapped by sanitization before the pipeline
            # ever saw it, so "unchanged here" does not mean "what was asked".
            # Scoped to this display's channels so a secondary's realized size
            # never rescales the primary page.
            self.rtc_app.send_remote_resolution(f"{w}x{h}", did)

    async def _apply_wayland_cursor_size(self, dpi_value: float) -> None:
        """Wayland counterpart of the X11 per-DPI cursor resize: the compositor
        reloads its theme cursor (composited overlay and named-cursor delivery
        both re-render) at the DPI-scaled size, live, no capture restart."""
        if CURSOR_SIZE is None:
            return
        module = self._wayland_capture_handle()
        setter = getattr(module, "set_cursor_size", None) if module else None
        if setter is None:
            logger.warning("Wayland cursor resize unavailable (no set_cursor_size).")
            return
        size = cursor_size_for_dpi(dpi_value, CURSOR_SIZE)
        try:
            if await asyncio.to_thread(setter, size):
                logger.info(f"Wayland cursor size set to {size} (DPI {dpi_value}).")
            else:
                logger.warning(f"Wayland compositor refused cursor size {size}.")
        except Exception as e:
            logger.warning(f"Wayland cursor resize failed: {e}")

    def _display_consumers(self, display_id: str) -> List[Dict[str, Any]]:
        """Registered peers (controller + viewers) consuming this display's
        stream."""
        if self.rtc_app is None:
            return []
        return [
            p for p in self.rtc_app.peer_connections.values()
            if (p.get("display_id") or "primary") == display_id
        ]

    async def handle_peer_gone(
        self, peer_id: str, peer: Optional[Dict[str, Any]] = None
    ) -> None:
        """Release the input a departing peer may still be holding, matching the
        websockets disconnect cleanup. Gamepad slots are per-connection, so they
        are always released. Held keys and pointer buttons are one global desktop
        state instead: they are force-released only when the departing peer could
        drive input and no input-capable peer is left, so a viewer (or a second
        display's peer) leaving never drops the controller's held keys or its
        in-flight drag. A controller that vanishes while others remain is covered
        by the input handler's heartbeat stale-sweep."""
        if self.input_handler is None:
            return
        try:
            await self.input_handler.release_gamepads_for_conn(peer_id)
        except Exception as e:
            logger.warning(f"Gamepad release for departed peer {peer_id} failed: {e}")
        if self.rtc_app is None or not self.rtc_app.peer_holds_input_authority(peer):
            return
        if (peer.get("display_id") or "primary") != "primary":
            # The departing peer is already out of peer_connections at every call
            # site, so this walks the survivors only.
            for survivor in list(self.rtc_app.peer_connections.values()):
                if self.rtc_app.peer_holds_input_authority(survivor):
                    return
        for release in (self.input_handler.release_mouse_buttons,
                        self.input_handler.reset_keyboard):
            try:
                await release()
            except Exception as e:
                logger.warning(f"Input release for departed peer {peer_id} failed: {e}")

    async def handle_video_consumer_active(self, peer_id: str, display_id: str, active: bool) -> None:
        """Tab-visibility pause/resume for ONE peer (data-channel STOP_VIDEO /
        START_VIDEO, websockets parity). The peer's own RTP sender gates its
        delivery; the shared capture only stops once EVERY consumer of the
        display (controller and viewers alike) is paused, and restarts with an
        IDR on the first resume so decode resyncs immediately (PLI stays the
        fallback)."""
        display_id = display_id or "primary"
        peer = self.rtc_app.peer_connections.get(peer_id) if self.rtc_app else None
        if peer is None:
            return
        peer["video_paused"] = not active
        sender = peer.get("video_sender")
        if sender is not None:
            # Per-peer gate: a disabled sender keeps draining its relay proxy
            # (nothing accumulates) but sends no RTP, so THIS peer's
            # bytesReceived stalls while other consumers stream on.
            sender._enabled = active
        pipeline = self.display_pipelines.get(display_id)
        if pipeline is None:
            return
        if active:
            # IDR unconditionally: the resuming peer's decoder needs a resync
            # even when the capture kept running for other consumers.
            await self._resume_display_capture(display_id, pipeline,
                                               "consumer resume", idr_always=True)
        elif all(p.get("video_paused", False) for p in self._display_consumers(display_id)):
            if await pipeline.pause_screen_capture():
                logger.info(
                    f"All consumers of display '{display_id}' are paused; capture stopped."
                )

    async def _close_peer_signaling_ws(self, peer_id: str, code: int, message: bytes) -> None:
        """Fatal verdict on ONE peer's signaling socket (websockets KILL parity);
        bounded so a wedged socket cannot stall the caller."""
        if self.peer_manager is None:
            return
        async with self.peer_manager.lock:
            peer = self.peer_manager.peers.get(peer_id)
            peer_ws = getattr(peer, "ws", None) if peer is not None else None
        if peer_ws is not None and not peer_ws.closed:
            try:
                await asyncio.wait_for(peer_ws.close(code=code, message=message), timeout=2.0)
            except Exception:
                pass

    async def reconcile_webrtc_peers(self) -> None:
        """Token-update reconciliation for LIVE WebRTC peers (websockets
        reconcile_clients parity): a revoked or role-changed token closes the
        peer (signaling verdict 4002 + pipeline stop); an mk-token handoff
        pushes the new input verdict to every surviving peer over its data
        channel, controllers included (a handoff strips their authority too).
        Per-message input authority already reads the live store — this covers
        the media stream and the client-side grant, which otherwise persist
        until the peer disconnects itself."""
        if self.rtc_app is None:
            return
        tokens, mk = current_session_tokens()
        for peer_id, peer in list(self.rtc_app.peer_connections.items()):
            token = peer.get("client_token")
            if not token:
                # Legacy (token-less) peer: governed by its URL role only.
                continue
            ctype = peer.get("client_type")
            role_now = "controller" if ctype == ClientType.CONTROLLER else "viewer"
            new_perms = tokens.get(token)
            if not new_perms or (new_perms.get("role") or "controller") != role_now:
                reason = "Token revoked" if not new_perms else "Permissions changed significantly"
                logger.info(f"Disconnecting WebRTC peer {peer_id}: {reason}")
                await self._close_peer_signaling_ws(peer_id, 4002, reason.encode())
                try:
                    await self.rtc_app.stop_rtc_connection(peer_id, role_now)
                except Exception:
                    logger.warning(f"stop_rtc_connection failed for {peer_id}", exc_info=True)
                continue
            self.rtc_app._send_collab_state(peer.get("data_channel"), ctype, token)
            # A slot-only change keeps the peer but must reach its client
            # (websockets ROLE_UPDATE parity): the gamepad slot mapping lives
            # client-side and would silently desync otherwise.
            new_slot = new_perms.get("slot")
            if new_slot != peer.get("client_slot"):
                peer["client_slot"] = new_slot
                channel = peer.get("data_channel")
                if channel is not None and channel.readyState == "open":
                    try:
                        verdict = json.dumps({"role": role_now, "slot": new_slot})
                        channel.send(json.dumps(
                            {"type": "system", "data": {"action": f"role_update,{verdict}"}}))
                    except Exception:
                        logger.debug("role_update send failed (channel closing)", exc_info=True)

    async def handle_consumers_changed(self, display_id: str) -> None:
        """A peer joined or left a display's consumer set: re-evaluate the
        all-paused stop in both directions — a departing unpaused peer cannot
        leave the capture running for hidden-only consumers, and a JOINING
        unpaused peer must re-open a capture the rule stopped (else a viewer
        arriving while every prior consumer is hidden gets a permanently black
        stream: a paused capture emits no RTP, so the browser never even PLIs).
        (A departing controller's pipeline is torn down elsewhere; both
        pause_screen_capture and resume_screen_capture no-op on a stopped
        pipeline.)"""
        display_id = display_id or "primary"
        pipeline = self.display_pipelines.get(display_id)
        if pipeline is None:
            return
        consumers = self._display_consumers(display_id)
        if not consumers:
            return
        if all(p.get("video_paused", False) for p in consumers):
            if await pipeline.pause_screen_capture():
                logger.info(
                    f"All remaining consumers of display '{display_id}' are paused; capture stopped."
                )
        else:
            # IDR only on an actual restart: with the capture already live, RTP
            # flows and the joining browser's own PLI covers its resync.
            await self._resume_display_capture(display_id, pipeline, "joining consumer")

    async def _resume_display_capture(self, display_id: str, pipeline: MediaPipelinePixel,
                                      why: str, idr_always: bool = False) -> None:
        """Resume a capture stopped by the all-consumers-paused rule (no-op on a
        live or fully-stopped pipeline) and request the resync IDR — always, or
        only when the resume actually restarted the capture."""
        restarted = False
        try:
            restarted = await pipeline.resume_screen_capture()
            if restarted:
                logger.info(f"Display '{display_id}': capture restarted ({why}).")
        except Exception as e:
            logger.error(f"Display '{display_id}': capture resume failed ({why}): {e}")
        if idr_always or restarted:
            await self.request_idr_for_display(display_id)

    async def _drop_wayland_secondary(self, did: str, reason: str) -> None:
        """Refuse a secondary display the compositor cannot realize: unregister
        it, stop its pipeline, destroy its output, and close its peers with a
        fatal signaling verdict (4000) so the client does not re-register in a
        loop — the Wayland mirror of the X11 unrealizable-extension drop.
        Caller holds _display_lock."""
        pipeline = self.display_pipelines.pop(did, None)
        self.display_clients.pop(did, None)
        self.display_layouts.pop(did, None)
        primary_layout = self.display_layouts.get("primary")
        if primary_layout:
            # Input offsets follow the layout; the primary is re-anchored at the
            # origin below.
            primary_layout["x"], primary_layout["y"] = 0, 0
        if pipeline is not None:
            await pipeline.stop_media_pipeline()
        module = self._wayland_capture_handle()
        if module is not None:
            try:
                await asyncio.to_thread(module.destroy_output, wayland_output_id(did))
            except Exception:
                pass
            # The primary may sit at a 'left'/'up' offset for the arrangement
            # this display anchored; put it back at the origin.
            await wayland_reposition_primary(module, 0, 0)
        if self.peer_manager is not None:
            async with self.peer_manager.lock:
                doomed = [
                    p.ws for p in self.peer_manager.peers.values()
                    if p.peer_type != "server"
                    and getattr(p, "display_id", "primary") == did
                    and getattr(p, "ws", None) is not None and not p.ws.closed
                ]
            for peer_ws in doomed:
                try:
                    await asyncio.wait_for(
                        peer_ws.close(code=4000, message=reason.encode("utf-8")),
                        timeout=2.0,
                    )
                except Exception:
                    pass
        if self.rtc_app is not None:
            await self.rtc_app.close_display_peers(did)
        logger.error(f"Secondary display '{did}' dropped on Wayland: {reason}")

    async def _drop_x11_secondary(self, did: str, reason: str) -> None:
        """Refuse a secondary display the X server cannot fit in its root:
        unregister it, stop its pipeline, and close its peers with a fatal
        signaling verdict (4000) so the client does not reload and re-register
        in a loop — the X11 mirror of the compositor-side drop, and the
        websockets engine's KILL parity. Caller holds _display_lock.

        Unregistering inline rather than through stop_display_media, which
        would re-acquire the lock.
        """
        pipeline = self.display_pipelines.pop(did, None)
        self.display_clients.pop(did, None)
        self.display_layouts.pop(did, None)
        primary_layout = self.display_layouts.get("primary")
        if primary_layout:
            # Input offsets follow the layout: with the arrangement void, the
            # primary is back at the origin.
            primary_layout["x"], primary_layout["y"] = 0, 0
        if pipeline is not None:
            await pipeline.stop_media_pipeline()
        if self.peer_manager is not None:
            async with self.peer_manager.lock:
                doomed = [
                    p.ws for p in self.peer_manager.peers.values()
                    if p.peer_type != "server"
                    and getattr(p, "display_id", "primary") == did
                    and getattr(p, "ws", None) is not None and not p.ws.closed
                ]
            for peer_ws in doomed:
                try:
                    await asyncio.wait_for(
                        peer_ws.close(code=4000, message=reason.encode("utf-8")),
                        timeout=2.0,
                    )
                except Exception:
                    pass
        if self.rtc_app is not None:
            await self.rtc_app.close_display_peers(did)
        logger.error(
            f"Extended layout for '{did}' is unrealizable; the secondary display stays "
            f"disabled. {reason}"
        )

    async def reconfigure_displays(self) -> None:
        """Lay the extended desktop out for the connected displays and point each
        display's capture at its region — the WR counterpart of the websockets
        reconfigure engine, for the primary plus one secondary display. On
        Wayland the layout realizes as compositor outputs instead of xrandr
        monitors."""
        async with self._display_lock:
            secondary = next(
                ((did, info) for did, info in self.display_clients.items()
                 if did != "primary" and info.get("width", 0) > 0 and info.get("height", 0) > 0),
                None,
            )
            if secondary is None:
                # Back to a single display: restore the plain full-screen capture.
                if self.display_layouts:
                    self.display_layouts = {}
                    p_w, p_h = self._primary_dims or (self.media_pipeline.width, self.media_pipeline.height)
                    # The layout no longer diverts the primary's resolution; the
                    # pipeline dimensions below become its authority again, and a
                    # later secondary re-derives the layout from them.
                    self._primary_dims = None
                    if IS_WAYLAND:
                        # The secondary's compositor output goes away (its windows
                        # relocate to the primary) and the primary re-anchors at
                        # the origin (it sat at an offset in a 'left'/'up'
                        # arrangement); no framebuffer to shrink.
                        await self._destroy_wayland_secondary_outputs()
                        await wayland_reposition_primary(self._wayland_capture_handle(), 0, 0)
                        self.media_pipeline.capture_region = None
                        if (self.media_pipeline.width, self.media_pipeline.height) != (p_w, p_h):
                            self.media_pipeline.width, self.media_pipeline.height = p_w, p_h
                            if self.media_pipeline.is_media_pipeline_running():
                                await self.media_pipeline.restart_screen_capture()
                        self._broadcast_display_config()
                        return
                    # Remove the stale selkies-* logical monitors before shrinking the
                    # framebuffer, so a secondary region does not linger outside it (WS parity).
                    await clear_selkies_monitors()
                    realized = await resize_display(f"{p_w}x{p_h}")
                    if realized:
                        p_w, p_h = realized
                    self.media_pipeline.capture_region = None
                    self.media_pipeline.width, self.media_pipeline.height = p_w, p_h
                    if self.media_pipeline.is_media_pipeline_running():
                        await self.media_pipeline.restart_screen_capture()
                    self._push_x11_layout_geometry({"primary": {"w": p_w, "h": p_h}})
                elif self._primary_dims is not None:
                    # No laid-out secondary (one is registered but has no
                    # dimensions yet, or its layout was unrealizable), so there is
                    # nothing to compute: the primary's diverted resolution
                    # request goes straight to the real display.
                    await self._resize_primary_display(
                        "{}x{}".format(*self._primary_dims)
                    )
                self._broadcast_display_config()
                return
            did, info = secondary
            await self._wm_swap.ensure_for(len(self.display_clients), IS_WAYLAND)
            if self._primary_dims is None:
                # The primary never resized through the layout path: take its
                # pipeline dimensions (kept current by the single-display path),
                # falling back to the live screen resolution.
                p_w, p_h = self.media_pipeline.width, self.media_pipeline.height
                if IS_WAYLAND:
                    # The compositor lays the secondary out against its own output
                    # rects and rejects an overlap, so its realized geometry -- not
                    # the capture size, which may still trail a resize -- is what
                    # the offset has to be computed from.
                    realized = await self._realized_wayland_dims("primary")
                    if realized is not None:
                        p_w, p_h = realized
                if p_w <= 0 or p_h <= 0:
                    if IS_WAYLAND:
                        # No X server to ask: the pipeline dimensions are the only
                        # authority on Wayland.
                        logger.error("Cannot determine primary display size; aborting layout.")
                        return
                    curr, _, _, _, _ = await get_new_res("1x1")
                    try:
                        p_w, p_h = (int(v) for v in curr.lower().split("x"))
                    except (ValueError, AttributeError):
                        logger.error("Cannot determine primary display size; aborting layout.")
                        return
                self._primary_dims = (p_w, p_h)
            position = info.get("position", "right")
            # Auto-resize feedback guard, shared with the websockets layout engine.
            self._primary_dims = clamp_primary_feedback(
                self._primary_dims, self.display_layouts, position
            )
            layouts, total_w, total_h = compute_dual_layout(
                self._primary_dims, (info["width"], info["height"]), position,
            )
            layouts[did] = layouts.pop("secondary")
            if IS_WAYLAND:
                if not await self._apply_wayland_extension(did, layouts):
                    await self._drop_wayland_secondary(
                        did, "The compositor cannot create an output for this display."
                    )
                    return
            else:
                # An input channel left connected would keep feeding a display
                # that has no laid-out region, so a layout the server cannot
                # realize takes the secondary down with it. The helper fits the
                # layout to the root that was really produced: the displays it
                # keeps may come back smaller than the request, and one it
                # cannot place at all disappears from `layouts`.
                requested = {d: (r["w"], r["h"]) for d, r in layouts.items()}
                if (not await apply_extended_layout(layouts, total_w, total_h)
                        or did not in layouts):
                    await self._drop_x11_secondary(
                        did, "The X server cannot extend the desktop to fit this display."
                    )
                    return
                # Only what the server refused is written back. The roster feeds
                # the resolution reports and _primary_dims feeds the next
                # layout, so a display the root cut down has to be recorded at
                # the size it really got — while a rectangle the layout itself
                # derived must not be, or each pass would recompute the layout
                # from its own previous output.
                for fitted_id, fitted in layouts.items():
                    if (fitted["w"], fitted["h"]) == requested[fitted_id]:
                        continue
                    client = self.display_clients.get(fitted_id)
                    if client is not None:
                        client["width"], client["height"] = fitted["w"], fitted["h"]
                    if fitted_id == "primary":
                        self._primary_dims = (fitted["w"], fitted["h"])
            self.display_layouts = layouts
            p = layouts["primary"]
            if IS_WAYLAND:
                # The apply helper already moved the primary output to its
                # layout offset (live, no restart) and the output resizes
                # through its capture (re)start; skip the churn when the size
                # is unchanged.
                if (self.media_pipeline.width, self.media_pipeline.height) != (p["w"], p["h"]):
                    await self.media_pipeline.update_capture_region(p["x"], p["y"], p["w"], p["h"])
                    await self._push_wayland_realized_geometry("primary", self.media_pipeline)
            else:
                await self.media_pipeline.update_capture_region(p["x"], p["y"], p["w"], p["h"])
            s = layouts[did]
            pipeline = self.display_pipelines.get(did)
            if pipeline is None:
                # A display's pipeline is built from ITS settings (the display's
                # SETTINGS arrive before its first resize lays it out), falling
                # back per key to the service defaults.
                setting = lambda key: self._display_setting(did, key)
                pipeline = MediaPipelinePixel(
                    async_event_loop=asyncio.get_running_loop(),
                    encoder=str(setting("encoder")),
                    framerate=int(setting("framerate")),
                    video_bitrate=int(setting("video_bitrate")),
                    audio_enabled=False,
                    width=s["w"],
                    height=s["h"],
                    crf=int(setting("video_crf")),
                    video_fullcolor=bool(setting("video_fullcolor")),
                    use_cpu=bool(setting("use_cpu")),
                    video_streaming_mode=bool(setting("video_streaming_mode")),
                    use_paint_over_quality=bool(setting("use_paint_over_quality")),
                    video_paintover_crf=int(setting("video_paintover_crf")),
                    video_paintover_burst_frames=int(setting("video_paintover_burst_frames")),
                    display_id=did,
                    capture_region=(s["x"], s["y"]),
                )
                if self.args.enable_rate_control:
                    pipeline.rc_mode = RateControlMode(setting("rate_control_mode"))
                else:
                    pipeline.rc_mode = self.media_pipeline.rc_mode
                # Compositor output scale follows the session DPI (Wayland only;
                # a no-op field on X11). The ladder runs for this display's own
                # screen rather than copying whatever the primary was left with.
                if IS_WAYLAND and self.input_handler is not None:
                    pipeline.scale = await self.input_handler.realize_wayland_dpi(
                        getattr(self, "_last_applied_dpi", None)
                        or getattr(settings, "scaling_dpi", 96) or 96,
                        session_screen_index(did), (s["w"], s["h"]))
                else:
                    pipeline.scale = getattr(self.media_pipeline, "scale", 1.0)
                # The native-cursor toggle is global across displays: a secondary
                # joining after the toggle starts with the primary's current state.
                pipeline.capture_cursor = self.media_pipeline.capture_cursor
                pipeline.produce_data = (
                    lambda buf, pts, kind, _did=did: self.rtc_app.consume_data(buf, pts, kind, _did)
                )
                # pixelflux's cursor-callback slot is process-global (the last
                # registration wins), so a secondary's capture start must route
                # cursor events into the same transport sink as the primary.
                pipeline.on_cursor_data = self.media_pipeline.on_cursor_data
                pipeline.get_cursor_size_cap = self.media_pipeline.get_cursor_size_cap
                self.display_pipelines[did] = pipeline
                try:
                    await pipeline.start_media_pipeline()
                except Exception as e:
                    # Leave no half-built pipeline behind: drop it so the next resize /
                    # reconfigure retries the bring-up instead of finding a dead pipeline
                    # and only re-targeting it (bring-up recovery, WS-style fallback).
                    logger.error(f"Secondary display '{did}' pipeline failed to start ({e}); will retry on next reconfigure.")
                    self.display_pipelines.pop(did, None)
                    if IS_WAYLAND:
                        # Leave no orphan output behind a failed bring-up; the
                        # client gets the fatal verdict instead of a silent stall.
                        await self._drop_wayland_secondary(
                            did, "The compositor could not start a capture for this display."
                        )
                    return
                if IS_WAYLAND and not await self._wayland_capture_live(did, pipeline):
                    last_error = self._wayland_capture_last_error(pipeline, did)
                    await self._drop_wayland_secondary(
                        did,
                        last_error or "The compositor could not start a capture for this "
                        "display (encoder session or GPU resources exhausted).",
                    )
                    return
                if IS_WAYLAND:
                    await self._push_wayland_realized_geometry(did, pipeline)
                logger.info(f"Secondary display '{did}' pipeline started at {s}")
            else:
                # A Wayland restart is a full capture reconfigure: skip it when
                # the region is unchanged and the capture is verifiably live.
                unchanged = IS_WAYLAND and (
                    (pipeline.width, pipeline.height) == (s["w"], s["h"])
                    and pipeline.capture_region == (s["x"], s["y"])
                    and await self._wayland_capture_live(did, pipeline)
                )
                if not unchanged:
                    await pipeline.update_capture_region(s["x"], s["y"], s["w"], s["h"])
                    if IS_WAYLAND:
                        await self._push_wayland_realized_geometry(did, pipeline)
            if not IS_WAYLAND:
                self._push_x11_layout_geometry(layouts)
        self._broadcast_display_config()

    def _push_x11_layout_geometry(self, layouts: Dict[str, Dict[str, int]]) -> None:
        """Tell each laid-out display's pages the size the X11 layout pass gave
        it (websockets parity: the engine broadcasts every display's realized
        resolution after each pass). The root may not fit the request, and a
        page whose display the server cut down would otherwise keep its
        requested size in its manual-mode bookkeeping and re-assert it
        forever; idempotent on the client for an unchanged size. The Wayland
        branches push through the compositor's realized geometry instead."""
        if self.rtc_app is None:
            return
        for did, rect in layouts.items():
            w, h = int(rect.get("w", 0)), int(rect.get("h", 0))
            if w > 0 and h > 0:
                self.rtc_app.send_remote_resolution(f"{w}x{h}", did)

    def _broadcast_display_config(self) -> None:
        """Tell every connected page which displays are attached (websockets
        parity: the primary page forces browser-cursor rendering while a
        secondary is connected, keyed off this broadcast)."""
        if not self.rtc_app:
            return
        displays = ["primary"] + [d for d in self.display_clients.keys() if d != "primary"]
        self.rtc_app.send_media_data_over_channel(
            "display_config_update", {"displays": displays}
        )

    def _update_cursor_cap(self, dpi_value: float) -> None:
        """Scale the remote-cursor delivery cap with the DPI and push it to
        every running capture (pixelflux applies cursor_size_cap live through
        update_tunables; later (re)starts read it through CaptureSettings)."""
        ih = self.input_handler
        if ih is None:
            return
        try:
            ih.system_dpi = float(dpi_value)
            ih.cursor_size_cap = int(ih.max_cursor_size * float(dpi_value) / 96.0)
        except Exception as e:
            logger.debug(f"cursor cap update skipped: {e}")
            return
        updated = 0
        for did, pipeline in list(self.display_pipelines.items()):
            module = getattr(pipeline, "capture_module", None)
            if pipeline is None or module is None or not pipeline.is_media_pipeline_running():
                continue
            try:
                module.update_tunables(pipeline.generate_capture_settings())
                updated += 1
            except Exception as e:
                logger.debug(f"Live cursor cap update skipped for '{did}': {e}")
        logger.info(f"Cursor size cap {ih.cursor_size_cap}px for DPI {dpi_value} "
                    f"({updated} live capture(s) updated).")

    async def handle_scaling(self, dpi_value: float) -> None:
        """Apply a client DPI sync to the desktop (X11 xrdb/cursor themes) or
        run the per-display Wayland scale ladder.

        Fractional DPI is legal on the shared verb (websockets parity); the
        desktop DPI property itself is integral. Bounded by the declared
        scaling_dpi span so a client cannot drive xrdb — and, with it, the
        cursor size and the Wayland compositor scale — to an arbitrary value.
        """
        try:
            dpi_value = min(SCALING_DPI_MAX,
                            max(SCALING_DPI_MIN, int(round(float(dpi_value)))))
        except (TypeError, ValueError, OverflowError):
            logger.warning(f"Ignoring malformed DPI sync: {dpi_value!r}")
            return
        if settings._overridden.get("scaling_dpi", False):
            # An operator-set DPI (CLI/env) governs the desktop: client DPI
            # syncs must not clobber it.
            logger.info("Ignoring client DPI sync: scaling_dpi is operator-overridden.")
            return
        # Idempotent: the dashboard and the core each re-assert their DPI on
        # settings broadcasts, and every apply churns xrdb + xsettingsd SIGHUP
        # + cursor themes. Re-applying the current value is a no-op.
        if getattr(self, "_last_applied_dpi", None) == int(dpi_value):
            logger.debug(f"DPI already {int(dpi_value)}; skipping re-apply.")
            return
        if not IS_WAYLAND:
            if await set_dpi(int(dpi_value)):
                self._last_applied_dpi = int(dpi_value)
                logger.info(f"Successfully set DPI to {dpi_value}")
            else:
                logger.error(f"Failed to set DPI to {dpi_value}")

        # Remote-cursor delivery cap follows the DPI on both backends (WS
        # parity): running captures take it live through the tunables path, and
        # the Wayland restarts below read it through CaptureSettings, so it is
        # set first — a session compositor that absorbs the scale restarts nothing.
        self._update_cursor_cap(dpi_value)

        # On Wayland a DPI runs the scale ladder per display: the session
        # compositor scales the screen backing it, and only what it leaves
        # becomes that display's capture scale, whose change restarts the
        # capture — the WS path threads the same scale through CaptureSettings.
        if IS_WAYLAND:
            self._last_applied_dpi = int(dpi_value)
            for did, pipeline in list(self.display_pipelines.items()):
                if pipeline is None:
                    continue
                new_scale = (await self.input_handler.realize_wayland_dpi(
                    dpi_value, session_screen_index(did),
                    (pipeline.width, pipeline.height))
                    if self.input_handler else float(dpi_value) / 96.0)
                if pipeline.scale == new_scale:
                    continue
                pipeline.scale = new_scale
                if pipeline.is_media_pipeline_running():
                    await pipeline.restart_screen_capture()
                    await self._push_wayland_realized_geometry(did, pipeline)
            await self._apply_wayland_cursor_size(dpi_value)
            return

        if CURSOR_SIZE is None:
            # Auto (platform default): only the DPI itself is applied.
            return

        new_cursor_size = cursor_size_for_dpi(dpi_value, CURSOR_SIZE)

        logger.info(
            f"Attempting to set cursor size to: {new_cursor_size} (based on DPI {dpi_value})"
        )
        if await set_cursor_size(new_cursor_size):
            logger.info(f"Successfully set cursor size to {new_cursor_size}")
        else:
            logger.error(f"Failed to set cursor size to {new_cursor_size}")

    async def handle_system_monitor(self, t: float) -> None:
        """System-monitor tick: push CPU/memory stats and a ping to clients,
        and recover the audio capture if its worker died since the last tick."""
        if self.input_handler and self.rtc_app and self.system_monitor:
            self.input_handler.ping_start = t
            self.rtc_app.send_system_stats(
                self.system_monitor.cpu_percent,
                self.system_monitor.mem_total,
                self.system_monitor.mem_used,
            )
            self.rtc_app.send_ping(t)
        # pcmflux reports a clean start while its worker is still coming up, so a
        # device that dies during bring-up (or later) only shows through
        # last_error: poll it here and cycle the audio capture (audio lives on
        # the primary pipeline; the call no-ops on the audio-less secondaries).
        if self.media_pipeline is not None:
            try:
                await self.media_pipeline.recover_audio_if_failed()
            except Exception as e:
                logger.debug(f"audio health poll skipped: {e}")

    async def handle_gpu_stats(
        self, load: float, memory_total: int, memory_used: int
    ) -> None:
        """GPU-monitor tick: push GPU stats to clients and into metrics."""
        if self.rtc_app:
            self.rtc_app.send_gpu_stats(load, memory_total, memory_used)
        if self.metrics:
            self.metrics.set_gpu_utilization(load * 100)

    # Live per-pipeline setters for the client-tunable video settings. Each
    # display's pipeline owns its running values; these route one sanitized
    # value into one pipeline.
    _VIDEO_SETTING_APPLIERS: Dict[str, Callable[[MediaPipelinePixel, Any], Awaitable[Any]]] = {
        "rate_control_mode": lambda p, v: p.update_rate_control_mode(RateControlMode(v)),
        "video_crf": lambda p, v: p.set_crf(v),
        "video_bitrate": lambda p, v: p.set_video_bitrate(v),
        "framerate": lambda p, v: p.set_framerate(v),
        "use_cpu": lambda p, v: p.set_use_cpu(bool(v)),
        "encoder": lambda p, v: p.set_encoder(str(v)),
        "video_fullcolor": lambda p, v: p.set_video_fullcolor(bool(v)),
        "video_streaming_mode": lambda p, v: p.set_video_streaming_mode(bool(v)),
        "use_paint_over_quality": lambda p, v: p.set_use_paint_over_quality(bool(v)),
        "video_paintover_crf": lambda p, v: p.set_video_paintover_crf(int(v)),
        "video_paintover_burst_frames": lambda p, v: p.set_video_paintover_burst_frames(int(v)),
    }

    def _seed_display_settings(self, entry: Dict[str, Any]) -> None:
        """Give a joining secondary display its own copy of every client-tunable
        video setting, taken from the args at join time (websockets parity: a
        display registers with a full seeded snapshot). Sharing the args instead
        would make a later primary change move what the secondary reports as its
        current value while its stream keeps running the old one, and the equality
        guards on its own controls would then compare against a value that display
        never ran."""
        for key in list(self._VIDEO_SETTING_APPLIERS) + ["force_aligned_resolution"]:
            if key in entry:
                continue
            value = getattr(self.args, key, None)
            if value is not None:
                entry[key] = value

    def _display_setting(self, display_id: str, key: str) -> Any:
        """A display's current value for a client-tunable setting: the primary
        reads the service args; a secondary reads its own stored overrides,
        falling back to the args it was seeded from (websockets model: each
        display's SETTINGS payload configures only that display's stream)."""
        if display_id != "primary":
            entry = self.display_clients.get(display_id)
            if entry is not None and key in entry:
                return entry[key]
        return getattr(self.args, key, None)

    def _store_display_setting(self, display_id: str, key: str, value: Any) -> None:
        """Record a setting as the display's current value (args for the
        primary, the display's own entry for a secondary)."""
        if display_id == "primary":
            setattr(self.args, key, value)
        else:
            entry = self.display_clients.get(display_id)
            if entry is not None:
                entry[key] = value

    async def _apply_display_setting(self, display_id: str, key: str, value: Any) -> None:
        """Store one video setting as the display's current value and apply it to
        the display's live pipeline when it exists (a secondary that has not been
        laid out yet picks the stored value up at pipeline creation)."""
        applier = self._VIDEO_SETTING_APPLIERS.get(key)
        if applier is None:
            return
        self._store_display_setting(display_id, key, value)
        pipeline = self.display_pipelines.get(display_id)
        if pipeline is not None:
            await applier(pipeline, value)
        if key == "encoder" and display_id == "primary":
            # Written through to the settings singleton: transport services
            # re-seed from it on a mode switch, so the session's encoder must
            # live there for a client pick to survive the trip. The flag marks
            # a fresh pick during the webrtc leg, which outranks any stashed
            # pre-clamp value on the switch back.
            self.settings.encoder = str(value)
            self.settings._encoder_client_set = True
            if self.rtc_app:
                # Keep the RTCApp's global encoder current: it is the default
                # for munge_sdp/codec choice on CONNECTIONS CREATED LATER, and
                # the startup snapshot would go stale after a switch. Secondary
                # displays resolve per display through get_encoder_for_display.
                self.rtc_app.encoder = str(value)

    def _encoder_for_display(self, display_id: str) -> str:
        return str(self._display_setting(display_id, "encoder") or self.args.encoder)

    def _fullcolor_for_display(self, display_id: str) -> bool:
        return bool(self._display_setting(display_id, "video_fullcolor"))

    def _use_cpu_for_display(self, display_id: str) -> bool:
        return bool(self._display_setting(display_id, "use_cpu"))

    async def handle_update_settings(
        self, settings_json: Dict[str, Any], display_id: str = "primary"
    ) -> None:
        """Apply a client SETTINGS payload to the display that sent it.

        Every allowed entry needs server-side backing: a live setter dispatched
        below, or state the server reads later (the manual-resolution trio feeds
        the start-time resize and resolution policy; the live resize itself
        rides the `r,` input message). Video keys apply to the SENDING display
        only (websockets model); audio and the clipboard policy are
        stream-global whichever display asserts them.
        """
        settings_allowed_to_update = [
            "rate_control_mode",
            "video_crf",
            "video_bitrate",
            "audio_bitrate",
            "framerate",
            "use_cpu",
            "enable_binary_clipboard",
            "is_manual_resolution_mode",
            "manual_width",
            "manual_height",
            # Enforced on the resize paths (the client also aligns before sending).
            "force_aligned_resolution",
            "encoder",
            "video_fullcolor",
            "video_streaming_mode",
            "use_paint_over_quality",
            "video_paintover_crf",
            "video_paintover_burst_frames",
        ]
        # The manual-resolution trio is server/startup resolution policy, not a
        # per-stream tunable: only the primary's payload may assert it.
        primary_only_keys = ("is_manual_resolution_mode", "manual_width", "manual_height")

        display_id = display_id or "primary"
        if display_id != "primary" and display_id not in self.display_clients:
            logger.warning(
                f"Ignoring settings for unknown display '{display_id}' (not connected)."
            )
            return

        # Optional client keyboard-layout hint (seat-global, not a per-display
        # stream tunable): base-layout push on Wayland, informational on X11.
        kb_layout = settings_json.get("keyboardLayout")
        if kb_layout and self.input_handler is not None:
            await self.input_handler.apply_client_keyboard_layout(kb_layout)

        dpi_val = settings_json.get("scaling_dpi")
        if dpi_val is not None and display_id == "primary":
            # Websockets parity: the client seeds its DPR-derived scaling_dpi into
            # the very first SETTINGS payload; honoring it through the same guarded
            # path as 's,' applies the right scale on the first sync instead of
            # waiting for the dashboard's later correction (one fewer interval at
            # the wrong scale; handle_scaling is idempotent for the later 's,').
            try:
                await self.handle_scaling(float(dpi_val))
            except (TypeError, ValueError):
                logger.warning(f"Ignoring malformed scaling_dpi in SETTINGS: {dpi_val!r}")

        def sanitize_value(name: str, client_value: Any) -> Any:
            """One-transport wrapper over the shared sanitizer (settings.py)."""
            return sanitize_client_setting(name, client_value, self.settings, logger)

        # A secondary's position is runtime-adjustable (websockets parity): its
        # page may move its display to any side of the primary after joining.
        new_position = settings_json.get("displayPosition")
        if new_position is not None and display_id != "primary":
            new_position = str(new_position)
            if new_position not in ("right", "left", "up", "down"):
                logger.warning(f"Ignoring invalid displayPosition from '{display_id}': {new_position!r}")
            else:
                entry = self.display_clients.get(display_id)
                if entry is not None and entry.get("position", "right") != new_position:
                    entry["position"] = new_position
                    await self.reconfigure_displays()

        for key in settings_allowed_to_update:
            client_value = settings_json.get(key)
            if client_value is None:
                continue
            if key == "rate_control_mode" and self.args.enable_rate_control is False:
                logger.debug(
                    f"Server has rate control disabled. Ignoring update for '{key}'."
                )
                continue
            if key in primary_only_keys and display_id != "primary":
                continue
            if getattr(self.args, key, None) is None:
                logger.warning(f"Received unknown setting '{key}' from client")
                continue
            current_value = self._display_setting(display_id, key)
            sanitized_value = sanitize_value(key, client_value)
            if sanitized_value is None or sanitized_value == current_value:
                continue
            if key == "audio_bitrate":
                # Audio exists only on the primary display's pipeline.
                if self.media_pipeline:
                    await self.media_pipeline.set_audio_bitrate(int(sanitized_value))
                setattr(self.args, key, sanitized_value)
            elif key == "enable_binary_clipboard":
                await self.input_handler.update_binary_clipboard_setting(sanitized_value)
                setattr(self.args, key, sanitized_value)
            elif key in self._VIDEO_SETTING_APPLIERS:
                await self._apply_display_setting(display_id, key, sanitized_value)
            else:
                # Policy state with no live setter (manual trio, force_aligned):
                # stored for the resize paths / startup to read.
                self._store_display_setting(display_id, key, sanitized_value)
            logger.debug(
                f"Updated setting '{key}' for display '{display_id}' from {current_value} to {sanitized_value}"
            )

    def mon_rtc_config(
        self, stun_servers: List[str], turn_servers: List[str], rtc_config: Any
    ) -> None:
        """Monitor callback: fan a refreshed RTC config out to the signaling
        server (for clients) and the RTC app (for its own ICE agents)."""
        if self.peer_manager:
            logger.debug("updating signaling server RTC config")
            self.peer_manager.set_rtc_config(rtc_config)
        if self.rtc_app:
            logger.debug("updating STUN/TURN servers in RTC app")
            self.rtc_app.update_rtc_config(stun_servers, turn_servers)

    def _ensure_pacer(self, pc: Any, peer: Dict[str, Any], display_id: str) -> Optional[Any]:
        """Ensure the per-transport packet pacer is enabled/configured (called
        from the congestion loop; idempotent and cheap).

        Encoder ceiling: the display's configured video bitrate, CBR or not.

        Returns:
            The DTLS transport (so callers can snapshot its pacer), or None
            when it is not available yet.
        """
        # The shared DTLS transport is reachable via pc.sctp only once the
        # data-channel m-line is negotiated; media can flow (and TWCC
        # estimates accumulate) BEFORE that. Fall back to any transceiver's
        # sender transport — it is the same shared RTCDtlsTransport object.
        transport = getattr(getattr(pc, "sctp", None), "transport", None)
        if transport is None:
            for tr in pc.getTransceivers() or []:
                transport = getattr(getattr(tr, "sender", None), "transport", None)
                if transport is not None:
                    break
        if transport is None:
            return None
        lo_kbps, hi_kbps = settings.video_bitrate
        enc_kbps = float(self._display_setting(display_id, "video_bitrate") or hi_kbps)
        enc_bps = int(max(lo_kbps, min(hi_kbps, enc_kbps)) * 1000)
        if not transport.pacer_enabled():
            vsender = None
            for tr in pc.getTransceivers() or []:
                if getattr(tr, "kind", None) == "video":
                    vsender = tr.sender
                    break
            transport.enable_pacer(
                encoder_bps=enc_bps,
                # Keyframe requests must hit the encoder PIPELINE (dynamic IDR
                # injection, shared + throttled across viewers): on this stack
                # video rides the pre-encoded pack() path, where the sender's
                # __force_keyframe flag is silently ignored.
                request_keyframe=lambda did_=display_id: asyncio.ensure_future(
                    self.request_idr_for_display(did_)),
            )
            # Bootstrap the IDR floor from the session-start keyframe — on a
            # late attach, waiting for the next natural IDR would start the
            # floor at 0 and reset on the first real burst.
            kf_bytes = getattr(vsender, "_keyframe_bytes", None)
            if kf_bytes:
                transport.note_video_keyframe(
                    kf_bytes, natural=getattr(vsender, "_keyframe_natural", True))
            if transport.pacer_enabled():
                logger.info(
                    f"WebRTC pacer enabled for display '{display_id}' "
                    f"(encoder ceiling {enc_bps // 1000} kbps)")
        else:
            transport.set_pacer_encoder_bps(enc_bps)
        return transport

    async def _congestion_control_loop(self) -> None:
        """GCC-style bitrate adaptation from transport-wide-cc receiver feedback:
        per display, follow the slowest of ITS peers' goodput estimates with
        headroom, back off multiplicatively on loss, and retarget that display's
        encoder within the allowed video_bitrate range — one display's congested
        link never steers another's stream. Only CBR mode has a target to steer."""
        lo_kbps, hi_kbps = settings.video_bitrate
        logger.info(
            f"Congestion control loop started (CBR only, range {lo_kbps}-{hi_kbps} kbps)."
        )
        # Direct attribute access, no getattr default: a missing or misnamed
        # setting must raise rather than silently disable the pacer.
        pacer_on = bool(settings.webrtc_pacer[0])
        logger.info(f"WebRTC pacer setting: {'ON' if pacer_on else 'OFF'}.")
        while True:
            await asyncio.sleep(1.0)
            rtc_app = self.rtc_app
            if not rtc_app:
                continue
            # Group receiver estimates by the display each peer watches
            # (viewers of a display count toward that display's link).
            per_display: Dict[str, Dict[str, Any]] = {}
            for peer in rtc_app.peer_connections.values():
                pc = peer.get("peer_conn")
                did = peer.get("display_id", "primary") or "primary"
                if pacer_on:
                    # Attach before the feedback gate below: the pacer must run
                    # on links that never send transport-cc too. Its config
                    # tracks this loop's inputs (encoder ceiling from the
                    # display setting; goodput is fed per transport inside
                    # RTCDtlsTransport._twcc_process_feedback), and this is its
                    # only configuration path, so one bad peer must never kill
                    # the tick loop.
                    try:
                        dtls = self._ensure_pacer(pc, peer, did)
                        if self.metrics is not None and dtls is not None:
                            self.metrics.set_pacer_snapshot(did, dtls.pacer_snapshot())
                    except Exception:
                        logger.exception("_ensure_pacer failed (display %s)", did)
                sctp = getattr(pc, "sctp", None)
                estimate = getattr(getattr(sctp, "transport", None), "twcc_estimate", None)
                if not estimate:
                    continue
                bucket = per_display.setdefault(
                    did, {"goodputs": [], "worst_loss": 0.0})
                if estimate.get("goodput_bps"):
                    bucket["goodputs"].append(estimate["goodput_bps"])
                bucket["worst_loss"] = max(bucket["worst_loss"], estimate.get("loss_fraction", 0.0))
            for did, bucket in per_display.items():
                if not self.args.congestion_control:
                    continue
                pipeline = self.display_pipelines.get(did)
                if (
                    pipeline is None
                    or getattr(pipeline, "rc_mode", None) != RateControlMode.CBR
                ):
                    continue
                goodputs, worst_loss = bucket["goodputs"], bucket["worst_loss"]
                if not goodputs:
                    continue
                current = float(pipeline.video_bitrate)
                # The user-selected bitrate is the CEILING: congestion control only
                # backs off below it and recovers up to it. Clamping to the allowed
                # RANGE instead let a fast local segment ramp an 8000 kbps session to
                # 80000+ kbps, saturating the real path (TURN/WAN) with queuing lag and
                # loss-corrupted frames.
                ceiling = float(self._display_setting(did, "video_bitrate") or hi_kbps)
                ceiling = max(lo_kbps, min(hi_kbps, ceiling))
                if worst_loss > 0.10:
                    target = current * 0.7
                else:
                    # Damage-gated encoders are application-limited: measured goodput
                    # is merely what was sent (an idle screen reads ~0), NOT link
                    # capacity — so it must never drag the target DOWN. It may lift
                    # the target when it shows real headroom; otherwise recover
                    # multiplicatively toward the user ceiling after a loss backoff.
                    target = min(ceiling, max(current * 1.15, min(goodputs) * 0.85 / 1_000))
                # Steer at kbps precision (what set_video_bitrate applies).
                target = round(max(lo_kbps, min(ceiling, target)))
                if target != round(current):
                    logger.info(
                        f"Congestion control[{did}]: video bitrate {current:.0f} -> {target:.0f} kbps "
                        f"(goodput {min(goodputs) / 1e3:.1f} kbps, loss {worst_loss:.1%})"
                    )
                    await pipeline.set_video_bitrate(target)

    async def start_components(self) -> None:
        """Start the background tasks: input/clipboard/cursor workers, startup
        DPI and cursor size, the congestion/pacer loop, the monitors, the
        signaling client, and the configured TURN credential refreshers."""
        if self.input_handler:
            self.tasks.append(asyncio.create_task(self.input_handler.connect()))
            self.tasks.append(asyncio.create_task(self.input_handler.start_clipboard()))
            self.tasks.append(
                asyncio.create_task(self.input_handler.start_cursor_monitor())
            )

        # Apply the configured desktop DPI at startup so the first session sees
        # it even before any client syncs its own (96 is unity — skip the churn
        # when nothing diverges). On Wayland it becomes the session compositor's
        # output scale; with none up yet, the input handler re-applies when it
        # adopts one.
        startup_dpi = int(float(getattr(settings, "scaling_dpi", "96") or 96))
        if startup_dpi != 96:
            if IS_WAYLAND:
                if self.input_handler is not None:
                    await self.input_handler.realize_wayland_dpi(startup_dpi)
                self._last_applied_dpi = startup_dpi
            elif await set_dpi(startup_dpi):
                self._last_applied_dpi = startup_dpi

        # Apply an explicit --cursor-size to the X server at startup (handle_scaling
        # re-derives it on DPI changes); Wayland gets its size via CaptureSettings.
        if not IS_WAYLAND and settings.cursor_size > 0:
            initial_dpi = float(getattr(settings, "scaling_dpi", "96"))
            await set_cursor_size(cursor_size_for_dpi(initial_dpi, CURSOR_SIZE))

        # Logical monitors describe the layout of whichever transport defined
        # them, and a live switch leaves the previous one's behind: a desktop
        # keeps tiling its panels and maximized windows against a rectangle
        # that is no longer the screen. This service defines its own when a
        # second display arrives, and needs none for a single one.
        if not IS_WAYLAND:
            await clear_selkies_monitors()

        # The pacer rides this loop's 1 s tick but is an independent feature:
        # encoder steering stays gated on --congestion-control, the pacer on
        # --webrtc-pacer.
        if self.args.congestion_control or bool(settings.webrtc_pacer[0]):
            self.tasks.append(asyncio.create_task(self._congestion_control_loop()))

        if self.gpu_monitor:
            self.gpu_monitor.start()
        if self.system_monitor:
            self.system_monitor.start()
        if self.signaling_client:
            self.signaling_client.start()

        if self.monitoring_utils_used:
            turn_rest_username = self.args.turn_rest_username.replace(":", "-")
            if self.monitoring_utils_used.get("using_hmac_turn", False):
                self.mon_hmac_turn = HMACRTCMonitor(
                    turn_host=self.args.turn_host,
                    turn_port=self.args.turn_port,
                    turn_shared_secret=self.args.turn_shared_secret,
                    turn_username=turn_rest_username,
                    turn_protocol=self.args.turn_protocol,
                    turn_tls=self.args.turn_tls,
                    stun_host=self.args.stun_host,
                    stun_port=self.args.stun_port,
                    period=60,
                    enabled=True,
                )
                self.mon_hmac_turn.on_rtc_config = self.mon_rtc_config
                self.mon_hmac_turn.start()
            if self.monitoring_utils_used.get("using_rest_api", False):
                self.mon_rest_api = RESTRTCMonitor(
                    turn_rest_uri=self.args.turn_rest_uri,
                    turn_rest_username=turn_rest_username,
                    turn_rest_username_auth_header=self.args.turn_rest_username_auth_header,
                    turn_protocol=self.args.turn_protocol,
                    turn_rest_protocol_header=self.args.turn_rest_protocol_header,
                    turn_tls=self.args.turn_tls,
                    turn_rest_tls_header=self.args.turn_rest_tls_header,
                    turn_api_key=self.args.turn_rest_api_key,
                    period=60,
                    enabled=True,
                )
                self.mon_rest_api.on_rtc_config = self.mon_rtc_config
                self.mon_rest_api.start()
            if self.monitoring_utils_used.get("using_rtc_config_json", False):
                self.mon_rtc_config_file = RTCConfigFileMonitor(
                    rtc_file=self.args.rtc_config_json, enabled=True
                )
                self.mon_rtc_config_file.on_rtc_config = self.mon_rtc_config
                await self.mon_rtc_config_file.start()
            if self.monitoring_utils_used.get("using_cloudflare_turn", False):
                self.mon_cloudflare_turn = CloudflareRTCMonitor(
                    turn_token_id=self.args.cloudflare_turn_token_id,
                    api_token=self.args.cloudflare_turn_api_token,
                    enabled=True,
                )
                self.mon_cloudflare_turn.on_rtc_config = self.mon_rtc_config
                self.mon_cloudflare_turn.start()

    async def shutdown(self) -> None:
        """Gracefully shutdown all components."""
        if self._shutdown_called:
            logger.info("Shutdown already called, skipping")
            return
        self._shutdown_called = True
        logger.info("Starting shutdown sequence")

        # Cancel all running tasks
        self._cancel_primary_stop_grace()
        for task in list(self.tasks):
            try:
                if not task.done():
                    task.cancel()
            except Exception:
                logger.exception("Error cancelling task during shutdown")

        async def _await_with_timeout(
            coro: Awaitable[Any], name: str, timeout: float = 3.0
        ) -> Optional[Any]:
            """Await one component's stop with a timeout, swallowing every
            error so one wedged component cannot abort the whole shutdown."""
            try:
                return await asyncio.wait_for(coro, timeout=timeout)
            except asyncio.TimeoutError:
                logger.warning(
                    f"Timeout while waiting for {name} to stop (after {timeout}s)"
                )
            except asyncio.CancelledError:
                logger.info(f"{name} was cancelled during shutdown")
            except Exception as e:
                logger.exception(f"Error while stopping {name}: {e}")
            return None

        try:
            await asyncio.wait_for(
                asyncio.gather(*self.tasks, return_exceptions=True), timeout=5.0
            )
        except asyncio.TimeoutError:
            logger.warning(
                "Some background tasks did not exit within timeout; continuing with component shutdown"
            )
        except Exception:
            logger.exception("Unexpected error while awaiting background tasks")

        # Stop each component concurrently
        stop_coros = []
        if self.signaling_client:
            stop_coros.append(
                (
                    _await_with_timeout(
                        self.signaling_client.stop(), "signaling_client", 3.0
                    )
                )
            )
        for display_id, pipeline in list(self.display_pipelines.items()):
            stop_coros.append(
                (
                    _await_with_timeout(
                        pipeline.stop_media_pipeline(), f"media_pipeline[{display_id}]", 3.0
                    )
                )
            )
        if self.media_pipeline and "primary" not in self.display_pipelines:
            stop_coros.append(
                (
                    _await_with_timeout(
                        self.media_pipeline.stop_media_pipeline(), "media_pipeline", 3.0
                    )
                )
            )
        if self.rtc_app:
            stop_coros.append(
                (
                    _await_with_timeout(
                        self.rtc_app.stop_all_rtc_connections(), "rtc_app", 3.0
                    )
                )
            )
        stop_coros.append(
            _await_with_timeout(
                self._teardown_webrtc_virtual_mic(), "webrtc_virtual_mic", 3.0
            )
        )
        if self.input_handler:
            try:
                self.input_handler.stop_clipboard()
            except Exception:
                logger.exception("Error stopping clipboard monitor")
            try:
                self.input_handler.stop_cursor_monitor()
            except Exception:
                logger.exception("Error stopping cursor monitor")
            stop_coros.append(
                (
                    _await_with_timeout(
                        self.input_handler.disconnect(), "input_handler.disconnect", 3.0
                    )
                )
            )

        if self.gpu_monitor:
            stop_coros.append(
                (_await_with_timeout(self.gpu_monitor.stop(), "gpu_monitor", 2.0))
            )
        if self.system_monitor:
            stop_coros.append(
                (_await_with_timeout(self.system_monitor.stop(), "system_monitor", 2.0))
            )

        if self.mon_hmac_turn:
            stop_coros.append(
                (
                    _await_with_timeout(
                        self.mon_hmac_turn.stop(), "HMAC RTC Monitor", 2.0
                    )
                )
            )
        if self.mon_rest_api:
            stop_coros.append(
                (_await_with_timeout(self.mon_rest_api.stop(), "REST RTC Monitor", 2.0))
            )
        if self.mon_rtc_config_file:
            stop_coros.append(
                (
                    _await_with_timeout(
                        self.mon_rtc_config_file.stop(), "RTC Config File Monitor", 2.0
                    )
                )
            )
        if self.mon_cloudflare_turn:
            stop_coros.append(
                (
                    _await_with_timeout(
                        self.mon_cloudflare_turn.stop(), "Cloudflare TURN RTC Monitor", 2.0
                    )
                )
            )

        # Await all stop coroutines with a global timeout
        if stop_coros:
            try:
                await asyncio.wait_for(
                    asyncio.gather(*stop_coros, return_exceptions=True), timeout=5
                )
            except asyncio.TimeoutError:
                logger.warning(
                    "Component shutdown exceeded global timeout; some components may still be cleaning up"
                )
            except Exception:
                logger.exception(
                    "Unexpected error during concurrent component shutdown"
                )
        if self.metrics:
            try:
                # unregister() drains the CSV executor via shutdown(wait=True);
                # run it off the loop thread so the deterministic drain doesn't
                # block the event loop during teardown.
                await asyncio.to_thread(self.metrics.unregister)
            except Exception as e:
                logger.exception(f"Error unregistering metrics: {e}")

        self.tasks.clear()

        # Release component references to free memory
        self.signaling_client = None
        self.media_pipeline = None
        self.rtc_app = None
        self.input_handler = None
        self.system_monitor = None
        self.gpu_monitor = None
        self.metrics = None
        self.mon_hmac_turn = None
        self.mon_rest_api = None
        self.mon_rtc_config_file = None

        logger.info("Shutdown complete")

    async def run(self) -> None:
        """Bring the service up and block until the shutdown event fires."""
        self._shutdown_called = False
        try:
            _install_webrtc_teardown_noise_filters(asyncio.get_running_loop())
            await self.initialize_components()
            self.setup_callbacks()

            await self.start_components()
            await self.shutdown_event.wait()

        except asyncio.CancelledError:
            logger.info("Received webrtc stream mode shutdown")
        except Exception as e:
            logger.critical(f"Fatal error: {e}", exc_info=True)
            sys.exit(1)
        finally:
            await self.shutdown()

    async def start(self) -> None:
        """Supervisor entry point: run the service until stop() is called."""
        self.shutdown_event.clear()
        await self.run()

    async def stop(self) -> None:
        """Signal run() to exit and shut the service down."""
        self.shutdown_event.set()

    def register_routes(self, api_prefix: str, main_router: web.UrlDispatcher) -> None:
        """Register the WebRTC HTTP/WebSocket endpoints.

        Every endpoint lives under /api so the single nginx /api proxy rule
        fronts them — the signaling socket included, since that location
        forwards WebSocket upgrades. Paths off /api (such as a bare /turn or
        /webrtc/signaling) would sit unproxied behind the LSIO nginx: the TURN
        fetch and the signaling handshake would 404 and freeze the dashboard.
        """
        main_router.add_get(
            f"{api_prefix}/api/webrtc/signaling{{slash:/?}}", self.rtc_ws_handler
        )
        main_router.add_get(f"{api_prefix}/api/ws", self.rtc_ws_handler)
        main_router.add_get(f"{api_prefix}/api/turn", self.handle_turn_req)

    async def rtc_ws_handler(
        self, request: web.Request
    ) -> Union[web.Response, web.WebSocketResponse]:
        """Accept a signaling WebSocket, refusing with 409/503 while the WebRTC
        mode is inactive or still starting."""
        if self.supervisor.current_mode != self.mode:
            return web.Response(status=409, text="WebRTC mode is inactive")
        if self.peer_manager is None:
            # Mode flips before the service finishes initializing (RTC config
            # fetch precedes the peer manager); tell the client to retry rather
            # than AttributeError its handshake.
            return web.Response(status=503, headers={"Retry-After": "1"},
                                text="WebRTC service is still starting")
        ws = web.WebSocketResponse()
        await ws.prepare(request)

        peername = request.transport.get_extra_info("peername")
        remote_address = peername if peername else (request.remote, 0)
        await self.peer_manager.signaling_handler(
            ws, remote_address, auth_role_ceiling=request.get("auth_role_ceiling")
        )
        return ws

    async def handle_turn_req(self, request: web.Request) -> web.Response:
        """Serve a TURN credential request via the peer manager, refusing with
        409/503 while the WebRTC mode is inactive or still starting."""
        if self.supervisor.current_mode != self.mode:
            return web.json_response({"error": "WebRTC mode is inactive"}, status=409)
        if self.peer_manager is None:
            return web.json_response(
                {"error": "WebRTC service is still starting"},
                status=503, headers={"Retry-After": "1"})
        return await self.peer_manager.handle_turn_req(request)

