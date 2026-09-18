# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Live readings for the dashboards' gauges: GPU utilization and memory,
universal across vendors, the session's CPU and memory (`SystemUsage`), and
the one sampler both transports run over them (`ResourceMonitor`).

Sources, best-first: NVIDIA reads NVML in-process via ``nvidia-ml-py`` (the API
behind nvitop/nvtop; exact PCI identity, no subprocess per poll) with an
``nvidia-smi`` fallback, and Tegra's integrated GPU, which has neither, reports
its load through devfreq. Every other card is read through DRM: amdgpu counts
utilization and VRAM device-wide in sysfs, and the rest have the engine times in
their clients' fdinfo summed, the kernel's vendor-neutral interface that i915,
xe, Mali, Adreno and VideoCore all write. A card whose driver writes neither --
Apple's, on Asahi -- is listed with no utilization rather than a zero.

``get_gpus(dri_node=...)`` keys the readings to the render node the pipeline
captures/encodes on (PCI match when the source knows its address, else a
vendor-unique match), so the monitored GPU is always the one doing the work.
Objects expose ``.load`` as a 0..1 fraction, None where nothing on the host
counts the card's utilization, and ``.memoryTotal`` / ``.memoryUsed`` in MiB,
the units the stats collectors serialize.
"""

import asyncio
import glob
import logging
import os
import shutil
import subprocess
import time
from typing import Any, Awaitable, Callable, Dict, List, Optional, Tuple

import psutil

try:
    import pynvml
except ImportError:
    pynvml = None

logger = logging.getLogger("stats")

# None = not yet attempted, True = initialized, False = init failed. NVML cannot
# appear after startup, so a failure is never retried or re-logged.
_nvml_ready: Optional[bool] = None

# utilization.gpu is a percentage; memory.* are MiB (nounits strips the suffix);
# pci.bus_id keys the stats to the render node the pipeline encodes on.
_NVIDIA_SMI_QUERY: str = "utilization.gpu,memory.total,memory.used,pci.bus_id"

# Overridable so tests can point at fabricated trees.
_SYSFS_DRM_ROOT: str = "/sys/class/drm"
_PROC_ROOT: str = "/proc"

# Keyed by the driver sysfs names the card's device is bound to, which is the
# platform driver's name on an SoC and need not match the DRM driver's own
# (Broadcom's VideoCore IV binds as vc4-drm). A card whose driver is not here is
# still read when its clients report engine time; only the vendor match
# `dri_node` falls back to needs the keyword.
_DRIVER_VENDORS: Dict[str, str] = {
    "nvidia": "nvidia", "nvidia-drm": "nvidia", "nouveau": "nvidia",
    "amdgpu": "amd", "radeon": "amd",
    "i915": "intel", "xe": "intel",
    "asahi": "apple",
    "panfrost": "arm", "panthor": "arm", "lima": "arm",
    "msm": "qualcomm", "msm-kms": "qualcomm", "adreno": "qualcomm",
    "v3d": "broadcom", "vc4": "broadcom", "vc4-drm": "broadcom",
    "powervr": "imagination", "etnaviv": "vivante",
}

class GPUStat:
    """One GPU's live reading in the units the stats collectors serialize.

    Attributes:
        id: Position of this GPU in the merged detection list.
        load: Utilization as a 0..1 fraction, or None where nothing on the
            host counts it.
        memoryTotal: Total device memory in MiB.
        memoryUsed: Used device memory in MiB.
        pci: Normalized PCI address (lowercase, 4-hex-digit domain), or None
            when the source cannot name one.
        vendor: Vendor keyword (`nvidia`/`amd`/`intel`/`apple`), or None.
    """

    __slots__ = ("id", "load", "memoryTotal", "memoryUsed", "pci", "vendor")

    def __init__(
        self,
        gpu_id: int,
        load: Optional[float],
        memory_total: float,
        memory_used: float,
        pci: Optional[str] = None,
        vendor: Optional[str] = None,
    ) -> None:
        self.id = gpu_id
        self.load = load
        self.memoryTotal = memory_total
        self.memoryUsed = memory_used
        self.pci = pci
        self.vendor = vendor


def _normalize_pci(bus_id: str) -> Optional[str]:
    """Lowercase PCI address with the 4-hex-digit domain sysfs uses
    (nvidia-smi prints an 8-digit domain)."""
    parts = str(bus_id).strip().lower().split(":")
    if len(parts) == 3:
        parts[0] = parts[0][-4:].zfill(4)
        return ":".join(parts)
    return None


def _device_of_node(dri_node: Optional[str], root: Optional[str] = None) -> str:
    """sysfs directory of the device a /dev/dri/card* or renderD* node belongs
    to, the one name both of a card's nodes resolve to."""
    name = os.path.basename(str(dri_node or "").strip())
    if not name:
        return ""
    return os.path.realpath(os.path.join(root or _SYSFS_DRM_ROOT, name, "device"))


