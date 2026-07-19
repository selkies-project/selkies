# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import base64
import io
import re
import os
import signal
import struct
import time
import sys
import zlib
from asyncio import subprocess
import asyncio
import threading
from shutil import which

from PIL import Image, ImageMath

from .Xlib import X as x11_X
from .Xlib import display as x11_display
from .Xlib import error as x11_error
from .Xlib.ext import randr

import logging

logger_app_resize = logging.getLogger("resize")
logger_app_resize.setLevel(logging.INFO)

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


async def _communicate_or_kill(process, timeout=5.0):
    """process.communicate() bounded to `timeout` seconds: on expiry the process
    is killed and reaped, and empty output is returned so callers observe the
    nonzero returncode."""
    try:
        return await asyncio.wait_for(process.communicate(), timeout)
    except asyncio.TimeoutError:
        try:
            process.kill()
        except ProcessLookupError:
            pass
        await process.wait()
        return b"", f"timed out after {timeout:g}s".encode()


def _cvt_rb_mode_info(width, height, refresh=60.0):
    """VESA CVT 1.2 reduced-blanking timings for WxH@refresh, mirroring `cvt -r`
    (width rounds up to the 8-pixel cell, vertical stays exact). Returns the
    RandR_ModeInfo fields minus id/name_length."""
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
_x11_conn = None


def _module_display():
    """This module's cached X connection (call under _x11_lock). RandR user
    modes are owned by the connection that created them, so it stays open for
    the process lifetime and retains its resources past disconnect."""
    global _x11_conn
    if _x11_conn is None:
        conn = x11_display.Display()
        conn.set_close_down_mode(x11_X.RetainPermanent)
        try:
            # An alive-but-unresponsive X server (driver hang, a foreign
            # client's server grab) would otherwise block these helpers forever
            # waiting for a reply while they hold _x11_lock — and every retry
            # then parks another executor thread behind that lock. Bound the
            # reply wait so it raises ConnectionClosedError instead; the helpers
            # drop this connection and fall back to their subprocess paths.
            conn.display.blocking_timeout = 15.0
        except Exception:
            pass
        conn.sync()
        _x11_conn = conn
    return _x11_conn


def _drop_module_display():
    """Close and forget the cached connection so the next call reconnects."""
    global _x11_conn
    if _x11_conn is not None:
        try:
            _x11_conn.close()
        except Exception:
            pass
        _x11_conn = None


def _connected_output_state(d):
    """(root, resources, output_id, output_info, {mode_id: name}) for the first
    connected RandR output on connection `d`."""
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


def _sync_query_randr():
    """Blocking RandR query on the module connection: ("WxH" screen size,
    sorted "WxH" mode names of the first connected output, output name)."""
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
            # X protocol errors leave the connection healthy; only a broken
            # connection (which also frees this session's modes) is dropped.
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _ensure_mode_on_display(d, root, res, oi, out_id, names, res_str, w_req, h_req):
    """Mode id + pixel size for the mode named res_str on output out_id,
    creating it from CVT-RB timings and attaching it to the output if absent.
    Modes are owned by the creating connection, so this must run on the
    retained module connection for the mode to outlive the call."""
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


def _sync_ensure_mode(res_str):
    """Blocking ensure-mode on the module connection (no CRTC/screen change)."""
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
            # create/attach errors arrive asynchronously (printed, not raised),
            # so verify the mode really is attached before claiming success.
            _, _, _, oi, _ = _connected_output_state(d)
            if mode_id not in oi.modes:
                raise RuntimeError(f"mode '{res_str}' did not attach (server refused it)")
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


async def ensure_mode(res_str):
    """Ensure a RandR mode named res_str exists and is attached to the first
    connected output, so later xrandr calls can reference it by name. Returns
    True on success; False leaves the caller to its subprocess fallback."""
    try:
        await asyncio.to_thread(_sync_ensure_mode, res_str)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native RandR ensure-mode for '{res_str}' failed ({e}).")
        return False


def _sync_resize_randr(res_str):
    """Blocking RandR resize on the module connection: ensure a mode named
    res_str exists on the first connected output (creating CVT-RB timings when
    absent), activate it, and size the screen to match. Returns (w, h) applied,
    raising on any failure so the caller can fall back to xrandr."""
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


