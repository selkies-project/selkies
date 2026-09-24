# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""The X11 display paths for a server that cannot give a display its own output.

`display_utils` lays an extended desktop out as real RandR outputs wherever
the X server offers pluggable ones, and resizes through native RandR. This
module is everything it falls back to where that is not available, and
nothing here runs on a server where it is:

- An extended desktop as RandR 1.5 logical monitors over the one output a
  framebuffer server has (`apply_monitor_layout`, `replace_selkies_monitors`).
  The whole set is swapped under one server grab, every monitor asks for the
  physical output because GTK realizes a monitor only where one is listed,
  whether the server lets them share it is read back rather than assumed, and
  the swap is announced on an output property because RRSetMonitor emits no
  RandR event of its own.
- The window-manager restart that layout needs (`MultiMonitorWindowManager`):
  a manager that reads the monitor set only as it starts has to be restarted
  once to tile against it. A display that arrives as an output is a hotplug,
  which every manager follows, so there is nothing to restart there.
- The `xrandr`/`cvt`/`gtf` subprocess fallbacks behind the native RandR calls
  (`_resize_display_xrandr`, `_get_new_res_xrandr`,
  `generate_xrandr_gtf_modeline`, `_run_xrandr`), taken only when the native
  call fails.

It shares `display_utils`' retained X connection and lock, so the same rules
hold: blocking X work runs on executor threads under ``_x11_lock``, and a
mutation reads its result back before claiming success.
"""

import asyncio
import re
from asyncio import subprocess
from typing import Any, Dict, List, Optional, Tuple

from .Xlib import X as x11_X
from .Xlib import Xatom as x11_Xatom
from .Xlib import display as x11_display
from .Xlib import error as x11_error
from .Xlib.ext import randr
from .Xlib.ext import res as xres
from .display_utils import (
    Rect,
    _communicate_or_kill,
    _drop_module_display,
    _first_connected_output,
    _module_display,
    _sync_client_windows,
    _sync_follow_display_moves,
    seat_desktop_windows,
    _x11_lock,
    applied_dpi,
    ensure_mode,
    fit_res,
    get_new_res,
    grow_framebuffer,
    has_pluggable_outputs,
    logger_app_resize,
    read_realized_root,
    reconcile_realized_layout,
    resize_display,
)

# Whether this X server lets several logical monitors list the same physical
# output, measured the first time a layout asks for it; None until then. RandR
# 1.5 has an output belong to one monitor and servers before 21.1 enforce it
# (`_sync_set_selkies_layout`).
_OUTPUT_SHARED: Optional[bool] = None
# Bumped on the physical output to announce a monitor-set change.
_MONITOR_SERIAL_ATOM: str = "_SELKIES_MONITOR_SERIAL"
_MONITOR_SERIAL: int = 0


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
    out_id: Optional[int],
    name: str,
    x: int,
    y: int,
    w: int,
    h: int,
    take_output: bool = True,
) -> Dict[str, Any]:
    """Build the RRSetMonitor request dict for logical monitor ``name``.

    ``take_output`` lists the (single) connected physical output on this
    monitor, and a server that has none lists nothing whatever it asks for.
    GTK3's X11 backend realizes a GdkMonitor only for a RandR monitor that
    carries a live output, so a monitor without one is invisible to every GTK
    app and their desktops paint and tile short of that region — which is why
    every monitor asks for it. Whether the server lets them all
    keep it is a property of the server (`_sync_set_selkies_layout`), so a
    caller that has measured a refusal passes False for the rest.

    The primary monitor carries the RandR primary flag: the WM tiles panels
    against it, and the same-set no-op in `_sync_replace_selkies_monitors`
    reads it back to prove the live set already matches. The flag is the
    only mark of the primary in a multi-display set, because Qt also takes
    a monitor listing the primary output for the primary screen, which
    every one of these does.
    """
    return {
        "name": d.intern_atom(name),
        "primary": name == "selkies-primary",
        "automatic": False,
        "x": int(x),
        "y": int(y),
        "width_in_pixels": int(w),
        "height_in_pixels": int(h),
        "width_in_millimeters": max(1, round(w * 25.4 / (applied_dpi() or 96.0))),
        "height_in_millimeters": max(1, round(h * 25.4 / (applied_dpi() or 96.0))),
        "crtcs": [out_id] if (take_output and out_id is not None) else [],
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


def _sync_set_monitor(name: str, x: int, y: int, w: int, h: int,
                      take_output: bool = True) -> None:
    """Blocking RandR 1.5 set-monitor on the module connection."""
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            out_id = _first_connected_output(d)
            randr.set_monitor(root, _monitor_info(d, out_id, name, x, y, w, h, take_output))
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
    """Blocking RandR set-output-primary (first connected output).

    A no-op on a server with no output, where the logical monitors carry the
    primary flag and nothing else can.
    """
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            out_id = _first_connected_output(d)
            if out_id is None:
                return
            randr.set_output_primary(root, out_id)
            d.sync()
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            raise


def _sync_selkies_monitors(
    d: x11_display.Display, root: Any
) -> Dict[str, Tuple[int, int, int, int, bool, bool]]:
    """Live selkies-* monitors as `{name: (x, y, w, h, has_output, primary)}`."""
    monitors = {}
    for m in randr.get_monitors(root, is_active=False).monitors:
        try:
            name = d.get_atom_name(m.name)
        except Exception:
            continue
        if name.startswith("selkies-"):
            monitors[name] = (m.x, m.y, m.width_in_pixels, m.height_in_pixels,
                              bool(m.crtcs), bool(m.primary))
    return monitors


def _sync_display_rects(d: x11_display.Display, root: Any) -> Dict[str, Rect]:
    """Each display's rectangle as the logical monitors have it; with none
    defined, the primary is the framebuffer."""
    rects = {name[len("selkies-"):]: m[:4] for name, m in _sync_selkies_monitors(d, root).items()}
    if not rects:
        geom = root.get_geometry()
        rects["primary"] = (0, 0, int(geom.width), int(geom.height))
    return rects


def _sync_window_snapshot() -> Tuple[list, Dict[str, Rect]]:
    """The manager's clients and the displays' rectangles, read before a
    layout change moves anything."""
    with _x11_lock:
        d = _module_display()
        root = d.screen().root
        return _sync_client_windows(d, root), _sync_display_rects(d, root)


def _sync_follow(snapshot: Tuple[list, Dict[str, Rect]], after: Dict[str, Rect]) -> None:
    """Move the windows of `snapshot` with their displays into ``after``."""
    windows, before = snapshot
    with _x11_lock:
        d = _module_display()
        _sync_follow_display_moves(d, d.screen().root, windows, before, after)
    seat_desktop_windows()


def _sync_windows_to_origin(snapshot: Tuple[list, Dict[str, Rect]]) -> None:
    """With the monitors gone the primary is the framebuffer at the origin:
    move the windows of `snapshot` there with it."""
    if "primary" in snapshot[1]:
        _sync_follow(snapshot, {"primary": (0, 0) + snapshot[1]["primary"][2:]})


async def window_snapshot() -> Tuple[list, Dict[str, Rect]]:
    """The manager's clients and the displays' rectangles as the logical
    monitors have them, read before a layout is swapped in."""
    return await asyncio.to_thread(_sync_window_snapshot)


async def follow_display_moves(snapshot: Tuple[list, Dict[str, Rect]],
                               layouts: Dict[str, Dict[str, int]]) -> None:
    """Move the windows of `snapshot` with their displays into ``layouts``,
    once the framebuffer holds the layout: a manager constrains a move against
    the screen it has, so one asked between the monitor swap and the resize
    that follows it is pushed back inside the old framebuffer."""
    await asyncio.to_thread(_sync_follow, snapshot, {
        did: (int(l["x"]), int(l["y"]), int(l["w"]), int(l["h"])) for did, l in layouts.items()})


def drop_selkies_monitors(d: x11_display.Display, root: Any) -> None:
    """Delete every selkies-* monitor on ``d``; the caller holds ``_x11_lock``.

    For the output layout, which takes over from a monitor layout a previous
    run or a fallback left behind: the server derives a monitor from every
    active CRTC only once none is defined by hand.
    """
    for name in _sync_selkies_monitors(d, root):
        randr.delete_monitor(root, d.intern_atom(name))


def _monitors_match(
    live: Dict[str, Tuple[int, int, int, int, bool, bool]],
    desired: Dict[str, Tuple[int, int, int, int]],
    share_output: bool,
    has_output: bool = True,
) -> bool:
    """Whether the live selkies-* set already is what a publish would define:
    the same rectangles, the primary flag on the primary, and the output where
    this server was measured to keep it."""
    if {name: m[:4] for name, m in live.items()} != desired:
        return False
    if "selkies-primary" in desired and not live["selkies-primary"][5]:
        return False
    if not has_output:
        return all(not m[4] for m in live.values())
    return all(m[4] == (share_output or name == "selkies-primary")
               for name, m in live.items())


def _sync_announce_monitor_change(
    d: x11_display.Display, root: Any, out_id: Optional[int]
) -> None:
    """Emit the RandR event a toolkit re-reads its monitor set on.

    RRSetMonitor emits no RandR event: the server sends core ConfigureNotify on
    the root, which GTK3 discards wherever RandR 1.3 is present
    (`_gdk_x11_screen_size_changed`), re-reading its monitors only for
    RRScreenChangeNotify or RRNotify. So a swap that does not resize the
    framebuffer never reaches a running desktop, which keeps painting and
    constraining windows against the monitors it last saw. An output property
    is the one RRNotify carrying no geometry, physical size, or CRTC of its own,
    and the server emits it even for an unchanged value.

    A server with no output carries no property to bump, so nothing is sent
    there; the framebuffer resize a swap on such a server is part of emits its
    own event, which is what makes the new set visible.

    Advisory, and synced here so a failure surfaces inside it: a set that is
    right but unannounced is no reason to report the swap as failed.
    """
    global _MONITOR_SERIAL
    if out_id is None:
        return
    _MONITOR_SERIAL = (_MONITOR_SERIAL + 1) & 0x7FFFFFFF
    try:
        randr.change_output_property(
            root, out_id, d.intern_atom(_MONITOR_SERIAL_ATOM), x11_Xatom.INTEGER,
            x11_X.PropModeReplace, (32, [_MONITOR_SERIAL]),
        )
        d.sync()
    except Exception as e:
        logger_app_resize.debug(f"Could not announce the monitor-set change ({e}).")


def _sync_set_selkies_layout(
    d: x11_display.Display, root: Any, out_id: Optional[int],
    ordered: List[Tuple[str, Dict[str, int]]], share_output: bool,
) -> Dict[str, Tuple[int, int, int, int, bool, bool]]:
    """Define the whole selkies-* set from scratch; returns what survived.

    RandR 1.5 has an output belong to one logical monitor: defining a monitor
    over an output takes it from whichever monitor held it and deletes that
    monitor once it is left with none. Servers before 21.1 do exactly that, so
    listing the one physical output on every display would leave the last
    display alone. ``share_output`` asks for it on all of them anyway, because
    a server that does not enforce the rule is the only way several displays
    become GdkMonitors at once (see `_monitor_info`); the caller compares the
    returned set against what it asked for and repeats with ``False``, which
    leaves the output on the primary and the rest of the displays invisible to
    GTK apps.
    """
    for name in _sync_selkies_monitors(d, root):
        randr.delete_monitor(root, d.intern_atom(name))
    take_output = True
    for display_id, l in ordered:
        randr.set_monitor(root, _monitor_info(
            d, out_id, f"selkies-{display_id}",
            l["x"], l["y"], l["w"], l["h"], take_output,
        ))
        take_output = share_output
    d.sync()
    return _sync_selkies_monitors(d, root)


def _sync_replace_selkies_monitors(layouts: Dict[str, Dict[str, int]]) -> None:
    """Blocking swap of ALL selkies-* logical monitors to exactly ``layouts``.

    ``layouts`` maps display id to an `{x, y, w, h}` rectangle. The whole swap
    runs under one X server grab: RRSetMonitor cannot replace an existing name
    (BadValue), so a delete gap is unavoidable, and the grab makes it
    invisible: every event a window manager acts on is delivered after the
    final set is in place, so it never tiles against a monitor-less or
    half-defined screen. The set alone announces nothing a toolkit listens to,
    so `_sync_announce_monitor_change` closes the grab. Foreign (non-selkies)
    monitors are left untouched. A request matching the live set returns
    without touching the server: even that swap costs a delete+create and a
    re-tile. A live set whose outputs sit differently than this server was
    measured to allow never counts as a match: monitors outlive the client
    that set them, and a stale outputless set at the right geometry is
    invisible to GTK apps.
    """
    global _OUTPUT_SHARED
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            out_id = _first_connected_output(d)
            desired = {
                f"selkies-{did}": (int(l["x"]), int(l["y"]), int(l["w"]), int(l["h"]))
                for did, l in layouts.items()
            }
            ordered = sorted(layouts.items(), key=lambda kv: kv[0] != "primary")
            share = _OUTPUT_SHARED is not False
            if _monitors_match(
                _sync_selkies_monitors(d, root), desired, share, out_id is not None
            ):
                return
            d.grab_server()
            try:
                live = _sync_set_selkies_layout(d, root, out_id, ordered, share)
                if share and set(live) != set(desired):
                    _OUTPUT_SHARED = False
                    logger_app_resize.info(
                        "This X server keeps a logical monitor's output to itself, so only "
                        "the primary display becomes a monitor toolkits can see; the rest "
                        "of the desktop paints and tiles as if they were not there."
                    )
                    live = _sync_set_selkies_layout(d, root, out_id, ordered, False)
                elif share:
                    _OUTPUT_SHARED = True
                # Qt takes any monitor listing the primary output for the
                # primary screen, and these monitors all list the one output
                # (`_monitor_info`), so with several displays no output is
                # primary and the monitor flag alone names the primary.
                if out_id is not None:
                    randr.set_output_primary(root, out_id if len(layouts) == 1 else 0)
                _sync_announce_monitor_change(d, root, out_id)
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


def _sync_announce_current_monitors() -> None:
    """Announce the live monitor set on the module connection."""
    with _x11_lock:
        try:
            d = _module_display()
            root = d.screen().root
            _sync_announce_monitor_change(d, root, _first_connected_output(d))
        except Exception as e:
            logger_app_resize.debug(f"Could not announce the monitor-set change ({e}).")


async def announce_monitor_change() -> None:
    """Tell the toolkits already running that the monitor set changed.

    The grab-protected replace announces its own; this is for the routes that
    reach a final set some other way (see `_sync_announce_monitor_change`).
    """
    await asyncio.to_thread(_sync_announce_current_monitors)


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
    take_output = True
    for display_id, l in sorted(layouts.items(), key=lambda kv: kv[0] != "primary"):
        ok &= await set_logical_monitor(
            f"selkies-{display_id}", l["x"], l["y"], l["w"], l["h"],
            take_output, screen_name=screen_name,
        )
        take_output = _OUTPUT_SHARED is not False
    await designate_primary_output(screen_name)
    await announce_monitor_change()
    return ok


def _sync_wm_check_window():
    """The running EWMH window manager's check window (root
    _NET_SUPPORTING_WM_CHECK), None when no manager advertises one. Under
    `_x11_lock`."""
    d = _module_display()
    root = d.screen().root
    check_atom = d.intern_atom('_NET_SUPPORTING_WM_CHECK')
    # 33 is XA_WINDOW, the property's type.
    prop = root.get_full_property(check_atom, 33)
    if not prop or not prop.value:
        return None
    return d.create_resource_object('window', int(prop.value[0]))


def _sync_wm_name() -> str:
    """Name of the running EWMH window manager ('' when none): the check
    window's _NET_WM_NAME."""
    with _x11_lock:
        try:
            d = _module_display()
            wm_win = _sync_wm_check_window()
            if wm_win is None:
                return ""
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


