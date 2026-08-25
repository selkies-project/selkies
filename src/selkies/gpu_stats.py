# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Live GPU utilization and memory readings, universal across vendors.

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

import glob
import logging
import os
import shutil
import subprocess
import threading
from typing import Any, Dict, List, Optional, Set

try:
    import pynvml
except ImportError:
    pynvml = None

try:
    from aitop.core.gpu.factory import GPUMonitorFactory
except Exception:
    GPUMonitorFactory = None

logger = logging.getLogger("gpu_stats")

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