def _resize_on_display(d, res_str, w_req, h_req):
    """The RandR mode-create/activate/screen-size sequence on connection `d`."""
    root, res, out_id, oi, names = _connected_output_state(d)
    # CVT-RB snaps the width up to its 8-pixel cell, so the realized mode can
    # be wider than requested. Key the mode by its REAL geometry: a mode whose
    # name disagrees with its pixel size breaks later xrandr calls that derive
    # framebuffer dimensions from the name.
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
    mm_w = max(1, round(mode_w * 25.4 / 96.0))
    mm_h = max(1, round(mode_h * 25.4 / 96.0))
    rotation = ci.rotation or randr.Rotate_0
    # The screen may not shrink under an active CRTC, so a CRTC that would
    # poke out of the new screen is disabled first (as xrandr does).
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
        # The ungrab must be FLUSHED here, not just queued: when an X error
        # aborts the sequence above, the exception propagates before any later
        # sync would run, and a queued-but-unsent ungrab leaves the whole X
        # server grabbed — every other client (xrandr, capture, WMs) hangs
        # until this process exits.
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


def wayland_output_id(display_id):
    """Stable compositor output id for a display name, shared by both transports:
    'primary' -> 0 (the pixelflux primary output), 'displayN' -> N. A secondary
    name without a numeric suffix falls back to 2 (one secondary is supported)."""
    if not display_id or display_id == "primary":
        return 0
    m = re.search(r"(\d+)$", str(display_id))
    return int(m.group(1)) if m else 2


async def wayland_reposition_primary(module, x, y):
    """Move the pixelflux primary output (id 0) to a union-layout offset — the
    Wayland counterpart of laying the primary at a non-origin xrandr position
    for 'left'/'up' arrangements (and of re-anchoring it at the origin on
    teardown). The compositor remaps the output, its windows, and the input
    offset live; the capture follows without a restart. Returns False when the
    compositor refuses the move or the module lacks the API."""
    mover = getattr(module, "reposition_output", None) if module else None
    if mover is None:
        logger_app_resize.error(
            "pixelflux lacks reposition_output; cannot move the primary output.")
        return False
    try:
        return bool(await asyncio.to_thread(mover, 0, int(x), int(y)))
    except Exception as e:
        logger_app_resize.error(f"Wayland primary reposition to +{x}+{y} failed: {e}")
        return False


def compute_dual_layout(primary_wh, secondary_wh, position):
    """Extended-desktop layout for a primary display plus one secondary placed at
    `position` ("right"/"left"/"up"/"down") — the same placement model the
    websockets transport uses, so a display looks identical over either transport.
    Returns ({display_id_or_primary: {x, y, w, h}}, total_w, total_h) with the
    total width rounded up to a multiple of 8 (xrandr framebuffer alignment);
    the secondary's id is filled in by the caller."""
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


def clamp_primary_feedback(primary_wh, layouts, position):
    """Guard the extended-desktop layout against auto-resize feedback, shared by
    both transports: after the extend, a maximized primary client reports the
    FULL extended screen; re-cropping the primary to that would span both
    monitors and grow the framebuffer without bound. When the primary's
    reported size fills the current extended screen (`layouts`) along the
    secondary's axis, keep the established primary-monitor size instead.
    Returns the (w, h) to lay the primary out with."""
    prev_primary = layouts.get("primary") if layouts else None
    if not prev_primary:
        return primary_wh
    p_w, p_h = primary_wh
    cur_total_w = max(l["x"] + l["w"] for l in layouts.values())
    cur_total_h = max(l["y"] + l["h"] for l in layouts.values())
    if (position in ("right", "left") and p_w >= cur_total_w) or (
        position in ("up", "down") and p_h >= cur_total_h
    ):
        return prev_primary["w"], prev_primary["h"]
    return primary_wh


def parse_resize_dims(res_str):
    """Parse a client resize request "WxH", shared by both transports: cap to
    the 8K ceiling a client may drive the server to, and round down to even
    (YUV 4:2:0 chroma alignment). Returns (w, h), or None when malformed or
    non-positive."""
    try:
        w_str, h_str = res_str.split("x")
        w, h = int(w_str), int(h_str)
    except (ValueError, AttributeError):
        return None
    w, h = min(w, 7680) & ~1, min(h, 4320) & ~1
    if w <= 0 or h <= 0:
        return None
    return w, h


def cursor_size_for_dpi(dpi, base_size):
    """Cursor pixel size scaled from its 96-DPI base (both transports derive
    the X cursor size from the desktop DPI with this)."""
    return max(1, int(round(float(dpi) / 96.0 * base_size)))


