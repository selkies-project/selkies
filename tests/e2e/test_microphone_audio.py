#!/usr/bin/env python3
"""The microphone uplink's sound: a browser's microphone, switched on through
the dashboard's pipeline control, must come out of the server's recordable
virtual source carrying what the browser captured — over the WebSocket
(WebCodecs Opus in 0x02 frames) and over WebRTC (the Opus track on the
reserved sendonly transceiver) — and go quiet again when switched off.
Chromium captures a WAV of a known tone as its fake microphone, so the PCM
recorded from the server's PulseAudio/PipeWire source is checked for that
tone (a Goertzel detector against the signal's RMS), not merely for a source
that exists. The operator lock (``microphone_enabled=false|locked``) must
withhold the uplink on both transports. Audio is independent of the capture
backend, so the blocks run against the X test display only.

    python3 tests/e2e/test_microphone_audio.py websockets|webrtc|locked
"""
import math
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from array import array
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

TONE_HZ = 1000
CAPTURE_RATE = 48000
# The recordable source the server provisions; PipeWire's pulse server prefixes
# virtual sources with the sink they hang off, PulseAudio does not.
VIRTUAL_MIC = "SelkiesVirtualMic"

MIC_JS = """
  window.__micStatus = [];
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || !d.type) return;
    if ((d.type === 'sidebarButtonStatusUpdate' || d.type === 'pipelineStatusUpdate') && d.microphone !== undefined) {
      window.__micStatus.push(!!d.microphone);
    }
  });
"""


def tone_wav(path: str, seconds: int = 2, amplitude: int = 11000) -> None:
    """A mono 16-bit WAV of TONE_HZ for Chromium's fake audio capture, which
    loops the file for as long as the microphone is open."""
    import wave
    frames = bytearray()
    for i in range(CAPTURE_RATE * seconds):
        frames += struct.pack("<h", int(amplitude * math.sin(2 * math.pi * TONE_HZ * i / CAPTURE_RATE)))
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(CAPTURE_RATE)
        w.writeframes(bytes(frames))


def sources() -> list:
    """Names of the sources the sound server currently exposes."""
    out = subprocess.run(["pactl", "list", "short", "sources"], capture_output=True, text=True).stdout
    return [line.split("\t")[1] for line in out.splitlines() if "\t" in line]


