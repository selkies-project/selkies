# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Display plumbing shared by the WebSocket and WebRTC transports.

Covers X11 RandR display management (resolution modes, extended-desktop
logical monitors, framebuffer sizing), per-desktop-environment DPI
application, Wayland output-id mapping, pixelflux CaptureSettings
population, and cursor payload/cache-handle helpers.

Every RandR operation runs natively on a retained python-xlib connection;
the xrandr/cvt/gtf subprocess each one degrades to when the native call fails
lives in `display_utils_xrandr`. Blocking X work runs on executor threads
(``asyncio.to_thread``) under ``_x11_lock`` so the event loop never waits on
the X server. A helper drops the cached connection on any failure other than
an X protocol error: an ``XError`` leaves the connection healthy, while a
broken connection also frees this session's RandR modes. RandR request
failures arrive through the connection's asynchronous error handler --
printed, never raised -- so every mutation reads its result back before
claiming success.

An extended desktop is laid out one of two ways, decided by what the server
offers (`apply_extended_layout`). Where it has pluggable outputs -- spare
RandR outputs carrying a ``Connected`` property, each with a CRTC of its own,
which the Xvfb the images build provides -- every display is a real output: a
secondary is plugged in, given its exact mode and a position, and unplugged
when its client leaves, so window managers and toolkits meet what hardware
would show them and need no knowledge of the framebuffer
(`_sync_apply_output_layout`). A layout that both adds a display and moves
the primary publishes the move first (`output_layout_stage`). Anywhere else
the server has one CRTC covering the framebuffer and each display is a RandR
1.5 logical monitor over it, which consumers that build their screens from
CRTCs do not follow; that layout, the window-manager restart it needs and the
subprocess fallbacks are `display_utils_xrandr`, which this module imports
only at the point it falls back.

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
from typing import Any, Dict, Iterable, List, NamedTuple, Optional, Tuple, Union

from PIL import Image, ImageMath

from .Xlib import X as x11_X
from .Xlib import Xatom as x11_Xatom
from .Xlib import display as x11_display
from .Xlib import error as x11_error
from .Xlib.ext import randr
from .Xlib.protocol import request as x11_request

import logging

logger_app_resize = logging.getLogger("display")

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


def _first_connected_output(d: x11_display.Display) -> Optional[int]:
    """The first connected RandR output on ``d``, or None where the server has none.

    Modes belong to an output, so the mode paths need one and raise without it.
    Logical monitors do not: a server whose driver reports no display device --
    a GPU with no display engine -- still carries a framebuffer to define them
    over, and they are the only screens the toolkits will find there.
    """
    try:
        _, _, out_id, _, _ = _connected_output_state(d)
        return out_id
    except RuntimeError:
        return None


def _sync_query_randr() -> Tuple[str, List[str], str]:
    """Blocking RandR query on the module connection.

    Returns:
        ``(current "WxH" screen size, sorted "WxH" mode names of the first
        connected output, output name)``.
    """
    with _x11_lock:
        try:
            d = _module_display()
            geom = d.screen().root.get_geometry()
            curr_res = f"{geom.width}x{geom.height}"
            try:
                _, _, _, oi, names = _connected_output_state(d)
            except RuntimeError:
                # A server with no connected output (a GPU without a display
                # engine, a driver told to use none): the root is all there is.
                return curr_res, [], ""
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


#: The output property a server offers on an output a client may plug in and
#: out; its presence is how the capability is detected.
_CONNECTED_PROP = "Connected"
#: The pluggable output each secondary display holds, so a display that stays
#: from one layout to the next keeps its output.
_output_of: Dict[str, int] = {}


def _pluggable_outputs(d: x11_display.Display, res: Any, primary_out: int) -> List[int]:
    """The outputs other than ``primary_out`` that carry `_CONNECTED_PROP`."""
    connected = d.intern_atom(_CONNECTED_PROP)
    return [
        out_id for out_id in res.outputs
        if out_id != primary_out
        and connected in randr.list_output_properties(d, out_id).atoms
    ]


