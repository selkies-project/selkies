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

import sys
import logging
import asyncio
import argparse
import shutil

from aiohttp import web
from typing import Any, Dict, List, Optional

from .rtc import RTCApp
from .media_pipeline import MediaPipeline, MediaPipelinePixel, RateControlMode
from .webrtc.codecs import configure_multiopus
from .webrtc_signaling import WebRTCSignalingClient
from .signaling_server import WebRTCPeerManagement
from .input_handler import WebRTCInput
from .display_utils import (resize_display, set_dpi, set_cursor_size, parse_gpu_id,
                            compute_dual_layout, apply_extended_layout, get_new_res,
                            clear_selkies_monitors, clamp_primary_feedback,
                            parse_resize_dims, cursor_size_for_dpi, align_dims_16)
from .webrtc_utils import SystemMonitor, Metrics, GPUMonitor, get_rtc_configuration
from .settings import (settings, AppSettings, SETTING_DEFINITIONS,
                       build_client_settings_payload, sanitize_client_setting)
from types import SimpleNamespace
from .webrtc_utils import HMACRTCMonitor, RESTRTCMonitor, RTCConfigFileMonitor, CloudflareRTCMonitor
from .stream_server import BaseStreamingService, CentralizedStreamServer

logger = logging.getLogger("webrtc")


def _selkies_is_aioice_frame_chain(exc) -> bool:
    """True when every frame of the exception's traceback that belongs to a
    package points into aioice (allowing asyncio's own event-loop frames)."""
    tb = getattr(exc, "__traceback__", None)
    saw_aioice = False
    while tb is not None:
        fname = tb.tb_frame.f_code.co_filename
        if "aioice" in fname:
            saw_aioice = True
        tb = tb.tb_next
    return saw_aioice


class _AsyncioSendNoiseFilter(logging.Filter):
    """Drop asyncio's bare 'socket.send() raised exception.' warning: it fires
    per ICMP-unreachable UDP send, which is routine while ICE candidate pairs
    are probed or torn down, and carries no actionable detail."""

    def filter(self, record: logging.LogRecord) -> bool:
        return record.getMessage() != "socket.send() raised exception."


def _install_webrtc_teardown_noise_filters(loop) -> None:
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

    def handler(l, context):
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
                or "aioice" in str(context.get("future", ""))
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
# cursor_size setting (SELKIES_CURSOR_SIZE / XCURSOR_SIZE) when explicit,
# else the X11 default.
CURSOR_SIZE = settings.cursor_size if settings.cursor_size > 0 else 32
# Same switch selkies.py uses (SELKIES_WAYLAND / --wayland):
# the input backend must match the capture backend, which gets the choice per
# capture via the CaptureSettings use_wayland field.
IS_WAYLAND = bool(settings.wayland[0])

def get_server_settings() -> dict:
    return {"settings": build_client_settings_payload()}


