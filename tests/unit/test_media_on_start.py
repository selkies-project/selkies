#!/usr/bin/env python3
"""The session start policy: five `*_on_start` settings and one rule both transports read.

Every pipeline the side menu toggles (video, audio, microphone, webcam,
gamepad) has a bool setting naming the state a session starts in, defaulting
to what a session starts with anyway: video, audio and gamepad on, microphone
and webcam off. `pipeline_starts_on` resolves them for a page and exempts
shared viewers and second display pages. Server side, the WebRTC service
registers a peer with its senders paused by that rule, starts only the
captures a peer receives and pauses or resumes the shared audio capture as
peers toggle it, and the pipeline starts its two captures one by one. Driven
with fakes and loopback peer connections: no pixelflux, pcmflux or browser.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.settings import (
    START_STATE_PIPELINES, SETTING_DEFINITIONS, AppSettings, build_client_settings_payload,
    pipeline_starts_on, settings,
)
from selkies.media_pipeline import MediaPipelinePixel
from selkies.rtc import ClientType, RTCApp
from selkies import webrtc_mode

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CORES = {
    "ws-core": os.path.join(ROOT, "addons", "selkies-web-core", "selkies-ws-core.js"),
    "wr-core": os.path.join(ROOT, "addons", "selkies-web-core", "selkies-wr-core.js"),
}
DEFAULTS = {"video": True, "audio": True, "microphone": False, "webcam": False, "gamepad": True}


def set_policy(**states: bool) -> None:
    """Point the settings singleton's start states at `states` (others default)."""
    for pipeline in START_STATE_PIPELINES:
        setattr(settings, f"{pipeline}_on_start", (states.get(pipeline, DEFAULTS[pipeline]), False))


def settings_block(res: H.Results) -> None:
    by_name = {d["name"]: d for d in SETTING_DEFINITIONS}
    res.check("audio_start_muted is gone", "audio_start_muted" not in by_name)
    for pipeline, default in DEFAULTS.items():
        spec = by_name.get(f"{pipeline}_on_start")
        res.check(f"{pipeline}_on_start: bool defaulting to {default}",
                  spec is not None and spec["type"] == "bool" and spec["default"] is default, spec)
    payload = build_client_settings_payload()
    res.check("every start state reaches the client payload with its value",
              all(isinstance(payload.get(f"{p}_on_start", {}).get("value"), bool) for p in DEFAULTS),
              {p: payload.get(f"{p}_on_start") for p in DEFAULTS})

    argv, environ = sys.argv, dict(os.environ)
    try:
        sys.argv = ["selkies", "--audio-on-start=false", "--gamepad_on_start=false"]
        os.environ["SELKIES_MICROPHONE_ON_START"] = "true"
        os.environ["SELKIES_WEBCAM_ON_START"] = "true|locked"
        parsed = AppSettings(SETTING_DEFINITIONS)
    finally:
        sys.argv = argv
        os.environ.clear()
        os.environ.update(environ)
    res.check("flags parse in both spellings", parsed.audio_on_start[0] is False and parsed.gamepad_on_start[0] is False,
              (parsed.audio_on_start, parsed.gamepad_on_start))
    res.check("environment variables parse, locked suffix included",
              parsed.microphone_on_start == (True, False) and parsed.webcam_on_start == (True, True),
              (parsed.microphone_on_start, parsed.webcam_on_start))
    res.check("video_on_start keeps its default when unset", parsed.video_on_start == (True, False), parsed.video_on_start)

    set_policy()
    res.check("defaults: the primary owner starts video, audio and gamepad on, devices off",
              [pipeline_starts_on(p) for p in START_STATE_PIPELINES] == [True, True, False, False, True])
    set_policy(video=False, audio=False, microphone=True, webcam=True, gamepad=False)
    res.check("policy: the primary owner follows every setting",
              [pipeline_starts_on(p) for p in START_STATE_PIPELINES] == [False, False, True, True, False])
    res.check("policy: a shared viewer keeps the built-in start state",
              [pipeline_starts_on(p, viewer=True) for p in START_STATE_PIPELINES] == [True, True, False, False, True])
    res.check("policy: a second display page keeps the built-in start state",
              [pipeline_starts_on(p, "display2") for p in START_STATE_PIPELINES] == [True, True, False, False, True])
    try:
        pipeline_starts_on("clipboard")
        res.check("an unknown pipeline is refused", False)
    except ValueError:
        res.check("an unknown pipeline is refused", True)
    set_policy()


