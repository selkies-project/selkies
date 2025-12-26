# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
import os

# Constants
BACKPRESSURE_ALLOWED_DESYNC_MS = 2000
BACKPRESSURE_LATENCY_THRESHOLD_MS = 50
BACKPRESSURE_CHECK_INTERVAL_S = 0.5
MAX_UINT16_FRAME_ID = 65535
FRAME_ID_SUSPICIOUS_GAP_THRESHOLD = (
    MAX_UINT16_FRAME_ID // 2
)
STALLED_CLIENT_TIMEOUT_SECONDS = 4.0
RTT_SMOOTHING_SAMPLES = 20
SENT_FRAME_TIMESTAMP_HISTORY_SIZE = 1000
TARGET_FRAMERATE = 60

UINPUT_MOUSE_SOCKET = ""
JS_SOCKET_PATH = "/tmp"
ENABLE_CLIPBOARD = True
ENABLE_BINARY_CLIPBOARD = False
ENABLE_CURSORS = True
CURSOR_SIZE = 32
DEBUG_CURSORS = False
ENABLE_RESIZE = True
AUDIO_CHANNELS_DEFAULT = 2
AUDIO_BITRATE_DEFAULT = 320000
GPU_ID_DEFAULT = 0
PIXELFLUX_VIDEO_ENCODERS = ["jpeg", "x264enc", "x264enc-striped"]
IS_WAYLAND = os.environ.get("PIXELFLUX_WAYLAND", "false").lower() == "true"

import logging
LOGLEVEL = logging.INFO
logging.basicConfig(level=LOGLEVEL)
logger_selkies_gamepad = logging.getLogger("selkies_gamepad")
logger_gst_app = logging.getLogger("gst_app")
logger_gst_app_resize = logging.getLogger("gst_app_resize")
logger_input_handler = logging.getLogger("input_handler")
logger = logging.getLogger("main")
data_logger = logging.getLogger("data_websocket")

X11_CAPTURE_AVAILABLE = False
PCMFLUX_AVAILABLE = False

import asyncio
import argparse
import base64
import ctypes
import json
import pathlib
import re
import struct
from asyncio import subprocess
import urllib.parse
import sys
import time
import io
from PIL import Image, ImageDraw
import websockets
import websockets.asyncio.server as ws_async
from aiohttp import web
from collections import OrderedDict, deque
from datetime import datetime
from shutil import which
from signal import SIGINT, signal
from .settings import settings_ws as settings, FINAL_SETTING_DEFINITIONS_WEBSOCKETS as SETTING_DEFINITIONS

try:
    from pcmflux import AudioCapture, AudioCaptureSettings, AudioChunkCallback
    PCMFLUX_AVAILABLE = True
    data_logger.info("pcmflux library found. Audio capture is available.")
except ImportError:
    PCMFLUX_AVAILABLE = False
    data_logger.warning("pcmflux library not found. Audio capture is unavailable.")

try:
    import pulsectl
    import pasimple

    PULSEAUDIO_AVAILABLE = True
except ImportError:
    PULSEAUDIO_AVAILABLE = False
    data_logger.warning(
        "pulsectl or pasimple not found. Microphone forwarding will be disabled."
    )

try:
    from pixelflux import CaptureSettings, ScreenCapture, StripeCallback

    X11_CAPTURE_AVAILABLE = True
    data_logger.info("pixelflux library found. Striped encoding modes available.")
except ImportError:
    X11_CAPTURE_AVAILABLE = False
    data_logger.warning(
        "pixelflux library not found. Striped encoding modes unavailable."
    )

from .input_handler import WebRTCInput as InputHandler, SelkiesGamepad, GamepadMapper
import psutil
import GPUtil

upload_path = os.getenv('FILE_MANAGER_PATH', '~/Desktop')
upload_dir_path = os.path.expanduser(upload_path)

try:
    os.makedirs(upload_dir_path, exist_ok=True)
    logger.info(f"Upload directory ensured: {upload_dir_path}")
except OSError as e:
    logger.error(f"Could not create upload directory {upload_dir_path}: {e}")
    upload_dir_path = None

config_gate = asyncio.Event()
user_tokens = {}
client_permissions = {}

class SelkiesAppError(Exception):
    pass


class SelkiesStreamingApp:
    def __init__(
        self,
        async_event_loop,
        framerate,
        encoder,
        data_streaming_server=None,
        mode="websockets",
    ):
        self.server_enable_resize = ENABLE_RESIZE
        self.mode = mode
        self.display_width = 1024
        self.display_height = 768
        self.pipeline_running = False
        self.async_event_loop = async_event_loop
        self.audio_channels = AUDIO_CHANNELS_DEFAULT
        self.gpu_id = GPU_ID_DEFAULT
        self.audio_bitrate = AUDIO_BITRATE_DEFAULT
        self.encoder = encoder
        self.framerate = framerate
        self.last_cursor_sent = None
        self.data_streaming_server = data_streaming_server

    async def send_ws_clipboard_data(self, data, mime_type="text/plain"):
        """
        Asynchronously sends clipboard data to all clients, handling multipart for large data.
        """
        if not (self.data_streaming_server and self.data_streaming_server.clients):
            data_logger.warning("Cannot send clipboard: no clients or server not ready.")
            return
        try:
            is_binary = mime_type != "text/plain"
            if is_binary and not self.data_streaming_server.enable_binary_clipboard:
                data_logger.warning(
                    f"Attempted to send binary clipboard data ({mime_type}) but feature is disabled on server."
                )
                return
            data_bytes = data.encode('utf-8') if not is_binary and isinstance(data, str) else data
            total_size = len(data_bytes)
            from .input_handler import CLIPBOARD_CHUNK_SIZE
            if total_size < CLIPBOARD_CHUNK_SIZE:
                encoded_data = base64.b64encode(data_bytes).decode('ascii')
                if is_binary:
                    message = f"clipboard_binary,{mime_type},{encoded_data}"
                else:
                    message = f"clipboard,{encoded_data}"
                websockets.broadcast(self.data_streaming_server.clients, message)
            else:
                data_logger.info(f"Sending large clipboard data ({mime_type}, {total_size} bytes) via multipart.")
                start_message = f"clipboard_start,{mime_type},{total_size}"
                websockets.broadcast(self.data_streaming_server.clients, start_message)
                offset = 0
                while offset < total_size:
                    chunk = data_bytes[offset:offset + CLIPBOARD_CHUNK_SIZE]
                    encoded_chunk = base64.b64encode(chunk).decode('ascii')
                    data_message = f"clipboard_data,{encoded_chunk}"
                    websockets.broadcast(self.data_streaming_server.clients, data_message)
                    offset += len(chunk)
                    await asyncio.sleep(0)
                websockets.broadcast(self.data_streaming_server.clients, "clipboard_finish")
                data_logger.info("Finished sending multi-part clipboard data.")
        except Exception as e:
            data_logger.error(f"Failed to send clipboard data: {e}", exc_info=True)

    def send_ws_cursor_data(self, data):
        self.last_cursor_sent = data
        if (
            self.data_streaming_server
            and hasattr(self.data_streaming_server, "clients")
            and self.data_streaming_server.clients
            and self.async_event_loop
            and self.async_event_loop.is_running()
        ):

            msg_str = json.dumps(data)
            msg_to_broadcast = f"cursor,{msg_str}"
            clients_ref = self.data_streaming_server.clients

            async def _broadcast_cursor_helper():
                websockets.broadcast(clients_ref, msg_to_broadcast)

            asyncio.run_coroutine_threadsafe(
                _broadcast_cursor_helper(), self.async_event_loop
            )
        else:
            data_logger.warning("Cannot broadcast cursor data: no clients connected or server not ready.")

    async def stop_pipeline(self):
        logger_gst_app.info("Stopping pipelines (generic call)...")
        if self.data_streaming_server:
            await self.data_streaming_server.reconfigure_displays()
        self.pipeline_running = False
        logger_gst_app.info("Pipelines stop signal processed.")

    stop_ws_pipeline = stop_pipeline

    def set_framerate(self, framerate):
        self.framerate = int(framerate)
        logger_gst_app.info(
            f"Framerate for {self.encoder} set to {self.framerate}. Restart pipeline if active."
        )


def fit_res(w, h, max_w, max_h):
    if w <= max_w and h <= max_h:
        return w, h
    aspect = w / h
    if w > max_w:
        w = max_w
        h = int(w / aspect)
    if h > max_h:
        h = max_h
        w = int(h * aspect)
    return w - (w % 2), h - (h % 2)


async def get_new_res(res_str):
    if IS_WAYLAND:
        return res_str, res_str, [res_str], res_str, "Wayland-0"
    screen_name = None
    resolutions = []
    screen_pat = re.compile(r"(\S+) connected")
    current_pat = re.compile(r".*current (\d+\s*x\s*\d+).*")
    res_pat = re.compile(r"^(\d+x\d+)\s+\d+\.\d+.*")
    curr_res = new_res = max_res_str = res_str
    try:
        process = await subprocess.create_subprocess_exec(
            "xrandr",
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT
        )
        stdout, _ = await process.communicate()
        xrandr_output = stdout.decode('utf-8')
    except (FileNotFoundError, Exception) as e:
        logger_gst_app_resize.error(f"xrandr command failed: {e}")
        return curr_res, new_res, resolutions, max_res_str, screen_name
    current_screen_modes_started = False
    for line in xrandr_output.splitlines():
        screen_match = screen_pat.match(line)
        if screen_match:
            if screen_name is None:
                screen_name = screen_match.group(1)
            current_screen_modes_started = screen_name == screen_match.group(1)
        if current_screen_modes_started:
            current_match = current_pat.match(line)
            if current_match:
                curr_res = current_match.group(1).replace(" ", "")
            res_match = res_pat.match(line.strip())
            if res_match:
                resolutions.append(res_match.group(1))
    if not screen_name:
        logger_gst_app_resize.warning(
            "Could not determine connected screen from xrandr."
        )
        return curr_res, new_res, resolutions, max_res_str, screen_name
    max_w_limit, max_h_limit = 7680, 4320
    max_res_str = f"{max_w_limit}x{max_h_limit}"
    try:
        w, h = map(int, res_str.split("x"))
        new_w, new_h = fit_res(w, h, max_w_limit, max_h_limit)
        new_res = f"{new_w}x{new_h}"
    except ValueError:
        logger_gst_app_resize.error(f"Invalid resolution format for fitting: {res_str}")
    resolutions = sorted(list(set(resolutions)))
    return curr_res, new_res, resolutions, max_res_str, screen_name


