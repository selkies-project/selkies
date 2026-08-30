# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Display plumbing shared by the WebSocket and WebRTC transports.

Covers X11 RandR display management (resolution modes, extended-desktop
logical monitors, framebuffer sizing), per-desktop-environment DPI
application, Wayland output-id mapping, pixelflux CaptureSettings
population, and cursor payload/cache-handle helpers.

Every RandR operation runs natively on a retained python-xlib connection
first and degrades to an xrandr/cvt/gtf subprocess only when the native
call fails. Blocking X work runs on executor threads (``asyncio.to_thread``)
under ``_x11_lock`` so the event loop never waits on the X server. A helper
drops the cached connection on any failure other than an X protocol error:
an ``XError`` leaves the connection healthy, while a broken connection also
frees this session's RandR modes. RandR request failures arrive through the
connection's asynchronous error handler — printed, never raised — so every
mutation reads its result back before claiming success.

DPI handling here is X11-only by design: on the Wayland backend a DPI is an
output scale on the session compositor (applied in-process through
wlr-output-management), never Xft resources — XWayland runs in the
compositor's logical space and is scaled with it, so Xft resources merged
there would scale applications twice.
"""

import base64
import io
import re
import os
import signal
import stat
import struct
import sys
import tempfile
import zlib
from asyncio import subprocess
import asyncio
import threading
from shutil import which
from typing import Any, Dict, List, NamedTuple, Optional, Tuple, Union

from PIL import Image, ImageMath

from .Xlib import X as x11_X
from .Xlib import display as x11_display
from .Xlib import error as x11_error
from .Xlib.ext import randr
from .Xlib.protocol import request as x11_request

import logging

logger_app_resize = logging.getLogger("resize")
logger_app_resize.setLevel(logging.INFO)

def fit_res(w: int, h: int, max_w: int, max_h: int) -> Tuple[int, int]:
    """Fit WxH inside the given bounds, preserving aspect, rounded down to even."""
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


async def _communicate_or_kill(
    process: subprocess.Process, timeout: float = 5.0
) -> Tuple[bytes, bytes]:
    """Run ``process.communicate()`` bounded to ``timeout`` seconds.

    On expiry the process is killed and reaped, and empty stdout plus a
    timeout message are returned so callers observe the nonzero returncode
    instead of hanging.

    Returns:
        The ``(stdout, stderr)`` bytes pair.
    """
    try:
        return await asyncio.wait_for(process.communicate(), timeout)
    except asyncio.TimeoutError:
        try:
            process.kill()
        except ProcessLookupError:
            pass
        await process.wait()
        return b"", f"timed out after {timeout:g}s".encode()


def _cvt_rb_mode_info(width: int, height: int, refresh: float = 60.0) -> Dict[str, int]:
    """VESA CVT 1.2 reduced-blanking timings for WxH at ``refresh``.

    Mirrors ``cvt -r``: the width rounds up to the 8-pixel CVT cell while the
    vertical stays exact.

    Returns:
        The RandR ``_ModeInfo`` fields minus ``id``/``name_length``.
    """
    h_active = -(-width // 8) * 8
    v_active = height
    if v_active % 3 == 0 and v_active * 4 // 3 == h_active:
        v_sync = 4
    elif v_active % 9 == 0 and v_active * 16 // 9 == h_active:
        v_sync = 5
    elif v_active % 10 == 0 and v_active * 16 // 10 == h_active:
        v_sync = 6
    elif v_active % 4 == 0 and v_active * 5 // 4 == h_active:
        v_sync = 7
    elif v_active % 9 == 0 and v_active * 15 // 9 == h_active:
        v_sync = 7
    else:
        v_sync = 10
    h_period_est = (1_000_000.0 / refresh - 460.0) / v_active
    vbi_lines = max(int(460.0 / h_period_est) + 1, 3 + v_sync + 6)
    v_total = v_active + vbi_lines
    h_total = h_active + 160
    clock_khz = h_total * 1_000.0 / h_period_est
    clock_khz -= clock_khz % 250.0
    return {
        "width": h_active,
        "height": v_active,
        "dot_clock": int(round(clock_khz)) * 1_000,
        "h_sync_start": h_active + 48,
        "h_sync_end": h_active + 80,
        "h_total": h_total,
        "h_skew": 0,
        "v_sync_start": v_active + 3,
        "v_sync_end": v_active + 3 + v_sync,
        "v_total": v_total,
        "flags": randr.HSyncPositive | randr.VSyncNegative,
    }


_x11_lock = threading.Lock()
_x11_conn: Optional[x11_display.Display] = None


def _module_display() -> x11_display.Display:
    """This module's cached X connection; call under ``_x11_lock``.

    RandR user modes are owned by the connection that created them and die
    with it (the screen falls back to a built-in mode), so the connection
    stays open for the process lifetime and retains its resources past
    disconnect. Retention is temporary: a retained client record holds one of
    the server's client slots until something issues KillClient(AllTemporary),
    and a server whose selkies is restarted many times would otherwise run out
    of slots. So the first connection of a process reaps the records its
    predecessors left, and when that takes the live mode with it (the screen
    reverts), it recreates that mode at once — a brief revert on a restart,
    no slot ever held by a finished process.

    The connection carries a blocking timeout: an alive-but-unresponsive X
    server (driver hang, a foreign client's server grab) would otherwise block
    these helpers forever — in the handshake, then in any reply wait — while
    they hold ``_x11_lock``, and every retry would park another executor
    thread behind it. The bound makes both raise ``ConnectionClosedError``,
    so the helpers drop this connection and fall back to their subprocess
    paths.
    """
    global _x11_conn
    if _x11_conn is None:
        conn = x11_display.Display(blocking_timeout=15.0)
        conn.set_close_down_mode(x11_X.RetainTemporary)
        conn.sync()
        _reap_retained_predecessors(conn)
        _x11_conn = conn
    return _x11_conn


def _reap_retained_predecessors(conn: x11_display.Display) -> None:
    """Release the temporarily retained resources of earlier processes and
    restore the screen size if the reap reverted it."""
    try:
        geom = conn.screen().root.get_geometry()
        before = (int(geom.width), int(geom.height))
        x11_request.KillClient(display=conn.display, resource=x11_X.AllTemporary)
        conn.sync()
        geom = conn.screen().root.get_geometry()
        after = (int(geom.width), int(geom.height))
    except Exception as e:
        logger_app_resize.debug(f"Retained X client reap skipped: {e}")
        return
    if after == before:
        return
    try:
        realized = _resize_on_display(conn, f"{before[0]}x{before[1]}", before[0], before[1])
        logger_app_resize.info(
            f"Restored the {before[0]}x{before[1]} screen mode a finished process had "
            f"left retained (realized {realized[0]}x{realized[1]}).")
    except Exception as e:
        logger_app_resize.warning(
            f"Screen reverted to {after[0]}x{after[1]} while reaping retained X clients "
            f"and could not be restored: {e}")


def _drop_module_display() -> None:
    """Close and forget the cached connection so the next call reconnects."""
    global _x11_conn
    if _x11_conn is not None:
        try:
            _x11_conn.close()
        except Exception:
            pass
        _x11_conn = None


def _connected_output_state(
    d: x11_display.Display,
) -> Tuple[Any, Any, int, Any, Dict[int, str]]:
    """Locate the first connected RandR output on connection ``d``.

    Returns:
        ``(root, resources, output_id, output_info, id_to_name)`` where
        ``id_to_name`` maps each mode id to its mode name.

    Raises:
        RuntimeError: If no RandR output is connected.
    """
    root = d.screen().root
    res = randr.get_screen_resources(root)
    mode_names = res.mode_names
    if isinstance(mode_names, bytes):
        mode_names = mode_names.decode("latin-1")
    names = {}
    pos = 0
    for m in res.modes:
        names[m.id] = mode_names[pos:pos + m.name_length]
        pos += m.name_length
    for out_id in res.outputs:
        oi = randr.get_output_info(d, out_id, res.config_timestamp)
        if oi.connection == randr.Connected:
            return root, res, out_id, oi, names
    raise RuntimeError("no connected RandR output")


def _sync_query_randr() -> Tuple[str, List[str], str]:
    """Blocking RandR query on the module connection.

    Returns:
        ``(current "WxH" screen size, sorted "WxH" mode names of the first
        connected output, output name)``.
    """
    with _x11_lock:
        try:
            d = _module_display()
            root, _, _, oi, names = _connected_output_state(d)
            geom = root.get_geometry()
            curr_res = f"{geom.width}x{geom.height}"
            wh_pat = re.compile(r"\d+x\d+")
            resolutions = sorted(
                {names[m] for m in oi.modes if m in names and wh_pat.fullmatch(names[m])}
            )
            name = oi.name
            screen_name = name.decode("latin-1") if isinstance(name, bytes) else str(name)
            return curr_res, resolutions, screen_name
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _ensure_mode_on_display(
    d: x11_display.Display,
    root: Any,
    res: Any,
    oi: Any,
    out_id: int,
    names: Dict[int, str],
    res_str: str,
    w_req: int,
    h_req: int,
) -> Tuple[int, int, int]:
    """Resolve or create the mode named ``res_str`` on output ``out_id``.

    Creates the mode from CVT-RB timings and attaches it to the output when
    absent. Modes are owned by the creating connection, so this must run on
    the retained module connection for the mode to outlive the call.

    Returns:
        ``(mode_id, width, height)`` of the resolved mode.
    """
    mode_id = next((m for m in oi.modes if names.get(m) == res_str), None)
    if mode_id is not None:
        w, h = next((m.width, m.height) for m in res.modes if m.id == mode_id)
        return mode_id, w, h
    mode_id = next((mid for mid, n in names.items() if n == res_str), None)
    if mode_id is None:
        info = _cvt_rb_mode_info(w_req, h_req)
        info["id"] = 0
        info["name_length"] = len(res_str)
        mode_id = randr.create_mode(root, info, res_str).mode
        randr.add_output_mode(d, out_id, mode_id)
        return mode_id, info["width"], info["height"]
    randr.add_output_mode(d, out_id, mode_id)
    w, h = next((m.width, m.height) for m in res.modes if m.id == mode_id)
    return mode_id, w, h


def _sync_ensure_mode(res_str: str) -> None:
    """Blocking ensure-mode on the module connection (no CRTC/screen change).

    Raises:
        ValueError: If ``res_str`` is not a positive "WxH".
        RuntimeError: If the server silently refused to attach the mode.
    """
    w_req, h_req = (int(p) for p in res_str.split("x"))
    if w_req <= 0 or h_req <= 0:
        raise ValueError(f"invalid resolution '{res_str}'")
    with _x11_lock:
        try:
            d = _module_display()
            root, res, out_id, oi, names = _connected_output_state(d)
            mode_id, _, _ = _ensure_mode_on_display(
                d, root, res, oi, out_id, names, res_str, w_req, h_req
            )
            d.sync()
            _, _, _, oi, _ = _connected_output_state(d)
            if mode_id not in oi.modes:
                raise RuntimeError(f"mode '{res_str}' did not attach (server refused it)")
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


async def ensure_mode(res_str: str) -> bool:
    """Ensure a RandR mode named ``res_str`` is attached to the connected output.

    Later xrandr calls can then reference the mode by name.

    Returns:
        True on success; False leaves the caller to its subprocess fallback.
    """
    try:
        await asyncio.to_thread(_sync_ensure_mode, res_str)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native RandR ensure-mode for '{res_str}' failed ({e}).")
        return False


def _sync_resize_randr(res_str: str) -> Tuple[int, int]:
    """Blocking RandR resize on the module connection.

    Ensures a mode named ``res_str`` exists on the first connected output
    (creating CVT-RB timings when absent), activates it, and sizes the screen
    to match. Raises on any failure so the caller can fall back to xrandr.

    Returns:
        The ``(width, height)`` actually applied.
    """
    w_req, h_req = (int(p) for p in res_str.split("x"))
    if w_req <= 0 or h_req <= 0:
        raise ValueError(f"invalid resolution '{res_str}'")
    with _x11_lock:
        try:
            return _resize_on_display(_module_display(), res_str, w_req, h_req)
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _resize_on_display(
    d: x11_display.Display, res_str: str, w_req: int, h_req: int
) -> Tuple[int, int]:
    """The RandR mode-create/activate/screen-size sequence on connection ``d``.

    CVT-RB snaps the width up to its 8-pixel cell, so the realized mode can be
    wider than requested; the mode is named for its real geometry because a
    name that disagrees with the pixel size breaks later xrandr calls that
    derive framebuffer dimensions from it. The physical size follows the DPI
    the last ``set_dpi`` stamped (96 when never retargeted): xdpyinfo and the
    toolkit paths reading RandR's physical size would otherwise un-scale after
    every resize. The screen may not shrink under an active CRTC, so a CRTC
    that would poke out of the new screen is disabled first, as xrandr does.
    """
    root, res, out_id, oi, names = _connected_output_state(d)
    mode_name = f"{-(-w_req // 8) * 8}x{h_req}"
    mode_id, mode_w, mode_h = _ensure_mode_on_display(
        d, root, res, oi, out_id, names, mode_name, w_req, h_req
    )
    crtc = oi.crtc or (oi.crtcs[0] if oi.crtcs else 0)
    if not crtc:
        raise RuntimeError("output has no usable CRTC")
    ci = randr.get_crtc_info(d, crtc, res.config_timestamp)
    outputs = list(ci.outputs) or [out_id]
    geom = root.get_geometry()
    dpi_hint = _APPLIED_DPI if _APPLIED_DPI is not None else 96
    mm_w = max(1, round(mode_w * 25.4 / dpi_hint))
    mm_h = max(1, round(mode_h * 25.4 / dpi_hint))
    rotation = ci.rotation or randr.Rotate_0
    crtc_fits = ci.x + ci.width <= mode_w and ci.y + ci.height <= mode_h
    d.grab_server()
    try:
        if ci.mode and not crtc_fits:
            status = randr.set_crtc_config(
                d, crtc, res.config_timestamp, ci.x, ci.y, 0, rotation, [],
            ).status
            if status != randr.SetConfigSuccess:
                raise RuntimeError(f"CRTC disable returned status {status}")
        if (geom.width, geom.height) != (mode_w, mode_h):
            randr.set_screen_size(root, mode_w, mode_h, mm_w, mm_h)
        status = randr.set_crtc_config(
            d, crtc, res.config_timestamp, ci.x, ci.y, mode_id,
            rotation, outputs,
        ).status
        if status != randr.SetConfigSuccess:
            raise RuntimeError(f"SetCrtcConfig returned status {status}")
    finally:
        # Flushed, not just queued: an X error aborting the sequence would leave
        # an unsent ungrab and every other X client wedged until this process exits.
        try:
            d.ungrab_server()
            d.flush()
        except Exception:
            pass
    d.sync()
    geom = root.get_geometry()
    if (geom.width, geom.height) != (mode_w, mode_h):
        raise RuntimeError(
            f"screen is {geom.width}x{geom.height} after applying '{res_str}'"
        )
    return mode_w, mode_h


#: The compositor output every display is cut from. The session sees this one
#: screen however many displays the client is shown, which is what lets a window
#: be dragged from one to the next: the pointer grab a drag runs under stays
#: inside one surface for the whole desk.
WAYLAND_SCREEN_OUTPUT_ID = 0


def wayland_output_id(display_id: Optional[str]) -> int:
    """Stable compositor id for a display name, shared by both transports.

    'primary' maps to 1 and 'displayN' to N, leaving 0 to the screen they are
    all views of. A secondary name without a numeric suffix falls back to 2,
    and so does one whose digits would land on the screen or the primary's
    view ('display0', 'display1'): a client-chosen name must never address
    the session's own nodes.
    """
    if not display_id or display_id == "primary":
        return 1
    m = re.search(r"(\d+)$", str(display_id))
    n = int(m.group(1)) if m else 2
    return n if n >= 2 else 2


def compute_dual_layout(
    primary_wh: Tuple[int, int],
    secondary_wh: Tuple[int, int],
    position: str,
) -> Tuple[Dict[str, Dict[str, int]], int, int]:
    """Extended-desktop layout for a primary display plus one secondary.

    The secondary is placed at ``position`` ("right"/"left"/"up"/"down") —
    the same placement model the websockets transport uses, so a display
    looks identical over either transport.

    Returns:
        ``(layouts, total_w, total_h)`` where ``layouts`` maps "primary" and
        "secondary" to `{x, y, w, h}` rectangles (the secondary's real id is
        filled in by the caller) and the total width is rounded up to a
        multiple of 8 (xrandr framebuffer alignment).
    """
    p_w, p_h = primary_wh
    s_w, s_h = secondary_wh
    if position == "left":
        layouts = {"secondary": {"x": 0, "y": 0, "w": s_w, "h": s_h},
                   "primary": {"x": s_w, "y": 0, "w": p_w, "h": p_h}}
        total_w, total_h = p_w + s_w, max(p_h, s_h)
    elif position == "down":
        layouts = {"primary": {"x": 0, "y": 0, "w": p_w, "h": p_h},
                   "secondary": {"x": 0, "y": p_h, "w": s_w, "h": s_h}}
        total_w, total_h = max(p_w, s_w), p_h + s_h
    elif position == "up":
        layouts = {"secondary": {"x": 0, "y": 0, "w": s_w, "h": s_h},
                   "primary": {"x": 0, "y": s_h, "w": p_w, "h": p_h}}
        total_w, total_h = max(p_w, s_w), p_h + s_h
    else:
        layouts = {"primary": {"x": 0, "y": 0, "w": p_w, "h": p_h},
                   "secondary": {"x": p_w, "y": 0, "w": s_w, "h": s_h}}
        total_w, total_h = p_w + s_w, max(p_h, s_h)
    return layouts, (total_w + 7) & ~7, total_h


def layout_extent(layouts: Optional[Dict[str, Dict[str, int]]]) -> Tuple[int, int]:
    """Size of the region the laid-out displays cover together.

    A layout table is normalized to a non-negative origin, so its extent is
    the furthest right and bottom edge over all entries. An entry with a
    missing or null field contributes only what it does carry, and an empty
    table has no extent at all.

    Returns:
        ``(width, height)``; either is 0 when nothing bounds that axis.
    """
    width = 0
    height = 0
    for layout in (layouts or {}).values():
        width = max(width, int(layout.get("x") or 0) + int(layout.get("w") or 0))
        height = max(height, int(layout.get("y") or 0) + int(layout.get("h") or 0))
    return width, height


def clamp_primary_feedback(
    primary_wh: Tuple[int, int],
    layouts: Optional[Dict[str, Dict[str, int]]],
    position: str,
) -> Tuple[int, int]:
    """Guard the extended-desktop layout against auto-resize feedback.

    Shared by both transports: after the extend, a maximized primary client
    reports the FULL extended screen; re-cropping the primary to that would
    span both monitors and grow the framebuffer without bound. When the
    primary's reported size fills the current extended screen (``layouts``)
    along the secondary's axis, keep the established primary-monitor size
    instead.

    Returns:
        The ``(w, h)`` to lay the primary out with.
    """
    prev_primary = layouts.get("primary") if layouts else None
    if not prev_primary:
        return primary_wh
    p_w, p_h = primary_wh
    cur_total_w, cur_total_h = layout_extent(layouts)
    if (position in ("right", "left") and p_w >= cur_total_w) or (
        position in ("up", "down") and p_h >= cur_total_h
    ):
        return prev_primary["w"], prev_primary["h"]
    return primary_wh


def parse_resize_dims(res_str: str) -> Optional[Tuple[int, int]]:
    """Parse a client resize request "WxH", shared by both transports.

    Caps to the 8K ceiling a client may drive the server to, and rounds down
    to even (YUV 4:2:0 chroma alignment).

    Returns:
        ``(w, h)``, or None when malformed or non-positive.
    """
    try:
        w_str, h_str = res_str.split("x")
        w, h = int(w_str), int(h_str)
    except (ValueError, AttributeError):
        return None
    w, h = min(w, 7680) & ~1, min(h, 4320) & ~1
    if w <= 0 or h <= 0:
        return None
    return w, h


def cursor_size_for_dpi(dpi: float, base_size: int) -> int:
    """Cursor pixel size scaled from its 96-DPI base (both transports derive
    the X cursor size from the desktop DPI with this)."""
    return max(1, int(round(float(dpi) / 96.0 * base_size)))


def align_dims_16(w: int, h: int) -> Tuple[int, int]:
    """Round dimensions down to multiples of 16 for force_aligned_resolution.

    Encoder macroblock alignment; refuses to shrink below 16.

    Returns:
        The aligned dimensions, or the originals unchanged when alignment
        would collapse them.
    """
    aligned_w, aligned_h = w - (w % 16), h - (h % 16)
    if aligned_w >= 16 and aligned_h >= 16:
        return aligned_w, aligned_h
    return w, h


async def _run_xrandr(args: List[str], what: str) -> bool:
    """Run one xrandr command, returning success; failures are logged, not raised
    (layout application degrades per step exactly like the websockets engine)."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "xrandr", *args,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await _communicate_or_kill(proc)
        if proc.returncode != 0:
            logger_app_resize.warning(f"xrandr {what} failed: {stderr.decode(errors='replace').strip()}")
            return False
        return True
    except Exception as e:
        logger_app_resize.warning(f"xrandr {what} failed: {e}")
        return False


def _sync_list_monitors() -> List[str]:
    """Blocking RandR 1.5 monitor-name query on the module connection."""
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            reply = randr.get_monitors(root, is_active=False)
            names = []
            for m in reply.monitors:
                try:
                    names.append(d.get_atom_name(m.name))
                except Exception:
                    continue
            return names
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _monitor_info(
    d: x11_display.Display,
    out_id: int,
    name: str,
    x: int,
    y: int,
    w: int,
    h: int,
) -> Dict[str, Any]:
    """Build the RRSetMonitor request dict for logical monitor ``name``.

    Every monitor lists the (single) connected physical output: the server
    does not reserve an output for one monitor, and GTK3's X11 backend
    realizes a GdkMonitor only for RandR monitors that carry a live output —
    an outputless monitor is invisible to GTK apps, whose desktops then
    paint and tile short of that region. The primary monitor carries the
    RandR primary flag: the WM tiles panels against it, and the same-set
    no-op in `_sync_replace_selkies_monitors` reads it back to prove the
    live set already matches.
    """
    return {
        "name": d.intern_atom(name),
        "primary": name == "selkies-primary",
        "automatic": False,
        "x": int(x),
        "y": int(y),
        "width_in_pixels": int(w),
        "height_in_pixels": int(h),
        "width_in_millimeters": max(1, round(w * 25.4 / (_APPLIED_DPI if _APPLIED_DPI is not None else 96.0))),
        "height_in_millimeters": max(1, round(h * 25.4 / (_APPLIED_DPI if _APPLIED_DPI is not None else 96.0))),
        "crtcs": [out_id],
    }


def _verify_monitors_on_display(
    d: x11_display.Display, expected: Dict[str, Tuple[int, int, int, int]]
) -> None:
    """Verify every expected logical monitor is defined with its geometry.

    RRSetMonitor failures (e.g. BadValue for an already-taken name) arrive
    through the async error handler — printed, never raised — so callers must
    verify the result instead of trusting the request.

    Args:
        d: The X connection to query.
        expected: Mapping of monitor name to its ``(x, y, w, h)``.

    Raises:
        RuntimeError: If any expected monitor is missing or mismatched.
    """
    root = d.screen().root
    reply = randr.get_monitors(root, is_active=False)
    actual = {}
    for m in reply.monitors:
        try:
            actual[d.get_atom_name(m.name)] = (m.x, m.y, m.width_in_pixels, m.height_in_pixels)
        except Exception:
            continue
    for name, geom in expected.items():
        if actual.get(name) != geom:
            raise RuntimeError(
                f"monitor '{name}' is {actual.get(name)} after define, wanted {geom}"
            )


def _sync_set_monitor(name: str, x: int, y: int, w: int, h: int) -> None:
    """Blocking RandR 1.5 set-monitor on the module connection."""
    with _x11_lock:
        try:
            d = _module_display()
            root, _, out_id, _, _ = _connected_output_state(d)
            randr.set_monitor(root, _monitor_info(d, out_id, name, x, y, w, h))
            d.sync()
            _verify_monitors_on_display(
                d, {name: (int(x), int(y), int(w), int(h))}
            )
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _sync_delete_monitor(name: str) -> None:
    """Blocking RandR 1.5 delete-monitor on the module connection."""
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            randr.delete_monitor(root, d.intern_atom(name))
            d.sync()
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _sync_set_output_primary() -> None:
    """Blocking RandR set-output-primary (first connected output)."""
    with _x11_lock:
        try:
            d = _module_display()
            root, _, out_id, _, _ = _connected_output_state(d)
            randr.set_output_primary(root, out_id)
            d.sync()
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _sync_grow_screen(w: int, h: int) -> None:
    """Blocking grow-only screen resize (never shrinks; no CRTC change)."""
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            geom = root.get_geometry()
            if geom.width >= w and geom.height >= h:
                return
            dpi_hint = _APPLIED_DPI if _APPLIED_DPI is not None else 96.0
            randr.set_screen_size(
                root, w, h,
                max(1, round(w * 25.4 / dpi_hint)), max(1, round(h * 25.4 / dpi_hint)),
            )
            d.sync()
            geom = root.get_geometry()
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise
        # Outside the try: the connection is healthy, so a short root must not
        # drop the module display.
        if geom.width < w or geom.height < h:
            raise RuntimeError(f"screen is {geom.width}x{geom.height} after grow to {w}x{h}")


def _sync_replace_selkies_monitors(layouts: Dict[str, Dict[str, int]]) -> None:
    """Blocking swap of ALL selkies-* logical monitors to exactly ``layouts``.

    ``layouts`` maps display id to an `{x, y, w, h}` rectangle. The whole
    swap runs under one X server grab: window managers re-read the monitor
    set whenever the root emits a core ConfigureNotify — which both a screen
    resize AND deleting/creating the monitor that holds the physical output
    do (the server swaps an automatic whole-CRTC monitor in and out).
    RRSetMonitor cannot replace an existing name (BadValue), so a delete gap
    is unavoidable; the grab makes the swap invisible: every event the WM
    acts on is delivered after the final set is in place, so it never tiles
    against a monitor-less or half-defined screen. Foreign (non-selkies)
    monitors are left untouched. A request matching the live set returns
    without touching the server: even that swap costs a delete+create and
    hands the WM a ConfigureNotify to re-tile against. A live monitor
    listing no output never counts as a match: monitors outlive the client
    that set them, and a stale outputless set at the right geometry is
    invisible to GTK apps (see `_monitor_info`), so it is re-swapped.
    """
    with _x11_lock:
        try:
            d = _module_display()
            root, _, out_id, _, _ = _connected_output_state(d)
            reply = randr.get_monitors(root, is_active=False)
            stale = []
            live = {}
            primary_name = None
            every_output_listed = True
            for m in reply.monitors:
                try:
                    name = d.get_atom_name(m.name)
                except Exception:
                    continue
                if name.startswith("selkies-"):
                    stale.append(name)
                    live[name] = (m.x, m.y, m.width_in_pixels, m.height_in_pixels)
                    if m.primary:
                        primary_name = name
                    every_output_listed &= bool(m.crtcs)
            desired = {
                f"selkies-{did}": (int(l["x"]), int(l["y"]), int(l["w"]), int(l["h"]))
                for did, l in layouts.items()
            }
            if live == desired and every_output_listed and (
                primary_name == "selkies-primary" or "selkies-primary" not in desired
            ):
                return
            ordered = sorted(layouts.items(), key=lambda kv: kv[0] != "primary")
            d.grab_server()
            try:
                for name in stale:
                    randr.delete_monitor(root, d.intern_atom(name))
                for display_id, l in ordered:
                    randr.set_monitor(root, _monitor_info(
                        d, out_id, f"selkies-{display_id}",
                        l["x"], l["y"], l["w"], l["h"],
                    ))
                if layouts:
                    randr.set_output_primary(root, out_id)
            finally:
                # Flushed, not just queued: an X error aborting the sequence
                # would leave an unsent ungrab and every other X client wedged.
                try:
                    d.ungrab_server()
                    d.flush()
                except Exception:
                    pass
            d.sync()
            _verify_monitors_on_display(d, {
                f"selkies-{did}": (int(l["x"]), int(l["y"]), int(l["w"]), int(l["h"]))
                for did, l in layouts.items()
            })
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


async def replace_selkies_monitors(
    layouts: Dict[str, Dict[str, int]], screen_name: Optional[str] = None
) -> bool:
    """Swap the selkies-* logical monitor set to exactly ``layouts``.

    Native grab-protected replace first, per-monitor xrandr fallback second
    (the fallback exposes transient states to the WM, but ends at the same
    result).

    Returns:
        True when the final monitor set is in place.
    """
    if not layouts:
        await clear_selkies_monitors()
        return True
    try:
        await asyncio.to_thread(_sync_replace_selkies_monitors, layouts)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native monitor replace failed ({e}); using xrandr fallback.")
    await clear_selkies_monitors()
    ok = True
    for display_id, l in sorted(layouts.items(), key=lambda kv: kv[0] != "primary"):
        ok &= await set_logical_monitor(
            f"selkies-{display_id}", l["x"], l["y"], l["w"], l["h"],
            screen_name=screen_name,
        )
    await designate_primary_output(screen_name)
    return ok


def _sync_wm_name() -> str:
    """Name of the running EWMH window manager ('' when none): root
    _NET_SUPPORTING_WM_CHECK -> child window -> _NET_WM_NAME."""
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            check_atom = d.intern_atom('_NET_SUPPORTING_WM_CHECK')
            # 33 is XA_WINDOW, the property's type.
            prop = root.get_full_property(check_atom, 33)
            if not prop or not prop.value:
                return ""
            wm_win = d.create_resource_object('window', int(prop.value[0]))
            name_prop = wm_win.get_full_property(
                d.intern_atom('_NET_WM_NAME'), d.intern_atom('UTF8_STRING'))
            if name_prop and name_prop.value:
                v = name_prop.value
                return v.decode("utf-8", "replace") if isinstance(v, bytes) else str(v)
            return ""
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            return ""


def _declared_desktop() -> str:
    """What the session calls itself, from the variables it sets for exactly
    this, or "" when it says nothing."""
    for var in ("XDG_CURRENT_DESKTOP", "DESKTOP_SESSION"):
        value = os.environ.get(var, "").strip()
        if value:
            return value
    return ""


def _running_desktop(name: str, session_binary: str) -> bool:
    """Whether the session runs the named desktop.

    A session that says what it is settles it; an installed binary is only a
    hint, and the wrong one wherever more than one desktop is installed, so it
    answers for a session that says nothing at all.

    Args:
        name: Desktop name as it appears in the session's own variables.
        session_binary: The binary that names that desktop, for a session that
            declares none.
    """
    declared = _declared_desktop().lower()
    if declared:
        return name.lower() in declared
    return bool(which(session_binary))


class MultiMonitorWindowManager:
    """Hands X11 window management to another window manager once a session has
    two displays.

    Some desktops (XFCE and Plasma among them) tile a maximized window across
    the whole framebuffer rather than against the per-display regions an
    extended layout defines, and handing the session to a window manager that
    does not is the way out. Which one is `--multi-monitor-wm`, unset by
    default: restarting window management belongs to a session the deployment
    assembled, never to a desktop somebody is using, and only the deployment
    knows which of the two it runs. Both transports share this state: the swap
    is attempted once per session either way, since a second one would restart
    window management under whoever is using it. Wayland sessions manage their
    own windows and never swap.
    """

    def __init__(self) -> None:
        self._swapped = False

    async def ensure_for(self, display_count: int, is_wayland: bool) -> None:
        """Swap if this session was given a window manager and has not swapped.

        The replacement is started detached, so it outlives this process's
        session, and with the arguments the deployment gave: a window manager
        started off its own stock configuration chain keeps the bindings its
        compiled-in defaults do not cover, which a hand-written minimal config
        would strip.
        """
        if is_wayland or self._swapped or display_count <= 1:
            return
        from .settings import settings as _s
        command = str(getattr(_s, "multi_monitor_wm", "") or "").strip().split()
        if not command:
            return
        name = os.path.basename(command[0])
        self._swapped = True
        if name.lower() in (await current_wm_name()).lower():
            logger_app_resize.info(
                f"Multi-monitor setup: {name} already manages the session; no WM swap.")
            return
        logger_app_resize.info(f"Multi-monitor setup: switching to {name}.")
        try:
            await asyncio.create_subprocess_exec(
                *command,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
                start_new_session=True,
            )
        except Exception as e:
            logger_app_resize.error(f"Failed to switch to {name}: {e}")
            return
        # Before the layout applies: a WM snapshotting the monitor set mid-swap
        # re-tiles maximized windows across the whole framebuffer.
        if not await wait_for_wm(name):
            logger_app_resize.warning(
                f"{name} takeover not confirmed; applying layout anyway.")


async def current_wm_name() -> str:
    """Name of the running EWMH window manager, '' when undetectable."""
    return await asyncio.to_thread(_sync_wm_name)


async def wait_for_wm(name_substring: str, timeout: float = 3.0) -> bool:
    """Wait until the EWMH WM name contains ``name_substring`` (case-insensitive).

    Used after a WM --replace so layout changes are not applied while two
    window managers hand over the selection (the incoming WM snapshots the
    monitor set it starts against).

    Returns:
        True when the name matched within ``timeout`` seconds.
    """
    deadline = asyncio.get_running_loop().time() + timeout
    want = name_substring.lower()
    while True:
        name = await current_wm_name()
        if want in name.lower():
            return True
        if asyncio.get_running_loop().time() >= deadline:
            return False
        await asyncio.sleep(0.15)


async def list_logical_monitors() -> List[str]:
    """Names of ALL RandR logical monitors: native query first, xrandr
    --listmonitors parse as fallback."""
    try:
        return await asyncio.to_thread(_sync_list_monitors)
    except Exception as e:
        logger_app_resize.info(f"Native monitor list failed ({e}); using xrandr fallback.")
    names = []
    try:
        proc = await subprocess.create_subprocess_exec(
            "xrandr", "--listmonitors",
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        stdout, _ = await _communicate_or_kill(proc)
        if proc.returncode == 0:
            for line in stdout.decode(errors="replace").splitlines()[1:]:
                parts = line.split()
                if len(parts) >= 2:
                    names.append(parts[1].lstrip("*+"))
    except Exception as e:
        logger_app_resize.warning(f"xrandr --listmonitors failed: {e}")
    return names


async def set_logical_monitor(
    name: str,
    x: int,
    y: int,
    w: int,
    h: int,
    screen_name: Optional[str] = None,
) -> bool:
    """Define/replace logical monitor ``name`` over the given pixel geometry:
    native RandR 1.5 first, xrandr --setmonitor fallback. The monitor lists
    the physical output (see `_monitor_info`); the fallback goes outputless
    only when no output name is known. Returns success."""
    try:
        await asyncio.to_thread(_sync_set_monitor, name, x, y, w, h)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native set-monitor '{name}' failed ({e}); using xrandr fallback.")
    geometry = f"{w}/0x{h}/0+{x}+{y}"
    return await _run_xrandr(
        ["--setmonitor", name, geometry, screen_name or "none"],
        f"set logical monitor {name}",
    )


async def delete_logical_monitor(name: str) -> bool:
    """Delete logical monitor ``name``: native first, xrandr fallback. Returns
    success (deleting an absent monitor counts as failure on both paths)."""
    try:
        await asyncio.to_thread(_sync_delete_monitor, name)
        return True
    except Exception as e:
        logger_app_resize.debug(f"Native delete-monitor '{name}' failed ({e}); using xrandr fallback.")
    return await _run_xrandr(["--delmonitor", name], f"delete monitor {name}")


async def designate_primary_output(screen_name: Optional[str] = None) -> bool:
    """Flag the connected physical output primary so the WM anchors panels and
    new windows to it: native first, xrandr fallback. Returns success."""
    try:
        await asyncio.to_thread(_sync_set_output_primary)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native set-output-primary failed ({e}); using xrandr fallback.")
    if screen_name:
        return await _run_xrandr(["--output", screen_name, "--primary"], "designate primary output")
    return False


async def grow_framebuffer(w: int, h: int) -> bool:
    """Grow-only framebuffer resize, used before live captures re-target so no
    region ever lies outside the root: native first, xrandr --fb fallback."""
    try:
        await asyncio.to_thread(_sync_grow_screen, w, h)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native framebuffer grow failed ({e}); using xrandr fallback.")
    return await _run_xrandr(["--fb", f"{w}x{h}"], "grow framebuffer")


async def list_selkies_monitors() -> List[str]:
    """Names of the logical monitors this software created (selkies-*)."""
    return [n for n in await list_logical_monitors() if "selkies-" in n]


async def clear_selkies_monitors() -> None:
    """Delete every logical monitor this software created (selkies-*)."""
    for monitor_name in await list_selkies_monitors():
        await delete_logical_monitor(monitor_name)


class RealizedFit(NamedTuple):
    """What fitting a layout to the realized root changed.

    ``dropped`` displays are gone from the layout, ``clamped`` ones kept a
    smaller rectangle, and ``reanchored`` means the primary moved back to the
    origin (which voids the arrangement, so every secondary is dropped).
    """
    dropped: List[str]
    reanchored: bool
    clamped: List[str]


def reconcile_realized_layout(
    layouts: Dict[str, Dict[str, int]], realized_w: int, realized_h: int
) -> RealizedFit:
    """Fit ``layouts`` to the root the server actually realized, in place.

    The X server, not the request, is the authority on the realized geometry: a
    driver can reject the mode or framebuffer size and leave the root at its
    old dimensions, and a capture region outside the root fails or grabs
    garbage while a pointer warp cannot reach it. Rectangles are clamped inside
    the root; a primary that no longer fits at its offset is re-anchored at the
    origin, which voids the arrangement and drops every secondary with it.

    Callers own the side effects the result implies: stopping captures, telling
    the client, and re-swapping the logical monitors.
    """
    primary = layouts.get("primary")
    reanchored = bool(primary) and (
        (primary["x"] > 0 and primary["x"] + primary["w"] > realized_w)
        or (primary["y"] > 0 and primary["y"] + primary["h"] > realized_h)
    )
    if reanchored:
        primary["x"], primary["y"] = 0, 0
    dropped: List[str] = []
    clamped: List[str] = []
    for did, layout in list(layouts.items()):
        if did != "primary" and (
            reanchored or layout["x"] >= realized_w or layout["y"] >= realized_h
        ):
            del layouts[did]
            dropped.append(did)
            continue
        fit_w = max(2, min(layout["w"], realized_w - layout["x"]) & ~1)
        fit_h = max(2, min(layout["h"], realized_h - layout["y"]) & ~1)
        if (fit_w, fit_h) != (layout["w"], layout["h"]):
            layout["w"], layout["h"] = fit_w, fit_h
            clamped.append(did)
    return RealizedFit(dropped, reanchored, clamped)


async def read_realized_root(fallback: Tuple[int, int]) -> Tuple[int, int]:
    """The root window's realized size, or ``fallback`` when it cannot be read."""
    realized_res, _, _, _, _ = await get_new_res("1x1")
    try:
        w, h = (int(v) for v in (realized_res or "").lower().replace(" ", "").split("x"))
    except (ValueError, AttributeError):
        return fallback
    return (w, h) if w > 0 and h > 0 else fallback


async def apply_extended_layout(
    layouts: Dict[str, Dict[str, int]], total_w: int, total_h: int
) -> bool:
    """Drive the server into an extended-desktop framebuffer covering ``layouts``.

    ``layouts`` maps display id to an `{x, y, w, h}` rectangle. Ensures the
    total mode exists, sizes the framebuffer, and defines one `selkies-<id>`
    logical monitor per display so window managers tile against the
    per-display regions. Mirrors the websockets engine's command sequence.

    The monitors go first, at their final rectangles and under a server grab:
    window managers re-tile maximized windows on every root ConfigureNotify,
    so no WM-visible stimulus (the swap itself, the resize after it) may ever
    expose a monitor-less or partial set. A server that refuses runtime mode
    creation may still honor a plain framebuffer grow (RRSetScreenSize): the
    output keeps its mode while captures and pointer warps address the
    enlarged root. The server, not the request, is the authority on the
    realized geometry, and it can report success while leaving the root
    short, so the layout is fitted to what is really there before any capture
    is pointed at it; where fitting moved a display the monitors are all
    redefined at the fitted rectangles (a dropped display's monitor
    disappears with the swap), while a root that merely came back larger than
    asked leaves them alone, since every swap makes window managers re-tile.

    Returns:
        True when the framebuffer and monitors were set. ``layouts`` is fitted
        in place to the root the server actually produced, so the caller must
        read the rectangles back rather than reuse the ones it passed in: a
        display kept at a smaller size carries the smaller one, and a display
        that could not be placed at all is gone from the mapping. False when
        nothing could be laid out; the monitors are torn down.
    """
    total_mode = f"{total_w}x{total_h}"
    curr_res, _, available, _, screen_name = await get_new_res(total_mode)
    if not screen_name:
        logger_app_resize.error("Could not determine output name; cannot apply layout.")
        return False
    if total_mode not in (available or []):
        if not await ensure_mode(total_mode):
            try:
                _, modeline = await generate_xrandr_gtf_modeline(total_mode)
                await _run_xrandr(["--newmode", total_mode] + modeline.split(), "create mode")
                await _run_xrandr(["--addmode", screen_name, total_mode], "add mode")
            except Exception as e:
                logger_app_resize.error(f"Could not create extended mode {total_mode}: {e}")
                return False
    if not await replace_selkies_monitors(layouts, screen_name=screen_name):
        await clear_selkies_monitors()
        return False
    if (curr_res or "").lower().replace(" ", "") != total_mode:
        if not await resize_display(total_mode):
            if not await grow_framebuffer(total_w, total_h):
                logger_app_resize.error(
                    f"Neither a mode-set nor a framebuffer grow reached {total_mode}; "
                    "fitting the layout to whatever the root realized."
                )
    realized_w, realized_h = await read_realized_root((total_w, total_h))
    if (realized_w, realized_h) == (total_w, total_h):
        return True
    logger_app_resize.warning(
        f"Realized screen size {realized_w}x{realized_h} differs from target "
        f"{total_mode}; fitting the display layouts to it."
    )
    offsets = {d: (l["x"], l["y"]) for d, l in layouts.items()}
    fit = reconcile_realized_layout(layouts, realized_w, realized_h)
    if fit.reanchored:
        logger_app_resize.error(
            f"Primary at +{offsets['primary'][0]}+{offsets['primary'][1]} does not fit the "
            f"realized {realized_w}x{realized_h} root; re-anchored at the origin."
        )
    for did in fit.dropped:
        logger_app_resize.error(
            f"Display '{did}' at +{offsets[did][0]}+{offsets[did][1]} does not fit the realized "
            f"{realized_w}x{realized_h} root; dropping it. The X server must allow a framebuffer "
            "covering all displays (e.g. a larger Xvfb -screen) for extended layouts."
        )
    for did in fit.clamped:
        logger_app_resize.warning(
            f"Display '{did}': layout clamped to {layouts[did]['w']}x{layouts[did]['h']} "
            "inside the realized root."
        )
    if fit.dropped or fit.reanchored or fit.clamped:
        if not await replace_selkies_monitors(layouts, screen_name=screen_name):
            await clear_selkies_monitors()
            return False
    return True


async def get_new_res(res_str: str) -> Tuple[str, str, List[str], str, Optional[str]]:
    """Current/fitted resolution info for the first connected output.

    Native RandR query first, xrandr parse as fallback.

    Returns:
        ``(curr_res, fitted res_str, sorted mode names, max res, output
        name)``; the output name is None when no screen could be identified.
    """
    try:
        curr_res, resolutions, screen_name = await asyncio.to_thread(_sync_query_randr)
    except Exception as e:
        logger_app_resize.info(f"Native RandR query failed ({e}); using xrandr fallback.")
        return await _get_new_res_xrandr(res_str)
    max_w_limit, max_h_limit = 7680, 4320
    max_res_str = f"{max_w_limit}x{max_h_limit}"
    new_res = res_str
    try:
        w, h = map(int, res_str.split("x"))
        new_w, new_h = fit_res(w, h, max_w_limit, max_h_limit)
        new_res = f"{new_w}x{new_h}"
    except ValueError:
        logger_app_resize.error(f"Invalid resolution format for fitting: {res_str}")
    return curr_res, new_res, resolutions, max_res_str, screen_name


async def _get_new_res_xrandr(
    res_str: str,
) -> Tuple[str, str, List[str], str, Optional[str]]:
    """xrandr-subprocess fallback for get_new_res (same result tuple)."""
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
        stdout, _ = await _communicate_or_kill(process)
        xrandr_output = stdout.decode('utf-8')
    except (FileNotFoundError, Exception) as e:
        logger_app_resize.error(f"xrandr command failed: {e}")
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
        logger_app_resize.warning(
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
        logger_app_resize.error(f"Invalid resolution format for fitting: {res_str}")
    resolutions = sorted(list(set(resolutions)))
    return curr_res, new_res, resolutions, max_res_str, screen_name


async def resize_display(res_str: str) -> Optional[Tuple[int, int]]:
    """Resize the display to ``res_str`` (e.g. "2560x1280").

    Native RandR first (mode created from CVT-RB timings when absent), with
    the xrandr/cvt subprocess chain as fallback.

    Returns:
        The realized ``(width, height)`` — CVT cell alignment may make it
        wider than requested — or None on failure. Callers must capture and
        report the realized size, not the request.
    """
    try:
        w, h = await asyncio.to_thread(_sync_resize_randr, res_str)
    except Exception as e:
        logger_app_resize.info(
            f"Native RandR resize for '{res_str}' failed ({e}); falling back to xrandr."
        )
        return await _resize_display_xrandr(res_str)
    logger_app_resize.info(
        f"Successfully applied RandR mode '{res_str}' ({w}x{h})."
    )
    return w, h


async def _resize_display_xrandr(res_str: str) -> Optional[Tuple[int, int]]:
    """Resize the display using xrandr subprocesses.

    Adds a new mode via cvt/gtf if the requested mode doesn't exist, naming
    it for the geometry the modeline really carries (cvt snaps width up to
    the 8-pixel CVT cell, so it can be wider than requested). The mode is set
    together with ``--fb`` sized from that realized geometry: without it a
    larger root left over from a prior extended layout keeps the screen
    oversized, so the new mode lands top-left and whole-root capture shows
    black bars (the native path and the websockets engine both force it),
    and a framebuffer narrower than the active mode is rejected outright.

    Returns:
        The realized ``(width, height)``, or None on failure.
    """
    _, _, available_resolutions, _, screen_name = await _get_new_res_xrandr(res_str)

    if not screen_name:
        logger_app_resize.error(
            "Cannot resize display via xrandr, no screen identified."
        )
        return None

    try:
        w_req, h_req = (int(p) for p in res_str.split("x"))
    except ValueError:
        logger_app_resize.error(f"Invalid resolution format: {res_str}")
        return None

    target_mode_to_set = res_str
    realized_w, realized_h = w_req, h_req

    if res_str not in available_resolutions:
        logger_app_resize.info(
            f"Mode {res_str} not found in xrandr list. Attempting to add for screen '{screen_name}'."
        )
        try:
            (
                modeline_name_from_cvt_output,
                modeline_params,
            ) = await generate_xrandr_gtf_modeline(res_str)
        except Exception as e:
            logger_app_resize.error(
                f"Failed to generate modeline for {res_str}: {e}"
            )
            return None

        # Modeline fields: clock hdisp hss hse htot vdisp vss vse vtot flags...
        params = modeline_params.split()
        try:
            realized_w, realized_h = int(params[1]), int(params[5])
        except (IndexError, ValueError):
            realized_w, realized_h = w_req, h_req
        target_mode_to_set = f"{realized_w}x{realized_h}"

        if target_mode_to_set not in available_resolutions:
            cmd_new = ["xrandr", "--newmode", target_mode_to_set] + params
            new_mode_proc = await subprocess.create_subprocess_exec(
                *cmd_new,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            stdout_new, stderr_new = await _communicate_or_kill(new_mode_proc)
            if new_mode_proc.returncode != 0:
                logger_app_resize.error(
                    f"Failed to create new xrandr mode with '{' '.join(cmd_new)}': {stderr_new.decode()}"
                )
                return None
            logger_app_resize.info(f"Successfully ran: {' '.join(cmd_new)}")

            cmd_add = ["xrandr", "--addmode", screen_name, target_mode_to_set]
            add_mode_proc = await subprocess.create_subprocess_exec(
                *cmd_add,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            stdout_add, stderr_add = await _communicate_or_kill(add_mode_proc)
            if add_mode_proc.returncode != 0:
                logger_app_resize.error(
                    f"Failed to add mode '{target_mode_to_set}' to screen '{screen_name}': {stderr_add.decode()}"
                )
                delmode_proc = await subprocess.create_subprocess_exec(
                    "xrandr", "--delmode", screen_name, target_mode_to_set,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE
                )
                await _communicate_or_kill(delmode_proc)

                rmmode_proc = await subprocess.create_subprocess_exec(
                    "xrandr", "--rmmode", target_mode_to_set,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE
                )
                await _communicate_or_kill(rmmode_proc)
                return None
            logger_app_resize.info(f"Successfully ran: {' '.join(cmd_add)}")

    logger_app_resize.info(
        f"Applying xrandr mode '{target_mode_to_set}' for screen '{screen_name}'."
    )
    cmd_output = ["xrandr", "--output", screen_name, "--mode", target_mode_to_set,
                  "--fb", f"{realized_w}x{realized_h}"]
    set_mode_proc = await subprocess.create_subprocess_exec(
        *cmd_output,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE
    )
    stdout_set, stderr_set = await _communicate_or_kill(set_mode_proc)
    if set_mode_proc.returncode != 0:
        # A pre-existing mode can be CVT-snapped wider than its name claims, so
        # a framebuffer sized from the name is rejected; retry at the snapped width.
        snapped_w = -(-w_req // 8) * 8
        retried = False
        if target_mode_to_set == res_str and snapped_w != realized_w:
            cmd_retry = ["xrandr", "--output", screen_name, "--mode", target_mode_to_set,
                         "--fb", f"{snapped_w}x{h_req}"]
            retry_proc = await subprocess.create_subprocess_exec(
                *cmd_retry,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            _, stderr_retry = await _communicate_or_kill(retry_proc)
            if retry_proc.returncode == 0:
                realized_w, realized_h = snapped_w, h_req
                retried = True
            else:
                stderr_set = stderr_retry
        if not retried:
            logger_app_resize.error(
                f"Failed to set mode '{target_mode_to_set}' on screen '{screen_name}': {stderr_set.decode()}"
            )
            return None

    logger_app_resize.info(
        f"Successfully applied xrandr mode '{target_mode_to_set}' ({realized_w}x{realized_h})."
    )
    return realized_w, realized_h


# Keyed by (resolution, refresh): the timings change with the refresh rate.
_MODELINE_CACHE: Dict[Tuple[str, int], Tuple[str, str]] = {}


async def generate_xrandr_gtf_modeline(
    res_wh_str: str, refresh_hz: int = 60
) -> Tuple[str, str]:
    """Generate an xrandr modeline using cvt, falling back to gtf.

    ``refresh_hz`` defaults to 60 (the rate selkies requests for display
    modes); it is part of the cache key so a mode generated at another rate
    gets its own timings rather than a stale 60 Hz modeline for the same
    size. Successful results are memoized so a size/refresh computed once
    never re-spawns the cvt/gtf subprocess, including when the X mode was
    later dropped and has to be re-created on a subsequent reconfigure.

    Returns:
        ``(mode name, timing parameters)`` as parsed from the tool output.

    Raises:
        Exception: If neither tool can produce a parseable modeline.
    """
    cache_key = (res_wh_str, refresh_hz)
    cached = _MODELINE_CACHE.get(cache_key)
    if cached is not None:
        return cached
    refresh_str = str(refresh_hz)
    tool_name = "cvt"
    try:
        try:
            w_str, h_str = res_wh_str.split("x")
        except ValueError as e:
            raise Exception(
                f"Invalid resolution format for modeline generation: {res_wh_str}"
            ) from e
        cmd = ["cvt", w_str, h_str, refresh_str]
        try:
            process = await subprocess.create_subprocess_exec(
                *cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            stdout, stderr = await _communicate_or_kill(process)
            if process.returncode != 0:
                raise Exception(f"cvt failed: {stderr.decode()}")
            modeline_output = stdout.decode('utf-8')
        except Exception:
            logger_app_resize.warning(
                "cvt command failed or not found, trying gtf."
            )
            cmd = ["gtf", w_str, h_str, refresh_str]
            tool_name = "gtf"
            process = await subprocess.create_subprocess_exec(
                *cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            stdout, stderr = await _communicate_or_kill(process)
            if process.returncode != 0:
                raise Exception(f"gtf failed: {stderr.decode()}") from None
            modeline_output = stdout.decode('utf-8')
    except Exception as e:
        raise Exception(
            f"Failed to generate modeline using {tool_name} for {res_wh_str}: {e}"
        ) from e
    match = re.search(r'Modeline\s+"([^"]+)"\s+(.*)', modeline_output)
    if not match:
        raise Exception(
            f"Could not parse modeline from {tool_name} output: {modeline_output}"
        )
    result = (match.group(1).strip(), match.group(2))
    _MODELINE_CACHE[cache_key] = result
    return result

def _atomic_write_text(path: str, content: str) -> None:
    """Install `content` at `path` by writing a temporary file in the same
    directory and renaming it over the target.

    Nothing can observe a truncated or half-written file: a write that fails
    part-way (a full filesystem, an I/O error, the process dying) leaves the
    existing target untouched. A symlinked path is resolved first so a dotfile
    managed as a link into a dotfiles repository stays a link, and an existing
    target's permission bits are carried onto the replacement.
    """
    target = os.path.realpath(path)
    fd, tmp_path = tempfile.mkstemp(prefix=".selkies-", dir=os.path.dirname(target) or ".")
    try:
        with os.fdopen(fd, "w") as f:
            f.write(content)
            f.flush()
            os.fsync(f.fileno())
        try:
            mode = stat.S_IMODE(os.stat(target).st_mode)
        except OSError:
            mode = 0o644
        os.chmod(tmp_path, mode)
        os.replace(tmp_path, target)
    except BaseException:
        try:
            os.remove(tmp_path)
        except OSError:
            pass
        raise


def _write_xresources_dpi(xresources_path_str: str, dpi_value: int) -> None:
    """Persist Xft.dpi in the user's Xresources file, rewriting only that resource.

    Every other line the user keeps there (colors, terminal settings, keyboard
    resources) is preserved, so a DPI change never costs them their session
    configuration. A missing file is created with just the DPI line. Blocking
    file I/O: callers on the event loop run it on an executor.
    """
    try:
        with open(xresources_path_str, "r") as f:
            lines = [
                line for line in f.read().splitlines()
                if not re.match(r"^\s*Xft\.dpi\s*:", line)
            ]
    except FileNotFoundError:
        lines = []
    lines.append(f"Xft.dpi:   {dpi_value}")
    _atomic_write_text(xresources_path_str, "\n".join(lines) + "\n")


_LXQT_FONT_LINE = re.compile(r'^(\s*font\s*=\s*)"?([^"\n]*)"?\s*$', re.I)


def _rewrite_lxqt_font(path: str, dpi_value: int) -> Optional[Tuple[float, int]]:
    """Resolve the session font's point size to pixels at `dpi_value`.

    Returns (points, pixels) when the file was rewritten, None when there is no
    LXQt configuration or no font in it. Only the font line changes; every other
    setting in the file is left byte for byte.

    Qt keeps a widget's font from the moment it is built, so a density delivered
    as Xft resources alone reaches nothing already on screen. The LXQt platform
    theme watches this file and answers a change with QApplication::setFont,
    which is the one call that repolishes those widgets — but Qt drops a font it
    considers equal, and a point size is equal at every density. Resolving it to
    pixels is what makes the change land. The point size stays in the field Qt
    ignores once a pixel size is set, so the next density has a base to scale.
    """
    try:
        with open(path, "r") as handle:
            lines = handle.readlines()
    except OSError:
        return None

    section, out, resolved = "", [], None
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped[1:-1].lower()
        match = _LXQT_FONT_LINE.match(line) if section == "qt" else None
        if match is None or resolved is not None:
            out.append(line)
            continue
        fields = match.group(2).split(",")
        if len(fields) < 3:
            out.append(line)
            continue
        try:
            points, pixels = float(fields[1]), float(fields[2])
        except ValueError:
            out.append(line)
            continue
        if points <= 0:
            # A file Qt round-tripped carries pixels alone, resolved at the
            # density currently on the display.
            points = pixels * 72.0 / max(1, _APPLIED_DPI or 96)
        if points <= 0:
            out.append(line)
            continue
        fields[1] = f"{points:g}"
        fields[2] = str(max(1, round(points * dpi_value / 72.0)))
        out.append(f'{match.group(1)}"{",".join(fields)}"\n')
        resolved = (points, int(fields[2]))

    if resolved is None:
        return None
    _atomic_write_text(path, "".join(out))
    return resolved


async def _run_lxqt_font(dpi_value: int, logger: logging.Logger) -> bool:
    """Hand the density to a running LXQt session's Qt applications.

    The X11 counterpart of the Wayland output scale: there the compositor tells
    clients their scale and they redraw, here the platform theme repolishes them
    from its own configuration. Applications on other toolkits, and any started
    later, take the same density from the Xft resources instead.
    """
    path = os.path.expanduser("~/.config/lxqt/lxqt.conf")
    try:
        resolved = await asyncio.to_thread(_rewrite_lxqt_font, path, dpi_value)
    except OSError as e:
        logger.debug(f"LXQt session font not retargeted: {e}")
        return False
    if resolved is None:
        return False
    logger.info(
        f"LXQt session font resolved to {resolved[1]}px for DPI {dpi_value} "
        f"({resolved[0]:g}pt), repolishing running applications.")
    return True


async def _run_xrdb(dpi_value: int, logger: logging.Logger) -> bool:
    """Apply DPI via Xresources/xrdb and the xsettingsd config.

    Writes ``Xft.dpi`` into ~/.Xresources and merges it into the running
    resource database — merged, never loaded wholesale, so the database keeps
    every resource the file does not define — then rewrites ~/.xsettingsd
    with the matching Xft/DPI value (in 1024ths) and SIGHUPs every running
    xsettingsd: the one serving this display is not necessarily the oldest,
    and a daemon that is not ours only re-reads a configuration this write
    did not touch.

    Returns:
        True when the xrdb merge succeeded.
    """
    if not which("xrdb"):
        logger.debug("xrdb not found. Skipping Xresources DPI setting.")
        return False

    xresources_path_str = os.path.expanduser("~/.Xresources")
    loop = asyncio.get_running_loop()
    try:
        await loop.run_in_executor(
            None, _write_xresources_dpi, xresources_path_str, dpi_value
        )
        logger.info(f"Wrote 'Xft.dpi:   {dpi_value}' to {xresources_path_str}.")

        cmd_xrdb = ["xrdb", "-merge", xresources_path_str]
        process = await subprocess.create_subprocess_exec(
            *cmd_xrdb,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        stdout, stderr = await _communicate_or_kill(process)
        
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
        
        await loop.run_in_executor(
            None, _atomic_write_text, xsettingsd_config_path, config_content
        )
        logger.info(f"Wrote font and DPI settings to {xsettingsd_config_path}.")

        if not which("pgrep"):
            logger.debug("pgrep not found. Skipping xsettingsd reload.")
        else:
            pgrep_proc = await subprocess.create_subprocess_exec(
                "pgrep", "xsettingsd",
                stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            pgrep_stdout, _ = await _communicate_or_kill(pgrep_proc)

            if pgrep_proc.returncode == 0:
                signalled = []
                for line in pgrep_stdout.decode().split():
                    try:
                        os.kill(int(line), signal.SIGHUP)
                        signalled.append(line)
                    except (OSError, ValueError) as e:
                        logger.debug(f"Failed to send SIGHUP to xsettingsd process {line}: {e}")
                if signalled:
                    logger.info(
                        f"Sent SIGHUP to xsettingsd to reload config ({', '.join(signalled)}).")
                else:
                    logger.warning("No xsettingsd process could be signalled to reload.")
            else:
                logger.info("xsettingsd process not found. Skipping reload.")
        
        return xrdb_success

    except Exception as e:
        logger.error(f"Error updating or loading DPI settings: {e}")
        return False

async def _get_xfce_session_env(logger: logging.Logger) -> Optional[Dict[str, str]]:
    """Environment of the running xfce4-session process.

    xfconf-query must talk to the session's own D-Bus bus, so the variables
    are lifted from the process's ``/proc/pid/environ``.

    Returns:
        The environment mapping, or None when the session (or its
        DBUS_SESSION_BUS_ADDRESS) cannot be found.
    """
    try:
        proc_pid = await subprocess.create_subprocess_exec(
            "pgrep", "-o", "-x", "xfce4-session",
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        stdout_pid, stderr_pid = await _communicate_or_kill(proc_pid)

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


async def _run_xfconf(dpi_value: int, logger: logging.Logger) -> bool:
    """Apply DPI and a DPI-scaled cursor size via xfconf-query for XFCE.

    Commands run inside the live XFCE session environment when it can be
    found, so they reach the session's own D-Bus bus.

    Returns:
        True when both settings were applied.
    """
    if not which("xfconf-query"):
        logger.debug("xfconf-query not found. Skipping XFCE DPI setting via xfconf-query.")
        return False

    session_env = await _get_xfce_session_env(logger)
    if session_env:
        logger.info("Found active XFCE session environment. Commands will be executed within this context.")
    else:
        logger.warning("Could not obtain XFCE session environment. Falling back to direct execution.")

    async def run_command(cmd: List[str], success_msg: str, failure_msg: str) -> bool:
        try:
            process = await subprocess.create_subprocess_exec(
                *cmd,
                env=session_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            _stdout, stderr = await _communicate_or_kill(process)
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

async def _run_mate_gsettings(dpi_value: int, logger: logging.Logger) -> bool:
    """Apply DPI via MATE gsettings (window-scaling-factor and font DPI).

    ``window-scaling-factor`` is integer-only, so it carries whole scales and
    stays 1 otherwise, the fractional part riding on the font DPI.

    Returns:
        True when at least one setting was applied.
    """
    if not which("gsettings"):
        logger.debug("gsettings not found. Skipping MATE gsettings.")
        return False

    mate_settings_succeeded_at_least_once = False

    try:
        target_mate_scale_float = float(dpi_value) / 96.0
        if target_mate_scale_float == int(target_mate_scale_float):
            mate_window_scaling_factor = int(target_mate_scale_float)
        else:
            mate_window_scaling_factor = 1 
        
        mate_window_scaling_factor = max(1, mate_window_scaling_factor)

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
        stdout_mate_window, stderr_mate_window = await _communicate_or_kill(result_mate_window_scale)
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

    try:
        cmd_gsettings_mate_font_dpi = [
            "gsettings", "set",
            "org.mate.font-rendering", "dpi",
            str(dpi_value)
        ]
        result_mate_font_dpi = await subprocess.create_subprocess_exec(
            *cmd_gsettings_mate_font_dpi,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE
        )
        stdout_mate_font, stderr_mate_font = await _communicate_or_kill(result_mate_font_dpi)
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


def _is_wayland() -> bool:
    """True when selkies runs its Wayland compositor (no X display tools apply).
    Lazy so the selkies-resize CLI works without full settings initialization."""
    try:
        from .settings import settings as _s
        return bool(_s.wayland[0])
    except Exception:
        return False


# The DPI the last set_dpi applied; resizes size the pane in mm from it.
_APPLIED_DPI: Optional[int] = None


def _sync_stamp_root_dpi(dpi_value: int) -> None:
    """Resize the root pane's reported physical size to match ``dpi_value``.

    Keeps the DPI the X server itself reports (xdpyinfo/RandR consumers) in
    step with the DPI the desktops were told to render at. Idempotent: a
    matching mm size posts no RRSetScreenSize at all, so the idleness of
    repeated SETTINGS payloads stays ConfigureNotify-free.
    """
    global _APPLIED_DPI
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            geom = root.get_geometry()
            mm_w = max(1, round(geom.width * 25.4 / dpi_value))
            mm_h = max(1, round(geom.height * 25.4 / dpi_value))
            info = randr.get_screen_info(root)
            cur = info.sizes[info.size_id] if info.sizes else None
            if cur and (cur.width_in_millimeters, cur.height_in_millimeters) == (mm_w, mm_h):
                _APPLIED_DPI = dpi_value
                return
            randr.set_screen_size(root, geom.width, geom.height, mm_w, mm_h)
            d.sync()
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise
    _APPLIED_DPI = dpi_value


async def set_dpi(dpi_setting: Union[int, str]) -> bool:
    """Set the X11 display DPI using DE-specific methods.

    Detection order: KDE, XFCE, MATE, i3, LXQt, Openbox, then a generic xrdb
    fallback. XFCE takes only xfconf-query so scaling is never applied
    twice; MATE takes gsettings plus xrdb for wider application coverage.
    The LXQt font repolish runs whichever branch was taken: the session that
    owns the windows decides whether anything already drawn follows, and it
    is the only one that can repolish them. On success the root pane's
    physical size is stamped with the same density, because xdpyinfo/RandR
    consumers (Qt's fallback included) read it and would otherwise render
    unscaled against the rest of the desktop.

    X11 only. On the Wayland backend a DPI is an output scale, never Xft
    resources: the compositor hands applications the scale, and XWayland runs
    in its LOGICAL space, so resources merged there would be applied twice —
    once by the toolkit and again by the compositor upscaling the surface.

    Args:
        dpi_setting: A positive integer, or a string representing one.

    Returns:
        True when at least one method succeeded.
    """
    try:
        dpi_value = int(str(dpi_setting))
        if dpi_value <= 0:
            logger_app_resize.error(f"Invalid DPI value: {dpi_value}. Must be a positive integer.")
            return False
    except ValueError:
        logger_app_resize.error(f"Invalid DPI format: '{dpi_setting}'. Must be convertible to a positive integer.")
        return False

    if _is_wayland():
        logger_app_resize.debug(
            "Wayland backend: DPI realizes as a compositor output scale.")
        return False

    global _APPLIED_DPI
    if _APPLIED_DPI == dpi_value:
        logger_app_resize.debug(f"DPI {dpi_value} already applied; skipping the re-ladder.")
        return True

    any_method_succeeded = False
    desktop = _declared_desktop() or "unnamed"

    # Only two desktops keep the density somewhere other than the X resource
    # database, and everything else — named or not — reads it from there, so
    # there is nothing to gain from recognising any of them by name.
    if _running_desktop("xfce", "xfce4-session"):
        logger_app_resize.info(f"XFCE session ({desktop}): applying xfconf-query for DPI {dpi_value}.")
        if await _run_xfconf(dpi_value, logger_app_resize):
            any_method_succeeded = True
    elif _running_desktop("mate", "mate-session"):
        logger_app_resize.info(f"MATE session ({desktop}): applying gsettings and xrdb for DPI {dpi_value}.")
        mate_gsettings_success = await _run_mate_gsettings(dpi_value, logger_app_resize)
        xrdb_for_mate_success = await _run_xrdb(dpi_value, logger_app_resize)
        if mate_gsettings_success or xrdb_for_mate_success:
            any_method_succeeded = True
    else:
        logger_app_resize.info(f"{desktop} session: applying xrdb for DPI {dpi_value}.")
        if await _run_xrdb(dpi_value, logger_app_resize):
            any_method_succeeded = True

    if await _run_lxqt_font(dpi_value, logger_app_resize):
        any_method_succeeded = True

    if not any_method_succeeded:
        logger_app_resize.warning(
            f"No DPI setting method succeeded for DPI {dpi_value} ({desktop} session).")
    else:
        try:
            await asyncio.to_thread(_sync_stamp_root_dpi, dpi_value)
        except Exception as e:
            logger_app_resize.warning(f"Root mm-size retarget to {dpi_value} DPI failed: {e}")

    return any_method_succeeded

async def _set_xcursor_resource(size: int) -> bool:
    """Merge Xcursor.size into the root resource database. Xcursor-driven
    consumers (openbox after the multi-monitor WM swap, plain X apps) take
    their cursor size from here, not from XFCE/GNOME settings daemons."""
    if not which("xrdb"):
        return False
    process = None
    try:
        process = await subprocess.create_subprocess_exec(
            "xrdb", "-merge", "-",
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        await asyncio.wait_for(process.communicate(f"Xcursor.size: {size}\n".encode()), timeout=10)
        return process.returncode == 0
    except Exception as e:
        logger_app_resize.debug(f"xrdb Xcursor.size merge failed: {e}")
        if process is not None:
            try:
                process.kill()
            except Exception:
                pass
        return False


async def set_cursor_size(size: int) -> bool:
    """Set the X cursor size through every applicable settings channel.

    Merges Xcursor.size via xrdb, then tries the XFCE and GNOME settings
    daemons; desktop-aware toolkits follow their daemon while plain X apps
    follow the Xcursor resource, so daemon success returns immediately and
    the xrdb merge alone still counts as success.

    Returns:
        True when any channel applied the size.
    """
    if not isinstance(size, int) or size <= 0:
        logger_app_resize.error(f"Invalid cursor size: {size}")
        return False
    xrdb_ok = await _set_xcursor_resource(size)
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
        await _communicate_or_kill(process)
        if process.returncode == 0:
            return True
        logger_app_resize.warning("Failed to set XFCE cursor size.")
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
            await _communicate_or_kill(process_set)
            if process_set.returncode == 0:
                logger_app_resize.info(f"Set GNOME cursor-size to {size}")
                return True
            logger_app_resize.warning("Failed to set GNOME cursor-size.")
        except Exception as e:
            logger_app_resize.warning(
                f"Error trying to set GNOME cursor size via gsettings: {e}"
            )
    if xrdb_ok:
        return True
    logger_app_resize.warning("No supported tool found/worked to set cursor size.")
    return False

async def main() -> None:
    """CLI entry: resize the display to sys.argv[1] ("WxH") and print the result."""
    logging.basicConfig(level=logging.INFO)

    if len(sys.argv) < 2:
        print("USAGE: %s WxH" % sys.argv[0])
        sys.exit(1)
    res = sys.argv[1]
    print(await resize_display(res))

def entrypoint() -> None:
    """Console-script entry point for the resize CLI."""
    asyncio.run(main())

if __name__ == "__main__":
    entrypoint()

def parse_gpu_id(value: Any) -> Optional[int]:
    """Parse the gpu_id setting into an encoder-device selector.

    Returns:
        None for empty/invalid (no explicit pick — pixelflux encodes on ID 0
        or the AUTO_GPU-selected device), -1 for the explicit software-encode
        request, or a device index >= 0.
    """
    value = str(value or "").strip()
    try:
        gid = int(value)
    except ValueError:
        return None
    return gid if gid >= -1 else None


def parse_dri_node_to_index(node_path: str) -> int:
    """Parse a DRI render-node path like '/dev/dri/renderD128' into an index.

    Returns:
        The zero-based index (renderD128 maps to 0), or -1 when the path is
        invalid, malformed, or empty, which disables hardware encoding in
        the capture module.
    """
    logger = logging.getLogger("display_utils")
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


def apply_common_capture_settings(
    cs: Any,
    server: Any,
    *,
    is_wayland: bool,
    display_name: str,
    scale: float,
    framerate: float,
    encoder: str,
    use_cpu: bool,
    cbr: bool,
    bitrate_kbps: float,
    crf: int,
    paintover_crf: int,
    paintover_burst: int,
    fullcolor: bool,
    streaming: bool,
    use_paint_over_quality: bool,
    capture_cursor: bool,
    cursor_size_cap_hint: int = 0,
) -> Any:
    """Assign every CaptureSettings field the WebSocket and WebRTC paths share.

    All shared knobs are set here, once — a knob plumbed into only one path
    is a parity bug. Callers keep the per-mode fields (geometry, output/JPEG
    mode, stripe-header framing).

    ``use_cpu`` is the caller's resolved choice (`effective_use_cpu`); which
    software H.264 encoder then runs is the pixelflux build's. Device
    selection forwards only explicit paths, hardware detection being
    pixelflux's: an ``--encode-dri`` path is authoritative, otherwise
    ``--gpu-id`` picks the encoder device by index (-1 requests software
    encoding) and, unset, pixelflux encodes on ID 0 — the first GPU — unless
    AUTO_GPU affinity aims it elsewhere. The compositor render node is
    distinct: ``--render-dri`` wins, else pixelflux resolves ``--auto-gpu``
    ("true" or a vendor/driver/DT-prefix/PCI-id token) against the machine.
    The cursor size is the Wayland compositor's theme size (X11 sets it on
    the X server itself; `<=0` keeps the theme default), and the cap tracks the
    input handler's DPI-scaled value so pixelflux's XFixes monitor caps shapes
    the same way the python cursor-monitor fallback does. On Wayland the
    capture is bound to its compositor output (`display2` -> output 2), the
    id every per-display IDR/rate/tunable call routes through. The watermark
    is burned into the frame by pixelflux on every backend and stays
    server-side (never broadcast).

    Args:
        cs: The pixelflux CaptureSettings instance to populate.
        server: The parsed Settings object carrying the global knobs; the
            keyword arguments are the per-display ones each path resolves
            from its own client state.

    Returns:
        The same ``cs``, populated.
    """
    cs.target_fps = float(framerate)
    cs.capture_cursor = capture_cursor
    cs.debug_logging = bool(server.debug[0])

    cs.video_crf = crf
    cs.video_paintover_crf = paintover_crf
    cs.video_paintover_burst_frames = paintover_burst
    cs.video_fullcolor = fullcolor
    cs.video_streaming_mode = streaming
    cs.video_fullframe = encoder == "h264enc"
    cs.video_cbr_mode = cbr
    cs.video_bitrate_kbps = int(round(float(bitrate_kbps)))
    # 0 = infinite GOP (on-demand keyframes only).
    cs.keyframe_interval_s = float(getattr(server, "keyframe_interval", 0) or 0)
    # CBR QP clamp (0 = encoder default).
    cs.video_min_qp = int(getattr(server, "video_min_qp", 0) or 0)
    cs.video_max_qp = int(getattr(server, "video_max_qp", 0) or 0)
    cs.use_cpu = bool(use_cpu)
    if cs.use_cpu and encoder != "jpeg":
        from .settings import software_h264_encoder
        logging.getLogger("display_utils").info(
            f"Display '{display_name}' encodes H.264 in software ({software_h264_encoder()}).")

    cs.use_paint_over_quality = use_paint_over_quality
    cs.paint_over_trigger_frames = 15
    cs.damage_block_threshold = 10
    cs.damage_block_duration = 20

    dri_node = str(getattr(server, "encode_dri", "") or "")
    gid = parse_gpu_id(getattr(server, "gpu_id", ""))
    if dri_node:
        cs.encode_node_path = dri_node.encode("utf-8")
        cs.encode_node_index = parse_dri_node_to_index(dri_node)
    elif gid is not None:
        cs.encode_node_index = gid
    render_dri = str(getattr(server, "render_dri", "") or "")
    if render_dri:
        cs.render_node_path = render_dri.encode("utf-8")
    cs.auto_gpu = str(getattr(server, "auto_gpu", "") or "")

    cs.use_wayland = is_wayland
    cs.recording_socket = str(getattr(server, "recording_socket", "") or "")
    cs.wayland_host_display = str(getattr(server, "wayland_host_display", "") or "")
    cs.cursor_size = int(getattr(server, "cursor_size", -1))
    cap = int(cursor_size_cap_hint or 0)
    cs.cursor_size_cap = cap if cap > 0 else max(32, cs.cursor_size)
    if is_wayland:
        cs.scale = scale
        cs.display_id = wayland_output_id(display_name)

    watermark_path = str(getattr(server, "watermark_path", "") or "")
    if watermark_path and os.path.exists(watermark_path):
        cs.watermark_path = watermark_path.encode("utf-8")
        cs.watermark_location_enum = int(getattr(server, "watermark_location", -1))
    return cs


def unpremultiply_rgba(im: Image.Image) -> Image.Image:
    """Convert premultiplied-alpha RGBA to straight alpha (what PNG carries).

    Cursor pixel sources store premultiplied color (XFixes and Xcursor by
    format definition, wl_shm by Wayland convention). The integer math is
    bit-identical to pixelflux's rust ``unpremultiply_rgba`` — floor((c*255 +
    a//2) / a) clamped to 255, alpha-0 color forced to 0 (PIL's I-mode "/" is
    C integer division whose zero-divisor guard yields 0) — so the python
    seed and the rust live path hash a cursor to the same content handle.
    Runs as C-level band arithmetic; a binary-alpha image (most cursors) is
    returned untouched after one histogram.
    """
    if im.mode != "RGBA":
        im = im.convert("RGBA")
    alpha = im.getchannel("A")
    hist = alpha.histogram()
    if not sum(hist[1:255]):
        if not hist[0]:
            return im
        # Binary alpha with transparent pixels: only their color needs zeroing.
        out = Image.new("RGBA", im.size, (0, 0, 0, 0))
        out.paste(im, mask=alpha.point(lambda v: 255 if v else 0))
        return out
    a32 = alpha.convert("I")
    bands = []
    for name in ("R", "G", "B"):
        c32 = im.getchannel(name).convert("I")
        if hasattr(ImageMath, "lambda_eval"):
            band = ImageMath.lambda_eval(
                lambda d: d["min"]((d["c"] * 255 + d["a"] / 2) / d["a"], 255),
                c=c32, a=a32)
        else:
            band = ImageMath.eval("min((c*255 + a/2)/a, 255)", c=c32, a=a32)
        bands.append(band.convert("L"))
    bands.append(alpha)
    return Image.merge("RGBA", bands)


def cursor_content_handle(
    rgba_bytes: bytes, width: int, height: int, hot_x: int, hot_y: int
) -> int:
    """Encoder-independent cursor cache handle.

    A CRC over the straight-alpha pixels and geometry rather than the PNG
    bytes, so the python-xlib seed and the pixelflux live path (different PNG
    encoders) agree on one handle per shape. Downscaled (capped) cursors may
    still differ between the two sources — the resamplers differ — costing
    one redundant client redraw. Never 0: the wire contract reserves handle
    0 for hide.
    """
    meta = struct.pack("<iiii", width, height, hot_x, hot_y)
    return zlib.crc32(meta, zlib.crc32(rgba_bytes)) or 1


def format_pixelflux_cursor(
    msg_type: str,
    data_bytes: Optional[bytes],
    hot_x: int,
    hot_y: int,
    size: int,
) -> Optional[Dict[str, Any]]:
    """Translate a pixelflux cursor event into the client cursor payload.

    Events come from the Wayland compositor or the X11 XFixes monitor.
    "hide" clears the cursor; "png" carries an image; anything else (a
    transient extraction failure) keeps the last good cursor. The handle is
    derived from the decoded pixel content, so a client's cursor cache
    dedupes flips between the same shapes regardless of which source encoded
    them. The payload carries the image's real pixel size, not the nominal
    cursor-size setting: clients scale and place the hotspot against these
    dimensions, and cropped/capped shapes are rarely square.

    Returns:
        The payload dict, or None to skip the event.
    """
    if msg_type == "hide":
        return {
            "curdata": "", "width": 0, "height": 0,
            "hotx": 0, "hoty": 0, "handle": 0,
        }
    if msg_type == "png" and data_bytes:
        width, height = size, size
        try:
            with Image.open(io.BytesIO(data_bytes)) as im:
                rgba = im.convert("RGBA")
                width, height = rgba.width, rgba.height
                handle = cursor_content_handle(
                    rgba.tobytes(), rgba.width, rgba.height, hot_x, hot_y)
        except Exception:
            handle = zlib.crc32(data_bytes) or 1
        return {
            "curdata": base64.b64encode(data_bytes).decode("ascii"),
            "width": width, "height": height,
            "hotx": hot_x, "hoty": hot_y,
            "handle": handle,
        }
    return None