def _sync_has_pluggable_outputs() -> bool:
    """Blocking check for a pluggable output on the module connection."""
    with _x11_lock:
        try:
            d = _module_display()
            _, res, out_id, _, _ = _connected_output_state(d)
            return bool(_pluggable_outputs(d, res, out_id))
        except RuntimeError:
            return False
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            return False


async def has_pluggable_outputs() -> bool:
    """Whether this X server lets a display be added as an output of its own
    (see the module docstring); False on any failure, which leaves the
    logical-monitor layout in charge."""
    return await asyncio.to_thread(_sync_has_pluggable_outputs)


def _exact_mode(
    d: x11_display.Display, root: Any, res: Any, out_id: int,
    names: Dict[int, str], w: int, h: int,
) -> int:
    """Resolve or create a mode of exactly ``w`` x ``h`` on ``out_id``.

    A display's output is the rectangle its client streams, so the width is
    not rounded up to the CVT cell as `_ensure_mode_on_display` does for a
    mode covering the whole framebuffer: two outputs side by side would
    overlap by the difference. The name says which kind it is, because a
    "WxH" name is looked up by both.
    """
    name = f"selkies-{w}x{h}"
    oi = randr.get_output_info(d, out_id, res.config_timestamp)
    mode_id = next((m for m in oi.modes if names.get(m) == name), None)
    if mode_id is not None:
        return mode_id
    mode_id = next((mid for mid, n in names.items() if n == name), None)
    if mode_id is None:
        info = _cvt_rb_mode_info(w, h)
        info["width"] = w
        info["id"] = 0
        info["name_length"] = len(name)
        mode_id = randr.create_mode(root, info, name).mode
        names[mode_id] = name
    randr.add_output_mode(d, out_id, mode_id)
    return mode_id


def _set_crtc(
    d: x11_display.Display, crtc: int, timestamp: int,
    x: int, y: int, mode_id: int, outputs: List[int],
) -> None:
    """Configure a CRTC, unless it is configured so already."""
    ci = randr.get_crtc_info(d, crtc, timestamp)
    if (ci.x, ci.y, ci.mode, list(ci.outputs)) == (x, y, mode_id, outputs):
        return
    status = randr.set_crtc_config(
        d, crtc, timestamp, x, y, mode_id, randr.Rotate_0, outputs).status
    if status != randr.SetConfigSuccess:
        raise RuntimeError(f"SetCrtcConfig returned status {status}")


def _plug(d: x11_display.Display, out_id: int, plugged: bool) -> None:
    randr.change_output_property(
        d, out_id, d.intern_atom(_CONNECTED_PROP), x11_Xatom.INTEGER,
        x11_X.PropModeReplace, (32, [1 if plugged else 0]))


