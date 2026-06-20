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

from aiohttp import web
from typing import Any, Dict, List, Optional

from .rtc import RTCApp
from .media_pipeline import MediaPipeline, MediaPipelinePixel, RateControlMode
from .webrtc.codecs import configure_multiopus
from .webrtc_signaling import WebRTCSignalingClient
from .signaling_server import WebRTCPeerManagement
from .input_handler import WebRTCInput
from .display_utils import resize_display, set_dpi, set_cursor_size, parse_gpu_id
from .webrtc_utils import SystemMonitor, Metrics, GPUMonitor, get_rtc_configuration
from .settings import settings, AppSettings, SETTING_DEFINITIONS
from types import SimpleNamespace
from .webrtc_utils import HMACRTCMonitor, RESTRTCMonitor, RTCConfigFileMonitor, CloudflareRTCMonitor
from .stream_server import BaseStreamingService, CentralizedStreamServer

logger = logging.getLogger("webrtc")
logger.setLevel(logging.INFO)

# Cursor base size in points at 96 DPI (DPI changes scale from it): the
# cursor_size setting (SELKIES_CURSOR_SIZE / XCURSOR_SIZE) when explicit,
# else the X11 default.
CURSOR_SIZE = settings.cursor_size if settings.cursor_size > 0 else 32
# Same switch selkies.py uses (SELKIES_WAYLAND / legacy PIXELFLUX_WAYLAND / --wayland):
# the input backend must match the capture backend, which gets the choice per
# capture via the CaptureSettings use_wayland field.
IS_WAYLAND = bool(settings.wayland[0])

# Default int bounds (mirror selkies.py). Min is not 0: settings without an
# explicit min may use -1 sentinels that must not be clamped up.
INT_SETTING_DEFAULT_MAX = 1_000_000
INT_SETTING_DEFAULT_MIN = -1_000_000


