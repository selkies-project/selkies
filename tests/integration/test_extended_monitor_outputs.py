#!/usr/bin/env python3
"""Every display stays a logical monitor, with the output where the server allows.

GTK3's X11 backend realizes a GdkMonitor only for a RandR monitor carrying a
live output, so a display published without one is invisible to every GTK app
and their desktops paint and tile short of it. RandR 1.5 has an output belong
to one monitor, though: a server that enforces the rule deletes the monitor a
new one takes the output from, which would drop a display outright. So the
publish asks for the output on all of them and reads the reply back, and the
enforcing shape is driven here through a shim implementing that clause on top
of whichever server the host runs. Proven from the wire (GetMonitors), because
the server accepts either form without complaint.
"""
import asyncio
import os
import shutil
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))) + "/src")
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

PRIMARY = {"x": 0, "y": 0, "w": 1024, "h": 640}
DISPLAY2 = {"x": 0, "y": 640, "w": 1024, "h": 640}
LAYOUTS = {"primary": dict(PRIMARY), "display2": dict(DISPLAY2)}


def read_monitors(display_name: str) -> dict:
    """Live logical monitors as `{name: (x, y, w, h, primary, outputs)}`."""
    from selkies.Xlib import display as x11_display
    from selkies.Xlib.ext import randr

    d = x11_display.Display(display_name)
    try:
        root = d.screen().root
        reply = randr.get_monitors(root, is_active=True)
        monitors = {}
        for m in reply.monitors:
            monitors[d.get_atom_name(m.name)] = (
                m.x, m.y, m.width_in_pixels, m.height_in_pixels,
                bool(m.primary), list(m.crtcs),
            )
        geom = root.get_geometry()
        return {"monitors": monitors, "root": (geom.width, geom.height)}
    finally:
        d.close()


def publish_outputless_secondary(display_name: str) -> None:
    """Redefine selkies-display2 at its rectangle with no outputs listed."""
    from selkies.Xlib import display as x11_display
    from selkies.Xlib.ext import randr

    d = x11_display.Display(display_name)
    try:
        root = d.screen().root
        randr.delete_monitor(root, d.intern_atom("selkies-display2"))
        d.sync()
        randr.set_monitor(root, {
            "name": d.intern_atom("selkies-display2"),
            "primary": False, "automatic": False,
            "x": DISPLAY2["x"], "y": DISPLAY2["y"],
            "width_in_pixels": DISPLAY2["w"], "height_in_pixels": DISPLAY2["h"],
            "width_in_millimeters": 271, "height_in_millimeters": 169,
            "crtcs": [],
        })
        d.sync()
    finally:
        d.close()


def exclusive_outputs(du) -> object:
    """Make RRSetMonitor behave as servers before 21.1 implement it.

    Their clause: each output the new monitor lists is removed from every
    pre-existing monitor, and a monitor left with none is deleted. One output
    exists here, so any monitor holding it loses its last one. The clause is
    about monitors a client defined; the whole-CRTC one the server makes up
    for an unclaimed output is not deletable and goes away by itself.

    Returns:
        The real `set_monitor`, to hand back to `du.randr`.
    """
    real = du.randr.set_monitor

    def enforcing(root, info):
        taken = set(info.get("crtcs") or ())
        if taken:
            for m in du.randr.get_monitors(root, is_active=False).monitors:
                if (not m.automatic and m.name != info["name"]
                        and taken.intersection(m.crtcs)):
                    du.randr.delete_monitor(root, m.name)
        return real(root, info)

    du.randr.set_monitor = enforcing
    return real