def align_dims_16(w, h):
    """force_aligned_resolution: round dimensions down to multiples of 16
    (encoder macroblock alignment), refusing to shrink below 16. Returns the
    dimensions unchanged when alignment would collapse them."""
    aligned_w, aligned_h = w - (w % 16), h - (h % 16)
    if aligned_w >= 16 and aligned_h >= 16:
        return aligned_w, aligned_h
    return w, h


async def _run_xrandr(args, what):
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


def _sync_list_monitors():
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


def _monitor_info(d, out_id, name, x, y, w, h, take_output):
    """RRSetMonitor request dict for logical monitor `name`. take_output
    attaches the (single) connected physical output; an output can belong to
    only one logical monitor, so exactly one monitor per layout takes it."""
    return {
        "name": d.intern_atom(name),
        "primary": False,
        "automatic": False,
        "x": int(x),
        "y": int(y),
        "width_in_pixels": int(w),
        "height_in_pixels": int(h),
        "width_in_millimeters": max(1, round(w * 25.4 / 96.0)),
        "height_in_millimeters": max(1, round(h * 25.4 / 96.0)),
        "crtcs": [out_id] if take_output else [],
    }


def _verify_monitors_on_display(d, expected):
    """Raise unless every (name -> (x, y, w, h)) in `expected` matches a defined
    logical monitor. RRSetMonitor failures (e.g. BadValue for an already-taken
    name) arrive through the async error handler — printed, never raised — so
    callers must verify the result instead of trusting the request."""
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


def _sync_set_monitor(name, x, y, w, h, take_output):
    """Blocking RandR 1.5 set-monitor on the module connection."""
    with _x11_lock:
        try:
            d = _module_display()
            root, _, out_id, _, _ = _connected_output_state(d)
            randr.set_monitor(root, _monitor_info(d, out_id, name, x, y, w, h, take_output))
            d.sync()
            _verify_monitors_on_display(
                d, {name: (int(x), int(y), int(w), int(h))}
            )
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _sync_delete_monitor(name):
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


def _sync_set_output_primary():
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


def _sync_grow_screen(w, h):
    """Blocking grow-only screen resize (never shrinks; no CRTC change)."""
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            geom = root.get_geometry()
            if geom.width >= w and geom.height >= h:
                return
            randr.set_screen_size(
                root, w, h,
                max(1, round(w * 25.4 / 96.0)), max(1, round(h * 25.4 / 96.0)),
            )
            d.sync()
            geom = root.get_geometry()
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise
        # SetScreenSize failures (e.g. beyond the server's size range) surface
        # through the connection's error handler, not as a raise: verify the
        # root actually reached the requested size. The connection is healthy
        # here, so this must not drop the module display.
        if geom.width < w or geom.height < h:
            raise RuntimeError(f"screen is {geom.width}x{geom.height} after grow to {w}x{h}")


def _sync_replace_selkies_monitors(layouts):
    """Blocking swap of ALL selkies-* logical monitors to exactly `layouts`
    (display_id -> {x,y,w,h}) under one X server grab.

    Window managers re-read the monitor set whenever the root emits a core
    ConfigureNotify — which both a screen resize AND deleting/creating the
    monitor that holds the physical output do (the server swaps an automatic
    whole-CRTC monitor in and out). RRSetMonitor cannot replace an existing
    name (BadValue), so a delete gap is unavoidable; the grab makes the swap
    invisible: every event the WM acts on is delivered after the final set is
    in place, so it never tiles against a monitor-less or half-defined screen.
    Foreign (non-selkies) monitors are left untouched."""
    with _x11_lock:
        try:
            d = _module_display()
            root, _, out_id, _, _ = _connected_output_state(d)
            reply = randr.get_monitors(root, is_active=False)
            stale = []
            for m in reply.monitors:
                try:
                    name = d.get_atom_name(m.name)
                except Exception:
                    continue
                if name.startswith("selkies-"):
                    stale.append(name)
            ordered = sorted(layouts.items(), key=lambda kv: kv[0] != "primary")
            d.grab_server()
            try:
                for name in stale:
                    randr.delete_monitor(root, d.intern_atom(name))
                take_output = True
                for display_id, l in ordered:
                    randr.set_monitor(root, _monitor_info(
                        d, out_id, f"selkies-{display_id}",
                        l["x"], l["y"], l["w"], l["h"], take_output,
                    ))
                    take_output = False
                if layouts:
                    randr.set_output_primary(root, out_id)
            finally:
                # Flush the ungrab even when an X error aborts the sequence:
                # a queued-but-unsent ungrab wedges every other X client until
                # this process exits.
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