class FakePipeline:
    """A MediaPipelinePixel stand-in recording what the service asked of it."""

    def __init__(self, running: bool = False, video: bool = False, audio: bool = False) -> None:
        self.running, self.video, self.audio = running, video, audio
        self.calls: list = []

    async def start_media_pipeline(self, video: bool = True, audio: bool = True) -> None:
        self.calls.append(("start", video, audio))
        if self.running:
            return
        self.running, self.video, self.audio = True, video, audio

    async def stop_media_pipeline(self) -> None:
        self.calls.append(("stop",))
        self.running = self.video = self.audio = False

    def is_media_pipeline_running(self) -> bool:
        return self.running

    async def dynamic_idr_frame(self) -> None:
        self.calls.append(("idr",))

    async def pause_screen_capture(self) -> bool:
        self.calls.append(("pause_video",))
        was = self.running and self.video
        self.video = False
        return was

    async def resume_screen_capture(self) -> bool:
        self.calls.append(("resume_video",))
        if not self.running or self.video:
            return False
        self.video = True
        return True

    async def pause_audio_capture(self) -> bool:
        self.calls.append(("pause_audio",))
        was = self.running and self.audio
        self.audio = False
        return was

    async def resume_audio_capture(self) -> bool:
        self.calls.append(("resume_audio",))
        if not self.running or self.audio:
            return False
        self.audio = True
        return True


class Sender:
    def __init__(self) -> None:
        self._enabled = True


def make_service(peers: dict) -> webrtc_mode.WebRTCService:
    svc = webrtc_mode.WebRTCService(SimpleNamespace(set_clients_present=lambda present: None))
    svc.settings = SimpleNamespace(second_screen=(True, False), wayland_host_display="")
    svc.media_pipeline = FakePipeline()
    svc.display_pipelines = {"primary": svc.media_pipeline}
    svc.rtc_app = SimpleNamespace(peer_connections=peers, send_media_data_over_channel=lambda *a: None)
    return svc


def peer(video_paused: bool = False, audio_paused: bool = False, display: str = "primary") -> dict:
    return {"display_id": display, "video_paused": video_paused, "audio_paused": audio_paused,
            "video_sender": Sender(), "audio_sender": Sender()}


async def pipeline_block(res: H.Results) -> None:
    def make(audio_enabled: bool = True) -> MediaPipelinePixel:
        p = MediaPipelinePixel(async_event_loop=asyncio.get_running_loop(),
                               encoder="h264enc", audio_enabled=audio_enabled)
        p.log: list = []

        async def start_screen():
            p.log.append("video+")
            p._is_screen_capturing = True

        async def stop_screen():
            if p._is_screen_capturing:
                p.log.append("video-")
            p._is_screen_capturing = False

        async def start_audio():
            p.log.append("audio+")
            p._is_pcmflux_capturing = True

        async def stop_audio():
            if p._is_pcmflux_capturing:
                p.log.append("audio-")
            p._is_pcmflux_capturing = False

        p.start_screen_capture, p.stop_screen_capture = start_screen, stop_screen
        p._start_audio_pipeline, p._stop_audio_pipeline = start_audio, stop_audio
        p.on_pipeline_started = lambda: None
        return p

    p = make()
    await p.start_media_pipeline(video=False, audio=False)
    res.check("pipeline: starts running with both captures left off",
              p.is_media_pipeline_running() and p.log == [], p.log)
    res.check("pipeline: a paused-at-start audio capture resumes on demand",
              await p.resume_audio_capture() is True and p.log == ["audio+"], p.log)
    res.check("pipeline: a paused-at-start screen capture resumes on demand",
              await p.resume_screen_capture() is True and p.log == ["audio+", "video+"], p.log)
    res.check("pipeline: audio pauses alone, video untouched",
              await p.pause_audio_capture() is True and p.log[-1] == "audio-" and p._is_screen_capturing, p.log)
    res.check("pipeline: a second pause is a no-op", await p.pause_audio_capture() is False and p.log[-1] == "audio-")
    await p.stop_media_pipeline()
    res.check("pipeline: stop ends the running video and nothing else twice",
              p.log[-1] == "video-" and p.log.count("audio-") == 1 and not p.is_media_pipeline_running(), p.log)
    res.check("pipeline: resume on a stopped pipeline is a no-op",
              await p.resume_audio_capture() is False and await p.resume_screen_capture() is False, p.log)

    p = make()
    await p.start_media_pipeline(video=True, audio=False)
    res.check("pipeline: video wanted, audio not: only the screen starts", p.log == ["video+"], p.log)
    p = make(audio_enabled=False)
    await p.start_media_pipeline(video=False, audio=True)
    res.check("pipeline: audio disabled never starts through a resume",
              p.log == [] and await p.resume_audio_capture() is False and p.log == [], p.log)
    p = make()
    await p.start_media_pipeline()
    res.check("pipeline: the default start opens both captures", p.log == ["video+", "audio+"], p.log)