def _sync_wm_pid() -> int:
    """Process id of the running EWMH window manager (0 when none): the check
    window's _NET_WM_PID, or, for a manager that sets none (Openbox), what
    the X-Resource extension knows of the client owning that window."""
    with _x11_lock:
        try:
            d = _module_display()
            wm_win = _sync_wm_check_window()
            if wm_win is None:
                return 0
            # 6 is XA_CARDINAL, the property's type.
            pid_prop = wm_win.get_full_property(d.intern_atom('_NET_WM_PID'), 6)
            if pid_prop and pid_prop.value:
                return int(pid_prop.value[0])
            if d.query_extension('X-Resource') is None:
                return 0
            reply = d.res_query_client_ids(
                [{'client': wm_win.id, 'mask': xres.LocalClientPIDMask}])
            for entry in reply.ids:
                if entry.spec.mask & xres.LocalClientPIDMask and entry.value:
                    return int(entry.value[0])
            return 0
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            return 0


#: Options that start a desktop session rather than a window manager: the hook
#: that runs the session's autostart, and the session-manager id the running
#: instance registered. Reusing them would launch every autostart application
#: a second time and claim an id that is already held.
RESTART_DROPS_OPTIONS = ("--startup", "--sm-client-id")


def restart_command(command: List[str]) -> List[str]:
    """`command` reduced to what restarts the manager alone.

    The command line a session started its manager with carries the autostart
    hook (Openbox's `--startup openbox-autostart`), and a restart that keeps
    it runs the desktop's autostart again: a terminal, a panel, and everything
    else the session opens, once per restart. The manager is being restarted
    only to re-read the monitor set, so the session's own options are dropped.

    Args:
        command: The manager's command line, as it was started.

    Returns:
        The command line to restart it with.
    """
    kept: List[str] = []
    drop_value = False
    for arg in command:
        if drop_value:
            drop_value = False
            continue
        if arg.split("=", 1)[0] in RESTART_DROPS_OPTIONS:
            drop_value = "=" not in arg
            continue
        kept.append(arg)
    return kept


