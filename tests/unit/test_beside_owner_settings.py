#!/usr/bin/env python3
"""The stream of a display that two controller pages share, on both transports.

A page beside the display's owner changes the display by what its user picks, and only where
both pages hold full permissions; what it changes on its own follows the owner, and it is told
what that is. Over WebSockets a page's later SETTINGS applies the stream settings it changed,
not the ones it repeats; over WebRTC its stream verbs are its page's own and are dropped, and
what would size the display waits for it to own it, without the stream settings. A page that
does not own its display repairs the shared stream at most once a second, and over WebSockets
starts and stops audio for itself alone; over WebRTC the owner's link alone steers the display's
rate while it reports. Once the owner is gone, the oldest page beside it that holds full
permissions takes the display over. The servers run on the tables these read, with
the apply and the sends recorded.
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


class Channel:
    pass


class RTCApp:
    """The peers a WebRTC service routes between, with its sends recorded."""

    def __init__(self, peers: dict) -> None:
        self.peer_connections = peers
        self.sent: list = []
        self.denied: set = set()

    def send_message_to_channel(self, channel, msg_type, data) -> None:
        self.sent.append((channel, msg_type, data))

    def peer_holds_input_authority(self, peer) -> bool:
        return bool(peer) and peer.get("name") not in self.denied


async def run_webrtc_checks() -> None:
    import json
    from selkies import webrtc_mode as rm
    from selkies.webrtc_engine import ClientType

    owner = {"name": "owner", "client_type": ClientType.CONTROLLER, "display_id": "primary", "data_channel": Channel()}
    joiner = {"name": "joiner", "client_type": ClientType.CONTROLLER, "display_id": "primary",
              "data_channel": Channel()}
    service = object.__new__(rm.WebRTCService)
    service.rtc_app = RTCApp({"o": owner, "j": joiner})
    service._peer_tabs = {"o": "tab-a", "j": "tab-b"}
    service._display_owner_tabs = {"primary": "tab-a"}
    service.args = type("Args", (), {"encoder": "vp8enc", "framerate": 60, "video_crf": 25})()
    service.display_clients = {}
    handled, updated = [], []

    class Handler:
        def on_message(self, msg, display_id, conn_id=None):
            handled.append((msg, conn_id))

    async def update(settings, display_id="primary"):
        updated.append(settings)
    service.input_handler = Handler()
    service.handle_update_settings = update

    def told():
        return [(ch, data["settings"].get("encoder")) for ch, kind, data in service.rtc_app.sent
                if kind == "display_settings"]

    await service._on_peer_data_message("SETTINGS," + json.dumps({"encoder": "h264enc", "video_crf": 18,
                                        "scaling_dpi": 144, "picked": ["encoder"]}), "primary", "j")
    check("webrtc: a pick beside the owner applies", updated == [{"encoder": "h264enc"}], updated)
    check("webrtc: and the owner and the page are told", {ch for ch, _ in told()}
          == {owner["data_channel"], joiner["data_channel"]}, told())
    held = joiner.get("held_owner_messages", {}).get("SETTINGS", "")
    check("webrtc: what would size the display waits for it to own it, without the stream settings",
          '"scaling_dpi": 144' in held and "encoder" not in held and "video_crf" not in held, held)

    updated.clear(), service.rtc_app.sent.clear()
    await service._on_peer_data_message('SETTINGS,{"encoder": "av1enc"}', "primary", "j")
    check("webrtc: what it changed on its own does not apply", updated == [], updated)
    check("webrtc: it is told what the display streams with",
          [ch for ch, _ in told()] == [joiner["data_channel"]], told())

    for verb in ("_arg_fps,144", "vb,2000", "ab,64000", "_crf,40", "_rc,crf"):
        check(f"webrtc: its {verb.split(',')[0]} verb is dropped",
              service._on_peer_data_message(verb, "primary", "j") is None and not handled, handled)
    service._on_peer_data_message("r,1024x640", "primary", "j")
    check("webrtc: its resize is held", joiner["held_owner_messages"].get("r") == "r,1024x640" and not handled)

    service.rtc_app.denied = {"joiner"}
    updated.clear()
    await service._on_peer_data_message('SETTINGS,{"encoder": "h264enc", "picked": ["encoder"]}', "primary", "j")
    check("webrtc: nor a pick from a page whose keyboard and mouse another token holds", updated == [], updated)
    service.rtc_app.denied = set()
    joiner["client_type"] = ClientType.VIEWER
    await service._on_peer_data_message('SETTINGS,{"encoder": "h264enc", "picked": ["encoder"]}', "primary", "j")
    check("webrtc: nor a viewer's", updated == [], updated)
    joiner["client_type"] = ClientType.CONTROLLER

    service.rtc_app.sent.clear()
    result = service._on_peer_data_message("_crf,30", "primary", "o")
    if asyncio.iscoroutine(result):
        await result
    check("webrtc: the owner's verb applies", handled == [("_crf,30", "o")], handled)
    check("webrtc: and the page beside it is told", [ch for ch, _ in told()] == [joiner["data_channel"]], told())


class Socket:
    """A page's websocket that records what it was sent."""

    def __init__(self) -> None:
        self.closed = False
        self.sent: list = []

    async def send_str(self, text: str) -> None:
        self.sent.append(text)


