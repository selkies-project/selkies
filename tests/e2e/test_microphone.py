#!/usr/bin/env python3
"""The microphone uplink end to end: a browser's microphone, switched on through
the dashboard's pipeline control, must bring up the SelkiesVirtualMic on the
server and feed it -- over the WebSocket (0x02 Opus frames) and over WebRTC (the
audio track on the reserved sendonly transceiver) -- through the shared
in-process sound-server control plane: no ``pactl`` process is forked on
either transport. Chromium's fake audio device stands in for a microphone.

    python3 tests/e2e/test_microphone.py websockets|webrtc
"""
import os
import shutil
import subprocess
import sys
import time
from typing import List, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

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


def pactl(*args: str) -> str:
    exe = shutil.which("pactl")
    if not exe:
        return ""
    r = subprocess.run([exe, *args], capture_output=True, text=True, timeout=10,
                       env=dict(os.environ, LC_ALL="C"))
    return r.stdout


def virtual_mic_sources() -> List[str]:
    return [line.split("\t")[1] for line in pactl("list", "short", "sources").splitlines()
            if len(line.split("\t")) > 1 and VIRTUAL_MIC in line.split("\t")[1]]


def virtual_mic_modules() -> List[str]:
    return [line.split("\t")[0] for line in pactl("list", "short", "modules").splitlines()
            if "module-virtual-source" in line and VIRTUAL_MIC in line]


def unload_leftover_virtual_mic() -> None:
    """A SelkiesVirtualMic a previous run left behind would be reused rather than
    created, hiding the provisioning under test; unload it first."""
    for module in virtual_mic_modules():
        pactl("unload-module", module)


def pcmflux_playback_sink() -> Optional[str]:
    """Index of the sink pcmflux's mic playback stream feeds, or None."""
    sink = None
    for block in pactl("list", "sink-inputs").split("Sink Input #")[1:]:
        if 'application.name = "pcmflux"' in block:
            for line in block.splitlines():
                if line.strip().startswith("Sink:"):
                    sink = line.split(":", 1)[1].strip()
    return sink


def sink_index(name: str) -> Optional[str]:
    for line in pactl("list", "short", "sinks").splitlines():
        parts = line.split("\t")
        if len(parts) > 1 and parts[1] == name:
            return parts[0]
    return None


def default_source() -> str:
    for line in pactl("info").splitlines():
        if line.startswith("Default Source:"):
            return line.split(":", 1)[1].strip()
    return ""


def wait_for(pred, timeout: float, interval: float = 0.5) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pred():
            return True
        time.sleep(interval)
    return bool(pred())


def toggle(page, enabled: bool) -> None:
    page.evaluate(f"window.postMessage({{type: 'pipelineControl', pipeline: 'microphone', "
                  f"enabled: {str(enabled).lower()}}}, window.location.origin)")


def wait_status(page, value: bool, timeout: float = 20) -> bool:
    return wait_for(lambda: (page.evaluate("window.__micStatus") or [None])[-1] is value, timeout, 0.25)


def launch(p, mode: str):
    # The headless shell has no media capture; the full Chromium build (new
    # headless mode) or the system Chrome named by E2E_CHROME is needed. The
    # fake device plays a tone into getUserMedia.
    args = C.BROWSER_ARGS + ["--use-fake-device-for-media-stream"]
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
    page.on("dialog", lambda d: (errors.append(f"dialog: {d.message}"), d.dismiss()))
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page, errors


def transport_block(mode: str) -> "H.Results":
    res = H.Results(f"microphone-{mode}")
    unload_leftover_virtual_mic()
    H.server_start(mode=mode, wayland=False)
    try:
        with sync_playwright() as p:
            browser, page, errors = launch(p, mode)
            try:
                video = C.wait_ws_video(page) if mode == "websockets" else C.wait_wr_video(page)
                res.check("stream up", bool(video), str(video)[:100])
                if mode == "webrtc" and video:
                    res.check("audio track negotiated", video.get("audio", 0) >= 1, video.get("audio"))
                if mode == "webrtc":
                    # The WebRTC audio start validates the capture device through
                    # the shared control plane before pcmflux opens it.
                    res.check("wr: capture device resolved in-process",
                              C.wait_log("Configured audio device 'output.monitor' is valid.", timeout=15), "")
                else:
                    res.check("ws: mic control connected at the handshake",
                              C.wait_log("Sound server control ready for the microphone (pulsectl)", timeout=10), "")
                toggle(page, True)
                res.check("client reports the microphone active", wait_status(page, True),
                          str(page.evaluate("window.__micStatus")))
                res.check("virtual mic provisioned (server log)",
                          C.wait_log(f"Virtual microphone '{VIRTUAL_MIC}' is ready", timeout=20), "")
                res.check("pcmflux capture routing verified in-process",
                          C.wait_log("pcmflux correctly connected to 'output.monitor'", timeout=10), "")
                res.check("virtual mic source appears", wait_for(lambda: bool(virtual_mic_sources()), 10),
                          virtual_mic_sources())
                res.check("virtual mic is the default source",
                          wait_for(lambda: VIRTUAL_MIC in default_source(), 10), default_source())
                input_sink = sink_index("input")
                res.check("pcmflux mic playback feeds the 'input' sink",
                          wait_for(lambda: pcmflux_playback_sink() == input_sink and input_sink is not None, 15),
                          (pcmflux_playback_sink(), input_sink))
                toggle(page, False)
                res.check("client reports the microphone inactive", wait_status(page, False),
                          str(page.evaluate("window.__micStatus")))
                res.check("no page errors", not errors, "; ".join(errors)[:200])
            finally:
                browser.close()
        if mode == "websockets":
            # The socket that loaded the module unloads it when it goes away.
            res.check("ws: module unloaded on disconnect",
                      C.wait_log("Unloading PulseAudio module", timeout=10)
                      and wait_for(lambda: not virtual_mic_sources(), 10), virtual_mic_sources())
        res.check("no pactl fallback engaged", C.wait_log_absent("pactl", timeout=1), "")
        res.check("no sound-server control failures",
                  C.wait_log_absent("Sound server control failed", timeout=1)
                  and C.wait_log_absent("Sound server did not answer", timeout=1), "")
    finally:
        H.server_stop()
    if mode == "webrtc":
        # The service unloads its module in the graceful shutdown SIGTERM triggers.
        res.check("webrtc: module unloaded at shutdown",
                  wait_for(lambda: not virtual_mic_sources(), 10), virtual_mic_sources())
    unload_leftover_virtual_mic()
    return res


def main() -> int:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    if which in ("all", "websockets"):
        blocks.append(transport_block("websockets"))
    if which in ("all", "webrtc"):
        blocks.append(transport_block("webrtc"))
    ok = all(b.summary() for b in blocks)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
