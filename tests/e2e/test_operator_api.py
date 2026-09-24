#!/usr/bin/env python3
"""The operator API end to end: sessions, recording, and screenshots, with the
audit trail they leave.

websockets / webrtc:
    A controller page and a shared viewer page connect; `/api/sessions` lists
    both with their roles, an RFC 3339 connection time, and a measured round
    trip; disconnecting the viewer by id closes it, and the collector hears a
    connect for each page and the disconnect with its length. A recording
    started with no body lands in the file-manager directory as a fragmented
    MP4 that grows while the desktop is captured, its stop reports the frames
    and bytes written, and both are on the audit trail; a screenshot is a PNG
    the size of the display. Over websockets the block also runs the
    view-only credential against every endpoint.
wayland:
    The same screenshot and recording against the Wayland backend.

Usage: python3 tests/e2e/test_operator_api.py [websockets|webrtc|wayland|all]
"""
import base64
import http.client
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "integration"))
import helpers as H
import core_lib as C
from array import array
from test_audit_webhook import Collector
from test_microphone_audio import CAPTURE_RATE, analyze, tone_wav
from playwright.sync_api import sync_playwright

FILES_DIR = os.path.join(H.WORKDIR, "operator-files")
BASIC_USER, BASIC_PASSWORD, VIEWONLY_PASSWORD = "user", "secret", "look"
RFC3339 = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$")


def request(method: str, path: str, body: Optional[bytes] = None, headers: Optional[dict] = None) -> tuple:
    """One request against the test server: `(status, body bytes)`."""
    conn = http.client.HTTPConnection("localhost", H.PORT, timeout=60)
    try:
        conn.request(method, path, body=body, headers=dict(headers or {}))
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def api(method: str, path: str, body: Optional[dict] = None, headers: Optional[dict] = None) -> tuple:
    """A JSON request: `(status, decoded body or None)`."""
    extra = dict(headers or {})
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        extra["Content-Type"] = "application/json"
    status, raw = request(method, path, data, extra)
    try:
        return status, json.loads(raw) if raw else None
    except ValueError:
        return status, None


def basic(password: str) -> dict:
    return {"Authorization": "Basic " + base64.b64encode(f"{BASIC_USER}:{password}".encode()).decode()}


def server_env(collector: Collector) -> dict:
    env = {"SELKIES_FILE_MANAGER_PATH": FILES_DIR, "SELKIES_AUDIT_WEBHOOK_URL": collector.url}
    # A pixelflux on PYTHONPATH is the one under test rather than the installed one.
    if os.environ.get("PYTHONPATH"):
        env["PYTHONPATH"] = os.environ["PYTHONPATH"]
    return env


def new_page(pw: Any, browser: Any, mode: str, url_hash: str = "") -> Any:
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return page


def wait_video(page: Any, mode: str) -> Optional[dict]:
    return C.wait_wr_video(page, timeout=45) if mode == "webrtc" else C.wait_ws_video(page, timeout=30)


def sessions() -> list:
    status, body = api("GET", "/api/sessions")
    return (body or {}).get("sessions", []) if status == 200 else []


def wait_sessions(count: int, timeout: float = 20) -> list:
    deadline = time.time() + timeout
    while time.time() < deadline:
        found = sessions()
        if len(found) >= count:
            return found
        time.sleep(0.5)
    return sessions()


