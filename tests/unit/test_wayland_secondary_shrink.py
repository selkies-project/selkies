#!/usr/bin/env python3
"""A secondary display that shrinks at its origin keeps the second screen (Wayland).

Every display is a compositor screen, and the compositor refuses to move an
output into room a live one still holds. A left-hand secondary whose rectangle
shrinks -- a page whose density changed, a smaller browser window -- leaves the
primary's new offset inside its old rectangle, so the layout pass has to shrink
that output before it moves the primary, and let the capture start that follows
grow it to its whole rectangle. Driven on both transports' layout passes against
a model of pixelflux's output management with its overlap rule, so what is
asserted is the order of the compositor calls.
"""
import asyncio
import os
import sys
import tempfile
from typing import Dict, List, Tuple

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
os.environ["SELKIES_FILE_MANAGER_PATH"] = tempfile.mkdtemp(prefix="selkies-wl-shrink-")

import helpers as H  # noqa: E402

import selkies.selkies as S  # noqa: E402
import selkies.webrtc_mode as W  # noqa: E402

Rect = Tuple[int, int, int, int]


def overlaps(a: Rect, b: Rect) -> bool:
    return (a[0] < b[0] + b[2] and b[0] < a[0] + a[2]
            and a[1] < b[1] + b[3] and b[1] < a[1] + a[3])


# The primary's capture is a view (output id 1) over its screen (output id 0): it
# moves with the screen and never stands in anything's way.
PRIMARY_VIEW = 1


class FakeCompositor:
    """pixelflux's output management: create and reposition refuse a rectangle that
    overlaps a live screen, a resize takes any size, and every call is recorded."""

    def __init__(self, outputs: Dict[int, Tuple[int, int, int, int, float]]) -> None:
        self.outputs = dict(outputs)
        self.calls: List[tuple] = []
        self.refuse_resize = False

    def list_outputs(self):
        return [(oid, x, y, w, h, scale, True) for oid, (x, y, w, h, scale) in sorted(self.outputs.items())]

    def _free(self, oid: int, rect: Rect) -> bool:
        return not any(overlaps(rect, (x, y, w, h)) for other, (x, y, w, h, _s) in self.outputs.items()
                       if other not in (oid, PRIMARY_VIEW))

    def create_output(self, oid, w, h, x, y, scale):
        self.calls.append(("create", oid, w, h, x, y))
        if oid in self.outputs or not self._free(oid, (x, y, w, h)):
            return False
        self.outputs[oid] = (x, y, w, h, scale)
        return True

    def destroy_output(self, oid):
        self.calls.append(("destroy", oid))
        return self.outputs.pop(oid, None) is not None

    def reposition_output(self, oid, x, y):
        self.calls.append(("move", oid, x, y))
        cur = self.outputs.get(oid)
        if cur is None or not self._free(oid, (x, y, cur[2], cur[3])):
            return False
        self.outputs[oid] = (x, y, cur[2], cur[3], cur[4])
        if oid == 0 and PRIMARY_VIEW in self.outputs:
            view = self.outputs[PRIMARY_VIEW]
            self.outputs[PRIMARY_VIEW] = (x, y, view[2], view[3], view[4])
        return True

    def resize_output(self, oid, w, h, scale):
        self.calls.append(("resize", oid, w, h))
        cur = self.outputs.get(oid)
        if cur is None or self.refuse_resize:
            return False
        self.outputs[oid] = (cur[0], cur[1], w, h, scale)
        return True


# The report's geometry: a left-hand secondary at density 2 that comes down to
# density 1, so the primary moves from 3024 to 1512 into its old rectangle.
PRIMARY = {"x": 1512, "y": 0, "w": 1920, "h": 992}
SECONDARY = {"x": 0, "y": 0, "w": 1512, "h": 882}


def compositor_before() -> FakeCompositor:
    return FakeCompositor({0: (3024, 0, 1920, 992, 1.0), 1: (3024, 0, 1920, 992, 1.0),
                           2: (0, 0, 3024, 1764, 1.0)})


def webrtc_service(module: FakeCompositor) -> W.WebRTCService:
    svc = W.WebRTCService.__new__(W.WebRTCService)
    svc.media_pipeline = type("Pipeline", (), {"capture_module": module, "scale": 1.0})()
    svc.input_handler = None
    svc._wayland_ctl_module = None
    return svc


def websockets_server(module: FakeCompositor) -> Tuple[S.DataStreamingServer, list]:
    srv = S.DataStreamingServer.__new__(S.DataStreamingServer)
    srv._persistent_capture_modules = {"primary": module}
    srv._wayland_ctl_module = None
    srv.input_handler = None
    srv.display_clients = {"primary": {"scale": 1.0}, "display2": {"scale": 1.0}}
    stopped: list = []

    async def stop_capture(display_id):
        stopped.append(display_id)

    async def drop(display_id, reason):
        stopped.append(("dropped", display_id))

    srv._stop_capture_for_display = stop_capture
    srv._drop_wayland_secondary = drop
    return srv, stopped


def moves_and_resizes(calls: list) -> list:
    return [c for c in calls if c[0] in ("resize", "move", "destroy", "create")]