def _sync_apply_output_layout(
    layouts: Dict[str, Dict[str, int]], total_w: int, total_h: int
) -> None:
    """Blocking layout of every display as an output of its own.

    The primary is the output the server started with; each other display
    keeps the pluggable output it holds or takes a free one, since to the
    desktop a display that stays is one monitor, not one unplugged and another
    added. Under one server grab, so a
    window manager acts on the finished arrangement only: outputs no display
    uses are switched off and unplugged, the screen grows to hold the old and
    the new arrangement at once (a CRTC may never poke out of the screen),
    each display's output is plugged in and its CRTC given the display's
    exact mode at the display's position, and the screen shrinks to the
    total. Logical monitors a previous layout defined are deleted, since the
    server derives a monitor from every active CRTC once none is defined.

    Raises:
        RuntimeError: If there are more displays than outputs, or the server
            refused a step; the caller falls back to the logical monitors.
    """
    from .display_utils_xrandr import drop_selkies_monitors

    with _x11_lock:
        try:
            d = _module_display()
            root, res, primary_out, _, names = _connected_output_state(d)
            spare = _pluggable_outputs(d, res, primary_out)
            if len(layouts) > 1 + len(spare):
                raise RuntimeError(
                    f"{len(layouts)} displays but {1 + len(spare)} outputs")
            held = {did: out for did, out in _output_of.items()
                    if did in layouts and out in spare}
            free = [out for out in spare if out not in held.values()]
            assigned = {}
            for did, l in layouts.items():
                out = primary_out if did == "primary" else held.get(did) or free.pop(0)
                assigned[out] = (did, l)
            _output_of.clear()
            _output_of.update(
                {did: out for out, (did, _) in assigned.items() if did != "primary"})
            ts = res.config_timestamp
            dpi = _APPLIED_DPI if _APPLIED_DPI is not None else 96.0

            def size_screen(w: int, h: int) -> None:
                # Only when it changes: Xvfb rebuilds its framebuffer on every
                # screen size set, the same size included, and the desktop is
                # blank until every window repaints.
                geom = root.get_geometry()
                if (geom.width, geom.height) != (w, h):
                    randr.set_screen_size(
                        root, w, h,
                        max(1, round(w * 25.4 / dpi)), max(1, round(h * 25.4 / dpi)))

            d.grab_server()
            try:
                drop_selkies_monitors(d, root)
                geom = root.get_geometry()
                size_screen(max(geom.width, total_w), max(geom.height, total_h))
                for out_id in [primary_out] + spare:
                    oi = randr.get_output_info(d, out_id, ts)
                    if out_id in assigned:
                        continue
                    if oi.crtc:
                        _set_crtc(d, oi.crtc, ts, 0, 0, 0, [])
                    if oi.connection == randr.Connected:
                        _plug(d, out_id, False)
                for out_id, (_, l) in assigned.items():
                    oi = randr.get_output_info(d, out_id, ts)
                    if out_id != primary_out and oi.connection != randr.Connected:
                        _plug(d, out_id, True)
                    crtc = oi.crtc or (oi.crtcs[0] if oi.crtcs else 0)
                    if not crtc:
                        raise RuntimeError(f"output {oi.name} has no usable CRTC")
                    mode_id = _exact_mode(d, root, res, out_id, names, l["w"], l["h"])
                    _set_crtc(d, crtc, ts, l["x"], l["y"], mode_id, [out_id])
                size_screen(total_w, total_h)
                randr.set_output_primary(root, primary_out)
            finally:
                # Flushed, not just queued: an X error aborting the sequence
                # would leave an unsent ungrab and every other X client wedged.
                try:
                    d.ungrab_server()
                    d.flush()
                except Exception:
                    pass
            d.sync()
            for out_id, (did, l) in assigned.items():
                oi = randr.get_output_info(d, out_id, ts)
                ci = randr.get_crtc_info(d, oi.crtc, ts) if oi.crtc else None
                got = (ci.x, ci.y, ci.width, ci.height) if ci else None
                if got != (l["x"], l["y"], l["w"], l["h"]):
                    raise RuntimeError(f"display '{did}' realized as {got}")
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


#: How long the desktop is given to take in a moved primary before a display
#: is plugged in beside it. A toolkit reads the outputs when it handles the
#: event, not when the event was sent, so an arrangement replaced sooner is
#: never seen.
_OUTPUT_SETTLE_S = 0.5


def output_layout_stage(
    primary_at: Optional[Tuple[int, int]], live: Iterable[str],
    layouts: Dict[str, Dict[str, int]],
) -> Optional[Dict[str, Dict[str, int]]]:
    """The arrangement to publish ahead of ``layouts``, where it both moves
    the primary and adds a display.

    A desktop takes in a new screen at once and a moved one later: Qt
    announces an added screen synchronously and a changed geometry through
    its event queue, so whatever reacts to the new screen still holds the
    primary's old rectangle. plasmashell drops a primary that a new display
    covers as redundant, takes it back a moment later, and its desktop view
    never follows its screen again. Moving the primary first, on its own,
    gives every desktop a pass in which nothing is added, and a display
    arriving beside a primary that is already in place is an ordinary
    hotplug. Removing a display needs no such step.

    Args:
        primary_at: Where the primary's CRTC is now, or None when it is off.
        live: The secondary displays that are outputs now.
        layouts: The layout about to be published.

    Returns:
        The displays already live at their new rectangles, or None when the
        layout adds nothing or leaves the primary where it is.
    """
    new = layouts.get("primary")
    live = {"primary", *live}
    if primary_at is None or new is None or live >= set(layouts):
        return None
    if primary_at == (new["x"], new["y"]):
        return None
    return {did: l for did, l in layouts.items() if did in live}


