#!/usr/bin/env python3
"""Windows go where their display goes when a layout moves it.

RandR leaves every window at its root coordinates when a CRTC or a logical
monitor moves, so `display_utils.window_moves` decides, from the displays'
rectangles before and after a layout change, which windows the manager is
asked to move and where: along with a display that moved, onto the primary
from a display that is gone, and nowhere from a display that stayed.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.display_utils import window_moves

PRIMARY = (0, 0, 1280, 720)
LEFT = {"display2": (0, 0, 1024, 768), "primary": (1024, 0, 1280, 720)}
UP = {"display2": (0, 0, 1280, 720), "primary": (0, 720, 1280, 720)}
RIGHT = {"primary": PRIMARY, "display2": (1280, 0, 1024, 768)}


def main() -> "H.Results":
    res = H.Results("window-moves")
    plain = ("plain", 200, 150, 400, 300)
    res.check("a display added on the left carries the primary's windows along",
              window_moves([plain], {"primary": PRIMARY}, LEFT) == [("plain", 1224, 150)],
              window_moves([plain], {"primary": PRIMARY}, LEFT))
    res.check("and one added above moves them down by its height",
              window_moves([plain], {"primary": PRIMARY}, UP) == [("plain", 200, 870)])
    res.check("one added on the right moves nothing",
              window_moves([plain], {"primary": PRIMARY}, RIGHT) == [])
    moved = ("plain", 1224, 150, 400, 300)
    res.check("the left display leaving brings the primary's windows back",
              window_moves([moved], LEFT, {"primary": PRIMARY}) == [("plain", 200, 150)])
    res.check("a display that stays where it is keeps its windows still",
              window_moves([moved], LEFT, {**LEFT, "display2": (0, 0, 800, 600)}) == [])
    home = ("home", 100, 100, 300, 200)
    res.check("the right display leaving moves nothing already on the primary",
              window_moves([home], RIGHT, {"primary": PRIMARY}) == [])
    guest = ("guest", 1380, 100, 300, 200)
    res.check("a window on the departing right display lands on the primary where it sat on its own",
              window_moves([guest], RIGHT, {"primary": PRIMARY}) == [("guest", 100, 100)],
              window_moves([guest], RIGHT, {"primary": PRIMARY}))
    far = ("far", 1280 + 800, 600, 300, 200)
    res.check("and is brought inside a primary too short for its place",
              window_moves([far], RIGHT, {"primary": PRIMARY}) == [("far", 800, 520)],
              window_moves([far], RIGHT, {"primary": PRIMARY}))
    stray = ("stray", 5000, 5000, 100, 100)
    res.check("a window on no display keeps its place",
              window_moves([stray], LEFT, {"primary": PRIMARY}) == [])
    straddling = ("straddling", 900, 100, 400, 300)
    res.check("a window straddling the seam follows the display holding its center, kept inside it",
              window_moves([straddling], LEFT, {"primary": PRIMARY}) == [("straddling", 0, 100)],
              window_moves([straddling], LEFT, {"primary": PRIMARY}))
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
