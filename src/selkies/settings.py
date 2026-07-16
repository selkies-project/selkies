# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import argparse
import os
import logging
import re
import zlib
from typing import Any, Dict, List

# Settings precedence: CLI flag > SELKIES_<NAME> env > fallback env_var(s) > 'default'.
# Names derive from 'name': my_setting -> --my-setting / SELKIES_MY_SETTING.
# Special syntax:
#   - List/enum (e.g. SELKIES_ENCODER="jpeg,h264enc"): first item is default,
#     full list is the allowed options; a single value locks the choice. Invalid
#     items are dropped; an entirely-invalid override keeps the full built-in
#     menu and default.
#   - Bool (case-insensitive): "true"/"1" = on, anything else = off; a
#     "|locked" suffix (e.g. "true|locked") forbids the client changing it.
#   - Range: "8-240" restricts the allowed span (initial value = built-in
#     default, clamped in); a bare value "60" keeps the built-in span and makes
#     it the initial value, widening the span if it falls outside (so legacy
#     fixed-value configs still resolve); "60,8-240" sets initial + span in one
#     value; a degenerate span "60-60" locks the setting.
#   - An override set to "" means "use the built-in default"; list types keep
#     their explicit ""/"none" = disable semantics.

# One WebSocket message ceiling for BOTH directions: enforced on receive
# (aiohttp max_msg_size) and advertised to clients so multipart chunk sizing
# (clipboard, uploads) fills the frame on either end.
# WS_MESSAGE_SIZE_HARD_CAP is the absolute per-message bound (32 MiB): proxies
# and WebSocket stacks in the field degrade or reject frames beyond it, so the
# advertised/enforced value is clamped under it and the server refuses to emit
# any single frame above it (see _broadcast_to_clients) or to inflate a client
# 0x05 gzip frame past WS_MAX_MESSAGE_BYTES.
WS_MESSAGE_SIZE_HARD_CAP = 32 * 1024 * 1024
WS_MAX_MESSAGE_BYTES = min(8 * 1024 * 1024, WS_MESSAGE_SIZE_HARD_CAP)


def inflate_gz_bounded(payload):
    """Inflate a client gzip payload, bounded by the shared message ceiling.

    The inflated bytes stand in for a raw TEXT message, which could never
    exceed WS_MAX_MESSAGE_BYTES on either transport (aiohttp's max_msg_size on
    WebSockets, the negotiated max-message-size on the data channel), so the
    same budget applies here — an unbounded gzip.decompress would let a single
    small frame balloon ~1000x into process memory.
    Raises ValueError for a payload that inflates past the cap, is truncated,
    or does not decode as UTF-8.
    """
    d = zlib.decompressobj(wbits=31)  # gzip container
    inflated = d.decompress(payload, WS_MAX_MESSAGE_BYTES + 1)
    if len(inflated) > WS_MAX_MESSAGE_BYTES:
        raise ValueError(
            f"inflates past the {WS_MAX_MESSAGE_BYTES}-byte per-message ceiling"
        )
    if not d.eof:
        raise ValueError("truncated gzip stream")
    return inflated.decode("utf-8")

