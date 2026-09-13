# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""CPU and memory usage as the session's own cgroup accounts for them.

psutil shows a container, or a slice, every processor and byte of the host,
though its cgroup may allow it a share of them, and the host's figures count
every other tenant too. The sampler reads the cgroup the process runs in: on
the unified hierarchy `cpu.stat` and `memory.current` against `cpu.max` and
`memory.max`, on the legacy one the `cpuacct` and `memory` controllers against
the CFS quota and the memory limit. Memory in use leaves out the page cache
the kernel can drop (the inactive file pages), the way container tools report
it, and CPU is the usage counter's growth between two samples over the cores
the cgroup may use: its quota, else the processors it is allowed. A limit the
cgroup does not set is the host's total, and a cgroup that cannot be read at
all leaves psutil's system-wide figures.
"""
import os
import time
from typing import Callable, Optional, Tuple

import psutil

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
    """Samples `(cpu_percent, mem_total_bytes, mem_used_bytes)` for the dashboards' gauges."""

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