async def run_stream_checks() -> None:
    from collections import OrderedDict
    owner, joiner, viewer = Socket(), Socket(), Socket()
    server, _, _ = server_with(owner, joiner)
    server.clients = {owner, joiner, viewer}
    server._repair_times = {}
    server._beside_audio = {}
    server._session_audio = None
    check("websockets: the owner's repairs are always taken",
          all(server._repair_taken(owner, "primary", "keyframe") for _ in range(3)))
    taken = [server._repair_taken(joiner, "primary", "keyframe") for _ in range(3)]
    check("websockets: a page beside it repairs at most once a second", taken == [True, False, False], taken)
    check("websockets: each kind on its own", server._repair_taken(joiner, "primary", "lost"))
    check("websockets: a viewer likewise", [server._repair_taken(viewer, None, "lost") for _ in range(2)]
          == [True, False])

    check("websockets: before the owner says, its audio follows the start policy",
          server._hears_audio(owner) == wm.pipeline_starts_on("audio", "primary"))
    server._session_audio = False
    server._beside_audio[joiner] = True
    check("websockets: the owner's audio off holds for it and the viewers",
          not server._hears_audio(owner) and not server._hears_audio(viewer))
    check("websockets: while a page beside it hears its own", server._hears_audio(joiner))
    check("websockets: and the capture runs for it", server._audio_wanted())
    server._beside_audio[joiner] = False
    check("websockets: nobody hearing, it may stop", not server._audio_wanted())
    server._session_audio = True
    check("websockets: the owner's on, it and the viewers hear, the page beside it not by that",
          server._hears_audio(owner) and server._hears_audio(viewer) and not server._hears_audio(joiner))

    first, second = Socket(), Socket()
    server.co_controllers = {"primary": OrderedDict([(first, "tab-b"), (second, "tab-c")])}
    wm.client_permissions.clear()
    wm.client_permissions[first] = {"role": "controller", "token": "b"}
    wm.client_permissions[second] = {"role": "controller", "token": "c"}
    saved = sessions.active_mk_token
    try:
        sessions.active_mk_token = "c"
        promoted = await server._promote_co_controller("primary")
        check("websockets: the owner gone, the oldest page beside it with full permissions owns the display",
              promoted and second.sent == ["DISPLAY_OWNER primary"] and not first.sent, (first.sent, second.sent))
        check("websockets: the other stays beside it", list(server.co_controllers["primary"]) == [first])
        sessions.active_mk_token = "x"
        await server._promote_co_controller("primary")
        check("websockets: with none holding them, the oldest", first.sent == ["DISPLAY_OWNER primary"], first.sent)
    finally:
        sessions.active_mk_token = saved
        wm.client_permissions.clear()


async def run_webrtc_stream_checks() -> None:
    from selkies import webrtc_mode as rm
    from selkies.webrtc_engine import ClientType, RTCApp as Engine

    engine = object.__new__(Engine)
    engine._peer_recovery_times = {}
    engine.peer_owns_display = lambda peer_id: peer_id == "o"
    check("webrtc: the owner's repairs are always taken",
          all(engine.peer_recovery_taken("o", "keyframe") for _ in range(3)))
    taken = [engine.peer_recovery_taken("j", "keyframe") for _ in range(3)]
    check("webrtc: another peer's at most once a second", taken == [True, False, False], taken)
    check("webrtc: each kind on its own", engine.peer_recovery_taken("j", "lost"))

    older = {"name": "older", "client_type": ClientType.CONTROLLER, "display_id": "primary", "data_channel": Channel()}
    newer = {"name": "newer", "client_type": ClientType.CONTROLLER, "display_id": "primary", "data_channel": Channel()}
    service = object.__new__(rm.WebRTCService)
    service.rtc_app = RTCApp({"a": older, "b": newer})
    service.rtc_app.denied = {"older"}
    service._peer_tabs = {"a": "tab-b", "b": "tab-c"}
    service._display_owner_tabs = {"primary": "tab-a"}
    service.RECONNECT_GRACE_S = 0
    replayed = []

    class Handler:
        def on_message(self, msg, display_id, conn_id=None):
            replayed.append((msg, conn_id))
    service.input_handler = Handler()
    newer["held_owner_messages"] = {"r": "r,1024x640"}
    await service._succeed_display_owner("primary", "tab-a")
    check("webrtc: the owner gone, the oldest controller with full permissions owns the display",
          service._display_owner_tabs["primary"] == "tab-c", service._display_owner_tabs)
    check("webrtc: and what it held back applies", replayed == [("r,1024x640", "b")], replayed)
    check("webrtc: its repairs are the owner's now", service._peer_owns_display("b")
          and not service._peer_owns_display("a"))


