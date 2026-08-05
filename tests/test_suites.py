#!/usr/bin/env python3
"""pytest entry point for the suites in suites.py.

Each suite runs as its own process, exactly as it does standalone, so a wedged
server or a crashed browser cannot take the rest of the run with it. Select by
tier with the markers:

    pytest tests -m unit
    pytest tests -m "integration or e2e"
"""
import os
import subprocess
import sys

import pytest

import suites

TESTS = os.path.dirname(os.path.abspath(__file__))
PYTHON = os.environ.get("SELKIES_TEST_PYTHON", sys.executable)

CASES = [
    pytest.param(path, selector, timeout,
                 marks=getattr(pytest.mark, tier),
                 id=path[:-3].replace("/", ":") + (f"-{selector}" if selector else ""))
    for path, selector, tier, timeout in suites.cases()
]


@pytest.mark.parametrize("path,selector,timeout", CASES)
def test_suite(path, selector, timeout):
    cmd = [PYTHON, os.path.join(TESTS, path)] + ([selector] if selector else [])
    proc = subprocess.run(cmd, cwd=TESTS, capture_output=True, text=True,
                          timeout=timeout)
    sys.stdout.write(proc.stdout)
    sys.stderr.write(proc.stderr)
    failed = [line for line in proc.stdout.splitlines() if line.startswith("FAIL")]
    # A suite that died rather than failing checks reports through stderr, so
    # the traceback is what the message has to carry
    detail = "\n".join(failed[:20]) or proc.stderr[-2000:] or proc.stdout[-2000:]
    assert proc.returncode == 0, (
        f"{path} {selector or ''} exited {proc.returncode}\n{detail}")