#: Window managers measured to read the RandR monitor set only as they start,
#: by the leading run of letters and digits of the name they advertise: a
#: session extending onto a second display restarts one of these so it reads
#: the set the layout published. Others follow monitor changes live, or never
#: read the set at all (kwin_x11 builds its screens from CRTCs, of which a
#: framebuffer server has one).
RESTART_TO_READ_MONITORS = ("openbox", "xfwm4")


class MultiMonitorWindowManager:
    """Restarts X11 window management once a session has two displays, where
    the manager running needs it.

    A window manager that reads the monitor set only as it starts tiles a
    maximized window across the whole framebuffer rather than the per-display
    regions an extended layout defines; restarted, it reads the set the layout
    published. None of this applies where the displays are outputs of their
    own (`has_pluggable_outputs`): a second output arriving is a hotplug, which
    every manager follows. Which managers need that is measured
    (`RESTART_TO_READ_MONITORS`), not configured, and the one running is
    restarted with the command line it was started with plus `--replace`, so
    it keeps the configuration chain its session gave it, less the options
    that would run the session's autostart again (`restart_command`). Both
    transports
    share this state: once per session, since a second restart would take
    window management away from whoever is using it. Wayland sessions manage
    their own windows and never restart.
    """

    def __init__(self) -> None:
        self._done = False

    async def ensure_for(self, display_count: int, is_wayland: bool) -> None:
        """Restart the running window manager if it is one that needs it and
        this session has not yet.

        The replacement is started detached, so it outlives this process's
        session.
        """
        if is_wayland or self._done or display_count <= 1:
            return
        self._done = True
        if await has_pluggable_outputs():
            logger_app_resize.info(
                "Multi-monitor setup: the displays are outputs of their own, which "
                "every window manager follows live; no restart.")
            return
        running = await current_wm_name()
        needing = next((wm for wm in RESTART_TO_READ_MONITORS
                        if wm_name_matches(wm, running)), None)
        if needing is None:
            logger_app_resize.info(
                f"Multi-monitor setup: {running or 'no window manager'} keeps its "
                "monitor set current; no restart.")
            return
        replacing = await current_wm_pid()
        command = restart_command(wm_command(replacing)) or [needing]
        if "--replace" not in command:
            command.append("--replace")
        logger_app_resize.info(
            f"Multi-monitor setup: restarting {needing} to read the monitor set.")
        try:
            await asyncio.create_subprocess_exec(
                *command,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
                start_new_session=True,
            )
        except Exception as e:
            logger_app_resize.error(f"Failed to restart {needing}: {e}")
            return
        # Before the layout applies: a WM snapshotting the monitor set mid-swap
        # re-tiles maximized windows across the whole framebuffer.
        if not await wait_for_wm(needing, replacing):
            logger_app_resize.warning(
                f"{needing} restart not confirmed; applying layout anyway.")


