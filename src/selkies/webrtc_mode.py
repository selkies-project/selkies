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
import socket
import sys
import aiofiles
import aiofiles.os
from typing import Any, List, Optional
import uuid

logger = logging.getLogger("main")
logger.setLevel(logging.INFO)

from .rtc import RTCApp
from .media_pipeline import MediaPipeline
from .webrtc_signaling import WebRTCSignaling
from .signaling_server import WebRTCSimpleServer
from .input_handler import WebRTCInput as InputHandler
from .display_utils import resize_display, set_dpi, set_cursor_size
from .webrtc_utils import SystemMonitor, Metrics, GPUMonitor, get_rtc_configuration

CURSOR_SIZE = 32

class WebRTCApp:
    def __init__(self):
        self.args: Optional[argparse.Namespace] = None
        self.tasks: List[asyncio.Task] = []
        self.shutdown_event = asyncio.Event() # Could also be used by StreamMonitor to handle the shutdown more appropriately
        self.signaling_client: Optional[WebRTCSignaling] = None
        self.media_pipeline: Optional[MediaPipeline] = None
        self.rtc_app: Optional[RTCApp] = None
        self.input_handler: Optional[InputHandler] = None
        self.system_monitor: Optional[SystemMonitor] = None
        self.gpu_monitor: Optional[GPUMonitor] = None
        self.metrics: Optional[Metrics] = None
        self.signaling_server: Optional[WebRTCSimpleServer] = None
        self._json_config_lock = asyncio.Lock()
        self.peer_id = 1

        # signal handlers
        signal.signal(signal.SIGINT, self.handle_signal)
        signal.signal(signal.SIGTERM, self.handle_signal)


    def handle_signal(self, signum, event) -> None:
        logger.info(f"Received signal {signum}, initiating shutdown")
        self.shutdown_event.set()

    async def initialize_components(self) -> None:
        """Initialize all application components"""

        if self.args.enable_metrics_http.lower() == 'true':
            webrtc_csv = self.args.enable_webrtc_statistics.lower() == 'true'
            self.metrics = Metrics(int(self.args.metrics_http_port), webrtc_csv)

        # Init signaling client
        self.signaling_client = self.create_signaling_client()

        self.media_pipeline = MediaPipeline(
            async_event_loop=asyncio.get_running_loop(),
            encoder=self.args.encoder,
            audio_channels=int(self.args.audio_channels),
            framerate=int(self.args.framerate),
            gpu_id=int(self.args.gpu_id),
            video_bitrate=int(self.args.video_bitrate),
            audio_bitrate=int(self.args.audio_bitrate),
            keyframe_distance=float(self.args.keyframe_distance),
            video_packetloss_percent=float(self.args.video_packetloss_percent),
            audio_packetloss_percent=float(self.args.audio_packetloss_percent)
        )

        # Fetch rtc configuration
        stun_servers, turn_servers, rtc_config = await get_rtc_configuration(self.args)
        self.rtc_app = RTCApp(
            async_event_loop=asyncio.get_running_loop(),
            encoder=self.args.encoder,
            stun_servers=stun_servers,
            turn_servers=turn_servers,
        )

        # Input handler
        self.input_handler = InputHandler(
            gst_webrtc_app=self.rtc_app,
            uinput_mouse_socket_path="",
            js_socket_path_prefix="/tmp",
            enable_clipboard="true",
            enable_cursors=True,
            cursor_size=self.args.cursor_size,
            cursor_scale=1.0,
            cursor_debug=False,
        )

        # Initialize monitoring instances
        self.system_monitor = SystemMonitor()
        self.gpu_monitor = GPUMonitor(enabled=self.args.encoder.startswith("nv"))

        # Signaling server
        self.signaling_server = self.create_signaling_server(rtc_config)

    def create_signaling_client(self) -> WebRTCSignaling:
        """Create and configure signaling client."""
        using_https = self.args.enable_https.lower() == 'true'
        using_basic_auth = self.args.enable_basic_auth.lower() == 'true'
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
        await self.media_pipeline.stop_pipeline()
        await self.rtc_app.stop_rtc_connection()

    async def handle_session_start(self, session_peer_id: int) -> None:
        logger.info(f"starting session for peer id {session_peer_id}")
        if str(session_peer_id) == str(self.peer_id):
            await self.media_pipeline.start_media_pipeline()
            await self.rtc_app.start_rtc_connection()
            # Initialize stats location directory
            if self.args.enable_webrtc_statistics.lower() == 'true':
                self.metrics.initialize_webrtc_csv_file(self.args.webrtc_statistics_dir)
        else:
            logger.error(f"failed to start pipeline for peer_id: {session_peer_id}")
        logger.info(f"started session for peer id {session_peer_id}")

    def create_signaling_server(self, rtc_config: Any) -> WebRTCSimpleServer:
        """Create signaling server instance."""
        options = argparse.Namespace()
        options.addr = self.args.addr
        options.port = self.args.port
        options.enable_basic_auth = self.args.enable_basic_auth.lower() == 'true'
        options.basic_auth_user = self.args.basic_auth_user
        options.basic_auth_password = self.args.basic_auth_password
        options.enable_https = self.args.enable_https.lower() == 'true'
        options.https_cert = self.args.https_cert
        options.https_key = self.args.https_key
        options.health = "/health"
        options.web_root = os.path.abspath(self.args.web_root)
        options.keepalive_timeout = 30
        options.cert_restart = False # using_https
        options.rtc_config_file = self.args.rtc_config_json
        options.rtc_config = rtc_config
        options.turn_shared_secret = self.args.turn_shared_secret
        options.turn_host = self.args.turn_host
        options.turn_port = self.args.turn_port
        options.turn_protocol = self.args.turn_protocol
        options.turn_tls = self.args.turn_tls.lower() == 'true'
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
        self.media_pipeline.produce_data = self.rtc_app.consume_data
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
        self.input_handler.on_clipboard_read = lambda d: self.rtc_app.send_clipboard_data(d)
        self.input_handler.on_set_fps = self.handle_fps_change
        self.input_handler.on_client_fps = lambda f: self.metrics.set_fps(f) if self.metrics else None
        self.input_handler.on_client_latency = lambda l: self.metrics.set_latency(l) if self.metrics else None
        self.input_handler.on_ping_response = lambda latency: self.rtc_app.send_latency_time(latency)
        self.input_handler.on_client_webrtc_stats = self.handle_client_werbtc_stats

        if self.args.enable_resize.lower() == 'true':
            self.input_handler.on_resize = self.on_resize_handler
            self.input_handler.on_scaling_ratio = self.handle_scaling
        else:
            self.input_handler.on_resize = lambda res: logger.warning("remote resizing disabled, kipping resize to {res}")
            self.input_handler.on_scaling_ratio = lambda scale: logger.warning(f"remote resize is disabled, skipping DPI scale change to {str(scale)}")

        # Monitoring callbacks
        self.gpu_monitor.on_stats = lambda load, memory_total, memory_used: (
            self.rtc_app.send_gpu_stats(load, memory_total, memory_used) if self.rtc_app else None,
            self.metrics.set_gpu_utilization(load * 100) if self.metrics else None
        )
        self.system_monitor.on_timer = self.handle_system_monitor_timer

    def handle_data_channel_open(self) -> None:
        logger.info("opened peer data channel for user input to X11")

        self.rtc_app.send_framerate(self.media_pipeline.framerate)
        self.rtc_app.send_video_bitrate(self.media_pipeline.video_bitrate)
        self.rtc_app.send_audio_bitrate(self.media_pipeline.audio_bitrate)
        self.rtc_app.send_resize_enabled(self.args.enable_resize.lower())
        self.rtc_app.send_encoder(self.media_pipeline.encoder)
        self.rtc_app.send_cursor_data(self.rtc_app.last_cursor_sent)

    async def handle_video_bitrate_change(self, bitrate: int) -> None:
        """Handle video bitrate change request."""
        updated = await self.set_json_app_argument("video_bitrate", bitrate)
        if updated and self.media_pipeline:
            self.media_pipeline.set_video_bitrate(bitrate)

    async def handle_audio_bitrate_change(self, bitrate: int) -> None:
        """Handle audio bitrate change request."""
        updated = await self.set_json_app_argument("audio_bitrate", bitrate)
        if updated and self.media_pipeline:
            self.media_pipeline.set_audio_bitrate(bitrate)

    def handle_fps_change(self, fps: int) -> None:
        """Handle FPS change request."""
        # TODO: update fps_change in ws mode to async and refactor this func
        asyncio.run_coroutine_threadsafe(self.set_json_app_argument("framerate", fps), asyncio.get_event_loop())
        if self.media_pipeline:
            self.media_pipeline.set_framerate(fps)

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
        if self.args.enable_metrics_http.lower() == 'true':
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
                self.rtc_app.send_remote_resolution(res)
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

    async def handle_system_monitor_timer(self, t: float) -> None:
        """Handle system monitoring timer."""
        if self.input_handler and self.rtc_app and self.system_monitor:
            self.input_handler.ping_start = t
            self.rtc_app.send_system_stats(
                self.system_monitor.cpu_percent,
                self.system_monitor.mem_total,
                self.system_monitor.mem_used
            )
            self.rtc_app.send_ping(t)

    def parse_args(self) -> argparse.Namespace:
        parser = argparse.ArgumentParser()
        parser.add_argument('--json_config',
                            default=os.environ.get(
                                'SELKIES_JSON_CONFIG', '/tmp/selkies_config.json'),
                            help='Path to the JSON file containing argument key-value pairs that are overlaid with CLI arguments or environment variables, this path must be writable')
        parser.add_argument('--addr',
                            default=os.environ.get(
                                'SELKIES_ADDR', '0.0.0.0'),
                            help='Host to listen to for the signaling and web server, default: "0.0.0.0"')
        parser.add_argument('--port',
                            default=os.environ.get(
                                'SELKIES_PORT', '8080'),
                            help='Port to listen to for the signaling and web server, default: "8080"')
        parser.add_argument('--web_root',
                            default=os.environ.get(
                                'SELKIES_WEB_ROOT', '/opt/gst-web'),
                            help='Path to directory containing web application files, default: "/opt/gst-web"')
        parser.add_argument('--enable_https',
                            default=os.environ.get(
                                'SELKIES_ENABLE_HTTPS', 'false'),
                            help='Enable or disable HTTPS for the web application, specifying a valid server certificate is recommended')
        parser.add_argument('--https_cert',
                            default=os.environ.get(
                                'SELKIES_HTTPS_CERT', '/etc/ssl/certs/ssl-cert-snakeoil.pem'),
                            help='Path to the TLS server certificate file when HTTPS is enabled')
        parser.add_argument('--https_key',
                            default=os.environ.get(
                                'SELKIES_HTTPS_KEY', '/etc/ssl/private/ssl-cert-snakeoil.key'),
                            help='Path to the TLS server private key file when HTTPS is enabled, set to an empty value if the private key is included in the certificate')
        parser.add_argument('--enable_basic_auth',
                            default=os.environ.get(
                                'SELKIES_ENABLE_BASIC_AUTH', 'true'),
                            help='Enable basic authentication on server, must set --basic_auth_password and optionally --basic_auth_user to enforce basic authentication')
        parser.add_argument('--basic_auth_user',
                            default=os.environ.get(
                                'SELKIES_BASIC_AUTH_USER', os.environ.get('USER', '')),
                            help='Username for basic authentication, default is to use the USER environment variable or a blank username if not present, must also set --basic_auth_password to enforce basic authentication')
        parser.add_argument('--basic_auth_password',
                            default=os.environ.get(
                                'SELKIES_BASIC_AUTH_PASSWORD', 'mypasswd'),
                            help='Password used when basic authentication is set')
        parser.add_argument('--rtc_config_json',
                            default=os.environ.get(
                                'SELKIES_RTC_CONFIG_JSON', '/tmp/rtc.json'),
                            help='JSON file with WebRTC configuration to use, checked periodically, overriding all other STUN/TURN settings')
        parser.add_argument('--turn_rest_uri',
                            default=os.environ.get(
                                'SELKIES_TURN_REST_URI', ''),
                            help='URI for TURN REST API service, example: http://localhost:8008')
        parser.add_argument('--turn_rest_username',
                            default=os.environ.get(
                                'SELKIES_TURN_REST_USERNAME', "selkies-{}".format(socket.gethostname())),
                            help='URI for TURN REST API service, default set to system hostname')
        parser.add_argument('--turn_rest_username_auth_header',
                            default=os.environ.get(
                                'SELKIES_TURN_REST_USERNAME_AUTH_HEADER', 'x-auth-user'),
                            help='Header to pass user to TURN REST API service')
        parser.add_argument('--turn_rest_protocol_header',
                            default=os.environ.get(
                                'SELKIES_TURN_REST_PROTOCOL_HEADER', 'x-turn-protocol'),
                            help='Header to pass desired TURN protocol to TURN REST API service')
        parser.add_argument('--turn_rest_tls_header',
                            default=os.environ.get(
                                'SELKIES_TURN_REST_TLS_HEADER', 'x-turn-tls'),
                            help='Header to pass TURN (D)TLS usage to TURN REST API service')
        parser.add_argument('--turn_host',
                            default=os.environ.get(
                                'SELKIES_TURN_HOST', 'staticauth.openrelay.metered.ca'),
                            help='TURN host when generating RTC config from shared secret or using long-term credentials, IPv6 addresses must be enclosed with square brackets such as [::1]')
        parser.add_argument('--turn_port',
                            default=os.environ.get(
                                'SELKIES_TURN_PORT', '443'),
                            help='TURN port when generating RTC config from shared secret or using long-term credentials')
        parser.add_argument('--turn_protocol',
                            default=os.environ.get(
                                'SELKIES_TURN_PROTOCOL', 'udp'),
                            help='TURN protocol for the client to use ("udp" or "tcp"), set to "tcp" without the quotes if "udp" is blocked on the network, "udp" is otherwise strongly recommended')
        parser.add_argument('--turn_tls',
                            default=os.environ.get(
                                'SELKIES_TURN_TLS', 'false'),
                            help='Enable or disable TURN over TLS (for the TCP protocol) or TURN over DTLS (for the UDP protocol), valid TURN server certificate required')
        parser.add_argument('--turn_shared_secret',
                            default=os.environ.get(
                                'SELKIES_TURN_SHARED_SECRET', 'openrelayprojectsecret'),
                            help='Shared TURN secret used to generate HMAC credentials, also requires --turn_host and --turn_port')
        parser.add_argument('--turn_username',
                            default=os.environ.get(
                                'SELKIES_TURN_USERNAME', ''),
                            help='Legacy non-HMAC TURN credential username, also requires --turn_host and --turn_port')
        parser.add_argument('--turn_password',
                            default=os.environ.get(
                                'SELKIES_TURN_PASSWORD', ''),
                            help='Legacy non-HMAC TURN credential password, also requires --turn_host and --turn_port')
        parser.add_argument('--stun_host',
                            default=os.environ.get(
                                'SELKIES_STUN_HOST', 'stun.l.google.com'),
                            help='STUN host for NAT hole punching with WebRTC, change to your internal STUN/TURN server for local networks without internet, defaults to "stun.l.google.com"')
        parser.add_argument('--stun_port',
                            default=os.environ.get(
                                'SELKIES_STUN_PORT', '19302'),
                            help='STUN port for NAT hole punching with WebRTC, change to your internal STUN/TURN server for local networks without internet, defaults to "19302"')
        parser.add_argument('--enable_cloudflare_turn',
                            default=os.environ.get(
                                'SELKIES_ENABLE_CLOUDFLARE_TURN', 'false'),
                            help='Enable Cloudflare TURN service, requires SELKIES_CLOUDFLARE_TURN_TOKEN_ID, and SELKIES_CLOUDFLARE_TURN_API_TOKEN')
        parser.add_argument('--cloudflare_turn_token_id',
                            default=os.environ.get(
                                'SELKIES_CLOUDFLARE_TURN_TOKEN_ID', ''),
                            help='The Cloudflare TURN App token ID.')
        parser.add_argument('--cloudflare_turn_api_token',
                            default=os.environ.get(
                                'SELKIES_CLOUDFLARE_TURN_API_TOKEN', ''),
                            help='The Cloudflare TURN API token.')
        parser.add_argument('--app_wait_ready',
                            default=os.environ.get('SELKIES_APP_WAIT_READY', 'false'),
                            help='Waits for --app_ready_file to exist before starting stream if set to "true"')
        parser.add_argument('--app_ready_file',
                            default=os.environ.get('SELKIES_APP_READY_FILE', '/tmp/selkies-appready'),
                            help='File set by sidecar used to indicate that app is initialized and ready')
        parser.add_argument('--uinput_mouse_socket',
                            default=os.environ.get('SELKIES_UINPUT_MOUSE_SOCKET', ''),
                            help='Path to the uinput mouse socket, if not provided uinput is used directly')
        parser.add_argument('--js_socket_path',
                            default=os.environ.get('SELKIES_JS_SOCKET_PATH', '/tmp'),
                            help='Directory to write the Selkies Joystick Interposer communication sockets to, default: /tmp, results in socket files: /tmp/selkies_js{0-3}.sock')
        parser.add_argument('--encoder',
                            default=os.environ.get('SELKIES_ENCODER', 'x264enc'),
                            help='GStreamer video encoder to use')
        parser.add_argument('--gpu_id',
                            default=os.environ.get('SELKIES_GPU_ID', '0'),
                            help='GPU ID for GStreamer hardware video encoders, will use enumerated GPU ID (0, 1, ..., n) for NVIDIA and /dev/dri/renderD{128 + n} for VA-API')
        parser.add_argument('--framerate',
                            default=os.environ.get('SELKIES_FRAMERATE', '60'),
                            help='Framerate of the streamed remote desktop')
        parser.add_argument('--video_bitrate',
                            default=os.environ.get('SELKIES_VIDEO_BITRATE', '8000'),
                            help='Default video bitrate in kilobits per second')
        parser.add_argument('--keyframe_distance',
                            default=os.environ.get('SELKIES_KEYFRAME_DISTANCE', '-1'),
                            help='Distance between video keyframes/GOP-frames in seconds, defaults to "-1" for infinite keyframe distance (ideal for low latency and preventing periodic blurs)')
        parser.add_argument('--congestion_control',
                            default=os.environ.get('SELKIES_CONGESTION_CONTROL', 'false'),
                            help='Enable Google Congestion Control (GCC), suggested if network conditions fluctuate and when bandwidth is >= 2 mbps but may lead to lower quality and microstutter due to adaptive bitrate in some encoders')
        parser.add_argument('--video_packetloss_percent',
                            default=os.environ.get('SELKIES_VIDEO_PACKETLOSS_PERCENT', '0'),
                            help='Expected packet loss percentage (%%) for ULP/RED Forward Error Correction (FEC) in video, use "0" to disable FEC, less effective because of other mechanisms including NACK/PLI, enabling not recommended if Google Congestion Control is enabled')
        parser.add_argument('--audio_bitrate',
                            default=os.environ.get('SELKIES_AUDIO_BITRATE', '128000'),
                            help='Default audio bitrate in bits per second')
        parser.add_argument('--audio_channels',
                            default=os.environ.get('SELKIES_AUDIO_CHANNELS', '2'),
                            help='Number of audio channels, defaults to stereo (2 channels)')
        parser.add_argument('--audio_packetloss_percent',
                            default=os.environ.get('SELKIES_AUDIO_PACKETLOSS_PERCENT', '0'),
                            help='Expected packet loss percentage (%%) for ULP/RED Forward Error Correction (FEC) in audio, use "0" to disable FEC')
        parser.add_argument('--enable_clipboard',
                            default=os.environ.get('SELKIES_ENABLE_CLIPBOARD', 'true'),
                            help='Enable or disable the clipboard features, supported values: true, false, in, out')
        parser.add_argument('--enable_resize',
                            default=os.environ.get('SELKIES_ENABLE_RESIZE', 'false'),
                            help='Enable dynamic resizing to match browser size')
        parser.add_argument('--enable_cursors',
                            default=os.environ.get('SELKIES_ENABLE_CURSORS', 'true'),
                            help='Enable passing remote cursors to client')
        parser.add_argument('--debug_cursors',
                            default=os.environ.get('SELKIES_DEBUG_CURSORS', 'false'),
                            help='Enable cursor debug logging')
        parser.add_argument('--cursor_size',
                            default=os.environ.get('SELKIES_CURSOR_SIZE', os.environ.get('XCURSOR_SIZE', '-1')),
                            help='Cursor size in points for the local cursor, set instead XCURSOR_SIZE without of this argument to configure the cursor size for both the local and remote cursors')
        parser.add_argument('--enable_webrtc_statistics',
                            default=os.environ.get('SELKIES_ENABLE_WEBRTC_STATISTICS', 'false'),
                            help='Enable WebRTC Statistics CSV dumping to the directory --webrtc_statistics_dir with filenames selkies-stats-video-[timestamp].csv and selkies-stats-audio-[timestamp].csv')
        parser.add_argument('--webrtc_statistics_dir',
                            default=os.environ.get('SELKIES_WEBRTC_STATISTICS_DIR', '/tmp'),
                            help='Directory to save WebRTC Statistics CSV from client with filenames selkies-stats-video-[timestamp].csv and selkies-stats-audio-[timestamp].csv')
        parser.add_argument('--enable_metrics_http',
                            default=os.environ.get('SELKIES_ENABLE_METRICS_HTTP', 'false'),
                            help='Enable the Prometheus HTTP metrics port')
        parser.add_argument('--metrics_http_port',
                            default=os.environ.get('SELKIES_METRICS_HTTP_PORT', '8000'),
                            help='Port to start the Prometheus metrics server on')
        parser.add_argument('--debug', action='store_true',
                            help='Enable debug logging')
        parser.add_argument('--mode', default=os.environ.get("SELKIES_MODE", "webrtc"),
                            help="Specify the mode: 'webrtc' or 'websockets'; defaults to webrtc")
        parser.add_argument('--upload_dir', default=os.environ.get('SELKIES_UPLOAD_DIR', '~/Desktop'),
                            help="Directory to save the uploaded content, in absolute path format. Default to '~/Desktop' directory")

        args, _ = parser.parse_known_args()
        if os.path.exists(args.json_config):
            # Read and overlay arguments from json file
            # Note that these are explicit overrides only.
            try:
                with open(args.json_config) as f:
                    json_args = json.load(f)
                for k, v in json_args.items():
                    if k == "framerate":
                        args.framerate = str(int(v))
                    if k == "video_bitrate":
                        args.video_bitrate = str(int(v))
                    if k == "audio_bitrate":
                        args.audio_bitrate = str(int(v))
                    if k == "enable_resize":
                        args.enable_resize = str((str(v).lower() == 'true')).lower()
                    if k == "encoder":
                        args.encoder = v.lower()
            except Exception as e:
                logger.error(f"failed to load json config from {args.json_config}: {str(e)}")

        return args

    async def start_components(self) -> None:
        """Start all asynchronous tasks."""
        # Start signaling server
        self.tasks.append(asyncio.create_task(self.signaling_server.run()))

        # Start components
        if self.input_handler:
            await self.input_handler.connect()
            self.tasks.append(asyncio.create_task(self.input_handler.start_clipboard()))
            self.tasks.append(asyncio.create_task(self.input_handler.start_cursor_monitor()))

        if self.metrics and self.args.enable_metrics_http.lower() == 'true':
           self.metrics.start_http()

        self.gpu_monitor.start()
        self.system_monitor.start()

        self.signaling_client.start()

    async def stop_pipelines(self) -> None:
        """Stop all media pipelines."""
        if self.media_pipeline:
            await self.media_pipeline.stop_pipeline()
        if self.rtc_app:
            await self.rtc_app.stop_rtc_connection()

    async def shutdown(self) -> None:
        """Gracefully shutdown all components."""
        logger.info("Starting shutdown sequence")

        # Cancel all running tasks
        for task in self.tasks:
            if not task.done():
                task.cancel()

        await self.signaling_client.stop()
        # Stop pipelines
        await self.stop_pipelines()

        # Disconnect input
        if self.input_handler:
            self.input_handler.stop_clipboard()
            self.input_handler.stop_cursor_monitor()
            await self.input_handler.disconnect()

        if self.gpu_monitor:
            await self.gpu_monitor.stop()
        if self.system_monitor:
            await self.system_monitor.stop()
        if self.metrics:
            await self.metrics.stop_http()
        if self.signaling_server:
            await self.signaling_server.stop()
        logger.info("Shutdown complete")

    async def run(self) -> None:
        try:
            self.args = self.parse_args()

            # Configure logging
            if self.args.debug:
                logging.getLogger().setLevel(logging.DEBUG)
            else:
                logging.getLogger().setLevel(logging.INFO)
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