#!/usr/bin/env python3
"""A layout that adds a display and moves the primary publishes the move first.

Desktops take in an added screen before a moved one, so a display plugged in
over the place the primary is leaving is judged against a primary that has not
moved yet. This pins which layouts are staged and what the stage holds; the
outputs themselves need an X server that offers them.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.display_utils import output_layout_stage


def rect(x: int, y: int, w: int, h: int) -> dict:
    return {"x": x, "y": y, "w": w, "h": h}


def main() -> bool:
    res = H.Results("output-layout-stage")

    left = {"display2": rect(0, 0, 2560, 1334), "primary": rect(2560, 0, 2560, 1280)}
    res.check("a display added beside a primary that moves stages the move",
              output_layout_stage((0, 0), [], left) == {"primary": rect(2560, 0, 2560, 1280)},
              output_layout_stage((0, 0), [], left))

    right = {"primary": rect(0, 0, 2560, 1280), "display2": rect(2560, 0, 1920, 1046)}
    res.check("a display added beside a primary that stays is published at once",
              output_layout_stage((0, 0), [], right) is None)

    res.check("two displays already live are moved together",
              output_layout_stage((0, 0), ["display2"], left) is None)

    res.check("a display removed needs no stage",
              output_layout_stage((2560, 0), ["display2"], {"primary": rect(0, 0, 2560, 1280)}) is None)

    res.check("a primary that is off has nothing to move",
              output_layout_stage(None, [], left) is None)

    three = {"primary": rect(1920, 0, 2560, 1280), "display3": rect(4480, 0, 1920, 1046),
             "display2": rect(0, 0, 1920, 1046)}
    res.check("a display arriving beside a live one stages that one with the primary",
              output_layout_stage((0, 0), ["display3"], three)
              == {"primary": three["primary"], "display3": three["display3"]},
              output_layout_stage((0, 0), ["display3"], three))

    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
