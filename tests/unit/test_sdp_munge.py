#!/usr/bin/env python3
"""The offer's codec rewrites reach the display's own video section and no other.

The offer bundles the display's sendonly video with the recvonly webcam
section the browser encodes. Full colour rewrites the H.264 profile to High
4:4:4 and VP9 to profile 1, and H.264 or H.265 gets `sps-pps-idr-in-keyframe`,
all of it describing the stream the server sends. A browser handed High 4:4:4
for the camera it is asked to encode rejects that section and stops its
sender, so the webcam section has to keep the profiles it offered. Driven
against a real offer from RTCApp (a loopback peer connection with stubbed
signalling), then through munge_sdp with each encoder and colour setting.
"""
import asyncio
import os
import re
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.webrtc_engine import RTCApp


async def _nothing(*args):
    return None


def sections(sdp: str, kind: str, direction: str) -> list:
    """The `m=<kind>` sections carrying `a=<direction>`; "sendrecv" is what
    the server's own media is offered as."""
    return [s for s in re.split(r'(?m)(?=^m=)', sdp)
            if s.startswith(f"m={kind}") and f"a={direction}" in s]


def profiles(section: str) -> list:
    return [m.lower() for m in re.findall(r'profile-level-id=([0-9A-Fa-f]{6})', section)]


def vp9_fmtp(section: str) -> list:
    """The fmtp parameters of the section's VP9 payload types."""
    pts = re.findall(r'a=rtpmap:(\d+) VP9/', section)
    return [re.sub(r'x-google-[a-z-]+=\d+;?', '', m.group(1)).strip() for pt in pts
            for m in [re.search(rf'(?m)^a=fmtp:{pt} (.*)$', section)] if m]


async def scenario(res: H.Results) -> None:
    loop = asyncio.get_running_loop()
    app = RTCApp(async_event_loop=loop, encoder="h264enc", stun_servers=[], turn_servers=[])
    app.start_display_media = _nothing
    app.stop_display_media = _nothing
    app.on_sdp = _nothing
    app.on_ice = _nothing
    offers: list = []
    munge = app.munge_sdp
    app.munge_sdp = lambda sdp, *a, **k: (offers.append(sdp), munge(sdp, *a, **k))[1]
    await app.start_rtc_connection("v1", "viewer", None, "primary")
    try:
        raw = offers[0]
    finally:
        await app.stop_rtc_connection("v1", "viewer")

    display = sections(raw, "video", "sendrecv")
    webcam = sections(raw, "video", "recvonly")
    res.check("the offer has the display's own video and the recvonly webcam section",
              len(display) == 1 and len(webcam) == 1, (len(display), len(webcam)))
    res.check("both sections offer H.264 with a profile",
              profiles(display[0]) and profiles(webcam[0]), (profiles(display[0]), profiles(webcam[0])))
    res.check("the webcam section offers VP9 profile 0 as well",
              vp9_fmtp(webcam[0]) == ["profile-id=0"], vp9_fmtp(webcam[0]))
    res.check("the raw offer carries no sps-pps-idr-in-keyframe of its own",
              "sps-pps-idr-in-keyframe" not in raw)

    full = app.munge_sdp(raw, "h264enc", True, False)
    out_display, out_webcam = sections(full, "video", "sendrecv")[0], sections(full, "video", "recvonly")[0]
    res.check("h264enc full colour: the display's profiles are all High 4:4:4",
              set(profiles(out_display)) == {"f4001f"}, profiles(out_display))
    res.check("h264enc full colour: the webcam keeps the profiles it offered",
              profiles(out_webcam) == profiles(webcam[0]), profiles(out_webcam))
    res.check("h264enc: sps-pps-idr-in-keyframe is asked of the display's stream only",
              "sps-pps-idr-in-keyframe=1" in out_display and "sps-pps-idr-in-keyframe" not in out_webcam)
    res.check("both video sections get the bandwidth ceiling",
              "b=AS:300000" in out_display and "b=AS:300000" in out_webcam)

    plain = app.munge_sdp(raw, "h264enc", False, False)
    out_display = sections(plain, "video", "sendrecv")[0]
    res.check("h264enc 4:2:0: the display keeps the profiles it offered",
              profiles(out_display) == profiles(display[0]), profiles(out_display))

    vp9 = app.munge_sdp(raw, "vp9enc", True, False)
    out_display, out_webcam = sections(vp9, "video", "sendrecv")[0], sections(vp9, "video", "recvonly")[0]
    res.check("vp9enc full colour: the display offers profile 1",
              vp9_fmtp(out_display) == ["profile-id=1"], vp9_fmtp(out_display))
    res.check("vp9enc full colour: the webcam keeps profile 0",
              vp9_fmtp(out_webcam) == ["profile-id=0"], vp9_fmtp(out_webcam))
    res.check("vp9enc: the H.264 profiles are untouched",
              profiles(out_display) == profiles(display[0]), profiles(out_display))

    hevc = app.munge_sdp(raw, "h265enc", True, False)
    out_display = sections(hevc, "video", "sendrecv")[0]
    res.check("h265enc: sps-pps-idr-in-keyframe without an H.264 profile rewrite",
              "sps-pps-idr-in-keyframe=1" in out_display and profiles(out_display) == profiles(display[0]))

    audio_out = sections(full, "audio", "sendrecv")
    mic_out = sections(full, "audio", "recvonly")
    res.check("the Opus ptime sits in the audio the server sends, not the mic section",
              len(audio_out) == 1 and "a=ptime:" in audio_out[0]
              and all("a=ptime:" not in s for s in mic_out), (len(audio_out), len(mic_out)))


def main() -> "H.Results":
    res = H.Results("sdp-munge")
    asyncio.run(scenario(res))
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