def _pci_of_node(dri_node: Optional[str], root: Optional[str] = None) -> Optional[str]:
    """PCI address backing a /dev/dri/renderD* (or card*) node, via sysfs."""
    device = _device_of_node(dri_node, root)
    return _normalize_pci(os.path.basename(device)) if device else None


def _vendor_of_node(dri_node: Optional[str], root: Optional[str] = None) -> Optional[str]:
    """Vendor keyword for the node's kernel driver, via sysfs."""
    root = root or _SYSFS_DRM_ROOT
    name = os.path.basename(str(dri_node or "").strip())
    if not name:
        return None
    try:
        driver = os.path.basename(os.readlink(os.path.join(root, name, "device", "driver")))
    except OSError:
        return None
    return _DRIVER_VENDORS.get(driver)


def _nvml_gpus() -> List[GPUStat]:
    """NVIDIA via NVML: no subprocess, exact per-device PCI identity."""
    global _nvml_ready
    if pynvml is None or _nvml_ready is False:
        return []
    if _nvml_ready is None:
        try:
            pynvml.nvmlInit()
            _nvml_ready = True
        except Exception as exc:
            _nvml_ready = False
            logger.debug("NVML init failed; not retrying: %s", exc)
            return []
    gpus = []
    try:
        for idx in range(pynvml.nvmlDeviceGetCount()):
            handle = pynvml.nvmlDeviceGetHandleByIndex(idx)
            util = pynvml.nvmlDeviceGetUtilizationRates(handle).gpu
            mem = pynvml.nvmlDeviceGetMemoryInfo(handle)
            bus_id = pynvml.nvmlDeviceGetPciInfo(handle).busId
            if isinstance(bus_id, bytes):
                bus_id = bus_id.decode("ascii", "replace")
            gpus.append(
                GPUStat(
                    idx,
                    util / 100.0,
                    mem.total / (1024 * 1024),
                    mem.used / (1024 * 1024),
                    _normalize_pci(bus_id),
                    "nvidia",
                )
            )
    except Exception as exc:
        logger.debug("NVML query failed: %s", exc)
        return []
    return gpus


