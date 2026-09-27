#!/usr/bin/env python3
"""A resize on a real X server keeps the display's refresh.

A driver lists several modes under one name, each at its own refresh: NVIDIA's
pool holds most sizes at 60 Hz beside the configured mode and lists 1024x768 at
43 Hz ahead of the rest. A vsynced application presents at the refresh, so a
mode taken by name alone caps it at whatever rate the driver sorted first. The
mode is chosen by geometry and refresh instead, at the configured refresh or
the stream's frame rate where that is higher, and a missing one is made at that
rate under a name RandR accepts beside the driver's.

Driven against `display_utils._mode_at` over a mode pool shaped like NVIDIA's,
with the RandR requests stubbed, so what is asserted is which mode serves and
what is created.
"""
import os
import sys
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

import helpers as H  # noqa: E402

import selkies.display_utils as du  # noqa: E402
from selkies.Xlib.ext import randr  # noqa: E402


def mode(mode_id: int, w: int, h: int, hz: float, flags: int = 0) -> SimpleNamespace:
    """A RandR mode whose timings give ``hz``."""
    h_total, v_total = w + 160, h + 30
    v_scan = v_total * (2 if flags & randr.DoubleScan else 1) / (2 if flags & randr.Interlace else 1)
    return SimpleNamespace(id=mode_id, width=w, height=h, h_total=h_total, v_total=v_total,
                           dot_clock=int(round(hz * h_total * v_scan)), flags=flags)


class Server:
    """A mode pool and output, recording what is created and attached."""

    def __init__(self, modes: list, names: dict, on_output: list) -> None:
        self.res = SimpleNamespace(modes=list(modes))
        self.oi = SimpleNamespace(modes=list(on_output))
        self.names = dict(names)
        self.created: list = []
        self.attached: list = []

    def create_mode(self, root, info, name):
        mode_id = 0x900 + len(self.created)
        self.created.append((name, info))
        self.res.modes.append(SimpleNamespace(id=mode_id, **{
            k: info[k] for k in ("width", "height", "h_total", "v_total", "dot_clock", "flags")}))
        return SimpleNamespace(mode=mode_id)

    def add_output_mode(self, d, out_id, mode_id):
        self.attached.append(mode_id)
        self.oi.modes.append(mode_id)

    def pick(self, w: int, h: int, stream_fps, name: str) -> int:
        mode_id, rate = du._mode_at(None, None, self.res, self.oi, 1, self.names, w, h,
                                    du._target_refresh(stream_fps), name)
        self.rate = rate
        return mode_id


def nvidia_pool() -> Server:
    modes = [
        mode(0x1bd, 1024, 768, 43.48),
        mode(0x1be, 1024, 768, 60.0),
        mode(0x1de, 1920, 1080, 119.88),
        mode(0x1df, 1920, 1080, 59.96),
        mode(0x1e1, 1920, 1080, 60.01, randr.DoubleScan),
        mode(0x1ee, 1600, 900, 59.95),
        mode(0x1ef, 1280, 720, 119.9, randr.Interlace),
    ]
    names = {m.id: f"{m.width}x{m.height}" for m in modes}
    return Server(modes, names, [m.id for m in modes])


def run() -> H.Results:
    res = H.Results("display-modes")
    real = (randr.create_mode, randr.add_output_mode)
    try:
        du._configured_refresh = 119.88
        srv = nvidia_pool()
        randr.create_mode, randr.add_output_mode = srv.create_mode, srv.add_output_mode

        res.check("the configured size keeps the configured mode",
                  srv.pick(1920, 1080, 60, "1920x1080") == 0x1de and not srv.created,
                  srv.created)

        got = srv.pick(1600, 900, 60, "1600x900")
        name, info = srv.created[-1] if srv.created else (None, {})
        rate = du._mode_refresh(next(m for m in srv.res.modes if m.id == got))
        res.check("the refresh reported is the created mode's own",
                  abs(srv.rate - rate) < 1e-6, f"{srv.rate:.3f} vs {rate:.3f}")
        res.check("a size the pool holds only at 60 Hz gets a mode at the configured refresh",
                  got != 0x1ee and name == "1600x900_120" and 119.88 <= rate <= 119.88 * 1.01
                  and got in srv.attached and info.get("width") == 1600,
                  f"{name} {rate:.3f} Hz")

        again = srv.pick(1600, 900, 60, "1600x900")
        res.check("that mode serves the next resize to the size",
                  again == got and len(srv.created) == 1, srv.created)

        got = srv.pick(1280, 720, 60, "1280x720")
        res.check("an interlaced mode never serves, even at the rate asked",
                  got != 0x1ef and srv.created[-1][0] == "1280x720_120", srv.created[-1][0])

        got = srv.pick(1920, 1080, 144, "1920x1080")
        rate = du._mode_refresh(next(m for m in srv.res.modes if m.id == got))
        res.check("a stream faster than the configured refresh raises the mode to it",
                  srv.created[-1][0] == "1920x1080_144" and 144 <= rate <= 144 * 1.01,
                  f"{srv.created[-1][0]} {rate:.3f} Hz")

        got = srv.pick(1366, 768, 60, "selkies-1366x768")
        res.check("a free name is taken as asked, exactly as wide as asked",
                  srv.created[-1][0] == "selkies-1366x768" and srv.created[-1][1]["width"] == 1366,
                  srv.created[-1])

        du._configured_refresh = 60.0
        srv = nvidia_pool()
        randr.create_mode, randr.add_output_mode = srv.create_mode, srv.add_output_mode
        res.check("a 60 Hz display skips the 43 Hz mode the driver lists first",
                  srv.pick(1024, 768, 30, "1024x768") == 0x1be and not srv.created, srv.created)

        du._configured_refresh = None
        fb = mode(0x3c, 8192, 4096, 0.0)
        srv = Server([fb], {0x3c: "8192x4096"}, [0x3c])
        randr.create_mode, randr.add_output_mode = srv.create_mode, srv.add_output_mode
        res.check("a framebuffer server's mode without timings serves any refresh",
                  srv.pick(8192, 4096, 120, "8192x4096") == 0x3c and not srv.created, srv.created)
        res.check("with nothing configured the stream's rate is the target, else 60",
                  du._target_refresh(144) == 144 and du._target_refresh(None) == 60.0,
                  (du._target_refresh(144), du._target_refresh(None)))

        other = mode(0x40, 1280, 720, 60.0)
        srv = Server([other], {0x40: "selkies-1280x720"}, [])
        randr.create_mode, randr.add_output_mode = srv.create_mode, srv.add_output_mode
        got = srv.pick(1280, 720, 60, "1280x720")
        res.check("another output's exact mode is not taken for the framebuffer's",
                  got != 0x40 and srv.created[-1][0] == "1280x720", srv.created)
    finally:
        randr.create_mode, randr.add_output_mode = real
        du._configured_refresh = None
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
