# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import argparse
import os
import logging
import re

# Settings Precedence and Naming Convention
# -----------------------------------------
# The settings in this file follow a clear order of precedence:
#
# 1. Command-line (CLI) arguments (e.g., --port 9000) have the highest precedence.
# 2. The standard environment variable (e.g., export SELKIES_PORT=9000) is used if no CLI flag is set.
# 3. A legacy environment variable (e.g., export CUSTOM_WS_PORT=8888), if defined for the setting,
#    is used as a FALLBACK if the standard environment variable is not set.
# 4. The 'default' value in the SETTING_DEFINITIONS list is used if none of the above are set.
#
# Naming is automatically derived from the 'name' key in each setting's definition.
# A setting with `name: 'my_setting_name'` will correspond to:
#   - CLI Flag: --my-setting-name
#   - Standard Environment Variable: SELKIES_MY_SETTING_NAME
#
# Examples and Special Handling:
# ------------------------------
# - Simple setting (port): `export SELKIES_PORT=9000`
#
# - List/Enum settings (encoder): `export SELKIES_ENCODER="jpeg,x264enc"`
#   The first item (`jpeg`) becomes the default. The full list (`['jpeg', 'x264enc']`)
#   becomes the allowed options. Providing a single value locks the choice.
#
# - Boolean locking (use_cpu): `export SELKIES_USE_CPU="true|locked"`
#   The `|locked` suffix prevents the user from changing the value disabling the input for it.

