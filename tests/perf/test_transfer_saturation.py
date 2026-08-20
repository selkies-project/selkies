#!/usr/bin/env python3
"""File-transfer saturation contracts on a shaped loopback link.

Topology per scenario: streaming page and transfer client reach the server
through tests/tools/bwrelay.py, whose token buckets are the bottleneck link.
The relay bounds SO_RCVBUF toward each receiver so the standing queue lives
in the modeled path — loopback autotuning would otherwise hide it from the
server's congestion gauge. The relay is a rate model: ACK-latency coupling
between the directions only exists on a genuinely shaped link, so the
contracts here are rate caps and fps floors, not latency assertions.

Scenarios:
  download-cc      6 Mbit/s downlink, adaptive pacing only — the stream must
                   keep flowing while a download shares the link.
  download-static  6 Mbit/s downlink, 3 Mbit/s static cap — the cap is
                   honored and the stream stays clean.
  upload-static    6 Mbit/s uplink, 3 Mbit/s cap — upload reads pace the
                   client down to the cap.
  upload-auto      6 Mbit/s uplink behind a fat first-hop buffer, no cap —
                   the uplink allowance holds the upload to a share of what
                   the client can send, so a small request beside it crosses
                   at its unloaded latency (2 ms measured) instead of waiting
                   out the buffer (1421 ms unpaced).
"""
import os
import statistics
import subprocess
import sys
import time
from typing import Callable

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C

from playwright.sync_api import sync_playwright

TOOLS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "tools")
RELAY = os.path.join(TOOLS, "bwrelay.py")
SERVER_PORT = 8085
BLOB_MB = 8

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [saturation] {label}  {detail}", flush=True)


