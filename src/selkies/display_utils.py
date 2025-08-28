import re
import os
from asyncio import subprocess
from shutil import which

import logging
LOGLEVEL = logging.INFO
logging.basicConfig(level=LOGLEVEL)
logger_gst_app_resize = logging.getLogger("resize")

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

async def _run_xrdb(dpi_value, logger):
    """Helper function to apply DPI via xrdb."""
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
        if process.returncode == 0:
            logger.info(f"Successfully loaded {xresources_path_str} using xrdb.")
            return True
        else:
            logger.warning(f"Failed to load {xresources_path_str} using xrdb. RC: {process.returncode}, Error: {stderr.decode().strip()}")
            return False
    except Exception as e:
        logger.error(f"Error updating or loading Xresources: {e}")
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