SETTING_DEFINITIONS: List[Dict[str, Any]] = [
    # -------------------- Common Settings for both modes --------------------
    # Core Feature Toggles
    {
        "name": "addr",
        "type": "str",
        "default": "0.0.0.0",
        "help": 'Host address to start the streaming service, default: "0.0.0.0"',
    },
    {
        "name": "port",
        "type": "int",
        "default": 8081,
        "min": 1,
        "max": 65535,
        'env_var': 'CUSTOM_WS_PORT',
        "help": 'Port to start the streaming service, default: "8081"',
    },
    {
        "name": "web_root",
        "type": "str",
        "default": "",
        "help": 'Path to directory containing web application files. Defaults to web files packaged with Selkies application',
    },
    {
        "name": "audio_enabled",
        "type": "bool",
        "default": True,
        "help": "Enable server-to-client audio streaming. Disabling this will also disable microphone support.",
    },
    {
        "name": "microphone_enabled",
        "type": "bool",
        "default": False,
        "help": "Enable client-to-server microphone forwarding.",
    },
    {
        "name": "gamepad_enabled",
        "type": "bool",
        "default": True,
        "help": "Enable gamepad support.",
    },
    {
        "name": "enable_clipboard",
        "type": "str",
        "default": "true",
        "help": 'Clipboard policy for both transports: "true" (both directions), "in" (client-to-server only), "out" (server-to-client only), "false" (disabled).',
    },
    {
        "name": "command_enabled",
        "type": "bool",
        "default": False,
        "help": "Enable parsing of command websocket messages. Disabled by default for security; opt in with SELKIES_COMMAND_ENABLED=true (or --command-enabled true).",
    },
    {
        "name": "file_transfers",
        "type": "list",
        "default": "upload,download",
        "meta": {"allowed": ["upload", "download"]},
        "help": 'Allowed file transfer directions (comma-separated: "upload,download"). Set to "" or "none" to disable.',
    },
    {
        "name": "framerate",
        "type": "range",
        "default": "8-240",
        "meta": {"default_value": 60},
        "help": 'Framerate: allowed range (e.g., "8-240"), initial value (e.g., "60"), or both ("60,8-240"); "60-60" locks.',
    },
    {
        "name": "video_crf",
        "type": "range",
        "default": "5-50",
        "meta": {"default_value": 25},
        "help": 'Video CRF (constant quality): allowed range (e.g., "5-50"), initial value (e.g., "25"), or both ("25,5-50"); "25-25" locks.',
    },
    {
        "name": "video_bitrate",
        "type": "range",
        "default": "0.1-1000",
        "meta": {"default_value": 8},
        "help": 'Video bitrate aka CBR, in Megabits per second (Mbps): allowed range (e.g., "0.1-1000"), initial value (e.g., "8" for 8 Mbps, "0.25" for 250 Kbps), or both ("8,0.1-1000"); "8-8" locks.',
    },
    {
        "name": "rate_control_mode",
        "type": "enum",
        "default": "crf",
        # CBR listed first purely for dropdown order; the default stays "crf" and
        # allowed[0] is never used as a fallback for this setting (clients only
        # ever send crf/cbr, both valid), so ordering is display-only here.
        "meta": {"allowed": ["cbr", "crf"]},
        "help": "Rate control mode for the H.264 encoders (crf = constant quality/QP, cbr = constant bitrate). Honored for every H.264 encoder when enable_rate_control is true (the default).",
    },
    {
        "name": "enable_rate_control",
        "type": "bool",
        "default": True,
        "help": "Honor the client-selected rate_control_mode (crf/cbr). Enabled by default so both modes are selectable; set false to lock the encoder to its built-in default.",
    },
    {
        "name": "keyframe_interval",
        "type": "float",
        "default": 0.0,
        "min": 0.0,
        "max": 300.0,
        "help": "Seconds between scheduled video recovery keyframes (any video codec). 0 (default) keeps the GOP infinite: keyframes are sent only on demand (client join/reset, keyframe requests), which keeps bitrate and quality steady.",
    },
    {
        "name": "video_min_qp",
        "type": "int",
        "default": 0,
        "min": 0,
        "max": 51,
        "help": "CBR-mode minimum H.264 QP (0 = encoder default). Raising it caps bit spend on easy content when the bitrate budget is generous.",
    },
    {
        "name": "video_max_qp",
        "type": "int",
        "default": 0,
        "min": 0,
        "max": 51,
        "help": "CBR-mode maximum H.264 QP (0 = encoder default). Lowering it keeps screen text legible under motion at the cost of overshooting the bitrate target on hard content (measured at 720p60 scrolling text: 35 lifts x264 by ~19 dB at ~2.5x the target).",
    },
    # Audio Settings
    {
        "name": "audio_frame_duration_ms",
        "type": "enum",
        "default": "10",
        "meta": {"allowed": ["2.5", "5", "10", "20", "40", "60"]},
        "help": "Opus frame duration in milliseconds for server-to-client audio. Lower values cut audio latency (each frame must fill before it can be sent, and the client buffers a fixed number of frames) at a small bitrate-efficiency and packet-rate cost. On WebRTC the SDP ptime/minptime follow this value.",
    },
    {
        "name": "audio_bitrate",
        "type": "enum",
        "default": "128000",
        # Curated dropdown stops for the web UI; 510000 is libopus's hard maximum
        # (not 512k). value_range lets the SERVER (env/CLI) accept any Opus bitrate
        # in 6000-510000 bps verbatim, while the UI keeps offering only the stops.
        "meta": {
            "allowed": ["32000", "48000", "64000", "96000", "128000", "192000", "256000", "320000", "384000", "510000"],
            "value_range": [6000, 510000],
        },
        "help": "The default audio bitrate.",
    },
    {
        "name": "audio_redundancy",
        "type": "bool",
        "default": True,
        "help": "Enable Opus RED (RFC 2198) audio redundancy to cut dropouts/concealment under packet loss. On by default; carries prior frames as redundancy on WebRTC (browsers de-RED natively, plain-opus fallback for peers that decline) and, on WebSocket, is gated on every client supporting it.",
    },
    {
        "name": "audio_redundancy_distance",
        "type": "int",
        "default": 2,
        "min": 0,
        "max": 4,
        "help": "Number of prior Opus frames carried as RED redundancy when audio_redundancy is enabled (0-4; higher survives longer loss bursts at proportionally more bandwidth).",
    },
    # Display & Resolution Settings
    {
        "name": "is_manual_resolution_mode",
        "type": "bool",
        "default": False,
        "help": "Lock the resolution to the manual width/height values.",
    },
    {
        "name": "manual_width",
        "type": "int",
        "default": 0,
        # Ceiling is the common GPU/X framebuffer limit, not 8K, so >8K manual
        # resolutions still drive xrandr --fb without being silently clamped.
        "meta": {"min": 0, "max": 16384},
        "help": "Lock width to a fixed value. Setting this forces manual resolution mode.",
    },
    {
        "name": "manual_height",
        "type": "int",
        "default": 0,
        # Ceiling is the common GPU/X framebuffer limit, not 8K, so >8K manual
        # resolutions still drive xrandr --fb without being silently clamped.
        "meta": {"min": 0, "max": 16384},
        "help": "Lock height to a fixed value. Setting this forces manual resolution mode.",
    },
    {
        "name": "scaling_dpi",
        "type": "enum",
        "default": "96",
        "meta": {
            "allowed": ["96", "120", "144", "168", "192", "216", "240", "264", "288"]
        },
        "help": "The default DPI for UI scaling.",
    },
    {
        "name": "force_aligned_resolution",
        "type": "bool",
        "default": False,
        "help": "Forces the display resolution to be a multiple of 16 pixels.",
    },
    # Input & Client Behavior Settings
    {
        "name": "enable_binary_clipboard",
        "type": "bool",
        "default": True,
        "help": "Allow binary data (e.g., images) on the clipboard.",
    },
    {
        "name": "use_browser_cursors",
        "type": "bool",
        "default": True,
        "help": "Use browser CSS cursors instead of rendering to canvas.",
    },
    {
        "name": "use_css_scaling",
        "type": "bool",
        "default": False,
        "help": "HiDPI when false, if true a lower resolution is sent from the client and the canvas is stretched.",
    },
    # UI Visibility Settings
    {
        "name": "ui_title",
        "type": "str",
        "default": "Selkies",
        "help": "Title in top left corner of sidebar.",
    },
    {
        "name": "ui_show_logo",
        "type": "bool",
        "default": True,
        "help": "Show the Selkies logo in the sidebar.",
    },
    {
        "name": "ui_show_core_buttons",
        "type": "bool",
        "default": True,
        "help": "Show the core components buttons display, audio, microphone, and gamepad.",
    },
    {
        "name": "ui_show_sidebar",
        "type": "bool",
        "default": True,
        "help": "Show the main sidebar UI.",
    },
    {
        "name": "ui_sidebar_show_video_settings",
        "type": "bool",
        "default": True,
        "help": "Show the video settings section in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_screen_settings",
        "type": "bool",
        "default": True,
        "help": "Show the screen settings section in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_audio_settings",
        "type": "bool",
        "default": True,
        "help": "Show the audio settings section in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_stats",
        "type": "bool",
        "default": True,
        "help": "Show the stats section in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_clipboard",
        "type": "bool",
        "default": True,
        "help": "Show the clipboard section in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_files",
        "type": "bool",
        "default": True,
        "help": "Show the file transfer section in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_apps",
        "type": "bool",
        "default": True,
        "help": "Show the applications section in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_sharing",
        "type": "bool",
        "default": True,
        "help": "Show the sharing section in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_gamepads",
        "type": "bool",
        "default": True,
        "help": "Show the gamepads section in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_fullscreen",
        "type": "bool",
        "default": True,
        "help": "Show the fullscreen button in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_gaming_mode",
        "type": "bool",
        "default": True,
        "help": "Show the gaming mode button in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_trackpad",
        "type": "bool",
        "default": True,
        "help": "Show the virtual trackpad button in the sidebar.",
    },
    {
        "name": "ui_sidebar_show_keyboard_button",
        "type": "bool",
        "default": True,
        "help": "Show the on-screen keyboard button in the display area.",
    },
    {
        "name": "ui_sidebar_show_soft_buttons",
        "type": "bool",
        "default": True,
        "help": "Show the soft buttons section in the sidebar.",
    },
    # Shared Modes
    {
        "name": "enable_sharing",
        "type": "bool",
        "default": True,
        "help": "Master toggle for all sharing features.",
    },
    {
        "name": "enable_collab",
        "type": "bool",
        "default": True,
        "help": "Enable collaborative (read-write) sharing link.",
    },
    {
        "name": "enable_shared",
        "type": "bool",
        "default": True,
        "help": "Enable view-only sharing links.",
    },
    {
        "name": "enable_player2",
        "type": "bool",
        "default": True,
        "help": "Enable sharing link for gamepad player 2.",
    },
    {
        "name": "enable_player3",
        "type": "bool",
        "default": True,
        "help": "Enable sharing link for gamepad player 3.",
    },
    {
        "name": "enable_player4",
        "type": "bool",
        "default": True,
        "help": "Enable sharing link for gamepad player 4.",
    },
    {
        "name": "debug",
        "type": "bool",
        "default": False,
        "help": "Enable debug logging.",
    },
    {
        "name": "mode",
        "type": "str",
        "default": "websockets",
        "help": "Specify the mode: 'webrtc' or 'websockets'; defaults to websockets",
    },
    {
        "name": "enable_dual_mode",
        "type": "bool",
        "default": True,
        "help": "Enable switching Streaming modes from UI",
    },
    {
        "name": "audio_device_name",
        "type": "str",
        "default": "output.monitor",
        "help": "Audio device name for pcmflux capture.",
    },
    {
        "name": "master_token",
        "type": "str",
        "default": "",
        "help": "Master token to enable secure mode and protect the control plane API.",
    },
    {
        "name": "enable_https",
        "type": "bool",
        "default": False,
        "help": "Enable or disable HTTPS for the web application, specifying a valid server certificate is recommended",
    },
    {
        "name": "https_cert",
        "type": "str",
        "default": "/etc/ssl/certs/ssl-cert-snakeoil.pem",
        "help": "Path to the TLS server certificate file when HTTPS is enabled",
    },
    {
        "name": "https_key",
        "type": "str",
        "default": "/etc/ssl/private/ssl-cert-snakeoil.key",
        "help": "Path to the TLS server private key file when HTTPS is enabled, set to an empty value if the private key is included in the certificate",
    },
    {
        "name": "cert_reload_interval",
        "type": "int",
        "default": 30,
        "min": 0,
        "help": "Seconds between checks for SSL certificate file changes when HTTPS is enabled, set to 0 to disable automatic certificate reloading",
    },
    {
        "name": "enable_basic_auth",
        "type": "bool",
        "default": True,
        "help": "Enable basic authentication on the server. On by default with the default credentials; set --basic_auth_password (a warning is logged while the well-known default password is in use).",
    },
    {
        "name": "basic_auth_user",
        "type": "str",
        "default": "ubuntu",
        "env_var": ["CUSTOM_USER", "USERNAME", "USER"],
        "help": 'Username for basic authentication; resolves from the CUSTOM_USER, then USERNAME, then USER environment variables, and defaults to "ubuntu" when none is set.',
    },
    {
        "name": "basic_auth_password",
        "type": "str",
        "default": "mypasswd",
        "env_var": "PASSWORD",
        "help": "Password used when basic authentication is set",
    },
    {
        "name": "basic_auth_viewonly_password",
        "type": "str",
        "default": "",
        "env_var": "VIEWONLY_PASSWORD",
        "help": "Optional second basic-auth password that grants view-only access. Clients authenticating with it are capped at the viewer role (no keyboard, mouse, clipboard, gamepad, or command input) regardless of the role they request, while the main password authorizes full control. Empty disables the split. Ignored in secure mode, where the master token governs roles.",
    },
    {
        "name": "subfolder",
        "type": "str",
        "default": "",
        "env_var": "SUBFOLDER",
        "help": 'URL path prefix the server is reverse-proxied under; prepended to every route (websockets, tokens, metrics, static files). Needs both slashes, e.g. "/subfolder/".',
    },
    {
        "name": "run_after_connect",
        "type": "str",
        "default": "",
        "help": "Shell command run after the first client has connected ('' = off); runs again each time a client connects while no others are connected.",
    },
    {
        "name": "run_after_disconnect",
        "type": "str",
        "default": "",
        "help": "Shell command run after the last client has disconnected ('' = off), including on server shutdown while clients are connected.",
    },
    # -------------------- WEBSOCKETS Settings --------------------
    # Video & Encoder
    {
        "name": "encoder",
        "type": "enum",
        "default": "h264enc",
        "meta": {"allowed": ["h264enc", "h264enc-striped", "openh264enc", "jpeg"]},
        "help": "The default video encoder.",
    },
    {
        "name": "jpeg_quality",
        "type": "range",
        "default": "1-100",
        "meta": {"default_value": 40},
        "help": 'JPEG quality: allowed range (e.g., "1-100"), initial value (e.g., "40"), or both ("40,1-100"); "40-40" locks.',
    },
    {
        "name": "video_fullcolor",
        "type": "bool",
        "default": False,
        "help": "Enable H.264 full color range for pixelflux encoders.",
    },
    {
        "name": "video_streaming_mode",
        "type": "bool",
        "default": True,
        "help": "Enable H.264 streaming mode (Turbo: encode every frame like a traditional video encoder) for pixelflux encoders.",
    },
    {
        "name": "use_cpu",
        "type": "bool",
        "default": False,
        "help": "Force CPU-based encoding for pixelflux.",
    },
    {
        "name": "use_paint_over_quality",
        "type": "bool",
        "default": True,
        "help": "Enable high-quality paint-over for static scenes.",
    },
    {
        "name": "paint_over_jpeg_quality",
        "type": "range",
        "default": "1-100",
        "meta": {"default_value": 90},
        "help": 'JPEG paint-over quality: allowed range, initial value, or both ("90,1-100"); "90-90" locks.',
    },
    {
        "name": "video_paintover_crf",
        "type": "range",
        "default": "5-50",
        "meta": {"default_value": 18},
        "help": 'H.264 paint-over CRF: allowed range, initial value, or both ("18,5-50"); "18-18" locks.',
    },
    {
        "name": "video_paintover_burst_frames",
        "type": "range",
        "default": "1-30",
        "meta": {"default_value": 5},
        "help": 'H.264 paint-over burst frames: allowed range, initial value, or both ("5,1-30"); "5-5" locks.',
    },
    {
        "name": "second_screen",
        "type": "bool",
        "default": True,
        "help": "Enable support for a second monitor/display.",
    },
    # Server Startup & Operational Settings
    {
        "name": "encode_dri",
        "type": "str",
        "default": "",
        "env_var": "DRI_NODE",
        "help": "Path to the DRI render node the ENCODER uses (VA-API/NVENC device selection).",
    },
    {
        "name": "render_dri",
        "type": "str",
        "default": "",
        "env_var": "DRINODE",
        "help": "Path to the DRI render node the Wayland compositor RENDERS on (defaults to auto_gpu selection, else software rendering).",
    },
    {
        "name": "auto_gpu",
        "type": "str",
        "default": "true",
        "env_var": "AUTO_GPU",
        "help": 'GPU auto-selection for rendering, enabled by default: "true" picks the first GPU; "false" disables it; otherwise a case-insensitive token picks the first GPU it matches — a vendor name (nvidia, amd/ati, intel, arm/mali, qualcomm/adreno, broadcom/videocore, apple, imagination/powervr, vmware, virtio, ...), a kernel driver name (amdgpu, i915, xe, nouveau, panfrost, msm, v3d, ...), a devicetree vendor prefix (qcom, rockchip, brcm, ...), or a raw PCI vendor ID (0x10de).',
    },
    {
        "name": "wayland",
        "type": "bool",
        "default": False,
        "env_var": "PIXELFLUX_WAYLAND",
        "help": "Run the Wayland (headless compositor) backend instead of X11 capture/input.",
    },
    {
        "name": "recording_socket",
        "type": "str",
        "default": "",
        "env_var": "PIXELFLUX_RECORDING_SOCKET",
        "help": "Unix socket path for the out-of-band H.264 recording tap ('' = off); pixelflux binds it and multiplexes the elementary stream to connected clients.",
    },
    {
        "name": "file_manager_path",
        "type": "str",
        "default": "~/Desktop",
        "env_var": "FILE_MANAGER_PATH",
        "help": "Directory for client file transfers on both transports: uploads land here and the file-browser/download API serves it (created at startup if missing).",
    },
    {
        "name": "watermark_path",
        "type": "str",
        "default": "",
        "env_var": "WATERMARK_PNG",
        "help": "Absolute path to the watermark PNG file.",
    },
    {
        "name": "watermark_location",
        "type": "int",
        "default": -1,
        "env_var": "WATERMARK_LOCATION",
        "help": "Watermark location enum (0-6).",
    },
    {
        "name": "wayland_socket_index",
        "type": "int",
        "default": 0,
        "min": 0,
        "help": "Index for the Wayland command socket (e.g. 0 for wayland-0).",
    },
    # -------------------- WEBRTC Settings --------------------
    {
        "name": "rtc_config_json",
        "type": "str",
        "default": "/tmp/rtc.json",
        "help": "JSON file with WebRTC configuration to use, checked periodically, overriding all other STUN/TURN settings",
    },
    # TURN/STUN
    {
        "name": "turn_rest_uri",
        "type": "str",
        "default": "",
        "help": "URI for TURN REST API service, example: http://localhost:8008",
    },
    {
        "name": "turn_rest_api_key",
        "type": "str",
        "default": "",
        "help": "API key to pass to the TURN REST API service",
    },
    {
        "name": "turn_rest_username",
        "type": "str",
        "default": "",
        "help": "Username sent to the TURN REST API service (x-auth-user header); the service embeds it in the HMAC credential. Empty (default) uses the generic 'selkies'.",
    },
    {
        "name": "turn_rest_username_auth_header",
        "type": "str",
        "default": "x-auth-user",
        "help": "Header to pass user to TURN REST API service",
    },
    {
        "name": "turn_rest_protocol_header",
        "type": "str",
        "default": "x-turn-protocol",
        "help": "Header to pass desired TURN protocol to TURN REST API service",
    },
    {
        "name": "turn_rest_tls_header",
        "type": "str",
        "default": "x-turn-tls",
        "help": "Header to pass TURN (D)TLS usage to TURN REST API service",
    },
    {
        "name": "turn_host",
        "type": "str",
        "default": "staticauth.openrelay.metered.ca",
        "help": "TURN host when generating RTC config from shared secret or using long-term credentials, IPv6 addresses must be enclosed with square brackets such as [::1]",
    },
    {
        "name": "turn_port",
        "type": "int",
        "default": 443,
        "min": 1,
        "max": 65535,
        "help": "TURN port when generating RTC config from shared secret or using long-term credentials",
    },
    {
        "name": "turn_protocol",
        "type": "str",
        "default": "udp",
        "help": 'TURN protocol for the client to use ("udp" or "tcp"), set to "tcp" without the quotes if "udp" is blocked on the network, "udp" is otherwise strongly recommended',
    },
    {
        "name": "turn_tls",
        "type": "bool",
        "default": False,
        "help": "Enable or disable TURN over TLS (for the TCP protocol) or TURN over DTLS (for the UDP protocol), valid TURN server certificate required",
    },
    {
        "name": "turn_shared_secret",
        "type": "str",
        "default": "openrelayprojectsecret",
        "help": "Shared TURN secret used to generate HMAC credentials, also requires --turn_host and --turn_port",
    },
    {
        "name": "turn_username",
        "type": "str",
        "default": "",
        "help": "Legacy non-HMAC TURN credential username, also requires --turn_host and --turn_port",
    },
    {
        "name": "turn_password",
        "type": "str",
        "default": "",
        "help": "Legacy non-HMAC TURN credential password, also requires --turn_host and --turn_port",
    },
    {
        "name": "stun_host",
        "type": "str",
        "default": "stun.l.google.com",
        "help": 'STUN host for NAT hole punching with WebRTC, change to your internal STUN/TURN server for local networks without internet, defaults to "stun.l.google.com"',
    },
    {
        "name": "stun_port",
        "type": "int",
        "default": 19302,
        "min": 1,
        "max": 65535,
        "help": 'STUN port for NAT hole punching with WebRTC, change to your internal STUN/TURN server for local networks without internet, defaults to "19302"',
    },
    {
        "name": "webrtc_public_ip",
        "type": "str",
        "default": "",
        "help": 'Public IPv4 address to advertise in WebRTC host ICE candidates (Pion-style NAT1TO1), for a host behind static 1:1 NAT (e.g. a cloud VM with an elastic IP where all UDP ports map through). When set, the private host-candidate address in each SDP offer is replaced with this IP so a remote peer can reach the host candidate directly; server-reflexive (STUN) and relay (TURN) candidates are left untouched, so hole-punching and TURN fallback still work. Empty (default) keeps the gathered private address unchanged.',
    },
    {
        "name": "enable_cloudflare_turn",
        "type": "bool",
        "default": False,
        "help": "Enable Cloudflare TURN service, requires SELKIES_CLOUDFLARE_TURN_TOKEN_ID, and SELKIES_CLOUDFLARE_TURN_API_TOKEN",
    },
    {
        "name": "cloudflare_turn_token_id",
        "type": "str",
        "default": "",
        "help": "The Cloudflare TURN App token ID.",
    },
    {
        "name": "cloudflare_turn_api_token",
        "type": "str",
        "default": "",
        "help": "The Cloudflare TURN API token.",
    },
    {
        "name": "encoder_rtc",
        "type": "enum",
        "default": "h264enc",
        # Only encoders the pipeline can actually PRODUCE may be listed (pixelflux emits
        # H.264 only; other codecs are a future addition — the vendored webrtc stack keeps
        # its VP8/RTP code for that). h264enc is hardware-first (NVENC/VA-API when present,
        # else software x264 — same behavior as the WS path); openh264enc stays a separate
        # software choice.
        "meta": {"allowed": ["h264enc", "openh264enc"]},
        "help": "Video encoder to encode video media",
    },
    {
        "name": "app_wait_ready",
        "type": "bool",
        "default": False,
        "help": 'Waits for --app_ready_file to exist before starting stream if set to "true"',
    },
    {
        "name": "app_ready_file",
        "type": "str",
        "default": "/tmp/selkies-appready",
        "help": "File set by sidecar used to indicate that app is initialized and ready",
    },
    {
        "name": "uinput_mouse_socket",
        "type": "str",
        "default": "",
        "help": "Path to the uinput mouse socket, if not provided uinput is used directly",
    },
    {
        "name": "js_socket_path",
        "type": "str",
        "default": "/tmp",
        "help": "Directory to write the Selkies Joystick Interposer communication sockets to, default: /tmp, results in socket files: /tmp/selkies_js{0-3}.sock",
    },
    {
        "name": "gpu_id",
        "type": "str",
        "default": "",
        "help": "GPU ID for hardware video encoders: selects /dev/dri/renderD{128 + n} and the GPU-stats index. Empty (default) sets no explicit pick, encoding on ID 0 — the first GPU — or on the GPU chosen by --auto-gpu; -1 disables hardware encoding. Ignored when --encode-dri specifies a device path.",
    },
    {
        "name": "congestion_control",
        "type": "bool",
        "default": False,
        "help": "Adapt the video bitrate to the transport-wide-cc (GCC-style) bandwidth estimate from WebRTC receiver feedback. Effective in CBR rate-control mode; may trade quality/stability for congestion responsiveness.",
    },
    {
        "name": "audio_channels",
        "type": "int",
        "default": 2,
        "min": 1,
        "help": "Number of audio channels, defaults to stereo (2 channels)",
    },
    {
        "name": "enable_resize",
        "type": "bool",
        "default": True,
        "help": "Enable dynamic resizing to match browser size",
    },
    {
        "name": "enable_cursors",
        "type": "bool",
        "default": True,
        "help": "Enable passing remote cursors to client",
    },
    {
        "name": "debug_cursors",
        "type": "bool",
        "default": False,
        "help": "Enable cursor debug logging",
    },
    {
        "name": "cursor_size",
        "type": "int",
        "default": -1,
        "env_var": "XCURSOR_SIZE",
        "help": "Cursor size in points at 96 DPI (scaled with the session DPI). Applies to the X11 server cursor, the Wayland compositor cursor theme, and the remote-cursor capture cap on both transports; -1 uses the platform default (32 on X11, 24 on Wayland).",
    },
    {
        "name": "enable_webrtc_statistics",
        "type": "bool",
        "default": False,
        "help": "Enable WebRTC Statistics CSV dumping to the directory --webrtc_statistics_dir with filenames selkies-stats-video-[timestamp].csv and selkies-stats-audio-[timestamp].csv",
    },
    {
        "name": "webrtc_statistics_dir",
        "type": "str",
        "default": "/tmp",
        "help": "Directory to save WebRTC Statistics CSV from client with filenames selkies-stats-video-[timestamp].csv and selkies-stats-audio-[timestamp].csv",
    },
    {
        "name": "enable_metrics_http",
        "type": "bool",
        "default": False,
        "help": "Enable the Prometheus HTTP /metrics endpoint.",
    },
    {
        "name": "backpressure_queue_size",
        "type": "int",
        "default": 120,
        "min": 1,
        "max": 100000,
        "help": "Max frames/audio chunks buffered per stream before dropping under backpressure (WebSockets mode). Higher tolerates larger client hiccups at the cost of latency.",
    },
    {
        "name": "allowed_origins",
        "type": "str",
        "default": "",
        "help": "Comma-separated browser Origins allowed to open the streaming WebSocket (cross-site WebSocket-hijacking guard). Empty (default) allows only same-origin plus non-browser clients that send no Origin; use '*' to allow any origin.",
    },
]