def _nvidia_gpus() -> List[GPUStat]:
    """NVIDIA via an nvidia-smi subprocess, when NVML is unavailable."""
    smi = shutil.which("nvidia-smi")
    if not smi:
        return []
    try:
        result = subprocess.run(
            [
                smi,
                f"--query-gpu={_NVIDIA_SMI_QUERY}",
                "--format=csv,noheader,nounits",
            ],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        logger.debug("nvidia-smi query failed: %s", exc)
        return []
    if result.returncode != 0:
        return []

    gpus = []
    for idx, line in enumerate(result.stdout.strip().splitlines()):
        parts = [p.strip() for p in line.split(",")]
        if len(parts) < 3:
            continue
        try:
            util = float(parts[0])
            mem_total = float(parts[1])
            mem_used = float(parts[2])
        except ValueError:
            continue
        pci = _normalize_pci(parts[3]) if len(parts) > 3 else None
        gpus.append(GPUStat(idx, util / 100.0, mem_total, mem_used, pci, "nvidia"))
    return gpus


def _read_sysfs_number(path: str) -> Optional[float]:
    """Float value of a sysfs counter file, or None when unreadable."""
    try:
        with open(path, "r") as f:
            return float(f.read().strip())
    except (OSError, ValueError):
        return None


# Utilization for a card whose driver counts nothing device-wide is summed from
# its clients' fdinfo. Finding those clients means walking every process's
# descriptors, which costs far more than reading the few that DRM owns, so the
# walk runs on its own cadence and the readings keep to the list it leaves.
_DRM_CLIENT_RESCAN_S: float = 5.0
_DRM_CLIENT_WINDOW_S: float = 0.1

# NVIDIA's driver counts nothing through DRM, NVML and nvidia-smi being its
# sources, so a host holding only its cards never pays for the walk.
_DRM_UNCOUNTED: Tuple[str, ...] = ("nvidia", "nvidia-drm")

_drm_clients: Dict[str, str] = {}
_drm_clients_at: float = 0.0
_drm_busy: Dict[Tuple[str, str], float] = {}
_drm_busy_at: float = 0.0


def _drm_client_paths(root: Optional[str] = None) -> Dict[str, str]:
    """{fdinfo path: device directory} for every DRM descriptor open on the
    host. Another user's descriptors are unreadable, so their work is missing
    from the total rather than guessed at."""
    clients: Dict[str, str] = {}
    for pid in os.listdir(_PROC_ROOT):
        if not pid.isdigit():
            continue
        try:
            names = os.listdir(f"{_PROC_ROOT}/{pid}/fd")
        except OSError:
            continue
        for name in names:
            try:
                target = os.readlink(f"{_PROC_ROOT}/{pid}/fd/{name}")
            except OSError:
                continue
            if target.startswith("/dev/dri/"):
                device = _device_of_node(target, root)
                if device:
                    clients[f"{_PROC_ROOT}/{pid}/fdinfo/{name}"] = device
    return clients


def _drm_engine_busy(clients: Dict[str, str]) -> Dict[Tuple[str, str], float]:
    """Nanoseconds each device's engines have run, summed over its clients.

    Clients that have exited are dropped from `clients` as they are found, so
    the list never outgrows what is open.
    """
    busy: Dict[Tuple[str, str], float] = {}
    for path, device in list(clients.items()):
        try:
            with open(path, "r") as f:
                lines = f.read().splitlines()
        except OSError:
            clients.pop(path, None)
            continue
        engines: Dict[str, float] = {}
        capacity: Dict[str, float] = {}
        for line in lines:
            key, _, value = line.partition(":")
            if key.startswith("drm-engine-capacity-"):
                name, into = key.removeprefix("drm-engine-capacity-"), capacity
            elif key.startswith("drm-engine-"):
                name, into = key.removeprefix("drm-engine-"), engines
            else:
                continue
            try:
                into[name] = float(value.split()[0])
            except (IndexError, ValueError):
                continue
        for engine, ns in engines.items():
            slot = (device, engine)
            busy[slot] = busy.get(slot, 0.0) + ns / max(1.0, capacity.get(engine, 1.0))
    return busy


def _drm_client_load(root: Optional[str] = None) -> Dict[str, float]:
    """{device directory: utilization} for cards their clients report engine
    time for, taken as the busiest engine's share of the wall time since the
    last reading. The first reading has no predecessor, so it measures a short
    window of its own rather than report a zero it did not observe."""
    global _drm_clients, _drm_clients_at, _drm_busy, _drm_busy_at
    now = time.monotonic()
    if now - _drm_clients_at >= _DRM_CLIENT_RESCAN_S:
        _drm_clients, _drm_clients_at = _drm_client_paths(root), now
    busy = _drm_engine_busy(_drm_clients)
    if busy and not _drm_busy:
        _drm_busy, _drm_busy_at = busy, now
        time.sleep(_DRM_CLIENT_WINDOW_S)
        now, busy = time.monotonic(), _drm_engine_busy(_drm_clients)
    elapsed = (now - _drm_busy_at) * 1e9
    load = {device: 0.0 for device, _ in busy}
    for slot, ns in busy.items():
        ran = ns - _drm_busy.get(slot, ns)
        if elapsed > 0:
            load[slot[0]] = max(load[slot[0]], min(1.0, max(0.0, ran / elapsed)))
    _drm_busy, _drm_busy_at = busy, now
    return load


def _drm_gpus(root: Optional[str] = None) -> List[GPUStat]:
    """Every DRM card, read the best way its driver allows.

    amdgpu counts utilization and VRAM device-wide in sysfs; the rest are read
    from their clients' fdinfo, which is only looked for once a card is found
    that needs it. Mali writes its engine times only while profiling is on, so
    a card left at the driver's default reads as one nothing counts. Memory is reported only where the card has its own and the
    driver counts it, so one drawing on system memory is listed without rather
    than with a share invented for it. A card whose driver we know is listed
    even when nothing counts it, so `dri_node` can still match the one the
    pipeline captures on.
    """
    root = root or _SYSFS_DRM_ROOT
    load: Optional[Dict[str, float]] = None
    gpus = []
    for card in sorted(glob.glob(os.path.join(root, "card[0-9]*"))):
        # Connector nodes (cardN-HDMI-A-1) are not devices.
        if "-" in os.path.basename(card):
            continue
        node = os.path.join(card, "device")
        try:
            driver = os.path.basename(os.readlink(os.path.join(node, "driver")))
        except OSError:
            continue
        device = os.path.realpath(node)
        vendor = _DRIVER_VENDORS.get(driver)
        busy = _read_sysfs_number(os.path.join(node, "gpu_busy_percent"))
        if busy is None and load is None and driver not in _DRM_UNCOUNTED:
            load = _drm_client_load(root)
        util = busy / 100.0 if busy is not None else (load or {}).get(device)
        if vendor is None and util is None:
            continue
        total = _read_sysfs_number(os.path.join(node, "mem_info_vram_total")) or 0.0
        used = _read_sysfs_number(os.path.join(node, "mem_info_vram_used")) or 0.0
        gpus.append(GPUStat(int(os.path.basename(card)[4:]), util,
                            total / (1024 * 1024), used / (1024 * 1024),
                            _normalize_pci(os.path.basename(device)), vendor))
    return gpus


# Tegra's integrated GPU has no DRM node of its own -- the tegra driver is the
# display controller -- and, before JetPack 6, no NVML either; devfreq (Xavier,
# Orin) and the legacy gpu.0 node (TX, Nano) report its load in tenths of a
# percent.
_TEGRA_LOAD_GLOBS: Tuple[str, ...] = tuple(
    f"/sys/class/devfreq/*.{name}/load" for name in ("gpu", "gv11b", "gp10b", "ga10b", "gb10b")
) + ("/sys/devices/gpu.0/load",)


def _tegra_gpus(globs: Optional[Tuple[str, ...]] = None) -> List[GPUStat]:
    """Tegra's integrated GPU, whose memory is the system's rather than its own.

    The first pattern that answers is the card; the rest are the same GPU under
    the names older kernels give it.
    """
    for pattern in globs or _TEGRA_LOAD_GLOBS:
        loads = [v for v in map(_read_sysfs_number, sorted(glob.glob(pattern))) if v is not None]
        if loads:
            return [GPUStat(idx, min(1.0, load / 1000.0), 0.0, 0.0, None, "nvidia")
                    for idx, load in enumerate(loads)]
    return []


def get_gpus(dri_node: Optional[str] = None) -> List[GPUStat]:
    """Return a list of GPUStat objects, one per detected GPU.

    Args:
        dri_node: The render node the pipeline captures/encodes on, e.g.
            `/dev/dri/renderD128`. When given, the list holds ONLY that node's
            GPU so stats always describe the same card the rest of the stack
            uses; an unresolvable node falls back to the full list.

    Returns:
        GPUStat objects with sequential ids, one source per vendor: the best
        that answered, in the order they are tried.
    """
    gpus = _nvml_gpus() or _nvidia_gpus()
    for source in (_tegra_gpus, _drm_gpus):
        present = {g.vendor for g in gpus}
        gpus += [g for g in source() if g.vendor not in present]
    for i, g in enumerate(gpus):
        g.id = i

    if dri_node:
        pci = _pci_of_node(dri_node)
        if pci:
            matched = [g for g in gpus if g.pci == pci]
            if matched:
                return matched
        vendor = _vendor_of_node(dri_node)
        if vendor:
            matched = [g for g in gpus if g.vendor == vendor]
            if len(matched) == 1:
                return matched
        logger.debug("No unique GPU stats source matches %s", dri_node)
    return gpus


CGROUP_ROOT = "/sys/fs/cgroup"
PROC_CGROUP = "/proc/self/cgroup"


def _read(path: str) -> Optional[str]:
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return None


def _stat(path: str, key: str) -> Optional[int]:
    """The value of `key` in a `key value` per line file."""
    for line in (_read(path) or "").splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0] == key and parts[1].isdigit():
            return int(parts[1])
    return None


