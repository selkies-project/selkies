#!/usr/bin/env python3
"""The truthful verdict a Wayland capture start produces.

A Wayland ``start_capture`` only enqueues a compositor command, so its real
outcome is not known when the call returns. The server reads it back through the
geometry barrier plus ``capture_state``: a start that left no live pipeline (a
missing display id, a dead host compositor) is a failure the caller must see and
surface to the client, while a live capture that came up degraded (a hardware
encoder that fell back to CPU, a host connect that failed into local
compositing) is a caveat that is logged but still streams.

This pins ``_wayland_start_verdict`` and ``_wayland_capture_last_error`` against
fake capture modules shaped like pixelflux's ``ScreenCapture`` -- the live
missing-display / dead-host paths run end to end in
``tests/integration`` against a real compositor.
"""
import asyncio
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

import selkies.selkies as S


class FakeModule:
    """A ScreenCapture stand-in: the readback the verdict reads."""

    def __init__(self, geom, state, last_error, capturing,
                 has_geometry=True, has_state=True):
        self._geom = geom
        self._state = state
        self._last_error = last_error
        self._capturing = capturing
        if has_geometry:
            self.get_realized_geometry = self._get_realized_geometry
        if has_state:
            self.capture_state = self._capture_state

    def _get_realized_geometry(self, _display_id):
        return self._geom

    def _capture_state(self, _display_id):
        return (self._state, self._last_error)

    @property
    def is_capturing(self):
        return self._capturing


def server():
    srv = S.DataStreamingServer.__new__(S.DataStreamingServer)
    return srv


def main() -> bool:
    res = H.Results("wl-start-verdict")
    # The verdict's Wayland gate reads the module-level flag; a test host may be
    # X11, so force it for the duration.
    saved = S.IS_WAYLAND
    S.IS_WAYLAND = True
    try:
        srv = server()

        # A clean start: live, no caveat.
        live, err = asyncio.run(srv._wayland_start_verdict(
            FakeModule((800, 480, 1.0), "running", None, True), "primary"))
        res.check("a clean start is live with no error", live is True and err is None,
                  f"{live} {err}")

        # A missing display id: the compositor recorded the failure, no pipeline.
        live, err = asyncio.run(srv._wayland_start_verdict(
            FakeModule((0, 0, 0.0), "failed", "no output with display id 3", False),
            "display3"))
        res.check("a missing display id reads as a failed start",
                  live is False and err == "no output with display id 3", f"{live} {err}")

        # A dead host compositor: is_capturing flipped false, reason surfaced.
        live, err = asyncio.run(srv._wayland_start_verdict(
            FakeModule(None, "failed", "host compositor connection lost; capture stopped",
                       False), "primary"))
        res.check("a dead host reads as a failed start with its reason",
                  live is False and "host compositor connection lost" in (err or ""),
                  f"{live} {err}")

        # A degraded-but-live start: streams, but the caveat is carried.
        live, err = asyncio.run(srv._wayland_start_verdict(
            FakeModule((800, 480, 1.0), "running", "NVENC init failed (x); using CPU encode",
                       True), "primary"))
        res.check("a CPU fallback is live with a caveat",
                  live is True and err == "NVENC init failed (x); using CPU encode",
                  f"{live} {err}")

        # A barrier timeout (geometry None) does not by itself fail the verdict:
        # is_capturing is still read.
        live, err = asyncio.run(srv._wayland_start_verdict(
            FakeModule(None, "running", None, True), "primary"))
        res.check("a geometry timeout still reads liveness", live is True and err is None,
                  f"{live} {err}")

        # An older pixelflux without the readback is trusted (X11 already raises on
        # failure; a Wayland build this old predates the async start too).
        live, err = asyncio.run(srv._wayland_start_verdict(
            FakeModule(None, None, None, False, has_geometry=False, has_state=False),
            "primary"))
        res.check("an older pixelflux without the readback is trusted",
                  live is True and err is None, f"{live} {err}")

        # capture_state present but is_capturing false with no recorded error still
        # fails: the pipeline is not live.
        live, err = asyncio.run(srv._wayland_start_verdict(
            FakeModule((0, 0, 0.0), "idle", None, False), "primary"))
        res.check("no live pipeline and no error still fails",
                  live is False and err is None, f"{live} {err}")

        # _wayland_capture_last_error reads the reason directly (no barrier).
        err = srv._wayland_capture_last_error(
            FakeModule((0, 0, 0.0), "failed", "encoder session exhausted", False), "display2")
        res.check("last-error reads the recorded reason",
                  err == "encoder session exhausted", err)
        err = srv._wayland_capture_last_error(
            FakeModule(None, None, None, True, has_state=False), "primary")
        res.check("last-error is None without capture_state", err is None, err)
        res.check("last-error tolerates a missing module",
                  srv._wayland_capture_last_error(None, "primary") is None)
    finally:
        S.IS_WAYLAND = saved

    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