# Common settings across both streaming modes
COMMON_SETTING_DEFINITIONS = [
    # Core Feature Toggles
    {'name': 'audio_enabled', 'type': 'bool', 'default': True, 'help': 'Enable server-to-client audio streaming.'},
    {'name': 'microphone_enabled', 'type': 'bool', 'default': True, 'help': 'Enable client-to-server microphone forwarding.'},
    {'name': 'gamepad_enabled', 'type': 'bool', 'default': True, 'help': 'Enable gamepad support.'},
    {'name': 'clipboard_enabled', 'type': 'bool', 'default': True, 'help': 'Enable clipboard synchronization.'},
    {'name': 'command_enabled', 'type': 'bool', 'default': True, 'help': 'Enable parsing of command websocket messages.'},
    {'name': 'file_transfers', 'type': 'list', 'default': 'upload,download', 'meta': {'allowed': ['upload', 'download']}, 'help': 'Allowed file transfer directions (comma-separated: "upload,download"). Set to "" or "none" to disable.'},
    {'name': 'framerate', 'type': 'range', 'default': '8-120', 'meta': {'default_value': 60}, 'help': 'Allowed framerate range (e.g., "8-165") or a fixed value (e.g., "60").'},

    # Audio Settings
    {'name': 'audio_bitrate', 'type': 'enum', 'default': '320000', 'meta': {'allowed': ['64000', '128000', '192000', '256000', '320000']}, 'help': 'The default audio bitrate.'},

    # Display & Resolution Settings
    {'name': 'is_manual_resolution_mode', 'type': 'bool', 'default': False, 'help': 'Lock the resolution to the manual width/height values.'},
    {'name': 'manual_width', 'type': 'int', 'default': 0, 'help': 'Lock width to a fixed value. Setting this forces manual resolution mode.'},
    {'name': 'manual_height', 'type': 'int', 'default': 0, 'help': 'Lock height to a fixed value. Setting this forces manual resolution mode.'},
    {'name': 'scaling_dpi', 'type': 'enum', 'default': '96', 'meta': {'allowed': ['96', '120', '144', '168', '192', '216', '240', '264', '288']}, 'help': 'The default DPI for UI scaling.'},

    # Input & Client Behavior Settings
    {'name': 'enable_binary_clipboard', 'type': 'bool', 'default': False, 'help': 'Allow binary data (e.g., images) on the clipboard.'},
    {'name': 'use_browser_cursors', 'type': 'bool', 'default': False, 'help': 'Use browser CSS cursors instead of rendering to canvas.'},
    {'name': 'use_css_scaling', 'type': 'bool', 'default': False, 'help': 'HiDPI when false, if true a lower resolution is sent from the client and the canvas is stretched.'},

    # UI Visibility Settings
    {'name': 'ui_title', 'type': 'str', 'default': 'Selkies', 'help': 'Title in top left corner of sidebar.'},
    {'name': 'ui_show_logo', 'type': 'bool', 'default': True, 'help': 'Show the Selkies logo in the sidebar.'},
    {'name': 'ui_show_core_buttons', 'type': 'bool', 'default': True, 'help': 'Show the core components buttons display, audio, microphone, and gamepad.'},
    {'name': 'ui_show_sidebar', 'type': 'bool', 'default': True, 'help': 'Show the main sidebar UI.'},
    {'name': 'ui_sidebar_show_video_settings', 'type': 'bool', 'default': True, 'help': 'Show the video settings section in the sidebar.'},
    {'name': 'ui_sidebar_show_screen_settings', 'type': 'bool', 'default': True, 'help': 'Show the screen settings section in the sidebar.'},
    {'name': 'ui_sidebar_show_audio_settings', 'type': 'bool', 'default': True, 'help': 'Show the audio settings section in the sidebar.'},
    {'name': 'ui_sidebar_show_stats', 'type': 'bool', 'default': True, 'help': 'Show the stats section in the sidebar.'},
    {'name': 'ui_sidebar_show_clipboard', 'type': 'bool', 'default': True, 'help': 'Show the clipboard section in the sidebar.'},
    {'name': 'ui_sidebar_show_files', 'type': 'bool', 'default': True, 'help': 'Show the file transfer section in the sidebar.'},
    {'name': 'ui_sidebar_show_apps', 'type': 'bool', 'default': True, 'help': 'Show the applications section in the sidebar.'},
    {'name': 'ui_sidebar_show_sharing', 'type': 'bool', 'default': True, 'help': 'Show the sharing section in the sidebar.'},
    {'name': 'ui_sidebar_show_gamepads', 'type': 'bool', 'default': True, 'help': 'Show the gamepads section in the sidebar.'},
    {'name': 'ui_sidebar_show_fullscreen', 'type': 'bool', 'default': True, 'help': 'Show the fullscreen button in the sidebar.'},
    {'name': 'ui_sidebar_show_gaming_mode', 'type': 'bool', 'default': True, 'help': 'Show the gaming mode button in the sidebar.'},
    {'name': 'ui_sidebar_show_trackpad', 'type': 'bool', 'default': True, 'help': 'Show the virtual trackpad button in the sidebar.'},
    {'name': 'ui_sidebar_show_keyboard_button', 'type': 'bool', 'default': True, 'help': 'Show the on-screen keyboard button in the display area.'},
    {'name': 'ui_sidebar_show_soft_buttons', 'type': 'bool', 'default': True, 'help': 'Show the soft buttons section in the sidebar.'},

    # Shared Modes
    {'name': 'enable_sharing', 'type': 'bool', 'default': True, 'help': 'Master toggle for all sharing features.'},
    {'name': 'enable_collab', 'type': 'bool', 'default': True, 'help': 'Enable collaborative (read-write) sharing link.'},
    {'name': 'enable_shared', 'type': 'bool', 'default': True, 'help': 'Enable view-only sharing links.'},
    {'name': 'enable_player2', 'type': 'bool', 'default': True, 'help': 'Enable sharing link for gamepad player 2.'},
    {'name': 'enable_player3', 'type': 'bool', 'default': True, 'help': 'Enable sharing link for gamepad player 3.'},
    {'name': 'enable_player4', 'type': 'bool', 'default': True, 'help': 'Enable sharing link for gamepad player 4.'},

    {'name': 'debug', 'type': 'bool', 'default': False, 'help': 'Enable debug logging.'},
    {'name': 'mode', 'type': 'str', 'default': 'websockets', 'help': "Specify the mode: 'webrtc' or 'websockets'; defaults to websockets"},
    {'name': 'enable_dual_mode', 'type': 'bool', 'default': False, 'help': 'Enable switching Streaming modes from UI'},
    {'name': 'audio_device_name', 'type': 'str', 'default': 'output.monitor', 'help': 'Audio device name for pcmflux capture.'},
]

