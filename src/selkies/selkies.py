# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
TARGET_VIDEO_BITRATE_KBPS = 16000
MIN_VIDEO_BITRATE_KBPS = 500

UINPUT_MOUSE_SOCKET = ""
JS_SOCKET_PATH = "/tmp"
ENABLE_CLIPBOARD = True
ENABLE_CURSORS = True
CURSOR_SIZE = 32
DEBUG_CURSORS = False
ENABLE_RESIZE = True
AUDIO_CHANNELS_DEFAULT = 2
AUDIO_BITRATE_DEFAULT = 320000
GPU_ID_DEFAULT = 0
KEYFRAME_DISTANCE_DEFAULT = -1.0
PIXELFLUX_VIDEO_ENCODERS = ["jpeg", "x264enc", "x264enc-striped"]

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
import os
import pathlib
import re
import struct
from asyncio import subprocess
import sys
import time
import websockets
import websockets.asyncio.server as ws_async
from collections import OrderedDict, deque
from datetime import datetime
from shutil import which
from signal import SIGINT, signal

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

upload_dir_path = os.path.expanduser("~/Desktop")
try:
    os.makedirs(upload_dir_path, exist_ok=True)
    logger.info(f"Upload directory ensured: {upload_dir_path}")
except OSError as e:
    logger.error(f"Could not create upload directory {upload_dir_path}: {e}")
    upload_dir_path = None


class SelkiesAppError(Exception):
    pass


