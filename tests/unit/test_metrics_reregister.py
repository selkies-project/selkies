#!/usr/bin/env python3
"""A mode switch builds the metrics again, so every collector the first build
registered has to be released, or the switch dies on a duplicated timeseries.
Constructs `Metrics` twice with an `unregister` between, and checks the global
registry holds nothing of the first build.
"""
import os
import sys
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
from prometheus_client import REGISTRY, Gauge, Histogram, Info
from selkies.metrics import Metrics


def collectors(metrics: Metrics) -> list:
    return [c for c in vars(metrics).values() if isinstance(c, (Gauge, Histogram, Info))]


def main() -> bool:
    res = H.Results("metrics-reregister")
    first = Metrics()
    built = collectors(first)
    res.check("the build registers collectors", len(built) >= 10, len(built))
    first.unregister()
    left = [c for c in built if c in REGISTRY._collector_to_names]
    res.check("unregister releases every one of them", not left,
              [next(iter(REGISTRY._collector_to_names[c])) for c in left])
    try:
        second = Metrics()
    except ValueError as exc:
        res.check("a second build after unregister succeeds", False, exc)
        return res.summary()
    res.check("a second build after unregister succeeds", True)
    second.unregister()
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