SETTING_DEFINITIONS_WEBSOCKETS = [
    # Video & Encoder Settings
    {'name': 'encoder', 'type': 'enum', 'default': 'x264enc', 'meta': {'allowed': ['x264enc', 'x264enc-striped', 'jpeg']}, 'help': 'The default video encoder.'},
    {'name': 'h264_crf', 'type': 'range', 'default': '5-50', 'meta': {'default_value': 25}, 'help': 'Allowed H.264 CRF range (e.g., "5-50") or a fixed value.'},
    {'name': 'jpeg_quality', 'type': 'range', 'default': '1-100', 'meta': {'default_value': 40}, 'help': 'Allowed JPEG quality range (e.g., "1-100") or a fixed value.'},
    {'name': 'h264_fullcolor', 'type': 'bool', 'default': False, 'help': 'Enable H.264 full color range for pixelflux encoders.'},
    {'name': 'h264_streaming_mode', 'type': 'bool', 'default': False, 'help': 'Enable H.264 streaming mode for pixelflux encoders.'},
    {'name': 'use_cpu', 'type': 'bool', 'default': False, 'help': 'Force CPU-based encoding for pixelflux.'},
    {'name': 'use_paint_over_quality', 'type': 'bool', 'default': True, 'help': 'Enable high-quality paint-over for static scenes.'},
    {'name': 'paint_over_jpeg_quality', 'type': 'range', 'default': '1-100', 'meta': {'default_value': 90}, 'help': 'Allowed JPEG paint-over quality range or a fixed value.'},
    {'name': 'h264_paintover_crf', 'type': 'range', 'default': '5-50', 'meta': {'default_value': 18}, 'help': 'Allowed H.264 paint-over CRF range or a fixed value.'},
    {'name': 'h264_paintover_burst_frames', 'type': 'range', 'default': '1-30', 'meta': {'default_value': 5}, 'help': 'Allowed H.264 paint-over burst frames range or a fixed value.'},
    {'name': 'second_screen', 'type': 'bool', 'default': True, 'help': 'Enable support for a second monitor/display.'},

    # Server Startup & Operational Settings
    {'name': 'port', 'type': 'int', 'default': 8081, 'env_var': 'CUSTOM_WS_PORT', 'help': 'Port for the data websocket server.'},
    {'name': 'control_port', 'type': 'int', 'default': 8083, 'help': 'Port for the internal control plane API.'},
    {'name': 'master_token', 'type': 'str', 'default': '', 'help': 'Master token to enable secure mode and protect the control plane API.'},
    {'name': 'dri_node', 'type': 'str', 'default': '', 'env_var': 'DRI_NODE', 'help': 'Path to the DRI render node for VA-API.'},
    {'name': 'watermark_path', 'type': 'str', 'default': '', 'env_var': 'WATERMARK_PNG', 'help': 'Absolute path to the watermark PNG file.'},
    {'name': 'watermark_location', 'type': 'int', 'default': -1, 'env_var': 'WATERMARK_LOCATION', 'help': 'Watermark location enum (0-6).'},
    {'name': 'wayland_socket_index', 'type': 'int', 'default': 0, 'help': 'Index for the Wayland command socket (e.g. 0 for wayland-0).'},
]