async def replace_selkies_monitors(layouts, screen_name=None):
    """Swap the selkies-* logical monitor set to exactly `layouts`: native
    grab-protected replace first, per-monitor xrandr fallback second (the
    fallback exposes transient states to the WM, but ends at the same result).
    Returns True when the final monitor set is in place."""
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
    take_output = True
    for display_id, l in sorted(layouts.items(), key=lambda kv: kv[0] != "primary"):
        ok &= await set_logical_monitor(
            f"selkies-{display_id}", l["x"], l["y"], l["w"], l["h"],
            take_output, screen_name=screen_name,
        )
        take_output = False
    await designate_primary_output(screen_name)
    return ok


def _sync_wm_name():
    """Name of the running EWMH window manager ('' when none): root
    _NET_SUPPORTING_WM_CHECK -> child window -> _NET_WM_NAME."""
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            check_atom = d.intern_atom('_NET_SUPPORTING_WM_CHECK')
            prop = root.get_full_property(check_atom, 33)  # XA_WINDOW
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


async def current_wm_name():
    """Name of the running EWMH window manager, '' when undetectable."""
    return await asyncio.to_thread(_sync_wm_name)


async def wait_for_wm(name_substring, timeout=3.0):
    """Wait until the EWMH window manager reports a name containing
    `name_substring` (case-insensitive). Returns success. Used after a WM
    --replace so layout changes are not applied while two window managers
    hand over the selection (the incoming WM snapshots the monitor set it
    starts against)."""
    deadline = asyncio.get_running_loop().time() + timeout
    want = name_substring.lower()
    while True:
        name = await current_wm_name()
        if want in name.lower():
            return True
        if asyncio.get_running_loop().time() >= deadline:
            return False
        await asyncio.sleep(0.15)


async def list_logical_monitors():
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


async def set_logical_monitor(name, x, y, w, h, take_output, screen_name=None):
    """Define/replace logical monitor `name` over the given pixel geometry:
    native RandR 1.5 first, xrandr --setmonitor fallback. Returns success."""
    try:
        await asyncio.to_thread(_sync_set_monitor, name, x, y, w, h, take_output)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native set-monitor '{name}' failed ({e}); using xrandr fallback.")
    geometry = f"{w}/0x{h}/0+{x}+{y}"
    output = screen_name if (take_output and screen_name) else "none"
    return await _run_xrandr(
        ["--setmonitor", name, geometry, output], f"set logical monitor {name}"
    )


async def delete_logical_monitor(name):
    """Delete logical monitor `name`: native first, xrandr fallback. Returns
    success (deleting an absent monitor counts as failure on both paths)."""
    try:
        await asyncio.to_thread(_sync_delete_monitor, name)
        return True
    except Exception as e:
        logger_app_resize.debug(f"Native delete-monitor '{name}' failed ({e}); using xrandr fallback.")
    return await _run_xrandr(["--delmonitor", name], f"delete monitor {name}")


async def designate_primary_output(screen_name=None):
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


async def grow_framebuffer(w, h):
    """Grow-only framebuffer resize, used before live captures re-target so no
    region ever lies outside the root: native first, xrandr --fb fallback."""
    try:
        await asyncio.to_thread(_sync_grow_screen, w, h)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native framebuffer grow failed ({e}); using xrandr fallback.")
    return await _run_xrandr(["--fb", f"{w}x{h}"], "grow framebuffer")


async def list_selkies_monitors():
    """Names of the logical monitors this software created (selkies-*)."""
    return [n for n in await list_logical_monitors() if "selkies-" in n]


async def clear_selkies_monitors():
    """Delete every logical monitor this software created (selkies-*)."""
    for monitor_name in await list_selkies_monitors():
        await delete_logical_monitor(monitor_name)