async def service_block(res: H.Results) -> None:
    peers = {"c1": peer(video_paused=True, audio_paused=True)}
    svc = make_service(peers)
    await svc.start_display_media("primary")
    res.check("service: a peer paused by policy starts a pipeline with both captures off",
              svc.media_pipeline.calls[0] == ("start", False, False), svc.media_pipeline.calls)
    res.check("service: the settle pass starts nothing for a paused-only peer",
              not svc.media_pipeline.video and not svc.media_pipeline.audio, svc.media_pipeline.calls)

    await svc.handle_audio_consumer_active("c1", True)
    res.check("service: START_AUDIO from the only peer restarts the audio capture",
              svc.media_pipeline.audio and not peers["c1"]["audio_paused"] and peers["c1"]["audio_sender"]._enabled,
              svc.media_pipeline.calls)
    res.check("service: video stays off while only audio was asked for", not svc.media_pipeline.video)
    await svc.handle_video_consumer_active("c1", "primary", True)
    res.check("service: START_VIDEO resumes the screen capture", svc.media_pipeline.video, svc.media_pipeline.calls)

    peers["v1"] = peer()
    await svc.handle_audio_consumer_active("c1", False)
    res.check("service: STOP_AUDIO pauses that peer's sender only while a viewer listens",
              svc.media_pipeline.audio and not peers["c1"]["audio_sender"]._enabled and peers["c1"]["audio_paused"],
              svc.media_pipeline.calls)
    await svc.handle_audio_consumer_active("v1", False)
    res.check("service: the last listener's STOP_AUDIO stops the audio capture",
              not svc.media_pipeline.audio and svc.media_pipeline.calls[-1] == ("pause_audio",), svc.media_pipeline.calls)
    await svc.handle_audio_consumer_active("v1", True)
    res.check("service: any listener returning restarts it", svc.media_pipeline.audio)

    del peers["v1"]
    await svc.handle_consumers_changed("primary")
    res.check("service: the listening viewer leaving pauses audio for the paused owner",
              not svc.media_pipeline.audio and svc.media_pipeline.calls[-1] == ("pause_audio",), svc.media_pipeline.calls)
    peers["v2"] = peer()
    await svc.handle_consumers_changed("primary")
    res.check("service: a joining unpaused viewer resumes audio", svc.media_pipeline.audio, svc.media_pipeline.calls)

    warm = {"c1": peer(video_paused=True, audio_paused=True)}
    svc = make_service(warm)
    svc.media_pipeline.running = svc.media_pipeline.video = svc.media_pipeline.audio = True
    await svc.start_display_media("primary")
    res.check("service: a paused reconnect onto a warm pipeline stops both captures",
              not svc.media_pipeline.video and not svc.media_pipeline.audio
              and ("pause_video",) in svc.media_pipeline.calls and ("pause_audio",) in svc.media_pipeline.calls,
              svc.media_pipeline.calls)

    mixed = {"c1": peer(video_paused=True, audio_paused=False), "v1": peer(video_paused=False, audio_paused=True)}
    svc = make_service(mixed)
    await svc.start_display_media("primary")
    res.check("service: each capture starts when any peer receives it",
              svc.media_pipeline.calls[0] == ("start", True, True), svc.media_pipeline.calls)
    svc = make_service({"c1": peer(display="display2")})
    await svc.start_display_media("primary")
    res.check("service: no primary consumer registered yet starts everything",
              svc.media_pipeline.calls[0] == ("start", True, True), svc.media_pipeline.calls)