def virtual_mic_source(timeout: float = 25) -> Optional[str]:
    """The virtual microphone source once the server has provisioned it."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        for name in sources():
            if name.endswith(VIRTUAL_MIC):
                return name
        time.sleep(0.5)
    return None


def record(source: str, seconds: float = 2.0) -> array:
    """Mono s16 PCM recorded from `source` for `seconds`, as an array of samples.

    parec records until it is stopped, so the run's timeout is what ends it,
    and the output drained up to then is the recording.
    """
    cmd = ["parec", f"--device={source}", "--format=s16le", f"--rate={CAPTURE_RATE}",
           "--channels=1", "--raw", "--latency-msec=50"]
    try:
        data = subprocess.run(cmd, capture_output=True, timeout=seconds).stdout
    except subprocess.TimeoutExpired as stopped:
        data = stopped.stdout or b""
    samples = array("h")
    samples.frombytes(data[:len(data) - len(data) % 2])
    return samples


def analyse(samples: array) -> dict:
    """RMS of the recording and how much of it is the tone.

    A Goertzel filter at TONE_HZ measures the tone's amplitude; against the
    RMS that gives the fraction of the signal that is the tone, close to 1 for
    the tone alone and near 0 for silence or noise. Pure Python on purpose: a
    second of audio is a trivial loop and the suites take no numeric stack.
    """
    n = min(len(samples), CAPTURE_RATE)
    if n < CAPTURE_RATE // 4:
        return {"samples": len(samples), "rms": 0.0, "tone": 0.0, "ratio": 0.0}
    window = samples[len(samples) - n:]
    coeff = 2.0 * math.cos(2.0 * math.pi * TONE_HZ / CAPTURE_RATE)
    s1 = s2 = 0.0
    energy = 0.0
    for x in window:
        s0 = x + coeff * s1 - s2
        s2, s1 = s1, s0
        energy += x * x
    power = s1 * s1 + s2 * s2 - coeff * s1 * s2
    tone_amplitude = 2.0 * math.sqrt(max(power, 0.0)) / n
    rms = math.sqrt(energy / n)
    tone_rms = tone_amplitude / math.sqrt(2.0)
    return {"samples": len(samples), "rms": round(rms, 1), "tone": round(tone_rms, 1),
            "ratio": round(tone_rms / rms, 3) if rms else 0.0}


def toggle(page, enabled: bool) -> None:
    page.evaluate(f"window.postMessage({{type: 'pipelineControl', pipeline: 'microphone', enabled: {str(enabled).lower()}}}, window.location.origin)")


def wait_status(page, value: bool, timeout: float = 20) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        statuses = page.evaluate("window.__micStatus")
        if statuses and statuses[-1] == value:
            return True
        time.sleep(0.25)
    return False


def launch(p, wav: str, mode: str):
    """Chromium with the tone as its microphone, on the stream page.

    The headless shell has no media capture; the full Chromium build (new
    headless mode) or the system Chrome named by E2E_CHROME is needed.
    """
    args = C.BROWSER_ARGS + ["--use-fake-device-for-media-stream", f"--use-file-for-fake-audio-capture={wav}"]
    kw = {"headless": True, "args": args}
    if C.CHROME_PATH:
        kw["executable_path"] = C.CHROME_PATH
    else:
        kw["channel"] = "chromium"
    browser = p.chromium.launch(**kw)
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, permissions=["microphone"])
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(MIC_JS)
    page = ctx.new_page()
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page, errors


def wait_video(page, mode: str):
    return C.wait_ws_video(page, timeout=30) if mode == "websockets" else C.wait_wr_video(page)


def transport_block(mode: str) -> "H.Results":
    res = H.Results(f"microphone-audio-{mode}")
    wav = os.path.join(tempfile.mkdtemp(prefix="selkies-mic-"), "tone.wav")
    tone_wav(wav)
    H.server_start(mode=mode, wayland=False)
    try:
        with sync_playwright() as p:
            browser, page, errors = launch(p, wav, mode)
            res.check("stream up", bool(wait_video(page, mode)))
            before = [s for s in sources() if s.endswith(VIRTUAL_MIC)]
            toggle(page, True)
            res.check("microphone reports active", wait_status(page, True), str(page.evaluate("window.__micStatus")))
            source = virtual_mic_source()
            res.check("virtual microphone source provisioned on first mic data",
                      source is not None and not before, f"{source} (before: {before})")
            if source:
                # pcmflux opens the playback stream off the loop once the first
                # chunk arrives; the sound server then has to route it through the
                # virtual source before the tone can be heard there.
                time.sleep(2.0)
                got = analyse(record(source))
                res.check("recorded PCM carries the browser's tone",
                          got["rms"] > 150 and got["ratio"] > 0.5, str(got))
            toggle(page, False)
            res.check("microphone reports inactive", wait_status(page, False), str(page.evaluate("window.__micStatus")))
            if source:
                # Frames already in flight land for a moment after the stop.
                time.sleep(1.5)
                got = analyse(record(source))
                res.check("source goes quiet after disable", got["rms"] < 50, str(got))
            res.check("no page errors", not errors, "; ".join(errors)[:200])
            browser.close()
    finally:
        H.server_stop()
    return res


def locked_block() -> "H.Results":
    res = H.Results("microphone-audio-locked")
    wav = os.path.join(tempfile.mkdtemp(prefix="selkies-mic-"), "tone.wav")
    tone_wav(wav)
    for mode in ("websockets", "webrtc"):
        H.server_start(mode=mode, wayland=False, extra_env={"SELKIES_MICROPHONE_ENABLED": "false|locked"})
        try:
            with sync_playwright() as p:
                browser, page, _ = launch(p, wav, mode)
                res.check(f"{mode}: stream up", bool(wait_video(page, mode)))
                toggle(page, True)
                time.sleep(5)
                statuses = page.evaluate("window.__micStatus")
                res.check(f"{mode}: client settles on microphone off", not statuses or statuses[-1] is False, str(statuses))
                res.check(f"{mode}: no virtual microphone provisioned",
                          not [s for s in sources() if s.endswith(VIRTUAL_MIC)], str(sources()))
                browser.close()
        finally:
            H.server_stop()
    return res


def main() -> int:
    for tool in ("pactl", "parec"):
        if not shutil.which(tool):
            H.skip_suite(f"{tool} is not installed")
    sel = sys.argv[1] if len(sys.argv) > 1 else "websockets"
    ok = locked_block().summary() if sel == "locked" else transport_block(sel).summary()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