def _sync_output_stage(
    layouts: Dict[str, Dict[str, int]]
) -> Optional[Dict[str, Dict[str, int]]]:
    """Blocking `output_layout_stage` against the outputs as they are now."""
    with _x11_lock:
        try:
            d = _module_display()
            _, res, primary_out, poi, _ = _connected_output_state(d)
            ts = res.config_timestamp
            plugged = {
                out_id for out_id in _pluggable_outputs(d, res, primary_out)
                if randr.get_output_info(d, out_id, ts).connection == randr.Connected}
            live = [did for did, out in _output_of.items() if out in plugged]
            primary_at = None
            if poi.crtc:
                ci = randr.get_crtc_info(d, poi.crtc, ts)
                primary_at = (ci.x, ci.y) if ci.mode else None
            return output_layout_stage(primary_at, live, layouts)
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _sync_retire_outputs() -> None:
    """Blocking return to the one output the server started with: every
    pluggable output switched off and unplugged, the primary's CRTC back at
    the origin. The screen keeps its size, which the resize that follows a
    teardown sets. A no-op on a server without pluggable outputs."""
    with _x11_lock:
        try:
            d = _module_display()
            try:
                _, res, primary_out, poi, _ = _connected_output_state(d)
            except RuntimeError:
                return
            spare = _pluggable_outputs(d, res, primary_out)
            if not spare:
                return
            _output_of.clear()
            ts = res.config_timestamp
            d.grab_server()
            try:
                for out_id in spare:
                    oi = randr.get_output_info(d, out_id, ts)
                    if oi.crtc:
                        _set_crtc(d, oi.crtc, ts, 0, 0, 0, [])
                    if oi.connection == randr.Connected:
                        _plug(d, out_id, False)
                if poi.crtc:
                    ci = randr.get_crtc_info(d, poi.crtc, ts)
                    if ci.mode and (ci.x, ci.y) != (0, 0):
                        _set_crtc(d, poi.crtc, ts, 0, 0, ci.mode, list(ci.outputs))
            finally:
                try:
                    d.ungrab_server()
                    d.flush()
                except Exception:
                    pass
            d.sync()
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            logger_app_resize.warning(f"Could not retire the secondary outputs ({e}).")


async def apply_output_layout(
    layouts: Dict[str, Dict[str, int]], total_w: int, total_h: int
) -> bool:
    """Lay every display out as an output of its own, where the server offers
    pluggable outputs (`_sync_apply_output_layout`). A layout that adds a
    display and moves the primary is published in two steps, the move first
    (`output_layout_stage`).

    Returns:
        True when the arrangement is in place. False -- the server has no
        such outputs, not enough of them, or refused -- leaves the caller to
        the logical-monitor layout.
    """
    if not await has_pluggable_outputs():
        return False
    try:
        stage = await asyncio.to_thread(_sync_output_stage, layouts)
        if stage:
            await asyncio.to_thread(_sync_apply_output_layout, stage, total_w, total_h)
            await asyncio.sleep(_OUTPUT_SETTLE_S)
        await asyncio.to_thread(_sync_apply_output_layout, layouts, total_w, total_h)
        return True
    except Exception as e:
        logger_app_resize.warning(
            f"Could not lay the displays out as outputs ({e}); using logical monitors.")
        await asyncio.to_thread(_sync_retire_outputs)
        return False


#: The compositor screen the primary display shows: the output the session
#: compositor boots on, which is never destroyed. Its size is the primary's,
#: and every secondary display is a screen of its own beside it.
WAYLAND_SCREEN_OUTPUT_ID = 0

#: How long a started capture may go without its first frame before the log
#: says so. A capture emits its first frame at once, damage or none, so a
#: silence this long means the capture never came up, and the page shows
#: only its waiting message.
FIRST_FRAME_WAIT_S = 5.0

