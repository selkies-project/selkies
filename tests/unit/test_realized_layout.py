#!/usr/bin/env python3
"""Fitting a display layout to the root an X server actually gave.

Both engines compute a layout, ask the server for a framebuffer that covers it,
and then have to cope with a server that gave them something smaller. The rule
lives in one place; this pins every branch of it, including the ones a
Wayland-or-Xvfb test host cannot stage end to end (an all-or-nothing server
never leaves a root that is larger than the primary but smaller than the
union). tests/e2e/test_constrained_root.py drives the wiring against a real
server that refuses to grow.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.display_utils import reconcile_realized_layout


def rect(x: int, y: int, w: int, h: int) -> dict:
    return {"x": x, "y": y, "w": w, "h": h}


def main() -> bool:
    res = H.Results("realized-layout")

    # A root that covers the layout changes nothing.
    layouts = {"primary": rect(0, 0, 1280, 720), "d2": rect(1280, 0, 800, 600)}
    fit = reconcile_realized_layout(layouts, 2080, 720)
    res.check("a root that fits leaves the layout alone",
              fit == ([], False, []) and layouts["d2"] == rect(1280, 0, 800, 600),
              "{} {}".format(fit, layouts))

    # A display whose origin is inside the root but whose extent runs past it
    # keeps its place at a smaller size.
    layouts = {"primary": rect(0, 0, 1280, 720), "d2": rect(1280, 0, 1280, 720)}
    fit = reconcile_realized_layout(layouts, 1920, 720)
    res.check("an overhanging display is clamped, not dropped",
              fit.dropped == [] and fit.clamped == ["d2"]
              and layouts["d2"] == rect(1280, 0, 640, 720),
              "{} {}".format(fit, layouts))

    # Clamped sizes stay even so a subsampled encoder can take them.
    layouts = {"primary": rect(0, 0, 1280, 720), "d2": rect(1280, 0, 800, 600)}
    reconcile_realized_layout(layouts, 1965, 720)
    res.check("a clamped size stays even",
              layouts["d2"]["w"] == 684 and layouts["d2"]["w"] % 2 == 0, layouts["d2"])

    # An origin at or past the edge leaves nothing to show.
    layouts = {"primary": rect(0, 0, 1280, 720), "d2": rect(1280, 0, 1280, 720)}
    fit = reconcile_realized_layout(layouts, 1280, 720)
    res.check("a display starting at the edge is dropped",
              fit.dropped == ["d2"] and "d2" not in layouts, "{} {}".format(fit, layouts))

    layouts = {"primary": rect(0, 0, 1280, 720), "d2": rect(0, 720, 1280, 720)}
    fit = reconcile_realized_layout(layouts, 1280, 720)
    res.check("the same rule applies below the root",
              fit.dropped == ["d2"] and "d2" not in layouts, "{} {}".format(fit, layouts))

    # A primary pushed right by a secondary on its left cannot be left hanging:
    # it goes back to the origin and the arrangement is void.
    layouts = {"primary": rect(1280, 0, 1280, 720), "d2": rect(0, 0, 1280, 720)}
    fit = reconcile_realized_layout(layouts, 1280, 720)
    res.check("an unplaceable primary is re-anchored at the origin",
              fit.reanchored and layouts["primary"]["x"] == 0
              and layouts["primary"]["y"] == 0, "{} {}".format(fit, layouts))
    res.check("re-anchoring drops the secondary that caused it",
              fit.dropped == ["d2"] and "d2" not in layouts, "{} {}".format(fit, layouts))

    # A secondary that still fits beside a re-anchored primary is dropped too:
    # the arrangement it was placed for no longer exists.
    layouts = {"primary": rect(0, 900, 1280, 720), "d2": rect(0, 0, 1280, 720)}
    fit = reconcile_realized_layout(layouts, 1280, 1000)
    res.check("a vertical arrangement re-anchors the same way",
              fit.reanchored and fit.dropped == ["d2"], "{} {}".format(fit, layouts))

    # A primary at the origin is never dropped, only clamped: the session has to
    # stay usable even when the server refuses everything it asked for.
    layouts = {"primary": rect(0, 0, 1920, 1080)}
    fit = reconcile_realized_layout(layouts, 1024, 768)
    res.check("a too-large primary is clamped, never dropped",
              fit.dropped == [] and not fit.reanchored
              and layouts["primary"] == rect(0, 0, 1024, 768), "{} {}".format(fit, layouts))

    # A primary at an offset that still fits is left where it is.
    layouts = {"primary": rect(800, 0, 1280, 720), "d2": rect(0, 0, 800, 720)}
    fit = reconcile_realized_layout(layouts, 2080, 720)
    res.check("an offset primary that fits is not disturbed",
              not fit.reanchored and layouts["primary"]["x"] == 800,
              "{} {}".format(fit, layouts))

    # Degenerate roots must not produce zero-sized or negative rectangles.
    layouts = {"primary": rect(0, 0, 1280, 720)}
    reconcile_realized_layout(layouts, 1, 1)
    res.check("a clamp never yields a rectangle an encoder cannot take",
              layouts["primary"]["w"] >= 2 and layouts["primary"]["h"] >= 2,
              layouts["primary"])

    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
