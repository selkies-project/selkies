# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Live readings for the dashboards' gauges: GPU utilization and memory,
universal across vendors, the session's CPU and memory (`SystemUsage`), and
the one sampler both transports run over them (`ResourceMonitor`).

Sources, best-first per vendor: NVIDIA reads NVML in-process via ``nvidia-ml-py``
(the API behind nvitop/nvtop; exact PCI identity, no subprocess per poll) with a
``nvidia-smi`` fallback; every other vendor (AMD, Intel, Apple) comes from the
``aitop`` monitors (rocm-smi/amd-smi, intel_gpu_top); the amdgpu sysfs counters
(``gpu_busy_percent`` + ``mem_info_vram_*``) backfill AMD hosts without ROCm
tooling, and i915/xe cards without a readable counter report load/memory 0 —
listed, and honest about what the kernel provides.

``get_gpus(dri_node=...)`` keys the readings to the render node the pipeline
captures/encodes on (PCI match when the source knows its address, else a
vendor-unique match), so the monitored GPU is always the one doing the work.
Objects expose ``.load`` as a 0..1 fraction and ``.memoryTotal`` /
``.memoryUsed`` in MiB, the units the stats collectors serialize.
"""

import asyncio
import glob
import logging
import os
import shutil
import subprocess
import threading
import time
from datetime import datetime
from typing import Any, Awaitable, Callable, Dict, List, Optional, Set, Tuple

import psutil

try:
    import pynvml
except ImportError:
    pynvml = None

try:
    from aitop.core.gpu.factory import GPUMonitorFactory
except Exception:
    GPUMonitorFactory = None

logger = logging.getLogger("stats")

# GPU presence is reported through this logger; aitop re-emits vendor detection
# at INFO on every factory build, so keep its detection chatter off the stream.
if GPUMonitorFactory is not None:
    logging.getLogger("aitop.core.gpu.factory").setLevel(logging.WARNING)

# None = not yet attempted, True = initialized, False = init failed. NVML cannot
# appear after startup, so a failure is never retried or re-logged.
_nvml_ready: Optional[bool] = None

# utilization.gpu is a percentage; memory.* are MiB (nounits strips the suffix);
# pci.bus_id keys the stats to the render node the pipeline encodes on.
_NVIDIA_SMI_QUERY: str = "utilization.gpu,memory.total,memory.used,pci.bus_id"

# Overridable so tests can point at a fabricated tree.
_SYSFS_DRM_ROOT: str = "/sys/class/drm"

_DRIVER_VENDORS: Dict[str, str] = {"nvidia": "nvidia", "amdgpu": "amd", "radeon": "amd", "i915": "intel", "xe": "intel"}

class GPUStat:
    """One GPU's live reading in the units the stats collectors serialize.

    Attributes:
        id: Position of this GPU in the merged detection list.
        load: Utilization as a 0..1 fraction.
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
        load: float,
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


def _pci_of_node(dri_node: Optional[str], root: Optional[str] = None) -> Optional[str]:
    """PCI address backing a /dev/dri/renderD* (or card*) node, via sysfs."""
    root = root or _SYSFS_DRM_ROOT
    name = os.path.basename(str(dri_node or "").strip())
    if not name:
        return None
    dev = os.path.realpath(os.path.join(root, name, "device"))
    return _normalize_pci(os.path.basename(dev))


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


def _aitop_vendor(monitor: Any) -> str:
    """Vendor keyword derived from an aitop monitor's class name."""
    return type(monitor).__name__.replace("GPUMonitor", "").replace("NPUMonitor", "").lower()


# aitop monitor -> the CLI its readings come from. Without the tool a monitor can
# never produce data, and the NVIDIA/AMD ones log an ERROR every poll when a
# DRM-visible card has no userspace in the container; NPU/Apple read other sources.
_AITOP_MONITOR_TOOLS: Dict[str, tuple] = {
    "NvidiaGPUMonitor": ("nvidia-smi",),
    "AMDGPUMonitor": ("rocm-smi", "amd-smi"),
    "IntelGPUMonitor": ("intel_gpu_top",),
}