class SelkiesStreamingApp:
    def __init__(
        self,
        async_event_loop,
        framerate,
        encoder,
        video_bitrate,
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
        self.keyframe_distance = KEYFRAME_DISTANCE_DEFAULT
        self.encoder = encoder
        self.framerate = framerate
        self.video_bitrate = video_bitrate
        self.min_keyframe_frame_distance = 60
        self.keyframe_frame_distance = (
            -1
            if self.keyframe_distance == -1.0
            else max(
                self.min_keyframe_frame_distance,
                int(self.framerate * self.keyframe_distance),
            )
        )
        self.last_cursor_sent = None
        self.data_streaming_server = data_streaming_server
        self._current_server_fps = 0.0
        self._ws_frame_count = 0
        self._ws_fps_last_calc_time = time.monotonic()
        self._fps_interval_sec = 2.0

    def send_ws_clipboard_data(
        self, data
    ):
        if (
            self.data_streaming_server
            and hasattr(self.data_streaming_server, "clients")
            and self.data_streaming_server.clients
            and self.async_event_loop
            and self.async_event_loop.is_running()
        ):

            msg_to_broadcast = f"clipboard,{base64.b64encode(data.encode()).decode()}"
            clients_ref = self.data_streaming_server.clients

            async def _broadcast_clipboard_helper():
                websockets.broadcast(clients_ref, msg_to_broadcast)

            asyncio.run_coroutine_threadsafe(
                _broadcast_clipboard_helper(), self.async_event_loop
            )
        else:
            data_logger.warning(
                "Cannot broadcast clipboard data: no clients connected or server not ready."
            )

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
        self.pipeline_running = False
        logger_gst_app.info("Pipelines stop signal processed.")

    stop_ws_pipeline = stop_pipeline

    def get_current_server_fps(self):
        return self._current_server_fps

    def set_framerate(self, framerate):
        self.framerate = int(framerate)
        self.keyframe_frame_distance = (
            -1
            if self.keyframe_distance == -1.0
            else max(
                self.min_keyframe_frame_distance,
                int(self.framerate * self.keyframe_distance),
            )
        )
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

        # Use res_str (e.g., "2560x1280") as the mode name for --addmode
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
            await rmmode_proc.communicate()
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

async def _run_xfconf(dpi_value, logger):
    """Helper function to apply DPI via xfconf-query for XFCE."""
    if not which("xfconf-query"):
        logger.debug("xfconf-query not found. Skipping XFCE DPI setting via xfconf-query.")
        return False

    cmd_xfconf = [
        "xfconf-query",
        "-c", "xsettings",
        "-p", "/Xft/DPI",
        "-s", str(dpi_value),
        "--create",
        "-t", "int",
    ]
    try:
        process = await subprocess.create_subprocess_exec(
            *cmd_xfconf,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        stdout, stderr = await process.communicate()
        if process.returncode == 0:
            logger.info(f"Successfully set XFCE DPI to {dpi_value} using xfconf-query.")
            return True
        else:
            logger.warning(f"Failed to set XFCE DPI using xfconf-query. RC: {process.returncode}, Error: {stderr.decode().strip()}")
            return False
    except Exception as e:
        logger.error(f"Error running xfconf-query: {e}")
        return False

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
    ):
        self.port = port
        self.mode = "websockets"
        self.server = None
        self.stop_server = None
        self.data_ws = (
            None  # Represents the specific connection in a ws_handler context
        )
        self.clients = set()  # Set of all active client WebSocket connections
        self.app = app
        self.cli_args = cli_args
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
        self._initial_x264_crf = self.cli_args.h264_crf
        self.h264_crf = self._initial_x264_crf
        self._initial_h264_fullcolor = self.cli_args.h264_fullcolor
        self.h264_fullcolor = self._initial_h264_fullcolor
        self._initial_h264_streaming_mode = self.cli_args.h264_streaming_mode
        self.h264_streaming_mode = self._initial_h264_streaming_mode
        self.capture_cursor = False
        self._initial_jpeg_quality = 60
        self.jpeg_quality = self._initial_jpeg_quality
        self._initial_paint_over_jpeg_quality = 90
        self.paint_over_jpeg_quality = self._initial_paint_over_jpeg_quality
        self._initial_h264_paintover_crf = 18
        self.h264_paintover_crf = self._initial_h264_paintover_crf
        self._initial_h264_paintover_burst_frames = 5
        self.h264_paintover_burst_frames = self._initial_h264_paintover_burst_frames
        self._initial_use_cpu = False
        self.use_cpu = self._initial_use_cpu
        self._initial_use_paint_over_quality = True
        self.use_paint_over_quality = self._initial_use_paint_over_quality
        self._system_monitor_task_ws = None
        self._gpu_monitor_task_ws = None
        self._stats_sender_task_ws = None
        self._shared_stats_ws = {}
        self.uinput_mouse_socket = uinput_mouse_socket
        self.js_socket_path = js_socket_path
        self.enable_clipboard = enable_clipboard
        self.enable_cursors = enable_cursors
        self.cursor_size = cursor_size
        self.cursor_scale = cursor_scale
        self.cursor_debug = cursor_debug
        self.input_handler = None
        self._last_adjustment_timestamp = 0.0
        self.jpeg_capture_module = None
        self.is_jpeg_capturing = False
        self.jpeg_capture_loop = None
        self.x264_striped_capture_module = None
        self.is_x264_striped_capturing = False
        self.client_settings_received = None
        self._initial_target_bitrate_kbps = (
            self.app.video_bitrate if self.app else TARGET_VIDEO_BITRATE_KBPS
        )
        self._current_target_bitrate_kbps = self._initial_target_bitrate_kbps
        self._pipeline_lock = asyncio.Lock()
        self._bytes_sent_in_interval = 0
        self._last_bandwidth_calc_time = time.monotonic()
        # Frame-based backpressure settings
        self.allowed_desync_ms = BACKPRESSURE_ALLOWED_DESYNC_MS
        self.latency_threshold_for_adjustment_ms = BACKPRESSURE_LATENCY_THRESHOLD_MS
        self.backpressure_check_interval_s = BACKPRESSURE_CHECK_INTERVAL_S
        self._backpressure_send_frames_enabled = True
        self._last_client_frame_id_report_time = 0.0

        # pcmflux audio capture state
        self.audio_device_name = audio_device_name
        self.pcmflux_module = None
        self.is_pcmflux_capturing = False
        self.pcmflux_settings = None
        self.pcmflux_callback = None
        self.pcmflux_audio_queue = None
        self.pcmflux_send_task = None
        self.pcmflux_capture_loop = None

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

                if not self.clients:
                    self.pcmflux_audio_queue.task_done()
                    continue
                
                # Protocol: 1-byte data type (0x01=audio) + 1-byte frame type (0x00=opus) + payload
                message_to_send = b'\x01\x00' + opus_bytes
                self._bytes_sent_in_interval += len(message_to_send)
                active_clients = list(self.clients)
                tasks = [client.send(message_to_send) for client in active_clients]
                if tasks:
                    await asyncio.gather(*tasks, return_exceptions=True)

                self.pcmflux_audio_queue.task_done()
        except asyncio.CancelledError:
            data_logger.info("pcmflux audio chunk broadcasting task cancelled.")
        finally:
            data_logger.info("pcmflux audio chunk broadcasting task finished.")

    async def _start_pcmflux_pipeline(self):
        if not PCMFLUX_AVAILABLE:
            data_logger.error("Cannot start audio pipeline: pcmflux library not available.")
            return False
        if self.is_pcmflux_capturing:
            data_logger.info("pcmflux audio pipeline is already capturing.")
            return True
        if not self.app:
            data_logger.error("Cannot start pcmflux: self.app (SelkiesStreamingApp instance) is not available.")
            return False
        
        self.pcmflux_capture_loop = self.jpeg_capture_loop or asyncio.get_running_loop()
        if not self.pcmflux_capture_loop:
            data_logger.error("Cannot start pcmflux: asyncio event loop not found.")
            return False

        data_logger.info("Starting pcmflux audio pipeline...")
        try:
            settings = AudioCaptureSettings()
            # To capture desktop audio on Linux with PulseAudio, find the ".monitor" source name.
            # Use `pactl list sources` in a terminal.
            # Set to None or empty string to use the system's default microphone.
            device_name_bytes = self.audio_device_name.encode('utf-8') if self.audio_device_name else None
            settings.device_name = device_name_bytes
            settings.sample_rate = 48000
            settings.channels = self.app.audio_channels
            settings.opus_bitrate = self.app.audio_bitrate
            settings.frame_duration_ms = 20
            settings.use_vbr = True
            settings.use_silence_gate = False
            self.pcmflux_settings = settings

            data_logger.info(f"pcmflux settings: device='{self.audio_device_name}', "
                             f"bitrate={settings.opus_bitrate}, channels={settings.channels}")

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
        self.is_jpeg_capturing = False
        self.is_x264_striped_capturing = False
        self.is_pcmflux_capturing = False
        await asyncio.sleep(0.01)
        stop_tasks = []
        loop = asyncio.get_running_loop()
        if self.jpeg_capture_module:
            logger.info("Queueing JPEG capture stop.")
            stop_tasks.append(
                loop.run_in_executor(None, self.jpeg_capture_module.stop_capture)
            )
        
        if self.x264_striped_capture_module:
            logger.info("Queueing x264-striped capture stop.")
            stop_tasks.append(
                loop.run_in_executor(None, self.x264_striped_capture_module.stop_capture)
            )

        if self.pcmflux_module:
            logger.info("Queueing pcmflux audio capture stop.")
            stop_tasks.append(
                loop.run_in_executor(None, self.pcmflux_module.stop_capture)
            )
        if stop_tasks:
            logger.info(f"Waiting for {len(stop_tasks)} capture module(s) to stop...")
            await asyncio.gather(*stop_tasks, return_exceptions=True)
            logger.info("All C++ capture modules have stopped.")
        if self.jpeg_capture_module:
            del self.jpeg_capture_module
            self.jpeg_capture_module = None
        if self.x264_striped_capture_module:
            del self.x264_striped_capture_module
            self.x264_striped_capture_module = None
        if self.pcmflux_module:
            del self.pcmflux_module
            self.pcmflux_module = None
        await self._ensure_backpressure_task_is_stopped()
        if self.pcmflux_send_task and not self.pcmflux_send_task.done():
            self.pcmflux_send_task.cancel()
            try:
                await self.pcmflux_send_task
            except asyncio.CancelledError:
                pass
        
        logger.info("Unified pipeline shutdown complete.")

    async def _ensure_backpressure_task_is_stopped(self):
        """
        Safely cancels and cleans up the _frame_backpressure_task.
        If the task was running before being stopped, it calls _reset_frame_ids_and_notify.
        _reset_frame_ids_and_notify will then check for active clients before broadcasting.
        Sets _backpressure_send_frames_enabled to True by default.
        """
        task_was_actually_running_and_cancelled = False
        if self._frame_backpressure_task and not self._frame_backpressure_task.done():
            data_logger.debug("Ensuring frame backpressure task is stopped.")
            self._frame_backpressure_task.cancel()
            try:
                await self._frame_backpressure_task
                task_was_actually_running_and_cancelled = True 
            except asyncio.CancelledError:
                data_logger.debug("Frame backpressure task cancelled successfully.")
                task_was_actually_running_and_cancelled = True 
            except Exception as e_cancel:
                data_logger.error(f"Error awaiting backpressure task cancellation: {e_cancel}")
            self._frame_backpressure_task = None
        
        self._backpressure_send_frames_enabled = True 

        if task_was_actually_running_and_cancelled:
            data_logger.info("Backpressure task was stopped. Calling _reset_frame_ids_and_notify.")
            await self._reset_frame_ids_and_notify()

    async def _reset_frame_ids_and_notify(self):
        data_logger.info("Resetting frame IDs.")
        self._active_pipeline_last_sent_frame_id = 0
        self._client_acknowledged_frame_id = -1
        
        if self.clients:
            data_logger.info(f"Broadcasting PIPELINE_RESETTING to {len(self.clients)} client(s).")
            websockets.broadcast(self.clients, "PIPELINE_RESETTING 0")
        else:
            data_logger.info("Frame IDs reset, but no clients to notify.")
            
        self._backpressure_send_frames_enabled = True
        self._last_client_acknowledged_frame_id_update_time = (
            time.monotonic()
        )

    async def _start_backpressure_task_if_needed(self):
        """
        Starts the _frame_backpressure_task if a video pipeline is active
        and client settings have been received.
        Ensures any old task is stopped first (without client notification from *this specific call*,
        as pipeline start/restart logic handles its own notifications).
        """
        await self._ensure_backpressure_task_is_stopped()

        if not self.client_settings_received or not self.client_settings_received.is_set():
            data_logger.warning(
                "Attempting to start backpressure task, but client_settings_received event is not set or None. "
                "The task will wait for this event. Ensure it's set when initial client settings are processed."
            )
            if hasattr(self, 'client_settings_received') and \
               self.client_settings_received and \
               isinstance(self.client_settings_received, asyncio.Event) and \
               not self.client_settings_received.is_set():
                 data_logger.info("Trying to ensure client_settings_received is set for backpressure task start.")
                 self.client_settings_received.set()

        if not self._frame_backpressure_task or self._frame_backpressure_task.done():
            self._frame_backpressure_task = asyncio.create_task(
                self._run_frame_backpressure_logic()
            )
            data_logger.info(f"New frame backpressure task started (current encoder: '{self.app.encoder if self.app else 'N/A'}').")
        else:
            data_logger.warning("Frame backpressure task was already running or not properly cleaned up when trying to start. Not starting a new one.")


    def is_video_pipeline_active(self):
        """Checks if any of the possible video pipelines are currently running."""
        if not self.app:
            return False

        jpeg_running = self.is_jpeg_capturing
        x264_running = self.is_x264_striped_capturing

        return jpeg_running or x264_running


    async def _run_frame_backpressure_logic(self):
        data_logger.info("Frame-based backpressure logic task started.")
        try:
            await self.client_settings_received.wait() # Ensure initial settings are processed
            data_logger.info("Client settings received, proceeding with backpressure loop.")

            while True:
                await asyncio.sleep(self.backpressure_check_interval_s)
                if not self.is_video_pipeline_active:
                    if not self._backpressure_send_frames_enabled:
                        data_logger.info("Backpressure LIFTED (video pipeline is not active).")
                    self._backpressure_send_frames_enabled = True
                    continue 
                if not self.clients: # No clients connected
                    self._backpressure_send_frames_enabled = True # Default to sending if no clients
                    continue

                current_server_frame_id = self._active_pipeline_last_sent_frame_id
                last_client_acked_frame_id = self._client_acknowledged_frame_id

                # Condition 1: Client hasn't ACKed anything yet after a server reset (ideal state)
                if last_client_acked_frame_id == -1:
                    if not self._backpressure_send_frames_enabled:
                         data_logger.info("Backpressure LIFTED (client ACK is -1). Enabling frame sending.")
                    self._backpressure_send_frames_enabled = True
                    self._last_client_acknowledged_frame_id_update_time = time.monotonic() # Reset stall detection with any ACK
                    continue

                # Condition 2: Client FPS is unknown or zero, cannot reliably calculate frame-based desync
                client_fps = self._latest_client_render_fps
                if client_fps <= 0 and self.app: 
                    client_fps = self.app.framerate
                if client_fps <= 0: 
                    if not self._backpressure_send_frames_enabled:
                        data_logger.info("Backpressure LIFTED (client FPS is 0 or unknown). Enabling frame sending.")
                    self._backpressure_send_frames_enabled = True 
                    continue
                
                server_id = current_server_frame_id
                client_id = last_client_acked_frame_id

                # Condition 3: Special handling for server just reset (S:0) and client ACKing small positive ID (C:>0)
                if server_id == 0 and client_id > 0 and client_id < FRAME_ID_SUSPICIOUS_GAP_THRESHOLD : # check client_id is not a huge number from a wrap
                    data_logger.debug(
                        f"Post-reset S:0, C:{client_id} scenario. Allowing frames to flow to resolve."
                    )
                    if not self._backpressure_send_frames_enabled:
                        data_logger.info("Backpressure LIFTED (Post-reset S:0, C:>0 scenario).")
                    self._backpressure_send_frames_enabled = True
                    self._last_client_acknowledged_frame_id_update_time = time.monotonic() # Reset stall detection
                    continue

                # Condition 4: Handle suspected frame ID wrap-around or very large actual desyncs
                if abs(server_id - client_id) > FRAME_ID_SUSPICIOUS_GAP_THRESHOLD:
                    data_logger.debug(
                        f"Frame ID wrap-around suspected or large gap (S:{server_id}, C:{client_id}). "
                        f"Skipping backpressure decision, ensuring frames flow."
                    )
                    if not self._backpressure_send_frames_enabled:
                        data_logger.info("Backpressure LIFTED due to suspected frame ID wrap/large gap.")
                    self._backpressure_send_frames_enabled = True
                    self._last_client_acknowledged_frame_id_update_time = time.monotonic() 
                    continue

                # --- Normal Desync Calculation ---
                # Never calculate on init 
                if server_id == 0:
                    return
                # Normal calculations
                if server_id >= client_id:
                    frame_desync = server_id - client_id
                else:
                    frame_desync = (MAX_UINT16_FRAME_ID - client_id) + server_id + 1
                
                if frame_desync < 0: 
                    frame_desync = 0

                allowed_desync_frames = (self.allowed_desync_ms / 1000.0) * client_fps
                current_rtt_ms = self._smoothed_rtt_ms
                latency_adjustment_frames = 0
                if current_rtt_ms > self.latency_threshold_for_adjustment_ms:
                    latency_adjustment_frames = (current_rtt_ms / 1000.0) * client_fps
                effective_desync_frames = frame_desync - latency_adjustment_frames

                time_since_last_ack = time.monotonic() - self._last_client_acknowledged_frame_id_update_time
                client_stalled = time_since_last_ack > STALLED_CLIENT_TIMEOUT_SECONDS

                if client_stalled:
                    if self._backpressure_send_frames_enabled:
                        data_logger.warning(
                            f"Client stall detected: No ACK update in {time_since_last_ack:.1f}s. "
                            f"Last ACK ID: {last_client_acked_frame_id}. Forcing backpressure."
                        )
                    self._backpressure_send_frames_enabled = False
                elif effective_desync_frames > allowed_desync_frames:
                    if frame_desync > 10000:
                      return
                    if self._backpressure_send_frames_enabled: 
                        data_logger.warning(
                            f"Backpressure TRIGGERED. S:{server_id}, C:{client_id} (Desync:{frame_desync:.0f}f, "
                            f"EffDesync:{effective_desync_frames:.1f}f > Allowed:{allowed_desync_frames:.1f}f). "
                            f"FPS:{client_fps:.1f}, RTT:{current_rtt_ms:.1f}ms. Disabling frame sending."
                        )
                    self._backpressure_send_frames_enabled = False
                else: 
                    if not self._backpressure_send_frames_enabled: 
                        data_logger.info(
                            f"Backpressure LIFTED. S:{server_id}, C:{client_id} (Desync:{frame_desync:.0f}f, "
                            f"EffDesync:{effective_desync_frames:.1f}f <= Allowed:{allowed_desync_frames:.1f}f). "
                            f"Enabling frame sending."
                        )
                    self._backpressure_send_frames_enabled = True

        except asyncio.CancelledError:
            data_logger.info("Frame-based backpressure logic task cancelled.")
        except Exception as e:
            data_logger.error(f"Error in frame-based backpressure logic: {e}", exc_info=True)
            self._backpressure_send_frames_enabled = True 
        finally:
            data_logger.info("Frame-based backpressure logic task finished.")
            self._backpressure_send_frames_enabled = True

    async def broadcast_stream_resolution(self):
        if (
            self.app
            and hasattr(self.app, "display_width")
            and hasattr(self.app, "display_height")
        ):
            # Ensure resolution is valid before broadcasting
            if self.app.display_width > 0 and self.app.display_height > 0:
                message = {
                    "type": "stream_resolution",
                    "width": self.app.display_width,
                    "height": self.app.display_height,
                }
                message_str = json.dumps(message)
                data_logger.info(f"Broadcasting stream resolution: {message_str}")
                if self.clients:
                    websockets.broadcast(self.clients, message_str)
            else:
                data_logger.warning(
                    f"Skipping stream resolution broadcast due to invalid dimensions: "
                    f"{self.app.display_width}x{self.app.display_height}"
                )
        else:
            data_logger.warning(
                "Cannot broadcast stream resolution: SelkiesStreamingApp instance or its display dimensions not available."
            )

    def _x264_striped_stripe_callback(self, result_ptr, user_data):
        current_async_loop = (
            self.jpeg_capture_loop
        )  # This is the loop for DataStreamingServer
        if (
            not self.is_x264_striped_capturing
            or not current_async_loop
            or not self.clients 
            or not result_ptr
        ):
            return
        try:
            result = result_ptr.contents
            if result.size <= 0:
                return
            payload_from_cpp = bytes(result.data[:result.size])
            clients_ref = self.clients
            data_to_send_ref = payload_from_cpp
            frame_id_ref = result.frame_id

            async def _broadcast_x264_data_and_update_frame_id():
                self.update_last_sent_frame_id(
                    frame_id_ref
                )  # Update server's knowledge of sent frame ID
                if not self._backpressure_send_frames_enabled:
                    return
                if clients_ref:
                    self._bytes_sent_in_interval += len(data_to_send_ref)
                    websockets.broadcast(clients_ref, data_to_send_ref)
            if current_async_loop and current_async_loop.is_running():
                asyncio.run_coroutine_threadsafe(
                    _broadcast_x264_data_and_update_frame_id(), current_async_loop
                )
        except Exception as e:
            data_logger.error(f"X264-Striped callback error: {e}", exc_info=True)
    async def _start_x264_striped_pipeline(self):
        if not X11_CAPTURE_AVAILABLE:
            data_logger.error("Cannot start x264-striped/x264enc: pixelflux library not available.")
            return False
        if self.is_x264_striped_capturing:
            data_logger.info(f"{self.app.encoder} pipeline is already capturing.")
            return True
        if not self.app:
            data_logger.error(f"Cannot start {self.app.encoder}: self.app (SelkiesStreamingApp instance) is not available.")
            return False
        
        self.jpeg_capture_loop = self.jpeg_capture_loop or asyncio.get_running_loop()
        if not self.jpeg_capture_loop:
            data_logger.error(f"Cannot start {self.app.encoder}: asyncio event loop not found for executor.")
            return False

        width = getattr(self.app, "display_width", 1024)
        height = getattr(self.app, "display_height", 768)
        fps = float(getattr(self.app, "framerate", TARGET_FRAMERATE))
        
        crf = self.h264_crf 

        # Determine if fullframe should be enabled based on the specific encoder string
        enable_fullframe = False
        if self.app.encoder == "x264enc": # The new default pixelflux mode
            enable_fullframe = True
        # For "x264enc-striped", fullframe remains False by default here.

        data_logger.info(
            f"Starting {self.app.encoder}: {width}x{height} @ {fps}fps, CRF: {crf}, FullFrame: {enable_fullframe}"
        )
        try:
            cs = CaptureSettings()
            cs.capture_width = width
            cs.capture_height = height
            cs.target_fps = fps
            cs.output_mode = 1
            cs.h264_crf = crf

            cs.use_paint_over_quality = self.use_paint_over_quality
            cs.h264_paintover_crf = self.h264_paintover_crf
            cs.h264_paintover_burst_frames = self.h264_paintover_burst_frames
            cs.paint_over_trigger_frames = 5
            cs.damage_block_threshold = 10
            cs.damage_block_duration = 20
            cs.h264_fullcolor = self.h264_fullcolor
            cs.h264_fullframe = enable_fullframe
            cs.h264_streaming_mode = self.h264_streaming_mode 
            cs.capture_cursor = self.capture_cursor
            cs.use_cpu = self.use_cpu
            if self.cli_args.dri_node:
                cs.vaapi_render_node_index = parse_dri_node_to_index(self.cli_args.dri_node)
            else:
                cs.vaapi_render_node_index = -1
 
            cs.capture_x = 0
            cs.capture_y = 0

            watermark_path_str = self.cli_args.watermark_path
            if watermark_path_str and os.path.exists(watermark_path_str):
                cs.watermark_path = watermark_path_str.encode('utf-8')
                watermark_location = self.cli_args.watermark_location
                if watermark_location < 0 or watermark_location > 6:
                    cs.watermark_location_enum = 4
                else:
                    cs.watermark_location_enum = watermark_location
                data_logger.info(f"Applying watermark to {self.app.encoder}: {watermark_path_str} at location {cs.watermark_location_enum}")
            elif watermark_path_str:
                data_logger.warning(f"Watermark path specified for {self.app.encoder} but file not found: {watermark_path_str}")

            data_logger.debug(f"{self.app.encoder} CaptureSettings: w={cs.capture_width}, h={cs.capture_height}, fps={cs.target_fps}, "
                              f"crf={cs.h264_crf}, use_paint_over={cs.use_paint_over_quality}, "
                              f"trigger_frames={cs.paint_over_trigger_frames}, "
                              f"dmg_thresh={cs.damage_block_threshold}, dmg_dur={cs.damage_block_duration}, "
                              f"fullframe={cs.h264_fullframe}, fullcolor={cs.h264_fullcolor}")

            # Ensure module is fresh if it existed
            if self.x264_striped_capture_module:
                del self.x264_striped_capture_module
            self.x264_striped_capture_module = ScreenCapture()
            
            await self.jpeg_capture_loop.run_in_executor(
               None, self.x264_striped_capture_module.start_capture, cs, self._x264_striped_stripe_callback
            )
            self.is_x264_striped_capturing = True
            await self._start_backpressure_task_if_needed()
            data_logger.info(f"{self.app.encoder} capture started successfully.")
            return True
        except Exception as e:
            data_logger.error(f"Failed to start {self.app.encoder}: {e}", exc_info=True)
            self.is_x264_striped_capturing = False
            if self.x264_striped_capture_module:
                del self.x264_striped_capture_module
                self.x264_striped_capture_module = None
            return False

    async def _stop_x264_striped_pipeline(self):
        if not self.is_x264_striped_capturing or not self.x264_striped_capture_module:
            return True
        self.is_x264_striped_capturing = False
        data_logger.info("Stopping X11 x264-striped capture...")
        try:
            if self.jpeg_capture_loop and self.x264_striped_capture_module:
                await self.jpeg_capture_loop.run_in_executor(
                    None, self.x264_striped_capture_module.stop_capture
                )
        finally:
            if self.x264_striped_capture_module:
                del self.x264_striped_capture_module
                self.x264_striped_capture_module = None
            await self._ensure_backpressure_task_is_stopped()
        return True

    def _jpeg_stripe_callback(self, result_ptr, user_data):
        current_async_loop = (
            self.jpeg_capture_loop
        )  # This is the loop for DataStreamingServer
        if (
            not self.is_jpeg_capturing
            or not current_async_loop
            or not self.clients
            or not result_ptr
        ):
            return
        try:
            result = result_ptr.contents
            if result.size <= 0:
                return
            jpeg_buffer = bytes(result.data[:result.size])
            clients_ref = self.clients
            prefixed_jpeg_data = b"\x03\x00" + jpeg_buffer
            frame_id_ref = result.frame_id
            async def _broadcast_jpeg_data_and_update_frame_id():
                # self here refers to DataStreamingServer instance
                self.update_last_sent_frame_id(
                    frame_id_ref
                )  # Update server's knowledge of sent frame ID
                if not self._backpressure_send_frames_enabled:
                    return
                if clients_ref:
                    self._bytes_sent_in_interval += len(prefixed_jpeg_data)
                    websockets.broadcast(clients_ref, prefixed_jpeg_data)
            if current_async_loop and current_async_loop.is_running():
                asyncio.run_coroutine_threadsafe(
                    _broadcast_jpeg_data_and_update_frame_id(), current_async_loop
                )
        except Exception as e:
            data_logger.error(f"JPEG callback error: {e}", exc_info=True)

    async def _start_jpeg_pipeline(self):
        if not X11_CAPTURE_AVAILABLE:
            return False
        if self.is_jpeg_capturing:
            return True
        if not self.app:
            return False
        self.jpeg_capture_loop = self.jpeg_capture_loop or asyncio.get_running_loop()
        if not self.jpeg_capture_loop:
            return False

        width = getattr(self.app, "display_width", 1024)
        height = getattr(self.app, "display_height", 768)
        fps = float(getattr(self.app, "framerate", TARGET_FRAMERATE))

        data_logger.info(f"Starting JPEG: {width}x{height} @ {fps}fps, Q: {self.jpeg_quality}")
        try:
            cs = CaptureSettings()
            cs.capture_width = width
            cs.capture_height = height
            cs.capture_x = 0
            cs.capture_y = 0
            cs.target_fps = fps
            cs.output_mode = 0
            cs.capture_cursor = self.capture_cursor
            cs.jpeg_quality = self.jpeg_quality
            cs.paint_over_jpeg_quality = self.paint_over_jpeg_quality
            cs.use_paint_over_quality = self.use_paint_over_quality
            cs.paint_over_trigger_frames = 15
            cs.damage_block_threshold = 10
            cs.damage_block_duration = 20

            watermark_path_str = self.cli_args.watermark_path
            if watermark_path_str and os.path.exists(watermark_path_str):
                cs.watermark_path = watermark_path_str.encode('utf-8')
                watermark_location = self.cli_args.watermark_location
                if watermark_location < 0 or watermark_location > 6:
                    cs.watermark_location_enum = 4
                else:
                    cs.watermark_location_enum = watermark_location
                data_logger.info(f"Applying watermark to JPEG: {watermark_path_str} at location {cs.watermark_location_enum}")
            elif watermark_path_str:
                data_logger.warning(f"Watermark path specified for JPEG but file not found: {watermark_path_str}")

            if self.jpeg_capture_module:
                del self.jpeg_capture_module
            self.jpeg_capture_module = ScreenCapture()

            await self.jpeg_capture_loop.run_in_executor(
                None, self.jpeg_capture_module.start_capture, cs, self._jpeg_stripe_callback
            )
            self.is_jpeg_capturing = True
            data_logger.info("X11 JPEG capture started with detailed settings.")
            await self._start_backpressure_task_if_needed()
            return True
        except Exception as e:
            data_logger.error(f"Failed to start JPEG: {e}", exc_info=True)
            self.is_jpeg_capturing = False
            if self.jpeg_capture_module:
                del self.jpeg_capture_module
                self.jpeg_capture_module = None
            return False

    async def _stop_jpeg_pipeline(self):
        if not self.is_jpeg_capturing or not self.jpeg_capture_module:
            return True
        self.is_jpeg_capturing = False
        data_logger.info("Stopping X11 JPEG capture...")
        try:
            if self.jpeg_capture_loop and self.jpeg_capture_module:
                await self.jpeg_capture_loop.run_in_executor(
                    None, self.jpeg_capture_module.stop_capture
                )
        finally:
            if self.jpeg_capture_module:
                del self.jpeg_capture_module
                self.jpeg_capture_module = None
            await self._ensure_backpressure_task_is_stopped()
        return True

    def update_last_sent_frame_id(self, frame_id: int):
        self._active_pipeline_last_sent_frame_id = frame_id
        now = time.monotonic()
        self._sent_frame_timestamps[frame_id] = now
        if len(self._sent_frame_timestamps) > SENT_FRAME_TIMESTAMP_HISTORY_SIZE:
            self._sent_frame_timestamps.popitem(last=False)
        if hasattr(self, "_sent_frames_log"):
            self._sent_frames_log.append((now, frame_id))

    def _parse_settings_payload(self, payload_str: str) -> dict:
        settings_data = json.loads(payload_str)
        parsed = {}

        def get_int(k, d):
            v = settings_data.get(k)
            return int(v) if v is not None else d

        def get_bool(k, d):
            v = settings_data.get(k)
            return str(v).lower() == "true" if v is not None else d

        def get_str(k, d):
            v = settings_data.get(k)
            return str(v) if v is not None else d

        parsed["videoBitRate"] = get_int(
            "webrtc_videoBitRate", self.app.video_bitrate * 1000
        )
        parsed["videoFramerate"] = get_int("webrtc_videoFramerate", self.app.framerate)
        parsed["videoCRF"] = get_int("webrtc_videoCRF", self.h264_crf)
        parsed["encoder"] = get_str("webrtc_encoder", self.app.encoder)
        parsed["h264_fullcolor"] = get_bool("webrtc_h264_fullcolor", self.h264_fullcolor)
        parsed["h264_streaming_mode"] = get_bool("webrtc_h264_streaming_mode", self.h264_streaming_mode)
        parsed["resizeRemote"] = get_bool(
            "webrtc_resizeRemote",
            getattr(self.app, "client_preferred_resize_enabled", True),
        )
        parsed["isManualResolutionMode"] = get_bool(
            "webrtc_isManualResolutionMode",
            getattr(self.app, "client_is_manual_resolution_mode", False),
        )
        parsed["manualWidth"] = get_int(
            "webrtc_manualWidth",
            getattr(self.app, "client_manual_width", self.app.display_width),
        )
        parsed["manualHeight"] = get_int(
            "webrtc_manualHeight",
            getattr(self.app, "client_manual_height", self.app.display_height),
        )
        parsed["audioBitRate"] = get_int("webrtc_audioBitRate", self.app.audio_bitrate)
        parsed["videoBufferSize"] = get_int(
            "webrtc_videoBufferSize", getattr(self.app, "video_buffer_size", 0)
        )
        parsed["initialClientWidth"] = get_int(
            "webrtc_initialClientWidth", self.app.display_width
        )
        parsed["initialClientHeight"] = get_int(
            "webrtc_initialClientHeight", self.app.display_height
        )
        parsed["jpeg_quality"] = get_int("pixelflux_jpeg_quality", self.jpeg_quality)
        parsed["paint_over_jpeg_quality"] = get_int(
            "pixelflux_paint_over_jpeg_quality", self.paint_over_jpeg_quality
        )
        parsed["use_cpu"] = get_bool("pixelflux_use_cpu", self.use_cpu)
        parsed["h264_paintover_crf"] = get_int("pixelflux_h264_paintover_crf", self.h264_paintover_crf)
        parsed["h264_paintover_burst_frames"] = get_int("pixelflux_h264_paintover_burst_frames", self.h264_paintover_burst_frames)
        parsed["use_paint_over_quality"] = get_bool("pixelflux_use_paint_over_quality", self.use_paint_over_quality)
        parsed["scaling_dpi"] = get_int("webrtc_SCALING_DPI", 96)
        data_logger.debug(f"Parsed client settings: {parsed}")
        return parsed

    async def _apply_client_settings(
        self, websocket_obj, settings: dict, is_initial_settings: bool
    ):
        data_logger.info(
            f"Applying client settings (initial={is_initial_settings}): {settings}"
        )
        old_encoder = self.app.encoder
        old_video_bitrate_kbps = self.app.video_bitrate
        old_framerate = self.app.framerate
        old_h264_crf = self.h264_crf
        old_h264_fullcolor = self.h264_fullcolor
        old_h264_streaming_mode = self.h264_streaming_mode
        old_audio_bitrate_bps = self.app.audio_bitrate
        old_display_width = self.app.display_width
        old_display_height = self.app.display_height
        old_jpeg_quality = self.jpeg_quality
        old_paint_over_jpeg_quality = self.paint_over_jpeg_quality
        old_use_cpu = self.use_cpu
        old_h264_paintover_crf = self.h264_paintover_crf
        old_h264_paintover_burst_frames = self.h264_paintover_burst_frames
        old_use_paint_over_quality = self.use_paint_over_quality

        is_manual_res_mode_from_settings = settings.get(
            "isManualResolutionMode",
            getattr(self.app, "client_is_manual_resolution_mode", False),
        )
        target_w_for_app, target_h_for_app = old_display_width, old_display_height
        if is_manual_res_mode_from_settings:
            target_w_for_app = settings.get("manualWidth", old_display_width)
            target_h_for_app = settings.get("manualHeight", old_display_height)
        elif is_initial_settings:
            target_w_for_app = settings.get("initialClientWidth", old_display_width)
            target_h_for_app = settings.get("initialClientHeight", old_display_height)

        if target_w_for_app <= 0: target_w_for_app = old_display_width
        if target_h_for_app <= 0: target_h_for_app = old_display_height
        if target_w_for_app % 2 != 0: target_w_for_app -= 1
        if target_h_for_app % 2 != 0: target_h_for_app -= 1
        if target_w_for_app <= 0: target_w_for_app = old_display_width
        if target_h_for_app <= 0: target_h_for_app = old_display_height

        if (target_w_for_app != old_display_width or target_h_for_app != old_display_height):
            self.app.display_width = target_w_for_app
            self.app.display_height = target_h_for_app
            effective_resize_enabled = ENABLE_RESIZE and settings.get("resizeRemote", True)
            if effective_resize_enabled:
                await on_resize_handler(f"{self.app.display_width}x{self.app.display_height}", self.app, self)
        setattr(self.app, "client_is_manual_resolution_mode", is_manual_res_mode_from_settings)
        if is_manual_res_mode_from_settings:
            setattr(self.app, "client_manual_width", settings.get("manualWidth", getattr(self.app, "client_manual_width", old_display_width)))
            setattr(self.app, "client_manual_height", settings.get("manualHeight", getattr(self.app, "client_manual_height", old_display_height)))
        setattr(self.app, "client_preferred_resize_enabled", settings.get("resizeRemote", getattr(self.app, "client_preferred_resize_enabled", True)))

        encoder_actually_changed = False
        requested_new_encoder = settings.get("encoder")
        if requested_new_encoder and requested_new_encoder != old_encoder:
            if requested_new_encoder in PIXELFLUX_VIDEO_ENCODERS and X11_CAPTURE_AVAILABLE:
                self.app.encoder = requested_new_encoder
                encoder_actually_changed = True
                data_logger.info(f"Encoder changed from '{old_encoder}' to '{self.app.encoder}'.")
            else:
                data_logger.warning(f"Requested encoder '{requested_new_encoder}' is not available or not supported. Keeping '{old_encoder}'.")

        if "videoBitRate" in settings:
            new_bitrate_kbps = settings["videoBitRate"] // 1000
            if self.app.video_bitrate != new_bitrate_kbps:
                self.app.video_bitrate = new_bitrate_kbps

        if "videoFramerate" in settings:
            if self.app.framerate != settings["videoFramerate"]:
                self.app.framerate = settings["videoFramerate"]

        is_pixelflux_h264 = self.app.encoder in PIXELFLUX_VIDEO_ENCODERS and self.app.encoder != "jpeg"
        if "videoCRF" in settings and is_pixelflux_h264:
            if self.h264_crf != settings["videoCRF"]:
                self.h264_crf = settings["videoCRF"]

        if "h264_fullcolor" in settings and is_pixelflux_h264:
            if self.h264_fullcolor != settings["h264_fullcolor"]:
                self.h264_fullcolor = settings["h264_fullcolor"]

        if "h264_streaming_mode" in settings and is_pixelflux_h264:
            if self.h264_streaming_mode != settings["h264_streaming_mode"]:
                self.h264_streaming_mode = settings["h264_streaming_mode"]

        is_jpeg = self.app.encoder == "jpeg"
        if "jpeg_quality" in settings and is_jpeg:
            self.jpeg_quality = settings["jpeg_quality"]

        if "paint_over_jpeg_quality" in settings and is_jpeg:
            self.paint_over_jpeg_quality = settings["paint_over_jpeg_quality"]

        if "use_paint_over_quality" in settings:
            self.use_paint_over_quality = settings["use_paint_over_quality"]

        if "h264_paintover_crf" in settings and is_pixelflux_h264:
            self.h264_paintover_crf = settings["h264_paintover_crf"]

        if "h264_paintover_burst_frames" in settings and is_pixelflux_h264:
            self.h264_paintover_burst_frames = settings["h264_paintover_burst_frames"]

        if "use_cpu" in settings and is_pixelflux_h264:
            self.use_cpu = settings["use_cpu"]
 
        if "audioBitRate" in settings:
            if self.app.audio_bitrate != settings["audioBitRate"]:
                 self.app.audio_bitrate = settings["audioBitRate"]

        if "scaling_dpi" in settings:
            dpi_value = settings["scaling_dpi"]
            data_logger.info(f"Applying SCALING_DPI from initial settings: {dpi_value}")
            if await set_dpi(dpi_value):
                data_logger.info(f"Successfully set DPI to {dpi_value} from initial settings.")
            else:
                data_logger.error(f"Failed to set DPI to {dpi_value} from initial settings.")

            if CURSOR_SIZE > 0:
                calculated_cursor_size = int(round(dpi_value / 96.0 * CURSOR_SIZE))
                new_cursor_size = max(1, calculated_cursor_size)
                data_logger.info(f"Attempting to set cursor size to {new_cursor_size} based on initial DPI.")
                if await set_cursor_size(new_cursor_size):
                    data_logger.info(f"Successfully set cursor size to {new_cursor_size}.")
                else:
                    data_logger.error(f"Failed to set cursor size to {new_cursor_size}.")

        if "videoBufferSize" in settings:
            setattr(self.app, "video_buffer_size", settings["videoBufferSize"])
        async with self._pipeline_lock:
            resolution_actually_changed_on_server = (
                self.app.display_width != old_display_width
                or self.app.display_height != old_display_height
            )
            framerate_param_changed = self.app.framerate != old_framerate
            crf_param_changed = is_pixelflux_h264 and self.h264_crf != old_h264_crf
            h264_fullcolor_param_changed = is_pixelflux_h264 and self.h264_fullcolor != old_h264_fullcolor
            h264_streaming_mode_param_changed = is_pixelflux_h264 and self.h264_streaming_mode != old_h264_streaming_mode
            jpeg_quality_param_changed = is_jpeg and self.jpeg_quality != old_jpeg_quality
            paint_over_jpeg_quality_param_changed = (
                is_jpeg and self.paint_over_jpeg_quality != old_paint_over_jpeg_quality
            )
            use_cpu_param_changed = is_pixelflux_h264 and self.use_cpu != old_use_cpu
            h264_paintover_crf_param_changed = is_pixelflux_h264 and self.h264_paintover_crf != old_h264_paintover_crf
            h264_paintover_burst_frames_param_changed = is_pixelflux_h264 and self.h264_paintover_burst_frames != old_h264_paintover_burst_frames
            use_paint_over_quality_param_changed = self.use_paint_over_quality != old_use_paint_over_quality
            audio_bitrate_param_changed = self.app.audio_bitrate != old_audio_bitrate_bps
            restart_video_pipeline = False
            if encoder_actually_changed or resolution_actually_changed_on_server:
                restart_video_pipeline = True
            elif self.app.encoder in PIXELFLUX_VIDEO_ENCODERS:
                if framerate_param_changed:
                    restart_video_pipeline = True
                if is_pixelflux_h264 and (
                    crf_param_changed
                    or h264_fullcolor_param_changed
                    or h264_streaming_mode_param_changed
                    or use_cpu_param_changed
                    or h264_paintover_crf_param_changed
                    or h264_paintover_burst_frames_param_changed
                    or use_paint_over_quality_param_changed
                ):
                    restart_video_pipeline = True
                if is_jpeg and (
                    jpeg_quality_param_changed 
                    or paint_over_jpeg_quality_param_changed
                    or use_paint_over_quality_param_changed
                ):
                    restart_video_pipeline = True
            video_is_currently_active = self.is_jpeg_capturing or self.is_x264_striped_capturing
            if is_initial_settings and not video_is_currently_active:
                data_logger.warning(
                    "Pipeline is inactive for the initial client. Forcing a start."
                )
                restart_video_pipeline = True
            restart_audio_pipeline = audio_bitrate_param_changed
            if restart_video_pipeline:
                if self.is_jpeg_capturing or self.is_x264_striped_capturing:
                    data_logger.info(
                        f"Restarting video pipeline (was {old_encoder}, now {self.app.encoder}) due to settings change or inactive state."
                    )
                    if old_encoder == "jpeg": await self._stop_jpeg_pipeline()
                    elif old_encoder in PIXELFLUX_VIDEO_ENCODERS and old_encoder != "jpeg": await self._stop_x264_striped_pipeline()
                else:
                    data_logger.info(f"Video pipeline for {self.app.encoder} needs to start (was not active or forced).")
                if self.app.encoder == "jpeg": await self._start_jpeg_pipeline()
                elif self.app.encoder in PIXELFLUX_VIDEO_ENCODERS and self.app.encoder != "jpeg": await self._start_x264_striped_pipeline()
            if restart_audio_pipeline:
                if self.is_pcmflux_capturing:
                    data_logger.info("Restarting audio pipeline due to settings update.")
                    await self._stop_pcmflux_pipeline()
                    await self._start_pcmflux_pipeline()
        if is_initial_settings and self.client_settings_received and not self.client_settings_received.is_set():
            self.client_settings_received.set()
            data_logger.info("Initial client settings processed and event set by _apply_client_settings.")

    async def ws_handler(self, websocket):
        global TARGET_FRAMERATE, TARGET_VIDEO_BITRATE_KBPS
        raddr = websocket.remote_address
        data_logger.info(f"Data WebSocket connected from {raddr}")
        self.clients.add(websocket)
        self.data_ws = (
            websocket  # self.data_ws is specific to this handler instance/connection
        )
        self.jpeg_capture_loop = self.jpeg_capture_loop or asyncio.get_running_loop()
        self.client_settings_received = asyncio.Event()
        initial_settings_processed = False
        self._sent_frame_timestamps.clear()
        self._rtt_samples.clear()
        self._smoothed_rtt_ms = 0.0

        try:
            await websocket.send(f"MODE {self.mode}")
            await self.broadcast_stream_resolution()
        except websockets.exceptions.ConnectionClosed:
            self.clients.discard(websocket)  # Ensure removal on early exit
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

        available_encoders = []
        if X11_CAPTURE_AVAILABLE:
            available_encoders.append("x264enc")
            available_encoders.append("x264enc-striped")
            available_encoders.append("jpeg")

        server_settings_payload = {
            "type": "server_settings",
            "encoders": available_encoders,
        }
        try:
            await websocket.send(json.dumps(server_settings_payload))
        except websockets.exceptions.ConnectionClosed:
            self.clients.discard(websocket)  # Ensure removal on early exit
            if self.data_ws is websocket:
                self.data_ws = None
            return

        self._initial_target_bitrate_kbps = self.app.video_bitrate
        self._current_target_bitrate_kbps = self._initial_target_bitrate_kbps
        self._last_adjustment_time = self._last_time_client_ok = time.monotonic()
        self._active_pipeline_last_sent_frame_id = 0
        self._client_acknowledged_frame_id = -1
        self._last_client_acknowledged_frame_id_update_time = time.monotonic()
        self._previous_ack_id_for_stall_check = -1
        self._previous_sent_id_for_stall_check = -1
        self._last_client_stable_report_time = time.monotonic()
        self._initial_x264_crf = self.cli_args.h264_crf
        self.h264_crf = self._initial_x264_crf
        self.h264_fullcolor = self._initial_h264_fullcolor
        self.h264_streaming_mode = self._initial_h264_streaming_mode
        self.jpeg_quality = self._initial_jpeg_quality
        self.paint_over_jpeg_quality = self._initial_paint_over_jpeg_quality
        self.use_cpu = self._initial_use_cpu
        self.h264_paintover_crf = self._initial_h264_paintover_crf
        self.h264_paintover_burst_frames = self._initial_h264_paintover_burst_frames
        self.use_paint_over_quality = self._initial_use_paint_over_quality

        self._backpressure_send_frames_enabled = True
        active_uploads_by_path_conn = {}
        active_upload_target_path_conn = None
        upload_dir_valid = upload_dir_path is not None
        
        mic_setup_done = False 
        pa_module_index = None  # Stores the index of the loaded module-virtual-source
        pa_stream = None  # For pasimple playback
        pulse = None  # pulsectl.Pulse client instance
        
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
            )  # Stats are per-client
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
                    pulse = None  # Ensure pulse is None if connection fails

            async for message in websocket:
                if isinstance(message, bytes):
                    msg_type, payload = message[0], message[1:]
                    if msg_type == 0x01:  # File data
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
                        if not PULSEAUDIO_AVAILABLE:
                            if len(payload) > 0:
                                data_logger.warning(
                                    "PulseAudio library not available. Skipping microphone data."
                                )
                            continue
                        if pulse is None:  # Check if PulseAudio client object exists
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
                                    )  # Get module index if it exists
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

                                    # Verify creation
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
                                        ):  # Check if it's not None before trying to unload
                                            try:
                                                pulse.module_unload(pa_module_index)
                                            except Exception as unload_err:
                                                data_logger.error(
                                                    f"Failed to unload module {pa_module_index}: {unload_err}"
                                                )
                                            pa_module_index = None  # Reset on failure to unload or if it was problematic

                                if mic_setup_done:
                                    current_source_list = (
                                        pulse.source_list()
                                    )
                                    # Mic is automatically set to the source for recording (pcmflux) and input
                                    # Set pcmflux back to the input source
                                    if self.is_pcmflux_capturing:
                                        try:
                                            # Get all source outputs to find pcmflux
                                            source_outputs = pulse.source_output_list()
                                            pcmflux_output = None
                                            
                                            for output in source_outputs:
                                                if hasattr(output, 'proplist') and output.proplist.get('application.name') == 'pcmflux':
                                                    pcmflux_output = output
                                                    break
                                            
                                            if pcmflux_output:
                                                # Get the source pcmflux is connected to
                                                connected_source = None
                                                for source in current_source_list:
                                                    if source.index == pcmflux_output.source:
                                                        connected_source = source
                                                        break
                                                
                                                # Check if it's connected to the wrong source
                                                if connected_source and connected_source.name != self.audio_device_name:
                                                    data_logger.warning(
                                                        f"pcmflux connected to wrong source '{connected_source.name}', moving to '{self.audio_device_name}'"
                                                    )
                                                    
                                                    # Find the correct source to move to
                                                    correct_source = None
                                                    for source in current_source_list:
                                                        if source.name == self.audio_device_name:
                                                            correct_source = source
                                                            break
                                                    
                                                    if correct_source:
                                                        # Move pcmflux to the correct source
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
                                # No need to close pulse here, as it's managed by the outer try/finally
                                # Reset mic_setup_done to false on any error during setup
                                mic_setup_done = False
                                # If pa_module_index was set from a failed load, try to unload it.
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
                                    pa_module_index = None  # Reset after attempt
                                continue  # Skip processing this mic packet if setup failed

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
                                    device_name="input",  # Play to system's default input (which should be our virtual mic)
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
                    if message.startswith("FILE_UPLOAD_START:"):
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
                            await self._apply_client_settings(
                                websocket,
                                parsed_settings,
                                not initial_settings_processed,
                            )
                            if not initial_settings_processed: # ws_handler's local flag
                                initial_settings_processed = True
                                data_logger.info("Initial client settings message processed by ws_handler.")
                                current_encoder = getattr(self.app, "encoder", None)
                                video_is_active = (self.is_jpeg_capturing or self.is_x264_striped_capturing)
                                if not video_is_active and current_encoder:
                                    data_logger.warning(f"Initial setup: Video pipeline for '{current_encoder}' was expected to be started by _apply_client_settings but is not. This might indicate an issue or a no-op change.")
                                
                                async with self._pipeline_lock:
                                    audio_is_active = self.is_pcmflux_capturing
                                    if not audio_is_active and PCMFLUX_AVAILABLE:
                                        data_logger.info("Initial setup: Audio pipeline not yet active, attempting start.")
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
                            acked_frame_id = int(message.split(" ", 1)[1])
                            self._client_acknowledged_frame_id = acked_frame_id
                            self._last_client_acknowledged_frame_id_update_time = (
                                time.monotonic()
                            )
                            if acked_frame_id in self._sent_frame_timestamps:
                                send_time = self._sent_frame_timestamps.pop(
                                    acked_frame_id
                                )
                                rtt_sample_ms = (time.monotonic() - send_time) * 1000.0
                                if rtt_sample_ms >= 0:
                                    self._rtt_samples.append(rtt_sample_ms)
                                if self._rtt_samples:
                                    self._smoothed_rtt_ms = sum(
                                        self._rtt_samples
                                    ) / len(self._rtt_samples)
                        except (IndexError, ValueError):
                            data_logger.warning(
                                f"Malformed CLIENT_FRAME_ACK: {message}"
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

                    elif message == "PING":
                        try:
                            await websocket.send("PONG")
                            data_logger.debug(f"Responded to heartbeat PING from {raddr}")
                        except Exception as e:
                            data_logger.warning(f"Failed to send PONG response to {raddr}: {e}")

                    elif message == "START_VIDEO":
                        async with self._pipeline_lock:
                            current_encoder = getattr(self.app, "encoder", None)
                            data_logger.info(f"Received START_VIDEO for encoder: {current_encoder}")
                            started_successfully = False

                            if current_encoder == "jpeg":
                                started_successfully = await self._start_jpeg_pipeline()
                            elif current_encoder in PIXELFLUX_VIDEO_ENCODERS and current_encoder != "jpeg":
                                started_successfully = await self._start_x264_striped_pipeline()
                        
                            if started_successfully:
                                websockets.broadcast(self.clients, "VIDEO_STARTED")
                            elif not started_successfully:
                                data_logger.warning(f"START_VIDEO: Failed to start pipeline for encoder '{current_encoder}'.")

                    elif message == "STOP_VIDEO":
                        async with self._pipeline_lock:
                            data_logger.info("Received STOP_VIDEO")
                            current_encoder = getattr(self.app, "encoder", None) 

                            if self.is_jpeg_capturing and current_encoder == "jpeg":
                                await self._stop_jpeg_pipeline()
                            elif self.is_x264_striped_capturing and (current_encoder in PIXELFLUX_VIDEO_ENCODERS and current_encoder != "jpeg"):
                                await self._stop_x264_striped_pipeline()
                        
                            if self.clients:
                                websockets.broadcast(self.clients, "VIDEO_STOPPED")

                    elif message == "START_AUDIO":
                        async with self._pipeline_lock:
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
                        async with self._pipeline_lock:
                            data_logger.info("Received STOP_AUDIO")
                            if self.is_pcmflux_capturing:
                                await self._stop_pcmflux_pipeline()
                            if self.clients:
                                websockets.broadcast(self.clients, "AUDIO_STOPPED")

                    elif message.startswith("r,"):
                        await self.client_settings_received.wait() 
                        raddr = websocket.remote_address
                        target_res_str = message[2:]
                        current_res_str = f"{self.app.display_width}x{self.app.display_height}"
                        if target_res_str == current_res_str:
                            data_logger.info(f"Received redundant resize request for {target_res_str}. No action taken.")
                            continue
                        data_logger.info(f"Received resize request: {target_res_str} from {raddr}")

                        video_was_running = False
                        encoder_at_resize_start = str(self.app.encoder)
                        if self.is_jpeg_capturing and encoder_at_resize_start == "jpeg":
                            data_logger.info("Resize handler: Stopping JPEG pipeline.")
                            await self._stop_jpeg_pipeline()
                            video_was_running = True
                        elif self.is_x264_striped_capturing and (encoder_at_resize_start in PIXELFLUX_VIDEO_ENCODERS and encoder_at_resize_start != "jpeg"):
                            data_logger.info(f"Resize handler: Stopping {encoder_at_resize_start} (Pixelflux H264) pipeline.")
                            await self._stop_x264_striped_pipeline()
                            video_was_running = True
                        
                        await on_resize_handler(target_res_str, self.app, self)

                        if getattr(self.app, "last_resize_success", False) and video_was_running:
                            data_logger.info(f"Resize handler: Restarting video ({encoder_at_resize_start}) after successful resize to {self.app.display_width}x{self.app.display_height}")
                            if encoder_at_resize_start == "jpeg":
                                await self._start_jpeg_pipeline()
                            elif encoder_at_resize_start in PIXELFLUX_VIDEO_ENCODERS and encoder_at_resize_start != "jpeg":
                                await self._start_x264_striped_pipeline()

                    elif message.startswith("SET_ENCODER,"):
                        await self.client_settings_received.wait()
                        new_encoder_cmd = message.split(",")[1].strip().lower()
                        data_logger.info(f"Received SET_ENCODER: {new_encoder_cmd}")
                        
                        is_valid_new_encoder = False
                        if new_encoder_cmd in PIXELFLUX_VIDEO_ENCODERS and X11_CAPTURE_AVAILABLE:
                            is_valid_new_encoder = True
                        
                        if not is_valid_new_encoder:
                            data_logger.warning(f"SET_ENCODER: '{new_encoder_cmd}' is not valid or available. No change.")
                            continue

                        if new_encoder_cmd != self.app.encoder:
                            old_encoder_for_stop = str(self.app.encoder)
                            
                            if self.is_jpeg_capturing and old_encoder_for_stop == "jpeg":
                                await self._stop_jpeg_pipeline()
                            elif self.is_x264_striped_capturing and (old_encoder_for_stop in PIXELFLUX_VIDEO_ENCODERS and old_encoder_for_stop != "jpeg"):
                                await self._stop_x264_striped_pipeline()

                            self.app.encoder = new_encoder_cmd

                            # Start new pipeline
                            if new_encoder_cmd == "jpeg":
                                await self._start_jpeg_pipeline()
                            elif new_encoder_cmd in PIXELFLUX_VIDEO_ENCODERS and new_encoder_cmd != "jpeg":
                                await self._start_x264_striped_pipeline()
                            else: 
                                data_logger.warning(f"SET_ENCODER: No start method or support for validated new encoder {new_encoder_cmd}")
                        else:
                            data_logger.info(f"SET_ENCODER: Encoder '{new_encoder_cmd}' is already active.")

                    elif message.startswith("SET_FRAMERATE,"):
                        await self.client_settings_received.wait()
                        new_fps_cmd = int(message.split(",")[1])
                        data_logger.info(f"Received SET_FRAMERATE: {new_fps_cmd}")
                        
                        if self.app.framerate == new_fps_cmd:
                            data_logger.info(f"SET_FRAMERATE: Framerate {new_fps_cmd} is already set.")
                            continue

                        self.app.set_framerate(new_fps_cmd)

                        current_enc = getattr(self.app, "encoder", None)
                        is_pixelflux_h264_active = (current_enc in PIXELFLUX_VIDEO_ENCODERS and current_enc != "jpeg" and self.is_x264_striped_capturing)
                        is_jpeg_active = (current_enc == "jpeg" and self.is_jpeg_capturing)

                        if is_jpeg_active or is_pixelflux_h264_active:
                            data_logger.info(f"Restarting {current_enc} pipeline for new framerate {new_fps_cmd}")
                            if is_jpeg_active: await self._stop_jpeg_pipeline()
                            if is_pixelflux_h264_active: await self._stop_x264_striped_pipeline()
                            
                            if is_jpeg_active: await self._start_jpeg_pipeline()
                            if is_pixelflux_h264_active: await self._start_x264_striped_pipeline()

                    elif message.startswith("SET_CRF,"):
                        await self.client_settings_received.wait()
                        new_crf_cmd = int(message.split(",")[1])
                        data_logger.info(f"Received SET_CRF: {new_crf_cmd}")

                        current_enc = getattr(self.app, "encoder", None)
                        is_pixelflux_h264 = (current_enc in PIXELFLUX_VIDEO_ENCODERS and current_enc != "jpeg")

                        if is_pixelflux_h264:
                            if self.h264_crf != new_crf_cmd:
                                self.h264_crf = new_crf_cmd 
                                if self.is_x264_striped_capturing:
                                    data_logger.info(f"Restarting {current_enc} pipeline for CRF change to {self.h264_crf}")
                                    await self._stop_x264_striped_pipeline()
                                    await self._start_x264_striped_pipeline()
                            else:
                                data_logger.info(f"SET_CRF: Value {new_crf_cmd} is already set.")
                        else:
                            data_logger.warning(f"SET_CRF received but current encoder '{current_enc}' is not a Pixelflux H.264 encoder.")

                    elif message.startswith("SET_H264_FULLCOLOR,"):
                        await self.client_settings_received.wait()
                        try:
                            new_fullcolor_cmd_str = message.split(",")[1].strip().lower()
                            new_fullcolor_cmd = new_fullcolor_cmd_str == "true"
                            data_logger.info(f"Received SET_H264_FULLCOLOR: {new_fullcolor_cmd}")

                            current_enc = getattr(self.app, "encoder", None)
                            is_pixelflux_h264 = (current_enc in PIXELFLUX_VIDEO_ENCODERS and current_enc != "jpeg")

                            if is_pixelflux_h264:
                                if self.h264_fullcolor != new_fullcolor_cmd:
                                    self.h264_fullcolor = new_fullcolor_cmd
                                    if self.is_x264_striped_capturing:
                                        data_logger.info(f"Restarting {current_enc} pipeline for H264_FULLCOLOR change to {self.h264_fullcolor}")
                                        await self._stop_x264_striped_pipeline()
                                        await self._start_x264_striped_pipeline()
                                else:
                                    data_logger.info(f"SET_H264_FULLCOLOR: Value {new_fullcolor_cmd} is already set.")
                            else:
                                data_logger.warning(f"SET_H264_FULLCOLOR received but current encoder '{current_enc}' is not a Pixelflux H.264 encoder.")
                        except IndexError:
                            data_logger.warning(f"Malformed SET_H264_FULLCOLOR message: {message}")

                    elif message.startswith("SET_H264_STREAMING_MODE,"):
                        await self.client_settings_received.wait()
                        try:
                            new_streaming_mode_str = message.split(",")[1].strip().lower()
                            new_streaming_mode = new_streaming_mode_str == "true"
                            data_logger.info(f"Received SET_H264_STREAMING_MODE: {new_streaming_mode}")

                            current_enc = getattr(self.app, "encoder", None)
                            is_pixelflux_h264 = (current_enc in PIXELFLUX_VIDEO_ENCODERS and current_enc != "jpeg")

                            if is_pixelflux_h264:
                                if self.h264_streaming_mode != new_streaming_mode:
                                    self.h264_streaming_mode = new_streaming_mode
                                    if self.is_x264_striped_capturing:
                                        data_logger.info(f"Restarting {current_enc} pipeline for H264_STREAMING_MODE change to {self.h264_streaming_mode}")
                                        await self._stop_x264_striped_pipeline()
                                        await self._start_x264_striped_pipeline()
                                else:
                                    data_logger.info(f"SET_H264_STREAMING_MODE: Value {new_streaming_mode} is already set.")
                            else:
                                data_logger.warning(f"SET_H264_STREAMING_MODE received but current encoder '{current_enc}' is not a Pixelflux H.264 encoder.")
                        except IndexError:
                            data_logger.warning(f"Malformed SET_H264_STREAMING_MODE message: {message}")

                    elif message.startswith("SET_JPEG_QUALITY,"):
                        await self.client_settings_received.wait()
                        try:
                            new_quality = int(message.split(",")[1])
                            data_logger.info(f"Received SET_JPEG_QUALITY: {new_quality}")

                            current_enc = getattr(self.app, "encoder", None)
                            if current_enc == "jpeg":
                                if self.jpeg_quality != new_quality:
                                    self.jpeg_quality = new_quality
                                    if self.is_jpeg_capturing:
                                        data_logger.info(f"Restarting jpeg pipeline for JPEG_QUALITY change to {self.jpeg_quality}")
                                        await self._stop_jpeg_pipeline()
                                        await self._start_jpeg_pipeline()
                                else:
                                    data_logger.info(f"SET_JPEG_QUALITY: Value {new_quality} is already set.")
                            else:
                                data_logger.warning(f"SET_JPEG_QUALITY received but current encoder is '{current_enc}', not 'jpeg'.")
                        except (IndexError, ValueError):
                            data_logger.warning(f"Malformed SET_JPEG_QUALITY message: {message}")

                    elif message.startswith("SET_PAINT_OVER_JPEG_QUALITY,"):
                        await self.client_settings_received.wait()
                        try:
                            new_quality = int(message.split(",")[1])
                            data_logger.info(f"Received SET_PAINT_OVER_JPEG_QUALITY: {new_quality}")

                            current_enc = getattr(self.app, "encoder", None)
                            if current_enc == "jpeg":
                                if self.paint_over_jpeg_quality != new_quality:
                                    self.paint_over_jpeg_quality = new_quality
                                    if self.is_jpeg_capturing:
                                        data_logger.info(f"Restarting jpeg pipeline for PAINT_OVER_JPEG_QUALITY change to {self.paint_over_jpeg_quality}")
                                        await self._stop_jpeg_pipeline()
                                        await self._start_jpeg_pipeline()
                                else:
                                    data_logger.info(f"SET_PAINT_OVER_JPEG_QUALITY: Value {new_quality} is already set.")
                            else:
                                data_logger.warning(f"SET_PAINT_OVER_JPEG_QUALITY received but current encoder is '{current_enc}', not 'jpeg'.")
                        except (IndexError, ValueError):
                            data_logger.warning(f"Malformed SET_PAINT_OVER_JPEG_QUALITY message: {message}")

                    elif message.startswith("SET_USE_PAINT_OVER_QUALITY,"):
                        await self.client_settings_received.wait()
                        try:
                            new_val_str = message.split(",")[1].strip().lower()
                            new_val = new_val_str == "true"
                            data_logger.info(f"Received SET_USE_PAINT_OVER_QUALITY: {new_val}")
                            
                            if self.use_paint_over_quality != new_val:
                                self.use_paint_over_quality = new_val
                                current_enc = getattr(self.app, "encoder", None)
                                is_pixelflux_h264_active = (current_enc in PIXELFLUX_VIDEO_ENCODERS and current_enc != "jpeg" and self.is_x264_striped_capturing)
                                is_jpeg_active = (current_enc == "jpeg" and self.is_jpeg_capturing)
                                
                                if is_jpeg_active or is_pixelflux_h264_active:
                                    data_logger.info(f"Restarting {current_enc} pipeline for USE_PAINT_OVER_QUALITY change to {self.use_paint_over_quality}")
                                    if is_jpeg_active:
                                        await self._stop_jpeg_pipeline()
                                        await self._start_jpeg_pipeline()
                                    if is_pixelflux_h264_active:
                                        await self._stop_x264_striped_pipeline()
                                        await self._start_x264_striped_pipeline()
                            else:
                                data_logger.info(f"SET_USE_PAINT_OVER_QUALITY: Value {new_val} is already set.")
                        except IndexError:
                            data_logger.warning(f"Malformed SET_USE_PAINT_OVER_QUALITY message: {message}")

                    elif message.startswith("SET_H264_PAINTOVER_CRF,"):
                        await self.client_settings_received.wait()
                        try:
                            new_crf = int(message.split(",")[1])
                            data_logger.info(f"Received SET_H264_PAINTOVER_CRF: {new_crf}")
                            current_enc = getattr(self.app, "encoder", None)
                            is_pixelflux_h264 = (current_enc in PIXELFLUX_VIDEO_ENCODERS and current_enc != "jpeg")
                            if is_pixelflux_h264:
                                if self.h264_paintover_crf != new_crf:
                                    self.h264_paintover_crf = new_crf
                                    if self.is_x264_striped_capturing:
                                        data_logger.info(f"Restarting {current_enc} pipeline for H264_PAINTOVER_CRF change to {self.h264_paintover_crf}")
                                        await self._stop_x264_striped_pipeline()
                                        await self._start_x264_striped_pipeline()
                                else:
                                    data_logger.info(f"SET_H264_PAINTOVER_CRF: Value {new_crf} is already set.")
                            else:
                                data_logger.warning(f"SET_H264_PAINTOVER_CRF received but current encoder '{current_enc}' is not a Pixelflux H.264 encoder.")
                        except (IndexError, ValueError):
                            data_logger.warning(f"Malformed SET_H264_PAINTOVER_CRF message: {message}")

                    elif message.startswith("SET_H264_PAINTOVER_BURST_FRAMES,"):
                        await self.client_settings_received.wait()
                        try:
                            new_burst = int(message.split(",")[1])
                            data_logger.info(f"Received SET_H264_PAINTOVER_BURST_FRAMES: {new_burst}")
                            current_enc = getattr(self.app, "encoder", None)
                            is_pixelflux_h264 = (current_enc in PIXELFLUX_VIDEO_ENCODERS and current_enc != "jpeg")
                            if is_pixelflux_h264:
                                if self.h264_paintover_burst_frames != new_burst:
                                    self.h264_paintover_burst_frames = new_burst
                                    if self.is_x264_striped_capturing:
                                        data_logger.info(f"Restarting {current_enc} pipeline for H264_PAINTOVER_BURST_FRAMES change to {self.h264_paintover_burst_frames}")
                                        await self._stop_x264_striped_pipeline()
                                        await self._start_x264_striped_pipeline()
                                else:
                                    data_logger.info(f"SET_H264_PAINTOVER_BURST_FRAMES: Value {new_burst} is already set.")
                            else:
                                data_logger.warning(f"SET_H264_PAINTOVER_BURST_FRAMES received but current encoder '{current_enc}' is not a Pixelflux H.264 encoder.")
                        except (IndexError, ValueError):
                            data_logger.warning(f"Malformed SET_H264_PAINTOVER_BURST_FRAMES message: {message}")

                    elif message.startswith("SET_USE_CPU,"):
                        await self.client_settings_received.wait()
                        try:
                            new_use_cpu_str = message.split(",")[1].strip().lower()
                            new_use_cpu = new_use_cpu_str == "true"
                            data_logger.info(f"Received SET_USE_CPU: {new_use_cpu}")

                            current_enc = getattr(self.app, "encoder", None)
                            is_pixelflux_h264 = (current_enc in PIXELFLUX_VIDEO_ENCODERS and current_enc != "jpeg")

                            if is_pixelflux_h264:
                                if self.use_cpu != new_use_cpu:
                                    self.use_cpu = new_use_cpu
                                    if self.is_x264_striped_capturing:
                                        data_logger.info(f"Restarting {current_enc} pipeline for USE_CPU change to {self.use_cpu}")
                                        await self._stop_x264_striped_pipeline()
                                        await self._start_x264_striped_pipeline()
                                else:
                                    data_logger.info(f"SET_USE_CPU: Value {new_use_cpu} is already set.")
                            else:
                                data_logger.warning(f"SET_USE_CPU received but current encoder '{current_enc}' is not a Pixelflux H.264 encoder.")
                        except IndexError:
                            data_logger.warning(f"Malformed SET_USE_CPU message: {message}")

                    elif message.startswith("SET_NATIVE_CURSOR_RENDERING,"):
                        await self.client_settings_received.wait()
                        try:
                            new_capture_cursor_str = message.split(",")[1].strip().lower()
                            new_capture_cursor = new_capture_cursor_str in ("1", "true")
                            data_logger.info(f"Received SET_NATIVE_CURSOR_RENDERING: {new_capture_cursor}")

                            if self.capture_cursor != new_capture_cursor:
                                self.capture_cursor = new_capture_cursor

                                current_enc = getattr(self.app, "encoder", None)
                                is_pixelflux_h264_active = (current_enc in PIXELFLUX_VIDEO_ENCODERS and current_enc != "jpeg" and self.is_x264_striped_capturing)
                                is_jpeg_active = (current_enc == "jpeg" and self.is_jpeg_capturing)

                                if is_jpeg_active or is_pixelflux_h264_active:
                                    data_logger.info(f"Restarting {current_enc} pipeline for cursor rendering change to {self.capture_cursor}")
                                    if is_jpeg_active:
                                        await self._stop_jpeg_pipeline()
                                        await self._start_jpeg_pipeline()
                                    if is_pixelflux_h264_active:
                                        await self._stop_x264_striped_pipeline()
                                        await self._start_x264_striped_pipeline()
                            else:
                                data_logger.info(f"SET_NATIVE_CURSOR_RENDERING: Value {new_capture_cursor} is already set.")
                        except (IndexError, ValueError) as e:
                            data_logger.warning(f"Malformed SET_NATIVE_CURSOR_RENDERING message: {message}, error: {e}")

                    elif message.startswith("s,"):
                        await self.client_settings_received.wait() # Ensure initial settings are processed if needed
                        try:
                            dpi_value_str = message.split(",")[1]
                            dpi_value = int(dpi_value_str)
                            data_logger.info(f"Received DPI setting from client: {dpi_value}")

                            if await set_dpi(dpi_value): # set_dpi defined globally
                                data_logger.info(f"Successfully set DPI to {dpi_value}")
                            else:
                                data_logger.error(f"Failed to set DPI to {dpi_value}")

                            if CURSOR_SIZE > 0: # Ensure CURSOR_SIZE is positive
                                calculated_cursor_size = int(round(dpi_value / 96.0 * CURSOR_SIZE))
                                new_cursor_size = max(1, calculated_cursor_size) # Ensure at least 1px

                                data_logger.info(f"Attempting to set cursor size to: {new_cursor_size} (based on DPI {dpi_value})")
                                if await set_cursor_size(new_cursor_size): # set_cursor_size defined globally
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

                    else:
                        if self.input_handler and hasattr(
                            self.input_handler, "on_message"
                        ):
                            await self.input_handler.on_message(message)

        except websockets.exceptions.ConnectionClosedOK:
            data_logger.info(f"Data WS disconnected gracefully from {raddr}")
        except websockets.exceptions.ConnectionClosedError as e:
            data_logger.warning(f"Data WS closed with error from {raddr}: {e}")
        except Exception as e_main_loop:
            data_logger.error(
                f"Error in Data WS handler for {raddr}: {e_main_loop}", exc_info=True
            )
        finally:
            data_logger.info(f"Cleaning up Data WS handler for {raddr}...")

            # 1. Remove current client from the central set
            self.clients.discard(websocket)
            if self.data_ws is websocket:
                self.data_ws = None

            # 2. Cancel tasks created specifically for THIS connection's ws_handler instance
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
                ):  # self.clients already had the current websocket removed at this point
                    data_logger.info(
                        f"Last client ({raddr}) disconnected. Cancelling frame backpressure task."
                    )
                else:
                    data_logger.info(
                        f"Client {raddr} disconnected, but other clients remain. Frame backpressure task continues."
                    )

            # 3. Clean up resources specific to THIS connection's ws_handler instance
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
                    os.remove(_local_active_path)  # os is imported globally
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

            # 4. Decide whether to stop global pipelines based on OTHER clients
            stop_pipelines_flag = False
            if not self.clients:  # No other clients were in the set to begin with
                data_logger.info(
                    f"No other clients in set after {raddr} disconnected. Marking pipelines for stop."
                )
                stop_pipelines_flag = True
            else:  # Other clients *appear* to remain in the set, check their responsiveness
                data_logger.info(
                    f"Client from {raddr} disconnected. Checking responsiveness of remaining {len(self.clients)} client(s)..."
                )
                active_clients_found_after_check = False
                clients_to_remove_as_stale = []

                current_remaining_clients = list(self.clients)  # Snapshot for iteration

                for other_client_ws in current_remaining_clients:
                    try:
                        # Attempt to ping. If this fails, the client is considered unresponsive.
                        pong_waiter = await other_client_ws.ping()
                        await asyncio.wait_for(
                            pong_waiter, timeout=3.0
                        )  # Short timeout for this check
                        data_logger.info(
                            f"  Remaining client {other_client_ws.remote_address} is responsive."
                        )
                        active_clients_found_after_check = True
                    except asyncio.TimeoutError:
                        data_logger.warning(
                            f"  Remaining client {other_client_ws.remote_address} timed out on ping. Marking as stale."
                        )
                        clients_to_remove_as_stale.append(other_client_ws)
                    except (
                        websockets.exceptions.ConnectionClosed,
                        websockets.exceptions.ConnectionClosedError,
                        websockets.exceptions.ConnectionClosedOK,
                    ) as e_conn_closed:
                        data_logger.warning(
                            f"  Remaining client {other_client_ws.remote_address} connection definitively closed during ping: {type(e_conn_closed).__name__}. Marking as stale."
                        )
                        clients_to_remove_as_stale.append(other_client_ws)
                    except Exception as e_ping:  # Catch any other error during ping, e.g., OS errors if socket is truly gone
                        data_logger.error(
                            f"  Error pinging remaining client {other_client_ws.remote_address}: {e_ping}. Marking as stale."
                        )
                        clients_to_remove_as_stale.append(other_client_ws)

                # Remove all identified stale clients from the central set
                if clients_to_remove_as_stale:
                    for stale_ws in clients_to_remove_as_stale:
                        self.clients.discard(stale_ws)
                        # Attempt to close from server-side; websockets library handles if already closed.
                        try:
                            await stale_ws.close(
                                code=1001,
                                reason="Stale client detected on other client disconnect",
                            )
                        except (
                            websockets.exceptions.ConnectionClosed,
                            websockets.exceptions.ConnectionClosedError,
                            websockets.exceptions.ConnectionClosedOK,
                        ):
                            pass  # Already closed or closing
                        except Exception as e_close_stale:
                            data_logger.debug(
                                f"Minor error closing stale client {stale_ws.remote_address}: {e_close_stale}"
                            )  # Best effort

                # Now, re-evaluate if any truly active clients are left OR if self.clients is now empty
                if not self.clients:  # All "other" clients were stale and removed
                    data_logger.info(
                        f"All other clients were stale or disconnected. Marking pipelines for stop after {raddr} disconnect."
                    )
                    stop_pipelines_flag = True
                elif (
                    not active_clients_found_after_check
                ):  # No responsive clients were found among the remaining
                    data_logger.info(
                        f"No responsive clients remain after check for {raddr}'s disconnect. Marking pipelines for stop."
                    )
                    stop_pipelines_flag = True
                else:
                    data_logger.info(
                        f"Client from {raddr} disconnected. Responsive clients ({len(self.clients)}) remain. Global pipelines will NOT be stopped by this handler."
                    )

            # 5. Stop global pipelines if the flag is set
            if stop_pipelines_flag:
                data_logger.info(f"Stopping global pipelines due to last client disconnect ({raddr}).")
                self.capture_cursor = False
                async with self._pipeline_lock:  # WRAP HERE
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
            latency_ms = server_instance._smoothed_rtt_ms
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