async def apply_extended_layout(layouts, total_w, total_h):
    """Drive xrandr into an extended-desktop framebuffer covering `layouts`
    (display_id -> {x,y,w,h}): ensure the total mode exists, size the framebuffer,
    and define one selkies-<id> logical monitor per display so window managers
    tile against the per-display regions. Mirrors the websockets engine's command
    sequence. Returns True when the framebuffer was set."""
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
    # Monitors first, at their final rectangles, swapped under a server grab:
    # window managers re-tile maximized windows on every root ConfigureNotify,
    # so no WM-visible stimulus (the swap itself, the resize below) may ever
    # expose a monitor-less or partial set.
    if not await replace_selkies_monitors(layouts, screen_name=screen_name):
        await clear_selkies_monitors()
        return False
    if (curr_res or "").lower().replace(" ", "") != total_mode:
        if not await resize_display(total_mode):
            # Some servers refuse runtime mode creation/attachment but still
            # honor a plain framebuffer grow (RRSetScreenSize): the output
            # keeps its mode while captures and pointer warps address the
            # enlarged root (websockets-engine parity).
            if not await grow_framebuffer(total_w, total_h):
                await clear_selkies_monitors()
                return False
            realized, _, _, _, _ = await get_new_res("1x1")
            try:
                realized_w, realized_h = (int(v) for v in (realized or "").lower().split("x"))
            except (ValueError, AttributeError):
                await clear_selkies_monitors()
                return False
            if realized_w < total_w or realized_h < total_h:
                logger_app_resize.error(
                    f"Framebuffer grow reached {realized_w}x{realized_h}, short of {total_mode}; "
                    "extended layout aborted."
                )
                await clear_selkies_monitors()
                return False
    return True


async def get_new_res(res_str):
    """Current/fitted resolution info for the first connected output:
    (curr_res, fitted res_str, sorted mode names, max res, output name).
    Native RandR query first, xrandr parse as fallback."""
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


async def _get_new_res_xrandr(res_str):
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


async def resize_display(res_str):  # e.g., res_str is "2560x1280"
    """Resizes the display to res_str: native RandR first (mode created from
    CVT-RB timings when absent), xrandr/cvt subprocess chain as fallback.
    Returns the realized (width, height) — CVT cell alignment may make it
    wider than requested — or None on failure. Callers must capture and
    report the realized size, not the request."""
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


async def _resize_display_xrandr(res_str):
    """
    Resizes the display using xrandr to the specified resolution string.
    Adds a new mode via cvt/gtf if the requested mode doesn't exist, naming it
    for the geometry the modeline really carries (cvt snaps width up to the
    8-pixel CVT cell, so it can be wider than requested). Returns the realized
    (width, height), or None on failure.
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

        # Modeline layout: clock hdisp hss hse htot vdisp vss vse vtot flags…
        # The active pixels in the timings are the mode's real geometry; the
        # mode is named for what it IS so the name stays usable for --fb math.
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
                # Cleanup commands
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
    # Force the framebuffer to the mode too. Without --fb, a larger root left
    # over from a prior extended layout keeps the screen oversized, so the new
    # mode lands in the top-left and whole-root capture shows black bars — the
    # native RandR path (set_screen_size) and the websockets path (--fb) both
    # force it, so the fallback must match. The framebuffer is sized from the
    # mode's realized geometry, never the request: a framebuffer narrower than
    # the active mode is rejected outright by xrandr.
    cmd_output = ["xrandr", "--output", screen_name, "--mode", target_mode_to_set,
                  "--fb", f"{realized_w}x{realized_h}"]
    set_mode_proc = await subprocess.create_subprocess_exec(
        *cmd_output,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE
    )
    stdout_set, stderr_set = await _communicate_or_kill(set_mode_proc)
    if set_mode_proc.returncode != 0:
        # A pre-existing mode may carry CVT-snapped geometry wider than its
        # name says (modes created before names tracked geometry); a
        # framebuffer sized from the name is then too small and xrandr rejects
        # the whole command. Retry once with the snapped width.
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


# A modeline is fully determined by (resolution, refresh) — the timings change
# with the refresh rate, so resolution alone does not identify a mode. Successful
# results are memoized on that pair so a size/refresh computed once never
# re-spawns the cvt/gtf subprocess, including when the X mode was later dropped
# and has to be re-created on a subsequent reconfigure.
_MODELINE_CACHE: dict = {}


async def generate_xrandr_gtf_modeline(res_wh_str, refresh_hz=60):
    """Generates an xrandr modeline string using cvt or gtf.

    refresh_hz defaults to 60 (the display mode selkies has always requested);
    it is threaded through — and is part of the cache key — so that if the mode
    is ever generated at another rate, both the timings and the memoization stay
    correct rather than returning a stale 60 Hz modeline for the same size.
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
        except ValueError:
            raise Exception(
                f"Invalid resolution format for modeline generation: {res_wh_str}"
            )
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
        except (FileNotFoundError, Exception):
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
                raise Exception(f"gtf failed: {stderr.decode()}")
            modeline_output = stdout.decode('utf-8')
    except (FileNotFoundError, Exception) as e:
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

