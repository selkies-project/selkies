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

import argparse
import asyncio
import json
from json import encoder
import logging
import os
import signal
import sys
import aiofiles
import aiofiles.os
from typing import Any, Dict, List, Optional
import uuid

logger = logging.getLogger("main")
logger.setLevel(logging.INFO)

from .rtc import RTCApp
from .media_pipeline import MediaPipeline, MediaPipelineGst, MediaPipelinePixel
from .webrtc_signaling import WebRTCSignaling
from .signaling_server import WebRTCSimpleServer
from .input_handler import WebRTCInput
from .display_utils import resize_display, set_dpi, set_cursor_size
from .webrtc_utils import SystemMonitor, Metrics, GPUMonitor, get_rtc_configuration
from .settings import settings_webrtc as settings, AppSettings, FINAL_SETTING_DEFINITIONS_WEBRTC as SETTING_DEFINITIONS
from types import SimpleNamespace
from .webrtc_utils import HMACRTCMonitor, RESTRTCMonitor, RTCConfigFileMonitor

CURSOR_SIZE = 32

def get_server_settings() -> dict:
    server_settings_payload = {"settings": {}}
    for setting_def in SETTING_DEFINITIONS:
        name = setting_def['name']
        if name in ['port', 'dri_node', 'debug', 'audio_device_name', 'watermark_path']:
            continue
        value = getattr(settings, name)
        if setting_def['type'] == 'bool':
            bool_val, is_locked = value
            payload_entry = {'value': bool_val, 'locked': is_locked}
        else:
            payload_entry = {'value': value}

        if setting_def['type'] == 'range':
            payload_entry['min'], payload_entry['max'] = value
            if 'meta' in setting_def and 'default_value' in setting_def['meta']:
                payload_entry['default'] = setting_def['meta']['default_value']
        elif setting_def['type'] in ['enum', 'list']:
            if 'meta' in setting_def and 'allowed' in setting_def['meta']:
                payload_entry['allowed'] = setting_def['meta']['allowed']
        server_settings_payload["settings"][name] = payload_entry
    return server_settings_payload

