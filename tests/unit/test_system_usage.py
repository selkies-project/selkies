#!/usr/bin/env python3
"""The figures the dashboards' CPU and memory gauges show come from the
session's own cgroup where it limits CPU or memory: its usage against its
limits on the unified and the legacy hierarchies, the host's total where only
the other resource is limited; and from the node, through psutil, where the
cgroup limits nothing or cannot be read.
"""
import os
import sys
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.argv = ["selkies"]

import psutil  # noqa: E402
from selkies.resource_stats import SystemUsage  # noqa: E402

passed = failed = 0
HOST_TOTAL = psutil.virtual_memory().total


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [system-usage] {label}  {detail}", flush=True)


class Clock:
    def __init__(self) -> None:
        self.now = 100.0

    def __call__(self) -> float:
        return self.now


def tree(root: str, files: dict) -> None:
    for rel, text in files.items():
        path = os.path.join(root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)


def unified(root: str, usage_usec: int, cpu_max: str, current: int, limit: str, inactive_file: int) -> None:
    tree(root, {"proc/cgroup": "0::/\n", "cpu.stat": f"usage_usec {usage_usec}\nuser_usec 1\n", "cpu.max": cpu_max + "\n",
                "memory.current": f"{current}\n", "memory.max": limit + "\n",
                "memory.stat": f"anon 1\ninactive_file {inactive_file}\n"})


with tempfile.TemporaryDirectory() as root:
    clock = Clock()
    unified(root, 1_000_000, "200000 100000", 3_000_000_000, "4000000000", 1_000_000_000)
    usage = SystemUsage(root=root, proc_cgroup=os.path.join(root, "proc/cgroup"), clock=clock)
    cpu, total, used = usage.sample()
    check("unified: the first sample reports no CPU, the memory limit and the use without dropped cache",
          cpu == 0.0 and total == 4_000_000_000 and used == 2_000_000_000, (cpu, total, used))
    unified(root, 2_000_000, "200000 100000", 3_000_000_000, "4000000000", 1_000_000_000)
    clock.now += 1.0
    cpu, _, _ = usage.sample()
    check("unified: a second of CPU over a second on a two-core quota is half", cpu == 50.0, cpu)
    unified(root, 12_000_000, "200000 100000", 3_000_000_000, "4000000000", 1_000_000_000)
    clock.now += 1.0
    cpu, _, _ = usage.sample()
    check("unified: usage past the quota's cores caps at a hundred", cpu == 100.0, cpu)

with tempfile.TemporaryDirectory() as root:
    clock = Clock()
    cores = len(os.sched_getaffinity(0))
    unified(root, 0, "max 100000", 500_000_000, "max", 0)
    usage = SystemUsage(root=root, proc_cgroup=os.path.join(root, "proc/cgroup"), clock=clock)
    cpu, total, used = usage.sample()
    check("unified without limits: the cgroup is not the environment, the node's figures stand",
          total == HOST_TOTAL and used != 500_000_000 and 0 <= cpu <= 100, (total, used, cpu, cores))
    unified(root, 0, "max 100000", 500_000_000, "2000000000", 0)
    usage = SystemUsage(root=root, proc_cgroup=os.path.join(root, "proc/cgroup"), clock=clock)
    _, total, used = usage.sample()
    unified(root, int(cores * 1_000_000), "max 100000", 500_000_000, "2000000000", 0)
    clock.now += 1.0
    cpu, _, _ = usage.sample()
    check("unified with a memory limit alone: the cgroup counts, its CPU over the allowed processors",
          total == 2_000_000_000 and used == 500_000_000 and cpu == 100.0, (total, used, cpu, cores))

with tempfile.TemporaryDirectory() as root:
    clock = Clock()
    tree(root, {"proc/cgroup": "5:cpu,cpuacct:/docker/abc\n4:memory:/docker/abc\n1:name=systemd:/docker/abc\n",
                "cpu,cpuacct/docker/abc/cpuacct.usage": "1000000000\n",
                "cpu,cpuacct/docker/abc/cpu.cfs_quota_us": "400000\n", "cpu,cpuacct/docker/abc/cpu.cfs_period_us": "100000\n",
                "memory/docker/abc/memory.usage_in_bytes": "6000000000\n", "memory/docker/abc/memory.limit_in_bytes": "8000000000\n",
                "memory/docker/abc/memory.stat": "cache 1\ntotal_inactive_file 2000000000\n"})
    usage = SystemUsage(root=root, proc_cgroup=os.path.join(root, "proc/cgroup"), clock=clock)
    _, total, used = usage.sample()
    tree(root, {"cpu,cpuacct/docker/abc/cpuacct.usage": "3000000000\n"})
    clock.now += 1.0
    cpu, _, _ = usage.sample()
    check("legacy: the CFS quota, the memory limit and the use without dropped cache",
          total == 8_000_000_000 and used == 4_000_000_000 and cpu == 50.0, (total, used, cpu))

with tempfile.TemporaryDirectory() as root:
    clock = Clock()
    tree(root, {"proc/cgroup": "5:cpu,cpuacct:/docker/abc\n4:memory:/docker/abc\n",
                "cpu,cpuacct/cpuacct.usage": "0\n", "cpu,cpuacct/cpu.cfs_quota_us": "-1\n", "cpu,cpuacct/cpu.cfs_period_us": "100000\n",
                "memory/memory.usage_in_bytes": "100\n", "memory/memory.limit_in_bytes": "9223372036854771712\n", "memory/memory.stat": ""})
    usage = SystemUsage(root=root, proc_cgroup=os.path.join(root, "proc/cgroup"), clock=clock)
    _, total, used = usage.sample()
    check("legacy without a namespace and without limits: the node's figures stand",
          total == HOST_TOTAL and used != 100, (total, used))
    tree(root, {"cpu,cpuacct/cpu.cfs_quota_us": "100000\n"})
    usage = SystemUsage(root=root, proc_cgroup=os.path.join(root, "proc/cgroup"), clock=clock)
    _, total, used = usage.sample()
    check("legacy with a CPU quota alone: the controller mounts are the cgroup, the memory total the host's",
          total == HOST_TOTAL and used == 100, (total, used))

with tempfile.TemporaryDirectory() as root:
    usage = SystemUsage(root=root, proc_cgroup=os.path.join(root, "missing"))
    cpu, total, used = usage.sample()
    check("no cgroup: psutil's system-wide figures", total == HOST_TOTAL and 0 <= cpu <= 100 and 0 < used <= total,
          (cpu, total, used))

print(f"[system-usage] {passed}/{passed + failed} passed", flush=True)
sys.exit(1 if failed else 0)
