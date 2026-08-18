#!/usr/bin/env python3
"""Which socket input and clipboard are aimed at.

A nested session owns the applications on its own socket, so resolving it
wrongly silently sends every overlay keysym and the selection to a compositor
no application is on. The candidates are read from a real XDG_RUNTIME_DIR
holding real listening sockets, including the relay a session can add beside
the compositors.
"""
import os
import socket
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies.input_handler import WebRTCInput  # noqa: E402

CAPTURE = "wayland-1"
results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  {label}  {detail}", flush=True)


class FakeWaylandInput:
    """Records which socket the handler retargets injection at."""

    def __init__(self) -> None:
        self.targets: list = []

    def set_app_wayland_display(self, name: str) -> None:
        self.targets.append(name)


def make_handler(app_wayland_display: str = "") -> WebRTCInput:
    """A WebRTCInput with only the app-socket resolution state initialized."""
    h = WebRTCInput.__new__(WebRTCInput)
    h.wayland_input = FakeWaylandInput()
    h.app_wayland_display = app_wayland_display
    h._app_wl_display_cached = None
    h._app_wl_is_separate = False
    h._app_wl_negcache = None
    h._app_wl_negcache_at = 0.0
    h._wayland_display_name = lambda: CAPTURE
    h._schedule_session_dpi = lambda: None
    return h


def resolve(runtime: str, names, app_wayland_display: str = "") -> tuple:
    """Lay out `names` as live sockets and resolve against them.

    Returns:
        `(resolved_name, handler)` for inspecting the resolution state.
    """
    for entry in os.listdir(runtime):
        os.unlink(os.path.join(runtime, entry))
    keep = []
    for name in names:
        path = os.path.join(runtime, name)
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(path)
        server.listen(8)
        keep.append(server)
        open(path + ".lock", "w").close()
    h = make_handler(app_wayland_display)
    try:
        return h._app_wayland_display(), h
    finally:
        for server in keep:
            server.close()


def main() -> int:
    runtime = tempfile.mkdtemp(prefix="selkies-appsock-")
    os.environ["XDG_RUNTIME_DIR"] = runtime

    got, h = resolve(runtime, [CAPTURE])
    check("a plain session keeps input on the capture compositor",
          got == CAPTURE and not h._app_wl_is_separate, got)

    got, h = resolve(runtime, [CAPTURE, "wayland-0"])
    check("a nested compositor is adopted",
          got == "wayland-0" and h._app_wl_is_separate
          and h.wayland_input.targets == ["wayland-0"], got)

    # A session that proxies its own window verbs listens beside the two
    # compositors; the compositor is still the one to inject into.
    got, h = resolve(runtime, [CAPTURE, "wayland-0", "wayland-wm"])
    check("a relay socket beside a nested compositor does not hide it",
          got == "wayland-0" and h._app_wl_is_separate, got)

    got, h = resolve(runtime, [CAPTURE, "wayland-0", "wayland-2"])
    check("two nested compositors stay ambiguous",
          got == CAPTURE and not h._app_wl_is_separate, got)

    got, h = resolve(runtime, [CAPTURE, "wayland-0", "wayland-2"],
                     app_wayland_display="wayland-2")
    check("the configured socket wins over ambiguity",
          got == "wayland-2" and h._app_wl_is_separate, got)

    # Nothing is listening on the second name: a compositor that died must not
    # take input and clipboard with it.
    for entry in os.listdir(runtime):
        os.unlink(os.path.join(runtime, entry))
    for name in (CAPTURE, "wayland-0"):
        path = os.path.join(runtime, name)
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(path)
        server.listen(8)
        if name != CAPTURE:
            server.close()
    h = make_handler()
    check("a stale socket file is not adopted", h._app_wayland_display() == CAPTURE)

    failed = [name for name, ok in results if not ok]
    print(f"\n{len(results) - len(failed)}/{len(results)} checks passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