def _allowed_cores() -> float:
    try:
        return float(len(os.sched_getaffinity(0)))
    except (AttributeError, OSError):
        return float(os.cpu_count() or 1)


class SystemUsage:
    """Samples `(cpu_percent, mem_total_bytes, mem_used_bytes)` for the dashboards' gauges.

    psutil shows a container every processor and byte of the host, though its
    cgroup allows it a share of them, and the host's figures count every other
    tenant too. Where the cgroup the process runs in limits CPU or memory, the
    sampler reads that cgroup: on the unified hierarchy `cpu.stat` and
    `memory.current` against `cpu.max` and `memory.max`, on the legacy one the
    `cpuacct` and `memory` controllers against the CFS quota and the memory limit.
    Memory in use leaves out the page cache the kernel can drop (the inactive file
    pages), the way container tools report it, and CPU is the usage counter's
    growth between two samples over the cores the cgroup may use: its quota, else
    the processors it is allowed. A limit the cgroup does not set is the host's
    total. A cgroup that limits nothing is not the environment, and neither is one
    that cannot be read: the node is, through psutil's system-wide figures.
    """

    #: cgroup limit files, unified then legacy; the cgroup is the environment when one is set.
    LIMITS = ("cpu.max", "memory.max", "cpu.cfs_quota_us", "memory.limit_in_bytes")

    def __init__(self, root: str = CGROUP_ROOT, proc_cgroup: str = PROC_CGROUP,
                 clock: Callable[[], float] = time.monotonic) -> None:
        self._clock = clock
        self._unified = False
        self._cpu_dir: Optional[str] = None
        self._mem_dir: Optional[str] = None
        self._last: Optional[Tuple[float, float]] = None
        for line in (_read(proc_cgroup) or "").splitlines():
            parts = line.split(":", 2)
            if len(parts) != 3:
                continue
            controllers, path = parts[1], parts[2].lstrip("/")
            if controllers == "":
                own = os.path.join(root, path)
                if os.path.isfile(os.path.join(own, "cpu.stat")):
                    self._unified, self._cpu_dir, self._mem_dir = True, own, own
                continue
            names = controllers.split(",")
            # A container that keeps the host's cgroup names sees only its own
            # cgroup at the controller's mount, so that is the fallback.
            for mount in (os.path.join(root, controllers, path), os.path.join(root, controllers)):
                if not os.path.isdir(mount):
                    continue
                if ("cpuacct" in names or "cpu" in names) and self._cpu_dir is None:
                    self._cpu_dir = mount
                if "memory" in names and self._mem_dir is None:
                    self._mem_dir = mount
                break
        if not self._limited():
            self._cpu_dir = self._mem_dir = None

    def _limited(self) -> bool:
        """Whether the cgroup caps CPU or memory below the host."""
        host = psutil.virtual_memory().total
        for d in (self._cpu_dir, self._mem_dir):
            for name in self.LIMITS:
                text = (_read(os.path.join(d, name)) or "") if d else ""
                if text.split() and text.split()[0].isdigit():
                    if name.startswith("cpu") or int(text.split()[0]) < host:
                        return True
        return False

    def _cpu(self) -> Optional[Tuple[float, float]]:
        """The cgroup's CPU time in seconds and the cores it may use."""
        d = self._cpu_dir
        if d is None:
            return None
        if self._unified:
            usec = _stat(os.path.join(d, "cpu.stat"), "usage_usec")
            if usec is None:
                return None
            quota = (_read(os.path.join(d, "cpu.max")) or "").split()
            cores = int(quota[0]) / int(quota[1]) if len(quota) == 2 and quota[0].isdigit() else _allowed_cores()
            return usec / 1e6, cores
        nsec = _read(os.path.join(d, "cpuacct.usage"))
        if nsec is None or not nsec.isdigit():
            return None
        quota, period = _read(os.path.join(d, "cpu.cfs_quota_us")), _read(os.path.join(d, "cpu.cfs_period_us"))
        limited = quota is not None and period is not None and quota.isdigit() and int(quota) > 0 and int(period) > 0
        return int(nsec) / 1e9, int(quota) / int(period) if limited else _allowed_cores()

    def _memory(self) -> Optional[Tuple[int, int]]:
        """The cgroup's memory limit, the host's total without one, and its use."""
        d = self._mem_dir
        if d is None:
            return None
        if self._unified:
            current, limit = _read(os.path.join(d, "memory.current")), _read(os.path.join(d, "memory.max"))
            cache = _stat(os.path.join(d, "memory.stat"), "inactive_file")
        else:
            current, limit = _read(os.path.join(d, "memory.usage_in_bytes")), _read(os.path.join(d, "memory.limit_in_bytes"))
            cache = _stat(os.path.join(d, "memory.stat"), "total_inactive_file")
        if current is None or not current.isdigit():
            return None
        host = psutil.virtual_memory().total
        total = int(limit) if limit is not None and limit.isdigit() and int(limit) < host else host
        return total, max(0, int(current) - (cache or 0))

    def sample(self) -> Tuple[float, int, int]:
        cpu, mem, now = self._cpu(), self._memory(), self._clock()
        if cpu is None:
            percent = psutil.cpu_percent()
        else:
            used, cores = cpu
            percent = 0.0
            if self._last is not None and now > self._last[1]:
                percent = min(100.0, max(0.0, (used - self._last[0]) / (now - self._last[1]) / max(cores, 1e-9) * 100))
            self._last = (used, now)
        if mem is None:
            vm = psutil.virtual_memory()
            mem = (vm.total, vm.used)
        return round(percent, 1), mem[0], mem[1]


