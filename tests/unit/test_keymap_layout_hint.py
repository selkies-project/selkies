#!/usr/bin/env python3
"""Where the client keyboardLayout hint lands.

On Wayland with the apps on the capture compositor the hint becomes the seat's
base layout, once per value. Under a nested session compositor it is
informational, as on X11: the session translates keycodes with its own keymap,
so a seat moved to the client layout would transpose every key the two layouts
place apart. A hint only noted under a session is applied once the apps are
back on the capture compositor, and a session compositor that turns up after
the seat took a client layout puts the seat back on the deployment layout the
session runs on.
"""
import asyncio
import logging
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import input_handler as ih  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [kb-layout-hint] {label}  {detail}", flush=True)


class Records(logging.Handler):
    def __init__(self) -> None:
        super().__init__()
        self.lines: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append((record.levelno, record.getMessage()))


records = Records()
ih.logger_webrtc_input.addHandler(records)
ih.logger_webrtc_input.setLevel(logging.DEBUG)


class FakeWaylandInput:
    """Seat keymap control that records every base-layout push."""

    def __init__(self) -> None:
        self.layouts: list = []

    def set_xkb_layout(self, layout, variant="", options="", model="", rules=""):
        self.layouts.append((layout, variant, options, model, rules))
        return True

    def set_keymap_string(self, text: str) -> None:
        pass


def make_handler(is_wayland: bool = True, separate: bool = False) -> WebRTCInput:
    """A WebRTCInput with the keymap-hint state initialized; `separate`
    (mutable through h.topology) is whether a nested session compositor is
    reported."""
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = is_wayland
    h.wayland_input = FakeWaylandInput()
    h._client_kb_layout = None
    h._wl_seat_client_layout = None
    h._wl_keymap_owner = None
    h._wl_keymap_stale = False
    h._wl_keymap_retry_at = 0.0
    h._wl_keymap_owner_lock = asyncio.Lock()
    h._bg_tasks = set()
    h.topology = {"separate": separate}
    h._has_separate_app_compositor = lambda: h.topology["separate"]
    return h


def noted_lines(text: str) -> list:
    return [line for _, line in records.lines if text in line]


async def main() -> None:
    # --- nested session: the hint is noted, the seat is left alone ---
    h = make_handler(separate=True)
    await h.apply_client_keyboard_layout("de")
    await h.apply_client_keyboard_layout("de")
    check("nested session: hint not pushed onto the seat", h.wayland_input.layouts == [],
          str(h.wayland_input.layouts))
    check("nested session: hint noted", h._client_kb_layout == "de")
    check("nested session: noted once per value",
          len(noted_lines("session compositor owns the keymap")) == 1,
          str(noted_lines("session compositor owns the keymap")))
    check("nested session: seat records no client layout", h._wl_seat_client_layout is None)

    # --- the same hint applies once the apps are back on the capture compositor ---
    h.topology["separate"] = False
    await h.apply_client_keyboard_layout("de")
    check("direct topology later: the noted hint is applied",
          h.wayland_input.layouts == [("de", "", "", "", "")], str(h.wayland_input.layouts))
    check("seat records the client layout", h._wl_seat_client_layout == "de")
    check("keymap owner marked stale for the new base", h._wl_keymap_stale is True)

    # --- direct topology: once per value, variant carried ---
    h = make_handler()
    await h.apply_client_keyboard_layout("ch(fr)")
    await h.apply_client_keyboard_layout("ch(fr)")
    check("direct: layout and variant pushed once",
          h.wayland_input.layouts == [("ch", "fr", "", "", "")], str(h.wayland_input.layouts))
    await h.apply_client_keyboard_layout("us")
    check("direct: a new value is pushed", h.wayland_input.layouts[-1][0] == "us"
          and len(h.wayland_input.layouts) == 2)

    # --- X11: informational ---
    hx = make_handler(is_wayland=False)
    await hx.apply_client_keyboard_layout("de")
    check("x11: hint noted, nothing pushed", hx.wayland_input.layouts == []
          and hx._client_kb_layout == "de")

    # --- malformed hints never reach the seat ---
    h = make_handler()
    await h.apply_client_keyboard_layout("de;rm -rf")
    check("malformed hint rejected", h.wayland_input.layouts == []
          and h._client_kb_layout is None)

    # --- a session compositor turning up after a client layout took the seat ---
    # The whole XKB_DEFAULT_* set is scrubbed, not just the layout: a model or
    # rules exported by the developer's shell would otherwise reach the seat.
    saved = {k: os.environ.pop(k) for k in list(os.environ) if k.startswith("XKB_DEFAULT_")}
    try:
        os.environ["XKB_DEFAULT_LAYOUT"] = "fr"
        h = make_handler()
        await h.apply_client_keyboard_layout("de")
        h.topology["separate"] = True
        h._schedule_seat_layout_restore()
        await asyncio.sleep(0.05)
        check("late session compositor: seat restored to the deployment layout",
              h.wayland_input.layouts[-1][0] == "fr" and h._wl_seat_client_layout is None,
              str(h.wayland_input.layouts))
        await h.apply_client_keyboard_layout("de")
        check("the re-asserted hint stays informational under the session",
              h.wayland_input.layouts[-1][0] == "fr", str(h.wayland_input.layouts))

        os.environ.pop("XKB_DEFAULT_LAYOUT", None)
        h = make_handler()
        await h.apply_client_keyboard_layout("de")
        h.topology["separate"] = True
        h._schedule_seat_layout_restore()
        await asyncio.sleep(0.05)
        check("no env layout: restore pushes the xkbcommon default",
              h.wayland_input.layouts[-1] == ("", "", "", "", "")
              and h._wl_seat_client_layout is None, str(h.wayland_input.layouts))

        h = make_handler()
        h.topology["separate"] = True
        h._schedule_seat_layout_restore()
        await asyncio.sleep(0.05)
        check("nothing to restore when the seat never took a client layout",
              h.wayland_input.layouts == [])

        # Session start with no env layout leaves the seat default alone.
        h = make_handler()
        await h._push_wayland_base_layout()
        check("session start without an env layout pushes nothing",
              h.wayland_input.layouts == [])
    finally:
        os.environ.pop("XKB_DEFAULT_LAYOUT", None)
        os.environ.update(saved)


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
