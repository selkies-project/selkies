#!/usr/bin/env python3
"""The offer's codec rewrites reach the display's own video section and no other.

The offer bundles the display's sendonly video with the recvonly webcam
section the browser encodes. Full color rewrites the H.264 profile to High
4:4:4 and VP9 to profile 1, and H.264 or H.265 gets `sps-pps-idr-in-keyframe`,
all of it describing the stream the server sends. A browser handed High 4:4:4
for the camera it is asked to encode rejects that section and stops its
sender, so the webcam section has to keep the profiles it offered. Driven
against a real offer from RTCApp (a loopback peer connection with stubbed
signaling), then through munge_sdp with each encoder and color setting.

A surround session offers `multiopus` ahead of stereo RED and Opus on the
audio it sends, and the client's `takeMultiopus` (lib/webrtc.js, run under
node) puts it first in an answer the way an engine that decodes it keeps it,
on that section alone; the microphone's section offers only what its sink
decodes, RED and Opus first and no multiopus.
"""
import asyncio
import json
import os
import re
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.webrtc_engine import RTCApp
from selkies.webrtc import codecs as rtc_codecs

WEBRTC_JS = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
                         "addons/selkies-web-core/lib/webrtc.js")


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
    res.check("h264enc full color: the display's profiles are all High 4:4:4",
              set(profiles(out_display)) == {"f4001f"}, profiles(out_display))
    res.check("h264enc full color: the webcam keeps the profiles it offered",
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
    res.check("vp9enc full color: the display offers profile 1",
              vp9_fmtp(out_display) == ["profile-id=1"], vp9_fmtp(out_display))
    res.check("vp9enc full color: the webcam keeps profile 0",
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


async def offer_of_app() -> str:
    """One offer from a loopback RTCApp, as the browser receives it."""
    loop = asyncio.get_running_loop()
    app = RTCApp(async_event_loop=loop, encoder="h264enc", stun_servers=[], turn_servers=[])
    app.start_display_media = _nothing
    app.stop_display_media = _nothing
    app.on_sdp = _nothing
    app.on_ice = _nothing
    offers: list = []
    munge = app.munge_sdp
    app.munge_sdp = lambda sdp, *a, **k: (offers.append(sdp), munge(sdp, *a, **k))[1]
    await app.start_rtc_connection("s1", "viewer", None, "primary")
    try:
        return app.munge_sdp(offers[0], "h264enc", False, False)
    finally:
        await app.stop_rtc_connection("s1", "viewer")


def engine_answer(offer: str) -> str:
    """An answer the way the engines write one to a surround offer: CRLF lines,
    and every section's codecs but multiopus, which none lists on its own."""
    out = []
    for section in re.split(r'(?m)(?=^m=)', re.sub(r'\r?\n', '\r\n', offer)):
        pt = re.search(r'(?m)^a=rtpmap:(\d+) multiopus/', section)
        if pt:
            lines = [ln for ln in section.split("\r\n")
                     if not re.match(rf'a=(rtpmap|fmtp|rtcp-fb):{pt.group(1)} ', ln)]
            m = lines[0].split(" ")
            lines[0] = " ".join(m[:3] + [f for f in m[3:] if f != pt.group(1)])
            section = "\r\n".join(lines)
        out.append(section.replace("a=sendrecv", "a=recvonly-answer")
                   .replace("a=recvonly\r", "a=sendonly\r").replace("a=recvonly-answer", "a=recvonly"))
    return "".join(out)


def take_multiopus(offer: str, answer: str) -> str:
    """lib/webrtc.js `takeMultiopus` on the pair, under node."""
    src = open(WEBRTC_JS).read()
    fn = re.search(r'export function takeMultiopus\(offer, answer\) \{.*?\n\}\n', src, re.S).group(0)
    driver = fn.replace("export function", "function") + \
        "\nconst io = JSON.parse(require('fs').readFileSync(0, 'utf8'));" \
        "\nprocess.stdout.write(takeMultiopus(io.offer, io.answer));"
    r = subprocess.run(["node", "-e", driver], input=json.dumps({"offer": offer, "answer": answer}).encode(),
                       capture_output=True, timeout=60)
    if r.returncode:
        raise RuntimeError(r.stderr.decode().strip()[:300])
    return r.stdout.decode()


async def surround(res: H.Results) -> None:
    saved = list(rtc_codecs.CODECS["audio"])
    rtc_codecs.configure_multiopus(6)
    try:
        offer = await offer_of_app()
    finally:
        rtc_codecs.CODECS["audio"] = saved
    sent = sections(offer, "audio", "sendrecv")
    mic = sections(offer, "audio", "recvonly")
    fmt = lambda section: section.splitlines()[0].split(" ")[3:]
    names = lambda section: {pt: name for pt, name in re.findall(r'(?m)^a=rtpmap:(\d+) ([^/]+)/', section)}
    res.check("surround: the audio sent offers multiopus first, then RED and Opus",
              len(sent) == 1 and [names(sent[0])[pt] for pt in fmt(sent[0])[:3]] == ["multiopus", "red", "opus"],
              [names(sent[0]).get(pt) for pt in fmt(sent[0])] if sent else None)
    res.check("surround: multiopus holds a payload type no video codec has",
              sent and not set(fmt(sent[0])[:1]) & set(re.findall(r'(?m)^a=rtpmap:(\d+) ', "".join(sections(offer, "video", "sendrecv")))))
    res.check("surround: the microphone's section offers RED and Opus first, and no multiopus",
              len(mic) == 1 and [names(mic[0]).get(pt) for pt in fmt(mic[0])[:2]] == ["red", "opus"]
              and "multiopus" not in names(mic[0]).values(),
              [names(mic[0]).get(pt) for pt in fmt(mic[0])] if mic else None)
    if not shutil.which("node"):
        res.check("node available for takeMultiopus", False, "node not found")
        return
    answer = engine_answer(offer)
    taken = take_multiopus(offer, answer)
    got = sections(taken, "audio", "recvonly")
    res.check("takeMultiopus: the answer's audio line takes multiopus first, with its rtpmap and fmtp",
              len(got) == 1 and names(got[0]).get(fmt(got[0])[0]) == "multiopus"
              and re.search(rf'(?m)^a=fmtp:{fmt(got[0])[0]} .*num_streams=4', got[0]) is not None,
              fmt(got[0]) if got else None)
    res.check("takeMultiopus: the microphone's section is left alone",
              sections(taken, "audio", "sendonly") == sections(answer, "audio", "sendonly"))
    res.check("takeMultiopus: an offer without multiopus leaves the answer as it was",
              take_multiopus(answer, answer) == answer)


def main() -> "H.Results":
    res = H.Results("sdp-munge")
    asyncio.run(scenario(res))
    asyncio.run(surround(res))
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