def shrunk_layouts() -> dict:
    return {"primary": dict(PRIMARY), "display2": dict(SECONDARY)}


async def scenario(res: "H.Results") -> None:
    layouts = shrunk_layouts()

    comp = compositor_before()
    ok = await webrtc_service(comp)._apply_wayland_extension("display2", layouts)
    res.check("[webrtc] the shrinking secondary is kept", ok, comp.calls)
    res.check("[webrtc] it shrinks before the primary moves into its old room",
              moves_and_resizes(comp.calls) == [("resize", 2, 1512, 882), ("move", 0, 1512, 0)],
              comp.calls)
    res.check("[webrtc] the primary sits at the new offset beside the shrunken output",
              comp.outputs[0][:2] == (1512, 0) and comp.outputs[2][2:4] == (1512, 882), comp.outputs)

    comp = compositor_before()
    srv, stopped = websockets_server(comp)
    keep = {"primary", "display2"}
    await srv._apply_wayland_output_layout(shrunk_layouts(), keep)
    res.check("[websockets] the shrinking secondary is kept live",
              keep == {"primary", "display2"} and not stopped, (keep, stopped))
    res.check("[websockets] it shrinks before the primary moves, and the primary's screen is fitted",
              moves_and_resizes(comp.calls) == [("resize", 2, 1512, 882), ("move", 0, 1512, 0),
                                                ("resize", 0, 1920, 992)],
              comp.calls)

    # Width down, height up: only the width has to give way before the move; the
    # height grows on the capture start.
    mixed = {"primary": dict(PRIMARY), "display2": {"x": 0, "y": 0, "w": 1512, "h": 2000}}
    comp = compositor_before()
    ok = await webrtc_service(comp)._apply_wayland_extension("display2", mixed)
    res.check("[webrtc] a mixed change shrinks only the axis that gives way",
              ok and moves_and_resizes(comp.calls)[:2] == [("resize", 2, 1512, 1764), ("move", 0, 1512, 0)],
              comp.calls)

    # Growing back: the primary moves out first, nothing is resized here.
    comp = FakeCompositor({0: (1512, 0, 1920, 992, 1.0), 1: (1512, 0, 1920, 992, 1.0),
                           2: (0, 0, 1512, 882, 1.0)})
    grown = {"primary": {"x": 3024, "y": 0, "w": 1920, "h": 992},
             "display2": {"x": 0, "y": 0, "w": 3024, "h": 1764}}
    ok = await webrtc_service(comp)._apply_wayland_extension("display2", grown)
    res.check("[webrtc] a growing secondary waits for the move and its capture start",
              ok and moves_and_resizes(comp.calls) == [("move", 0, 3024, 0)], comp.calls)
    comp = FakeCompositor({0: (1512, 0, 1920, 992, 1.0), 1: (1512, 0, 1920, 992, 1.0),
                           2: (0, 0, 1512, 882, 1.0)})
    srv, stopped = websockets_server(comp)
    keep = {"primary", "display2"}
    await srv._apply_wayland_output_layout({k: dict(v) for k, v in grown.items()}, keep)
    res.check("[websockets] a growing secondary is moved away from, not resized",
              moves_and_resizes(comp.calls) == [("move", 0, 3024, 0), ("resize", 0, 1920, 992)]
              and not stopped, comp.calls)

    # A compositor that will not shrink the output: recreated, on either transport.
    comp = compositor_before()
    comp.refuse_resize = True
    ok = await webrtc_service(comp)._apply_wayland_extension("display2", shrunk_layouts())
    res.check("[webrtc] a refused shrink recreates the output after the move",
              ok and moves_and_resizes(comp.calls) == [("resize", 2, 1512, 882), ("destroy", 2),
                                                       ("move", 0, 1512, 0),
                                                       ("create", 2, 1512, 882, 0, 0)],
              comp.calls)
    comp = compositor_before()
    comp.refuse_resize = True
    srv, stopped = websockets_server(comp)
    keep = {"primary", "display2"}
    await srv._apply_wayland_output_layout(shrunk_layouts(), keep)
    res.check("[websockets] a refused shrink destroys the output and leaves it to the start loop",
              keep == {"primary"} and stopped == ["display2"]
              and moves_and_resizes(comp.calls)[:3] == [("resize", 2, 1512, 882), ("destroy", 2),
                                                        ("move", 0, 1512, 0)],
              (keep, stopped, comp.calls))

    # An unchanged secondary is left alone.
    comp = FakeCompositor({0: (3024, 0, 1920, 992, 1.0), 1: (3024, 0, 1920, 992, 1.0),
                           2: (0, 0, 3024, 1764, 1.0)})
    same = {"primary": {"x": 3024, "y": 0, "w": 1920, "h": 992},
            "display2": {"x": 0, "y": 0, "w": 3024, "h": 1764}}
    ok = await webrtc_service(comp)._apply_wayland_extension("display2", same)
    res.check("[webrtc] an unchanged layout touches no output", ok and not moves_and_resizes(comp.calls),
              comp.calls)


def run() -> "H.Results":
    res = H.Results("wayland-secondary-shrink")
    asyncio.run(scenario(res))
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