# AUTO_GPU render-node detection lives in pixelflux (the device library owns
# hardware detection); selkies forwards only explicit --render-dri/--encode-dri paths.

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
        
        with open(xsettingsd_config_path, "w") as f:
            f.write(config_content)
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
                pid_output = pgrep_stdout.decode().strip()
                if pid_output:
                    pid = int(pid_output.splitlines()[0])
                    try:
                        os.kill(pid, signal.SIGHUP)
                        logger.info(f"Sent SIGHUP to xsettingsd process {pid} to reload config.")
                    except OSError as e:
                        logger.warning(f"Failed to send SIGHUP to xsettingsd process {pid}: {e}")
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

async def _run_mate_gsettings(dpi_value, logger):
    """Helper function to apply DPI via gsettings for MATE."""
    if not which("gsettings"):
        logger.debug("gsettings not found. Skipping MATE gsettings.")
        return False

    mate_settings_succeeded_at_least_once = False

    # MATE: org.mate.interface window-scaling-factor
    try:
        target_mate_scale_float = float(dpi_value) / 96.0
        # window-scaling-factor is integer-only: use it for whole scales, else 1
        # (fractional part handled via font DPI).
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


def _is_wayland():
    """True when selkies runs its Wayland compositor (no X display tools apply).
    Lazy so the selkies-resize CLI works without full settings initialization."""
    try:
        from .settings import settings as _s
        return bool(_s.wayland[0])
    except Exception:
        return False


async def set_dpi(dpi_setting):
    """
    Sets the display DPI using DE-specific methods based on a defined detection order.
    The dpi_setting is expected to be an integer or a string representing an integer.
    """
    if _is_wayland():
        return True
    try:
        dpi_value = int(str(dpi_setting))
        if dpi_value <= 0:
            logger_app_resize.error(f"Invalid DPI value: {dpi_value}. Must be a positive integer.")
            return False
    except ValueError:
        logger_app_resize.error(f"Invalid DPI format: '{dpi_setting}'. Must be convertible to a positive integer.")
        return False

    any_method_succeeded = False
    de_name_for_log = "Unknown" # For logging which DE path was taken

    # DE Detection and Action Order: KDE -> XFCE -> MATE -> i3 -> Openbox
    if which("startplasma-x11"):
        de_name_for_log = "KDE"
        logger_app_resize.info(f"{de_name_for_log} detected. Applying xrdb for DPI {dpi_value}.")
        if await _run_xrdb(dpi_value, logger_app_resize):
            any_method_succeeded = True
    
    elif which("xfce4-session"):
        de_name_for_log = "XFCE"
        logger_app_resize.info(f"{de_name_for_log} detected. Applying xfconf-query for DPI {dpi_value}.")
        if await _run_xfconf(dpi_value, logger_app_resize):
            any_method_succeeded = True
        # For XFCE, only xfconf-query is used to avoid potential double scaling.

    elif which("mate-session"):
        de_name_for_log = "MATE"
        logger_app_resize.info(f"{de_name_for_log} detected. Applying MATE gsettings and xrdb for DPI {dpi_value}.")
        mate_gsettings_success = await _run_mate_gsettings(dpi_value, logger_app_resize)
        # Also apply xrdb for MATE for wider application compatibility / fallback
        xrdb_for_mate_success = await _run_xrdb(dpi_value, logger_app_resize)
        if mate_gsettings_success or xrdb_for_mate_success:
            any_method_succeeded = True

    elif which("i3"):
        de_name_for_log = "i3"
        logger_app_resize.info(f"{de_name_for_log} detected. Applying xrdb for DPI {dpi_value}.")
        if await _run_xrdb(dpi_value, logger_app_resize):
            any_method_succeeded = True
            
    elif which("openbox-session") or which("openbox"): # Check for openbox binary as well
        de_name_for_log = "Openbox"
        logger_app_resize.info(f"{de_name_for_log} detected. Applying xrdb for DPI {dpi_value}.")
        if await _run_xrdb(dpi_value, logger_app_resize):
            any_method_succeeded = True
            
    else:
        de_name_for_log = "Generic/Unknown DE"
        logger_app_resize.info(f"No specific DE session binary found (KDE, XFCE, MATE, i3, Openbox). Attempting generic xrdb as a fallback for DPI {dpi_value}.")
        if await _run_xrdb(dpi_value, logger_app_resize):
            any_method_succeeded = True

    if not any_method_succeeded:
        logger_app_resize.warning(f"No DPI setting method succeeded for DPI {dpi_value} (Attempted for: {de_name_for_log}).")

    return any_method_succeeded