async def current_wm_name() -> str:
    """Name of the running EWMH window manager, '' when undetectable."""
    return await asyncio.to_thread(_sync_wm_name)


async def current_wm_pid() -> int:
    """Process id the running EWMH window manager advertises, 0 when none."""
    return await asyncio.to_thread(_sync_wm_pid)


def wm_command(pid: int) -> List[str]:
    """The command line process `pid` was started with, from /proc; empty
    when there is no such process to read."""
    if pid <= 0:
        return []
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            raw = f.read()
    except OSError:
        return []
    return [a.decode("utf-8", "replace") for a in raw.split(b"\0") if a]


def wm_name_matches(command_name: str, wm_name: str) -> bool:
    """Whether the window manager advertising ``wm_name`` is the one the
    command ``command_name`` starts.

    A window manager rarely advertises its binary's name: kwin_x11 answers
    "KWin", Openbox "Openbox". Both are reduced to their leading run of
    letters and digits, case aside, and one has to begin with the other.
    """
    def stem(name: str) -> str:
        m = re.match(r"[a-z0-9]+", name.strip().lower())
        return m.group(0) if m else ""

    want, have = stem(command_name), stem(wm_name)
    return bool(want) and bool(have) and (want.startswith(have) or have.startswith(want))


async def wait_for_wm(name_substring: str, replacing: int = 0,
                      timeout: float = 3.0) -> bool:
    """Wait until the window manager ``name_substring`` starts is the one
    running (`wm_name_matches`) and, given the process id of the one it
    replaces, no longer that one.

    Used after a WM --replace so layout changes are not applied while two
    window managers hand over the selection: the incoming WM snapshots the
    monitor set it starts against, and the outgoing one answers to the same
    name until it is gone.

    Returns:
        True when the manager matched within ``timeout`` seconds.
    """
    deadline = asyncio.get_running_loop().time() + timeout
    while True:
        if wm_name_matches(name_substring, await current_wm_name()) and (
                not replacing or await current_wm_pid() not in (0, replacing)):
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
    take_output: bool = True,
    screen_name: Optional[str] = None,
) -> bool:
    """Define/replace logical monitor ``name`` over the given pixel geometry:
    native RandR 1.5 first, xrandr --setmonitor fallback. ``take_output`` lists
    the physical output on the monitor (see `_monitor_info`); the fallback also
    goes outputless when no output name is known. Returns success."""
    try:
        await asyncio.to_thread(_sync_set_monitor, name, x, y, w, h, take_output)
        return True
    except Exception as e:
        logger_app_resize.info(f"Native set-monitor '{name}' failed ({e}); using xrandr fallback.")
    geometry = f"{w}/0x{h}/0+{x}+{y}"
    output = screen_name if (take_output and screen_name) else "none"
    return await _run_xrandr(
        ["--setmonitor", name, geometry, output],
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


async def list_selkies_monitors() -> List[str]:
    """Names of the logical monitors this software created (selkies-*)."""
    return [n for n in await list_logical_monitors() if "selkies-" in n]


def _sync_restore_framebuffer_monitor() -> bool:
    """Define one monitor covering the framebuffer, where the server has no output.

    Returns whether one was defined. A server whose driver exposes an output
    keeps a screen of its own once the layout monitors are gone, so nothing is
    defined there.
    """
    with _x11_lock:
        try:
            d = _module_display()
            if _first_connected_output(d) is not None:
                return False
            root = d.screen().root
            geom = root.get_geometry()
            randr.set_monitor(root, _monitor_info(
                d, None, "selkies-primary", 0, 0, int(geom.width), int(geom.height), False))
            d.sync()
            return True
        except Exception as e:
            if not isinstance(e, x11_error.XError):
                _drop_module_display()
            logger_app_resize.info(f"Could not define a monitor over the framebuffer ({e}).")
            return False


async def clear_selkies_monitors() -> None:
    """Delete every logical monitor this software created (selkies-*).

    A server with no connected output has no screen of its own to fall back to:
    its monitors are the only ones the toolkits see, so one covering the
    framebuffer takes the layout's place rather than leaving the desktop with
    nowhere to put a window. The primary's windows follow it back to the
    origin, and a departed display's onto it.
    """
    names = await list_selkies_monitors()
    snapshot = await window_snapshot() if names else None
    for monitor_name in names:
        await delete_logical_monitor(monitor_name)
    restored = await asyncio.to_thread(_sync_restore_framebuffer_monitor)
    if snapshot is not None:
        await asyncio.to_thread(_sync_windows_to_origin, snapshot)
    if names and not restored:
        await announce_monitor_change()


async def apply_monitor_layout(
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
    The windows follow their displays last of all, once the framebuffer holds
    the layout (`follow_display_moves`).

    Returns:
        True when the framebuffer and monitors were set. ``layouts`` is fitted
        in place to the root the server actually produced, so the caller must
        read the rectangles back rather than reuse the ones it passed in: a
        display kept at a smaller size carries the smaller one, and a display
        that could not be placed at all is gone from the mapping. False when
        nothing could be laid out; the monitors are torn down.
    """
    total_mode = f"{total_w}x{total_h}"
    snapshot = await window_snapshot()
    curr_res, _, available, _, screen_name = await get_new_res(total_mode)
    if not screen_name:
        # No output means no mode to create or set, and the framebuffer alone is
        # what the resize below sizes; the monitors are still what gives the
        # toolkits their screens, so the layout goes on.
        logger_app_resize.info(
            "No connected RandR output on this X server; the desktop is laid out on the "
            "framebuffer alone.")
    elif total_mode not in (available or []):
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
        await follow_display_moves(snapshot, layouts)
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
    await follow_display_moves(snapshot, layouts)
    return True


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
        # The root's size heads the listing, whether or not an output follows
        current_match = current_pat.match(line)
        if current_match:
            curr_res = current_match.group(1).replace(" ", "")
        screen_match = screen_pat.match(line)
        if screen_match:
            if screen_name is None:
                screen_name = screen_match.group(1)
            current_screen_modes_started = screen_name == screen_match.group(1)
        if current_screen_modes_started:
            res_match = res_pat.match(line.strip())
            if res_match:
                resolutions.append(res_match.group(1))
    if not screen_name:
        logger_app_resize.debug(
            "No connected RandR output on this X server; the root is all there is."
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
        logger_app_resize.debug(
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
            logger_app_resize.debug(f"Successfully ran: {' '.join(cmd_new)}")

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
            logger_app_resize.debug(f"Successfully ran: {' '.join(cmd_add)}")

    logger_app_resize.debug(
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
