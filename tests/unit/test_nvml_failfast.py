#!/usr/bin/env python3
"""NVML is probed once per process. A failed nvmlInit must be terminal and a
successful one must not re-init: without that latch, a host lacking the NVIDIA
library pays a dlopen attempt plus a log line on every GPU stats poll, a few
times per second for the life of the server. The stats path itself has to keep
working either way through the non-NVML sources.
"""
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

import selkies.gpu_stats as gpu_stats  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [nvml] {label}  {detail}", flush=True)


class FakeNvml:
    def __init__(self, fail):
        self.init_calls = 0
        self.fail = fail

    def nvmlInit(self):
        self.init_calls += 1
        if self.fail:
            raise RuntimeError("libnvidia-ml.so.1: cannot open shared object file")

    def nvmlDeviceGetCount(self):
        return 0


real_pynvml = gpu_stats.pynvml
real_ready = gpu_stats._nvml_ready
try:
    fake = FakeNvml(fail=True)
    gpu_stats.pynvml = fake
    gpu_stats._nvml_ready = None
    results = [gpu_stats._nvml_gpus() for _ in range(5)]
    check("a failed init returns no GPUs", all(r == [] for r in results), results)
    check("a failed init is attempted exactly once", fake.init_calls == 1,
          f"{fake.init_calls} attempts over 5 polls")
    check("the failure is recorded as terminal", gpu_stats._nvml_ready is False,
          gpu_stats._nvml_ready)

    fake = FakeNvml(fail=False)
    gpu_stats.pynvml = fake
    gpu_stats._nvml_ready = None
    for _ in range(3):
        gpu_stats._nvml_gpus()
    check("a successful init is not repeated", fake.init_calls == 1,
          f"{fake.init_calls} attempts over 3 polls")
    check("success is recorded", gpu_stats._nvml_ready is True, gpu_stats._nvml_ready)

    gpu_stats.pynvml = None
    gpu_stats._nvml_ready = None
    check("absent nvidia-ml-py stays a no-op", gpu_stats._nvml_gpus() == [])
    check("absence never marks the probe attempted", gpu_stats._nvml_ready is None,
          gpu_stats._nvml_ready)

    # The overall stats call must survive a terminally failed NVML by falling
    # through to the other sources (aitop, nvidia-smi, sysfs), whatever this
    # host provides.
    gpu_stats.pynvml = FakeNvml(fail=True)
    gpu_stats._nvml_ready = None
    gpus = gpu_stats.get_gpus()
    check("get_gpus still answers with NVML failed", isinstance(gpus, list),
          type(gpus).__name__)
finally:
    gpu_stats.pynvml = real_pynvml
    gpu_stats._nvml_ready = real_ready

print(f"[nvml] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