#: How many dropped frame ids a transport remembers, so it can hold back what
#: would predict from one. A frame predicts from one of the last few its encoder
#: produced (pixelflux's reference window), so an older id can never be named
#: again and nothing is lost by forgetting it.
LOST_FRAME_MEMORY = 64


def no_first_frame(display_id: str, encoder: str) -> str:
    """The one line a capture that never delivered a frame earns."""
    return (f"Capture for '{display_id}' ({encoder}) has delivered no frame in "
            f"{FIRST_FRAME_WAIT_S:.0f} s; the page is waiting for a stream that is not coming.")


def wayland_output_id(display_id: Optional[str]) -> int:
    """Stable compositor id for a display name, shared by both transports.

    'primary' maps to 1, the view covering screen 0 that its capture binds
    to, and 'displayN' to N, the screen that display owns. A secondary name
    without a numeric suffix falls back to 2, and so does one whose digits
    would land on the primary's screen or view ('display0', 'display1'): a
    client-chosen name must never address the session's own nodes.
    """
    if not display_id or display_id == "primary":
        return 1
    name = str(display_id)
    digits = name[len(name.rstrip("0123456789")):]
    n = int(digits) if 0 < len(digits) <= 9 else 2
    return n if n >= 2 else 2


async def wayland_reposition_primary(module: Any, x: int, y: int) -> bool:
    """Move the pixelflux primary screen (output 0) to a union-layout offset.

    The Wayland counterpart of laying the primary at a non-origin xrandr
    position for 'left'/'up' arrangements (and of re-anchoring it at the
    origin on teardown). The compositor remaps the screen, the view its
    capture binds to, its windows, and the input offset live; the capture
    follows without a restart.

    Returns:
        True on success; False when the compositor refuses the move.
    """
    if module is None:
        return False
    try:
        return bool(await asyncio.to_thread(
            module.reposition_output, WAYLAND_SCREEN_OUTPUT_ID, int(x), int(y)))
    except Exception as e:
        logger_app_resize.error(f"Wayland primary reposition to +{x}+{y} failed: {e}")
        return False


async def wayland_shrink_output(module: Any, output: Tuple, width: int, height: int) -> bool:
    """Shrink a secondary output to no more than ``width`` x ``height`` on each axis.

    The layout pass moves the primary before the captures restart, and the
    compositor refuses a move into room a live output still holds: a secondary
    whose rectangle shrinks (a page changing density, a smaller browser window)
    has to give that room up first. Only the shrink happens here, at the scale
    the output holds; the capture start that follows the primary's move grows
    the output to its whole rectangle and scale, into room the move has left.
    Shared by both transports.

    Args:
        module: A pixelflux capture handle for the compositor.
        output: The output's ``list_outputs`` entry,
            ``(id, x, y, width, height, scale, capturing)``.
        width: The width the layout asks of the output.
        height: The height the layout asks of the output.

    Returns:
        Whether the output now holds no more than the asked rectangle; False
        when the compositor refused the resize, for the caller to recreate the
        output instead.
    """
    oid, _x, _y, cur_w, cur_h, scale = output[:6]
    fit_w, fit_h = min(int(cur_w), int(width)), min(int(cur_h), int(height))
    if (fit_w, fit_h) == (int(cur_w), int(cur_h)):
        return True
    try:
        shrunk = bool(await asyncio.to_thread(
            module.resize_output, oid, fit_w, fit_h, float(scale)))
    except Exception as e:
        logger_app_resize.error(f"Wayland output {oid} shrink to {fit_w}x{fit_h} failed: {e}")
        return False
    if not shrunk:
        logger_app_resize.warning(f"Wayland output {oid} shrink to {fit_w}x{fit_h} refused.")
    return shrunk


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


def _sync_set_screen_size(w: int, h: int) -> Tuple[int, int]:
    """Blocking RRSetScreenSize to exactly ``w`` x ``h`` (no CRTC change).

    What sizes the root of a server with no connected output, where there is
    no mode to set: the framebuffer is all there is and the server sizes it
    freely in both directions.

    Returns:
        The root's size afterwards.
    """
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            dpi_hint = _APPLIED_DPI if _APPLIED_DPI is not None else 96.0
            randr.set_screen_size(
                root, w, h,
                max(1, round(w * 25.4 / dpi_hint)), max(1, round(h * 25.4 / dpi_hint)),
            )
            d.sync()
            geom = root.get_geometry()
            return int(geom.width), int(geom.height)
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


