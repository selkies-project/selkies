#!/usr/bin/env python3
"""Utilization for the cards no vendor tool reads, off the kernel's own counters.

amdgpu counts its cards device-wide in sysfs and Tegra reports its integrated
GPU through devfreq, but for everything else -- i915 and xe, Mali, Adreno,
VideoCore, Asahi -- the only universal source is the engine time the driver
writes into each client's fdinfo. What is pinned here is the arithmetic that
turns those running totals into a utilization: the window it is measured over,
the engine it is taken from, the clients it is summed across, and the identity
that joins a render node to the card it belongs to. A wrong reading here is a
gauge that moves for the wrong reason, so the trees are fabricated and the
clock is fake, and every number below is one the test states in full.

Driven against fabricated /sys and /proc trees; no GPU needed.
"""
import os
import shutil
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

import helpers as H  # noqa: E402
from selkies import resource_stats as RS  # noqa: E402

res = H.Results("drm-gpu-stats")
ROOT = tempfile.mkdtemp(prefix="drm-stats-")
GIB = 1024 * 1024 * 1024


class Clock:
    """Monotonic time the test advances itself, so a window is exactly as long
    as the check says it is."""

    def __init__(self) -> None:
        self.now = 1000.0

    def monotonic(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += seconds


def card(name: str, driver: str, pci: str, **files: str) -> str:
    """A DRM card in the fabricated tree, with its render node beside it.

    Returns the device directory both nodes resolve to, which is the key the
    fdinfo readings are aggregated under.
    """
    device = os.path.join(ROOT, "sys", "devices", "pci0000:00", pci)
    os.makedirs(os.path.join(device, "driver_dir", driver), exist_ok=True)
    driver_link = os.path.join(device, "driver")
    if not os.path.islink(driver_link):
        os.symlink(os.path.join("driver_dir", driver), driver_link)
    for key, value in files.items():
        with open(os.path.join(device, key), "w") as f:
            f.write(value)
    for node in (name, name.replace("card", "renderD1")):
        node_dir = os.path.join(ROOT, "sys", "class", "drm", node)
        os.makedirs(node_dir, exist_ok=True)
        link = os.path.join(node_dir, "device")
        if not os.path.islink(link):
            os.symlink(device, link)
    return device


def client(pid: int, fd: int, node: str, **engines: float) -> None:
    """One process holding that DRM node open, its engines at those totals in
    nanoseconds. A capacity is given as `engine__capacity`."""
    fds = os.path.join(ROOT, "proc", str(pid), "fd")
    info = os.path.join(ROOT, "proc", str(pid), "fdinfo")
    os.makedirs(fds, exist_ok=True)
    os.makedirs(info, exist_ok=True)
    link = os.path.join(fds, str(fd))
    if not os.path.islink(link):
        os.symlink(f"/dev/dri/{node}", link)
    lines = ["pos:\t0", "drm-driver:\tfabricated", f"drm-client-id:\t{pid}"]
    for name, value in engines.items():
        if name.endswith("__capacity"):
            lines.append(f"drm-engine-capacity-{name[:-10]}:\t{int(value)}")
        else:
            lines.append(f"drm-engine-{name}:\t{int(value)} ns")
    with open(os.path.join(info, str(fd)), "w") as f:
        f.write("\n".join(lines) + "\n")


def reset(clock: Clock) -> None:
    """Forget every reading, the way a fresh process starts."""
    RS._drm_clients, RS._drm_clients_at = {}, 0.0
    RS._drm_busy, RS._drm_busy_at = {}, 0.0
    RS.time = clock


real_time, real_drm_root, real_proc_root = RS.time, RS._SYSFS_DRM_ROOT, RS._PROC_ROOT
RS._SYSFS_DRM_ROOT = os.path.join(ROOT, "sys", "class", "drm")
RS._PROC_ROOT = os.path.join(ROOT, "proc")
MS = 1000 * 1000

try:
    clock = Clock()
    reset(clock)

    # A card read only through its clients: Mali, through the render node.
    mali = card("card0", "panthor", "0000:03:00.0")
    engines = dict(panthor=0, render=0, video=0, video__capacity=2)
    client(4001, 3, "renderD10", **engines)

    first = RS._drm_client_load()
    res.check("a card its clients report is keyed by the device both its nodes share",
              list(first) == [mali], first)
    res.check("and an idle one reads zero, which is a measurement, not an absence",
              first.get(mali) == 0.0, first)

    clock.now += 0.5
    engines["panthor"] = 250 * MS
    client(4001, 3, "renderD10", **engines)
    res.check("a quarter second of engine time over half a second is half the card",
              RS._drm_client_load().get(mali) == 0.5)

    # The busiest engine is the reading, not the sum: a card whose render and
    # video engines each ran would otherwise read over 100% between them.
    clock.now += 0.5
    engines["render"] = 400 * MS
    client(4001, 3, "renderD10", **engines)
    res.check("the busiest engine is the reading, not the engines added up",
              RS._drm_client_load().get(mali) == 0.8)

    # Identical engines the driver counts under one key: capacity divides.
    clock.now += 0.5
    engines["video"] = 500 * MS
    client(4001, 3, "renderD10", **engines)
    res.check("two engines under one key share the window between them",
              RS._drm_client_load().get(mali) == 0.5)

    # Two clients on one card add up, and no client can push it past full.
    client(4002, 7, "card0", panthor=0)
    RS._drm_clients_at = 0.0  # a new client is seen at the next rescan
    RS._drm_client_load()
    clock.now += 0.5
    engines["panthor"] = 550 * MS
    client(4001, 3, "renderD10", **engines)
    client(4002, 7, "card0", panthor=400 * MS)
    res.check("the clients of one card are summed, and the sum stops at full",
              RS._drm_client_load().get(mali) == 1.0)

    # A client that exits leaves nothing behind.
    shutil.rmtree(os.path.join(ROOT, "proc", "4002"))
    clock.now += 0.5
    RS._drm_client_load()
    res.check("a client that exits is dropped from the list it was found in",
              len(RS._drm_clients) == 1, RS._drm_clients)
    res.check("and from the totals carried between readings",
              {device for device, _ in RS._drm_busy} == {mali}
              and len(RS._drm_busy) == 3, sorted(RS._drm_busy))

    # What the collector makes of those cards.
    amd = card("card1", "amdgpu", "0000:04:00.0", gpu_busy_percent="37",
               mem_info_vram_total=str(8 * GIB), mem_info_vram_used=str(2 * GIB))
    card("card2", "nouveau", "0000:05:00.0")
    card("card3", "ast", "0000:12:00.0")
    client(4003, 5, "renderD11", gfx=0)
    RS._drm_clients_at = 0.0
    clock.now += 0.5
    gpus = {g.pci: g for g in RS._drm_gpus()}

    res.check("a card counted device-wide is read there, not from its clients",
              gpus["0000:04:00.0"].load == 0.37 and gpus["0000:04:00.0"].vendor == "amd",
              gpus.get("0000:04:00.0"))
    res.check("with its memory in MiB",
              gpus["0000:04:00.0"].memoryTotal == 8192
              and gpus["0000:04:00.0"].memoryUsed == 2048)
    res.check("a card on shared memory is listed without any of its own",
              gpus["0000:03:00.0"].memoryTotal == 0
              and gpus["0000:03:00.0"].vendor == "arm")
    res.check("a card nothing counts is listed for matching, with no utilization",
              gpus["0000:05:00.0"].load is None and gpus["0000:05:00.0"].vendor == "nvidia",
              gpus.get("0000:05:00.0"))
    res.check("a device that is not a GPU and has no engines is not a card",
              "0000:12:00.0" not in gpus, sorted(gpus))

    # Tegra, whose load devfreq reports in tenths of a percent.
    tegra = os.path.join(ROOT, "sys", "class", "devfreq", "17000000.ga10b")
    os.makedirs(tegra)
    with open(os.path.join(tegra, "load"), "w") as f:
        f.write("642\n")
    legacy = os.path.join(ROOT, "sys", "devices", "gpu.0")
    os.makedirs(legacy)
    with open(os.path.join(legacy, "load"), "w") as f:
        f.write("1000\n")
    globs = (os.path.join(ROOT, "sys", "class", "devfreq", "*.ga10b", "load"),
             os.path.join(legacy, "load"))
    found = RS._tegra_gpus(globs)
    res.check("Tegra's tenths of a percent are a fraction of one",
              len(found) == 1 and found[0].load == 0.642 and found[0].vendor == "nvidia",
              [(g.load, g.vendor) for g in found])
    res.check("and the same GPU under an older name is not a second card",
              RS._tegra_gpus(globs[1:])[0].load == 1.0)

    # A Jetson's DRM node belongs to its display controller, so the GPU's own
    # reading has to reach the list ahead of anything that node contributes.
    real = RS._TEGRA_LOAD_GLOBS, RS._nvml_gpus, RS._nvidia_gpus
    RS._TEGRA_LOAD_GLOBS, RS._nvml_gpus, RS._nvidia_gpus = globs, list, list
    try:
        merged = RS.get_gpus()
    finally:
        RS._TEGRA_LOAD_GLOBS, RS._nvml_gpus, RS._nvidia_gpus = real
    nvidia = [g for g in merged if g.vendor == "nvidia"]
    res.check("on a board whose GPU only devfreq counts, that is the one reading of it",
              len(nvidia) == 1 and nvidia[0].load == 0.642,
              [(g.load, g.vendor, g.pci) for g in merged])
finally:
    RS.time, RS._SYSFS_DRM_ROOT, RS._PROC_ROOT = real_time, real_drm_root, real_proc_root
    shutil.rmtree(ROOT, ignore_errors=True)

sys.exit(0 if res.summary() else 1)
