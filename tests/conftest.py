"""Test configuration.

The suites are programs, not pytest modules: they run servers and browsers at
import time and report through their exit status. test_suites.py drives them, so
pytest collects that module alone and leaves the suite directories to it.
"""
import os
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

collect_ignore_glob = ["unit/*.py", "integration/*.py", "e2e/*.py",
                       "perf/*.py", "soak/*.py", "packaging/*.py", "image/*.py",
                       "image/*/*.py", "tools/*.py", "tools/*/*.py"]


@pytest.hookimpl(trylast=True)
def pytest_collection_modifyitems(config, items):
    """`SELKIES_SUITE_SHARD=k/n` keeps the k-th of n shards of what the markers
    and keywords selected: each case is dealt, longest budget first, to the
    shard with the least budget so far, so the shards take about as long."""
    spec = os.environ.get("SELKIES_SUITE_SHARD", "")
    if not spec:
        return
    k, n = (int(part) for part in spec.split("/"))
    budget = lambda item: item.callspec.params.get("timeout", 0) if hasattr(item, "callspec") else 0
    load, keep = [0] * n, set()
    for item in sorted(items, key=budget, reverse=True):
        shard = min(range(n), key=load.__getitem__)
        load[shard] += budget(item)
        if shard == k - 1:
            keep.add(item.nodeid)
    config.hook.pytest_deselected(items=[item for item in items if item.nodeid not in keep])
    items[:] = [item for item in items if item.nodeid in keep]