def damage_window() -> subprocess.Popen:
    """Sustained but light screen damage: the video must genuinely use the
    link without demanding all of it, or a 6 Mbit/s scenario has no headroom
    for the transfer under test and measures only its own saturation."""
    return H.spawn(
        ["glxgears", "-geometry", "900x700+100+50"],
        env=dict(os.environ, DISPLAY=H.require_display()),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def small_request_ms() -> float:
    """How long a small request takes right now, in milliseconds."""
    t0 = time.monotonic()
    try:
        code, _ = H.curl("/api/health", timeout=30)
    except Exception:
        return float("nan")
    return (time.monotonic() - t0) * 1000 if code == 200 else float("nan")


def scenario(tag: str, down_kbit: int, up_kbit: int, limit_mbps: float,
             transfer: Callable, rcvbuf: int = 0) -> tuple:
    """Boot server+relay, stream with churn, run `transfer(files_root)` while
    sampling fps and small-request latency once a second.

    `rcvbuf` sizes the relay's modeled first-hop queue; the default models a
    shallow hop. Returns (result, fps_samples, latency_samples)."""
    files_root = os.path.join(H.WORKDIR, "sat-files")
    os.makedirs(files_root, exist_ok=True)
    blob = os.path.join(files_root, "blob.bin")
    if not os.path.exists(blob) or os.path.getsize(blob) != BLOB_MB * 2**20:
        with open(blob, "wb") as f:
            f.write(os.urandom(BLOB_MB * 2**20))
    # The relay listens on the page port before the server boots: helpers'
    # readiness probe reaches the server through it, like every later request.
    relay = H.spawn(
        [sys.executable, RELAY, str(H.PORT), str(SERVER_PORT),
         str(down_kbit), str(up_kbit)],
        env=(dict(os.environ, BWRELAY_RCVBUF=str(rcvbuf)) if rcvbuf else None),
        stdout=open(os.path.join(H.WORKDIR, "bwrelay.log"), "w"),
        stderr=subprocess.STDOUT)
    H.server_start(mode="websockets", wayland=False, port=SERVER_PORT, extra_env={
        "SELKIES_FILE_TRANSFER_LIMIT_MBPS": str(limit_mbps),
        "FILE_MANAGER_PATH": files_root,
    })
    churn = damage_window()
    fps_samples = []
    latency_samples = []
    try:
        with sync_playwright() as p:
            browser = C.launch_browser(p, "chromium")
            ctx = browser.new_context(viewport={"width": 1280, "height": 720},
                                      device_scale_factor=1)
            ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
            page = ctx.new_page()
            page.goto(H.BASE_URL + "/", wait_until="load")
            info = C.wait_ws_video(page)
            check(f"{tag}: stream up", info is not None, info)
            page.wait_for_timeout(3000)
            done = {"result": None}

            import threading
            th = threading.Thread(
                target=lambda: done.update(result=transfer(files_root)))
            th.start()
            while th.is_alive():
                fps_samples.append(page.evaluate("window.fps"))
                latency_samples.append(small_request_ms())
                time.sleep(1)
            th.join()
            browser.close()
    finally:
        churn.terminate()
        relay.terminate()
        H.server_stop()
    return done["result"], fps_samples, latency_samples


def timed_curl(args: list) -> tuple:
    t0 = time.monotonic()
    rc = subprocess.run(["curl", "-sS", "-o", "/dev/null", *args],
                        timeout=180).returncode
    dt = time.monotonic() - t0
    return (BLOB_MB * 8 / dt if rc == 0 else 0.0), dt


def fps_health(fps_samples: list) -> tuple:
    mid = fps_samples[2:-1] if len(fps_samples) > 4 else fps_samples
    stall = sum(1 for f in mid if not f or f < 10) / max(1, len(mid))
    return (statistics.median(mid) if mid else 0), stall


def main() -> int:
    dl = lambda root: timed_curl([f"{H.BASE_URL}/api/files/blob.bin"])

    result, fps, _lat = scenario("download-cc", 6000, 0, 0, dl)
    mbps, dt = result
    med, stall = fps_health(fps)
    check("download-cc: download completes through the shared link",
          mbps > 1.0, f"{mbps:.2f} Mbit/s in {dt:.0f}s")
    check("download-cc: stream keeps flowing beside the download",
          med >= 30 and stall <= 0.25,
          f"median fps={med} stall_frac={stall:.2f} samples={fps[:12]}")

    result, fps, _lat = scenario("download-static", 6000, 0, 3, dl)
    mbps, dt = result
    med, stall = fps_health(fps)
    check("download-static: static cap is honored",
          2.0 <= mbps <= 3.8, f"{mbps:.2f} Mbit/s")
    check("download-static: stream stays clean under the cap",
          med >= 45 and stall <= 0.10,
          f"median fps={med} stall_frac={stall:.2f}")

    up = lambda root: timed_curl([
        "-X", "POST", "-H", "X-Upload-Path: sat-upload.bin",
        "--data-binary", "@" + os.path.join(root, "blob.bin"),
        f"{H.BASE_URL}/api/upload"])

    result, fps, _lat = scenario("upload-static", 50000, 6000, 3, up)
    mbps, dt = result
    med, stall = fps_health(fps)
    check("upload-static: upload reads pace the client to the cap",
          2.2 <= mbps <= 3.8, f"{mbps:.2f} Mbit/s")
    check("upload-static: stream unaffected on the wide downlink",
          med >= 45 and stall <= 0.10,
          f"median fps={med} stall_frac={stall:.2f}")

    # A fat first-hop buffer is what turns an upload into session-wide lag:
    # 1 MiB at 6 Mbit/s is well over a second of queue for anything the client
    # sends behind it.
    result, fps, lat = scenario("upload-auto", 50000, 6000, 0, up,
                                rcvbuf=1024 * 1024)
    mbps, dt = result
    clean = sorted(v for v in lat if v == v)
    median_ms = statistics.median(clean) if clean else float("nan")
    check("upload-auto: the upload takes its share of the uplink and no more",
          2.5 <= mbps <= 4.5, f"{mbps:.2f} Mbit/s")
    check("upload-auto: a small request beside it is not stuck behind the queue",
          median_ms < 250, f"median={median_ms:.0f}ms samples={len(clean)}")

    print(f"[saturation] {passed}/{passed + failed} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