class WebRTCService(BaseStreamingService):
    def __init__(self, supervisor: CentralizedStreamServer):
        super().__init__("webrtc")
        self.settings: Optional[AppSettings] = settings
        self.tasks: List[asyncio.Task] = []
        self.shutdown_event = asyncio.Event()
        self._shutdown_called = False
        self.signaling_client: Optional[WebRTCSignalingClient] = None
        self.media_pipeline: Optional[MediaPipeline] = None
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
        self.display_pipelines: Dict[str, MediaPipeline] = {}
        self._display_lock = asyncio.Lock()
        self._primary_dims: Optional[tuple] = None
        # Last (w, h) a client asked the primary to become. The realized size
        # may legitimately differ (CVT cell alignment widens the mode), so
        # idempotence must be judged against the request, not just the result.
        self._last_resize_request: Optional[tuple] = None
        # Multi-monitor WM swap (websockets parity): heavy DEs tile poorly across the
        # per-display regions, so swap to a minimal Openbox once a secondary joins.
        self._wm_swap_is_supported: Optional[bool] = None
        self._is_wm_swapped = False

        self._init_default_settings()

    def _init_default_settings(self) -> None:
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
        self._manual_dims = None
        if getattr(self.args, "is_manual_resolution_mode", False):
            width = int(getattr(self.args, "manual_width", 0) or 0)
            height = int(getattr(self.args, "manual_height", 0) or 0)
            if width > 0 and height > 0:
                self._manual_dims = (width - (width % 2), height - (height % 2))

    async def initialize_components(self) -> None:
        """Initialize all application components"""

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
            encoder_rtc=self.args.encoder_rtc,
            framerate=int(self.args.framerate),
            # Mbps; fractional for sub-Mbps targets (kbps conversion happens
            # at the capture-settings boundary).
            video_bitrate=float(self.args.video_bitrate),
            audio_bitrate=int(self.args.audio_bitrate),
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

        # Fetch rtc configuration
        (
            stun_servers,
            turn_servers,
            rtc_config,
            self.monitoring_utils_used,
        ) = await get_rtc_configuration(self.args)
        self.rtc_app = RTCApp(
            async_event_loop=asyncio.get_running_loop(),
            encoder=self.args.encoder_rtc,
            stun_servers=stun_servers,
            turn_servers=turn_servers,
        )
        self.rtc_app.media_pipeline = self.media_pipeline
        self.display_pipelines["primary"] = self.media_pipeline
        if IS_WAYLAND:
            # Seed the compositor capture scale from the configured DPI so the
            # FIRST pipeline start honors it; handle_scaling updates it on later
            # client DPI syncs.
            self.media_pipeline.scale = float(getattr(settings, "scaling_dpi", "96") or 96) / 96.0

        # Input handler
        self.input_handler = WebRTCInput(
            rtc_app=self.rtc_app,
            uinput_mouse_socket_path="",
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
            # Duck-typed layout source: send_x11_mouse offsets a secondary
            # display's coordinates by display_layouts[display_id].
            data_server_instance=self,
        )
        self.input_handler.initialize_upload_dir()

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

        prefix = ("/" + self.settings.subfolder.strip("/")) if settings.subfolder else ""
        username = self.settings.basic_auth_user
        password = self.settings.basic_auth_password
        client = WebRTCSignalingClient(
            f"{ws_protocol}//127.0.0.1:{self.args.port}{prefix}/api/ws",
            enable_https=using_https,
            enable_basic_auth=using_basic_auth,
            basic_auth_user=username,
            basic_auth_password=password,
        )
        return client

    async def handle_signaling_error(self, error: Exception) -> None:
        """Handle signaling errors."""
        logger.error(f"Signaling client error: {error}. Closing the pipelines")
        await self.handle_signaling_disconnect()

    async def handle_signaling_disconnect(self) -> None:
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
        logger.info(
            f"starting session for client peer id: {session_peer_id} of type: {client_type} (display '{display_id}')"
        )
        try:
            if display_id != "primary" and client_type == "controller":
                second_screen_enabled, _ = self.settings.second_screen
                if not second_screen_enabled:
                    logger.warning(
                        "Secondary display '%s' refused: second screens are disabled by server settings.",
                        display_id,
                    )
                    return
                if IS_WAYLAND:
                    logger.warning("Secondary displays are X11-only; refusing display '%s' on Wayland.", display_id)
                    return
                # Dimensions arrive through the client's first resize message.
                entry = self.display_clients.setdefault(display_id, {"width": 0, "height": 0})
                entry["position"] = display_position
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

    def create_peer_manager(self, rtc_config):
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
        self.media_pipeline.send_data_channel_message = (
            self.rtc_app.send_media_data_over_channel
        )
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
        # different encoders.
        self.rtc_app.get_encoder_for_display = self._encoder_for_display

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

    def handle_data_channel_open(self, channel=None) -> None:
        logger.info("opened peer data channel for user input to X11")
        # Greet the peer that just joined (every display page and viewer needs the
        # server settings for conditional UI, the current cursor, and the display
        # roster) on ITS channel; without one, fall back to broadcasting.
        server_settings_payload = get_server_settings()
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

    def send_current_cursor(self, channel=None) -> None:
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

    async def handle_video_bitrate_change(self, bitrate: float, display_id: str = "primary") -> None:
        """Video bitrate change for the display whose page sent it."""
        await self._apply_display_setting(display_id or "primary", "video_bitrate", bitrate)

    async def handle_audio_bitrate_change(self, bitrate: int) -> None:
        """Handle audio bitrate change request."""
        if self.media_pipeline:
            await self.media_pipeline.set_audio_bitrate(bitrate)

    async def handle_fps_change(self, fps: int, display_id: str = "primary") -> None:
        """Framerate change for the display whose page sent it."""
        await self._apply_display_setting(display_id or "primary", "framerate", fps)

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
        """CRF change for the display whose page sent it."""
        await self._apply_display_setting(display_id or "primary", "video_crf", int(crf))

    async def handle_client_werbtc_stats(
        self, webrtc_stat_type: str, webrtc_stats: str
    ) -> None:
        # Gate on the Metrics object itself (built for metrics-http AND/OR the CSV
        # statistics flag) so CSV-only configs actually ingest the stats they enabled.
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

    async def _resize_primary_display(self, res: str) -> None:
        """Handle change of resolution change"""
        # Only an admin-configured manual-resolution lock (server config) blocks client
        # resizes, mirroring the WebSocket handler. The client's own manual/auto toggle
        # lives in self.args and must NOT gate here: in client manual mode the chosen
        # resolution is delivered through this same resize path.
        server_is_manual, _ = self.settings.is_manual_resolution_mode
        if server_is_manual:
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
                if self.media_pipeline.is_media_pipeline_running():
                    await self.media_pipeline.restart_screen_capture()
                self.media_pipeline.last_resize_success = True
                logger.info(f"Wayland capture resized to {target_w}x{target_h}")
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
                self.media_pipeline.width = realized_w
                self.media_pipeline.height = realized_h
                self.media_pipeline.last_resize_success = True
                self._last_resize_request = (target_w, target_h)
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
        pipeline = self.display_pipelines.get(display_id or "primary")
        if pipeline is not None:
            await pipeline.dynamic_idr_frame()

    async def start_display_media(self, display_id: str) -> None:
        """A display's controller connected: the primary starts its pipeline right
        away; a secondary waits for its dimensions (the client's first resize
        message), which trigger the layout pass that creates its pipeline."""
        if display_id == "primary" and self.media_pipeline:
            await self.media_pipeline.start_media_pipeline()

    async def stop_display_media(self, display_id: str) -> None:
        if display_id == "primary":
            if self.media_pipeline:
                await self.media_pipeline.stop_media_pipeline()
            return
        async with self._display_lock:
            pipeline = self.display_pipelines.pop(display_id, None)
            self.display_clients.pop(display_id, None)
            self.display_layouts.pop(display_id, None)
            if pipeline is not None:
                await pipeline.stop_media_pipeline()
        await self.reconfigure_displays()

    async def _maybe_swap_wm_for_multimonitor(self) -> None:
        """Swap a heavy DE (XFCE/Plasma) to a minimal Openbox once a secondary display
        joins, so windows tile cleanly against the per-display regions (websockets parity)."""
        if IS_WAYLAND or self._is_wm_swapped:
            return
        if self._wm_swap_is_supported is None:
            self._wm_swap_is_supported = bool(
                shutil.which("xfce4-session") or shutil.which("startplasma-x11")
            )
        if not self._wm_swap_is_supported:
            return
        cmd = ["openbox", "--replace"]
        config_path = "/tmp/openbox_selkies_config.xml"
        try:
            with open(config_path, "w") as f:
                f.write("<openbox_config></openbox_config>\n")
            cmd = ["openbox", "--config-file", config_path, "--replace"]
        except IOError as e:
            logger.error(f"Could not write Openbox config ({e}); proceeding without it.")
        # One attempt per session, succeed or not — the websockets engine likewise
        # fires its detached swap once and does not retry on later reconfigures.
        self._is_wm_swapped = True
        try:
            await asyncio.create_subprocess_exec(
                *cmd, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL
            )
            logger.info("Multi-monitor setup: switched to Openbox.")
        except Exception as e:
            logger.error(f"Failed to switch to Openbox: {e}")

    async def reconfigure_displays(self) -> None:
        """Lay the extended desktop out for the connected displays and point each
        display's capture at its region — the WR counterpart of the websockets
        reconfigure engine, for the primary plus one secondary display."""
        if IS_WAYLAND:
            return
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
                self._broadcast_display_config()
                return
            did, info = secondary
            await self._maybe_swap_wm_for_multimonitor()
            if self._primary_dims is None:
                # The primary never resized through the layout path: take its
                # pipeline dimensions (kept current by the single-display path),
                # falling back to the live screen resolution.
                p_w, p_h = self.media_pipeline.width, self.media_pipeline.height
                if p_w <= 0 or p_h <= 0:
                    curr, _, _, _, _ = await get_new_res("1x1")
                    try:
                        p_w, p_h = (int(v) for v in curr.lower().split("x"))
                    except (ValueError, AttributeError):
                        logger.error("Cannot determine primary display size; aborting layout.")
                        return
                self._primary_dims = (p_w, p_h)
            # Auto-resize feedback guard, shared with the websockets layout engine.
            self._primary_dims = clamp_primary_feedback(
                self._primary_dims, self.display_layouts, info.get("position", "right")
            )
            layouts, total_w, total_h = compute_dual_layout(
                self._primary_dims, (info["width"], info["height"]),
                info.get("position", "right"),
            )
            layouts[did] = layouts.pop("secondary")
            if not await apply_extended_layout(layouts, total_w, total_h):
                return
            self.display_layouts = layouts
            p = layouts["primary"]
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
                    encoder_rtc=str(setting("encoder_rtc")),
                    framerate=int(setting("framerate")),
                    video_bitrate=float(setting("video_bitrate")),
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
                    return
                logger.info(f"Secondary display '{did}' pipeline started at {s}")
            else:
                await pipeline.update_capture_region(s["x"], s["y"], s["w"], s["h"])
        self._broadcast_display_config()

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

    async def handle_scaling(self, dpi_value: float) -> None:
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
        if await set_dpi(int(dpi_value)):
            self._last_applied_dpi = int(dpi_value)
            logger.info(f"Successfully set DPI to {dpi_value}")
        else:
            logger.error(f"Failed to set DPI to {dpi_value}")

        # On Wayland, DPI maps to the pixelflux compositor capture scale (set_dpi is
        # a no-op there). Restart capture so the new scale is read, mirroring the WS
        # path which threads scale through CaptureSettings.
        if IS_WAYLAND and self.media_pipeline:
            new_scale = float(dpi_value) / 96.0
            if new_scale != self.media_pipeline.scale:
                self.media_pipeline.scale = new_scale
                if self.media_pipeline.is_media_pipeline_running():
                    await self.media_pipeline.restart_screen_capture()

        new_cursor_size = cursor_size_for_dpi(dpi_value, CURSOR_SIZE)

        logger.info(
            f"Attempting to set cursor size to: {new_cursor_size} (based on DPI {dpi_value})"
        )
        if await set_cursor_size(new_cursor_size):
            logger.info(f"Successfully set cursor size to {new_cursor_size}")
        else:
            logger.error(f"Failed to set cursor size to {new_cursor_size}")

    async def handle_system_monitor(self, t: float) -> None:
        """Handle system monitoring timer."""
        if self.input_handler and self.rtc_app and self.system_monitor:
            self.input_handler.ping_start = t
            self.rtc_app.send_system_stats(
                self.system_monitor.cpu_percent,
                self.system_monitor.mem_total,
                self.system_monitor.mem_used,
            )
            self.rtc_app.send_ping(t)

    async def handle_gpu_stats(
        self, load: float, memory_total: int, memory_used: int
    ) -> None:
        """Handle GPU stats monitoring timer."""
        if self.rtc_app:
            self.rtc_app.send_gpu_stats(load, memory_total, memory_used)
        if self.metrics:
            self.metrics.set_gpu_utilization(load * 100)

    # Live per-pipeline setters for the client-tunable video settings. Each
    # display's pipeline owns its running values; these route one sanitized
    # value into one pipeline.
    _VIDEO_SETTING_APPLIERS = {
        "rate_control_mode": lambda p, v: p.update_rate_control_mode(RateControlMode(v)),
        "video_crf": lambda p, v: p.set_crf(v),
        "video_bitrate": lambda p, v: p.set_video_bitrate(v),
        "framerate": lambda p, v: p.set_framerate(v),
        "use_cpu": lambda p, v: p.set_use_cpu(bool(v)),
        "encoder_rtc": lambda p, v: p.set_encoder_rtc(str(v)),
        "video_fullcolor": lambda p, v: p.set_video_fullcolor(bool(v)),
        "video_streaming_mode": lambda p, v: p.set_video_streaming_mode(bool(v)),
        "use_paint_over_quality": lambda p, v: p.set_use_paint_over_quality(bool(v)),
        "video_paintover_crf": lambda p, v: p.set_video_paintover_crf(int(v)),
        "video_paintover_burst_frames": lambda p, v: p.set_video_paintover_burst_frames(int(v)),
    }

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
        if key == "encoder_rtc" and display_id == "primary" and self.rtc_app:
            # Keep the RTCApp's global encoder current: it is the default for
            # munge_sdp/codec choice on CONNECTIONS CREATED LATER, and the
            # startup snapshot would go stale after a switch. Secondary displays
            # resolve per display through get_encoder_for_display.
            self.rtc_app.encoder = str(value)

    def _encoder_for_display(self, display_id: str) -> str:
        return str(self._display_setting(display_id, "encoder_rtc") or self.args.encoder_rtc)

    async def handle_update_settings(self, settings_json: dict, display_id: str = "primary") -> None:
        # Every entry needs server-side backing: a live setter dispatched below, or state
        # the server reads later (the manual-resolution trio feeds the start-time resize
        # and resolution policy; the live resize itself rides the `r,` input message).
        # Video keys apply to the SENDING display only (websockets model); audio and
        # the clipboard policy are stream-global whichever display asserts them.
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
            "encoder_rtc",
            "video_fullcolor",
            "video_streaming_mode",
            "use_paint_over_quality",
            "video_paintover_crf",
            "video_paintover_burst_frames",
        ]
        # The manual-resolution trio is server/startup resolution policy, not a
        # per-stream tunable: only the primary's payload may assert it.
        primary_only_keys = ("is_manual_resolution_mode", "manual_width", "manual_height")
        global_keys = ("audio_bitrate", "enable_binary_clipboard")

        display_id = display_id or "primary"
        if display_id != "primary" and display_id not in self.display_clients:
            logger.warning(
                f"Ignoring settings for unknown display '{display_id}' (not connected)."
            )
            return

        def sanitize_value(name: str, client_value: Any) -> Any:
            """One-transport wrapper over the shared sanitizer (settings.py)."""
            return sanitize_client_setting(name, client_value, self.settings, logger)

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

    def mon_rtc_config(self, stun_servers, turn_servers, rtc_config):
        if self.peer_manager:
            logger.debug("updating signaling server RTC config")
            self.peer_manager.set_rtc_config(rtc_config)
        if self.rtc_app:
            logger.debug("updating STUN/TURN servers in RTC app")
            self.rtc_app.update_rtc_config(stun_servers, turn_servers)

    async def _congestion_control_loop(self) -> None:
        """GCC-style bitrate adaptation from transport-wide-cc receiver feedback:
        per display, follow the slowest of ITS peers' goodput estimates with
        headroom, back off multiplicatively on loss, and retarget that display's
        encoder within the allowed video_bitrate range — one display's congested
        link never steers another's stream. Only CBR mode has a target to steer."""
        lo_mbps, hi_mbps = settings.video_bitrate
        logger.info(
            f"Congestion control loop started (CBR only, range {lo_mbps}-{hi_mbps} Mbps)."
        )
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
                sctp = getattr(pc, "sctp", None)
                estimate = getattr(getattr(sctp, "transport", None), "twcc_estimate", None)
                if not estimate:
                    continue
                did = peer.get("display_id", "primary") or "primary"
                bucket = per_display.setdefault(did, {"goodputs": [], "worst_loss": 0.0})
                if estimate.get("goodput_bps"):
                    bucket["goodputs"].append(estimate["goodput_bps"])
                bucket["worst_loss"] = max(bucket["worst_loss"], estimate.get("loss_fraction", 0.0))
            for did, bucket in per_display.items():
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
                # RANGE instead let a fast local segment ramp an 8 Mbps session to
                # 80+ Mbps, saturating the real path (TURN/WAN) with queuing lag and
                # loss-corrupted frames.
                ceiling = float(self._display_setting(did, "video_bitrate") or hi_mbps)
                ceiling = max(lo_mbps, min(hi_mbps, ceiling))
                if worst_loss > 0.10:
                    target = current * 0.7
                else:
                    # Damage-gated encoders are application-limited: measured goodput
                    # is merely what was sent (an idle screen reads ~0), NOT link
                    # capacity — so it must never drag the target DOWN. It may lift
                    # the target when it shows real headroom; otherwise recover
                    # multiplicatively toward the user ceiling after a loss backoff.
                    target = min(ceiling, max(current * 1.15, min(goodputs) * 0.85 / 1_000_000))
                # Steer at kbps precision (what set_video_bitrate applies). Float math
                # throughout so a sub-Mbps CBR target or range cap isn't truncated to 0.
                target = round(max(lo_mbps, min(ceiling, target)), 3)
                if target != round(current, 3):
                    logger.info(
                        f"Congestion control[{did}]: video bitrate {current:g} -> {target:g} Mbps "
                        f"(goodput {min(goodputs) / 1e6:.1f} Mbps, loss {worst_loss:.1%})"
                    )
                    await pipeline.set_video_bitrate(target)

    async def start_components(self) -> None:
        """Start all asynchronous tasks"""
        # Start components
        if self.input_handler:
            self.tasks.append(asyncio.create_task(self.input_handler.connect()))
            self.tasks.append(asyncio.create_task(self.input_handler.start_clipboard()))
            self.tasks.append(
                asyncio.create_task(self.input_handler.start_cursor_monitor())
            )

        # Apply the configured desktop DPI at startup so the first session sees
        # it even before any client syncs its own (96 is the X default — skip
        # the xrdb churn when nothing diverges).
        startup_dpi = int(float(getattr(settings, "scaling_dpi", "96") or 96))
        if not IS_WAYLAND and startup_dpi != 96:
            if await set_dpi(startup_dpi):
                self._last_applied_dpi = startup_dpi

        # Apply an explicit --cursor-size to the X server at startup (handle_scaling
        # re-derives it on DPI changes); Wayland gets its size via CaptureSettings.
        if not IS_WAYLAND and settings.cursor_size > 0:
            initial_dpi = float(getattr(settings, "scaling_dpi", "96"))
            await set_cursor_size(cursor_size_for_dpi(initial_dpi, CURSOR_SIZE))

        if self.args.congestion_control:
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
        for task in list(self.tasks):
            try:
                if not task.done():
                    task.cancel()
            except Exception:
                logger.exception("Error cancelling task during shutdown")

        # helper to attempt an await with timeout and catch all errors
        async def _await_with_timeout(coro, name: str, timeout: float = 3.0):
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
        self._shutdown_called = False
        try:
            _install_webrtc_teardown_noise_filters(asyncio.get_running_loop())
            # Initialize components and setup callbacks
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

    async def start(self):
        self.shutdown_event.clear()
        await self.run()

    async def stop(self):
        self.shutdown_event.set()

    def register_routes(self, api_prefix: str, main_router: web.UrlDispatcher):
        # All WebRTC endpoints live under /api so the single nginx /api proxy
        # rule fronts them — the signaling socket included, since that location
        # forwards WebSocket upgrades. Registering them off /api (as /turn and
        # /webrtc/signaling) left them unproxied behind the LSIO nginx, which
        # 404'd /turn and the signaling handshake and froze the dashboard.
        main_router.add_get(
            f"{api_prefix}/api/webrtc/signaling{{slash:/?}}", self.rtc_ws_handler
        )
        main_router.add_get(f"{api_prefix}/api/ws", self.rtc_ws_handler)
        main_router.add_get(f"{api_prefix}/api/turn", self.handle_turn_req)

    async def rtc_ws_handler(self, request: web.Request) -> web.WebSocketResponse:
        if self.supervisor.current_mode != self.mode:
            return web.Response(status=409, text="WebRTC mode is inactive")
        ws = web.WebSocketResponse()
        await ws.prepare(request)

        peername = request.transport.get_extra_info("peername")
        remote_address = peername if peername else (request.remote, 0)
        await self.peer_manager.signaling_handler(
            ws, remote_address, auth_role_ceiling=request.get("auth_role_ceiling")
        )
        return ws

    async def handle_turn_req(self, request: web.Request) -> web.Response:
        """Wrapper to handle TURN requests via aiohttp."""
        if self.supervisor.current_mode != self.mode:
            return web.json_response({"error": "WebRTC mode is inactive"}, status=409)
        return await self.peer_manager.handle_turn_req(request)