async def _set_xcursor_resource(size):
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


async def set_cursor_size(size):
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

async def main():
    logging.basicConfig(level=logging.INFO)

    if len(sys.argv) < 2:
        print("USAGE: %s WxH" % sys.argv[0])
        sys.exit(1)
    res = sys.argv[1]
    print(await resize_display(res))

def entrypoint():
    asyncio.run(main())

if __name__ == "__main__":
    entrypoint()

def parse_gpu_id(value) -> "int | None":
    """The gpu_id setting as an int: None for empty/invalid (no explicit pick —
    pixelflux encodes on ID 0 or the AUTO_GPU-selected device), -1 for the
    explicit software-encode request, >= 0 for a device index."""
    value = str(value or "").strip()
    try:
        gid = int(value)
    except ValueError:
        return None
    return gid if gid >= -1 else None


def parse_dri_node_to_index(node_path: str) -> int:
    """
    Parses a DRI node path like '/dev/dri/renderD128' into an index (e.g., 0).
    Returns -1 if the path is invalid, malformed, or empty, which
    disables hardware encoding in the capture module.
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
    cs,
    server,
    *,
    is_wayland,
    display_name,
    scale,
    framerate,
    encoder,
    use_cpu,
    cbr,
    bitrate_mbps,
    crf,
    paintover_crf,
    paintover_burst,
    fullcolor,
    streaming,
    use_paint_over_quality,
    capture_cursor,
    cursor_size_cap_hint=0,
):
    """Every CaptureSettings field the WebSocket and WebRTC paths share is
    assigned here, once — a knob plumbed into only one path is a parity bug.
    Callers keep the per-mode fields (geometry, output/JPEG mode, stripe-header
    framing); `server` is the parsed Settings object for the global knobs, the
    keyword arguments are the per-display ones each path resolves from its own
    client state."""
    cs.target_fps = float(framerate)
    cs.capture_cursor = capture_cursor
    cs.debug_logging = bool(server.debug[0])

    cs.video_crf = crf
    cs.video_paintover_crf = paintover_crf
    cs.video_paintover_burst_frames = paintover_burst
    cs.video_fullcolor = fullcolor
    cs.video_streaming_mode = streaming
    cs.video_fullframe = encoder in ("h264enc", "openh264enc")
    cs.use_openh264 = encoder == "openh264enc"
    cs.video_cbr_mode = cbr
    cs.video_bitrate_kbps = int(round(float(bitrate_mbps) * 1000))
    # 0 = infinite GOP (on-demand keyframes only).
    cs.keyframe_interval_s = float(getattr(server, "keyframe_interval", 0) or 0)
    # CBR QP clamp (0 = encoder default).
    cs.video_min_qp = int(getattr(server, "video_min_qp", 0) or 0)
    cs.video_max_qp = int(getattr(server, "video_max_qp", 0) or 0)
    cs.use_cpu = bool(use_cpu) or encoder == "openh264enc"

    cs.use_paint_over_quality = use_paint_over_quality
    cs.paint_over_trigger_frames = 15
    cs.damage_block_threshold = 10
    cs.damage_block_duration = 20

    # Forward explicit --encode-dri as an authoritative PATH; --gpu-id picks the
    # encoder device by index when no path is given. Unset, pixelflux defaults to
    # ID 0 — the first GPU — unless AUTO_GPU affinity aims it elsewhere.
    dri_node = str(getattr(server, "encode_dri", "") or "")
    gid = parse_gpu_id(getattr(server, "gpu_id", ""))
    if dri_node:
        cs.encode_node_path = dri_node.encode("utf-8")
        cs.encode_node_index = parse_dri_node_to_index(dri_node)
    elif gid is not None:
        # >= 0 picks the device; -1 requests software encoding.
        cs.encode_node_index = gid
    # Compositor render node, distinct from the encoder node above: an explicit
    # --render-dri wins; otherwise pixelflux resolves --auto-gpu ("true" or a
    # vendor/driver/DT-prefix/PCI-id token) against the machine itself.
    render_dri = str(getattr(server, "render_dri", "") or "")
    if render_dri:
        cs.render_node_path = render_dri.encode("utf-8")
    cs.auto_gpu = str(getattr(server, "auto_gpu", "") or "")

    cs.use_wayland = is_wayland
    cs.recording_socket = str(getattr(server, "recording_socket", "") or "")
    cs.wayland_host_display = str(getattr(server, "wayland_host_display", "") or "")
    # Wayland compositor cursor-theme size (X11 cursor size is set on the X
    # server itself); <=0 keeps the theme default. The cap tracks the input
    # handler's DPI-scaled value so pixelflux's XFixes monitor caps like the
    # python monitor did.
    cs.cursor_size = int(getattr(server, "cursor_size", -1))
    cap = int(cursor_size_cap_hint or 0)
    cs.cursor_size_cap = cap if cap > 0 else max(32, cs.cursor_size)
    if is_wayland:
        cs.scale = scale
        # Binds this capture to its compositor output ('display2' -> output 2);
        # per-display IDR/rate/tunable calls route through the same id.
        cs.display_id = wayland_output_id(display_name)

    # Server-embedded watermark, burned into the frame by pixelflux on every
    # backend. Kept server-side (never broadcast).
    watermark_path = str(getattr(server, "watermark_path", "") or "")
    if watermark_path and os.path.exists(watermark_path):
        cs.watermark_path = watermark_path.encode("utf-8")
        cs.watermark_location_enum = int(getattr(server, "watermark_location", -1))
    return cs


def unpremultiply_rgba(im):
    """Premultiplied-alpha RGBA image -> straight alpha (what PNG carries).
    Cursor pixel sources store premultiplied color (XFixes and Xcursor by
    format definition, wl_shm by Wayland convention). The integer math is
    bit-identical to pixelflux's rust `unpremultiply_rgba` — floor((c*255 +
    a//2) / a) clamped to 255, alpha-0 color forced to 0 (PIL's I-mode "/" is
    C integer division whose zero-divisor guard yields 0) — so the python seed
    and the rust live path hash a cursor to the same content handle. Runs as
    C-level band arithmetic; a binary-alpha image (most cursors) is returned
    untouched after one histogram."""
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


def cursor_content_handle(rgba_bytes, width, height, hot_x, hot_y):
    """Encoder-independent cursor cache handle: a CRC over the straight-alpha
    pixels and geometry rather than the PNG bytes, so the python-xlib seed and
    the pixelflux live path (different PNG encoders) agree on one handle per
    shape. Downscaled (capped) cursors may still differ between the two
    sources — the resamplers differ — costing one redundant client redraw.
    Never 0: the wire contract reserves handle 0 for hide."""
    meta = struct.pack("<iiii", width, height, hot_x, hot_y)
    return zlib.crc32(meta, zlib.crc32(rgba_bytes)) or 1


def format_pixelflux_cursor(msg_type, data_bytes, hot_x, hot_y, size):
    """pixelflux cursor event (Wayland compositor or X11 XFixes monitor) -> the
    client cursor payload, or None to skip. "hide" clears the cursor; "png"
    carries an image; anything else (a transient extraction failure) keeps the
    last good cursor. The handle is derived from the decoded pixel content, so
    a client's cursor cache dedupes flips between the same shapes regardless of
    which source encoded them."""
    if msg_type == "hide":
        return {
            "curdata": "", "width": 0, "height": 0,
            "hotx": 0, "hoty": 0, "handle": 0,
        }
    if msg_type == "png" and data_bytes:
        # The payload's real pixel size, not the nominal cursor-size setting:
        # clients scale and place the hotspot against these dimensions, and
        # cropped/capped shapes are rarely square.
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


def pixelflux_x11_cursor():
    """True when the installed pixelflux delivers X11 cursor shapes through
    set_cursor_callback (its XFixes monitor thread); selkies then skips its
    python cursor monitor. Older pixelflux stashes the callback without ever
    firing it on X11, so registering is safe either way."""
    try:
        import pixelflux
    except Exception:
        return False
    return bool(getattr(pixelflux, "X11_CURSOR_CALLBACK", False))