async def resize_display(res_str):  # e.g., res_str is "2560x1280"
    """
    Resizes the display using xrandr to the specified resolution string.
    Adds a new mode via cvt/gtf if the requested mode doesn't exist,
    using res_str (e.g., "2560x1280") as the mode name for xrandr.
    """
    if IS_WAYLAND:
        return True
    _, _, available_resolutions, _, screen_name = await get_new_res(res_str)

    if not screen_name:
        logger_gst_app_resize.error(
            "Cannot resize display via xrandr, no screen identified."
        )
        return False

    target_mode_to_set = res_str

    if res_str not in available_resolutions:
        logger_gst_app_resize.info(
            f"Mode {res_str} not found in xrandr list. Attempting to add for screen '{screen_name}'."
        )
        try:
            (
                modeline_name_from_cvt_output,
                modeline_params,
            ) = await generate_xrandr_gtf_modeline(res_str)
        except Exception as e:
            logger_gst_app_resize.error(
                f"Failed to generate modeline for {res_str}: {e}"
            )
            return False

        cmd_new = ["xrandr", "--newmode", res_str] + modeline_params.split()
        new_mode_proc = await subprocess.create_subprocess_exec(
            *cmd_new,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        stdout_new, stderr_new = await new_mode_proc.communicate()
        if new_mode_proc.returncode != 0:
            logger_gst_app_resize.error(
                f"Failed to create new xrandr mode with '{' '.join(cmd_new)}': {stderr_new.decode()}"
            )
            return False
        logger_gst_app_resize.info(f"Successfully ran: {' '.join(cmd_new)}")

        cmd_add = ["xrandr", "--addmode", screen_name, res_str]
        add_mode_proc = await subprocess.create_subprocess_exec(
            *cmd_add,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        stdout_add, stderr_add = await add_mode_proc.communicate()
        if add_mode_proc.returncode != 0:
            logger_gst_app_resize.error(
                f"Failed to add mode '{res_str}' to screen '{screen_name}': {stderr_add.decode()}"
            )
            # Cleanup commands
            delmode_proc = await subprocess.create_subprocess_exec(
                "xrandr", "--delmode", screen_name, res_str,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            await delmode_proc.communicate()
            
            rmmode_proc = await subprocess.create_subprocess_exec(
                "xrandr", "--rmmode", res_str,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            await rmmmode_proc.communicate()
            return False
        logger_gst_app_resize.info(f"Successfully ran: {' '.join(cmd_add)}")

    logger_gst_app_resize.info(
        f"Applying xrandr mode '{target_mode_to_set}' for screen '{screen_name}'."
    )
    cmd_output = ["xrandr", "--output", screen_name, "--mode", target_mode_to_set]
    set_mode_proc = await subprocess.create_subprocess_exec(
        *cmd_output,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE
    )
    stdout_set, stderr_set = await set_mode_proc.communicate()
    if set_mode_proc.returncode != 0:
        logger_gst_app_resize.error(
            f"Failed to set mode '{target_mode_to_set}' on screen '{screen_name}': {stderr_set.decode()}"
        )
        return False

    logger_gst_app_resize.info(
        f"Successfully applied xrandr mode '{target_mode_to_set}'."
    )
    return True


async def generate_xrandr_gtf_modeline(res_wh_str):
    """Generates an xrandr modeline string using cvt or gtf."""
    try:
        w_str, h_str = res_wh_str.split("x")
        cmd = ["cvt", w_str, h_str, "60"]
        tool_name = "cvt"
        try:
            process = await subprocess.create_subprocess_exec(
                *cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            stdout, stderr = await process.communicate()
            if process.returncode != 0:
                raise Exception(f"cvt failed: {stderr.decode()}")
            modeline_output = stdout.decode('utf-8')
        except (FileNotFoundError, Exception):
            logger_gst_app_resize.warning(
                "cvt command failed or not found, trying gtf."
            )
            cmd = ["gtf", w_str, h_str, "60"]
            tool_name = "gtf"
            process = await subprocess.create_subprocess_exec(
                *cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            stdout, stderr = await process.communicate()
            if process.returncode != 0:
                raise Exception(f"gtf failed: {stderr.decode()}")
            modeline_output = stdout.decode('utf-8')
    except (FileNotFoundError, Exception) as e:
        raise Exception(
            f"Failed to generate modeline using {tool_name} for {res_wh_str}: {e}"
        ) from e
    except ValueError:
        raise Exception(
            f"Invalid resolution format for modeline generation: {res_wh_str}"
        )
    match = re.search(r'Modeline\s+"([^"]+)"\s+(.*)', modeline_output)
    if not match:
        raise Exception(
            f"Could not parse modeline from {tool_name} output: {modeline_output}"
        )
    return match.group(1).strip(), match.group(2)

def parse_dri_node_to_index(node_path: str) -> int:
    """
    Parses a DRI node path like '/dev/dri/renderD128' into an index (e.g., 0).
    Returns -1 if the path is invalid, malformed, or empty, which
    disables VA-API usage in the capture module.
    """
    if not node_path or not node_path.startswith('/dev/dri/renderD'):
        if node_path:
             logger.warning(f"Invalid DRI node format: '{node_path}'. Expected '/dev/dri/renderD...'. VA-API will be disabled.")
        return -1
    try:
        num_str = node_path.split('renderD')[-1]
        render_num = int(num_str)
        index = render_num - 128
        if index < 0:
            logger.warning(f"Parsed DRI node number {render_num} from '{node_path}' is less than 128. Invalid.")
            return -1
        logger.info(f"Parsed DRI node '{node_path}' to index {index}.")
        return index
    except (ValueError, IndexError) as e:
        logger.warning(f"Could not parse DRI node path '{node_path}': {e}. VA-API will be disabled.")
        return -1

async def _run_xrdb(dpi_value, logger):
    """Helper function to apply DPI via xrdb and xsettingsd."""
    if not which("xrdb"):
        logger.debug("xrdb not found. Skipping Xresources DPI setting.")
        return False
        
    xresources_path_str = os.path.expanduser("~/.Xresources")
    try:    
        with open(xresources_path_str, "w") as f:
            f.write(f"Xft.dpi:   {dpi_value}\n")
        logger.info(f"Wrote 'Xft.dpi:   {dpi_value}' to {xresources_path_str}.")

        cmd_xrdb = ["xrdb", xresources_path_str]
        process = await subprocess.create_subprocess_exec(
            *cmd_xrdb,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        stdout, stderr = await process.communicate()
        
        xrdb_success = process.returncode == 0
        if xrdb_success:
            logger.info(f"Successfully loaded {xresources_path_str} using xrdb.")
        else:
            logger.warning(f"Failed to load {xresources_path_str} using xrdb. RC: {process.returncode}, Error: {stderr.decode().strip()}")

        xsettingsd_config_path = os.path.expanduser("~/.xsettingsd")
        xsettings_dpi = dpi_value * 1024
        
        config_content = (
            "Xft/Antialias 1\n"
            "Xft/Hinting 1\n"
            "Xft/HintStyle \"hintfull\"\n"
            "Xft/RGBA \"rgb\"\n"
            f"Xft/DPI {xsettings_dpi}\n"
        )
        
        with open(xsettingsd_config_path, "w") as f:
            f.write(config_content)
        logger.info(f"Wrote font and DPI settings to {xsettingsd_config_path}.")

        if not which("pgrep") or not which("kill"):
            logger.debug("pgrep or kill not found. Skipping xsettingsd reload.")
        else:
            pgrep_proc = await subprocess.create_subprocess_exec(
                "pgrep", "xsettingsd",
                stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            pgrep_stdout, _ = await pgrep_proc.communicate()

            if pgrep_proc.returncode == 0:
                pid_output = pgrep_stdout.decode().strip()
                if pid_output:
                    pid = pid_output.splitlines()[0]
                    logger.info(f"Found xsettingsd process with PID: {pid}.")
                    kill_proc = await subprocess.create_subprocess_exec(
                        "kill", "-1", pid,
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE
                    )
                    _, kill_stderr = await kill_proc.communicate()
                    if kill_proc.returncode == 0:
                        logger.info(f"Sent SIGHUP to xsettingsd process {pid} to reload config.")
                    else:
                        logger.warning(f"Failed to send SIGHUP to xsettingsd process {pid}. Error: {kill_stderr.decode().strip()}")
            else:
                logger.info("xsettingsd process not found. Skipping reload.")
        
        return xrdb_success

    except Exception as e:
        logger.error(f"Error updating or loading DPI settings: {e}")
        return False

async def _get_xfce_session_env(logger):
    """
    Finds the running xfce4-session process and extracts its environment variables.
    This is necessary to communicate with the correct D-Bus session.
    """
    try:
        proc_pid = await subprocess.create_subprocess_exec(
            "pgrep", "-o", "-x", "xfce4-session",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        stdout_pid, stderr_pid = await proc_pid.communicate()

        if proc_pid.returncode != 0:
            logger.debug(f"Could not find running xfce4-session: {stderr_pid.decode().strip()}")
            return None
        
        pid = stdout_pid.decode().strip()
        
        env_path = f"/proc/{pid}/environ"
        if not os.path.exists(env_path):
            logger.debug(f"Could not read environment for PID {pid}. Path {env_path} does not exist.")
            return None

        with open(env_path, "r") as f:
            environ_data = f.read()
        
        env = {}
        for line in environ_data.split('\x00'):
            if '=' in line:
                key, value = line.split('=', 1)
                env[key] = value
        
        if "DBUS_SESSION_BUS_ADDRESS" not in env:
            logger.debug(f"Found xfce4-session (PID {pid}), but DBUS_SESSION_BUS_ADDRESS was not in its environment.")
            return None

        return env

    except Exception as e:
        logger.warning(f"Failed to get XFCE session environment, will proceed with default environment: {e}")
        return None

async def _run_xfconf(dpi_value, logger):
    """Helper function to apply DPI via xfconf-query for XFCE."""
    if not which("xfconf-query"):
        logger.debug("xfconf-query not found. Skipping XFCE DPI setting via xfconf-query.")
        return False

    session_env = await _get_xfce_session_env(logger)
    if session_env:
        logger.info("Found active XFCE session environment. Commands will be executed within this context.")
    else:
        logger.warning("Could not obtain XFCE session environment. Falling back to direct execution.")

    async def run_command(cmd, success_msg, failure_msg):
        try:
            process = await subprocess.create_subprocess_exec(
                *cmd,
                env=session_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            _stdout, stderr = await process.communicate()
            if process.returncode == 0:
                logger.info(success_msg)
                return True
            else:
                logger.warning(f"{failure_msg}. RC: {process.returncode}, Error: {stderr.decode().strip()}")
                return False
        except Exception as e:
            logger.error(f"Error running command '{' '.join(cmd)}': {e}")
            return False

    cmd_dpi = [
        "xfconf-query", "-c", "xsettings", "-p", "/Xft/DPI",
        "-s", str(dpi_value), "--create", "-t", "int"
    ]
    if not await run_command(
        cmd_dpi,
        f"Successfully set XFCE DPI to {dpi_value} using xfconf-query.",
        "Failed to set XFCE DPI using xfconf-query"
    ):
        return False

    cursor_size = int(round(dpi_value / 96 * 32))
    logger.info(f"Attempting to set cursor size to: {cursor_size} (based on DPI {dpi_value})")
    cmd_cursor = [
        "xfconf-query", "-c", "xsettings", "-p", "/Gtk/CursorThemeSize",
        "-s", str(cursor_size), "--create", "-t", "int"
    ]
    if not await run_command(
        cmd_cursor,
        f"Successfully set cursor size to {cursor_size}",
        "Failed to set cursor size using xfconf-query"
    ):
        return False

    return True

async def _run_mate_gsettings(dpi_value, logger):
    """Helper function to apply DPI via gsettings for MATE."""
    if not which("gsettings"):
        logger.debug("gsettings not found. Skipping MATE gsettings.")
        return False

    mate_settings_succeeded_at_least_once = False

    # MATE: org.mate.interface window-scaling-factor
    try:
        target_mate_scale_float = float(dpi_value) / 96.0
        # For fractional scales (e.g., 1.5), MATE's integer window-scaling-factor
        # should be 1. We rely on font DPI / text scaling for the fractional part.
        # If it's an integer scale (e.g., 2.0 for 192 DPI), then use that integer.
        if target_mate_scale_float == int(target_mate_scale_float):
            mate_window_scaling_factor = int(target_mate_scale_float)
        else:
            mate_window_scaling_factor = 1 
        
        mate_window_scaling_factor = max(1, mate_window_scaling_factor) # Ensure it's at least 1

        cmd_gsettings_mate_window_scale = [
            "gsettings", "set",
            "org.mate.interface", "window-scaling-factor",
            str(mate_window_scaling_factor)
        ]
        result_mate_window_scale = await subprocess.create_subprocess_exec(
            *cmd_gsettings_mate_window_scale,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        stdout_mate_window, stderr_mate_window = await result_mate_window_scale.communicate()
        if result_mate_window_scale.returncode == 0:
            logger.info(f"Successfully set MATE window-scaling-factor to {mate_window_scaling_factor} (for DPI {dpi_value}) using gsettings.")
            mate_settings_succeeded_at_least_once = True
        else:
            stderr_text = stderr_mate_window.decode().strip()
            if "No such schema" in stderr_text or "No such key" in stderr_text:
                logger.debug(f"gsettings: Schema/key 'org.mate.interface window-scaling-factor' not found. Error: {stderr_text}")
            else:
                logger.warning(f"Failed to set MATE window-scaling-factor using gsettings. RC: {result_mate_window_scale.returncode}, Error: {stderr_text}")
    except Exception as e:
        logger.error(f"Error running gsettings for MATE window-scaling-factor: {e}")

    # MATE: org.mate.font-rendering dpi
    try:
        cmd_gsettings_mate_font_dpi = [
            "gsettings", "set",
            "org.mate.font-rendering", "dpi",
            str(dpi_value) # MATE font rendering takes the direct DPI value
        ]
        result_mate_font_dpi = await subprocess.create_subprocess_exec(
            *cmd_gsettings_mate_font_dpi,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        stdout_mate_font, stderr_mate_font = await result_mate_font_dpi.communicate()
        if result_mate_font_dpi.returncode == 0:
            logger.info(f"Successfully set MATE font-rendering DPI to {dpi_value} using gsettings.")
            mate_settings_succeeded_at_least_once = True
        else:
            stderr_font_text = stderr_mate_font.decode().strip()
            if "No such schema" in stderr_font_text or "No such key" in stderr_font_text:
                logger.debug(f"gsettings: Schema/key 'org.mate.font-rendering dpi' not found. Error: {stderr_font_text}")
            else:
                logger.warning(f"Failed to set MATE font-rendering DPI using gsettings. RC: {result_mate_font_dpi.returncode}, Error: {stderr_font_text}")
    except Exception as e:
        logger.error(f"Error running gsettings for MATE font-rendering DPI: {e}")
    
    return mate_settings_succeeded_at_least_once


async def set_dpi(dpi_setting):
    """
    Sets the display DPI using DE-specific methods based on a defined detection order.
    The dpi_setting is expected to be an integer or a string representing an integer.
    """
    if IS_WAYLAND:
        return True
    try:
        dpi_value = int(str(dpi_setting))
        if dpi_value <= 0:
            logger_gst_app_resize.error(f"Invalid DPI value: {dpi_value}. Must be a positive integer.")
            return False
    except ValueError:
        logger_gst_app_resize.error(f"Invalid DPI format: '{dpi_setting}'. Must be convertible to a positive integer.")
        return False

    any_method_succeeded = False
    de_name_for_log = "Unknown" # For logging which DE path was taken

    # DE Detection and Action Order: KDE -> XFCE -> MATE -> i3 -> Openbox
    if which("startplasma-x11"):
        de_name_for_log = "KDE"
        logger_gst_app_resize.info(f"{de_name_for_log} detected. Applying xrdb for DPI {dpi_value}.")
        if await _run_xrdb(dpi_value, logger_gst_app_resize):
            any_method_succeeded = True
    
    elif which("xfce4-session"):
        de_name_for_log = "XFCE"
        logger_gst_app_resize.info(f"{de_name_for_log} detected. Applying xfconf-query for DPI {dpi_value}.")
        if await _run_xfconf(dpi_value, logger_gst_app_resize):
            any_method_succeeded = True
        # For XFCE, only xfconf-query is used to avoid potential double scaling.

    elif which("mate-session"):
        de_name_for_log = "MATE"
        logger_gst_app_resize.info(f"{de_name_for_log} detected. Applying MATE gsettings and xrdb for DPI {dpi_value}.")
        mate_gsettings_success = await _run_mate_gsettings(dpi_value, logger_gst_app_resize)
        # Also apply xrdb for MATE for wider application compatibility / fallback
        xrdb_for_mate_success = await _run_xrdb(dpi_value, logger_gst_app_resize)
        if mate_gsettings_success or xrdb_for_mate_success:
            any_method_succeeded = True

    elif which("i3"):
        de_name_for_log = "i3"
        logger_gst_app_resize.info(f"{de_name_for_log} detected. Applying xrdb for DPI {dpi_value}.")
        if await _run_xrdb(dpi_value, logger_gst_app_resize):
            any_method_succeeded = True
            
    elif which("openbox-session") or which("openbox"): # Check for openbox binary as well
        de_name_for_log = "Openbox"
        logger_gst_app_resize.info(f"{de_name_for_log} detected. Applying xrdb for DPI {dpi_value}.")
        if await _run_xrdb(dpi_value, logger_gst_app_resize):
            any_method_succeeded = True
            
    else:
        de_name_for_log = "Generic/Unknown DE"
        logger_gst_app_resize.info(f"No specific DE session binary found (KDE, XFCE, MATE, i3, Openbox). Attempting generic xrdb as a fallback for DPI {dpi_value}.")
        if await _run_xrdb(dpi_value, logger_gst_app_resize):
            any_method_succeeded = True

    if not any_method_succeeded:
        logger_gst_app_resize.warning(f"No DPI setting method succeeded for DPI {dpi_value} (Attempted for: {de_name_for_log}).")

    return any_method_succeeded

async def set_cursor_size(size):
    if not isinstance(size, int) or size <= 0:
        logger_gst_app_resize.error(f"Invalid cursor size: {size}")
        return False
    if which("xfconf-query"):
        cmd = [
            "xfconf-query",
            "-c",
            "xsettings",
            "-p",
            "/Gtk/CursorThemeSize",
            "-s",
            str(size),
            "--create",
            "-t",
            "int",
        ]
        process = await subprocess.create_subprocess_exec(
            *cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        await process.communicate()
        if process.returncode == 0:
            return True
        logger_gst_app_resize.warning("Failed to set XFCE cursor size.")
    if which("gsettings"):
        try:
            cmd_set = [
                "gsettings",
                "set",
                "org.gnome.desktop.interface",
                "cursor-size",
                str(size),
            ]
            process_set = await subprocess.create_subprocess_exec(
                *cmd_set,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            await process_set.communicate()
            if process_set.returncode == 0:
                logger_gst_app_resize.info(f"Set GNOME cursor-size to {size}")
                return True
            logger_gst_app_resize.warning("Failed to set GNOME cursor-size.")
        except Exception as e:
            logger_gst_app_resize.warning(
                f"Error trying to set GNOME cursor size via gsettings: {e}"
            )
    logger_gst_app_resize.warning("No supported tool found/worked to set cursor size.")
    return False


class DataStreamingServer:
    """Handles the data WebSocket connection for input, stats, and control messages."""

    def __init__(
        self,
        port,
        app,
        uinput_mouse_socket,
        js_socket_path,
        enable_clipboard,
        enable_cursors,
        cursor_size,
        cursor_scale,
        cursor_debug,
        audio_device_name,
        cli_args,
        is_secure_mode,
    ):
        self.port = port
        self.mode = "websockets"
        self.server = None
        self.stop_server = None
        self.data_ws = (
            None
        )
        self.clients = set()
        self.app = app
        self.cli_args = cli_args
        self.is_secure_mode = is_secure_mode
        self.RECONNECT_DEBOUNCE_MS = 500
        self.MAX_RECENT_CLIENTS = 1000
        self.last_connection_times = OrderedDict()
        self._latest_client_render_fps = 0.0
        self._last_time_client_ok = 0.0
        self._client_acknowledged_frame_id = -1
        self._frame_backpressure_task = None
        self._last_client_acknowledged_frame_id_update_time = 0.0
        self._previous_ack_id_for_stall_check = -1
        self._previous_sent_id_for_stall_check = -1
        self._sent_frame_timestamps = OrderedDict()
        self._rtt_samples = deque(maxlen=RTT_SMOOTHING_SAMPLES)
        self._smoothed_rtt_ms = 0.0
        self._sent_frames_log = deque()
        
        def get_initial_value(setting_name):
            """Helper to get the correct initial integer/bool from a processed setting."""
            processed_value = getattr(self.cli_args, setting_name)
            setting_def = next((s for s in SETTING_DEFINITIONS if s['name'] == setting_name), None)
            if not setting_def: return None

            if setting_def['type'] == 'range':
                min_val, max_val = processed_value
                return min_val if min_val == max_val else setting_def.get('meta', {}).get('default_value')
            elif setting_def['type'] == 'bool':
                return processed_value[0]
            return processed_value

        self._initial_h264_crf = get_initial_value('h264_crf')
        self.h264_crf = self._initial_h264_crf
        self._initial_h264_fullcolor = get_initial_value('h264_fullcolor')
        self.h264_fullcolor = self._initial_h264_fullcolor
        self._initial_h264_streaming_mode = get_initial_value('h264_streaming_mode')
        self.h264_streaming_mode = self._initial_h264_streaming_mode
        self.capture_cursor = False
        self._initial_jpeg_quality = get_initial_value('jpeg_quality')
        self.jpeg_quality = self._initial_jpeg_quality
        self._initial_paint_over_jpeg_quality = get_initial_value('paint_over_jpeg_quality')
        self.paint_over_jpeg_quality = self._initial_paint_over_jpeg_quality
        self._initial_h264_paintover_crf = get_initial_value('h264_paintover_crf')
        self.h264_paintover_crf = self._initial_h264_paintover_crf
        self._initial_h264_paintover_burst_frames = get_initial_value('h264_paintover_burst_frames')
        self.h264_paintover_burst_frames = self._initial_h264_paintover_burst_frames
        self._initial_use_cpu = get_initial_value('use_cpu')
        self.use_cpu = self._initial_use_cpu
        self._initial_use_paint_over_quality = get_initial_value('use_paint_over_quality')
        self.use_paint_over_quality = self._initial_use_paint_over_quality

        self._system_monitor_task_ws = None
        self._gpu_monitor_task_ws = None
        self._stats_sender_task_ws = None
        self._shared_stats_ws = {}
        self.uinput_mouse_socket = uinput_mouse_socket
        self.js_socket_path = js_socket_path
        self.enable_clipboard = enable_clipboard
        self.enable_binary_clipboard = self.cli_args.enable_binary_clipboard[0]
        self.enable_cursors = enable_cursors
        self.cursor_size = cursor_size
        self.cursor_scale = cursor_scale
        self.cursor_debug = cursor_debug
        self.input_handler = None
        self._last_adjustment_timestamp = 0.0
        self.client_settings_received = asyncio.Event()
        self._reconfigure_lock = asyncio.Lock()
        self._is_reconfiguring = False
        self._bytes_sent_in_interval = 0
        self._last_bandwidth_calc_time = time.monotonic()
        # Frame-based backpressure settings
        self.last_start_video_request_times = {}
        self.allowed_desync_ms = BACKPRESSURE_ALLOWED_DESYNC_MS
        self.latency_threshold_for_adjustment_ms = BACKPRESSURE_LATENCY_THRESHOLD_MS
        self.backpressure_check_interval_s = BACKPRESSURE_CHECK_INTERVAL_S
        self._backpressure_send_frames_enabled = True
        self._last_client_frame_id_report_time = 0.0
        self.capture_loop = None

        self.display_clients = {}
        self.video_chunk_queues = {}
        self.capture_instances = {}

        # pcmflux audio capture state
        self.audio_device_name = audio_device_name
        self.pcmflux_module = None
        self.is_pcmflux_capturing = False
        self.pcmflux_settings = None
        self.pcmflux_callback = None
        self.pcmflux_audio_queue = None
        self.pcmflux_send_task = None
        self.pcmflux_capture_loop = None

        # State for window manager swapping
        self._last_display_count = 0
        self._is_wm_swapped = False
        self._wm_swap_is_supported = None

    async def broadcast_display_config(self):
        """Broadcasts the current display configuration to all clients."""
        if not self.clients:
            return
        
        connected_displays = list(self.display_clients.keys())
        payload = {
            "type": "display_config_update",
            "displays": connected_displays
        }
        message_str = f"DISPLAY_CONFIG_UPDATE,{json.dumps(payload)}"
        
        data_logger.info(f"Broadcasting display config update: {message_str}")
        websockets.broadcast(self.clients, message_str)

    def _pcmflux_audio_callback(self, result_ptr, user_data):
        """
        C-style callback passed to pcmflux, called from its capture thread.
        """
        if self.is_pcmflux_capturing and result_ptr and self.pcmflux_audio_queue is not None:
            result = result_ptr.contents
            if result.data and result.size > 0:
                data_bytes = bytes(ctypes.cast(
                    result.data, ctypes.POINTER(ctypes.c_ubyte * result.size)
                ).contents)

                if self.pcmflux_capture_loop and not self.pcmflux_capture_loop.is_closed():
                    asyncio.run_coroutine_threadsafe(
                        self.pcmflux_audio_queue.put(data_bytes), self.pcmflux_capture_loop)
    
    async def _pcmflux_send_audio_chunks(self):
        """
        Async task to broadcast Opus audio chunks from the queue to WebSocket clients.
        """
        data_logger.info("pcmflux audio chunk broadcasting task started.")
        try:
            while True:
                opus_bytes = await self.pcmflux_audio_queue.get()

                secondary_websockets = {
                    client_info.get('ws')
                    for did, client_info in self.display_clients.items()
                    if did != 'primary' and client_info.get('ws')
                }
                primary_viewers = self.clients - secondary_websockets

                if not primary_viewers:
                    self.pcmflux_audio_queue.task_done()
                    continue
                
                message_to_send = b'\x01\x00' + opus_bytes
                self._bytes_sent_in_interval += len(message_to_send) * len(primary_viewers)
                websockets.broadcast(primary_viewers, message_to_send)

                self.pcmflux_audio_queue.task_done()
        except asyncio.CancelledError:
            data_logger.info("pcmflux audio chunk broadcasting task cancelled.")
        finally:
            data_logger.info("pcmflux audio chunk broadcasting task finished.")

    async def _start_pcmflux_pipeline(self):
        if not settings.audio_enabled[0]:
            data_logger.info("Server-to-client audio is disabled by server settings. Not starting pipeline.")
            return False
        if not PCMFLUX_AVAILABLE:
            data_logger.error("Cannot start audio pipeline: pcmflux library not available.")
            return False
        if self.is_pcmflux_capturing:
            data_logger.info("pcmflux audio pipeline is already capturing.")
            return True
        if not self.app:
            data_logger.error("Cannot start pcmflux: self.app (SelkiesStreamingApp instance) is not available.")
            return False
        
        self.pcmflux_capture_loop = self.capture_loop or asyncio.get_running_loop()
        if not self.pcmflux_capture_loop:
            data_logger.error("Cannot start pcmflux: asyncio event loop not found.")
            return False

        data_logger.info("Starting pcmflux audio pipeline...")
        try:
            capture_settings = AudioCaptureSettings()
            device_name_bytes = self.audio_device_name.encode('utf-8') if self.audio_device_name else None
            capture_settings.device_name = device_name_bytes
            capture_settings.sample_rate = 48000
            capture_settings.channels = self.app.audio_channels
            capture_settings.opus_bitrate = int(self.app.audio_bitrate)
            capture_settings.frame_duration_ms = 20
            capture_settings.use_vbr = True
            capture_settings.use_silence_gate = False
            capture_settings.debug_logging = self.cli_args.debug[0]
            self.pcmflux_settings = capture_settings

            data_logger.info(f"pcmflux settings: device='{self.audio_device_name}', "
                             f"bitrate={capture_settings.opus_bitrate}, channels={capture_settings.channels}")

            self.pcmflux_callback = AudioChunkCallback(self._pcmflux_audio_callback)
            self.pcmflux_module = AudioCapture()
            self.pcmflux_audio_queue = asyncio.Queue()

            await self.pcmflux_capture_loop.run_in_executor(
                None, self.pcmflux_module.start_capture, self.pcmflux_settings, self.pcmflux_callback
            )

            self.is_pcmflux_capturing = True
            if self.pcmflux_send_task is None or self.pcmflux_send_task.done():
                self.pcmflux_send_task = asyncio.create_task(self._pcmflux_send_audio_chunks())
            
            data_logger.info("pcmflux audio capture started successfully.")
            return True
        except Exception as e:
            data_logger.error(f"Failed to start pcmflux audio pipeline: {e}", exc_info=True)
            await self._stop_pcmflux_pipeline() # Attempt cleanup on failure
            return False

    async def _stop_pcmflux_pipeline(self):
        if not self.is_pcmflux_capturing and not self.pcmflux_module:
            return True
        
        data_logger.info("Stopping pcmflux audio pipeline...")
        self.is_pcmflux_capturing = False # Prevent new items from being queued

        if self.pcmflux_send_task:
            self.pcmflux_send_task.cancel()
            try:
                await self.pcmflux_send_task
            except asyncio.CancelledError:
                pass
            self.pcmflux_send_task = None
        
        if self.pcmflux_module:
            try:
                if self.pcmflux_capture_loop:
                    await self.pcmflux_capture_loop.run_in_executor(
                        None, self.pcmflux_module.stop_capture
                    )
            except Exception as e:
                data_logger.error(f"Error during pcmflux stop_capture: {e}")
            finally:
                del self.pcmflux_module
                self.pcmflux_module = None
        
        self.pcmflux_audio_queue = None
        data_logger.info("pcmflux audio pipeline stopped.")
        return True

    async def shutdown_pipelines(self):
        """
        A unified, deadlock-proof method to stop all capture pipelines.
        This should be the ONLY way pipelines are programmatically stopped.
        """
        logger.info("Initiating unified pipeline shutdown...")
        await self.reconfigure_displays()
        await self._stop_pcmflux_pipeline()
        if self.display_clients:
            stop_bp_tasks = [
                self._ensure_backpressure_task_is_stopped(disp_id)
                for disp_id in self.display_clients.keys()
            ]
            await asyncio.gather(*stop_bp_tasks, return_exceptions=True)
        if self.pcmflux_send_task and not self.pcmflux_send_task.done():
            self.pcmflux_send_task.cancel()
            try:
                await self.pcmflux_send_task
            except asyncio.CancelledError:
                pass
        logger.info("Unified pipeline shutdown complete.")

    async def _ensure_backpressure_task_is_stopped(self, display_id: str):
        """Safely cancels and cleans up the backpressure task for a specific display."""
        display_state = self.display_clients.get(display_id)
        if not display_state:
            return

        task_was_running = False
        task = display_state.get('backpressure_task')
        if task and not task.done():
            data_logger.debug(f"Ensuring frame backpressure task for '{display_id}' is stopped.")
            task.cancel()
            try:
                await task
                task_was_running = True
            except asyncio.CancelledError:
                data_logger.debug(f"Backpressure task for '{display_id}' cancelled successfully.")
                task_was_running = True
            except Exception as e_cancel:
                data_logger.error(f"Error awaiting cancellation for '{display_id}' backpressure task: {e_cancel}")
            display_state['backpressure_task'] = None
        
        display_state['backpressure_enabled'] = True

        if task_was_running:
            data_logger.info(f"Backpressure task for '{display_id}' was stopped. Resetting its frame IDs.")
            await self._reset_frame_ids_and_notify(display_id)

    async def _reset_frame_ids_and_notify(self, display_id: str):
        """
        Resets frame IDs for a display. If it's the primary display,
        it broadcasts the reset to ALL clients.
        """
        display_state = self.display_clients.get(display_id)
        if not display_state:
            return

        data_logger.info(f"Resetting frame IDs for display '{display_id}'.")
        display_state['last_sent_frame_id'] = 0
        display_state['acknowledged_frame_id'] = -1
        
        message = f"PIPELINE_RESETTING {display_id}"
        
        if display_id == 'primary' and self.clients:
            data_logger.info(f"Broadcasting primary pipeline reset to all {len(self.clients)} clients: {message}")
            websockets.broadcast(self.clients, message)
        else:
            websocket = display_state.get('ws')
            if websocket:
                try:
                    await websocket.send(message)
                except websockets.ConnectionClosed:
                    data_logger.warning(f"Could not notify client for '{display_id}' of reset; connection closed.")
        
        display_state['backpressure_enabled'] = True
        display_state['last_ack_update_time'] = time.monotonic()

    async def _start_backpressure_task_if_needed(self, display_id: str):
        """Starts the backpressure task for a specific display if not already running."""
        # --- Disable for wayland mode ---
        if IS_WAYLAND:
            return

        display_state = self.display_clients.get(display_id)
        if not display_state:
            data_logger.error(f"Cannot start backpressure task: display '{display_id}' not found.")
            return

        await self._ensure_backpressure_task_is_stopped(display_id)

        task = display_state.get('backpressure_task')
        if not task or task.done():
            new_task = asyncio.create_task(self._run_frame_backpressure_logic(display_id))
            display_state['backpressure_task'] = new_task
            data_logger.info(f"New frame backpressure task started for display '{display_id}'.")
        else:
            data_logger.warning(f"Backpressure task for '{display_id}' was already running. Not starting a new one.")

    async def _run_frame_backpressure_logic(self, display_id: str):
        """The core backpressure and latency calculation loop for a single display."""
        data_logger.info(f"Frame-based backpressure logic task started for display '{display_id}'.")
        try:
            if self.client_settings_received:
                await self.client_settings_received.wait()
            data_logger.info(f"Client settings received, proceeding with backpressure loop for '{display_id}'.")

            while True:
                await asyncio.sleep(self.backpressure_check_interval_s)

                display_state = self.display_clients.get(display_id)
                if not display_state:
                    data_logger.warning(f"Backpressure task for '{display_id}' exiting: display no longer exists.")
                    break
                
                if display_id not in self.capture_instances:
                    if not display_state.get('backpressure_enabled', True):
                        data_logger.info(f"Backpressure LIFTED for '{display_id}' (video pipeline is not active).")
                    display_state['backpressure_enabled'] = True
                    continue

                current_server_frame_id = display_state.get('last_sent_frame_id', 0)
                last_client_acked_frame_id = display_state.get('acknowledged_frame_id', -1)

                if last_client_acked_frame_id == -1:
                    if not display_state.get('backpressure_enabled', True):
                         data_logger.info(f"Backpressure LIFTED for '{display_id}' (client ACK is -1).")
                    display_state['backpressure_enabled'] = True
                    display_state['last_ack_update_time'] = time.monotonic()
                    continue

                client_fps = display_state.get('latest_client_fps', 0.0)
                if client_fps <= 0:
                    client_fps = display_state.get('framerate', 60)

                server_id, client_id = current_server_frame_id, last_client_acked_frame_id

                if abs(server_id - client_id) > FRAME_ID_SUSPICIOUS_GAP_THRESHOLD:
                    display_state['backpressure_enabled'] = True
                    display_state['last_ack_update_time'] = time.monotonic()
                    continue
                
                if server_id == 0: continue

                frame_desync = (server_id - client_id) if server_id >= client_id else ((MAX_UINT16_FRAME_ID - client_id) + server_id + 1)
                allowed_desync_frames = (self.allowed_desync_ms / 1000.0) * client_fps
                current_rtt_ms = display_state.get('smoothed_rtt', 0.0)
                latency_adjustment_frames = (current_rtt_ms / 1000.0) * client_fps if current_rtt_ms > self.latency_threshold_for_adjustment_ms else 0
                effective_desync_frames = frame_desync - latency_adjustment_frames

                time_since_last_ack = time.monotonic() - display_state.get('last_ack_update_time', time.monotonic())
                
                if time_since_last_ack > STALLED_CLIENT_TIMEOUT_SECONDS:
                    if display_state.get('backpressure_enabled', True):
                        data_logger.warning(f"Client stall for '{display_id}': No ACK in {time_since_last_ack:.1f}s. Forcing backpressure.")
                    display_state['backpressure_enabled'] = False
                elif effective_desync_frames > allowed_desync_frames:
                    if display_state.get('backpressure_enabled', True):
                        data_logger.warning(f"Backpressure TRIGGERED for '{display_id}'. S:{server_id}, C:{client_id} (EffDesync:{effective_desync_frames:.1f}f > Allowed:{allowed_desync_frames:.1f}f).")
                    display_state['backpressure_enabled'] = False
                else:
                    if not display_state.get('backpressure_enabled', True):
                        data_logger.info(f"Backpressure LIFTED for '{display_id}'. S:{server_id}, C:{client_id} (EffDesync:{effective_desync_frames:.1f}f <= Allowed:{allowed_desync_frames:.1f}f).")
                    display_state['backpressure_enabled'] = True

        except asyncio.CancelledError:
            data_logger.info(f"Backpressure logic task for '{display_id}' cancelled.")
        finally:
            if 'display_state' in locals() and display_state:
                display_state['backpressure_enabled'] = True
            data_logger.info(f"Backpressure logic task for '{display_id}' finished.")

    async def broadcast_stream_resolution(self):
        """
        Broadcasts the primary display's resolution to ALL connected clients.
        """
        primary_client = self.display_clients.get('primary')
        if not primary_client:
            data_logger.warning("Cannot broadcast stream resolution: No primary client found.")
            return

        width = primary_client.get('width', 0)
        height = primary_client.get('height', 0)

        if width > 0 and height > 0 and self.clients:
            message = {
                "type": "stream_resolution",
                "width": width,
                "height": height,
            }
            message_str = json.dumps(message)
            data_logger.info(f"Broadcasting primary stream resolution to all clients: {message_str}")
            websockets.broadcast(self.clients, message_str)

    def _parse_settings_payload(self, payload_str: str) -> dict:
        settings_data = json.loads(payload_str)
        parsed = {}

        def get_int(k):
            v = settings_data.get(k)
            return int(v) if v is not None else None

        def get_bool(k):
            v = settings_data.get(k)
            return str(v).lower() == "true" if v is not None else None

        def get_str(k):
            v = settings_data.get(k)
            return str(v) if v is not None else None
        parsed["framerate"] = get_int("framerate")
        parsed["h264_crf"] = get_int("h264_crf")
        parsed["encoder"] = get_str("encoder")
        parsed["h264_fullcolor"] = get_bool("h264_fullcolor")
        parsed["h264_streaming_mode"] = get_bool("h264_streaming_mode")
        parsed["is_manual_resolution_mode"] = get_bool(
            "is_manual_resolution_mode"
        )
        parsed["manual_width"] = get_int(
            "manual_width"
        )
        parsed["manual_height"] = get_int(
            "manual_height"
        )
        parsed["audio_bitrate"] = get_int("audio_bitrate")
        parsed["initialClientWidth"] = get_int(
            "initialClientWidth"
        )
        parsed["initialClientHeight"] = get_int(
            "initialClientHeight"
        )
        parsed["jpeg_quality"] = get_int("jpeg_quality")
        parsed["paint_over_jpeg_quality"] = get_int(
            "paint_over_jpeg_quality"
        )
        parsed["use_cpu"] = get_bool("use_cpu")
        parsed["h264_paintover_crf"] = get_int("h264_paintover_crf")
        parsed["h264_paintover_burst_frames"] = get_int("h264_paintover_burst_frames")
        parsed["use_paint_over_quality"] = get_bool("use_paint_over_quality")
        parsed["scaling_dpi"] = get_int("scaling_dpi")
        parsed["enable_binary_clipboard"] = get_bool("enable_binary_clipboard")
        parsed["displayId"] = get_str("displayId")
        parsed["displayPosition"] = get_str("displayPosition")
        data_logger.debug(f"Parsed client settings: {parsed}")
        return parsed

    async def _apply_client_settings(
        self, websocket_obj, settings: dict, is_initial_settings: bool, client_role: str = "controller"
    ):

        if client_role == "viewer":
            data_logger.info(f"Ignoring SETTINGS payload from viewer {websocket_obj.remote_address}.")
            return

        display_id = settings.get("displayId", "primary")
        if display_id not in self.display_clients:
            data_logger.error(f"Cannot apply settings for unknown display_id '{display_id}'")
            return
        display_state = self.display_clients[display_id]
        data_logger.info(
            f"Applying and sanitizing client settings for '{display_id}' (initial={is_initial_settings})"
        )
        def sanitize_value(name, client_value):
            """Clamps ranges, validates enums, and enforces bools against server limits."""
            setting_def = next((s for s in SETTING_DEFINITIONS if s['name'] == name), None)
            if not setting_def:
                return None
            server_limit = getattr(self.cli_args, name)
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
                        data_logger.warning(f"Client value for '{name}' ({client_value}) was clamped to {sanitized} (server range: {min_val}-{max_val}).")
                    return sanitized
                elif setting_def['type'] == 'enum':
                    allowed_values = setting_def['meta']['allowed']
                    if str(client_value) in allowed_values:
                        return client_value
                    server_default = allowed_values[0] if allowed_values else setting_def['default']
                    data_logger.warning(f"Client value for '{name}' ('{client_value}') is not in the allowed list {allowed_values}. Using server default '{server_default}'.")
                    return server_default
                elif setting_def['type'] == 'bool':
                    server_val, is_locked = server_limit
                    client_bool = str(client_value).lower() in ['true', '1']
                    if is_locked:
                        if client_bool != server_val:
                            data_logger.warning(f"Client tried to change locked setting '{name}' to '{client_bool}'. Request ignored, using server value '{server_val}'.")
                        return server_val
                    return client_bool
            except (ValueError, TypeError, IndexError):
                def_val_meta = setting_def.get('meta', {}).get('default_value')
                return def_val_meta if def_val_meta is not None else setting_def.get('default')
            return client_value
        should_restart_video = False
        audio_restart_needed = False
        async with self._reconfigure_lock:
            old_settings = display_state.copy()
            old_display_width = display_state.get("width", 0)
            old_display_height = display_state.get("height", 0)
            old_position = display_state.get('position', 'right')
            new_position = settings.get("displayPosition", "right")
            target_w = None
            target_h = None
            server_is_manual, _ = self.cli_args.is_manual_resolution_mode
            client_wants_manual = sanitize_value("is_manual_resolution_mode", settings.get("is_manual_resolution_mode"))
            if server_is_manual:
                data_logger.info(f"Server override is active. Forcing manual resolution from server configuration for display '{display_id}'.")
                try:
                    w_val = self.cli_args.manual_width
                    h_val = self.cli_args.manual_height
                    target_w = int(w_val[0] if isinstance(w_val, (list, tuple)) else w_val)
                    target_h = int(h_val[0] if isinstance(h_val, (list, tuple)) else h_val)
                    data_logger.info(f"Server override: Applying manual resolution {target_w}x{target_h}.")
                except (ValueError, TypeError, IndexError) as e:
                    data_logger.error(f"Server override failed: Could not parse manual resolution from server config. Error: {e}. Falling back.")
                    target_w = 1024
                    target_h = 768
            elif client_wants_manual:
                data_logger.info(f"Client has requested manual resolution mode for display '{display_id}'.")
                target_w = sanitize_value("manual_width", settings.get("manual_width"))
                target_h = sanitize_value("manual_height", settings.get("manual_height"))
            elif is_initial_settings:
                target_w = settings.get("initialClientWidth")
                target_h = settings.get("initialClientHeight")
            if not isinstance(target_w, int) or target_w <= 0:
                target_w = old_display_width if old_display_width > 0 else 1024
            if not isinstance(target_h, int) or target_h <= 0:
                target_h = old_display_height if old_display_height > 0 else 768
            if target_w % 2 != 0: target_w -= 1
            if target_h % 2 != 0: target_h -= 1
            resolution_actually_changed = (target_w != old_display_width or target_h != old_display_height)
            position_actually_changed = (new_position != old_position)
            if resolution_actually_changed or position_actually_changed:
                display_state['width'] = target_w
                display_state['height'] = target_h
                display_state['position'] = new_position
                if display_id == 'primary':
                    self.app.display_width = target_w
                    self.app.display_height = target_h
            display_state["encoder"] = sanitize_value("encoder", settings.get("encoder"))
            display_state["framerate"] = sanitize_value("framerate", settings.get("framerate"))
            display_state["h264_crf"] = sanitize_value("h264_crf", settings.get("h264_crf"))
            display_state["h264_fullcolor"] = sanitize_value("h264_fullcolor", settings.get("h264_fullcolor"))
            display_state["h264_streaming_mode"] = sanitize_value("h264_streaming_mode", settings.get("h264_streaming_mode"))
            display_state["jpeg_quality"] = sanitize_value("jpeg_quality", settings.get("jpeg_quality"))
            display_state["paint_over_jpeg_quality"] = sanitize_value("paint_over_jpeg_quality", settings.get("paint_over_jpeg_quality"))
            display_state["use_paint_over_quality"] = sanitize_value("use_paint_over_quality", settings.get("use_paint_over_quality"))
            display_state["h264_paintover_crf"] = sanitize_value("h264_paintover_crf", settings.get("h264_paintover_crf"))
            display_state["h264_paintover_burst_frames"] = sanitize_value("h264_paintover_burst_frames", settings.get("h264_paintover_burst_frames"))
            display_state["use_cpu"] = sanitize_value("use_cpu", settings.get("use_cpu"))
            self.app.audio_bitrate = sanitize_value("audio_bitrate", settings.get("audio_bitrate"))
            display_state["audio_bitrate"] = self.app.audio_bitrate
            if self.input_handler:
                self.enable_binary_clipboard = sanitize_value("enable_binary_clipboard", settings.get("enable_binary_clipboard"))
                await self.input_handler.update_binary_clipboard_setting(self.enable_binary_clipboard)
            new_dpi = sanitize_value("scaling_dpi", settings.get("scaling_dpi"))
            if new_dpi is not None and new_dpi != old_settings.get("scaling_dpi"):
                data_logger.info(f"DPI changed from {old_settings.get('scaling_dpi')} to {new_dpi}. Applying system-level change.")
                await set_dpi(new_dpi)
                if CURSOR_SIZE > 0 and not IS_WAYLAND:
                    new_cursor_size = max(1, int(round(int(new_dpi) / 96.0 * CURSOR_SIZE)))
                    await set_cursor_size(new_cursor_size)
            display_state["scaling_dpi"] = new_dpi
            dimensional_change = resolution_actually_changed or position_actually_changed
            video_params_changed = any(
                display_state.get(key) != old_settings.get(key)
                for key in [
                    'encoder', 'framerate', 'h264_crf', 'h264_fullcolor', 'h264_streaming_mode',
                    'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu', 'h264_paintover_crf',
                    'h264_paintover_burst_frames', 'use_paint_over_quality'
                ]
            )
            audio_bitrate_changed = self.app.audio_bitrate != old_settings.get('audio_bitrate')
            if audio_bitrate_changed and self.is_pcmflux_capturing:
                audio_restart_needed = True
        if audio_restart_needed:
            data_logger.info("Restarting audio pipeline due to settings update.")
            await self._stop_pcmflux_pipeline()
            await self._start_pcmflux_pipeline()
        if is_initial_settings or dimensional_change:
            data_logger.info(
                f"Initial setup or dimensional change detected for '{display_id}'. "
                "Performing full display reconfiguration."
            )
            await self.reconfigure_displays()
        elif video_params_changed:
            data_logger.info(
                f"Video parameters changed for '{display_id}'. "
                "Restarting its capture stream without reconfiguring displays."
            )
            if hasattr(self, 'display_layouts') and display_id in self.display_layouts:
                layout = self.display_layouts[display_id]
                await self._stop_capture_for_display(display_id)
                await self._start_capture_for_display(
                    display_id=display_id,
                    width=layout['w'], height=layout['h'],
                    x_offset=layout['x'], y_offset=layout['y']
                )
                await self._start_backpressure_task_if_needed(display_id)
            else:
                data_logger.warning(
                    f"Cannot restart capture for '{display_id}': no layout found. "
                    "Triggering full reconfiguration as a fallback."
                )
                await self.reconfigure_displays()
        if is_initial_settings and self.client_settings_received and not self.client_settings_received.is_set():
            self.client_settings_received.set()

    async def ws_handler(self, websocket):
        if self.is_secure_mode:
            await config_gate.wait()

            query_params = urllib.parse.parse_qs(urllib.parse.urlparse(websocket.request.path).query)
            token = query_params.get('token', [None])[0]

            if not token or token not in user_tokens:
                data_logger.warning(f"Rejecting connection from {websocket.remote_address}: Missing or invalid token.")
                await websocket.close(code=4001, reason="Invalid authentication token")
                return

            permissions = user_tokens[token]
            client_permissions[websocket] = {
                "token": token,
                "role": permissions.get("role"),
                "slot": permissions.get("slot"),
            }
            data_logger.info(f"Client {websocket.remote_address} authenticated with token. Role: {permissions.get('role')}, Slot: {permissions.get('slot')}")
            auth_success_payload = json.dumps({
                "role": permissions.get("role"),
                "slot": permissions.get("slot"),
            })
            await websocket.send(f"AUTH_SUCCESS,{auth_success_payload}")
        else:
            path_hash = urllib.parse.urlparse(websocket.request.path).fragment
            role = "controller"
            slot = None
            if path_hash == "shared":
                role = "viewer"
            elif path_hash == "collab":
                role = "controller"
            elif path_hash.startswith("player"):
                try:
                    slot_num = int(path_hash[6:])
                    if 2 <= slot_num <= 4:
                        role = "viewer"
                        slot = slot_num
                except (ValueError, IndexError):
                    pass
            client_permissions[websocket] = {"token": None, "role": role, "slot": slot}
            data_logger.info(f"Legacy client {websocket.remote_address} connected. Role: {role}, Slot: {slot}")

        global TARGET_FRAMERATE
        current_time = time.monotonic()
        ip_address, _ = websocket.remote_address
        last_time = self.last_connection_times.get(ip_address)
        if last_time:
            elapsed_ms = (current_time - last_time) * 1000
            if elapsed_ms < self.RECONNECT_DEBOUNCE_MS:
                data_logger.warning(
                    f"Client {ip_address} reconnecting too quickly ({elapsed_ms:.1f}ms). Rejecting connection."
                )
                await websocket.close(code=4029, reason="Rate limited: reconnecting too quickly")
                return
        self.last_connection_times[ip_address] = current_time
        if len(self.last_connection_times) > self.MAX_RECENT_CLIENTS:
            self.last_connection_times.popitem(last=False)
        raddr = websocket.remote_address
        data_logger.info(f"Data WebSocket connected from {raddr}")
        self.clients.add(websocket)
        self.data_ws = (
            websocket 
        )
        self.capture_loop = self.capture_loop or asyncio.get_running_loop()
        initial_settings_processed = False
        self._sent_frame_timestamps.clear()
        self._rtt_samples.clear()
        self._smoothed_rtt_ms = 0.0

        client_display_id = None

        try:
            await websocket.send(f"MODE {self.mode}")
        except websockets.exceptions.ConnectionClosed:
            self.clients.discard(websocket)
            if self.data_ws is websocket:
                self.data_ws = None
            return

        if self.app and self.app.last_cursor_sent:
            data_logger.info(f"Sending last known cursor to new client {raddr}")
            try:
                msg_str = json.dumps(self.app.last_cursor_sent)
                await websocket.send(f"cursor,{msg_str}")
            except Exception as e:
                data_logger.warning(f"Failed to send initial cursor to new client {raddr}: {e}")

        server_settings_payload = {"type": "server_settings", "settings": {}}
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
        try:
            await websocket.send(json.dumps(server_settings_payload))
        except websockets.exceptions.ConnectionClosed:
            self.clients.discard(websocket)
            if self.data_ws is websocket:
                self.data_ws = None
            return

        self._last_adjustment_time = self._last_time_client_ok = time.monotonic()
        self._active_pipeline_last_sent_frame_id = 0
        self._client_acknowledged_frame_id = -1
        self._last_client_acknowledged_frame_id_update_time = time.monotonic()
        self._previous_ack_id_for_stall_check = -1
        self._previous_sent_id_for_stall_check = -1
        self._last_client_stable_report_time = time.monotonic()

        self._backpressure_send_frames_enabled = True
        active_uploads_by_path_conn = {}
        active_upload_target_path_conn = None
        upload_dir_valid = upload_dir_path is not None
        
        mic_setup_done = False 
        pa_module_index = None
        pa_stream = None
        pulse = None
        
        # Audio buffer management
        audio_buffer = []
        buffer_max_size = 24000 * 2 * 2  # 2 seconds at 24kHz, 16-bit mono
        
        # Define virtual source details
        virtual_source_name = "SelkiesVirtualMic"
        master_monitor = "input.monitor"

        if not self.input_handler:
            logger.error(
                f"Data WS handler for {raddr}: Critical - self.input_handler (global) is not set. Input processing will fail."
            )

        self._shared_stats_ws = {}
        gpu_id_for_stats = getattr(self.app, "gpu_id", GPU_ID_DEFAULT)
        self._system_monitor_task_ws = asyncio.create_task(
            _collect_system_stats_ws(self._shared_stats_ws)
        )
        if GPUtil.getGPUs():
            self._gpu_monitor_task_ws = asyncio.create_task(
                _collect_gpu_stats_ws(self._shared_stats_ws, gpu_id=gpu_id_for_stats)
            )
        self._stats_sender_task_ws = asyncio.create_task(
            _send_stats_periodically_ws(
                websocket, self._shared_stats_ws
            )
        )
        self._network_monitor_task_ws = asyncio.create_task(
            _collect_network_stats_ws(self._shared_stats_ws, self)
        )

        try:
            if PULSEAUDIO_AVAILABLE:
                try:
                    data_logger.info("Attempting to establish PulseAudio connection...")
                    pulse = pulsectl.Pulse("selkies-mic-handler")
                    data_logger.info("PulseAudio connection established.")
                except Exception as e_pa_conn:
                    data_logger.error(
                        f"Initial PulseAudio connection failed: {e_pa_conn}",
                        exc_info=True,
                    )
                    pulse = None

            async for message in websocket:
                if isinstance(message, bytes):
                    msg_type, payload = message[0], message[1:]
                    if msg_type == 0x01:
                        if (
                            active_upload_target_path_conn
                            and active_upload_target_path_conn
                            in active_uploads_by_path_conn
                        ):
                            try:
                                active_uploads_by_path_conn[
                                    active_upload_target_path_conn
                                ].write(payload)
                            except Exception as e_write:
                                data_logger.error(
                                    f"File write error for {active_upload_target_path_conn}: {e_write}"
                                )
                                try:
                                    active_uploads_by_path_conn[
                                        active_upload_target_path_conn
                                    ].close()
                                    os.remove(active_upload_target_path_conn)
                                except Exception:
                                    pass
                                del active_uploads_by_path_conn[
                                    active_upload_target_path_conn
                                ]
                                active_upload_target_path_conn = None
                    elif msg_type == 0x02:  # Mic data
                        if not settings.microphone_enabled[0]:
                            continue
                        if not PULSEAUDIO_AVAILABLE:
                            if len(payload) > 0:
                                data_logger.warning(
                                    "PulseAudio library not available. Skipping microphone data."
                                )
                            continue
                        if pulse is None:
                            if len(payload) > 0:
                                data_logger.warning(
                                    "PulseAudio client not connected. Skipping microphone data."
                                )
                            continue

                        if not mic_setup_done:
                            data_logger.info(
                                "Performing PulseAudio virtual microphone setup check..."
                            )
                            try:
                                existing_source_info = None
                                source_list = pulse.source_list()
                                for source_obj in source_list:
                                    if source_obj.name == virtual_source_name:
                                        existing_source_info = source_obj
                                        break

                                if existing_source_info:
                                    data_logger.info(
                                        f"Virtual source '{virtual_source_name}' (Index: {existing_source_info.index}) already exists."
                                    )
                                    actual_master = existing_source_info.proplist.get(
                                        "device.master_device"
                                    )
                                    if actual_master == master_monitor:
                                        data_logger.info(
                                            f"Existing source correctly linked to '{master_monitor}'."
                                        )
                                    else:
                                        data_logger.warning(
                                            f"Existing source '{virtual_source_name}' linked to '{actual_master}' not '{master_monitor}'. Manual fix may be needed."
                                        )
                                    pa_module_index = (
                                        existing_source_info.owner_module
                                    )
                                    mic_setup_done = True
                                else:
                                    data_logger.info(
                                        f"Virtual source '{virtual_source_name}' not found. Attempting to load module..."
                                    )
                                    load_args = f"source_name={virtual_source_name} master={master_monitor}"
                                    pa_module_index = pulse.module_load(
                                        "module-virtual-source", load_args
                                    )
                                    data_logger.info(
                                        f"Loaded module-virtual-source with index {pa_module_index} for '{virtual_source_name}'."
                                    )

                                    new_source_info = None
                                    source_list_after_load = pulse.source_list()
                                    for source_obj_after in source_list_after_load:
                                        if source_obj_after.name == virtual_source_name:
                                            new_source_info = source_obj_after
                                            break
                                    if new_source_info:
                                        data_logger.info(
                                            f"Successfully verified creation of source '{virtual_source_name}' (Index: {new_source_info.index})."
                                        )
                                        mic_setup_done = True
                                    else:
                                        data_logger.error(
                                            f"Loaded module {pa_module_index} but failed to find source '{virtual_source_name}'."
                                        )
                                        if (
                                            pa_module_index is not None
                                        ):
                                            try:
                                                pulse.module_unload(pa_module_index)
                                            except Exception as unload_err:
                                                data_logger.error(
                                                    f"Failed to unload module {pa_module_index}: {unload_err}"
                                                )
                                            pa_module_index = None

                                if mic_setup_done:
                                    current_source_list = (
                                        pulse.source_list()
                                    )
                                    if self.is_pcmflux_capturing:
                                        try:
                                            source_outputs = pulse.source_output_list()
                                            pcmflux_output = None
                                            
                                            for output in source_outputs:
                                                if hasattr(output, 'proplist') and output.proplist.get('application.name') == 'pcmflux':
                                                    pcmflux_output = output
                                                    break
                                            
                                            if pcmflux_output:
                                                connected_source = None
                                                for source in current_source_list:
                                                    if source.index == pcmflux_output.source:
                                                        connected_source = source
                                                        break
                                                if connected_source and connected_source.name != self.audio_device_name:
                                                    data_logger.warning(
                                                        f"pcmflux connected to wrong source '{connected_source.name}', moving to '{self.audio_device_name}'"
                                                    )
                                                    correct_source = None
                                                    for source in current_source_list:
                                                        if source.name == self.audio_device_name:
                                                            correct_source = source
                                                            break
                                                    if correct_source:
                                                        pulse.source_output_move(pcmflux_output.index, correct_source.index)
                                                        data_logger.info(
                                                            f"Successfully moved pcmflux from '{connected_source.name}' to '{self.audio_device_name}'"
                                                        )
                                                    else:
                                                        data_logger.error(
                                                            f"Could not find source '{self.audio_device_name}' to move pcmflux to"
                                                        )
                                                elif connected_source:
                                                    data_logger.info(f"pcmflux correctly connected to '{connected_source.name}'")
                                            else:
                                                data_logger.debug("Could not find pcmflux in source outputs")
                                                
                                        except Exception as e:
                                            data_logger.error(f"Error checking/fixing pcmflux source: {e}")
                                    
                                    data_logger.info(
                                        f"Virtual microphone '{virtual_source_name}' is ready for microphone forwarding."
                                    )

                            except Exception as e_pa_setup:
                                data_logger.error(
                                    f"PulseAudio mic setup error: {e_pa_setup}",
                                    exc_info=True,
                                )
                                mic_setup_done = False
                                if pa_module_index is not None:
                                    try:
                                        data_logger.info(
                                            f"Attempting to unload module {pa_module_index} due to setup error."
                                        )
                                        pulse.module_unload(pa_module_index)
                                    except Exception as e_unload_err:
                                        data_logger.error(
                                            f"Error unloading module {pa_module_index} after setup failure: {e_unload_err}"
                                        )
                                    pa_module_index = None
                                continue

                        if not mic_setup_done or not payload:
                            if not mic_setup_done and len(payload) > 0:
                                data_logger.warning(
                                    "Mic setup not complete, skipping mic data."
                                )
                            continue

                        try:
                            if pa_stream is None:
                                data_logger.info(
                                    f"Opening new pasimple playback stream to 'input' at 24000 Hz (s16le, mono)."
                                )
                                pa_stream = pasimple.PaSimple(
                                    pasimple.PA_STREAM_PLAYBACK,
                                    pasimple.PA_SAMPLE_S16LE,
                                    1,
                                    24000,
                                    "SelkiesClientMic",
                                    "MicStream",
                                    device_name="input",
                                )
                            
                            audio_buffer.extend(payload)
                            
                            if len(audio_buffer) > buffer_max_size:
                                audio_buffer = audio_buffer[len(audio_buffer)//2:]
                                data_logger.warning("Audio buffer overflow, dropping old audio to prevent drift")
                            
                            if pa_stream and len(audio_buffer) >= len(payload):
                                chunk_size = len(payload)
                                data_to_write = bytes(audio_buffer[:chunk_size])
                                audio_buffer[:chunk_size] = []
                                pa_stream.write(data_to_write)
                                    
                        except Exception as e_pa_write:
                            data_logger.error(
                                f"PulseAudio stream write error: {e_pa_write}",
                                exc_info=False,
                            )
                            if pa_stream:
                                try:
                                    pa_stream.close()
                                except:
                                    pass
                            audio_buffer.clear()

                elif isinstance(message, str):
                    if message.startswith("SETTINGS,"):
                        client_perms = client_permissions.get(websocket)
                        client_role = client_perms.get("role") if client_perms else "controller"

                    perms = client_permissions.get(websocket)
                    if perms and perms.get("role") == "viewer":
                        allowed_viewer_prefixes = [
                            "SETTINGS,",
                            "START_VIDEO",
                            "js,",
                        ]
                        if active_mk_token and perms.get("token") == active_mk_token:
                            allowed_viewer_prefixes.extend(["kd", "ku", "kr", "m", "m2", "cws", "cbs", "cwd", "cbd", "cwe", "cbe", "cw", "cb", "cr"])
                        if not any(message.startswith(prefix) for prefix in allowed_viewer_prefixes):
                            data_logger.warning(f"DENIED unauthorized message from viewer {websocket.remote_address}: {message[:100]}...")
                            continue

                    if message.startswith("FILE_UPLOAD_START:"):
                        if 'upload' not in settings.file_transfers:
                            data_logger.warning("Client tried to upload a file, but uploads are disabled by server settings.")
                            continue
                        if not upload_dir_valid:
                            data_logger.error("Upload dir invalid, skipping upload.")
                            continue
                        try:
                            _, rel_path_from_client, size_str = message.split(":", 2)
                            file_size = int(size_str)

                            sane_rel_path = rel_path_from_client.strip('/\\')
                            sane_rel_path = os.path.normpath(sane_rel_path)

                            path_components = [comp for comp in sane_rel_path.split(os.sep) if comp and comp != '.']

                            if not path_components or \
                               sane_rel_path.startswith(os.sep) or \
                               sane_rel_path.startswith('/') or \
                               sane_rel_path.startswith('\\') or \
                               ".." in path_components:
                                data_logger.error(f"Invalid or malicious relative path from client: '{rel_path_from_client}'. Discarding.")
                                continue
                            
                            sane_rel_path = os.path.join(*path_components)

                            final_server_path = os.path.join(upload_dir_path, sane_rel_path)

                            real_upload_dir = os.path.realpath(upload_dir_path)
                            intended_parent_dir_abs = os.path.abspath(os.path.dirname(final_server_path))
                            real_upload_dir_abs = os.path.abspath(real_upload_dir)

                            if not intended_parent_dir_abs.startswith(real_upload_dir_abs):
                                 data_logger.error(f"Path escape attempt detected: '{final_server_path}' (from client: '{rel_path_from_client}') is outside of '{real_upload_dir_abs}'. Discarding.")
                                 continue

                            target_dir = os.path.dirname(final_server_path)
                            
                            if target_dir and target_dir != real_upload_dir_abs and not os.path.exists(target_dir):
                                if not os.path.abspath(target_dir).startswith(real_upload_dir_abs):
                                    data_logger.error(f"Directory creation escape attempt: '{target_dir}' is outside of '{real_upload_dir_abs}'. Discarding.")
                                    continue
                                try:
                                    os.makedirs(target_dir, exist_ok=True)
                                    data_logger.info(f"Created directory for upload: {target_dir}")
                                except OSError as e_mkdir:
                                    data_logger.error(f"Could not create directory {target_dir} for upload: {e_mkdir}")
                                    continue
                            
                            if (
                                active_upload_target_path_conn
                                and active_upload_target_path_conn
                                in active_uploads_by_path_conn
                            ):
                                try:
                                    active_uploads_by_path_conn[active_upload_target_path_conn].close()
                                except Exception as e_close_old:
                                    data_logger.warning(f"Error closing previous upload stream {active_upload_target_path_conn}: {e_close_old}")
                                del active_uploads_by_path_conn[active_upload_target_path_conn]

                            active_uploads_by_path_conn[final_server_path] = open(final_server_path, "wb")
                            active_upload_target_path_conn = final_server_path
                            data_logger.info(
                                f"Upload started: {final_server_path} (client rel_path: '{rel_path_from_client}', size: {file_size})"
                            )
                        except ValueError:
                            data_logger.error(
                                f"Invalid FILE_UPLOAD_START format: {message}"
                            )
                        except Exception as e_fup_start:
                            data_logger.error(
                                f"FILE_UPLOAD_START processing error: {e_fup_start}", exc_info=True
                            )

                    elif message.startswith("FILE_UPLOAD_END:"):
                        if (
                            active_upload_target_path_conn
                            and active_upload_target_path_conn
                            in active_uploads_by_path_conn
                        ):
                            active_uploads_by_path_conn[
                                active_upload_target_path_conn
                            ].close()
                            data_logger.info(
                                f"Upload finished: {active_upload_target_path_conn}"
                            )
                            del active_uploads_by_path_conn[
                                active_upload_target_path_conn
                            ]
                        active_upload_target_path_conn = None

                    elif message.startswith("FILE_UPLOAD_ERROR:"):
                        data_logger.error(f"Client reported upload error: {message}")
                        if (
                            active_upload_target_path_conn
                            and active_upload_target_path_conn
                            in active_uploads_by_path_conn
                        ):
                            active_uploads_by_path_conn[
                                active_upload_target_path_conn
                            ].close()
                            try:
                                os.remove(active_upload_target_path_conn)
                            except OSError:
                                pass
                            del active_uploads_by_path_conn[
                                active_upload_target_path_conn
                            ]
                        active_upload_target_path_conn = None

                    elif message.startswith("SETTINGS,"):
                        try:
                            _, payload_str = message.split(",", 1)
                            parsed_settings = self._parse_settings_payload(payload_str)
                            display_id = parsed_settings.get("displayId", "primary")

                            client_perms = client_permissions.get(websocket)
                            client_role = client_perms.get("role") if client_perms else "controller"

                            if client_role == 'viewer':
                                data_logger.info(f"Viewer client {websocket.remote_address} sent initial SETTINGS. Syncing with current stream state.")
                                if not initial_settings_processed:
                                    initial_settings_processed = True
                                
                                await self.broadcast_stream_resolution()

                                data_logger.info(f"Broadcasting PIPELINE_RESETTING to sync new viewer.")
                                websockets.broadcast(self.clients, "PIPELINE_RESETTING primary")

                                continue

                            if display_id != 'primary':
                                second_screen_enabled, _ = self.cli_args.second_screen
                                if not second_screen_enabled:
                                    data_logger.warning(
                                        f"Client from {websocket.remote_address} attempted to connect as secondary display ('{display_id}'), "
                                        "but second screens are disabled by server settings. Rejecting connection."
                                    )
                                    try:
                                        await websocket.send("KILL Second screens are disabled on this server.")
                                        await websocket.close(code=1008, reason="Second screens disabled")
                                    except websockets.ConnectionClosed:
                                        pass
                                    return
 
                            client_display_id = display_id
                            if display_id in ['primary', 'display2']:
                                existing_client_info = self.display_clients.get(display_id)
                                if existing_client_info:
                                    old_ws = existing_client_info.get('ws')
                                    if old_ws and old_ws is not websocket and old_ws.state == websockets.protocol.State.OPEN:
                                        kill_reason = f"a new {display_id} client connected connection killed"
                                        data_logger.warning(
                                            f"Killing old client for '{display_id}' at {old_ws.remote_address}. Reason: {kill_reason}"
                                        )
                                        try:
                                            await old_ws.send(f"KILL {kill_reason}")
                                            await old_ws.close(code=1000, reason="Superseded by new client")
                                        except websockets.ConnectionClosed:
                                            data_logger.info(f"Old client for '{display_id}' was already disconnected.")
                                        except Exception as e:
                                            data_logger.error(f"Error while killing old client for '{display_id}': {e}")
                            if display_id != 'primary':
                                old_secondary_id = None
                                for existing_id, client_data in self.display_clients.items():
                                    if existing_id != 'primary' and client_data.get('ws') is not websocket:
                                        old_secondary_id = existing_id
                                        break
                                
                                if old_secondary_id:
                                    data_logger.warning(
                                        f"New secondary display '{display_id}' connected. "
                                        f"Deactivating old secondary '{old_secondary_id}'."
                                    )
                                    old_secondary_client = self.display_clients.get(old_secondary_id)
                                    if old_secondary_client:
                                        await self._stop_capture_for_display(old_secondary_id)
                                        old_secondary_client['video_active'] = False
                                        old_ws = old_secondary_client.get('ws')
                                        if old_ws:
                                            try:
                                                await old_ws.send("VIDEO_STOPPED")
                                            except websockets.ConnectionClosed:
                                                pass
                            if display_id not in self.display_clients:
                                data_logger.info(f"Registering new client for display: {display_id}")
                                self.display_clients[display_id] = {
                                    'ws': websocket, 
                                    'width': 0, 'height': 0, 'position': 'right',
                                    'acknowledged_frame_id': -1,
                                    'last_sent_frame_id': 0,
                                    'sent_timestamps': OrderedDict(),
                                    'rtt_samples': deque(maxlen=RTT_SMOOTHING_SAMPLES),
                                    'smoothed_rtt': 0.0,
                                    'backpressure_enabled': True,
                                    'backpressure_task': None,
                                    'last_ack_update_time': time.monotonic(),
                                    'latest_client_fps': 0.0,
                                    'video_active': True,
                                    'encoder': self.app.encoder,
                                    'framerate': self.app.framerate,
                                    'h264_crf': self._initial_h264_crf,
                                    'h264_fullcolor': self._initial_h264_fullcolor,
                                    'h264_streaming_mode': self._initial_h264_streaming_mode,
                                    'jpeg_quality': self._initial_jpeg_quality,
                                    'paint_over_jpeg_quality': self._initial_paint_over_jpeg_quality,
                                    'use_cpu': self._initial_use_cpu,
                                    'h264_paintover_crf': self._initial_h264_paintover_crf,
                                    'h264_paintover_burst_frames': self._initial_h264_paintover_burst_frames,
                                    'use_paint_over_quality': self._initial_use_paint_over_quality,
                                    'scale': 1.0,
                                }
                            else:
                                data_logger.info(f"Client is taking over existing display '{display_id}'. Updating state for new connection.")
                                display_state = self.display_clients[display_id]
                                display_state['ws'] = websocket
                                display_state['video_active'] = True
                                display_state['acknowledged_frame_id'] = -1
                                display_state['last_ack_update_time'] = time.monotonic()
                                display_state['sent_timestamps'].clear()
                                display_state['rtt_samples'].clear()
                                display_state['smoothed_rtt'] = 0.0
 
                            await self._apply_client_settings(
                                websocket,
                                parsed_settings,
                                not initial_settings_processed,
                                client_role
                            )
                            if not initial_settings_processed:
                                initial_settings_processed = True
                                data_logger.info("Initial client settings message processed by ws_handler.")
                                video_is_active = len(self.capture_instances) > 0
                                if not video_is_active:
                                    data_logger.error(f"FATAL: Initial reconfiguration completed, but video pipeline did not start.")
                                async with self._reconfigure_lock:
                                    audio_is_active = self.is_pcmflux_capturing
                                    if not audio_is_active and PCMFLUX_AVAILABLE and display_id == 'primary':
                                        data_logger.info("Initial setup: Primary client connected, audio not active, attempting start.")
                                        await self._start_pcmflux_pipeline()
                                    elif not PCMFLUX_AVAILABLE and not audio_is_active:
                                         data_logger.warning("Initial setup: Audio pipeline (server-to-client) cannot be started (pcmflux not available).")

                        except json.JSONDecodeError:
                            data_logger.error(f"SETTINGS JSON decode error: {message}")
                        except Exception as e_set:
                            data_logger.error(
                                f"Error processing SETTINGS: {e_set}", exc_info=True
                            )

                    elif message.startswith("CLIENT_FRAME_ACK"):
                        try:
                            parts = message.split(" ", 2)
                            acked_frame_id = -1
                            target_display_id = client_display_id
                            if not target_display_id:
                                continue
                            if len(parts) >= 2:
                                acked_frame_id = int(parts[-1])
                            else:
                                raise ValueError("ACK message has too few parts.")

                            display_state = self.display_clients.get(target_display_id)
                            if display_state:
                                display_state['acknowledged_frame_id'] = acked_frame_id
                                display_state['last_ack_update_time'] = time.monotonic()
                                
                                sent_ts = display_state.get('sent_timestamps')
                                if sent_ts and acked_frame_id in sent_ts:
                                    send_time = sent_ts.pop(acked_frame_id)
                                    rtt_sample_ms = (time.monotonic() - send_time) * 1000.0
                                    if rtt_sample_ms >= 0:
                                        rtt_samples = display_state.get('rtt_samples')
                                        if rtt_samples is not None:
                                            rtt_samples.append(rtt_sample_ms)
                                            if rtt_samples:
                                                display_state['smoothed_rtt'] = sum(rtt_samples) / len(rtt_samples)
                        except (IndexError, ValueError):
                            data_logger.warning(f"Malformed CLIENT_FRAME_ACK from {raddr}: {message}")

                    elif message.startswith("FILE_UPLOAD_END:"):
                        if (
                            active_upload_target_path_conn
                            and active_upload_target_path_conn
                            in active_uploads_by_path_conn
                        ):
                            active_uploads_by_path_conn[
                                active_upload_target_path_conn
                            ].close()
                            data_logger.info(
                                f"Upload finished: {active_upload_target_path_conn}"
                            )
                            del active_uploads_by_path_conn[
                                active_upload_target_path_conn
                            ]
                        active_upload_target_path_conn = None

                    elif message.startswith("FILE_UPLOAD_ERROR:"):
                        data_logger.error(f"Client reported upload error: {message}")
                        if (
                            active_upload_target_path_conn
                            and active_upload_target_path_conn
                            in active_uploads_by_path_conn
                        ):
                            active_uploads_by_path_conn[
                                active_upload_target_path_conn
                            ].close()
                            try:
                                os.remove(active_upload_target_path_conn)
                            except OSError:
                                pass
                            del active_uploads_by_path_conn[
                                active_upload_target_path_conn
                            ]
                        active_upload_target_path_conn = None

                    elif message == "START_VIDEO":
                        perms = client_permissions.get(websocket)
                        if perms and perms.get("role") == "viewer":
                            now = time.time()
                            last_req_time = self.last_start_video_request_times.get(websocket, 0)
                            if now - last_req_time < 30.0:
                                data_logger.warning(f"Throttled START_VIDEO request from viewer {websocket.remote_address}. Ignoring.")
                                continue
                            self.last_start_video_request_times[websocket] = now

                        if client_display_id and client_display_id in self.display_clients:
                            data_logger.info(f"Received START_VIDEO for '{client_display_id}'. Starting its stream.")
                            display_state = self.display_clients[client_display_id]
                            display_state['video_active'] = True
                            if hasattr(self, 'display_layouts') and client_display_id in self.display_layouts:
                                layout = self.display_layouts[client_display_id]
                                data_logger.info(f"Found existing layout for '{client_display_id}'. Starting capture with: {layout}")
                                try:
                                    await self._start_capture_for_display(
                                        display_id=client_display_id,
                                        width=layout['w'], height=layout['h'],
                                        x_offset=layout['x'], y_offset=layout['y']
                                    )
                                    await self._start_backpressure_task_if_needed(client_display_id)
                                    await websocket.send("VIDEO_STARTED")
                                except Exception as e:
                                    data_logger.error(f"Failed to restart individual stream for '{client_display_id}': {e}", exc_info=True)
                                    await self.reconfigure_displays()
                            else:
                                data_logger.warning(f"No layout found for '{client_display_id}' on START_VIDEO. Performing full reconfiguration.")
                                await self.reconfigure_displays()
                                await websocket.send("VIDEO_STARTED")
                        else:
                            data_logger.info(f"Received START_VIDEO from a shared client ({websocket.remote_address}). Triggering reconfiguration.")
                            await self.reconfigure_displays()

                    elif message == "STOP_VIDEO":
                        if client_display_id and client_display_id in self.display_clients:
                            data_logger.info(f"Received STOP_VIDEO for '{client_display_id}'. Stopping stream.")
                            self.display_clients[client_display_id]['video_active'] = False
                            
                            await self._stop_capture_for_display(client_display_id)
                            try:
                                await websocket.send("VIDEO_STOPPED")
                            except websockets.ConnectionClosed:
                                pass

                    elif message == "START_AUDIO":
                        async with self._reconfigure_lock:
                            await self.client_settings_received.wait()
                            data_logger.info(
                                "Received START_AUDIO command from client for server-to-client audio."
                            )
                            if PCMFLUX_AVAILABLE:
                                if not self.is_pcmflux_capturing:
                                    data_logger.info("START_AUDIO: Starting pcmflux audio pipeline.")
                                    await self._start_pcmflux_pipeline()
                                else:
                                    data_logger.info("START_AUDIO: pcmflux audio pipeline already active.")
                            else:
                                data_logger.warning("START_AUDIO: Cannot start server-to-client audio (pcmflux not available).")
                            websockets.broadcast(self.clients, "AUDIO_STARTED")

                    elif message == "STOP_AUDIO":
                        async with self._reconfigure_lock:
                            data_logger.info("Received STOP_AUDIO")
                            if self.is_pcmflux_capturing:
                                await self._stop_pcmflux_pipeline()
                            if self.clients:
                                websockets.broadcast(self.clients, "AUDIO_STOPPED")

                    elif message.startswith("r,"):
                        await self.client_settings_received.wait() 
                        raddr = websocket.remote_address
                        
                        parts = message.split(',')
                        if len(parts) != 3:
                            data_logger.warning(f"Malformed resize request from {raddr}: {message}")
                            continue
                        
                        target_res_str = parts[1]
                        display_id = parts[2]

                        client_info = self.display_clients.get(display_id)
                        if not client_info:
                            data_logger.warning(f"Resize request for unknown display_id '{display_id}' from {raddr}. Ignoring.")
                            continue
                        
                        current_res_str = f"{client_info.get('width', 0)}x{client_info.get('height', 0)}"

                        if target_res_str == current_res_str:
                            data_logger.info(f"Received redundant resize request for {display_id} ({target_res_str}). No action taken.")
                            continue
                        data_logger.info(f"Received resize request for {display_id}: {target_res_str} from {raddr}")

                        await on_resize_handler(target_res_str, self.app, self, display_id)

                    elif message.startswith("SET_NATIVE_CURSOR_RENDERING,"):
                        await self.client_settings_received.wait()
                        try:
                            new_capture_cursor_str = message.split(",")[1].strip().lower()
                            new_capture_cursor = new_capture_cursor_str in ("1", "true")
                            data_logger.info(f"Received SET_NATIVE_CURSOR_RENDERING: {new_capture_cursor}")

                            if self.capture_cursor != new_capture_cursor:
                                self.capture_cursor = new_capture_cursor
                                if len(self.capture_instances) > 0:
                                    data_logger.info(f"Cursor rendering changed, triggering display reconfiguration.")
                                    await self.reconfigure_displays()
                            else:
                                data_logger.info(f"SET_NATIVE_CURSOR_RENDERING: Value {new_capture_cursor} is already set.")
                        except (IndexError, ValueError) as e:
                            data_logger.warning(f"Malformed SET_NATIVE_CURSOR_RENDERING message: {message}, error: {e}")

                    elif message.startswith("s,"):
                        await self.client_settings_received.wait()
                        try:
                            dpi_value_str = message.split(",")[1]
                            dpi_value = int(dpi_value_str)
                            
                            # Calculate and store scale for Wayland stack
                            scale_val = float(dpi_value) / 96.0
                            if client_display_id and client_display_id in self.display_clients:
                                self.display_clients[client_display_id]['scale'] = scale_val

                            data_logger.info(f"Received DPI setting from client: {dpi_value} (Scale: {scale_val})")

                            if await set_dpi(dpi_value):
                                data_logger.info(f"Successfully set DPI to {dpi_value}")
                            else:
                                data_logger.error(f"Failed to set DPI to {dpi_value}")

                            if IS_WAYLAND and client_display_id:
                                data_logger.info(f"Wayland: Scaling changed to {scale_val}, restarting stream for {client_display_id}...")
                                await self._stop_capture_for_display(client_display_id)
                                c_info = self.display_clients[client_display_id]
                                await self._start_capture_for_display(
                                    client_display_id, 
                                    c_info.get('width', 1024), 
                                    c_info.get('height', 768), 
                                    0, 0
                                )

                            if CURSOR_SIZE > 0:
                                calculated_cursor_size = int(round(dpi_value / 96.0 * CURSOR_SIZE))
                                new_cursor_size = max(1, calculated_cursor_size)

                                data_logger.info(f"Attempting to set cursor size to: {new_cursor_size} (based on DPI {dpi_value})")
                                if await set_cursor_size(new_cursor_size):
                                    data_logger.info(f"Successfully set cursor size to {new_cursor_size}")
                                else:
                                    data_logger.error(f"Failed to set cursor size to {new_cursor_size}")
                            else:
                                data_logger.warning("CURSOR_SIZE is not positive. Skipping cursor size adjustment based on DPI.")

                        except ValueError:
                            data_logger.error(f"Invalid DPI value in message: {message}")
                        except IndexError:
                            data_logger.error(f"Malformed DPI message: {message}")
                        except Exception as e_dpi:
                            data_logger.error(f"Error processing DPI message '{message}': {e_dpi}", exc_info=True)

                    elif message.startswith("cmd,"):
                        if not settings.command_enabled[0]:
                            data_logger.warning("Received 'cmd' message, but command execution is disabled by server settings.")
                            continue

                        toks = message.split(',')
                        if len(toks) > 1:
                            command_to_run = ",".join(toks[1:])
                            data_logger.info(f"Attempting to execute command: '{command_to_run}'")
                            home_directory = os.path.expanduser("~")
                            try:
                                process = await subprocess.create_subprocess_shell(
                                    command_to_run,
                                    stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL,
                                    cwd=home_directory
                                )
                                data_logger.info(f"Successfully launched command: '{command_to_run}' with PID {process.pid}")
                            except Exception as e:
                                data_logger.error(f"Failed to launch command '{command_to_run}': {e}")
                        else:
                            data_logger.warning("Received 'cmd' message without a command string.")

                    else:
                        if message.startswith("js,") and self.is_secure_mode:
                            perms = client_permissions.get(websocket)
                            if not perms or not perms.get("token"):
                                data_logger.warning(f"BLOCK (Secure Mode): Gamepad input from {websocket.remote_address} dropped. Client has no token/perms.")
                                continue
                            
                            token = perms.get("token")
                            current_perms = user_tokens.get(token)
                            server_slot = current_perms.get("slot") if current_perms else None

                            if server_slot is None:
                                data_logger.warning(f"BLOCK (Secure Mode): Gamepad input from {websocket.remote_address} dropped. Client token has no assigned slot.")
                                continue
                            try:
                                client_index = int(message.split(',')[2])
                                if (int(server_slot) - 1) != client_index:
                                    data_logger.warning(f"BLOCK (Secure Mode): Gamepad input from {websocket.remote_address} dropped. Client sent for index {client_index}, but is assigned slot {server_slot}.")
                                    continue
                            except (IndexError, ValueError):
                                data_logger.warning(f"BLOCK (Secure Mode): Malformed gamepad message from {websocket.remote_address}: {message}")
                                continue

                        if self.is_secure_mode and message.split(',')[0] in ["kd", "ku", "m", "m2", "cws", "cbs", "cwd", "cbd", "cwe", "cbe", "cw", "cb", "cr"]:
                            perms = client_permissions.get(websocket)
                            token = perms.get("token") if perms else None
                            
                            if active_mk_token is not None:
                                if token != active_mk_token:
                                    continue
                            else:
                                role = perms.get("role") if perms else "viewer"
                                if role != "controller":
                                    continue
 
                        if self.input_handler and hasattr(
                            self.input_handler, "on_message"
                        ):
                            await self.input_handler.on_message(message, client_display_id)

        except websockets.exceptions.ConnectionClosedOK:
            data_logger.info(f"Data WS disconnected gracefully from {raddr}")
        except websockets.exceptions.ConnectionClosedError as e:
            data_logger.warning(f"Data WS closed with error from {raddr}: {e}")
        except Exception as e_main_loop:
            data_logger.error(
                f"Error in Data WS handler for {raddr}: {e_main_loop}", exc_info=True
            )
        finally:
            self.last_start_video_request_times.pop(websocket, None)
            client_permissions.pop(websocket, None)
            data_logger.info(f"Cleaning up Data WS handler for {raddr} (Display ID: {client_display_id})...")

            self.clients.discard(websocket)
            if self.data_ws is websocket:
                self.data_ws = None
            
            disconnected_display_id = None
            for disp_id, client_info in self.display_clients.items():
                if client_info.get('ws') is websocket:
                    disconnected_display_id = disp_id
                    break
            
            if disconnected_display_id:
                del self.display_clients[disconnected_display_id]
                data_logger.info(f"Client for '{disconnected_display_id}' disconnected. Removing and triggering full display reconfiguration.")
                await self.reconfigure_displays()
            else:
                data_logger.info(f"Unregistered client at {raddr} disconnected. No display reconfiguration needed.")

            if "_stats_sender_task_ws" in locals():
                _task_to_cancel = locals()["_stats_sender_task_ws"]
                if _task_to_cancel and not _task_to_cancel.done():
                    _task_to_cancel.cancel()
                    try:
                        await _task_to_cancel
                    except asyncio.CancelledError:
                        pass

            if "_system_monitor_task_ws" in locals():
                _task_to_cancel = locals()["_system_monitor_task_ws"]
                if _task_to_cancel and not _task_to_cancel.done():
                    _task_to_cancel.cancel()
                    try:
                        await _task_to_cancel
                    except asyncio.CancelledError:
                        pass

            if "_gpu_monitor_task_ws" in locals():
                _task_to_cancel = locals()["_gpu_monitor_task_ws"]
                if _task_to_cancel and not _task_to_cancel.done():
                    _task_to_cancel.cancel()
                    try:
                        await _task_to_cancel
                    except asyncio.CancelledError:
                        pass

            if "_network_monitor_task_ws" in locals():
                _task_to_cancel = locals()["_network_monitor_task_ws"]
                if _task_to_cancel and not _task_to_cancel.done():
                    _task_to_cancel.cancel()
                    try:
                        await _task_to_cancel
                    except asyncio.CancelledError:
                        pass

            if (
                self._frame_backpressure_task
                and not self._frame_backpressure_task.done()
            ):
                if (
                    not self.clients
                ):
                    data_logger.info(
                        f"Last client ({raddr}) disconnected. Cancelling frame backpressure task."
                    )
                else:
                    data_logger.info(
                        f"Client {raddr} disconnected, but other clients remain. Frame backpressure task continues."
                    )

            if "pa_stream" in locals() and locals()["pa_stream"]:
                try:
                    locals()["pa_stream"].close()
                    data_logger.debug(f"Closed PulseAudio stream for {raddr}.")
                except Exception as e_pa_close:
                    data_logger.error(
                        f"Error closing PulseAudio stream for {raddr}: {e_pa_close}"
                    )

            if "pulse" in locals() and locals()["pulse"]:
                _local_pulse = locals()["pulse"]
                if (
                    "pa_module_index" in locals()
                    and locals()["pa_module_index"] is not None
                ):
                    _local_pa_module_index = locals()["pa_module_index"]
                    try:
                        data_logger.info(
                            f"Unloading PulseAudio module {_local_pa_module_index} for virtual mic (client: {raddr})."
                        )
                        _local_pulse.module_unload(_local_pa_module_index)
                    except Exception as e_unload_final:
                        data_logger.error(
                            f"Error unloading PulseAudio module {_local_pa_module_index} for {raddr}: {e_unload_final}"
                        )
                try:
                    _local_pulse.close()
                    data_logger.debug(f"Closed PulseAudio connection for {raddr}.")
                except Exception as e_pulse_close:
                    data_logger.error(
                        f"Error closing PulseAudio connection for {raddr}: {e_pulse_close}"
                    )

            if (
                "active_upload_target_path_conn" in locals()
                and locals()["active_upload_target_path_conn"]
                and "active_uploads_by_path_conn" in locals()
                and locals()["active_upload_target_path_conn"]
                in locals()["active_uploads_by_path_conn"]
            ):
                _local_active_path = locals()["active_upload_target_path_conn"]
                _local_active_uploads = locals()["active_uploads_by_path_conn"]
                try:
                    file_handle = _local_active_uploads.pop(_local_active_path, None)
                    if file_handle:
                        file_handle.close()
                    os.remove(_local_active_path)
                    data_logger.info(
                        f"Cleaned up incomplete file upload: {_local_active_path} for {raddr}"
                    )
                except OSError as e_os_remove:
                    data_logger.warning(
                        f"Could not remove incomplete upload file {_local_active_path} for {raddr}: {e_os_remove}"
                    )
                except Exception as e_file_cleanup:
                    data_logger.error(
                        f"Error cleaning up file upload {_local_active_path} for {raddr}: {e_file_cleanup}"
                    )

            if not self.clients:
                 data_logger.info(f"Last client ({raddr}) disconnected. All pipelines should have been stopped by reconfigure_displays.")
                 self.capture_cursor = False
                 async with self._reconfigure_lock:
                     await self.shutdown_pipelines()

            data_logger.info(f"Data WS handler for {raddr} finished all cleanup.")

    async def run_server(self):
        self.stop_server = asyncio.Future()
        while not self.stop_server.done():
            _current_server_instance = None
            wait_closed_task = None
            try:
                async with ws_async.serve(
                    self.ws_handler,
                    "0.0.0.0",
                    self.port,
                    compression=None,
                    ping_interval=20,
                    ping_timeout=20,
                ) as server_obj:
                    _current_server_instance = server_obj
                    self.server = _current_server_instance
                    data_logger.info(
                        f"Data WebSocket Server listening on port {self.port}"
                    )
                    wait_closed_task = asyncio.create_task(
                        _current_server_instance.wait_closed()
                    )
                    done, pending = await asyncio.wait(
                        [self.stop_server, wait_closed_task],
                        return_when=asyncio.FIRST_COMPLETED,
                    )
                    if self.stop_server in done:
                        if wait_closed_task in pending:
                            wait_closed_task.cancel()
                        break
                    data_logger.warning(
                        f"Data WS Server on port {self.port} stopped unexpectedly. Restarting."
                    )
            except OSError as e:
                data_logger.error(
                    f"OSError starting Data WS on port {self.port}: {e}. Retrying in 5s..."
                )
                await asyncio.sleep(5)
            except asyncio.CancelledError:
                data_logger.info(
                    f"Data WS run_server task for port {self.port} cancelled."
                )
                break
            except Exception as e:
                data_logger.error(
                    f"Exception in Data WS run_server for port {self.port}: {e}. Retrying in 5s...",
                    exc_info=True,
                )
                await asyncio.sleep(5)
            finally:
                if self.server is _current_server_instance:
                    self.server = None
                if wait_closed_task and not wait_closed_task.done():
                    try:
                        await asyncio.wait_for(wait_closed_task, timeout=0.1)
                    except (asyncio.CancelledError, asyncio.TimeoutError):
                        pass
        data_logger.info(f"Data WS run_server loop for port {self.port} has finished.")

    async def stop(self):
        data_logger.info(f"Stopping Data WebSocket Server on port {self.port}...")
        if self.stop_server and not self.stop_server.done():
            self.stop_server.set_result(True)
        if self.server:
            self.server.close()
            try:
                await asyncio.wait_for(self.server.wait_closed(), timeout=2.0)
            except asyncio.TimeoutError:
                data_logger.warning(
                    f"Timeout closing Data WS listener on port {self.port}."
                )
            except Exception as e_close:
                data_logger.error(f"Error on server.wait_closed(): {e_close}")
        self.server = None
        await self.shutdown_pipelines()
        data_logger.info(f"Data WS on port {self.port} stop procedure complete.")

    async def _cleanup_client(self, websocket, display_id):
        """Removes a client and triggers reconfiguration if necessary."""
        data_logger.info(f"Cleaning up Data WS handler for {websocket.remote_address} (Display ID: {display_id})...")
        self.display_clients.pop(display_id, None)

        if self._is_reconfiguring:
            data_logger.warning(f"Client '{display_id}' disconnected DURING a reconfiguration. "
                                "A new reconfiguration will NOT be triggered to prevent a loop.")
            return

        data_logger.info(f"Client for {display_id} removed. Triggering display reconfiguration.")
        async with self._reconfigure_lock:
            await self.reconfigure_displays()

    async def _run_detached_command(self, cmd_list: list, description: str):
        """Runs a command via the shell using 'nohup ... &' to detach it from the server process."""
        import shlex
        quoted_cmd = ' '.join(shlex.quote(c) for c in cmd_list)
        shell_command = f"nohup {quoted_cmd} &"
        data_logger.info(f"Running detached command ({description}): {shell_command}")
        try:
            proc = await asyncio.create_subprocess_shell(
                shell_command,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL
            )
        except Exception as e:
            data_logger.error(f"Failed to run detached command '{shell_command}': {e}")

    async def _run_command(self, cmd, description):
        """Helper to run a shell command and log its output/errors."""
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE
            )
            stdout, stderr = await proc.communicate()
            if proc.returncode != 0:
                data_logger.error(
                    f"Failed ({description}). RC: {proc.returncode}, "
                    f"Stderr: {stderr.decode().strip()}"
                )
                return False
            return True
        except Exception as e:
            data_logger.error(f"Exception during '{description}': {e}", exc_info=True)
            return False

    async def _get_current_monitors(self):
        """Parses `xrandr --listmonitors` to get names of existing logical monitors."""
        monitors = []
        try:
            proc = await asyncio.create_subprocess_exec(
                "xrandr", "--listmonitors",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE
            )
            stdout, stderr = await proc.communicate()
            if proc.returncode == 0:
                output = stdout.decode()
                for line in output.splitlines()[1:]:
                    parts = line.split()
                    if len(parts) >= 4:
                        monitors.append(parts[1])
        except Exception as e:
            data_logger.error(f"Failed to list current monitors: {e}")
        return monitors


    async def _stop_capture_for_display(self, display_id: str):
        """Stops the capture, sender, and backpressure tasks for a single, specific display."""
        data_logger.info(f"Stopping all streams for display '{display_id}'...")
        await self._ensure_backpressure_task_is_stopped(display_id)
        capture_info = self.capture_instances.pop(display_id, None)
        if capture_info:
            capture_module = capture_info.get('module')
            if capture_module:
                await self.capture_loop.run_in_executor(None, capture_module.stop_capture)
            sender_task = capture_info.get('sender_task')
            if sender_task and not sender_task.done():
                sender_task.cancel()
        self.video_chunk_queues.pop(display_id, None)

        data_logger.info(f"Successfully stopped all streams for display '{display_id}'.")
 
    async def reconfigure_displays(self):
        """
        Central logic to create a virtual desktop for ALL connected clients.
        It then starts capture pipelines ONLY for clients with 'video_active' = True.
        This is called on connect, disconnect, or settings change.
        """
        if self._reconfigure_lock.locked():
            data_logger.warning("Reconfiguration already in progress. Ignoring concurrent request.")
            return
        async with self._reconfigure_lock:
            self._is_reconfiguring = True
            data_logger.info("Starting display reconfiguration...")
            try:
                current_display_count = len(self.display_clients)
                if not IS_WAYLAND and self._wm_swap_is_supported is None:
                    if which("xfce4-session") or which("startplasma-x11"):
                        self._wm_swap_is_supported = True
                    else:
                        self._wm_swap_is_supported = False
                if (not IS_WAYLAND and current_display_count > 1 and self._wm_swap_is_supported and not self._is_wm_swapped):
                    data_logger.info("Multi-monitor setup: switching to Openbox with a minimal config.")
                    config_path = "/tmp/openbox_selkies_config.xml"
                    config_content = "<openbox_config></openbox_config>\n"
                    try:
                        with open(config_path, "w") as f:
                            f.write(config_content)
                        data_logger.info(f"Wrote minimal Openbox config to {config_path}")
                        openbox_cmd = ["openbox", "--config-file", config_path, "--replace"]
                    except IOError as e:
                        data_logger.error(f"Could not write Openbox config to {config_path}: {e}. Proceeding without custom config.")
                        openbox_cmd = ["openbox", "--replace"]
                    await self._run_detached_command(openbox_cmd, "switch to openbox")
                    self._is_wm_swapped = True
                if self.capture_instances or any(d.get('backpressure_task') for d in self.display_clients.values()):
                    data_logger.info("Stopping all existing capture and backpressure tasks...")
                    stop_bp_tasks = [self._ensure_backpressure_task_is_stopped(disp_id) for disp_id in self.display_clients.keys()]
                    if stop_bp_tasks:
                        await asyncio.gather(*stop_bp_tasks, return_exceptions=True)
                    stop_capture_tasks = [
                        self.capture_loop.run_in_executor(None, inst['module'].stop_capture)
                        for inst in self.capture_instances.values() if inst.get('module')
                    ]
                    if stop_capture_tasks:
                        await asyncio.gather(*stop_capture_tasks, return_exceptions=True)

                    for display_id, inst in self.capture_instances.items():
                        sender_task = inst.get('sender_task')
                        if sender_task and not sender_task.done():
                            sender_task.cancel()
                    self.capture_instances.clear()
                    self.video_chunk_queues.clear()
                    data_logger.info("All capture instances, senders, and backpressure tasks stopped.")
                if not self.display_clients:
                    data_logger.warning("No display clients connected. Video pipelines remain stopped.")
                    _, _, _, _, screen_name = await get_new_res("1x1")
                    if screen_name:
                        current_monitors = await self._get_current_monitors()
                        for monitor_name in current_monitors:
                            await self._run_command(["xrandr", "--delmonitor", monitor_name], f"cleanup monitor {monitor_name}")
                    return
                data_logger.info("Calculating new extended desktop layout from ALL clients...")
                layouts = {}
                total_width = 0
                total_height = 0
                primary_client = self.display_clients.get('primary')
                secondary_client = None
                secondary_id = None
                for display_id, client in self.display_clients.items():
                    if display_id != 'primary':
                        secondary_client = client
                        secondary_id = display_id
                        break
                if primary_client and not secondary_client:
                    p_w, p_h = primary_client.get('width', 0), primary_client.get('height', 0)
                    if p_w > 0 and p_h > 0:
                        layouts['primary'] = {'x': 0, 'y': 0, 'w': p_w, 'h': p_h}
                        total_width, total_height = p_w, p_h
                elif primary_client and secondary_client:
                    p_w, p_h = primary_client.get('width', 0), primary_client.get('height', 0)
                    s_w, s_h = secondary_client.get('width', 0), secondary_client.get('height', 0)
                    position = secondary_client.get('position', 'right')
                    if p_w > 0 and p_h > 0 and s_w > 0 and s_h > 0:
                        if position == 'right':
                            layouts['primary'] = {'x': 0, 'y': 0, 'w': p_w, 'h': p_h}
                            layouts[secondary_id] = {'x': p_w, 'y': 0, 'w': s_w, 'h': s_h}
                            total_width, total_height = p_w + s_w, max(p_h, s_h)
                        elif position == 'left':
                            layouts[secondary_id] = {'x': 0, 'y': 0, 'w': s_w, 'h': s_h}
                            layouts['primary'] = {'x': s_w, 'y': 0, 'w': p_w, 'h': p_h}
                            total_width, total_height = p_w + s_w, max(p_h, s_h)
                        elif position == 'down':
                            layouts['primary'] = {'x': 0, 'y': 0, 'w': p_w, 'h': p_h}
                            layouts[secondary_id] = {'x': 0, 'y': p_h, 'w': s_w, 'h': s_h}
                            total_width, total_height = max(p_w, s_w), p_h + s_h
                        elif position == 'up':
                            layouts[secondary_id] = {'x': 0, 'y': 0, 'w': s_w, 'h': s_h}
                            layouts['primary'] = {'x': 0, 'y': s_h, 'w': p_w, 'h': p_h}
                            total_width, total_height = max(p_w, s_w), p_h + s_h
                if total_width == 0 or total_height == 0:
                    data_logger.error("Calculated total display size is zero. Aborting reconfiguration.")
                    return
                aligned_total_width = (total_width + 7) & ~7
                if aligned_total_width != total_width:
                    data_logger.info(f"Aligned total width from {total_width} to {aligned_total_width} for xrandr.")
                    total_width = aligned_total_width
                self.display_layouts = layouts
                data_logger.info(f"Layout calculated: Total Size={total_width}x{total_height}. Layouts: {layouts}")
                if not IS_WAYLAND:
                    _, _, available_resolutions, _, screen_name = await get_new_res("1x1")
                    if not screen_name:
                        data_logger.error("CRITICAL: Could not determine screen name from xrandr. Aborting.")
                        return
                    current_monitors = await self._get_current_monitors()
                    for monitor_name in current_monitors:
                        await self._run_command(["xrandr", "--delmonitor", monitor_name], f"delete old monitor {monitor_name}")
                    total_mode_str = f"{total_width}x{total_height}"
                    if total_mode_str not in available_resolutions:
                        data_logger.info(f"Mode {total_mode_str} not found. Creating it.")
                        try:
                            _, modeline_params = await generate_xrandr_gtf_modeline(total_mode_str)
                            await self._run_command(["xrandr", "--newmode", total_mode_str] + modeline_params.split(), "create new mode")
                            await self._run_command(["xrandr", "--addmode", screen_name, total_mode_str], "add new mode")
                        except Exception as e:
                            data_logger.error(f"FATAL: Could not create extended mode {total_mode_str}: {e}. Aborting.")
                            return
                    await self._run_command(["xrandr", "--fb", total_mode_str, "--output", screen_name, "--mode", total_mode_str], "set framebuffer")
                    data_logger.info("Defining logical monitors for the window manager...")
                    for display_id, layout in layouts.items():
                        geometry = f"{layout['w']}/0x{layout['h']}/0+{layout['x']}+{layout['y']}"
                        monitor_name = f"selkies-{display_id}"
                        cmd = ["xrandr", "--setmonitor", monitor_name, geometry, screen_name]
                        await self._run_command(cmd, f"set logical monitor {monitor_name}")
                    if 'primary' in layouts:
                        await self._run_command(
                            ["xrandr", "--output", screen_name, "--primary"],
                            "set primary output"
                        )
                data_logger.info("Starting separate capture instances for each ACTIVE display region...")
                for display_id, layout in layouts.items():
                    client_data = self.display_clients.get(display_id)
                    if client_data and client_data.get('video_active', False):
                        data_logger.info(f"Client '{display_id}' is active. Starting its capture.")
                        try:
                            await self._start_capture_for_display(
                                display_id=display_id,
                                width=layout['w'], height=layout['h'],
                                x_offset=layout['x'], y_offset=layout['y']
                            )
                            await self._start_backpressure_task_if_needed(display_id)
                        except Exception as e:
                            data_logger.error(
                                f"Failed to start capture for display '{display_id}' during reconfiguration. "
                                f"This display will not stream. Error: {e}", exc_info=False
                            )
                    else:
                        data_logger.info(f"Client '{display_id}' is connected but not active. Skipping video start.")
                await self.broadcast_stream_resolution()
                await self.broadcast_display_config()
                data_logger.info("Display reconfiguration finished successfully.")
            except Exception as e:
                data_logger.error(f"A critical error occurred during display reconfiguration: {e}", exc_info=True)
            finally:
                self._last_display_count = len(self.display_clients)
                self._is_reconfiguring = False
                data_logger.info("Reconfiguration process complete (state unlocked).")

    async def _video_chunk_sender(self, display_id: str):
        """
        Pulls data from a specific queue, records send timestamp, and sends to the correct client(s).
        """
        data_logger.info(f"Video chunk sender started for display '{display_id}'.")
        queue = self.video_chunk_queues.get(display_id)
        if not queue:
            data_logger.error(f"Cannot start sender for '{display_id}': Queue not found.")
            return

        try:
            while True:
                chunk_info = await queue.get()
                data_chunk = chunk_info['data']
                frame_id = chunk_info['frame_id']
                if display_id == 'primary':
                    secondary_websockets = {
                        client_info.get('ws')
                        for did, client_info in self.display_clients.items()
                        if did != 'primary' and client_info.get('ws')
                    }
                    primary_viewers = self.clients - secondary_websockets

                    if not primary_viewers:
                        queue.task_done()
                        continue
                    now = time.monotonic()
                    for client_ws in primary_viewers:
                        for primary_client_info in self.display_clients.values():
                            if primary_client_info.get('ws') is client_ws:
                                if primary_client_info.get('backpressure_enabled', True):
                                    primary_client_info['sent_timestamps'][frame_id] = now
                                    primary_client_info['last_sent_frame_id'] = frame_id
                                    if len(primary_client_info['sent_timestamps']) > SENT_FRAME_TIMESTAMP_HISTORY_SIZE:
                                        primary_client_info['sent_timestamps'].popitem(last=False)
                                break
                    try:
                        websockets.broadcast(primary_viewers, data_chunk)
                        self._bytes_sent_in_interval += len(data_chunk) * len(primary_viewers)
                    except Exception as e:
                        data_logger.error(f"Error during primary broadcast: {e}")

                else:
                    client_info = self.display_clients.get(display_id)
                    if not client_info or not client_info.get('ws') or not client_info.get('backpressure_enabled', True):
                        queue.task_done()
                        continue
                    websocket = client_info['ws']
                    now = time.monotonic()
                    client_info['sent_timestamps'][frame_id] = now
                    client_info['last_sent_frame_id'] = frame_id
                    if len(client_info['sent_timestamps']) > SENT_FRAME_TIMESTAMP_HISTORY_SIZE:
                        client_info['sent_timestamps'].popitem(last=False)
                    try:
                        await websocket.send(data_chunk)
                        self._bytes_sent_in_interval += len(data_chunk)
                    except websockets.ConnectionClosed:
                        data_logger.warning(f"Client for '{display_id}' connection closed during send.")
                        break
                queue.task_done()
        except asyncio.CancelledError:
            data_logger.info(f"Video chunk sender for '{display_id}' cancelled.")
        finally:
            data_logger.info(f"Video chunk sender for '{display_id}' finished.")

    async def _start_capture_for_display(self, display_id: str, width: int, height: int, x_offset: int, y_offset: int):
        """
        Starts a capture instance by creating the required CaptureSettings
        object and providing a callback with the correct signature.
        """
        if display_id in self.capture_instances:
            data_logger.warning(f"Capture instance for '{display_id}' already exists. Skipping start.")
            return

        data_logger.info(
            f"Preparing to start capture for display='{display_id}': "
            f"Res={width}x{height}, Offset={x_offset}x{y_offset}"
        )

        try:
            settings = self._get_capture_settings(display_id, width, height, x_offset, y_offset)
            display_state = self.display_clients.get(display_id, {})
            encoder_for_this_capture = display_state.get('encoder', self.app.encoder)

            def queue_data_for_display(result_ptr, user_data):
                # ... [Existing video callback implementation] ...
                if not result_ptr:
                    return
                try:
                    result = result_ptr.contents
                    if result.size > 0:
                        data_bytes = bytes(result.data[:result.size])
                        if encoder_for_this_capture == "jpeg":
                            final_data_to_queue = b"\x03\x00" + data_bytes
                        else:
                            final_data_to_queue = data_bytes
                        
                        queue = self.video_chunk_queues.get(display_id)
                        if queue:
                            item_to_queue = {'data': final_data_to_queue, 'frame_id': result.frame_id}
                            
                            def do_put():
                                try:
                                    queue.put_nowait(item_to_queue)
                                except asyncio.QueueFull:
                                    pass
                            
                            self.capture_loop.call_soon_threadsafe(do_put)

                except Exception as e:
                    data_logger.error(f"Error in capture callback for {display_id}: {e}", exc_info=False)

            def make_default_cursor_b64(size):
                im = Image.new("RGBA", (size, size), (0, 0, 0, 0))
                draw = ImageDraw.Draw(im)
                draw.ellipse((0, 0, size - 1, size - 1), fill=(255, 255, 255, 255))
                inset = 2
                draw.ellipse((inset, inset, size - 1 - inset, size - 1 - inset), fill=(0, 0, 0, 255))
                with io.BytesIO() as f:
                    im.save(f, "PNG")
                    png_bytes = f.getvalue()
                return base64.b64encode(png_bytes).decode('ascii')

            default_cursor_b64 = make_default_cursor_b64(16)

            def wayland_cursor_handler(msg_type, data_bytes, hot_x, hot_y):
                try:
                    if msg_type == "hide":
                        self.app.send_ws_cursor_data({
                            "curdata": "",
                            "width": 0, "height": 0,
                            "hotx": 0, "hoty": 0,
                            "handle": 0
                        })
                    elif msg_type == "png" and data_bytes:
                        b64_data = base64.b64encode(data_bytes).decode('ascii')
                        self.app.send_ws_cursor_data({
                            "curdata": b64_data,
                            "width": self.cursor_size, 
                            "height": self.cursor_size,
                            "hotx": hot_x,
                            "hoty": hot_y,
                            "handle": int(time.time() * 1000)
                        })
                    elif msg_type == "surface" or (msg_type == "png" and not data_bytes):
                        center = 16 // 2
                        self.app.send_ws_cursor_data({
                            "curdata": default_cursor_b64,
                            "width": 16,
                            "height": 16,
                            "hotx": center,
                            "hoty": center,
                            "handle": int(time.time() * 1000)
                        })
                except Exception as e:
                    data_logger.error(f"Error handling wayland cursor: {e}")

            queue_size = getattr(self, 'BACKPRESSURE_QUEUE_SIZE', 120)
            self.video_chunk_queues[display_id] = asyncio.Queue(maxsize=queue_size)
            sender_task = asyncio.create_task(self._video_chunk_sender(display_id))
            
            capture_module = ScreenCapture()

            if IS_WAYLAND:
                if hasattr(capture_module, 'set_cursor_callback'):
                    capture_module.set_cursor_callback(wayland_cursor_handler)
                    data_logger.info(f"Registered Wayland cursor callback for '{display_id}'")
                    wayland_cursor_handler("surface", b"", 0, 0)

            await self.capture_loop.run_in_executor(
                None,
                capture_module.start_capture,
                settings,
                StripeCallback(queue_data_for_display)
            )

            self.capture_instances[display_id] = {
                'module': capture_module,
                'sender_task': sender_task
            }
            data_logger.info(f"SUCCESS: Capture started for '{display_id}'.")

        except Exception as e:
            data_logger.error(f"Failed to start capture for '{display_id}': {e}", exc_info=True)
            if display_id in self.video_chunk_queues:
                del self.video_chunk_queues[display_id]
            if 'sender_task' in locals() and not sender_task.done():
                sender_task.cancel()

    def _get_capture_settings(self, display_id, width, height, x, y):
        """Helper to create CaptureSettings for a specific display region."""
        display_state = self.display_clients.get(display_id)
        if not display_state:
            raise SelkiesAppError(f"Cannot get capture settings for unknown display_id '{display_id}'")

        cs = CaptureSettings()
        cs.capture_width = width
        cs.capture_height = height
        cs.capture_x = x
        cs.capture_y = y
        if IS_WAYLAND:
            cs.scale = display_state.get('scale', 1.0)
        cs.target_fps = float(display_state.get('framerate', self.app.framerate))
        cs.capture_cursor = self.capture_cursor
        cs.debug_logging = self.cli_args.debug[0]
        
        encoder = display_state.get('encoder', self.app.encoder)
        if encoder == "jpeg":
            cs.output_mode = 0
            cs.jpeg_quality = display_state.get('jpeg_quality', self._initial_jpeg_quality)
            cs.paint_over_jpeg_quality = display_state.get('paint_over_jpeg_quality', self._initial_paint_over_jpeg_quality)
        else: # H.264 modes
            cs.output_mode = 1
            cs.h264_crf = display_state.get('h264_crf', self._initial_h264_crf)
            cs.h264_paintover_crf = display_state.get('h264_paintover_crf', self._initial_h264_paintover_crf)
            cs.h264_paintover_burst_frames = display_state.get('h264_paintover_burst_frames', self._initial_h264_paintover_burst_frames)
            cs.h264_fullcolor = display_state.get('h264_fullcolor', self._initial_h264_fullcolor)
            cs.h264_streaming_mode = display_state.get('h264_streaming_mode', self._initial_h264_streaming_mode)
            cs.h264_fullframe = (encoder == "x264enc")

        cs.use_paint_over_quality = display_state.get('use_paint_over_quality', self._initial_use_paint_over_quality)
        cs.paint_over_trigger_frames = 15
        cs.damage_block_threshold = 10
        cs.damage_block_duration = 20
        cs.use_cpu = display_state.get('use_cpu', self._initial_use_cpu)
        
        if self.cli_args.dri_node:
            cs.vaapi_render_node_index = parse_dri_node_to_index(self.cli_args.dri_node)
        else:
            cs.vaapi_render_node_index = -1

        watermark_path_str = self.cli_args.watermark_path
        if watermark_path_str and os.path.exists(watermark_path_str):
            cs.watermark_path = watermark_path_str.encode('utf-8')
            cs.watermark_location_enum = self.cli_args.watermark_location
        
        return cs

async def _collect_system_stats_ws(shared_data, interval_seconds=1):
    data_logger.debug(
        f"System monitor loop (WS mode) started, interval: {interval_seconds}s"
    )
    try:
        while True:
            cpu = psutil.cpu_percent()
            mem = psutil.virtual_memory()
            shared_data["system"] = {
                "type": "system_stats",
                "timestamp": datetime.now().isoformat(),
                "cpu_percent": cpu,
                "mem_total": mem.total,
                "mem_used": mem.used,
            }
            await asyncio.sleep(interval_seconds)
    except asyncio.CancelledError:
        data_logger.info("System monitor (WS) cancelled.")
    except Exception as e:
        data_logger.error(f"System monitor (WS) error: {e}", exc_info=True)


async def _collect_gpu_stats_ws(shared_data, gpu_id=0, interval_seconds=1):
    data_logger.debug(
        f"GPU monitor loop (WS mode) for GPU {gpu_id}, interval: {interval_seconds}s"
    )
    try:
        gpus = GPUtil.getGPUs()
        if not gpus:
            data_logger.warning("No GPUs detected for GPU monitor (WS).")
            return
        if not (0 <= gpu_id < len(gpus)):
            data_logger.error(f"Invalid GPU ID {gpu_id} for GPU monitor (WS).")
            return

        while True:
            try:
                gpus = GPUtil.getGPUs()
                if not gpus or gpu_id >= len(gpus):
                    data_logger.error(f"GPU {gpu_id} no longer available.")
                    break
                gpu = gpus[gpu_id]
                shared_data["gpu"] = {
                    "type": "gpu_stats",
                    "timestamp": datetime.now().isoformat(),
                    "gpu_id": gpu_id,
                    "load": gpu.load,
                    "memory_total": gpu.memoryTotal * 1024 * 1024,
                    "memory_used": gpu.memoryUsed * 1024 * 1024,
                }
            except Exception as e_gpu_stat:
                data_logger.error(
                    f"GPU monitor (WS): Error getting stats for ID {gpu_id}: {e_gpu_stat}"
                )
                await asyncio.sleep(interval_seconds * 2)
            await asyncio.sleep(interval_seconds)
    except asyncio.CancelledError:
        data_logger.info("GPU monitor (WS) cancelled.")
    except Exception as e:
        data_logger.error(f"GPU monitor (WS) error: {e}", exc_info=True)

async def _collect_network_stats_ws(shared_data, server_instance, interval_seconds=2):
    """Periodically calculates bandwidth and collects latency."""
    data_logger.debug(
        f"Network monitor loop (WS mode) started, interval: {interval_seconds}s"
    )
    try:
        while True:
            await asyncio.sleep(interval_seconds)
            current_time = time.monotonic()
            elapsed_time = current_time - server_instance._last_bandwidth_calc_time
            if elapsed_time > 0:
                current_mbps = (server_instance._bytes_sent_in_interval * 8) / elapsed_time / 1_000_000
            else:
                current_mbps = 0.0
            server_instance._bytes_sent_in_interval = 0
            server_instance._last_bandwidth_calc_time = current_time
            
            primary_client = server_instance.display_clients.get('primary')
            latency_ms = primary_client.get('smoothed_rtt', 0.0) if primary_client else 0.0

            shared_data["network"] = {
                "type": "network_stats",
                "timestamp": datetime.now().isoformat(),
                "bandwidth_mbps": round(current_mbps, 2),
                "latency_ms": round(latency_ms, 1),
            }
    except asyncio.CancelledError:
        data_logger.info("Network monitor (WS) cancelled.")
    except Exception as e:
        data_logger.error(f"Network monitor (WS) error: {e}", exc_info=True)

async def _send_stats_periodically_ws(websocket, shared_data, interval_seconds=5):
    try:
        while True:
            await asyncio.sleep(interval_seconds)
            system_stats = shared_data.pop("system", None)
            gpu_stats = shared_data.pop("gpu", None)
            network_stats = shared_data.pop("network", None)
            try:
                if not websocket:  # Check if websocket is still valid
                    data_logger.info("Stats sender: WS closed or invalid.")
                    break
                if system_stats:
                    await websocket.send(json.dumps(system_stats))
                if gpu_stats:
                    await websocket.send(json.dumps(gpu_stats))
                if network_stats:
                    await websocket.send(json.dumps(network_stats))
            except websockets.exceptions.ConnectionClosed:
                data_logger.info("Stats sender: WS connection closed.")
                break
            except Exception as e_send:
                data_logger.error(f"Stats sender: Error sending: {e_send}")
    except asyncio.CancelledError:
        data_logger.info("Stats sender (WS) cancelled.")
    except Exception as e:
        data_logger.error(f"Stats sender (WS) error: {e}", exc_info=True)

async def on_resize_handler(res_str, current_app_instance, data_server_instance=None, display_id='primary'):
    """
    Handles client resize request. Updates the state for a specific display and triggers a full reconfiguration.
    """
    logger_gst_app_resize.info(f"on_resize_handler for display '{display_id}' with resolution: {res_str}")
    if data_server_instance:
        server_is_manual, _ = data_server_instance.cli_args.is_manual_resolution_mode
        if server_is_manual:
            logger_gst_app_resize.warning(
                f"Client attempted to resize to {res_str} but server is in manual resolution mode. Request ignored."
            )
            return
    try:
        w_str, h_str = res_str.split("x")
        target_w, target_h = int(w_str), int(h_str)

        if target_w <= 0 or target_h <= 0:
            logger_gst_app_resize.error(f"Invalid target dimensions in resize request: {target_w}x{target_h}. Ignoring.")
            return
        if target_w % 2 != 0: target_w -= 1
        if target_h % 2 != 0: target_h -= 1
        if target_w <= 0 or target_h <= 0:
            logger_gst_app_resize.error(f"Dimensions became invalid ({target_w}x{target_h}) after odd adjustment. Ignoring.")
            return

        if data_server_instance and display_id in data_server_instance.display_clients:
            client_info = data_server_instance.display_clients[display_id]
            if client_info.get('width') == target_w and client_info.get('height') == target_h:
                logger_gst_app_resize.info(f"Redundant resize request for {display_id} to {target_w}x{target_h}. No action.")
                return

            client_info['width'] = target_w
            client_info['height'] = target_h
            
            if display_id == 'primary':
                current_app_instance.display_width = target_w
                current_app_instance.display_height = target_h

            if IS_WAYLAND:
                logger_gst_app_resize.info(f"Wayland Resize: Updating {display_id} to {target_w}x{target_h} and restarting pipeline.")
                await data_server_instance._stop_capture_for_display(display_id)
                await data_server_instance._start_capture_for_display(display_id, target_w, target_h, 0, 0)
            else:
                logger_gst_app_resize.info(f"Display client '{display_id}' dimensions updated to {target_w}x{target_h}. Triggering reconfiguration.")
                await data_server_instance.reconfigure_displays()
        else:
            logger_gst_app_resize.error(f"Cannot resize: display_id '{display_id}' not found in connected clients.")
    except ValueError:
        logger_gst_app_resize.error(f"Invalid resolution format in resize request: {res_str}")
    except Exception as e:
        logger_gst_app_resize.error(f"Error during resize handling for '{res_str}': {e}", exc_info=True)

async def ws_entrypoint():
    is_secure_mode = bool(settings.master_token)
    if is_secure_mode:
        logger.info("Secure Mode ENABLED (SELKIES_MASTER_TOKEN is set).")
    else:
        logger.info("Legacy Mode ENABLED (SELKIES_MASTER_TOKEN is not set).")
        config_gate.set()

    async def reconcile_clients():
        """Iterate through connected clients and disconnect those with invalid/changed permissions."""
        global user_tokens, client_permissions
        connected_websockets = list(client_permissions.keys())
        current_tokens = user_tokens.copy()
        for ws in connected_websockets:
            if ws.state != websockets.protocol.State.OPEN:
                continue
            perms = client_permissions.get(ws)
            if not perms or perms.get("token") is None:
                continue
            token = perms["token"]
            new_perms = current_tokens.get(token)
            should_disconnect = False
            reason = ""
            if not new_perms:
                should_disconnect, reason = True, "Token revoked"
            else:
                old_role, old_slot = perms.get("role"), perms.get("slot")
                new_role = new_perms.get("role")
                new_slot = new_perms.get("slot")

                if old_role != new_role:
                    should_disconnect, reason = True, "Permissions changed significantly"
                
                elif old_slot != new_slot:
                    data_logger.info(f"Updating client {ws.remote_address} for slot change: {old_slot} -> {new_slot}")                    
                    update_payload = json.dumps({"role": new_role, "slot": new_slot})
                    update_message = f"ROLE_UPDATE,{update_payload}"
                    try:
                        await ws.send(update_message)
                        client_permissions[ws]['role'] = new_role
                        client_permissions[ws]['slot'] = new_slot
                    except websockets.ConnectionClosed:
                        data_logger.warning(f"Could not send role update to {ws.remote_address}, connection closed.")
            if should_disconnect:
                data_logger.info(f"Disconnecting client {ws.remote_address} due to: {reason}")
                try:
                    await ws.close(code=4002, reason=reason)
                except websockets.ConnectionClosed:
                    pass
            has_mk_access = False
            if active_mk_token is not None:
                if token == active_mk_token:
                    has_mk_access = True
            elif new_perms.get("role") == "controller":
                has_mk_access = True
            
            mk_msg = "MK_ACCESS,1" if has_mk_access else "MK_ACCESS,0"
            try:
                await ws.send(mk_msg)
            except websockets.ConnectionClosed:
                pass

    async def handle_tokens_post(request):
        global user_tokens, config_gate, active_mk_token
        auth_header = request.headers.get("Authorization", "")
        if not auth_header.startswith("Bearer ") or auth_header.split(" ", 1)[1] != settings.master_token:
            return web.Response(status=401, text="Unauthorized")
        try:
            new_token_data = await request.json()
            if not isinstance(new_token_data, dict): raise ValueError("Payload must be a JSON object")
        except (json.JSONDecodeError, ValueError) as e:
            return web.Response(status=400, text=f"Bad Request: {e}")
        user_tokens = new_token_data
        new_mk_owner = None
        for tkn, perms in user_tokens.items():
            if perms.get("mk_control", False):
                new_mk_owner = tkn
                break
        active_mk_token = new_mk_owner
        logger.info(f"Updated user tokens. Now tracking {len(user_tokens)} tokens.")
        if not config_gate.is_set():
            config_gate.set()
            logger.info("Configuration gate is now open. WebSocket server will accept connections.")
        asyncio.create_task(reconcile_clients())
        return web.Response(status=200, text="OK")

    if "DEV_MODE" in os.environ:
        try:
            dev_version_file = pathlib.Path(
                "../../addons/gst-web-core/selkies-version.txt"
            )
            if dev_version_file.parent.exists():
                dev_version_file.touch(exist_ok=True)
        except OSError:
            pass

    global TARGET_FRAMERATE
    processed_framerate = settings.framerate
    min_fr, max_fr = processed_framerate
    if min_fr == max_fr:
        TARGET_FRAMERATE = min_fr
    else:
        fr_def = next((s for s in SETTING_DEFINITIONS if s['name'] == 'framerate'), None)
        TARGET_FRAMERATE = fr_def['meta']['default_value'] if fr_def else 60

    initial_encoder = settings.encoder

    if not settings.debug[0] and PULSEAUDIO_AVAILABLE:
        logging.getLogger("pulsectl").setLevel(logging.WARNING)

    logger.info(f"Starting Selkies (WebSocket Mode) with settings: {vars(settings)}")
    logger.info(
        f"Initial Encoder: {initial_encoder}, Framerate: {TARGET_FRAMERATE}"
    )

    event_loop = asyncio.get_running_loop()

    app = SelkiesStreamingApp(
        event_loop,
        framerate=TARGET_FRAMERATE,
        encoder=initial_encoder,
        mode="websockets",
    )
    app.server_enable_resize = ENABLE_RESIZE
    app.last_resize_success = True
    logger.info(
        f"SelkiesStreamingApp initialized: encoder={app.encoder}, display={app.display_width}x{app.display_height}"
    )

    data_server = DataStreamingServer(
        port=settings.port,
        app=app,
        uinput_mouse_socket=UINPUT_MOUSE_SOCKET,
        js_socket_path=JS_SOCKET_PATH,
        enable_clipboard=settings.clipboard_enabled,
        enable_cursors=ENABLE_CURSORS,
        cursor_size=CURSOR_SIZE,
        cursor_scale=1.0,
        cursor_debug=DEBUG_CURSORS,
        audio_device_name=settings.audio_device_name,
        cli_args=settings,
        is_secure_mode=is_secure_mode,
    )
    app.data_streaming_server = data_server

    input_handler = InputHandler(
        app,
        UINPUT_MOUSE_SOCKET,
        JS_SOCKET_PATH,
        str(settings.clipboard_enabled[0]).lower(),
        str(settings.enable_binary_clipboard[0]).lower(),
        ENABLE_CURSORS,
        CURSOR_SIZE,
        1.0,
        DEBUG_CURSORS,
        data_server_instance=data_server,
        is_wayland=IS_WAYLAND,
    )
    data_server.input_handler = (
        input_handler
    )

    input_handler.on_clipboard_read = app.send_ws_clipboard_data

    input_handler.on_set_fps = app.set_framerate
    if ENABLE_RESIZE:
        input_handler.on_resize = lambda res_str, display_id='primary': on_resize_handler(
            res_str, app, data_server, display_id
        )
    else:
        input_handler.on_resize = lambda res_str, display_id='primary': logger.warning("Resize disabled.")
        input_handler.on_scaling_ratio = lambda scale_val: logger.warning(
            "Scaling disabled."
        )

    tasks_to_run = []
    data_server_task = asyncio.create_task(data_server.run_server(), name="DataServer")
    tasks_to_run.append(data_server_task)

    control_plane_runner = None
    if is_secure_mode:
        api_app = web.Application()
        api_app.router.add_post('/tokens', handle_tokens_post)
        control_plane_runner = web.AppRunner(api_app)
        await control_plane_runner.setup()
        site = web.TCPSite(control_plane_runner, '0.0.0.0', settings.control_port)
        await site.start()
        logger.info(f"Control Plane API listening on port {settings.control_port}")

    if hasattr(input_handler, "connect"):
        tasks_to_run.append(
            asyncio.create_task(input_handler.connect(), name="InputConnect")
        )
    if hasattr(input_handler, "start_clipboard"):
        input_handler.clipboard_monitor_task = asyncio.create_task(input_handler.start_clipboard(), name="ClipboardMon")
        tasks_to_run.append(input_handler.clipboard_monitor_task)
    if hasattr(input_handler, "start_cursor_monitor"):
        tasks_to_run.append(
            asyncio.create_task(input_handler.start_cursor_monitor(), name="CursorMon")
        )

    try:
        logger.info("All main components initialized. Running server...")
        if data_server_task:
            await data_server_task
            if data_server_task.exception():
                logger.error(
                    "DataStreamingServer task exited with an exception.",
                    exc_info=data_server_task.exception(),
                )
            else:
                logger.info("DataStreamingServer task completed.")
        else:
            logger.error("DataStreamingServer task was not created. Cannot run.")

    except asyncio.CancelledError:
        logger.info("Main application task was cancelled.")
    except Exception as e_main:
        logger.critical(f"Critical error in main execution: {e_main}", exc_info=True)
    finally:
        if control_plane_runner:
            await control_plane_runner.cleanup()
            logger.info("Control Plane API shut down.")
        logger.info("Main loop ending or interrupted. Performing cleanup...")

        all_tasks_for_cleanup = [
            t for t in tasks_to_run if t and t is not data_server_task and not t.done()
        ]

        for task in all_tasks_for_cleanup:
            logger.debug(f"Cancelling task: {task.get_name()}")
            task.cancel()

        if all_tasks_for_cleanup:
            await asyncio.gather(*all_tasks_for_cleanup, return_exceptions=True)
            logger.info("Auxiliary tasks cancellation complete.")

        if app and hasattr(app, "stop_pipeline"):
            logger.info("Stopping SelkiesStreamingApp pipelines...")
            await app.stop_pipeline()

        if input_handler:
            logger.info("Stopping global InputHandler components...")
            if hasattr(input_handler, "stop_clipboard"):
                input_handler.stop_clipboard()
            if hasattr(input_handler, "stop_cursor_monitor"):
                input_handler.stop_cursor_monitor()
            if hasattr(input_handler, "stop_js_server") and asyncio.iscoroutinefunction(
                input_handler.stop_js_server
            ):
                await input_handler.stop_js_server()
            if hasattr(input_handler, "disconnect") and asyncio.iscoroutinefunction(
                input_handler.disconnect
            ):
                await input_handler.disconnect()
        if (
            data_server
            and hasattr(data_server, "stop")
            and asyncio.iscoroutinefunction(data_server.stop)
        ):
            logger.info("Ensuring DataStreamingServer resources are released...")
            await data_server.stop()
        logger.info("Cleanup complete. Exiting.")

if __name__ == "__main__":
    try:
        asyncio.run(ws_entrypoint())
    except KeyboardInterrupt:
        logger.info("Application stopped by KeyboardInterrupt.")
    except SystemExit as e:
        logger.info(f"Application exited with code {e.code}.")
    except Exception:
        logger.critical("Unhandled exception at entrypoint:", exc_info=True)
    finally:
        logging.shutdown()