SETTING_DEFINITIONS_WEBRTC = [
    {'name': 'json_config', 'type': 'str', 'default': '/tmp/selkies_config.json', 'help': 'Path to the JSON file containing argument key-value pairs that are overlaid with CLI arguments or environment variables, this path must be writable'},
    {'name': 'addr', 'type': 'str', 'default': '0.0.0.0', 'help': 'Host to listen to for the signaling and web server, default: "0.0.0.0"'},
    {'name': 'port', 'type': 'int', 'default': 8081, 'help': 'Port to listen to for the signaling and web server, default: "8081"'},
    {'name': 'web_root', 'type': 'str', 'default': '/opt/gst-web', 'help': 'Path to directory containing web application files, default: "/opt/gst-web"'},
    {'name': 'enable_https', 'type': 'bool', 'default': False, 'help': 'Enable or disable HTTPS for the web application, specifying a valid server certificate is recommended'},
    {'name': 'https_cert', 'type': 'str', 'default': '/etc/ssl/certs/ssl-cert-snakeoil.pem', 'help': 'Path to the TLS server certificate file when HTTPS is enabled'},
    {'name': 'https_key', 'type': 'str', 'default': '/etc/ssl/private/ssl-cert-snakeoil.key', 'help': 'Path to the TLS server private key file when HTTPS is enabled, set to an empty value if the private key is included in the certificate'},
    {'name': 'enable_basic_auth', 'type': 'bool', 'default': True, 'help': 'Enable basic authentication on server, must set --basic_auth_password and optionally --basic_auth_user to enforce basic authentication'},
    {'name': 'basic_auth_user', 'type': 'str', 'default': 'ubuntu', 'help': 'Username for basic authentication, default is to use the USER environment variable or a blank username if not present, must also set --basic_auth_password to enforce basic authentication'},
    {'name': 'basic_auth_password', 'type': 'str', 'default': 'mypasswd', 'help': 'Password used when basic authentication is set'},
    {'name': 'rtc_config_json', 'type': 'str', 'default': '/tmp/rtc.json', 'help': 'JSON file with WebRTC configuration to use, checked periodically, overriding all other STUN/TURN settings'},

    # TURN/STUN
    {'name': 'turn_rest_uri', 'type': 'str', 'default': '', 'help': 'URI for TURN REST API service, example: http://localhost:8008'},
    {'name': 'turn_rest_username', 'type': 'str', 'default': 'selkies-hostname', 'help': 'URI for TURN REST API service, default set to system hostname'},
    {'name': 'turn_rest_username_auth_header', 'type': 'str', 'default': 'x-auth-user', 'help': 'Header to pass user to TURN REST API service'},
    {'name': 'turn_rest_protocol_header', 'type': 'str', 'default': 'x-turn-protocol', 'help': 'Header to pass desired TURN protocol to TURN REST API service'},
    {'name': 'turn_rest_tls_header', 'type': 'str', 'default': 'x-turn-tls', 'help': 'Header to pass TURN (D)TLS usage to TURN REST API service'},
    {'name': 'turn_host', 'type': 'str', 'default': 'staticauth.openrelay.metered.ca', 'help': 'TURN host when generating RTC config from shared secret or using long-term credentials, IPv6 addresses must be enclosed with square brackets such as [::1]'},
    {'name': 'turn_port', 'type': 'int', 'default': 443, 'help': 'TURN port when generating RTC config from shared secret or using long-term credentials'},
    {'name': 'turn_protocol', 'type': 'str', 'default': 'udp', 'help': 'TURN protocol for the client to use ("udp" or "tcp"), set to "tcp" without the quotes if "udp" is blocked on the network, "udp" is otherwise strongly recommended'},
    {'name': 'turn_tls', 'type': 'bool', 'default': False, 'help': 'Enable or disable TURN over TLS (for the TCP protocol) or TURN over DTLS (for the UDP protocol), valid TURN server certificate required'},
    {'name': 'turn_shared_secret', 'type': 'str', 'default': 'openrelayprojectsecret', 'help': 'Shared TURN secret used to generate HMAC credentials, also requires --turn_host and --turn_port'},
    {'name': 'turn_username', 'type': 'str', 'default': '', 'help': 'Legacy non-HMAC TURN credential username, also requires --turn_host and --turn_port'},
    {'name': 'turn_password', 'type': 'str', 'default': '', 'help': 'Legacy non-HMAC TURN credential password, also requires --turn_host and --turn_port'},
    {'name': 'stun_host', 'type': 'str', 'default': 'stun.l.google.com', 'help': 'STUN host for NAT hole punching with WebRTC, change to your internal STUN/TURN server for local networks without internet, defaults to "stun.l.google.com"'},
    {'name': 'stun_port', 'type': 'int', 'default': 19302, 'help': 'STUN port for NAT hole punching with WebRTC, change to your internal STUN/TURN server for local networks without internet, defaults to "19302"'},
    {'name': 'enable_cloudflare_turn', 'type': 'bool', 'default': False, 'help': 'Enable Cloudflare TURN service, requires SELKIES_CLOUDFLARE_TURN_TOKEN_ID, and SELKIES_CLOUDFLARE_TURN_API_TOKEN'},
    {'name': 'cloudflare_turn_token_id', 'type': 'str', 'default': '', 'help': 'The Cloudflare TURN App token ID.'},
    {'name': 'cloudflare_turn_api_token', 'type': 'str', 'default': '', 'help': 'The Cloudflare TURN API token.'},

    {'name': 'encoder_rtc', 'type': 'enum', 'default': 'x264enc', 'meta': {'allowed': ['av1enc', 'x264enc', 'nvh264enc', 'vp8enc']}, 'help': 'GStreamer video encoder to use'},
    {'name': 'video_bitrate', 'type': 'range', 'default': '1-100', 'meta': {"default_value": 8}, 'help': 'Default video bitrate in Megabits per second (Mbps), allowed range (e.g., "1-100") or a fixed value (e.g., "8" for 8 Mbps)'},
    {'name': 'app_wait_ready', 'type': 'bool', 'default': False, 'help': 'Waits for --app_ready_file to exist before starting stream if set to "true"'},
    {'name': 'app_ready_file', 'type': 'str', 'default': '/tmp/selkies-appready', 'help': 'File set by sidecar used to indicate that app is initialized and ready'},
    {'name': 'uinput_mouse_socket', 'type': 'str', 'default': '', 'help': 'Path to the uinput mouse socket, if not provided uinput is used directly'},
    {'name': 'js_socket_path', 'type': 'str', 'default': '/tmp', 'help': 'Directory to write the Selkies Joystick Interposer communication sockets to, default: /tmp, results in socket files: /tmp/selkies_js{0-3}.sock'},
    {'name': 'gpu_id', 'type': 'str', 'default': '0', 'help': 'GPU ID for GStreamer hardware video encoders, will use enumerated GPU ID (0, 1, ..., n) for NVIDIA and /dev/dri/renderD{128 + n} for VA-API'},
    {'name': 'keyframe_distance', 'type': 'int', 'default': -1, 'help': 'Distance between video keyframes/GOP-frames in seconds, defaults to "-1" for infinite keyframe distance (ideal for low latency and preventing periodic blurs)'},
    {'name': 'congestion_control', 'type': 'bool', 'default': False, 'help': 'Enable Google Congestion Control (GCC), suggested if network conditions fluctuate and when bandwidth is >= 2 mbps but may lead to lower quality and microstutter due to adaptive bitrate in some encoders'},
    {'name': 'video_packetloss_percent', 'type': 'int', 'default': 0, 'help': 'Expected packet loss percentage (percent) for ULP/RED Forward Error Correction (FEC) in video, use "0" to disable FEC, less effective because of other mechanisms including NACK/PLI, enabling not recommended if Google Congestion Control is enabled'},
    {'name': 'audio_channels', 'type': 'int', 'default': 2, 'help': 'Number of audio channels, defaults to stereo (2 channels)'},
    {'name': 'audio_packetloss_percent', 'type': 'int', 'default': 0, 'help': 'Expected packet loss percentage (percent) for ULP/RED Forward Error Correction (FEC) in audio, use "0" to disable FEC'},
    {'name': 'enable_clipboard', 'type': 'str', 'default': 'true', 'help': 'Enable or disable the clipboard features, supported values: true, false, in, out'},
    {'name': 'enable_resize', 'type': 'bool', 'default': False, 'help': 'Enable dynamic resizing to match browser size'},
    {'name': 'enable_cursors', 'type': 'bool', 'default': True, 'help': 'Enable passing remote cursors to client'},
    {'name': 'debug_cursors', 'type': 'bool', 'default': False, 'help': 'Enable cursor debug logging'},
    {'name': 'cursor_size', 'type': 'int', 'default': -1, 'help': 'Cursor size in points for the local cursor, set instead XCURSOR_SIZE without of this argument to configure the cursor size for both the local and remote cursors'},
    {'name': 'enable_webrtc_statistics', 'type': 'bool', 'default': False, 'help': 'Enable WebRTC Statistics CSV dumping to the directory --webrtc_statistics_dir with filenames selkies-stats-video-[timestamp].csv and selkies-stats-audio-[timestamp].csv'},
    {'name': 'webrtc_statistics_dir', 'type': 'str', 'default': '/tmp', 'help': 'Directory to save WebRTC Statistics CSV from client with filenames selkies-stats-video-[timestamp].csv and selkies-stats-audio-[timestamp].csv'},
    {'name': 'enable_metrics_http', 'type': 'bool', 'default': False, 'help': 'Enable the Prometheus HTTP metrics port'},
    {'name': 'metrics_http_port', 'type': 'int', 'default': 8000, 'help': 'Port to start the Prometheus metrics server on'},
    {'name': 'upload_dir', 'type': 'str', 'default': '~/Desktop', 'help': "Directory to save the uploaded content, in absolute path format. Default to '~/Desktop' directory"},
    {'name': 'media_pipeline', 'type': 'enum', 'default': 'pixelflux', 'meta': {'allowed': ['gstreamer', 'pixelflux']}, 'help': 'Media pipeline to use; responsible for video and audio capturing and encoding of data. Defaults to pixelflux media pipeline'}
]