class WebRTCApp:
    def __init__(self):
        self.settings: Optional[AppSettings] = settings
        self.tasks: List[asyncio.Task] = []
        self.shutdown_event = asyncio.Event()
        self.signaling_client: Optional[WebRTCSignaling] = None
        self.media_pipeline: Optional[MediaPipeline] = None
        self.rtc_app: Optional[RTCApp] = None
        self.input_handler: Optional[WebRTCInput] = None
        self.system_monitor: Optional[SystemMonitor] = None
        self.gpu_monitor: Optional[GPUMonitor] = None
        self.metrics: Optional[Metrics] = None
        self.signaling_server: Optional[WebRTCSimpleServer] = None
        self._json_config_lock = asyncio.Lock()
        self.peer_id = 1
        self.args: Optional[SimpleNamespace] = None
        self._init_default_settings()
        self.monitoring_utils_used: Dict[str, bool] = {}
        self.mon_hmac_turn: Optional[HMACRTCMonitor] = None
        self.mon_rest_api: Optional[RESTRTCMonitor] = None
        self.mon_rtc_config_file: Optional[RTCConfigFileMonitor] = None

        # signal handlers
        signal.signal(signal.SIGINT, self.handle_signal)
        signal.signal(signal.SIGTERM, self.handle_signal)

    def _init_default_settings(self) -> None:
        self.args = SimpleNamespace()
        try:
            for setting_def in SETTING_DEFINITIONS:
                name = setting_def['name']
                stype = setting_def['type']
                if stype == 'bool':
                    value = getattr(self.settings, name)[0]
                elif stype == 'range':
                    min, max = getattr(self.settings, name)
                    value = min if min == max else setting_def.get('meta', {}).get('default_value', 0)
                elif stype == 'enum':
                    value = getattr(self.settings, name)
                elif stype == 'int' or stype == 'str':
                    value = getattr(self.settings, name)
                setattr(self.args, name, value)
        except Exception as e:
            logger.error(f"Error initializing default settings: {e}", exc_info=True)

        # TODO: Starting webrtc mode with some default resolution which will
        # be reconfigured upon client connection. Remove this later.
        asyncio.run_coroutine_threadsafe(resize_display("1920x1080"), asyncio.get_running_loop())

    def handle_signal(self, signum, event) -> None:
        logger.info(f"Received signal {signum}, initiating shutdown")
        self.shutdown_event.set()

    async def initialize_components(self) -> None:
        """Initialize all application components"""

        if self.args.enable_metrics_http:
            webrtc_csv = self.args.enable_webrtc_statistics
            self.metrics = Metrics(int(self.args.metrics_http_port), webrtc_csv)

        # Init signaling client
        self.signaling_client = self.create_signaling_client()

        if self.args.media_pipeline == 'gstreamer':
            self.media_pipeline = MediaPipelineGst(
                async_event_loop=asyncio.get_running_loop(),
                encoder=self.args.encoder_rtc,
                audio_channels=int(self.args.audio_channels),
                framerate=int(self.args.framerate),
                gpu_id=int(self.args.gpu_id),
                video_bitrate=int(self.args.video_bitrate) * 1000,  # Convert to kbps
                audio_bitrate=int(self.args.audio_bitrate),
                keyframe_distance=float(self.args.keyframe_distance),
                video_packetloss_percent=float(self.args.video_packetloss_percent),
                audio_packetloss_percent=float(self.args.audio_packetloss_percent)
            )
        else:
            self.media_pipeline = MediaPipelinePixel(
                async_event_loop=asyncio.get_running_loop(),
                encoder=self.args.encoder_rtc,
                framerate=int(self.args.framerate),
                video_bitrate=int(self.args.video_bitrate) * 1000,  # Convert to kbps
                audio_bitrate=int(self.args.audio_bitrate),
                audio_channels=int(self.args.audio_channels),
                audio_enabled = self.args.audio_enabled,
                audio_device_name=self.args.audio_device_name
            )

        # Fetch rtc configuration
        stun_servers, turn_servers, rtc_config, self.monitoring_utils_used = await get_rtc_configuration(self.args)
        self.rtc_app = RTCApp(
            async_event_loop=asyncio.get_running_loop(),
            encoder=self.args.encoder_rtc,
            stun_servers=stun_servers,
            turn_servers=turn_servers,
        )

        # Input handler
        self.input_handler = WebRTCInput(
            gst_webrtc_app=self.rtc_app,
            uinput_mouse_socket_path="",
            js_socket_path_prefix="/tmp",
            enable_clipboard=self.args.enable_clipboard,
            enable_cursors=self.args.enable_cursors,
            cursor_size=self.args.cursor_size,
            cursor_scale=1.0,
            cursor_debug=self.args.debug_cursors,
            upload_dir=self.args.upload_dir,
        )
        self.input_handler.initialize_upload_dir()

        # Initialize monitoring instances
        self.system_monitor = SystemMonitor()
        self.gpu_monitor = GPUMonitor(enabled=self.args.encoder_rtc.startswith("nv"))

        # Signaling server
        self.signaling_server = self.create_signaling_server(rtc_config, self.monitoring_utils_used)

    def create_signaling_client(self) -> WebRTCSignaling:
        """Create and configure signaling client."""
        using_https = self.args.enable_https
        using_basic_auth = self.args.enable_basic_auth
        ws_protocol = 'wss:' if using_https else 'ws:'

        client = WebRTCSignaling(
            f'{ws_protocol}//127.0.0.1:{self.args.port}/ws',
            0,  # server_peer_id
            1,  # client_peer_id
            enable_https=using_https,
            enable_basic_auth=using_basic_auth,
            basic_auth_user=self.args.basic_auth_user,
            basic_auth_password=self.args.basic_auth_password
        )
        return client

    # TODO: Handle the error scenario
    async def handle_signaling_error(self, error: Exception) -> None:
        """Handle signaling errors."""
        logger.error(f"Signaling client error: {error}. Closing the pipelines")
        await self.handle_signaling_disconnect()

    async def handle_signaling_disconnect(self) -> None:
        logger.info("signaling disconnected, stopping pipelines")
        try:
            await self.media_pipeline.stop_media_pipeline()
            await self.rtc_app.stop_rtc_connection()
        except Exception as e:
            logger.error(f"Error stopping pipelines on signaling disconnect: {e}", exc_info=True)

    async def handle_session_start(self, session_peer_id: int) -> None:
        logger.info(f"starting session for peer id {session_peer_id}")
        try:
            if str(session_peer_id) == str(self.peer_id):
                await self.media_pipeline.start_media_pipeline()
                await self.rtc_app.start_rtc_connection()
                # Initialize stats location directory
                if self.args.enable_webrtc_statistics:
                    self.metrics.initialize_webrtc_csv_file(self.args.webrtc_statistics_dir)
                logger.info(f"started session for peer id {session_peer_id}")
            else:
                logger.error(f"failed to start pipeline for peer_id: {session_peer_id}")
        except Exception as e:
            raise

    def create_signaling_server(self, rtc_config: Any, mon_utils_used: Dict[str, bool]) -> WebRTCSimpleServer:
        """Create signaling server instance."""
        using_hmac_turn = mon_utils_used.get('using_hmac_turn', False)
        options = argparse.Namespace()
        options.addr = self.args.addr
        options.port = self.args.port
        options.enable_basic_auth = self.args.enable_basic_auth
        options.basic_auth_user = self.args.basic_auth_user
        options.basic_auth_password = self.args.basic_auth_password
        options.enable_https = self.args.enable_https
        options.https_cert = self.args.https_cert
        options.https_key = self.args.https_key
        options.health = "/health"
        options.web_root = os.path.abspath(self.args.web_root)
        options.keepalive_timeout = 30
        options.cert_restart = False # using_https
        options.rtc_config_file = self.args.rtc_config_json
        options.rtc_config = rtc_config
        options.turn_shared_secret = self.args.turn_shared_secret if using_hmac_turn else ''
        options.turn_host = self.args.turn_host if using_hmac_turn else ''
        options.turn_port = self.args.turn_port if using_hmac_turn else ''
        options.turn_protocol = self.args.turn_protocol
        options.turn_tls = self.args.turn_tls
        options.turn_auth_header_name = self.args.turn_rest_username_auth_header
        options.stun_host = self.args.stun_host
        options.stun_port = self.args.stun_port
        options.mode = self.args.mode

        return WebRTCSimpleServer(options)

    def setup_callbacks(self) -> None:
        """Configure all application callbacks."""
        if not self.rtc_app or not self.media_pipeline or not self.input_handler:
            return

        # Signaling client callbacks
        self.signaling_client.on_error = self.handle_signaling_error
        self.signaling_client.on_disconnect = self.handle_signaling_disconnect
        self.signaling_client.on_session = self.handle_session_start
        self.signaling_client.on_sdp = self.rtc_app.set_sdp
        self.signaling_client.on_ice = self.rtc_app.set_ice

        # Media pipeline callbacks
        if self.args.media_pipeline == 'gstreamer':
            self.media_pipeline.produce_data = self.rtc_app.consume_data_gst
        else:
            self.media_pipeline.produce_data = self.rtc_app.consume_data_pixel
        self.media_pipeline.send_data_channel_message = self.rtc_app.send_media_data_over_channel

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
        self.input_handler.on_cursor_change = lambda data: self.rtc_app.send_cursor_data(data)
        self.input_handler.on_video_encoder_bit_rate = self.handle_video_bitrate_change
        self.input_handler.on_audio_encoder_bit_rate = self.handle_audio_bitrate_change
        self.input_handler.on_mouse_pointer_visible = lambda v: self.media_pipeline.set_pointer_visible(v)
        self.input_handler.on_clipboard_read = lambda d, t: self.rtc_app.send_clipboard_data(d, t)
        self.input_handler.on_set_fps = self.handle_fps_change
        self.input_handler.on_client_fps = lambda f: self.metrics.set_fps(f) if self.metrics else None
        self.input_handler.on_client_latency = lambda l: self.metrics.set_latency(l) if self.metrics else None
        self.input_handler.on_ping_response = lambda latency: self.rtc_app.send_latency_time(latency)
        self.input_handler.on_client_webrtc_stats = self.handle_client_werbtc_stats
        self.input_handler.on_update_settings = self.handle_update_settings

        if self.args.enable_resize:
            self.input_handler.on_resize = self.on_resize_handler
            self.input_handler.on_scaling_ratio = self.handle_scaling
        else:
            self.input_handler.on_resize = lambda res: logger.warning(f"remote resizing disabled, skipping resize to {res}")
            self.input_handler.on_scaling_ratio = lambda scale: logger.warning(f"remote scaling is disabled, skipping DPI scale change to {str(scale)}")

        # Monitoring callbacks
        self.gpu_monitor.on_stats = self.handle_gpu_stats
        self.system_monitor.on_timer = self.handle_system_monitor

    def handle_data_channel_open(self) -> None:
        logger.info("opened peer data channel for user input to X11")
        # Send initial server side settings to client for conditional UI rendering
        server_settings_payload = get_server_settings()
        self.rtc_app.send_media_data_over_channel("server_settings", server_settings_payload)
        self.rtc_app.send_cursor_data(self.rtc_app.last_cursor_sent)

    async def handle_video_bitrate_change(self, bitrate: int) -> None:
        """Handle video bitrate change request."""
        updated = await self.set_json_app_argument("video_bitrate", bitrate)
        if updated and self.media_pipeline:
            await self.media_pipeline.set_video_bitrate(bitrate)

    async def handle_audio_bitrate_change(self, bitrate: int) -> None:
        """Handle audio bitrate change request."""
        updated = await self.set_json_app_argument("audio_bitrate", bitrate)
        if updated and self.media_pipeline:
            await self.media_pipeline.set_audio_bitrate(bitrate)

    async def handle_fps_change(self, fps: int) -> None:
        """Handle FPS change request."""
        updated = await self.set_json_app_argument("framerate", fps)
        if updated and self.media_pipeline:
            await self.media_pipeline.set_framerate(fps)
        else:
            logger.error("Media pipeline not initialized, cannot set framerate")

    async def set_json_app_argument(self, key: str, value: Any) -> bool:
        """Asynchronously and atomically updates a JSON configuration file."""
        config_path = self.args.json_config
        # Create a unique temporary path in the same directory
        temp_path = os.path.join(os.path.dirname(config_path), f".{os.path.basename(config_path)}.{uuid.uuid4()}.tmp")

        async with self._json_config_lock:
            try:
                config = {}
                try:
                    if await aiofiles.os.path.exists(config_path):
                        async with aiofiles.open(config_path, 'r', encoding='utf-8') as f:
                            config = json.loads(await f.read())
                except (FileNotFoundError, json.JSONDecodeError):
                    pass

                config[key] = value

                # Write to a unique temporary file
                async with aiofiles.open(temp_path, 'w', encoding='utf-8') as f:
                    await f.write(json.dumps(config, indent=2))
                # Atomically replace the original file with the new one
                await aiofiles.os.replace(temp_path, config_path)
                return True

            except Exception as e:
                logger.error(f"Error updating json config file '{config_path}': {e}")
                # Ensure temp file is cleaned up on any error
                if await aiofiles.os.path.exists(temp_path):
                    await aiofiles.os.remove(temp_path)
                return False

    async def handle_client_werbtc_stats(self, webrtc_stat_type: str, webrtc_stats: str) -> None:
        if self.args.enable_metrics_http:
            await self.metrics.set_webrtc_stats(webrtc_stat_type, webrtc_stats)

    async def on_resize_handler(self, res: str) -> None:
        """Handle change of resolution change"""
        try:
            w_str, h_str = res.split("x")
            target_w, target_h = int(w_str), int(h_str)

            # Ensure dimensions are positive
            if target_w <= 0 or target_h <= 0:
                logger.error(f"Invalid target dimensions in resize request: {target_w}x{target_h}. Ignoring")
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
                logger.error(f"Dimensions became invalid ({target_w}x{target_h}) after odd adjustment. Ignoring")
                if self.media_pipeline:
                    self.media_pipeline.last_resize_success = False
                return

            success = await resize_display(f"{target_w}x{target_h}")
            if success:
                logger.info(f"resize_display('{target_w}x{target_h}') reported success")
                self.media_pipeline.width = target_w
                self.media_pipeline.height = target_h
            else:
                logger.error( f"resize_display('{target_w}x{target_h}') reported failure")
                self.media_pipeline.last_resize_success = False

        except ValueError:
            logger.error(f"Invalid resolution format in resize request: {res}")
            self.media_pipeline.last_resize_success = False
        except Exception as e:
            logger.error(f"Error during resize handling for '{res}': {e}", exc_info=True)
            if self.media_pipeline:
                self.media_pipeline.last_resize_success = False

    async def handle_scaling(self, dpi_value: float) -> None:
        if await set_dpi(int(dpi_value)):
            logger.info(f"Successfully set DPI to {dpi_value}")
        else:
            logger.error(f"Failed to set DPI to {dpi_value}")

        calculated_cursor_size = int(round(dpi_value / 96.0 * CURSOR_SIZE))
        new_cursor_size = max(1, calculated_cursor_size) # Ensure at least 1px

        logger.info(f"Attempting to set cursor size to: {new_cursor_size} (based on DPI {dpi_value})")
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
                self.system_monitor.mem_used
            )
            self.rtc_app.send_ping(t)

    async def handle_gpu_stats(self, load: float, memory_total: int, memory_used: int) -> None:
        """Handle GPU stats monitoring timer."""
        if self.rtc_app:
            self.rtc_app.send_gpu_stats(load, memory_total, memory_used)
        if self.metrics:
            self.metrics.set_gpu_utilization(load * 100)

    async def handle_update_settings(self, settings_json: dict) -> None:
        # TODO: Gradually expand the list of settings that can be updated via this method
        settings_allowed_to_update = [
            'video_bitrate',
            'audio_bitrate',
            'framerate',
        ]

        def sanitize_value(name: str, client_value: Any) -> Any:
            """Clamps ranges, validates enums, and enforces bools against server limits."""
            setting_def = next((s for s in SETTING_DEFINITIONS if s['name'] == name), None)
            if not setting_def:
                return None
            server_limit = getattr(self.settings, name)
            if client_value is None:
                if setting_def['type'] == 'range':
                    min_val, max_val = server_limit
                    return min_val if min_val == max_val else setting_def.get('meta', {}).get('default_value')
                elif setting_def['type'] == 'bool':
                    return server_limit[0]
                else: # enum, list, str, int
                    return server_limit
            try:
                if setting_def['type'] == 'range':
                    min_val, max_val = server_limit
                    sanitized = max(min_val, min(int(client_value), max_val))
                    if sanitized != int(client_value):
                        logger.warning(f"Client value for '{name}' ({client_value}) was clamped to {sanitized} (server range: {min_val}-{max_val}).")
                    return sanitized
                elif setting_def['type'] == 'enum':
                    allowed_values = setting_def['meta']['allowed']
                    if str(client_value) in allowed_values:
                        return client_value
                    server_default = allowed_values[0] if allowed_values else setting_def['default']
                    logger.warning(f"Client value for '{name}' ('{client_value}') is not in the allowed list {allowed_values}. Using server default '{server_default}'.")
                    return server_default
                elif setting_def['type'] == 'bool':
                    server_val, is_locked = server_limit
                    client_bool = str(client_value).lower() in ['true', '1']
                    if is_locked:
                        if client_bool != server_val:
                            logger.warning(f"Client tried to change locked setting '{name}' to '{client_bool}'. Request ignored, using server value '{server_val}'.")
                        return server_val
                    return client_bool
            except (ValueError, TypeError, IndexError):
                def_val_meta = setting_def.get('meta', {}).get('default_value')
                return def_val_meta if def_val_meta is not None else setting_def.get('default')
            return client_value

        for key, value in settings_json.items():
            if key not in settings_allowed_to_update:
                logger.warning(f"Client attempted to update disallowed setting '{key}'. Ignoring.")
                continue
            current_value = getattr(self.args, key, None)
            if current_value is not None:
                sanitized_value = sanitize_value(key, value)
                if sanitized_value and sanitized_value != current_value:
                    if key == 'video_bitrate' and self.media_pipeline:
                        await self.media_pipeline.set_video_bitrate(sanitized_value)
                    elif key == 'audio_bitrate' and self.media_pipeline:
                        await self.media_pipeline.set_audio_bitrate(int(sanitized_value))
                    elif key == 'framerate' and self.media_pipeline:
                        await self.media_pipeline.set_framerate(sanitized_value)
                    logger.debug(f"Updated setting '{key}' from {current_value} to {sanitized_value} based on client settings")
                    setattr(self.args, key, sanitized_value)
            else:
                logger.warning(f"Received unknown setting '{key}' from client")

    def mon_rtc_config(self, stun_servers, turn_servers, rtc_config):
        if self.signaling_server:
            logger.debug("updating signaling server RTC config")
            self.signaling_server.set_rtc_config(rtc_config)
        if self.rtc_app:
            logger.debug("updating STUN/TURN servers in RTC app")
            self.rtc_app.update_rtc_config(stun_servers, turn_servers)

    async def start_components(self) -> None:
        """Start all asynchronous tasks"""
        # Start signaling server
        self.tasks.append(asyncio.create_task(self.signaling_server.run()))

        # Start components
        if self.input_handler:
            self.tasks.append(asyncio.create_task(self.input_handler.connect()))
            self.tasks.append(asyncio.create_task(self.input_handler.start_clipboard()))
            self.tasks.append(asyncio.create_task(self.input_handler.start_cursor_monitor()))

        if self.metrics and self.args.enable_metrics_http:
           self.metrics.start_http()

        if self.gpu_monitor:
            self.gpu_monitor.start()
        if self.system_monitor:
            self.system_monitor.start()
        if self.signaling_client:
            self.signaling_client.start()

        if self.monitoring_utils_used:
            turn_rest_username = self.args.turn_rest_username.replace(":", "-")
            if self.monitoring_utils_used.get('using_hmac_turn', False):
                self.mon_hmac_turn = HMACRTCMonitor(
                    turn_host=self.args.turn_host,
                    turn_port=self.args.turn_port,
                    turn_shared_secret=self.args.turn_shared_secret,
                    turn_username=turn_rest_username,
                    turn_protocol=self.args.turn_protocol,
                    turn_tls=self.args.turn_tls,
                    stun_host=self.args.stun_host,
                    stun_port=self.args.stun_port,
                    period=60, enabled=True
                )
                self.mon_hmac_turn.on_rtc_config = self.mon_rtc_config
                self.mon_hmac_turn.start()
            if self.monitoring_utils_used.get('using_rest_api', False):
                self.mon_rest_api = RESTRTCMonitor(
                    turn_rest_uri=self.args.turn_rest_uri,
                    turn_rest_username=turn_rest_username,
                    turn_rest_username_auth_header=self.args.turn_rest_username_auth_header,
                    turn_protocol=self.args.turn_protocol,
                    turn_rest_protocol_header=self.args.turn_rest_protocol_header,
                    turn_tls=self.args.turn_tls,
                    turn_rest_tls_header=self.args.turn_rest_tls_header,
                    period=60, enabled=True            
                )
                self.mon_rest_api.on_rtc_config = self.mon_rtc_config
                self.mon_rest_api.start()
            if self.monitoring_utils_used.get('using_rtc_config_file', False):
                self.mon_rtc_config_file = RTCConfigFileMonitor(
                    rtc_file=self.args.rtc_config_json,
                    enabled=True
                )
                self.mon_rtc_config_file.on_rtc_config = self.mon_rtc_config
                self.mon_rtc_config_file.start()

    async def shutdown(self) -> None:
        """Gracefully shutdown all components."""
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
                logger.warning(f"Timeout while waiting for {name} to stop (after {timeout}s)")
            except asyncio.CancelledError:
                logger.info(f"{name} was cancelled during shutdown")
            except Exception as e:
                logger.exception(f"Error while stopping {name}: {e}")
            return None

        try:
            await asyncio.wait_for(asyncio.gather(*self.tasks, return_exceptions=True), timeout=5.0)
        except asyncio.TimeoutError:
            logger.warning("Some background tasks did not exit within timeout; continuing with component shutdown")
        except Exception:
            logger.exception("Unexpected error while awaiting background tasks")

        # Stop signaling client / media pipeline / rtc app / input_handler concurrently
        stop_coros = []
        if self.signaling_client:
            stop_coros.append((_await_with_timeout(self.signaling_client.stop(), "signaling_client", 3.0)))
        if self.signaling_server:
            stop_coros.append((_await_with_timeout(self.signaling_server.stop(), "signaling_server", 3.0)))
        if self.media_pipeline:
            stop_coros.append((_await_with_timeout(self.media_pipeline.stop_media_pipeline(), "media_pipeline", 3.0)))
        if self.rtc_app:
            stop_coros.append((_await_with_timeout(self.rtc_app.stop_rtc_connection(), "rtc_app", 3.0)))
        if self.input_handler:
            try:
                self.input_handler.stop_clipboard()
            except Exception:
                logger.exception("Error stopping clipboard monitor")
            try:
                self.input_handler.stop_cursor_monitor()
            except Exception:
                logger.exception("Error stopping cursor monitor")
            stop_coros.append((_await_with_timeout(self.input_handler.disconnect(), "input_handler.disconnect", 3.0)))

        if self.gpu_monitor:
            stop_coros.append((_await_with_timeout(self.gpu_monitor.stop(), "gpu_monitor", 2.0)))
        if self.system_monitor:
            stop_coros.append((_await_with_timeout(self.system_monitor.stop(), "system_monitor", 2.0)))
        if self.metrics:
            stop_coros.append((_await_with_timeout(self.metrics.stop_http(), "metrics", 2.0)))

        if self.mon_hmac_turn:
            stop_coros.append((_await_with_timeout(self.mon_hmac_turn.stop(), "HMAC RTC Monitor", 2.0)))
        if self.mon_rest_api:
            stop_coros.append((_await_with_timeout(self.mon_rest_api.stop(), "REST RTC Monitor", 2.0))) 
        if self.mon_rtc_config_file:
            stop_coros.append((_await_with_timeout(self.mon_rtc_config_file.stop(), "RTC Config File Monitor", 2.0))) 

         # Await all stop coroutines with a global timeout
        if stop_coros:
            try:
                await asyncio.wait_for(asyncio.gather(*stop_coros, return_exceptions=True), timeout=5)
            except asyncio.TimeoutError:
                logger.warning("Component shutdown exceeded global timeout; some components may still be cleaning up")
            except Exception:
                logger.exception("Unexpected error during concurrent component shutdown")

        self.tasks.clear()
        logger.info("Shutdown complete")

    async def run(self) -> None:
        try:
            # Initialize components and setup callbacks
            await self.initialize_components()
            self.setup_callbacks()

            await self.start_components()
            await self.shutdown_event.wait()

        except asyncio.CancelledError:
            logger.info("Received werbtc stream mode shutdown from supervisor")
        except Exception as e:
            logger.critical(f"Fatal error: {e}", exc_info=True)
            sys.exit(1)
        finally:
            await self.shutdown()

async def wr_entrypoint():
    """Main entry point for WebRTC application.
    Ideally called by StreamSupervisor class.
    """
    app = WebRTCApp()
    await app.run()

if __name__ == "__main__":
    # Run as standalone mode
    try:
        asyncio.run(wr_entrypoint())
    except (KeyboardInterrupt, SystemExit):
        logger.info("Application interrupted by user")