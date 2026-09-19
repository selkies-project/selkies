#!/usr/bin/env python3
"""Every display is an output of its own where the X server offers pluggable outputs.

A window manager that builds its screens from CRTCs, and a toolkit that
announces a moved monitor only when it is its CRTC, both get an extended
desktop wrong when its displays are logical monitors over one output. On a
server with spare outputs carrying a `Connected` property -- the Xvfb the
images build -- a display is plugged in, given a mode and a position, and
unplugged again, and nothing downstream has to know it is a framebuffer.
Proven from the wire (outputs, CRTCs, monitors), because the requests succeed
either way. A server without such outputs cannot run this; the logical-monitor
layout it gets instead is tests/integration/test_extended_monitor_outputs.py.
"""
import asyncio
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

ONE = {"primary": {"x": 0, "y": 0, "w": 1280, "h": 720}}
RIGHT = {"primary": {"x": 0, "y": 0, "w": 1280, "h": 720},
         "display2": {"x": 1280, "y": 0, "w": 1022, "h": 600}}
LEFT = {"primary": {"x": 1280, "y": 0, "w": 1280, "h": 720},
        "display2": {"x": 0, "y": 0, "w": 1280, "h": 800}}


def outputs(du) -> dict:
    """Connected outputs as `{name: (x, y, w, h)}`, None for one that is off."""
    from selkies.Xlib.ext import randr
    with du._x11_lock:
        d = du._module_display()
        res = randr.get_screen_resources_current(d.screen().root)
        found = {}
        for out_id in res.outputs:
            oi = randr.get_output_info(d, out_id, res.config_timestamp)
            if oi.connection != randr.Connected:
                continue
            ci = randr.get_crtc_info(d, oi.crtc, res.config_timestamp) if oi.crtc else None
            found[oi.name] = (ci.x, ci.y, ci.width, ci.height) if ci and ci.mode else None
        return found


def monitors(du) -> dict:
    from selkies.Xlib.ext import randr
    with du._x11_lock:
        d = du._module_display()
        root = d.screen().root
        return {d.get_atom_name(m.name): (m.x, m.y, m.width_in_pixels, m.height_in_pixels)
                for m in randr.get_monitors(root, is_active=True).monitors}


def rect(layout: dict) -> tuple:
    return (layout["x"], layout["y"], layout["w"], layout["h"])


def main() -> bool:
    res = H.Results("output-layout")
    server, display_name = H.private_x_server(1280, 720)
    os.environ["DISPLAY"] = display_name
    try:
        from selkies import display_utils as du
        du._drop_module_display()
        if not asyncio.run(du.has_pluggable_outputs()):
            res.skip("displays laid out as outputs",
                     "this X server offers no pluggable outputs (needs the Xvfb the images build)")
            return res.summary()

        res.check("one display is one output", asyncio.run(du.apply_output_layout(ONE, 1280, 720))
                  and outputs(du) == {"screen": rect(ONE["primary"])}, outputs(du))

        ok = asyncio.run(du.apply_output_layout(RIGHT, 2304, 720))
        got = outputs(du)
        res.check("a second display is plugged in at its exact size and position",
                  ok and got == {"screen": rect(RIGHT["primary"]), "screen_1": rect(RIGHT["display2"])},
                  got)
        res.check("the server derives one monitor per output and none is defined by hand",
                  monitors(du) == {"screen": rect(RIGHT["primary"]), "screen_1": rect(RIGHT["display2"])},
                  monitors(du))

        asyncio.run(du.apply_output_layout(ONE, 1280, 720))
        res.check("a display that leaves is unplugged", outputs(du) == {"screen": rect(ONE["primary"])},
                  outputs(du))

        third = {"x": 2302, "y": 0, "w": 800, "h": 600}
        asyncio.run(du.apply_output_layout({**RIGHT, "display3": third}, 3104, 720))
        asyncio.run(du.apply_output_layout(
            {"primary": RIGHT["primary"], "display3": {**third, "x": 1280}}, 2080, 720))
        res.check("a display that stays keeps its output when another leaves",
                  outputs(du) == {"screen": rect(RIGHT["primary"]), "screen_2": (1280, 0, 800, 600)},
                  outputs(du))
        asyncio.run(du.apply_output_layout(ONE, 1280, 720))

        started = time.monotonic()
        ok = asyncio.run(du.apply_output_layout(LEFT, 2560, 800))
        took = time.monotonic() - started
        got = outputs(du)
        res.check("a display added beside a primary that moves lands in two steps",
                  ok and took >= du._OUTPUT_SETTLE_S
                  and got == {"screen": rect(LEFT["primary"]), "screen_1": rect(LEFT["display2"])},
                  (round(took, 2), got))

        started = time.monotonic()
        asyncio.run(du.apply_output_layout(RIGHT, 2304, 720))
        took = time.monotonic() - started
        res.check("two live displays are rearranged in one step",
                  took < du._OUTPUT_SETTLE_S
                  and outputs(du) == {"screen": rect(RIGHT["primary"]), "screen_1": rect(RIGHT["display2"])},
                  (round(took, 2), outputs(du)))

        asyncio.run(du.apply_output_layout(LEFT, 2560, 800))
        asyncio.run(du.retire_displays())
        got = outputs(du)
        res.check("retiring the displays leaves the primary alone at the origin",
                  list(got) == ["screen"] and got["screen"][:2] == (0, 0), got)
        realized = asyncio.run(du.resize_display("1024x768"))
        res.check("a plain resize follows a teardown", realized == (1024, 768)
                  and outputs(du) == {"screen": (0, 0, 1024, 768)}, (realized, outputs(du)))

        layouts = {k: dict(v) for k, v in RIGHT.items()}
        ok = asyncio.run(du.apply_extended_layout(layouts, 2304, 720))
        res.check("the shared extended layout takes the outputs where they exist",
                  ok and "screen_1" in outputs(du)
                  and not [n for n in monitors(du) if n.startswith("selkies-")],
                  (outputs(du), monitors(du)))
    finally:
        server.terminate()
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