def _aitop_monitor_usable(monitor: Any) -> bool:
    """Whether the monitor's backing CLI exists, logging the skip when not."""
    tools = _AITOP_MONITOR_TOOLS.get(type(monitor).__name__)
    if tools is None or any(shutil.which(tool) for tool in tools):
        return True
    logger.info(
        "%s GPU visible but %s not installed; skipping its stats monitor",
        _aitop_vendor(monitor),
        "/".join(tools),
    )
    return False


_aitop_monitors_cache: Optional[List[Any]] = None
_aitop_monitors_lock = threading.Lock()


def _aitop_monitors() -> List[Any]:
    """aitop monitors, built once and reused.

    create_monitors() re-detects vendors and appends to PATH on every call, so
    polling it per frame grows PATH until subprocess spawns fail with E2BIG. A
    build failure is cached as an empty list for the same reason — retrying
    each poll would keep growing PATH; NVML/sysfs still cover the stats.
    """
    global _aitop_monitors_cache
    if _aitop_monitors_cache is None:
        with _aitop_monitors_lock:
            if _aitop_monitors_cache is None:
                try:
                    _aitop_monitors_cache = [
                        monitor
                        for monitor in GPUMonitorFactory.create_monitors()
                        if _aitop_monitor_usable(monitor)
                    ]
                except Exception as exc:
                    logger.warning("aitop monitor detection failed: %s", exc)
                    _aitop_monitors_cache = []
    return _aitop_monitors_cache


def _aitop_gpus(vendors: Optional[Set[str]] = None) -> List[GPUStat]:
    """Multi-vendor telemetry via aitop's monitors (utilization + memory in MiB).

    All-zero readings with no PCI identity are dropped as fabricated
    placeholders (aitop's Intel monitor emits them on hosts with no Intel
    GPU); a real but unreadable card resurfaces through the sysfs backfill
    with a true PCI address that `dri_node` matching can use.

    Args:
        vendors: When given, only monitors for these vendor keywords are read.
    """
    if GPUMonitorFactory is None:
        return []
    gpus = []
    try:
        for monitor in _aitop_monitors():
            vendor = _aitop_vendor(monitor)
            if vendors is not None and vendor not in vendors:
                continue
            for info in monitor.get_gpu_info() or []:
                if not info.utilization and not info.memory_total and not info.memory_used:
                    continue
                gpus.append(
                    GPUStat(
                        len(gpus),
                        float(info.utilization or 0.0) / 100.0,
                        float(info.memory_total or 0.0),
                        float(info.memory_used or 0.0),
                        None,
                        vendor,
                    )
                )
    except Exception as exc:
        logger.debug("aitop query failed: %s", exc)
        return []
    return gpus


def _read_sysfs_number(path: str) -> Optional[float]:
    """Float value of a sysfs counter file, or None when unreadable."""
    try:
        with open(path, "r") as f:
            return float(f.read().strip())
    except (OSError, ValueError):
        return None


def _drm_sysfs_gpus(root: Optional[str] = None) -> List[GPUStat]:
    """AMD (and best-effort Intel) cards via /sys/class/drm/card*/device counters.

    i915/xe expose no unprivileged utilization or VRAM counters, so those
    cards are listed with zeros rather than left out.
    """
    root = root or _SYSFS_DRM_ROOT
    gpus = []
    for card in sorted(glob.glob(os.path.join(root, "card[0-9]*"))):
        # Connector nodes (cardN-HDMI-A-1) are not devices.
        if "-" in os.path.basename(card):
            continue
        device = os.path.join(card, "device")
        try:
            driver = os.path.basename(os.readlink(os.path.join(device, "driver")))
        except OSError:
            continue
        idx = int(os.path.basename(card)[4:])
        pci = _normalize_pci(os.path.basename(os.path.realpath(device)))
        if driver in ("amdgpu", "radeon"):
            busy = _read_sysfs_number(os.path.join(device, "gpu_busy_percent"))
            vram_total = _read_sysfs_number(os.path.join(device, "mem_info_vram_total"))
            vram_used = _read_sysfs_number(os.path.join(device, "mem_info_vram_used"))
            gpus.append(
                GPUStat(
                    idx,
                    (busy or 0.0) / 100.0,
                    (vram_total or 0.0) / (1024 * 1024),
                    (vram_used or 0.0) / (1024 * 1024),
                    pci,
                    "amd",
                )
            )
        elif driver in ("i915", "xe"):
            gpus.append(GPUStat(idx, 0.0, 0.0, 0.0, pci, "intel"))
    return gpus