class Service:
    def __init__(self) -> None:
        self.display_clients: dict = {}
        self.audio_events: list = []

    async def start_display_media(self, display_id: str) -> None:
        pass

    async def stop_display_media(self, display_id: str) -> None:
        pass

    async def on_audio(self, peer_id: str, active: bool) -> None:
        self.audio_events.append((peer_id, active))


async def _no_sdp(sdp_type, sdp, peer_id):
    return None


async def _no_ice(mlineindex, candidate, peer_id):
    return None


async def rtc_block(res: H.Results) -> None:
    loop = asyncio.get_running_loop()

    def make_app() -> tuple:
        app = RTCApp(async_event_loop=loop, encoder="h264enc", stun_servers=[], turn_servers=[])
        svc = Service()
        app.start_display_media = svc.start_display_media
        app.stop_display_media = svc.stop_display_media
        app.on_sdp, app.on_ice = _no_sdp, _no_ice
        app.on_audio_consumer_active = svc.on_audio
        return app, svc

    try:
        set_policy(video=False, audio=False)
        app, svc = make_app()
        await app.start_rtc_connection("c1", "controller", None, "primary")
        await app.start_rtc_connection("v1", "viewer", None, "primary")
        c1, v1 = app.peer_connections["c1"], app.peer_connections["v1"]
        res.check("rtc: the owner's peer is registered paused with both senders disabled",
                  c1["video_paused"] and c1["audio_paused"]
                  and not c1["video_sender"]._enabled and not c1["audio_sender"]._enabled,
                  {k: c1[k] for k in ("video_paused", "audio_paused")})
        res.check("rtc: a viewer's peer is registered unpaused whatever the policy",
                  not v1["video_paused"] and not v1["audio_paused"]
                  and v1["video_sender"]._enabled and v1["audio_sender"]._enabled)
        chan = c1["data_channel"]
        await app._on_input_channel_message("START_AUDIO", chan, ClientType.CONTROLLER, None, "primary", "c1", None)
        await app._on_input_channel_message("STOP_AUDIO", chan, ClientType.VIEWER, None, "primary", "v1", None)
        res.check("rtc: START_AUDIO / STOP_AUDIO reach the audio hook per peer, viewers included",
                  svc.audio_events == [("c1", True), ("v1", False)], svc.audio_events)
        await app.stop_rtc_connection("c1", "controller")
        await app.stop_rtc_connection("v1", "viewer")

        set_policy(video=True, audio=False)
        app, svc = make_app()
        await app.start_rtc_connection("c1", "controller", None, "primary")
        c1 = app.peer_connections["c1"]
        res.check("rtc: the policy is read per pipeline",
                  not c1["video_paused"] and c1["audio_paused"] and c1["video_sender"]._enabled)
        svc.display_clients["display2"] = {"width": 0, "height": 0}
        set_policy(video=False, audio=False)
        await app.start_rtc_connection("c2", "controller", None, "display2")
        c2 = app.peer_connections["c2"]
        res.check("rtc: a second display page starts its video whatever the policy",
                  not c2["video_paused"] and c2["audio_sender"] is None, {k: c2[k] for k in ("video_paused", "audio_sender")})
    finally:
        set_policy()


def cores_block(res: H.Results) -> None:
    for label, path in CORES.items():
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
        missing = [f"{p}_on_start" for p in START_STATE_PIPELINES if f"{p}_on_start" not in text]
        res.check(f"{label}: reads every start state from the payload", not missing and "applyStartPolicy" in text, missing)
        res.check(f"{label}: no audio_start_muted left", "audio_start_muted" not in text)


async def main_async(res: H.Results) -> None:
    await pipeline_block(res)
    await service_block(res)
    await rtc_block(res)


def main() -> int:
    res = H.Results("media-on-start")
    settings_block(res)
    asyncio.run(main_async(res))
    cores_block(res)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