# Secret/credential settings, flagged 'sensitive' so consumers exclude them from
# client broadcasts. Add new secrets here.
SENSITIVE_SETTING_NAMES = frozenset({
    "master_token",
    "https_key",
    "basic_auth_user",
    "basic_auth_password",
    "basic_auth_viewonly_password",
    "turn_rest_api_key",
    "turn_shared_secret",
    "turn_password",
    "cloudflare_turn_token_id",
    "cloudflare_turn_api_token",
})
for _setting_def in SETTING_DEFINITIONS:
    if _setting_def["name"] in SENSITIVE_SETTING_NAMES:
        _setting_def["sensitive"] = True


def _range_number(text):
    """A range-setting number: int when integral, float otherwise (so
    sub-unit values like a 0.25 Mbps bitrate are representable)."""
    value = float(text)
    return int(value) if value.is_integer() else value


class AppSettings:
    """
    Parses and stores application settings from command-line arguments and
    environment variables, based on a centralized definition list.
    """

    def __init__(self, setting):
        parser = argparse.ArgumentParser(
            description="Selkies WebSocket Streaming Server"
        )
        self._setting_definitions = setting
        self._add_arguments(parser)
        args, _ = parser.parse_known_args()
        self._process_and_set_attributes(args)
        self._post_process_settings()

    @staticmethod
    def _fallback_env_vars(setting):
        """A setting's fallback env aliases as a tuple: `env_var` may be one
        name or an ordered list of names (earlier entries win)."""
        fallback = setting.get("env_var")
        if not fallback:
            return ()
        if isinstance(fallback, str):
            return (fallback,)
        return tuple(fallback)

    def _add_arguments(self, parser):
        """Programmatically add arguments to the parser from definitions."""
        for setting in self._setting_definitions:
            name = setting["name"]
            cli_flag = f"--{name.replace('_', '-')}"
            standard_env_var = f"SELKIES_{name.upper()}"
            fallback_env_vars = self._fallback_env_vars(setting)
            env_help_text = f"Env: {standard_env_var}"
            if fallback_env_vars:
                env_help_text = f"Env: {standard_env_var} (or {', '.join(fallback_env_vars)})"
            parser.add_argument(
                cli_flag,
                type=str,
                default=None,
                help=f"{setting['help']} ({env_help_text})",
            )

    def _process_and_set_attributes(self, args):
        """Process parsed arguments and set them as class attributes."""
        processed = {}
        overrides = {}
        for setting in self._setting_definitions:
            name = setting["name"]
            stype = setting["type"]
            cli_val = getattr(args, name, None)
            std_env_val = os.environ.get(f"SELKIES_{name.upper()}")
            fallback_env_val = None
            for fallback_var in self._fallback_env_vars(setting):
                fallback_env_val = os.environ.get(fallback_var)
                if fallback_env_val is not None:
                    break
            is_override = (
                cli_val is not None
                or std_env_val is not None
                or fallback_env_val is not None
            )
            overrides[name] = is_override

            raw_value = (
                cli_val
                if cli_val is not None
                else (
                    std_env_val
                    if std_env_val is not None
                    else (
                        fallback_env_val
                        if fallback_env_val is not None
                        else setting["default"]
                    )
                )
            )
            # An override set to the empty string means "use the built-in
            # default": it lets images neutralize envs baked into a base layer
            # (ENV SELKIES_FOO=) without guessing each default here. list-type
            # settings keep their explicit ""/none = disable semantics.
            if (
                is_override
                and stype != "list"
                and str(raw_value).strip() == ""
            ):
                is_override = False
                overrides[name] = False
                raw_value = setting["default"]
            processed_value = None
            try:
                if stype == "bool":
                    parts = [
                        part.strip()
                        for part in str(raw_value).strip().lower().split("|")
                    ]
                    is_locked = "locked" in parts[1:]
                    bool_value = parts[0] in ["true", "1"]
                    processed_value = (bool_value, is_locked)
                elif stype in ["enum", "list"]:
                    if is_override:
                        master_list = setting.get("meta", {}).get("allowed", [])
                        raw_value_str = str(raw_value)
                        if stype == "list" and raw_value_str.strip().lower() in ("", "none"):
                            # Disable list-type settings if set to "" or "none"
                            setting["meta"]["allowed"] = []
                            processed_value = []
                        else:
                            user_items = [item.strip() for item in raw_value_str.split(',') if item.strip()]
                            valid_items = [item for item in user_items if item in master_list]
                            # A numeric enum may declare meta.value_range: an admin-set
                            # single value inside that span is taken verbatim as the
                            # server value while the curated `allowed` stops are kept for
                            # the web UI (the server accepts more than the UI offers).
                            vr = setting.get("meta", {}).get("value_range")
                            in_range_value = None
                            # Gate on value_range presence, NOT on `not valid_items`:
                            # a single in-range value that happens to equal a curated
                            # stop must still keep the full menu (the whole point of
                            # value_range is that the server accepts more than the UI
                            # shows). Excluding on-stop values collapsed the dropdown
                            # to that one option.
                            if stype == "enum" and vr and len(user_items) == 1:
                                try:
                                    n = float(user_items[0])
                                    if vr[0] <= n <= vr[1]:
                                        # Normalize to canonical integer form so a
                                        # decimal-formatted override ('128000.0')
                                        # can't crash downstream int() consumers.
                                        in_range_value = (
                                            str(int(n)) if n == int(n) else user_items[0]
                                        )
                                except ValueError:
                                    pass
                            if in_range_value is not None:
                                processed_value = in_range_value  # allowed stays the curated stops
                            elif valid_items:
                                setting["meta"]["allowed"] = valid_items
                                processed_value = valid_items[0] if stype == "enum" else valid_items
                            else:
                                # Entirely-invalid override (e.g. a stale env
                                # baked into a container image): no restriction —
                                # full allowed list, built-in default.
                                if user_items:
                                    logging.warning(
                                        f"Invalid value(s) '{raw_value_str}' for {name}; "
                                        f"keeping the full allowed set with the system default."
                                    )
                                default_str = str(setting["default"])
                                default_items = [item.strip() for item in default_str.split(',') if item.strip() and item.strip() in master_list]
                                if stype == "enum":
                                    processed_value = default_items[0] if default_items else setting["default"]
                                else:
                                    processed_value = default_items
                    else:
                        if stype == "enum":
                            processed_value = setting["default"]
                        else:
                            processed_value = [
                                item.strip()
                                for item in str(setting["default"]).split(",")
                                if item.strip()
                            ]
                elif stype in ("int", "float"):
                    processed_value = int(raw_value) if stype == "int" else float(raw_value)
                    # Clamp to bounds from top-level ("min"/"max") or "meta" (top-level
                    # wins); only when declared, so -1/negative sentinels are preserved.
                    meta = setting.get("meta") or {}
                    lo = setting.get("min", meta.get("min"))
                    hi = setting.get("max", meta.get("max"))
                    orig = processed_value
                    if lo is not None:
                        processed_value = max(lo, processed_value)
                    if hi is not None:
                        processed_value = min(hi, processed_value)
                    if processed_value != orig:
                        logging.warning(
                            f"Setting '{name}' value {orig} out of range [{lo},{hi}], clamped to {processed_value}"
                        )
                elif stype == "str":
                    processed_value = str(raw_value)
                elif stype == "range":
                    # "min-max" sets the allowed span; a bare value sets the
                    # initial value on the built-in span, widening it when it
                    # falls outside (fixed-value configs must stay valid).
                    # "60,8-240" sets both; a degenerate span ("60-60") locks.
                    tokens = [
                        token.strip()
                        for token in str(raw_value).split(",")
                        if token.strip()
                    ]
                    span = None
                    initial = None
                    for token in tokens:
                        span_match = re.fullmatch(
                            r"(-?\d+(?:\.\d+)?)\s*-\s*(-?\d+(?:\.\d+)?)", token
                        )
                        if span_match:
                            span = (
                                _range_number(span_match.group(1)),
                                _range_number(span_match.group(2)),
                            )
                        else:
                            initial = _range_number(token)
                    if span is None and initial is None:
                        raise ValueError("no span or value given")
                    meta = setting.get("meta")
                    if span is None:
                        def_lo, def_hi = sorted(
                            _range_number(part)
                            for part in str(setting["default"]).split("-", 1)
                        )
                        processed_value = (
                            min(def_lo, initial),
                            max(def_hi, initial),
                        )
                        if meta is not None:
                            meta["default_value"] = initial
                    else:
                        # Normalize an inverted span ("100-1").
                        lo, hi = sorted(span)
                        processed_value = (lo, hi)
                        if meta is not None and initial is not None:
                            clamped = max(lo, min(initial, hi))
                            if clamped != initial:
                                logging.warning(
                                    f"Setting '{name}' initial value {initial} "
                                    f"outside span {lo}-{hi}, clamped to {clamped}"
                                )
                            meta["default_value"] = clamped
                        elif meta is not None and "default_value" in meta:
                            meta["default_value"] = max(
                                lo, min(meta["default_value"], hi)
                            )
            except (ValueError, TypeError, IndexError) as e:
                logging.error(
                    f"Could not parse setting '{name}' with value '{raw_value}'. Using default. Error: {e}"
                )
                processed_value = setting["default"]
                if stype == "range":
                    min_val, max_val = (
                        _range_number(part)
                        for part in str(processed_value).split("-", 1)
                    )
                    processed_value = (min_val, max_val)
            processed[name] = processed_value
        # A manual dimension activates manual mode only when it is a POSITIVE value.
        # 0 is both the built-in default and the "no manual width" sentinel, so a
        # templated launcher emitting `--manual-width 0` for an unset field must not
        # silently lock the display to 1024x768. (An explicit is_manual_resolution_mode
        # override still forces manual mode via manual_mode_bool_is_set below.)
        width_overridden = overrides.get("manual_width", False) and processed.get("manual_width", 0) > 0
        height_overridden = overrides.get("manual_height", False) and processed.get("manual_height", 0) > 0
        manual_mode_bool_is_set = processed.get(
            "is_manual_resolution_mode", (False, False)
        )[0]
        should_be_in_manual_mode = (
            width_overridden or height_overridden or manual_mode_bool_is_set
        )
        if should_be_in_manual_mode:
            logging.info(
                "A manual resolution setting was activated; locking to manual mode."
            )
            processed["is_manual_resolution_mode"] = (True, True)
            if processed.get("manual_width", 0) <= 0:
                processed["manual_width"] = 1024
                logging.info("Manual width not set or invalid, defaulting to 1024.")
            if processed.get("manual_height", 0) <= 0:
                processed["manual_height"] = 768
                logging.info("Manual height not set or invalid, defaulting to 768.")
        for name, value in processed.items():
            setattr(self, name, value)
        # Which settings were explicitly given (CLI/env), for default
        # resolution that depends on other settings (e.g. rate control).
        self._overridden = overrides

    # Rate-control default per encoder: the striped software encoder and jpeg
    # are quality-driven (CRF), the single-slice software encoders target a
    # bandwidth (CBR). An explicit rate_control_mode override wins; encoders
    # not listed keep the built-in default.
    ENCODER_RC_DEFAULTS = {
        "h264enc": "cbr",
        "openh264enc": "cbr",
        "h264enc-striped": "crf",
        "jpeg": "crf",
    }

    def _post_process_settings(self):
        """Additional processing of config data after initial parsing."""
        if not self._overridden.get("rate_control_mode"):
            active_encoder = (
                self.encoder if self.mode == "websockets" else self.encoder_rtc
            )
            self.rate_control_mode = self.ENCODER_RC_DEFAULTS.get(
                active_encoder, self.rate_control_mode
            )

        audio_enabled = self.audio_enabled[0]
        if not audio_enabled and self.microphone_enabled[0]:
            logging.warning(
                "Microphone support requires audio to be enabled. Disabling microphone support."
            )
            self.microphone_enabled = (False, self.microphone_enabled[1])

        # The single clipboard policy knob for both transports; normalize so
        # every consumer sees exactly one of the four values.
        mode = str(self.enable_clipboard).split("|")[0].strip().lower()
        if mode not in ("true", "false", "in", "out"):
            logging.warning(
                "Invalid enable_clipboard value %r; using 'true'.", self.enable_clipboard
            )
            mode = "true"
        self.enable_clipboard = mode

        # Username embedded in the TURN credential (the REST service's x-auth-user and
        # the HMAC credential alike). A generic default keeps it stable and non-empty
        # ('<expiry>:selkies') instead of a bare '<expiry>:' or a volatile pod hostname.
        if not self.turn_rest_username:
            self.turn_rest_username = "selkies"