async def grow_framebuffer(w: int, h: int) -> bool:
    """Grow-only framebuffer resize, used before live captures re-target so no
    region ever lies outside the root: native first, xrandr --fb fallback."""
    try:
        await asyncio.to_thread(_sync_grow_screen, w, h)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native framebuffer grow failed ({e}); using xrandr fallback.")
    from .display_utils_xrandr import _run_xrandr

    return await _run_xrandr(["--fb", f"{w}x{h}"], "grow framebuffer")


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
    """Drive the server into an extended desktop covering ``layouts``.

    ``layouts`` maps display id to an `{x, y, w, h}` rectangle. Every display
    becomes an output of its own where the server offers pluggable outputs
    (`apply_output_layout`); anywhere else they become logical monitors over
    its one output (`display_utils_xrandr.apply_monitor_layout`).

    Returns:
        True when the layout is in place. On the logical-monitor path
        ``layouts`` is fitted in place to the root the server realized, so the
        caller reads the rectangles back rather than reusing what it passed.
    """
    if await apply_output_layout(layouts, total_w, total_h):
        return True
    from .display_utils_xrandr import apply_monitor_layout

    return await apply_monitor_layout(layouts, total_w, total_h)


async def retire_displays() -> None:
    """Return the server to the one display it started with: the secondary
    outputs unplugged where displays are outputs (`_sync_retire_outputs`), and
    every logical monitor a fallback layout defined deleted. The screen keeps
    its size, which the resize that follows a teardown sets."""
    from .display_utils_xrandr import clear_selkies_monitors

    await asyncio.to_thread(_sync_retire_outputs)
    await clear_selkies_monitors()


async def get_new_res(res_str: str) -> Tuple[str, str, List[str], str, Optional[str]]:
    """Current/fitted resolution info for the first connected output.

    Native RandR query first, xrandr parse as fallback.

    Returns:
        ``(curr_res, fitted res_str, sorted mode names, max res, output
        name)``; the output name is None when the server has no connected
        output, in which case ``curr_res`` is still the root's size.
    """
    try:
        curr_res, resolutions, screen_name = await asyncio.to_thread(_sync_query_randr)
    except Exception as e:
        logger_app_resize.info(f"Native RandR query failed ({e}); using xrandr fallback.")
        from .display_utils_xrandr import _get_new_res_xrandr

        return await _get_new_res_xrandr(res_str)
    if not screen_name:
        return curr_res, res_str, [], res_str, None
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
    except RuntimeError as e:
        if "no connected RandR output" in str(e):
            # No mode to set, but the framebuffer itself may still be sized.
            try:
                w_req, h_req = (int(p) for p in res_str.split("x"))
                w, h = await asyncio.to_thread(_sync_set_screen_size, w_req, h_req)
            except Exception as size_error:
                logger_app_resize.info(
                    f"No connected RandR output to size for '{res_str}' and the framebuffer "
                    f"cannot be resized ({size_error}); the X server keeps its size."
                )
                return None
            if (w, h) != (w_req, h_req):
                logger_app_resize.info(
                    f"No connected RandR output; the framebuffer stays at {w}x{h} for '{res_str}'."
                )
                return None
            logger_app_resize.info(
                f"No connected RandR output; sized the framebuffer to {w}x{h}."
            )
            return w, h
        logger_app_resize.info(
            f"Native RandR resize for '{res_str}' failed ({e}); falling back to xrandr."
        )
        from .display_utils_xrandr import _resize_display_xrandr

        return await _resize_display_xrandr(res_str)
    except Exception as e:
        logger_app_resize.info(
            f"Native RandR resize for '{res_str}' failed ({e}); falling back to xrandr."
        )
        from .display_utils_xrandr import _resize_display_xrandr

        return await _resize_display_xrandr(res_str)
    logger_app_resize.info(
        f"Successfully applied RandR mode '{res_str}' ({w}x{h})."
    )
    return w, h



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
            points = pixels * 72.0 / max(1, _APPLIED_DPI or desktop_dpi() or 96)
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


