#!/usr/bin/env python3
"""pytest entry point for the suites in suites.py.

Each suite runs as its own process, exactly as it does standalone, so a wedged
server or a crashed browser cannot take the rest of the run with it. The child
is told its budget (`SELKIES_SUITE_DEADLINE`, read by helpers.py and by the
Rust suites' harness) so a suite that stalls says where it stuck and exits
before the kill here would throw that record away. Select by tier with the
markers:

    pytest tests -m unit
    pytest tests -m "integration or e2e"
"""
import os
import shutil
import signal
import subprocess
import sys
import time
from typing import Optional

import pytest

import helpers
import suites

TESTS = os.path.dirname(os.path.abspath(__file__))

CASES: list = [
    pytest.param(path, selector, timeout, suites.name(path, tier),
                 marks=getattr(pytest.mark, tier),
                 id=suites.name(path, tier).replace("/", ":") + (f"-{selector}" if selector else ""))
    for path, selector, tier, timeout in suites.cases()
]


def keep_logs(case: str, since: float) -> None:
    """Copy the logs a suite left in the work directory into a folder named after it,
    so a run's record carries every suite's server log and not only the last one's.

    The directory is shared, so only what this suite wrote is taken: a log an
    earlier suite left untouched reads as this one's and has been believed.
    """
    dest = os.path.join(helpers.WORKDIR, "suite-logs", case)
    try:
        os.makedirs(dest, exist_ok=True)
        for name in os.listdir(helpers.WORKDIR):
            src = os.path.join(helpers.WORKDIR, name)
            if not name.endswith(".log") or not os.path.isfile(src):
                continue
            if os.stat(src).st_mtime >= since:
                shutil.copy2(src, dest)
    except OSError:
        pass


# What `helpers.answers_within` prints when a browser stops answering, what
# Playwright raises when one goes away or a page's process dies, and what
# `core_lib.wait_wr_video` prints when a player never starts on frames its
# engine decodes. WebKit's video process wedges on a loaded runner often enough
# to take a suite with it, and every call after that reads as absent video, so
# a run that says so is not a result: it is repeated once, both attempts
# printed, and a loss that repeats fails as it did before.
STALLED = ("did not answer within", "stopped answering earlier",
           "Target page, context or browser has been closed", "Page crashed",
           "sat at HAVE_NOTHING on a live track")


def stalled(text: str) -> bool:
    return any(mark in text for mark in STALLED)


def run(cmd: list, timeout: int, env: dict) -> subprocess.CompletedProcess:
    """Run a suite to its end in a process group of its own, and kill the group
    when it runs past `timeout` or this run is interrupted.

    A Rust suite is `cargo` with the test binary under it, and a kill that
    reached `cargo` alone left that binary and its servers on the display.
    Killed with its group, the binary takes its servers along: each is tied to
    the thread that started it, as a Python suite's children are to theirs.

    Raises:
        subprocess.TimeoutExpired: Past `timeout`, with the output so far.
    """
    with subprocess.Popen(cmd, cwd=TESTS, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, env=env, start_new_session=True) as proc:
        try:
            out, err = proc.communicate(timeout=timeout)
        except BaseException as stopped:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except OSError:
                pass
            out, err = proc.communicate()
            if isinstance(stopped, subprocess.TimeoutExpired):
                raise subprocess.TimeoutExpired(cmd, timeout, output=out, stderr=err) from None
            raise
    return subprocess.CompletedProcess(cmd, proc.returncode, out, err)


@pytest.mark.parametrize("path,selector,timeout,name", CASES)
def test_suite(path: str, selector: Optional[str], timeout: int, name: str) -> None:
    """Run one suite as a subprocess and map its exit protocol onto pytest."""
    cmd = suites.command(path, selector)
    # A minute short of the kill, so what a stalled suite says lands in the
    # output that is kept: a Python suite dumps every thread's stack, a Rust
    # one, whose waits are all bounded, the end of each running server's log.
    env = dict(os.environ, SELKIES_SUITE_DEADLINE=str(max(30, timeout - 60)))
    case = name.replace("/", "-") + (f"-{selector}" if selector else "")
    started = time.time()
    try:
        proc = run(cmd, timeout, env)
        if proc.returncode != 0 and stalled(proc.stdout + proc.stderr):
            sys.stdout.write(proc.stdout)
            sys.stderr.write(proc.stderr)
            print(f"note: {case} lost a browser or its player mid-run; running it once more",
                  flush=True)
            keep_logs(case + "-stalled", started)
            proc = run(cmd, timeout, env)
    except subprocess.TimeoutExpired as e:
        keep_logs(case, started)
        # The checks the suite did finish are its record of where it stuck;
        # they would otherwise die with it.
        out, err = e.stdout or "", e.stderr or ""
        sys.stdout.write(out)
        sys.stderr.write(err)
        raise AssertionError(
            f"{path} {selector or ''} ran past {timeout}s\n"
            + ("\n".join(out.splitlines()[-20:]) or err[-2000:])) from None
    keep_logs(case, started)
    sys.stdout.write(proc.stdout)
    sys.stderr.write(proc.stderr)
    if proc.returncode == helpers.SKIP_EXIT:
        reason = next((line for line in proc.stdout.splitlines()
                       if line.startswith("SKIP")), "subject not installed")
        pytest.skip(f"{path}: {reason}")
    # libtest passes a filter that matches nothing, which is a selector that
    # names no test of the target, not a suite that passed.
    assert not path.endswith(".rs") or proc.returncode or "running 0 tests" not in proc.stdout, (
        f"{path} {selector or ''} ran no test")
    failed = [line for line in proc.stdout.splitlines() if line.startswith("FAIL")]
    # A suite that died rather than failing checks reports through stderr, so
    # the traceback is what the message has to carry; a Rust one's panic is in
    # the test's own output, and stderr holds cargo's.
    died = (proc.stdout[-4000:] if path.endswith(".rs") else "") or proc.stderr[-2000:] or proc.stdout[-2000:]
    detail = "\n".join(failed[:20]) or died
    assert proc.returncode == 0, (
        f"{path} {selector or ''} exited {proc.returncode}\n{detail}")