settings = AppSettings(SETTING_DEFINITIONS)

# Settings never broadcast to clients: server-local paths and lifecycle hooks.
CLIENT_PAYLOAD_EXCLUDED = [
    'port', 'addr', 'web_root', 'encode_dri', 'debug', 'audio_device_name',
    'watermark_path', 'recording_socket', 'file_manager_path',
    'run_after_connect', 'run_after_disconnect',
]


def build_client_settings_payload():
    """Client-facing settings snapshot shared by both transports: skips
    server-local/sensitive entries, carries locked/overridden flags plus
    enum/range metadata, and derives the clipboard gate booleans."""
    out = {}
    for setting_def in SETTING_DEFINITIONS:
        name = setting_def['name']
        if name in CLIENT_PAYLOAD_EXCLUDED or setting_def.get('sensitive'):
            continue
        value = getattr(settings, name)
        if setting_def['type'] == 'bool':
            bool_val, is_locked = value
            payload_entry = {'value': bool_val, 'locked': is_locked}
        else:
            payload_entry = {'value': value}
        # Whether this value came from an explicit CLI/env choice (vs the
        # built-in default). The client uses it to decide if a conditional
        # default (e.g. HiDPI-off when a manual resolution is set) should
        # apply or defer to the operator's explicit setting.
        payload_entry['overridden'] = bool(settings._overridden.get(name, False))
        if setting_def['type'] == 'range':
            payload_entry['min'], payload_entry['max'] = value
            if 'meta' in setting_def and 'default_value' in setting_def['meta']:
                payload_entry['default'] = setting_def['meta']['default_value']
        elif setting_def['type'] in ('enum', 'list'):
            if 'meta' in setting_def and 'allowed' in setting_def['meta']:
                payload_entry['allowed'] = setting_def['meta']['allowed']
        out[name] = payload_entry
    # Booleans the client gates its clipboard UI/handlers on, derived from
    # the single enable_clipboard policy string.
    clip = settings.enable_clipboard
    out['clipboard_enabled'] = {'value': clip != 'false'}
    out['clipboard_in_enabled'] = {'value': clip in ('true', 'in')}
    out['clipboard_out_enabled'] = {'value': clip in ('true', 'out')}
    return out