def check_shape(res, display_name: str, tag: str, shared: bool) -> None:
    """Both displays live at their rectangles, with the outputs `shared` says."""
    monitors = read_monitors(display_name)["monitors"]
    prim = monitors.get("selkies-primary")
    sec = monitors.get("selkies-display2")
    res.check(f"{tag}: both displays are logical monitors",
              sorted(monitors) == ["selkies-display2", "selkies-primary"],
              sorted(monitors))
    res.check(f"{tag}: rectangles match the layout",
              prim and prim[:4] == (0, 0, 1024, 640)
              and sec and sec[:4] == (0, 640, 1024, 640),
              f"primary={prim} display2={sec}")
    res.check(f"{tag}: the primary flag sits on selkies-primary",
              prim and prim[4] and sec and not sec[4], (prim, sec))
    res.check(f"{tag}: the primary carries the output",
              prim and bool(prim[5]), prim)
    res.check(f"{tag}: the secondary carries the output only where it may",
              sec and bool(sec[5]) == shared, f"display2={sec} shared={shared}")


def main() -> bool:
    res = H.Results("extended-monitor-outputs")
    server, display_name = H.private_x_server(1024, 1280)
    os.environ["DISPLAY"] = display_name
    try:
        from selkies import display_utils as du

        asyncio.run(du.apply_extended_layout({"primary": dict(PRIMARY)}, 1024, 640))
        state = read_monitors(display_name)
        single = state["monitors"].get("selkies-primary")
        res.check("single display: primary published with the output",
                  single and single[4] and single[5], single)

        ok = asyncio.run(du.apply_extended_layout(LAYOUTS, 1024, 1280))
        state = read_monitors(display_name)
        res.check("extended layout applied", ok and state["root"] == (1024, 1280),
                  f"root={state['root']}")
        shared = du._OUTPUT_SHARED
        res.check("the server's answer about sharing one output is recorded",
                  shared in (True, False), shared)
        check_shape(res, display_name, "as published", bool(shared))

        asyncio.run(du.replace_selkies_monitors(LAYOUTS))
        check_shape(res, display_name, "re-published", bool(shared))

        # Monitors outlive the client that set them: a stale outputless set at
        # matching geometry must be re-swapped, not taken as already-live.
        publish_outputless_secondary(display_name)
        stale = read_monitors(display_name)["monitors"].get("selkies-display2")
        res.check("stale outputless secondary is in place", stale and not stale[5], stale)
        asyncio.run(du.replace_selkies_monitors(LAYOUTS))
        check_shape(res, display_name, "after a stale set", bool(shared))

        # The other kind of server: it takes the output from whoever held it,
        # so asking for it on every monitor would publish one display alone.
        asyncio.run(du.clear_selkies_monitors())
        du._OUTPUT_SHARED = None
        real_set = exclusive_outputs(du)
        try:
            asyncio.run(du.apply_extended_layout(LAYOUTS, 1024, 1280))
            res.check("an exclusive-output server is measured as one",
                      du._OUTPUT_SHARED is False, du._OUTPUT_SHARED)
            check_shape(res, display_name, "exclusive outputs", False)
            before = read_monitors(display_name)["monitors"]
            asyncio.run(du.replace_selkies_monitors(LAYOUTS))
            res.check("the settled shape is a no-op to publish again",
                      read_monitors(display_name)["monitors"] == before, before)
        finally:
            du.randr.set_monitor = real_set
            du._OUTPUT_SHARED = None

        if shutil.which("xrandr"):
            _, _, _, _, screen_name = asyncio.run(du.get_new_res("1x1"))
            original = du._sync_set_monitor
            du._sync_set_monitor = lambda *a: (_ for _ in ()).throw(RuntimeError("forced"))
            try:
                ok = asyncio.run(du.set_logical_monitor(
                    "selkies-fallbackprobe", 0, 0, 64, 64, screen_name=screen_name))
            finally:
                du._sync_set_monitor = original
            probe = read_monitors(display_name)["monitors"].get("selkies-fallbackprobe")
            res.check("xrandr fallback also lists the output",
                      ok and probe and probe[5], probe)
            asyncio.run(du.delete_logical_monitor("selkies-fallbackprobe"))
        else:
            res.skip("xrandr fallback also lists the output", "xrandr not installed")

        asyncio.run(du.clear_selkies_monitors())
    finally:
        H.stop_x_server(server, display_name)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