async def run_webrtc_congestion_checks() -> None:
    from types import SimpleNamespace
    from unittest.mock import AsyncMock, patch

    from selkies import webrtc_mode as rm

    clean = dict(goodput_bps=4_000_000, sent_bps=4_000_000, loss_fraction=0.0,
                 queue_ms=0.0, queue_rising_ms=0.0, queue_depth_ms=0.0)
    queued = dict(goodput_bps=2_000_000, sent_bps=4_000_000, loss_fraction=0.0,
                  queue_ms=300.0, queue_rising_ms=300.0, queue_depth_ms=300.0)

    async def steered(owner: object, windows: dict) -> list:
        """The rates one congestion tick gives the primary display at 4000 kbps, its peers'
        feedback by peer id (a peer's tab is its id), `owner` the owner's tab."""
        pipeline = SimpleNamespace(rc_mode=rm.RateControlMode.CBR, video_bitrate=4000)
        rates = []

        async def set_video_bitrate(kbps: int) -> None:
            rates.append(kbps)
            pipeline.video_bitrate = kbps
        pipeline.set_video_bitrate = set_video_bitrate

        def peer(window: object) -> dict:
            transport = SimpleNamespace(take_twcc_window=lambda: window)
            return {"peer_conn": SimpleNamespace(sctp=SimpleNamespace(transport=transport)),
                    "display_id": "primary", "video_sender": None}
        service = object.__new__(rm.WebRTCService)
        service.metrics = None
        service.args = SimpleNamespace(congestion_control=True)
        service._congestion_steer, service._rate_holds = {}, {}
        service._display_setting = lambda did, key: 6000
        service.display_pipelines = {"primary": pipeline}
        service._display_owner_tabs = {"primary": owner} if owner else {}
        service._peer_tabs = {pid: pid for pid in windows}
        service.rtc_app = SimpleNamespace(peer_connections={pid: peer(w) for pid, w in windows.items()},
                                          send_cc_rate=lambda did, kbps: None,
                                          set_video_budget=lambda did, bps: None)
        sleep = AsyncMock(side_effect=[None, asyncio.CancelledError()])
        with patch.object(rm, "asyncio", SimpleNamespace(sleep=sleep)), \
                patch.object(rm, "time", SimpleNamespace(monotonic=lambda: 1.0)), \
                patch.object(rm, "settings", SimpleNamespace(video_bitrate=(1000, 12000), webrtc_pacer=(False,))):
            try:
                await service._congestion_control_loop()
            except asyncio.CancelledError:
                pass
        return rates

    rates = await steered("o", {"o": clean, "j": queued})
    check("webrtc: a queue on the link of a page beside the owner leaves the owner's rate be",
          all(kbps >= 4000 for kbps in rates), rates)
    rates = await steered(None, {"o": clean, "j": queued})
    check("webrtc: with no owner known, the worst link steers, as before", rates and rates[-1] < 4000, rates)
    rates = await steered("o", {"o": None, "j": queued})
    check("webrtc: and so does the rest while the owner sends no feedback", rates and rates[-1] < 4000, rates)
    rates = await steered("o", {"o": queued, "j": clean})
    check("webrtc: the owner's own queue lowers it", rates and rates[-1] < 4000, rates)


def main() -> int:
    for block in (run_pick_checks, run_repeat_checks, run_display_checks, run_webrtc_checks,
                  run_stream_checks, run_webrtc_stream_checks, run_webrtc_congestion_checks):
        asyncio.run(block())
    print(f"\n{failures} failure(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
