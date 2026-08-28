#!/usr/bin/env python3
"""The X11 overlay recycles the keycode it has gone longest without using.

Keysyms the layout lacks — Unicode, IME output — bind to a pool of spare
keycodes, which a pc105 layout leaves only tens of. A CJK composition spends
several of them per syllable (the jamo, the syllable, the syllable carrying the
next one's lead consonant), so the pool turns over within a few syllables and
every further character rebinds a keycode. Rebinding one a client may still be
translating is how a character arrives as another or not at all, so the slot
that goes is the one longest unused, not merely the one bound longest ago: a
keysym typed on the previous keystroke has to survive the turnover.

Runs against its own X server, since it rewrites the keyboard mapping.

Usage: python3 tests/integration/test_overlay_recycle.py
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))) + "/src")
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

# Hangul syllables, which no layout binds, so each one needs the overlay.
FIRST_SYLLABLE = 0xAC00


def keysym(index: int) -> int:
    return 0x01000000 | (FIRST_SYLLABLE + index)


def run() -> "H.Results":
    res = H.Results("overlay-recycle")
    proc, display = H.private_x_server(640, 480)
    try:
        from selkies.Xlib import display as xdisp
        import selkies.input_handler as ih

        d = xdisp.Display(display)
        kb = ih._XTestKeyboard(d)
        pool = kb._find_spare_keycodes()
        res.check("the layout leaves spare keycodes to bind", len(pool) >= 4, len(pool))
        if len(pool) < 4:
            return res

        # Fill the pool, then use the first binding again: it is now the most
        # recently used, and the second is the least.
        for i in range(len(pool)):
            kb._overlay_keycode(keysym(i))
        first, second = keysym(0), keysym(1)
        first_kc = kb._overlay_keycode(first)
        res.check("a full pool holds every binding", len(kb._overlay) == len(pool),
                  len(kb._overlay))

        # One more keysym has to take a slot from something.
        kb._overlay_keycode(keysym(len(pool)))
        res.check("the keysym used last keeps its keycode",
                  kb._overlay.get(first) == first_kc,
                  f"{kb._overlay.get(first)} was {first_kc}")
        res.check("the longest-unused keysym gave up its slot",
                  second not in kb._overlay, sorted(kb._overlay)[:3])
    finally:
        H.stop_x_server(proc, display)
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