class AppSettings:
    """
    Parses and stores application settings from command-line arguments and
    environment variables, based on a centralized definition list.
    """
    def __init__(self, setting):
        parser = argparse.ArgumentParser(description="Selkies WebSocket Streaming Server")
        self._setting_definitions = setting
        self._add_arguments(parser)
        args, _ = parser.parse_known_args()
        self._process_and_set_attributes(args)

    def _add_arguments(self, parser):
        """Programmatically add arguments to the parser from definitions."""
        for setting in self._setting_definitions:
            name = setting['name']
            cli_flag = f'--{name.replace("_", "-")}'
            standard_env_var = f'SELKIES_{name.upper()}'
            legacy_env_var = setting.get('env_var')
            env_help_text = f"Env: {standard_env_var}"
            if legacy_env_var:
                env_help_text = f"Env: {standard_env_var} (or {legacy_env_var})"
            parser.add_argument(
                cli_flag,
                type=str,
                default=None,
                help=f"{setting['help']} ({env_help_text})"
            )

    def _process_and_set_attributes(self, args):
        """Process parsed arguments and set them as class attributes."""
        processed = {}
        overrides = {}
        for setting in self._setting_definitions:
            name = setting['name']
            stype = setting['type']
            cli_val = getattr(args, name, None)
            std_env_val = os.environ.get(f'SELKIES_{name.upper()}')
            legacy_env_val = os.environ.get(setting['env_var']) if setting.get('env_var') else None
            is_override = cli_val is not None or std_env_val is not None or legacy_env_val is not None
            overrides[name] = is_override

            raw_value = cli_val if cli_val is not None else (std_env_val if std_env_val is not None else (legacy_env_val if legacy_env_val is not None else setting['default']))
            processed_value = None
            try:
                if stype == 'bool':
                    val_str = str(raw_value).lower()
                    is_locked = '|locked' in val_str
                    base_val_str = val_str.split('|')[0]
                    bool_value = base_val_str in ['true', '1']
                    processed_value = (bool_value, is_locked)
                elif stype in ['enum', 'list']:
                    if is_override:
                        master_list = setting.get('meta', {}).get('allowed', [])
                        user_items = [item.strip() for item in str(raw_value).split(',') if item.strip()]
                        valid_items = [item for item in user_items if item in master_list]
                        if not valid_items:
                            logging.warning(f"Invalid value(s) '{raw_value}' for {name}. Using system default.")
                            default_str = str(setting['default'])
                            valid_items = [item.strip() for item in default_str.split(',') if item in master_list]
                        setting['meta']['allowed'] = valid_items
                        if stype == 'enum':
                            processed_value = valid_items[0] if valid_items else setting['default']
                        else: # list
                            processed_value = valid_items
                    else:
                        if stype == 'enum':
                            processed_value = setting['default']
                        else:
                            processed_value = [item.strip() for item in str(setting['default']).split(',') if item.strip()]
                elif stype == 'int':
                    processed_value = int(raw_value)
                elif stype == 'str':
                    processed_value = str(raw_value)
                elif stype == 'range':
                    val_str = str(raw_value)
                    if '-' in val_str:
                        min_val, max_val = map(int, val_str.split('-', 1))
                        processed_value = (min_val, max_val)
                    else:
                        locked_val = int(val_str)
                        processed_value = (locked_val, locked_val)
            except (ValueError, TypeError, IndexError) as e:
                logging.error(f"Could not parse setting '{name}' with value '{raw_value}'. Using default. Error: {e}")
                processed_value = setting['default']
                if stype == 'range':
                    min_val, max_val = map(int, str(processed_value).split('-', 1))
                    processed_value = (min_val, max_val)
            processed[name] = processed_value
        width_overridden = overrides.get('manual_width', False)
        height_overridden = overrides.get('manual_height', False)
        manual_mode_bool_is_set = processed.get('is_manual_resolution_mode', (False, False))[0]
        should_be_in_manual_mode = width_overridden or height_overridden or manual_mode_bool_is_set
        if should_be_in_manual_mode:
            logging.info("A manual resolution setting was activated; locking to manual mode.")
            processed['is_manual_resolution_mode'] = (True, True)
            if processed.get('manual_width', 0) <= 0:
                processed['manual_width'] = 1024
                logging.info("Manual width not set or invalid, defaulting to 1280.")
            if processed.get('manual_height', 0) <= 0:
                processed['manual_height'] = 768
                logging.info("Manual height not set or invalid, defaulting to 720.")
        for name, value in processed.items():
            setattr(self, name, value)

FINAL_SETTING_DEFINITIONS_WEBSOCKETS = COMMON_SETTING_DEFINITIONS +  SETTING_DEFINITIONS_WEBSOCKETS
FINAL_SETTING_DEFINITIONS_WEBRTC = COMMON_SETTING_DEFINITIONS +  SETTING_DEFINITIONS_WEBRTC

settings_ws = AppSettings(FINAL_SETTING_DEFINITIONS_WEBSOCKETS)
settings_webrtc = AppSettings(FINAL_SETTING_DEFINITIONS_WEBRTC)

if settings_ws.debug[0] or settings_webrtc.debug[0]:
    logging.getLogger().setLevel(logging.DEBUG)
    logging.getLogger("websockets").setLevel(logging.WARNING)
else:
    logging.getLogger().setLevel(logging.INFO)
    logging.getLogger("websockets").setLevel(logging.WARNING)

  
