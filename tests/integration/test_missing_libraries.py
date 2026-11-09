#!/usr/bin/env python3
"""A host without one of the libraries the server opens at run time still runs it.

The server opens its GPU stack and a few desktop libraries with dlopen when a path
needs them: EGL and GL, CUDA, NVENC, NvFBC, NVML, VA-API, Vulkan, X11 and XCB (NvFBC
and the GPU probe), libwayland-server (the Wayland backend) and PipeWire. Where one is
missing, the path that wanted it declines and the session goes on without it. A loader
that panics instead takes the whole server down on such a host, and a machine that has
every library never shows it: smithay's EGL loader did, through `selkies gpu-probe` and
the Wayland compositor's GPU bring-up.

tests/tools/hide_libs.so makes dlopen fail for the libraries named, so this host runs
the paths of one without them. With each hidden, and with all of them at once:
`selkies gpu-probe` answers and exits 0, and an X11 and a Wayland session over
WebSockets start, send frames and stay up, with no panic in the server's log. Without
libwayland-server there is no Wayland backend at all: that session's server says so
and stays up. The same runs with nothing hidden are the control that the shim alone
changes nothing. The GPU paths run only where there is a render node, so without one
the sessions run with nothing and with everything hidden alone.

    python3 tests/integration/test_missing_libraries.py
"""
import asyncio
import glob
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

try:
    import websockets
except ImportError:
    H.skip_suite("websockets is not installed")

# What each name hides: file name prefixes, as hide_libs.c matches them.
LIBRARIES = {
    "EGL": "libEGL",
    "GL": "libGL:libOpenGL",
    "CUDA": "libcuda",
    "NVENC": "libnvidia-encode",
    "NvFBC": "libnvidia-fbc",
    "NVML": "libnvidia-ml",
    "VA-API": "libva",
    "Vulkan": "libvulkan",
    "X11 and XCB": "libX11:libxcb",
    "libwayland-server": "libwayland-server",
    "PipeWire": "libpipewire",
}
EVERYTHING = ":".join(LIBRARIES.values())
# What the Wayland compositor's thread logs where it cannot start.
NO_COMPOSITOR = "compositor thread aborting"
SETTINGS = {
    "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
    "manual_resolution": False, "framerate": 30, "encoder": "h264enc",
    "video_crf": 25, "video_bitrate": 4000, "audio_bitrate": 128000,
    "scaling_dpi": 96, "displayPosition": "right",
}
# A video stripe's type byte: JPEG, H.264.
FRAME_TYPES = (0x03, 0x04)
FRAME_WAIT_S = 20
# How long the server is watched after its first frame, for a panic on a later path.
SETTLE_S = 3


def build_shim() -> str:
    subprocess.run(["make", "-s", "-C", H.TOOLS, "hide_libs.so"],
                   check=True, capture_output=True, text=True)
    return os.path.join(H.TOOLS, "hide_libs.so")


def hiding(shim: str, libraries: str) -> dict:
    env = {"LD_PRELOAD": shim, "HIDE_LIBS": libraries}
    # A server built against a conda environment's libgbm finds the host's GBM
    # backends only where it is pointed at them.
    if os.environ.get("GBM_BACKENDS_PATH"):
        env["GBM_BACKENDS_PATH"] = os.environ["GBM_BACKENDS_PATH"]
    return env


def probe(res: H.Results, name: str, env: dict) -> None:
    run_env = {"PATH": os.environ.get("PATH", ""), "HOME": os.path.expanduser("~"),
               "XDG_RUNTIME_DIR": H.RUNTIME_DIR, **H.inherited_env(), **env}
    if H.TEST_DISPLAY:
        run_env["DISPLAY"] = H.TEST_DISPLAY
    os.makedirs(H.RUNTIME_DIR, exist_ok=True)
    out = subprocess.run([H.SERVER, "gpu-probe"], capture_output=True, text=True,
                         timeout=180, env=run_env)
    text = out.stdout + out.stderr
    res.check(f"{name} hidden: gpu-probe answers without a panic",
              out.returncode == 0 and "panicked" not in text and "SELKIES_GPU_PRESENT=" in out.stdout,
              f"exit {out.returncode}: " + (text[text.find("panicked"):][:240] if "panicked" in text
                                           else text.strip()[-240:]))


async def frames(seconds: float) -> int:
    """Video stripes the server sends a page within `seconds`, read on past the first for
    SETTLE_S so a panic on the path after it has its moment."""
    async with websockets.connect(f"ws://localhost:{H.PORT}/api/websockets", max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("SETTINGS," + json.dumps(SETTINGS))
        count, end = 0, time.monotonic() + seconds
        while time.monotonic() < end:
            try:
                m = await asyncio.wait_for(ws.recv(), timeout=0.5)
            except asyncio.TimeoutError:
                continue
            if isinstance(m, (bytes, bytearray)) and m and m[0] in FRAME_TYPES:
                if not count:
                    end = min(end, time.monotonic() + SETTLE_S)
                count += 1
        return count


def session(res: H.Results, name: str, env: dict, wayland: bool) -> None:
    backend = "Wayland" if wayland else "X11"
    no_backend = wayland and "libwayland-server" in env["HIDE_LIBS"].split(":")
    try:
        proc = H.server_start(mode="websockets", wayland=wayland, extra_env=env)
    except RuntimeError as e:
        log = H.server_log()
        res.check(f"{name} hidden: a {backend} server starts", False,
                  log[log.find("panicked"):][:240] if "panicked" in log else str(e))
        return
    try:
        try:
            sent = asyncio.run(frames(FRAME_WAIT_S))
        except Exception as e:  # noqa: BLE001 - a dropped socket is the finding
            sent = f"{type(e).__name__}: {e}"
        log = H.server_log()
        if no_backend:
            res.check(f"{name} hidden: the {backend} server says it has no compositor",
                      NO_COMPOSITOR in log, sent)
        else:
            res.check(f"{name} hidden: a {backend} session sends frames",
                      isinstance(sent, int) and sent > 0, sent)
        res.check(f"{name} hidden: the {backend} server is up and logged no panic",
                  proc.poll() is None and "panicked" not in log,
                  log[log.find("panicked"):][:240] if "panicked" in log else f"exit {proc.poll()}")
    finally:
        H.server_stop()


def main() -> bool:
    res = H.Results("missing-libraries")
    try:
        shim = build_shim()
    except (OSError, subprocess.CalledProcessError) as e:
        H.skip_suite(f"hide_libs.so does not build here: {e}")
    gpu = bool(glob.glob("/dev/dri/renderD*"))
    sets = {"nothing": "", **LIBRARIES, "every library": EVERYTHING}
    for name, libraries in sets.items():
        probe(res, name, hiding(shim, libraries))
    alone = {"nothing": "", "every library": EVERYTHING}
    for name, libraries in (sets if gpu else alone).items():
        for wayland in (False, True):
            session(res, name, hiding(shim, libraries), wayland)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