async def on_resize_handler(res_str, current_app_instance, data_server_instance=None):
    """
    Handles client resize request. Updates app state and calls xrandr.
    """
    logger_gst_app_resize.info(f"on_resize_handler attempting resize for: {res_str}")
    try:
        w_str, h_str = res_str.split("x")
        target_w, target_h = int(w_str), int(h_str)

        # Ensure dimensions are positive
        if target_w <= 0 or target_h <= 0:
            logger_gst_app_resize.error(
                f"Invalid target dimensions in resize request: {target_w}x{target_h}. Ignoring."
            )
            if current_app_instance:
                current_app_instance.last_resize_success = False
            return  # Do not proceed with invalid dimensions

        # Ensure dimensions are even
        if target_w % 2 != 0:
            logger_gst_app_resize.debug(
                f"Adjusting odd width {target_w} to {target_w - 1}"
            )
            target_w -= 1
        if target_h % 2 != 0:
            logger_gst_app_resize.debug(
                f"Adjusting odd height {target_h} to {target_h - 1}"
            )
            target_h -= 1

        # Re-check positivity after odd adjustment
        if target_w <= 0 or target_h <= 0:
            logger_gst_app_resize.error(
                f"Dimensions became invalid ({target_w}x{target_h}) after odd adjustment. Ignoring."
            )
            if current_app_instance:
                current_app_instance.last_resize_success = False
            return  # Do not proceed

        current_app_instance.display_width = target_w
        current_app_instance.display_height = target_h
        logger_gst_app_resize.info(
            f"App dimensions updated to {target_w}x{target_h} before xrandr call."
        )

        success = await resize_display(f"{target_w}x{target_h}")

        if success:
            logger_gst_app_resize.info(
                f"resize_display('{target_w}x{target_h}') reported success."
            )
            current_app_instance.last_resize_success = True
            if data_server_instance:
                asyncio.create_task(data_server_instance.broadcast_stream_resolution())
        else:
            logger_gst_app_resize.error(
                f"resize_display('{target_w}x{target_h}') reported failure."
            )
            current_app_instance.last_resize_success = False

    except ValueError:
        logger_gst_app_resize.error(
            f"Invalid resolution format in resize request: {res_str}"
        )
        current_app_instance.last_resize_success = False
    except Exception as e:
        logger_gst_app_resize.error(
            f"Error during resize handling for '{res_str}': {e}", exc_info=True
        )
        if current_app_instance:
            current_app_instance.last_resize_success = False

