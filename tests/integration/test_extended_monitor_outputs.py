#!/usr/bin/env python3
"""Every published logical monitor must list the physical output.

RandR lets one output appear in any number of logical monitors, and GTK3's
X11 backend realizes a GdkMonitor only for monitors that carry a live
output: an outputless monitor is invisible to every GTK3 app, so desktops
size themselves to the output-owning region alone and the rest of the root
stays black. Proven from the wire (GetMonitors) rather than the code path:
the server accepts the outputless form without complaint, so only the reply
shows the defect.
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

        layouts = {"primary": dict(PRIMARY), "display2": dict(DISPLAY2)}
        ok = asyncio.run(du.apply_extended_layout(layouts, 1024, 1280))
        state = read_monitors(display_name)
        monitors = state["monitors"]
        res.check("extended layout applied", ok and state["root"] == (1024, 1280),
                  f"root={state['root']}")
        res.check("exactly the two selkies monitors are live",
                  sorted(monitors) == ["selkies-display2", "selkies-primary"],
                  sorted(monitors))
        prim = monitors.get("selkies-primary")
        sec = monitors.get("selkies-display2")
        res.check("rectangles match the layout",
                  prim and prim[:4] == (0, 0, 1024, 640)
                  and sec and sec[:4] == (0, 640, 1024, 640),
                  f"primary={prim} display2={sec}")
        res.check("primary flag sits on selkies-primary",
                  prim and prim[4] and sec and not sec[4], (prim, sec))
        # The heart of it: a monitor with no outputs never becomes a GdkMonitor,
        # so a GTK3 desktop covers only the output-owning region.
        res.check("every monitor lists the physical output",
                  prim and sec and prim[5] and prim[5] == sec[5],
                  f"primary outputs={prim and prim[5]} display2 outputs={sec and sec[5]}")

        asyncio.run(du.replace_selkies_monitors(layouts))
        monitors = read_monitors(display_name)["monitors"]
        republished = [m for m in monitors.values() if m[5]]
        res.check("re-publish keeps the output on every monitor",
                  len(monitors) == 2 and len(republished) == 2,
                  {n: m[5] for n, m in monitors.items()})

        # Monitors outlive the client that set them: a stale outputless set at
        # matching geometry must be re-swapped, not taken as already-live.
        publish_outputless_secondary(display_name)
        stale = read_monitors(display_name)["monitors"].get("selkies-display2")
        res.check("stale outputless secondary is in place", stale and not stale[5], stale)
        asyncio.run(du.replace_selkies_monitors(layouts))
        monitors = read_monitors(display_name)["monitors"]
        res.check("same-geometry publish repairs an outputless live set",
                  all(m[5] for m in monitors.values()) and len(monitors) == 2,
                  {n: m[5] for n, m in monitors.items()})

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
