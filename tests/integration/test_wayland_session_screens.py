#!/usr/bin/env python3
"""A nested session grows and shrinks screens over its control socket.

No `WLR_WL_OUTPUTS` is passed: the session compositor boots with one screen
and every further one is asked for over `labwc.sock` (`ADD_SCREEN`), created
by the same backend call a startup count would have made, then adopted by the
capture output created for the display. Teardown runs the other way --
`REMOVE_SCREEN` evacuates the screen's windows to the primary -- and the
whole cycle repeats with fresh screen names, including an arrangement where a
later display takes the origin side while the primary moves off it.

The input half rides the same session: a click delivered right after a single
cross-screen hop must land at the hopped-to position (the capture compositor
repeats a motion whenever pointer focus changes windows, because a nested
wlroots backend reads positions from motion events alone), and a held drag
must carry the session's cursor across the boundary onto a screen that did
not exist at the session's start (`labwc-seam.patch`; an unpatched labwc
clamps at the first screen's last column).

The same rig then drives selkies' own session-screen logic: displays own
screens by the names `ADD_SCREEN` coined, so a removal never picks a
survivor's screen, addressing never assumes display numbers are contiguous,
and an arrangement follows ownership even when displays attached out of
number order.

Skips on a labwc without the control socket, which cannot grow a screen.

Usage: python3 tests/integration/test_wayland_session_screens.py
"""
import asyncio
import json
import os
import shutil
import socket
import subprocess
import sys
import time
from typing import Any, Callable, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

RUNTIME = os.path.join(H.WORKDIR, "wl-session-screens")
W, HGT = 1920, 1080

if shutil.which("labwc") is None:
    H.skip_suite("a nested session test needs labwc")
try:
    import pixelflux
except Exception as exc:
    H.skip_suite(f"pixelflux is not importable: {exc}")