# Default int bounds for client-provided numeric settings. Min is not 0:
# settings without an explicit min may use -1 sentinels that must not be
# clamped up. Shared by both transports' sanitizers.
INT_SETTING_DEFAULT_MAX = 1_000_000
INT_SETTING_DEFAULT_MIN = -1_000_000


def sanitize_client_setting(name, client_value, source, log):
    """Clamp/validate ONE client-provided setting against the server's limits —
    the sanitizer shared by both transports (websockets SETTINGS payloads and
    the WebRTC settings channel), so a value is accepted or clamped identically
    whichever path delivered it.

    `source` exposes the server's resolved values by attribute (the parsed
    settings namespace); `log` is the calling transport's logger. Returns the
    sanitized value, or None when the setting is unknown.

    Rules: ranges clamp into the server min/max (fractional values are legal —
    sub-Mbps bitrates — and integral values stay ints); enums fall back to the
    server default when not allowed; ints/floats clamp into declared bounds
    (top-level min/max win over meta so declared bounds aren't loosened by the
    sentinel-safe negative fallback); locked bools keep the server value. A
    None client value resolves to the server value/default for the type.
    """
    setting_def = next((s for s in SETTING_DEFINITIONS if s['name'] == name), None)
    if not setting_def:
        return None
    server_limit = getattr(source, name)
    if client_value is None:
        if setting_def['type'] == 'range':
            min_val, max_val = server_limit
            return min_val if min_val == max_val else setting_def.get('meta', {}).get('default_value')
        elif setting_def['type'] == 'bool':
            return server_limit[0]
        else:  # enum, list, str, int
            return server_limit
    try:
        if setting_def['type'] == 'range':
            min_val, max_val = server_limit
            numeric = float(client_value)
            if numeric.is_integer():
                numeric = int(numeric)
            sanitized = max(min_val, min(numeric, max_val))
            if sanitized != numeric:
                log.warning(
                    f"Client value for '{name}' ({client_value}) was clamped to {sanitized} (server range: {min_val}-{max_val})."
                )
            return sanitized
        elif setting_def['type'] == 'enum':
            allowed_values = setting_def['meta']['allowed']
            if str(client_value) in allowed_values:
                # Normalize to str so later equality checks don't flip on str-vs-int.
                return str(client_value)
            server_default = allowed_values[0] if allowed_values else setting_def['default']
            log.warning(
                f"Client value for '{name}' ('{client_value}') is not in the allowed list {allowed_values}. Using server default '{server_default}'."
            )
            return server_default
        elif setting_def['type'] in ('int', 'float'):
            sanitized = int(client_value) if setting_def['type'] == 'int' else float(client_value)
            meta = setting_def.get('meta', {})
            min_val = setting_def.get('min', meta.get('min', INT_SETTING_DEFAULT_MIN))
            max_val = setting_def.get('max', meta.get('max', INT_SETTING_DEFAULT_MAX))
            clamped = max(min_val, min(sanitized, max_val))
            if clamped != sanitized:
                log.warning(
                    f"Client value for '{name}' ({client_value}) was clamped to {clamped} (bounds: {min_val}-{max_val})."
                )
            return clamped
        elif setting_def['type'] == 'bool':
            server_val, is_locked = server_limit
            client_bool = str(client_value).lower() in ['true', '1']
            if is_locked:
                if client_bool != server_val:
                    log.warning(
                        f"Client tried to change locked setting '{name}' to '{client_bool}'. Request ignored, using server value '{server_val}'."
                    )
                return server_val
            return client_bool
    except (ValueError, TypeError, IndexError, OverflowError):
        # OverflowError guards JSON inf (1e999 -> int(inf)); fall back to default.
        def_val_meta = setting_def.get('meta', {}).get('default_value')
        return def_val_meta if def_val_meta is not None else setting_def.get('default')
    return client_value

if settings.debug[0]:
    logging.getLogger().setLevel(logging.DEBUG)
    logging.getLogger("websockets").setLevel(logging.WARNING)
else:
    logging.getLogger().setLevel(logging.INFO)
    logging.getLogger("websockets").setLevel(logging.WARNING)