def get_server_settings() -> dict:
    server_settings_payload = {"settings": {}}
    for setting_def in SETTING_DEFINITIONS:
        name = setting_def["name"]
        if name in ["port", "encode_dri", "debug", "audio_device_name", "watermark_path", "recording_socket", "file_manager_path"]:
            continue
        # Never broadcast secrets/credentials (master_token, passwords, TURN
        # secrets, etc.) to clients.
        if setting_def.get("sensitive"):
            continue
        value = getattr(settings, name)
        if setting_def["type"] == "bool":
            bool_val, is_locked = value
            payload_entry = {"value": bool_val, "locked": is_locked}
        else:
            payload_entry = {"value": value}

        if setting_def["type"] == "range":
            payload_entry["min"], payload_entry["max"] = value
            if "meta" in setting_def and "default_value" in setting_def["meta"]:
                payload_entry["default"] = setting_def["meta"]["default_value"]
        elif setting_def["type"] in ["enum", "list"]:
            if "meta" in setting_def and "allowed" in setting_def["meta"]:
                payload_entry["allowed"] = setting_def["meta"]["allowed"]
        server_settings_payload["settings"][name] = payload_entry
    # Booleans the client gates its clipboard UI/handlers on, derived from the
    # single enable_clipboard policy string.
    clip = settings.enable_clipboard
    server_settings_payload["settings"]["clipboard_enabled"] = {"value": clip != "false"}
    server_settings_payload["settings"]["clipboard_in_enabled"] = {"value": clip in ("true", "in")}
    server_settings_payload["settings"]["clipboard_out_enabled"] = {"value": clip in ("true", "out")}
    return server_settings_payload


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
        # client reconfigures it to its own size anyway.
        if getattr(self.args, "is_manual_resolution_mode", False):
            width = int(getattr(self.args, "manual_width", 0) or 0)
            height = int(getattr(self.args, "manual_height", 0) or 0)
            if width > 0 and height > 0:
                asyncio.run_coroutine_threadsafe(
                    resize_display(f"{width}x{height}"), asyncio.get_running_loop()
                )

    async def initialize_components(self) -> None:
        """Initialize all application components"""

        if self.args.enable_metrics_http:
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
            video_bitrate=int(self.args.video_bitrate),
            audio_bitrate=int(self.args.audio_bitrate),
            audio_channels=int(self.args.audio_channels),
            audio_enabled=self.args.audio_enabled,
            audio_device_name=self.args.audio_device_name,
            crf=int(self.args.video_crf),
            video_fullcolor=bool(self.args.video_fullcolor),
        )
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

        # Input handler
        self.input_handler = WebRTCInput(
            rtc_app=self.rtc_app,
            uinput_mouse_socket_path="",
            js_socket_path_prefix="/tmp",
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
            f"{ws_protocol}//127.0.0.1:{self.args.port}{prefix}/ws",
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
        self, session_peer_id: str, client_type: str
    ) -> None:
        logger.info(
            f"starting session for client peer id: {session_peer_id} of type: {client_type}"
        )
        try:
            await self.rtc_app.start_rtc_connection(session_peer_id, client_type)
            # Initialize stats location directory
            if self.args.enable_webrtc_statistics:
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
        self.rtc_app.request_idr_frame = self.media_pipeline.dynamic_idr_frame
        self.rtc_app.on_sdp = self.signaling_client.send_sdp
        self.rtc_app.on_ice = self.signaling_client.send_ice
        self.rtc_app.on_data_open = self.handle_data_channel_open
        self.rtc_app.on_data_close = lambda: logger.info("Data channel closed")
        self.rtc_app.on_data_error = lambda e: logger.error(f"Data channel error: {e}")
        self.rtc_app.on_data_message = self.input_handler.on_message
        self.rtc_app.on_data_msg_bytes = self.input_handler.on_msg_data

        # Input handler callbacks
        self.input_handler.on_cursor_change = lambda data: (
            self.rtc_app.send_cursor_data(data)
        )
        self.input_handler.on_video_encoder_bit_rate = self.handle_video_bitrate_change
        self.input_handler.on_audio_encoder_bit_rate = self.handle_audio_bitrate_change
        self.input_handler.on_mouse_pointer_visible = lambda v: (
            self.media_pipeline.set_pointer_visible(v)
        )
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
        self.input_handler.on_update_rate_control_mode = (
            self.media_pipeline.update_rate_control_mode
        )
        self.input_handler.on_update_crf = self.media_pipeline.set_crf

        if self.args.enable_resize:
            self.input_handler.on_resize = self.on_resize_handler
            self.input_handler.on_scaling_ratio = self.handle_scaling
        else:
            self.input_handler.on_resize = lambda res: logger.warning(
                f"remote resizing disabled, skipping resize to {res}"
            )
            self.input_handler.on_scaling_ratio = lambda scale: logger.warning(
                f"remote scaling is disabled, skipping DPI scale change to {str(scale)}"
            )

        # Monitoring callbacks
        self.gpu_monitor.on_stats = self.handle_gpu_stats
        self.system_monitor.on_timer = self.handle_system_monitor

    def handle_data_channel_open(self) -> None:
        logger.info("opened peer data channel for user input to X11")
        # Send initial server side settings to client for conditional UI rendering
        server_settings_payload = get_server_settings()
        self.rtc_app.send_media_data_over_channel(
            "server_settings", server_settings_payload
        )
        self.send_current_cursor()

    def send_current_cursor(self) -> None:
        """Resend the current cursor over the data channel (on channel open / video restart).

        Idempotent; a slept/woken tab clears its cursor canvas and needs it back.
        """
        if not self.rtc_app:
            return
        cursor_data = None
        if self.input_handler and hasattr(
            self.input_handler, "get_current_cursor_data"
        ):
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
            self.rtc_app.send_cursor_data(cursor_data)
        except Exception as e:
            logger.warning(f"Failed to send current cursor to client: {e}")

    async def handle_video_bitrate_change(self, bitrate: int) -> None:
        """Handle video bitrate change request."""
        if self.media_pipeline:
            await self.media_pipeline.set_video_bitrate(bitrate)

    async def handle_audio_bitrate_change(self, bitrate: int) -> None:
        """Handle audio bitrate change request."""
        if self.media_pipeline:
            await self.media_pipeline.set_audio_bitrate(bitrate)

    async def handle_fps_change(self, fps: int) -> None:
        """Handle FPS change request."""
        if self.media_pipeline:
            await self.media_pipeline.set_framerate(fps)
        else:
            logger.error("Media pipeline not initialized, cannot set framerate")

    async def handle_client_werbtc_stats(
        self, webrtc_stat_type: str, webrtc_stats: str
    ) -> None:
        if self.args.enable_metrics_http:
            await self.metrics.set_webrtc_stats(webrtc_stat_type, webrtc_stats)

    async def on_resize_handler(self, res: str) -> None:
        """Handle change of resolution change"""
        try:
            w_str, h_str = res.split("x")
            target_w, target_h = int(w_str), int(h_str)

            # Ensure dimensions are positive
            if target_w <= 0 or target_h <= 0:
                logger.error(
                    f"Invalid target dimensions in resize request: {target_w}x{target_h}. Ignoring"
                )
                if self.media_pipeline:
                    self.media_pipeline.last_resize_success = False
                return  # Do not proceed with invalid dimensions

            # Ensure dimensions are even
            if target_w % 2 != 0:
                logger.debug(f"Adjusting odd width {target_w} to {target_w - 1}")
                target_w -= 1
            if target_h % 2 != 0:
                logger.debug(f"Adjusting odd height {target_h} to {target_h - 1}")
                target_h -= 1

            # Re-check positivity after odd adjustment
            if target_w <= 0 or target_h <= 0:
                logger.error(
                    f"Dimensions became invalid ({target_w}x{target_h}) after odd adjustment. Ignoring"
                )
                if self.media_pipeline:
                    self.media_pipeline.last_resize_success = False
                return

            success = await resize_display(f"{target_w}x{target_h}")
            if success:
                logger.info(f"resize_display('{target_w}x{target_h}') reported success")
                self.media_pipeline.width = target_w
                self.media_pipeline.height = target_h
            else:
                logger.error(
                    f"resize_display('{target_w}x{target_h}') reported failure"
                )
                self.media_pipeline.last_resize_success = False

        except ValueError:
            logger.error(f"Invalid resolution format in resize request: {res}")
            self.media_pipeline.last_resize_success = False
        except Exception as e:
            logger.error(
                f"Error during resize handling for '{res}': {e}", exc_info=True
            )
            if self.media_pipeline:
                self.media_pipeline.last_resize_success = False

    async def handle_scaling(self, dpi_value: float) -> None:
        if await set_dpi(int(dpi_value)):
            logger.info(f"Successfully set DPI to {dpi_value}")
        else:
            logger.error(f"Failed to set DPI to {dpi_value}")

        calculated_cursor_size = int(round(dpi_value / 96.0 * CURSOR_SIZE))
        new_cursor_size = max(1, calculated_cursor_size)  # Ensure at least 1px

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

    async def handle_update_settings(self, settings_json: dict) -> None:
        # Every entry needs server-side backing: a live setter dispatched below, or state
        # the server reads later (the manual-resolution trio feeds the start-time resize
        # and resolution policy; the live resize itself rides the `r,` input message).
        settings_allowed_to_update = [
            "rate_control_mode",
            "video_crf",
            "video_bitrate",
            "audio_bitrate",
            "framerate",
            "enable_binary_clipboard",
            "is_manual_resolution_mode",
            "manual_width",
            "manual_height",
        ]

        def sanitize_value(name: str, client_value: Any) -> Any:
            """Clamps ranges, validates enums, and enforces bools against server limits."""
            setting_def = next(
                (s for s in SETTING_DEFINITIONS if s["name"] == name), None
            )
            if not setting_def:
                return None
            server_limit = getattr(self.settings, name)
            if client_value is None:
                if setting_def["type"] == "range":
                    min_val, max_val = server_limit
                    return (
                        min_val
                        if min_val == max_val
                        else setting_def.get("meta", {}).get("default_value")
                    )
                elif setting_def["type"] == "bool":
                    return server_limit[0]
                else:  # enum, list, str, int
                    return server_limit
            try:
                if setting_def["type"] == "range":
                    min_val, max_val = server_limit
                    sanitized = max(min_val, min(int(client_value), max_val))
                    if sanitized != int(client_value):
                        logger.warning(
                            f"Client value for '{name}' ({client_value}) was clamped to {sanitized} (server range: {min_val}-{max_val})."
                        )
                    return sanitized
                elif setting_def["type"] == "enum":
                    allowed_values = setting_def["meta"]["allowed"]
                    if str(client_value) in allowed_values:
                        # Normalize to str so later equality checks don't flip on str-vs-int.
                        return str(client_value)
                    server_default = (
                        allowed_values[0] if allowed_values else setting_def["default"]
                    )
                    logger.warning(
                        f"Client value for '{name}' ('{client_value}') is not in the allowed list {allowed_values}. Using server default '{server_default}'."
                    )
                    return server_default
                elif setting_def["type"] in ("int", "float"):
                    sanitized = int(client_value) if setting_def["type"] == "int" else float(client_value)
                    meta = setting_def.get("meta", {})
                    # Top-level min/max win over meta so declared bounds aren't loosened
                    # by the sentinel-safe negative fallback.
                    min_val = setting_def.get("min", meta.get("min", INT_SETTING_DEFAULT_MIN))
                    # Cap client numbers so they can't request resource-exhausting values.
                    max_val = setting_def.get("max", meta.get("max", INT_SETTING_DEFAULT_MAX))
                    clamped = max(min_val, min(sanitized, max_val))
                    if clamped != sanitized:
                        logger.warning(
                            f"Client value for '{name}' ({client_value}) was clamped to {clamped} (bounds: {min_val}-{max_val})."
                        )
                    return clamped
                elif setting_def["type"] == "bool":
                    server_val, is_locked = server_limit
                    client_bool = str(client_value).lower() in ["true", "1"]
                    if is_locked:
                        if client_bool != server_val:
                            logger.warning(
                                f"Client tried to change locked setting '{name}' to '{client_bool}'. Request ignored, using server value '{server_val}'."
                            )
                        return server_val
                    return client_bool
            except (ValueError, TypeError, IndexError, OverflowError):
                # OverflowError guards JSON inf (1e999 -> int(inf)); fall back to default.
                def_val_meta = setting_def.get("meta", {}).get("default_value")
                return (
                    def_val_meta
                    if def_val_meta is not None
                    else setting_def.get("default")
                )
            return client_value

        for key in settings_allowed_to_update:
            client_value = settings_json.get(key)
            if client_value is None:
                continue
            if key == "rate_control_mode" and self.args.enable_rate_control is False:
                logger.debug(
                    f"Server has rate control disabled. Ignoring update for '{key}'."
                )
                continue
            current_value = getattr(self.args, key, None)
            if current_value is not None:
                sanitized_value = sanitize_value(key, client_value)
                if sanitized_value is not None and sanitized_value != current_value:
                    if key == "rate_control_mode":
                        await self.media_pipeline.update_rate_control_mode(
                            RateControlMode(sanitized_value)
                        )
                    elif key == "video_crf":
                        await self.media_pipeline.set_crf(sanitized_value)
                    elif key == "video_bitrate" and self.media_pipeline:
                        await self.media_pipeline.set_video_bitrate(sanitized_value)
                    elif key == "audio_bitrate" and self.media_pipeline:
                        await self.media_pipeline.set_audio_bitrate(
                            int(sanitized_value)
                        )
                    elif key == "framerate" and self.media_pipeline:
                        await self.media_pipeline.set_framerate(sanitized_value)
                    elif key == "enable_binary_clipboard":
                        await self.input_handler.update_binary_clipboard_setting(
                            sanitized_value
                        )
                    logger.debug(
                        f"Updated setting '{key}' from {current_value} to {sanitized_value} based on client settings"
                    )
                    setattr(self.args, key, sanitized_value)
            else:
                logger.warning(f"Received unknown setting '{key}' from client")

    def mon_rtc_config(self, stun_servers, turn_servers, rtc_config):
        if self.peer_manager:
            logger.debug("updating signaling server RTC config")
            self.peer_manager.set_rtc_config(rtc_config)
        if self.rtc_app:
            logger.debug("updating STUN/TURN servers in RTC app")
            self.rtc_app.update_rtc_config(stun_servers, turn_servers)

    async def _congestion_control_loop(self) -> None:
        """GCC-style bitrate adaptation from transport-wide-cc receiver feedback:
        follow the slowest peer's goodput estimate with headroom, back off
        multiplicatively on loss, and retarget the encoder within the allowed
        video_bitrate range. Only CBR mode has a bitrate target to steer."""
        lo_mbps, hi_mbps = settings.video_bitrate
        logger.info(
            f"Congestion control loop started (CBR only, range {lo_mbps}-{hi_mbps} Mbps)."
        )
        while True:
            await asyncio.sleep(1.0)
            pipeline = self.media_pipeline
            rtc_app = self.rtc_app
            if (
                not pipeline
                or not rtc_app
                or getattr(pipeline, "rc_mode", None) != RateControlMode.CBR
            ):
                continue
            goodputs, worst_loss = [], 0.0
            for peer in rtc_app.peer_connections.values():
                pc = peer.get("peer_conn")
                sctp = getattr(pc, "sctp", None)
                estimate = getattr(getattr(sctp, "transport", None), "twcc_estimate", None)
                if not estimate:
                    continue
                if estimate.get("goodput_bps"):
                    goodputs.append(estimate["goodput_bps"])
                worst_loss = max(worst_loss, estimate.get("loss_fraction", 0.0))
            if not goodputs:
                continue
            current = int(pipeline.video_bitrate)
            if worst_loss > 0.10:
                target = int(current * 0.7)
            else:
                target = int(min(goodputs) * 0.85 / 1_000_000)
            target = max(int(lo_mbps), min(int(hi_mbps), target))
            if target != current:
                logger.info(
                    f"Congestion control: video bitrate {current} -> {target} Mbps "
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

        # Apply an explicit --cursor-size to the X server at startup (handle_scaling
        # re-derives it on DPI changes); Wayland gets its size via CaptureSettings.
        if not IS_WAYLAND and settings.cursor_size > 0:
            initial_dpi = float(getattr(settings, "scaling_dpi", "96"))
            await set_cursor_size(max(1, int(round(initial_dpi / 96.0 * CURSOR_SIZE))))

        if self.args.congestion_control:
            self.tasks.append(asyncio.create_task(self._congestion_control_loop()))

        # Metrics HTTP server is now integrated with aiohttp, no separate server needed

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
        if self.media_pipeline:
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
        # Metrics HTTP server is now integrated with aiohttp, no separate server to stop

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
        main_router.add_get(
            f"{api_prefix}/webrtc/signaling{{slash:/?}}", self.rtc_ws_handler
        )
        main_router.add_get(f"{api_prefix}/ws", self.rtc_ws_handler)
        main_router.add_get(f"{api_prefix}/turn", self.handle_turn_req)

    async def rtc_ws_handler(self, request: web.Request) -> web.WebSocketResponse:
        if self.supervisor.current_mode != self.mode:
            return web.Response(status=409, text="WebRTC mode is inactive")
        ws = web.WebSocketResponse()
        await ws.prepare(request)

        peername = request.transport.get_extra_info("peername")
        remote_address = peername if peername else (request.remote, 0)
        await self.peer_manager.signaling_handler(ws, remote_address)
        return ws

    async def handle_turn_req(self, request: web.Request) -> web.Response:
        """Wrapper to handle TURN requests via aiohttp."""
        if self.supervisor.current_mode != self.mode:
            return web.json_response({"error": "WebRTC mode is inactive"}, status=409)
        return await self.peer_manager.handle_turn_req(request)

