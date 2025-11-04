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

SETTING_DEFINITIONS = [
    # Core Feature Toggles
    {'name': 'audio_enabled', 'type': 'bool', 'default': True, 'help': 'Enable server-to-client audio streaming.'},
    {'name': 'microphone_enabled', 'type': 'bool', 'default': True, 'help': 'Enable client-to-server microphone forwarding.'},
    {'name': 'gamepad_enabled', 'type': 'bool', 'default': True, 'help': 'Enable gamepad support.'},
    {'name': 'clipboard_enabled', 'type': 'bool', 'default': True, 'help': 'Enable clipboard synchronization.'},
    {'name': 'command_enabled', 'type': 'bool', 'default': True, 'help': 'Enable parsing of command websocket messages.'},
    {'name': 'file_transfers', 'type': 'list', 'default': 'upload,download', 'meta': {'allowed': ['upload', 'download']}, 'help': 'Allowed file transfer directions (comma-separated: "upload,download"). Set to "" or "none" to disable.'},

    # Video & Encoder Settings
    {'name': 'encoder', 'type': 'enum', 'default': 'x264enc', 'meta': {'allowed': ['x264enc', 'x264enc-striped', 'jpeg']}, 'help': 'The default video encoder.'},
    {'name': 'framerate', 'type': 'range', 'default': '8-120', 'meta': {'default_value': 60}, 'help': 'Allowed framerate range (e.g., "8-165") or a fixed value (e.g., "60").'},
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

    # Audio Settings
    {'name': 'audio_bitrate', 'type': 'enum', 'default': '320000', 'meta': {'allowed': ['64000', '128000', '265000', '320000']}, 'help': 'The default audio bitrate.'},

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

    # Server Startup & Operational Settings
    {'name': 'port', 'type': 'int', 'default': 8082, 'env_var': 'CUSTOM_WS_PORT', 'help': 'Port for the data websocket server.'},
    {'name': 'control_port', 'type': 'int', 'default': 8083, 'help': 'Port for the internal control plane API.'},
    {'name': 'master_token', 'type': 'str', 'default': '', 'help': 'Master token to enable secure mode and protect the control plane API.'},
    {'name': 'dri_node', 'type': 'str', 'default': '', 'env_var': 'DRI_NODE', 'help': 'Path to the DRI render node for VA-API.'},
    {'name': 'audio_device_name', 'type': 'str', 'default': 'output.monitor', 'help': 'Audio device name for pcmflux capture.'},
    {'name': 'watermark_path', 'type': 'str', 'default': '', 'env_var': 'WATERMARK_PNG', 'help': 'Absolute path to the watermark PNG file.'},
    {'name': 'watermark_location', 'type': 'int', 'default': -1, 'env_var': 'WATERMARK_LOCATION', 'help': 'Watermark location enum (0-6).'},
    {'name': 'debug', 'type': 'bool', 'default': False, 'help': 'Enable debug logging.'},

    # Shared Modes
    {'name': 'enable_sharing', 'type': 'bool', 'default': True, 'help': 'Master toggle for all sharing features.'},
    {'name': 'enable_collab', 'type': 'bool', 'default': True, 'help': 'Enable collaborative (read-write) sharing link.'},
    {'name': 'enable_shared', 'type': 'bool', 'default': True, 'help': 'Enable view-only sharing links.'},
    {'name': 'enable_player2', 'type': 'bool', 'default': True, 'help': 'Enable sharing link for gamepad player 2.'},
    {'name': 'enable_player3', 'type': 'bool', 'default': True, 'help': 'Enable sharing link for gamepad player 3.'},
    {'name': 'enable_player4', 'type': 'bool', 'default': True, 'help': 'Enable sharing link for gamepad player 4.'},
]


class AppSettings:
    """
    Parses and stores application settings from command-line arguments and
    environment variables, based on a centralized definition list.
    """
    def __init__(self):
        parser = argparse.ArgumentParser(description="Selkies WebSocket Streaming Server")
        self._add_arguments(parser)
        args, _ = parser.parse_known_args()
        self._process_and_set_attributes(args)

    def _add_arguments(self, parser):
        """Programmatically add arguments to the parser from definitions."""
        for setting in SETTING_DEFINITIONS:
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
        for setting in SETTING_DEFINITIONS:
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

settings = AppSettings()

if settings.debug[0]:
    logging.getLogger().setLevel(logging.DEBUG)
    logging.getLogger("websockets").setLevel(logging.WARNING)
else:
    logging.getLogger().setLevel(logging.INFO)
    logging.getLogger("websockets").setLevel(logging.WARNING)
