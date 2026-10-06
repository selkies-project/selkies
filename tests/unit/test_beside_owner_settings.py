#!/usr/bin/env python3
"""The stream settings of a display that two controller pages share over WebSockets.

A page beside the display's owner changes the display by what its user picks, and only where
both pages hold full permissions; what it changes on its own follows the owner, and it is told
what that is. A page's later SETTINGS applies the stream settings it changed, not the ones it
repeats. The server runs on the tables these read, with the apply and the send recorded.
"""
import asyncio
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "src"))

from selkies import sessions  # noqa: E402
from selkies import websockets_mode as wm  # noqa: E402

failures = 0


def check(name: str, ok: bool, detail: object = "") -> None:
    global failures
    print(("PASS  " if ok else "FAIL  ") + name + (f"  {detail}" if detail != "" and not ok else ""))
    if not ok:
        failures += 1


class App:
    audio_bitrate = 128000


def server_with(owner: object, joiner: object) -> tuple:
    """A server whose primary display `owner` holds with `joiner` beside it; the apply and the
    sends are recorded."""
    server = object.__new__(wm.DataStreamingServer)
    server.clients = {owner, joiner}
    server.display_clients = {"primary": {"ws": owner, "encoder": "jpeg", "framerate": 60, "video_crf": 25,
                                          "use_cpu": True, "use_cpu_requested": None}}
    server.co_controllers = {"primary": {joiner: "tab-b"}}
    server._page_settings = {}
    server.app = App()
    server._initial_use_cpu = False
    applied, told = [], []

    async def apply(ws, settings, initial, role):
        applied.append((ws, settings, initial))

    async def tell(display_id, sockets):
        told.append((display_id, set(sockets)))

    server._apply_client_settings = apply
    server._tell_display_settings = tell
    return server, applied, told


def parsed(server: object, **values: object) -> dict:
    """A SETTINGS payload as the server parses it."""
    import json
    return server._parse_settings_payload(json.dumps(values))


async def run_pick_checks() -> None:
    owner, joiner = object(), object()
    wm.client_permissions.clear()
    wm.client_permissions[owner] = {"role": "controller"}
    wm.client_permissions[joiner] = {"role": "controller"}
    server, applied, told = server_with(owner, joiner)

    await server._take_pick_beside_owner(joiner, parsed(server, encoder="h264enc", video_crf=18,
                                                        manual_resolution=True, picked=["encoder"]), "controller")
    settings = applied[0][1] if applied else {}
    check("a pick beside the owner applies to the primary", len(applied) == 1 and applied[0][0] is joiner
          and settings.get("displayId") == "primary" and settings.get("encoder") == "h264enc"
          and applied[0][2] is False, applied)
    check("with nothing it did not pick, the display's geometry among it",
          settings.get("video_crf") is None and settings.get("manual_resolution") is None
          and "displayPosition" in settings and settings.get("displayPosition") is None, settings)
    check("and the encoder's fallback mark beside it", "encoderFallback" in settings, settings)
    check("the owner and the pages beside it are told", told == [("primary", {owner, joiner})], told)

    applied.clear(), told.clear()
    await server._take_pick_beside_owner(joiner, parsed(server, encoder="h264enc", scaling_dpi=144,
                                                        picked=["scaling_dpi", "manual_resolution"]), "controller")
    check("a pick of the display's size or density is the owner's", applied == [], applied)
    check("and the page is told what the display streams with", told == [("primary", {joiner})], told)

    applied.clear(), told.clear()
    await server._take_pick_beside_owner(joiner, parsed(server, encoder="vp8enc", framerate=144), "controller")
    check("what a page beside the owner changed on its own does not apply", applied == [], applied)
    check("it is told what the display streams with instead", told == [("primary", {joiner})], told)

    wm.client_permissions[joiner] = {"role": "viewer"}
    applied.clear(), told.clear()
    await server._take_pick_beside_owner(joiner, parsed(server, encoder="vp8enc", picked=["encoder"]), "controller")
    check("a viewer's pick does not apply", applied == [], applied)

    wm.client_permissions[joiner] = {"role": "controller", "token": "b"}
    wm.client_permissions[owner] = {"role": "controller", "token": "a"}
    saved = sessions.active_mk_token
    try:
        sessions.active_mk_token = "a"
        applied.clear()
        await server._take_pick_beside_owner(joiner, parsed(server, encoder="vp8enc", picked=["encoder"]),
                                             "controller")
        check("nor one from a page whose keyboard and mouse another token holds", applied == [], applied)
        sessions.active_mk_token = "b"
        applied.clear()
        await server._take_pick_beside_owner(joiner, parsed(server, encoder="vp8enc", picked=["encoder"]),
                                             "controller")
        check("nor one beside an owner without them", applied == [], applied)
    finally:
        sessions.active_mk_token = saved
    wm.client_permissions.clear()


async def run_repeat_checks() -> None:
    owner, joiner = object(), object()
    server, _, _ = server_with(owner, joiner)
    first = parsed(server, encoder="jpeg", video_crf=30, manual_resolution=False, initialClientWidth=1280)
    check("a page's first later SETTINGS carries all of it",
          server._settings_changed_by(owner, first)["encoder"] == "jpeg")
    again = server._settings_changed_by(owner, parsed(server, encoder="jpeg", video_crf=35, manual_resolution=False,
                                                      initialClientWidth=1280))
    check("then leaves out the stream settings it repeats", "encoder" not in again and again.get("video_crf") == 35,
          again)
    check("and keeps what is not a stream setting", again.get("manual_resolution") is False
          and again.get("initialClientWidth") == 1280, again)
    marked = server._settings_changed_by(owner, parsed(server, encoder="jpeg", video_crf=35, encoderFallback=True))
    check("an encoder whose fallback mark changed is a change", marked.get("encoder") == "jpeg", marked)


async def run_display_checks() -> None:
    owner, joiner = object(), object()
    server, _, _ = server_with(owner, joiner)
    values = server._display_settings("primary")
    check("a display's settings are what it streams with", values.get("encoder") == "jpeg"
          and values.get("framerate") == 60 and values.get("video_crf") == 25, values)
    check("with the use_cpu it asks for, not the one JPEG implies", values.get("use_cpu") is False, values)
    check("and the session's audio bitrate", values.get("audio_bitrate") == 128000, values)
    check("the keys it lacks are left out", "video_bitrate" not in values, values)
    check("a payload's picks are its own strings",
          parsed(server, picked=["encoder", 3, "framerate"])["picked"] == ["encoder", "framerate"]
          and parsed(server, picked="encoder")["picked"] == [])


def main() -> int:
    for block in (run_pick_checks, run_repeat_checks, run_display_checks):
        asyncio.run(block())
    print(f"\n{failures} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