def ipc(cmd: str, timeout: float = 5.0) -> dict:
    """One newline-terminated command over the session's control socket."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    try:
        s.connect(os.path.join(RUNTIME, "labwc.sock"))
        s.sendall(cmd.encode() + b"\n")
        buf = b""
        while not buf.endswith(b"\n"):
            d = s.recv(65536)
            if not d:
                break
            buf += d
    finally:
        s.close()
    return json.loads(buf.decode() or "null")


def poll(fn: Callable[[], Any], want: Callable[[Any], bool],
         timeout: float = 15.0) -> Any:
    """The first `fn()` result `want` accepts, else the last one seen."""
    end = time.monotonic() + timeout
    last = None
    while time.monotonic() < end:
        last = fn()
        if want(last):
            return last
        time.sleep(0.4)
    return last


def nested(capture_socket: str, config: str) -> subprocess.Popen:
    """Start labwc nested on the capture socket, one screen, no output count."""
    env = dict(os.environ, WAYLAND_DISPLAY=capture_socket, WLR_BACKENDS="wayland",
               XDG_RUNTIME_DIR=RUNTIME, WLR_RENDERER="pixman",
               XDG_CONFIG_HOME=config, LIBGL_ALWAYS_SOFTWARE="1")
    env.pop("DISPLAY", None)
    env.pop("WLR_WL_OUTPUTS", None)
    log = open(os.path.join(RUNTIME, "labwc.log"), "w")
    return H.spawn(["labwc"], env=env, stdout=log, stderr=subprocess.STDOUT)


def main() -> "H.Results":
    """Drive the grow/shrink lifecycle and the input paths that ride it."""
    shutil.rmtree(RUNTIME, ignore_errors=True)
    os.makedirs(RUNTIME, mode=0o700, exist_ok=True)
    os.environ["XDG_RUNTIME_DIR"] = RUNTIME
    os.environ.setdefault("LIBGL_ALWAYS_SOFTWARE", "1")
    config = os.path.join(RUNTIME, "config")
    os.makedirs(os.path.join(config, "labwc"), exist_ok=True)
    with open(os.path.join(config, "labwc", "rc.xml"), "w") as fh:
        fh.write('<?xml version="1.0"?>\n<labwc_config>\n'
                 '  <core><decoration>server</decoration></core>\n'
                 '</labwc_config>\n')

    res = H.Results("wl-session-screens")
    capture_socket = pixelflux.ensure_wayland_display(
        width=W, height=HGT, render_node="", auto_gpu="", cursor_size=-1)
    ctl = pixelflux.ScreenCapture()
    proc = nested(capture_socket, config)
    obs: Optional[H.WlObs] = None
    try:
        inner = poll(lambda: next(
            (n for n in sorted(os.listdir(RUNTIME))
             if n.startswith("wayland-") and not n.endswith(".lock")
             and n != capture_socket), None), lambda v: v is not None, 30)
        if not inner:
            H.skip_suite("the nested compositor did not come up")
        if not poll(lambda: os.path.exists(os.path.join(RUNTIME, "labwc.sock")),
                    bool, 10):
            H.skip_suite("this labwc has no control socket")

        screens = poll(lambda: ctl.list_app_screens(inner), lambda v: len(v) == 1)
        res.check("the session boots with a single screen",
                  screens is not None and len(screens) == 1, screens)

        reply = ipc("ADD_SCREEN")
        name2 = str(reply.get("output", ""))
        res.check("ADD_SCREEN answers with the new screen's name",
                  bool(reply.get("ok")) and name2, reply)
        wins = poll(ctl.list_windows,
                    lambda v: len(v) == 2 and sum(1 for w in v if w[4]) == 1)
        res.check("the new screen's host window waits for an output",
                  bool(wins) and sum(1 for w in wins if w[4]) == 1, wins)

        res.check("the second display's output is created",
                  ctl.create_output(2, W, HGT, W, 0, 1.0))
        wins = poll(ctl.list_windows,
                    lambda v: len(v) == 2 and not any(w[4] for w in v)
                    and sorted(w[3] for w in v) == [0, 2])
        res.check("the waiting screen adopts it",
                  bool(wins) and sorted(w[3] for w in wins) == [0, 2]
                  and not any(w[4] for w in wins), wins)

        placed = ctl.set_app_screen_layout(
            inner, [(0, 0, W, HGT), (W, 0, W, HGT)])
        got = poll(lambda: [(x, y) for _n, x, y in ctl.list_app_screens(inner)],
                   lambda v: v == [(0, 0), (W, 0)])
        res.check("the session lays the two screens side by side",
                  placed == 2 and got == [(0, 0), (W, 0)],
                  f"placed={placed} got={got}")

        obs = H.WlObs(inner)
        if not obs.ready(20):
            res.skip("a click lands where a single hop moved", "no observer surface")
            res.skip("a held drag crosses onto the grown screen", "no observer surface")
        else:
            time.sleep(1.0)
            # Park the cursor on the second screen, then reach the first in one
            # hop and click without another motion.
            ctl.inject_mouse_move(2500.0, 400.0)
            time.sleep(0.6)
            obs.lines.clear()
            ctl.inject_mouse_move(600.0, 500.0)
            time.sleep(0.4)
            ctl.inject_mouse_button(272, 1)
            time.sleep(0.3)
            press = poll(
                lambda: [ln for ln in obs.lines
                         if ln.get("kind") == "ptr_button" and ln.get("state") == 1],
                lambda v: bool(v), 5)
            at = [ln["x"] for ln in obs.lines
                  if ln.get("kind") in ("ptr_enter", "ptr_motion")]
            res.check("a click lands where a single hop moved",
                      bool(press) and at and abs(at[-1] - 600.0) < 2.0,
                      f"press={press} at={at[-3:]}")

            for x in range(600, 2 * W - 40, 50):
                ctl.inject_mouse_move(float(x), 500.0)
                time.sleep(0.03)
            time.sleep(0.6)
            ctl.inject_mouse_button(272, 0)
            reach = poll(
                lambda: max([ln["x"] for ln in obs.lines
                             if ln.get("kind") == "ptr_motion"] or [-1.0]),
                lambda v: v > W + 400, 10)
            res.check("a held drag crosses onto the grown screen",
                      reach > W + 400, f"reached x={reach}, boundary at {W}")

        res.check("the second display's output tears down", ctl.destroy_output(2))
        reply = ipc(f"REMOVE_SCREEN {name2}")
        res.check("REMOVE_SCREEN drops the screen", bool(reply.get("ok")), reply)
        wins = poll(ctl.list_windows, lambda v: len(v) == 1)
        scr = poll(lambda: ctl.list_app_screens(inner), lambda v: len(v) == 1)
        res.check("one screen and one host window remain",
                  bool(wins) and len(wins) == 1 and scr is not None
                  and len(scr) == 1, f"wins={wins} screens={scr}")

        clean = True
        for _ in range(3):
            r = ipc("ADD_SCREEN")
            n = str(r.get("output", ""))
            ok = bool(r.get("ok")) and ctl.create_output(2, W, HGT, W, 0, 1.0)
            w2 = poll(ctl.list_windows,
                      lambda v: len(v) == 2 and not any(x[4] for x in v))
            ok = ok and bool(w2) and len(w2) == 2
            ok = ok and ctl.destroy_output(2) \
                and bool(ipc(f"REMOVE_SCREEN {n}").get("ok"))
            w1 = poll(ctl.list_windows, lambda v: len(v) == 1)
            s1 = poll(lambda: ctl.list_app_screens(inner), lambda v: len(v) == 1)
            ok = ok and bool(w1) and s1 is not None and len(s1) == 1
            clean = clean and ok
        res.check("three grow/shrink rounds converge cleanly", clean)

        r = ipc("ADD_SCREEN")
        n_left = str(r.get("output", ""))
        ok_l = bool(r.get("ok")) and ctl.reposition_output(0, W, 0) \
            and ctl.create_output(2, W, HGT, 0, 0, 1.0)
        poll(ctl.list_windows, lambda v: len(v) == 2 and not any(w[4] for w in v))
        placed = ctl.set_app_screen_layout(
            inner, [(W, 0, W, HGT), (0, 0, W, HGT)])
        got = poll(lambda: [(x, y) for _n, x, y in ctl.list_app_screens(inner)],
                   lambda v: v == [(W, 0), (0, 0)])
        res.check("a later display can take the origin side",
                  ok_l and placed == 2 and got == [(W, 0), (0, 0)],
                  f"placed={placed} got={got}")
        ctl.destroy_output(2)
        ipc(f"REMOVE_SCREEN {n_left}")
        ctl.reposition_output(0, 0, 0)
        poll(ctl.list_windows, lambda v: len(v) == 1)

        reply = ipc("REMOVE_SCREEN")
        res.check("the last screen is refused removal", not reply.get("ok"), reply)

        # The session-screen logic selkies runs, bound to this rig through a
        # bare handler: the methods under test manage session screens and
        # touch nothing else of the handler's state.
        from selkies.input_handler import WebRTCInput

        def handler(ipc_ok: bool) -> WebRTCInput:
            h = WebRTCInput.__new__(WebRTCInput)
            h.is_wayland = True
            h.wayland_input = ctl
            h._session_ipc_ok = ipc_ok
            h._app_wayland_display = lambda: inner
            h._has_separate_app_compositor = lambda: True
            return h

        async def handler_phase() -> None:
            h = handler(ipc_ok=True)

            # A display numbered past the screen count still finds its screen.
            await h.ensure_session_screen("display7")
            ok7 = ctl.create_output(7, W, HGT, W, 0, 1.0)
            pos7 = await h.session_screen_position("display7")
            res.check("a display named past the count owns the one new screen",
                      ok7 and pos7 == 1, f"created={ok7} position={pos7}")
            scale7 = await h.realize_wayland_dpi(120, "display7", (W, HGT))
            res.check("its scale lands on that screen, absorbed by the session",
                      scale7 == 1.0, scale7)
            await h._apply_session_screen_layout(
                {"primary": {"x": 0, "y": 0, "w": W, "h": HGT},
                 "display7": {"x": W, "y": 0, "w": W, "h": HGT}})
            got = poll(lambda: [(x, y) for _n, x, y in ctl.list_app_screens(inner)],
                       lambda v: v == [(0, 0), (W, 0)])
            res.check("the layout lands on the screen the display owns",
                      got == [(0, 0), (W, 0)], got)
            ctl.destroy_output(7)
            await h.ensure_session_screens([])
            poll(ctl.list_windows, lambda v: len(v) == 1)

            # Displays attach newest-first; each output adopts its own screen.
            await h.ensure_session_screen("display3")
            ok3 = ctl.create_output(3, W, HGT, 2 * W, 0, 1.0)
            win3 = poll(ctl.list_windows, lambda v: any(w[3] == 3 for w in v))
            id3 = next((w[0] for w in win3 if w[3] == 3), None) if win3 else None
            await h.ensure_session_screen("display2")
            ok2 = ctl.create_output(2, W, HGT, W, 0, 1.0)
            poll(ctl.list_windows,
                 lambda v: len(v) == 3 and not any(w[4] for w in v))
            pos = await h._session_screen_positions(["display2", "display3"])
            res.check("out-of-order attach pairs each screen with its display",
                      ok3 and ok2 and pos == {"display3": 1, "display2": 2}, pos)
            await h._apply_session_screen_layout(
                {"primary": {"x": 0, "y": 0, "w": W, "h": HGT},
                 "display2": {"x": W, "y": 0, "w": W, "h": HGT},
                 "display3": {"x": 2 * W, "y": 0, "w": W, "h": HGT}})
            got = poll(lambda: [(x, y) for _n, x, y in ctl.list_app_screens(inner)],
                       lambda v: v == [(0, 0), (2 * W, 0), (W, 0)])
            res.check("the arrangement follows ownership, not screen order",
                      got == [(0, 0), (2 * W, 0), (W, 0)], got)

            # Both outputs recreated in one relayout: a parked window carries
            # the output it came from, so each returns to its own display
            # however the creates are ordered against attach order.
            wins = poll(ctl.list_windows,
                        lambda v: len(v) == 3 and not any(w[4] for w in v))
            owner = {w[3]: w[0] for w in wins} if wins else {}
            ctl.destroy_output(2)
            ctl.destroy_output(3)
            poll(ctl.list_windows, lambda v: sum(1 for w in v if w[4]) == 2)
            okb = ctl.create_output(3, W, HGT, 2 * W, 0, 1.0) \
                and ctl.create_output(2, W, HGT, W, 0, 1.0)
            wins = poll(ctl.list_windows,
                        lambda v: len(v) == 3 and not any(w[4] for w in v))
            back = {w[3]: w[0] for w in wins} if wins else {}
            res.check("a bulk recreate returns each window to its own display",
                      okb and owner and back == owner,
                      f"before={owner} after={back}")

            # An older display's departure keeps the newer display's screen.
            name3 = h._session_screens.get("display3", "")
            ctl.destroy_output(2)
            await h.ensure_session_screens(["display3"])
            scr = poll(lambda: [n for n, _x, _y in ctl.list_app_screens(inner)],
                       lambda v: len(v) == 2)
            wins = poll(ctl.list_windows,
                        lambda v: len(v) == 2 and not any(w[4] for w in v))
            kept = next((w for w in wins if w[3] == 3), None) if wins else None
            res.check("an older display's departure keeps the newer one's screen",
                      scr is not None and len(scr) == 2 and scr[1] == name3,
                      f"screens={scr} owned={name3}")
            res.check("the survivor's window never left its output",
                      kept is not None and kept[0] == id3, f"was={id3} now={kept}")
            res.check("the survivor's screen compacts to the next position",
                      await h.session_screen_position("display3") == 1)
            ctl.destroy_output(3)
            await h.ensure_session_screens([])
            poll(ctl.list_windows, lambda v: len(v) == 1)

            # A screen nobody owns (a pre-provisioned spare) is converged away.
            ipc("ADD_SCREEN")
            poll(ctl.list_windows, lambda v: len(v) == 2)
            await h.ensure_session_screens([])
            scr = poll(lambda: ctl.list_app_screens(inner), lambda v: len(v) == 1)
            res.check("a screen owned by no display is retired",
                      scr is not None and len(scr) == 1, scr)

            # Without the socket, positions fall back to attach-order ranks.
            h2 = handler(ipc_ok=False)
            await h2.ensure_session_screens(["display7"])
            res.check("a socket-less session ranks displays by presence",
                      await h2.session_screen_position("display7") == 1)

        asyncio.run(handler_phase())

        res.check("the session compositor survived the lifecycle",
                  proc.poll() is None)
    finally:
        if obs is not None:
            obs.proc.terminate()
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