class ResourceMonitor:
    """Samples the session's CPU and memory, and the GPU its pipeline encodes
    on, once a period off the event loop, and keeps the latest sample in the
    shapes the pages take.

    `system` is always the last `SystemUsage` sample; `gpu` is the card's last
    reading, None while it cannot be read, and never polled again once the
    first probe finds no GPU, since a vendor tool may be spawned per query.
    `on_tick` runs every period with the time, for whatever a transport does
    on the same cadence; `metrics`, when given, takes the GPU utilization. A
    period is sampled only for someone: `watched`, when set, says whether a
    page has its stats open, and without one and without `metrics` the period
    passes unsampled and `system` and `gpu` read None rather than go stale.
    The GPU is `dri_node`'s card when that narrows the list to one, else the
    `gpu_id`th of the unfiltered list.
    """

    def __init__(self, period: float = 1.0, gpu_id: int = 0, dri_node: str = "",
                 metrics: Optional[Any] = None) -> None:
        self.period = max(1.0, float(period))
        self.gpu_id = gpu_id
        self.dri_node = dri_node
        self.metrics = metrics
        self.system: Optional[Dict[str, Any]] = None
        self.gpu: Optional[Dict[str, Any]] = None
        self.on_tick: Optional[Callable[[float], Awaitable[None]]] = None
        self.watched: Optional[Callable[[], bool]] = None
        self._usage = SystemUsage()
        self._probe_gpu = True
        self._gpu_seen = False
        self._stop: Optional[asyncio.Event] = None
        self._task: Optional[asyncio.Task] = None

    def _gpu_sample(self) -> Optional[Dict[str, Any]]:
        """One GPU reading, or None where there is nothing to read.

        A card is listed for what it is even when it exposes no counters, so
        the pipeline can match it by vendor or PCI address, but a card nothing
        counts and that reports no memory is not a reading: published every
        tick it would leave a page showing a utilization that can never move
        and a memory total of nothing. None instead stops the probe and leaves
        those off the page. A card that is merely idle has a utilization, so it
        keeps reporting.
        """
        gpus = get_gpus(self.dri_node)
        idx = 0 if (self.dri_node and len(gpus) == 1) else self.gpu_id
        if not gpus or not 0 <= idx < len(gpus):
            return None
        gpu = gpus[idx]
        if gpu.load is None and gpu.memoryTotal <= 0:
            return None
        return {
            "gpu_percent": (gpu.load or 0.0) * 100,
            "memory_total": gpu.memoryTotal * 1024 * 1024,
            "memory_used": gpu.memoryUsed * 1024 * 1024,
        }

    def _sample(self) -> Tuple[Dict[str, Any], Optional[Dict[str, Any]]]:
        """One blocking sample of both, for a worker thread."""
        cpu, total, used = self._usage.sample()
        system = {"cpu_percent": cpu, "mem_total": total, "mem_used": used}
        gpu = None
        if self._probe_gpu:
            try:
                gpu = self._gpu_sample()
            except Exception as exc:
                logger.warning(f"GPU stats unavailable this tick: {exc}")
            if gpu is not None:
                self._gpu_seen = True
            elif not self._gpu_seen:
                self._probe_gpu = False
                logger.info(
                    f"No GPU with ID {self.gpu_id} reports utilization or memory; "
                    "GPU stats disabled.")
        return system, gpu

    async def _loop(self) -> None:
        try:
            while self._stop is not None and not self._stop.is_set():
                if self.metrics is not None or self.watched is None or self.watched():
                    self.system, self.gpu = await asyncio.to_thread(self._sample)
                else:
                    self.system = self.gpu = None
                if self.metrics is not None and self.gpu is not None:
                    self.metrics.set_gpu_utilization(self.gpu["gpu_percent"])
                if self.on_tick is not None:
                    await self.on_tick(time.time())
                try:
                    await asyncio.wait_for(self._stop.wait(), timeout=self.period)
                except asyncio.TimeoutError:
                    pass
        except asyncio.CancelledError:
            pass
        except Exception as exc:
            logger.error(f"Resource monitor error: {exc}", exc_info=True)

    def start(self) -> None:
        """Starts sampling on the running loop; a second start is a no-op."""
        if self._task is not None and not self._task.done():
            return
        self._stop = asyncio.Event()
        self._task = asyncio.create_task(self._loop())

    async def stop(self) -> None:
        """Ends the loop at once and waits for it."""
        if self._stop is not None:
            self._stop.set()
        if self._task is not None:
            await self._task
            self._task = None