async def main():
    if "DEV_MODE" in os.environ:
        try:
            dev_version_file = pathlib.Path(
                "../../addons/gst-web-core/selkies-version.txt"
            )
            if dev_version_file.parent.exists():
                dev_version_file.touch(exist_ok=True)
        except OSError:
            pass

    parser = argparse.ArgumentParser(description="Selkies WebSocket Streaming Server")
    parser.add_argument(
        "--encoder",
        default=os.environ.get("SELKIES_ENCODER", "x264enc"),
        help="Video encoder (e.g., x264enc, jpeg, x264enc-striped)",
    )
    parser.add_argument(
        "--framerate",
        default=os.environ.get("SELKIES_FRAMERATE", "60"),
        type=int,
        help="Target framerate",
    )
    parser.add_argument(
        "--video_bitrate",
        default=os.environ.get("SELKIES_VIDEO_BITRATE", "16000"),
        type=int,
        help="Target video bitrate in kbps",
    )
    parser.add_argument(
        "--dri_node",
        default=os.environ.get("DRI_NODE", ""),
        type=str,
        help="Path to the DRI render node (e.g., /dev/dri/renderD128) for VA-API.",
    )
    parser.add_argument(
        "--audio_device_name",
        default=os.environ.get("SELKIES_AUDIO_DEVICE", "output.monitor"),
        help="Audio device name for pcmflux (e.g., a PulseAudio .monitor source). Defaults to output.monitor.",
    )
    parser.add_argument(
        "--h264_crf",
        default=os.environ.get("SELKIES_H264_CRF", "25"),
        type=int,
        help="H.264 CRF for x264enc-striped (0-51)",
    )
    parser.add_argument(
        "--h264_fullcolor",
        default=os.environ.get("SELKIES_H264_FULLCOLOR", "False").lower() == "true",
        type=lambda x: (str(x).lower() == 'true'),
        help="Enable H.264 full color range for x264enc-striped (default: False)",
    )
    parser.add_argument(
        "--h264_streaming_mode",
        default=os.environ.get("SELKIES_H264_STREAMING_MODE", "False").lower() == "true",
        type=lambda x: (str(x).lower() == 'true'),
        help="Enable H.264 streaming mode for pixelflux encoders (default: False).",
    )
    parser.add_argument(
        "--watermark_path",
        default=os.environ.get("WATERMARK_PNG", ""),
        type=str,
        help="Absolute path to the watermark PNG file for pixelflux.",
    )
    parser.add_argument(
        "--watermark_location",
        default=os.environ.get("WATERMARK_LOCATION", "-1"),
        type=int,
        help="Watermark location enum (0-6). Defaults to 4 (Bottom Right) if path is set and this is not specified or invalid.",
    )
    parser.add_argument(
        "--port",
        default=os.environ.get("CUSTOM_WS_PORT", "8082"),
        type=int,
        help="The port for the data websocket server. Overrides the CUSTOM_WS_PORT environment variable.",
    )
    parser.add_argument("--debug", action="store_true", help="Enable debug logging")
    args, unknown = parser.parse_known_args()
    global TARGET_FRAMERATE, TARGET_VIDEO_BITRATE_KBPS
    TARGET_FRAMERATE = args.framerate
    TARGET_VIDEO_BITRATE_KBPS = args.video_bitrate
    initial_encoder = args.encoder.lower()

    if args.debug:
        logging.getLogger().setLevel(logging.DEBUG)
    else:
        logging.getLogger().setLevel(logging.INFO)
        logging.getLogger("websockets").setLevel(logging.WARNING)
    if not args.debug and PULSEAUDIO_AVAILABLE:
        logging.getLogger("pulsectl").setLevel(logging.WARNING)

    logger.info(f"Starting Selkies (WebSocket Mode) with args: {args}")
    logger.info(
        f"Initial Encoder: {initial_encoder}, Framerate: {TARGET_FRAMERATE}, Bitrate: {TARGET_VIDEO_BITRATE_KBPS}kbps"
    )

    event_loop = asyncio.get_running_loop()

    app = SelkiesStreamingApp(
        event_loop,
        framerate=TARGET_FRAMERATE,
        encoder=initial_encoder,
        video_bitrate=TARGET_VIDEO_BITRATE_KBPS,
        mode="websockets",
    )
    app.server_enable_resize = ENABLE_RESIZE
    app.last_resize_success = True
    logger.info(
        f"SelkiesStreamingApp initialized: encoder={app.encoder}, display={app.display_width}x{app.display_height}"
    )

    data_server = DataStreamingServer(
        port=args.port,
        app=app,
        uinput_mouse_socket=UINPUT_MOUSE_SOCKET,
        js_socket_path=JS_SOCKET_PATH,
        enable_clipboard=ENABLE_CLIPBOARD,
        enable_cursors=ENABLE_CURSORS,
        cursor_size=CURSOR_SIZE,
        cursor_scale=1.0,
        cursor_debug=DEBUG_CURSORS,
        audio_device_name=args.audio_device_name,
        cli_args=args,
    )
    app.data_streaming_server = data_server

    input_handler = InputHandler(
        app,
        UINPUT_MOUSE_SOCKET,
        JS_SOCKET_PATH,
        str(ENABLE_CLIPBOARD).lower(),
        ENABLE_CURSORS,
        CURSOR_SIZE,
        1.0,
        DEBUG_CURSORS,
    )
    data_server.input_handler = (
        input_handler 
    )

    input_handler.on_clipboard_read = app.send_ws_clipboard_data

    input_handler.on_set_fps = app.set_framerate
    if ENABLE_RESIZE:
        input_handler.on_resize = lambda res_str: on_resize_handler(
            res_str, app, data_server
        )
    else:
        input_handler.on_resize = lambda res_str: logger.warning("Resize disabled.")
        input_handler.on_scaling_ratio = lambda scale_val: logger.warning(
            "Scaling disabled."
        )

    tasks_to_run = []
    data_server_task = asyncio.create_task(data_server.run_server(), name="DataServer")
    tasks_to_run.append(data_server_task)

    if hasattr(input_handler, "connect"):  # This refers to the global input_handler
        tasks_to_run.append(
            asyncio.create_task(input_handler.connect(), name="InputConnect")
        )
    if hasattr(input_handler, "start_clipboard"):
        tasks_to_run.append(
            asyncio.create_task(input_handler.start_clipboard(), name="ClipboardMon")
        )
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

        if input_handler:  # This is the global input_handler instance
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


def ws_entrypoint():
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        logger.info("Application stopped by KeyboardInterrupt.")
    except SystemExit as e:
        logger.info(f"Application exited with code {e.code}.")
    except Exception:
        logger.critical("Unhandled exception at entrypoint:", exc_info=True)
    finally:
        logging.shutdown()
