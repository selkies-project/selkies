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
from typing import Optional

import pytest

import helpers
import suites

TESTS = os.path.dirname(os.path.abspath(__file__))
PYTHON = os.environ.get("SELKIES_TEST_PYTHON", sys.executable)

CASES: list = [
    pytest.param(path, selector, timeout,
                 marks=getattr(pytest.mark, tier),
                 id=path[:-3].replace("/", ":") + (f"-{selector}" if selector else ""))
    for path, selector, tier, timeout in suites.cases()
]


@pytest.mark.parametrize("path,selector,timeout", CASES)
def test_suite(path: str, selector: Optional[str], timeout: int) -> None:
    """Run one suite as a subprocess and map its exit protocol onto pytest."""
    cmd = [PYTHON, os.path.join(TESTS, path)] + ([selector] if selector else [])
    try:
        proc = subprocess.run(cmd, cwd=TESTS, capture_output=True, text=True,
                              timeout=timeout)
    except subprocess.TimeoutExpired as e:
        # The checks the suite did finish are its record of where it stuck;
        # they would otherwise die with it.
        out = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        err = (e.stderr or b"").decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
        sys.stdout.write(out)
        sys.stderr.write(err)
        raise AssertionError(
            f"{path} {selector or ''} ran past {timeout}s\n"
            + ("\n".join(out.splitlines()[-20:]) or err[-2000:])) from None
    sys.stdout.write(proc.stdout)
    sys.stderr.write(proc.stderr)
    if proc.returncode == helpers.SKIP_EXIT:
        reason = next((line for line in proc.stdout.splitlines()
                       if line.startswith("SKIP")), "subject not installed")
        pytest.skip(f"{path}: {reason}")
    failed = [line for line in proc.stdout.splitlines() if line.startswith("FAIL")]
    # A suite that died rather than failing checks reports through stderr, so
    # the traceback is what the message has to carry
    detail = "\n".join(failed[:20]) or proc.stderr[-2000:] or proc.stdout[-2000:]
    assert proc.returncode == 0, (
        f"{path} {selector or ''} exited {proc.returncode}\n{detail}")