def _process_environ(pid: int) -> Dict[str, str]:
    """The environment process ``pid`` runs with, from /proc; empty when it
    cannot be read."""
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            raw = f.read()
    except OSError:
        return {}
    env = {}
    for item in raw.split(b"\0"):
        key, sep, value = item.decode("utf-8", "replace").partition("=")
        if sep:
            env[key] = value
    return env


async def _pids_of(binary: str) -> List[int]:
    """PIDs of the processes running ``binary``, in PID order."""
    if not which("pgrep"):
        return []
    proc = await subprocess.create_subprocess_exec(
        "pgrep", "-x", binary, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    stdout, _ = await _communicate_or_kill(proc)
    return [int(p) for p in stdout.split() if p.isdigit()]


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
        logger.debug(f"Wrote 'Xft.dpi:   {dpi_value}' to {xresources_path_str}.")

        cmd_xrdb = ["xrdb", "-merge", xresources_path_str]
        process = await subprocess.create_subprocess_exec(
            *cmd_xrdb,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        stdout, stderr = await _communicate_or_kill(process)
        
        xrdb_success = process.returncode == 0
        if xrdb_success:
            logger.debug(f"Successfully loaded {xresources_path_str} using xrdb.")
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
        logger.debug(f"Wrote font and DPI settings to {xsettingsd_config_path}.")

        if not which("pgrep"):
            logger.debug("pgrep not found. Skipping xsettingsd reload.")
        else:
            pgrep_proc = await subprocess.create_subprocess_exec(
                "pgrep", "xsettingsd",
                stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            pgrep_stdout, _ = await _communicate_or_kill(pgrep_proc)

            if pgrep_proc.returncode == 0:
                signaled = []
                for line in pgrep_stdout.decode().split():
                    try:
                        os.kill(int(line), signal.SIGHUP)
                        signaled.append(line)
                    except (OSError, ValueError) as e:
                        logger.debug(f"Failed to send SIGHUP to xsettingsd process {line}: {e}")
                if signaled:
                    logger.debug(
                        f"Sent SIGHUP to xsettingsd to reload config ({', '.join(signaled)}).")
                else:
                    logger.warning("No xsettingsd process could be signaled to reload.")
            else:
                logger.debug("xsettingsd process not found. Skipping reload.")
        
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
    pids = await _pids_of("xfce4-session")
    env = _process_environ(pids[0]) if pids else {}
    if "DBUS_SESSION_BUS_ADDRESS" not in env:
        logger.debug("No running xfce4-session with a session bus address.")
        return None
    return env


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
        logger.debug("Found active XFCE session environment. Commands will be executed within this context.")
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
                logger.debug(success_msg)
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
    logger.debug(f"Attempting to set cursor size to: {cursor_size} (based on DPI {dpi_value})")
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
            logger.debug(f"Successfully set MATE window-scaling-factor to {mate_window_scaling_factor} (for DPI {dpi_value}) using gsettings.")
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
            logger.debug(f"Successfully set MATE font-rendering DPI to {dpi_value} using gsettings.")
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

_XFT_DPI = re.compile(r"^\s*Xft\.dpi\s*:\s*(\d+)", re.M)
_XSETTINGS_DPI = re.compile(r"^\s*Xft/DPI\s+(\d+)", re.M)


def applied_dpi() -> Optional[int]:
    """The density the last `set_dpi` gave the desktop; None before the first."""
    return _APPLIED_DPI


def desktop_dpi() -> Optional[int]:
    """The density the X11 desktop has before this process applies one.

    The server's resource database is read first, then the resources this home
    persists: they outlive a restart, and the session may not have merged them
    yet. None when nothing names one. Blocking.
    """
    texts = []
    with _x11_lock:
        try:
            d = _module_display()
            prop = d.screen().root.get_full_property(
                d.get_atom("RESOURCE_MANAGER"), x11_Xatom.STRING)
            if prop is not None:
                raw = prop.value
                texts.append((_XFT_DPI, raw.decode("latin-1") if isinstance(raw, bytes) else str(raw)))
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
    for pattern, name in ((_XFT_DPI, "~/.Xresources"), (_XSETTINGS_DPI, "~/.xsettingsd")):
        try:
            with open(os.path.expanduser(name)) as f:
                texts.append((pattern, f.read()))
        except OSError:
            continue
    for pattern, text in texts:
        found = pattern.findall(text)
        if found:
            return int(found[-1]) // (1024 if pattern is _XSETTINGS_DPI else 1)
    return None


async def restore_dpi(configured: int, locked: bool) -> int:
    """Settle the X11 desktop's density at startup and return the one in effect.

    A density persists in the home across restarts while this process starts
    knowing none, so the desktop is read before anything is written: an
    operator-set value is applied whenever the desktop differs from it, a
    client-driven desktop keeps the density its last page gave it, and either
    way the one in effect is what a page's DPI is then compared against. Unity
    on a fresh home writes nothing.
    """
    current = await asyncio.to_thread(desktop_dpi)
    target = configured if locked else (current or configured)
    if target != 96 or current not in (None, 96):
        await set_dpi(target)
    return target


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
    # there is nothing to gain from recognizing any of them by name.
    if _running_desktop("xfce", "xfce4-session"):
        logger_app_resize.debug(f"XFCE session ({desktop}): applying xfconf-query for DPI {dpi_value}.")
        if await _run_xfconf(dpi_value, logger_app_resize):
            any_method_succeeded = True
    elif _running_desktop("mate", "mate-session"):
        logger_app_resize.debug(f"MATE session ({desktop}): applying gsettings and xrdb for DPI {dpi_value}.")
        mate_gsettings_success = await _run_mate_gsettings(dpi_value, logger_app_resize)
        xrdb_for_mate_success = await _run_xrdb(dpi_value, logger_app_resize)
        if mate_gsettings_success or xrdb_for_mate_success:
            any_method_succeeded = True
    else:
        logger_app_resize.debug(f"{desktop} session: applying xrdb for DPI {dpi_value}.")
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
                logger_app_resize.debug(f"Set GNOME cursor-size to {size}")
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
    logger = logging.getLogger("display")
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
        logger.debug(f"Parsed DRI node '{node_path}' to index {index}.")
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
    cs.video_fullframe = encoder != "h264enc-striped"
    cs.video_cbr_mode = cbr
    cs.video_bitrate_kbps = int(round(float(bitrate_kbps)))
    # 0 = infinite GOP (on-demand keyframes only).
    cs.keyframe_interval_s = float(getattr(server, "keyframe_interval", 0) or 0)
    # CBR QP clamp (0 = encoder default).
    cs.video_min_qp = int(getattr(server, "video_min_qp", 0) or 0)
    cs.video_max_qp = int(getattr(server, "video_max_qp", 0) or 0)
    cs.use_cpu = bool(use_cpu)
    if cs.use_cpu and encoder != "jpeg":
        from .settings import CODEC_LABELS, codec_for_encoder, software_encoders
        codec = codec_for_encoder(encoder)
        library = software_encoders().get(codec, "no software encoder in this pixelflux build")
        logging.getLogger("display").info(
            f"Display '{display_name}' encodes {CODEC_LABELS.get(codec, codec)} in software ({library}).")

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


def release_pixelflux_cursor_callback(capture_module: Any) -> None:
    """Withdraw the cursor callback a stopping capture registered.

    pixelflux's cursor slot is process-wide and outlives the capture that set
    it, so a service that does not let go stays reachable through it until
    something else registers. A build without the withdrawal is left alone:
    the `None` it would store is a `None` its delivery path would call.

    Args:
        capture_module: The pixelflux `ScreenCapture` that registered it.
    """
    clear = getattr(capture_module, "clear_cursor_callback", None)
    if clear is None:
        return
    try:
        clear()
    except Exception:
        logger_app_resize.debug("Cursor callback withdrawal failed", exc_info=True)


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