def wait_measured(count: int, timeout: float = 20) -> list:
    """The listing once `count` pages are on it with a round trip measured:
    WebRTC reports one only after the page's first receiver report."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        found = sessions()
        if len(found) >= count and all(isinstance(s["rtt_ms"], (int, float)) for s in found):
            return found
        time.sleep(0.5)
    return sessions()


def wait_gone(session_id: str, timeout: float = 15) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if session_id not in {s["id"] for s in sessions()}:
            return True
        time.sleep(0.5)
    return False


def events(collector: Collector, kind: str) -> list:
    return [e for e in collector.events if e.get("event") == kind]


def wait_events(collector: Collector, kind: str, count: int, timeout: float = 15) -> list:
    deadline = time.time() + timeout
    while time.time() < deadline and len(events(collector, kind)) < count:
        time.sleep(0.2)
    return events(collector, kind)


def mp4_boxes(path: str) -> list:
    """The top-level box types of an MP4 file, in order."""
    boxes = []
    with open(path, "rb") as f:
        while True:
            head = f.read(8)
            if len(head) < 8:
                break
            size, kind = struct.unpack(">I4s", head)
            boxes.append(kind.decode("latin-1"))
            if size == 1:
                size = struct.unpack(">Q", f.read(8))[0]
                f.seek(size - 16, 1)
            elif size == 0:
                break
            else:
                f.seek(size - 8, 1)
    return boxes


def png_size(data: bytes) -> Optional[tuple]:
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        return None
    return struct.unpack(">II", data[16:24])


def play_tone(seconds: int) -> Optional[subprocess.Popen]:
    """Plays the microphone suite's tone into the session's sink, the one the
    recorder's audio capture reads, for `seconds`."""
    paplay, pulse = shutil.which("paplay"), H.pulse_server()
    if not paplay or not pulse:
        return None
    wav = os.path.join(tempfile.mkdtemp(prefix="selkies-rec-"), "tone.wav")
    tone_wav(wav, seconds=seconds)
    return subprocess.Popen([paplay, "--server", pulse, "--device", "output", wav],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def audio_track(path: str) -> Optional[dict]:
    """The file's first audio stream as ffprobe decodes it, with the tone
    analysis of its second second as mono PCM."""
    ffprobe, ffmpeg = shutil.which("ffprobe"), shutil.which("ffmpeg")
    if not ffprobe or not ffmpeg:
        return None
    probe = subprocess.run([ffprobe, "-v", "error", "-count_frames", "-select_streams", "a:0",
                            "-show_entries", "stream=codec_name,channels,nb_read_frames", "-of", "csv=p=0", path],
                           capture_output=True, text=True, timeout=120).stdout.strip().split(",")
    pcm = subprocess.run([ffmpeg, "-v", "error", "-i", path, "-vn", "-f", "s16le", "-ac", "1",
                          "-ar", str(CAPTURE_RATE), "-"], capture_output=True, timeout=120).stdout
    samples = array("h")
    samples.frombytes(pcm[:len(pcm) - len(pcm) % 2])
    return {"codec": probe[0] if probe else "", "channels": int(probe[1]) if len(probe) > 1 else 0,
            "frames": int(probe[2]) if len(probe) > 2 and probe[2].isdigit() else 0,
            "tone": analyze(samples[CAPTURE_RATE:2 * CAPTURE_RATE])}


def recording_round(res: "H.Results", label: str, collector: Collector, expect_size: Optional[tuple] = None) -> None:
    """One recording, with a tone playing into the session's sink while it
    runs, and one screenshot against the running server."""
    status, started = api("POST", "/api/recording")
    res.check(f"{label}: a recording starts into the file-manager directory",
              status == 200 and started and started["active"] and os.path.dirname(started["path"]) == FILES_DIR
              and re.fullmatch(r"recording-\d{8}T\d{6}Z\.mp4", os.path.basename(started["path"])), (status, started))
    path = (started or {}).get("path", "")
    status, again = api("POST", "/api/recording")
    res.check(f"{label}: a second start is a conflict", status == 409, status)
    tone = play_tone(3)
    time.sleep(4)
    status, live = api("GET", "/api/recording")
    res.check(f"{label}: the status shows the recording growing",
              status == 200 and live["active"] and live["frames"] > 0 and live["bytes"] > 0, live)
    status, final = api("DELETE", "/api/recording")
    res.check(f"{label}: stopping reports the frames and bytes written",
              status == 200 and final and not final["active"] and final["frames"] > 0 and final["bytes"] > 0
              and final["duration_s"] >= 2 and not final.get("error"), final)
    res.check(f"{label}: the file is a fragmented MP4 that plays from the first frame",
              os.path.isfile(path) and mp4_boxes(path)[:2] == ["ftyp", "moov"] and "moof" in mp4_boxes(path)
              and os.path.getsize(path) >= final["bytes"], mp4_boxes(path)[:6] if os.path.isfile(path) else "missing")
    ffprobe = shutil.which("ffprobe")
    if ffprobe:
        probe = subprocess.run([ffprobe, "-v", "error", "-count_frames", "-select_streams", "v:0",
                                "-show_entries", "stream=codec_name,nb_read_frames", "-of", "csv=p=0", path],
                               capture_output=True, text=True, timeout=120).stdout.strip()
        res.check(f"{label}: ffprobe decodes every frame of it as H.264",
                  probe.startswith("h264,") and probe.split(",")[1].isdigit() and int(probe.split(",")[1]) == final["frames"],
                  (probe, final["frames"]))
    else:
        res.skip(f"{label}: ffprobe decodes every frame of it as H.264", "no ffprobe on PATH")
    audio = audio_track(path) if os.path.isfile(path) else None
    if tone is None:
        res.skip(f"{label}: the session's audio is in the file as an Opus track", "no paplay or sound server")
    elif audio is None:
        res.skip(f"{label}: the session's audio is in the file as an Opus track", "no ffprobe or ffmpeg on PATH")
    else:
        tone.wait(timeout=10)
        res.check(f"{label}: the session's audio is in the file as an Opus track, every packet the status counted",
                  audio["codec"] == "opus" and audio["channels"] == 2 and audio["frames"] == final.get("audio_frames") > 0,
                  (audio["codec"], audio["channels"], audio["frames"], final.get("audio_frames")))
        res.check(f"{label}: the track spans the recording at one packet per 20 ms",
                  abs(audio["frames"] * 0.02 - final["duration_s"]) < 1.0, (audio["frames"], final["duration_s"]))
        res.check(f"{label}: the tone played while it recorded is what the track holds",
                  audio["tone"]["ratio"] > 0.8 and audio["tone"]["rms"] > 500, audio["tone"])
    status, _ = api("DELETE", "/api/recording")
    res.check(f"{label}: stopping nothing is a conflict", status == 409, status)
    starts, stops = wait_events(collector, "recording.start", 1), wait_events(collector, "recording.stop", 1)
    res.check(f"{label}: the audit carries the start and the stop with the file's figures",
              starts and starts[-1]["filename"] == path and stops
              and stops[-1]["filename"] == path and stops[-1]["frames"] == final["frames"]
              and stops[-1]["size_bytes"] == final["bytes"], (starts[-1:], stops[-1:]))
    status, png = request("GET", "/api/screenshot")
    size = png_size(png)
    res.check(f"{label}: a screenshot is a PNG of the display",
              status == 200 and size is not None and (expect_size is None or size == expect_size), (status, size, expect_size))


def transport_block(mode: str) -> "H.Results":
    res = H.Results(f"operator-{mode}")
    shutil.rmtree(FILES_DIR, ignore_errors=True)
    os.makedirs(FILES_DIR)
    collector = Collector()
    H.server_start(mode=mode, wayland=False, extra_env=server_env(collector))
    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        try:
            owner = new_page(pw, browser, mode)
            viewer = new_page(pw, browser, mode, "#shared")
            res.check("the controller page streams", wait_video(owner, mode) is not None)
            res.check("the viewer page streams", wait_video(viewer, mode) is not None)
            listed = wait_sessions(2)
            roles = sorted(s["role"] for s in listed)
            res.check("both pages are listed with their roles and transport",
                      roles == ["controller", "viewer"] and all(s["transport"] == mode for s in listed), listed)
            listed = wait_measured(2)
            res.check("each listing carries an RFC 3339 connection time and a measured round trip",
                      all(RFC3339.match(s["connected_at"] or "") for s in listed)
                      and all(isinstance(s["rtt_ms"], (int, float)) and 0 <= s["rtt_ms"] < 5000 for s in listed),
                      [(s["role"], s["connected_at"], s["rtt_ms"]) for s in listed])
            connects = wait_events(collector, "session.connect", 2)
            res.check("the audit heard both connections with their roles",
                      sorted(e["role"] for e in connects[-2:]) == ["controller", "viewer"]
                      and all(e["transport"] == mode for e in connects[-2:]), connects[-2:])
            viewer_id = next((s["id"] for s in listed if s["role"] == "viewer"), "")
            status, _ = request("DELETE", f"/api/sessions/{viewer_id}")
            res.check("the viewer is disconnected by id", status == 204 and wait_gone(viewer_id), status)
            ended = wait_events(collector, "session.disconnect", 1)
            res.check("the audit heard the disconnect with the connection's length",
                      ended and ended[-1]["role"] == "viewer" and ended[-1]["transport"] == mode
                      and ended[-1]["duration_s"] > 0, ended[-1:])
            status, _ = request("DELETE", "/api/sessions/nope")
            res.check("an unknown session is not found", status == 404, status)
            recording_round(res, mode, collector, expect_size=tuple(H.x_root_size()))
            viewer.context.close()
            owner.context.close()
        finally:
            browser.close()
    collector.stop()

    if mode == "websockets":
        collector = Collector()
        H.server_start(mode=mode, wayland=False, extra_env={
            **server_env(collector), "SELKIES_ENABLE_BASIC_AUTH": "true",
            "SELKIES_BASIC_AUTH_USER": BASIC_USER, "SELKIES_BASIC_AUTH_PASSWORD": BASIC_PASSWORD,
            "SELKIES_BASIC_AUTH_VIEWONLY_PASSWORD": VIEWONLY_PASSWORD})
        look, own = basic(VIEWONLY_PASSWORD), basic(BASIC_PASSWORD)
        status, _ = request("GET", "/api/sessions", headers=look)
        res.check("view-only: may list sessions", status == 200, status)
        status, _ = request("GET", "/api/screenshot", headers=look)
        res.check("view-only: may take a screenshot", status == 200, status)
        status, _ = request("POST", "/api/recording", headers=look)
        res.check("view-only: may not record", status == 403, status)
        status, _ = request("DELETE", "/api/sessions/x", headers=look)
        res.check("view-only: may not disconnect", status == 403, status)
        status, _ = request("GET", "/api/sessions")
        res.check("no credential is challenged", status == 401, status)
        status, started = api("POST", "/api/recording", headers=own)
        res.check("the main password records", status == 200 and started["active"], status)
        status, _ = api("DELETE", "/api/recording", headers=own)
        res.check("and stops", status == 200, status)
        collector.stop()
    res.summary()
    return res


def wayland_block() -> "H.Results":
    res = H.Results("operator-wayland")
    shutil.rmtree(FILES_DIR, ignore_errors=True)
    os.makedirs(FILES_DIR)
    collector = Collector()
    try:
        H.server_start(mode="websockets", wayland=True, extra_env=server_env(collector))
    except RuntimeError as exc:
        res.skip("the Wayland backend starts", str(exc)[:120])
        res.summary()
        return res
    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        try:
            page = new_page(pw, browser, "websockets")
            res.check("a page streams the Wayland session", wait_video(page, "websockets") is not None)
            recording_round(res, "wayland", collector)
            status, png = request("GET", "/api/screenshot?display=display9")
            res.check("an output the compositor does not have is not found", status == 404, status)
        finally:
            browser.close()
    collector.stop()
    res.summary()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    try:
        if which in ("all", "websockets"):
            blocks.append(transport_block("websockets"))
        if which in ("all", "webrtc"):
            blocks.append(transport_block("webrtc"))
        if which in ("all", "wayland"):
            blocks.append(wayland_block())
    finally:
        H.server_stop()
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== OPERATOR API: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
