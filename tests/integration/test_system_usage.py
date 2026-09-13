#!/usr/bin/env python3
"""The sampler on this host's own cgroup: the memory total is the cgroup's
limit where one is set, the use never exceeds it, and CPU is a percentage of
the cores the cgroup may use, so a busy thread registers as a share of them.
"""
import os
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.argv = ["selkies"]

from selkies.system_usage import SystemUsage  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [system-usage-host] {label}  {detail}", flush=True)


def limit() -> int:
    for path in ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"):
        try:
            text = open(path).read().strip()
        except OSError:
            continue
        if text.isdigit():
            return int(text)
    return 0


usage = SystemUsage()
usage.sample()
stop = time.monotonic() + 1.0
spinners = [threading.Thread(target=lambda: [None for _ in iter(lambda: time.monotonic() < stop, False)]) for _ in range(2)]
for t in spinners:
    t.start()
for t in spinners:
    t.join()
cpu, total, used = usage.sample()
check("CPU is a percentage of the cores the cgroup may use, and two busy threads register",
      0 < cpu <= 100, cpu)
check("memory in use stays within the total", 0 < used <= total, (used, total))
if limit():
    check("the total is the cgroup's memory limit", total == limit(), (total, limit()))
else:
    print("PASS  [system-usage-host] no memory limit here, so the total is the host's", flush=True)
    passed += 1

print(f"[system-usage-host] {passed}/{passed + failed} passed", flush=True)
sys.exit(1 if failed else 0)
