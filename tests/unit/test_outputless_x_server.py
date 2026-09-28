#!/usr/bin/env python3
"""An X server without a connected RandR output still has a root to stream.

A GPU with no display engine, or a driver told to use no display device,
brings up an X server whose RandR reports no output at all: no mode to set,
no monitor to publish, but a root of a fixed size. The resolution query
reports that root with no output name rather than failing, so a session
streams the desktop at the size it has. The window manager handover reads
the manager's advertised name, which is rarely its binary's name.

Drives the xrandr-parsing fallback with a stand-in xrandr on PATH and the
name matching directly; no X server.
"""
import asyncio
import os
import stat
import sys
import tempfile
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies import display_utils_xrandr as DX

OUTPUTLESS = """Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767
"""
WITH_OUTPUT = """Screen 0: minimum 1 x 1, current 1280 x 720, maximum 8192 x 4096
screen connected primary 1280x720+0+0 0mm x 0mm
   8192x4096      0.00
   1280x720       0.00*
"""
# A driver lists several refreshes under one mode name, marks the current one
# with * and the preferred with +, and nothing stops a mode being named like a
# rate; a line such as the last must neither count nor stall the parse.
MULTI_RATE = """Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767
DP-0 connected primary 1920x1080+0+0 520mm x 290mm
   1920x1080     60.00*+ 119.98    59.94    50.00
   1024x768      75.03    60.00
   1024x768i     43.48
 ! 0.""" + "000." * 40 + """ x
DP-1 disconnected
   800x600       56.25
"""


def fake_xrandr(directory: str, listing: str) -> None:
    path = os.path.join(directory, "xrandr")
    with open(path, "w") as f:
        f.write("#!/bin/sh\ncat <<'LISTING'\n" + listing + "LISTING\n")
    os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


async def scenario(res: "H.Results") -> None:
    bindir = tempfile.mkdtemp(prefix="selkies-xrandr-")
    saved_path = os.environ["PATH"]
    os.environ["PATH"] = bindir + os.pathsep + saved_path
    try:
        fake_xrandr(bindir, OUTPUTLESS)
        curr, fitted, modes, _, name = await DX._get_new_res_xrandr("1x1")
        res.check("a server without an output reports its root size and no output",
                  curr == "1920x1080" and name is None and modes == [], (curr, name, modes))
        fake_xrandr(bindir, WITH_OUTPUT)
        curr, fitted, modes, _, name = await DX._get_new_res_xrandr("2560x1440")
        res.check("one with an output reports the output, its modes, and the fitted size",
                  curr == "1280x720" and name == "screen" and modes == ["1280x720", "8192x4096"]
                  and fitted == "2560x1440", (curr, name, modes, fitted))
        fake_xrandr(bindir, MULTI_RATE)
        started = time.monotonic()
        name, rates = await DX._xrandr_output_rates()
        elapsed = time.monotonic() - started
        res.check("each mode of the connected output keeps every rate it lists, marks aside",
                  name == "DP-0" and rates == {"1920x1080": [60.0, 119.98, 59.94, 50.0],
                                               "1024x768": [75.03, 60.0], "1024x768i": [43.48]},
                  (name, rates))
        res.check("a line that only looks like rates is skipped in linear time", elapsed < 2.0, elapsed)
    finally:
        os.environ["PATH"] = saved_path

    cases = [
        ("kwin_x11", "KWin", True),
        ("openbox", "Openbox", True),
        ("xfwm4", "Xfwm4", True),
        ("labwc", "labwc", True),
        ("kwin_x11", "Openbox", False),
        ("openbox", "", False),
        ("", "KWin", False),
        ("kwin_x11 --replace", "KWin", True),
    ]
    for command, advertised, expected in cases:
        res.check(f"'{command}' {'is' if expected else 'is not'} the manager advertising '{advertised}'",
                  DX.wm_name_matches(command, advertised) is expected, None)


def main() -> "H.Results":
    res = H.Results("outputless-x-server")
    asyncio.run(scenario(res))
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