def get_gpus(dri_node: Optional[str] = None) -> List[GPUStat]:
    """Return a list of GPUStat objects, one per detected GPU.

    Args:
        dri_node: The render node the pipeline captures/encodes on, e.g.
            `/dev/dri/renderD128`. When given, the list holds ONLY that node's
            GPU so stats always describe the same card the rest of the stack
            uses; an unresolvable node falls back to the full list.

    Returns:
        GPUStat objects with sequential ids, merged best-source-first per
        vendor and backfilled from sysfs.
    """
    gpus = _nvml_gpus()
    if gpus:
        gpus += _aitop_gpus(vendors={"amd", "intel", "apple"})
    else:
        gpus = _aitop_gpus() or _nvidia_gpus()
    present = {g.vendor for g in gpus}
    gpus += [g for g in _drm_sysfs_gpus() if g.vendor not in present]
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
    `on_tick` runs after each sample with the time, for whatever a transport
    sends on the same cadence; `metrics`, when given, takes the GPU utilization.
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
        self._usage = SystemUsage()
        self._probe_gpu = True
        self._stop: Optional[asyncio.Event] = None
        self._task: Optional[asyncio.Task] = None

    def _gpu_sample(self) -> Optional[Dict[str, Any]]:
        """One GPU reading, or None where there is nothing to read.

        A card is listed for what it is even when it exposes no counters, so
        the pipeline can match it by vendor or PCI address, but a reading of
        nothing but zeros is not a reading: published every tick it would leave
        a page showing a utilization that can never move and a memory total of
        nothing. None instead stops the probe and leaves those off the page.
        """
        gpus = get_gpus(self.dri_node)
        idx = 0 if (self.dri_node and len(gpus) == 1) else self.gpu_id
        if not gpus or not 0 <= idx < len(gpus):
            return None
        gpu = gpus[idx]
        if gpu.load <= 0 and gpu.memoryTotal <= 0:
            return None
        return {
            "type": "gpu_stats",
            "timestamp": datetime.now().isoformat(),
            "gpu_id": self.gpu_id,
            "load": gpu.load,
            "gpu_percent": gpu.load * 100,
            "memory_total": gpu.memoryTotal * 1024 * 1024,
            "memory_used": gpu.memoryUsed * 1024 * 1024,
        }

    def _sample(self) -> Tuple[Dict[str, Any], Optional[Dict[str, Any]]]:
        """One blocking sample of both, for a worker thread."""
        cpu, total, used = self._usage.sample()
        system = {"type": "system_stats", "timestamp": datetime.now().isoformat(),
                  "cpu_percent": cpu, "mem_total": total, "mem_used": used}
        gpu = None
        if self._probe_gpu:
            try:
                gpu = self._gpu_sample()
            except Exception as exc:
                logger.warning(f"GPU stats unavailable this tick: {exc}")
            if gpu is None and self.gpu is None:
                self._probe_gpu = False
                logger.info(
                    f"No GPU with ID {self.gpu_id} reports utilization or memory; "
                    "GPU stats disabled.")
        return system, gpu

    async def _loop(self) -> None:
        try:
            while self._stop is not None and not self._stop.is_set():
                self.system, self.gpu = await asyncio.to_thread(self._sample)
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
