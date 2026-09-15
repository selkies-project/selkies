#!/usr/bin/env python3
"""What the density ladder reads about the session, and what it remembers.

A desktop that keeps its settings on a session bus is reached through the
environment its own process runs with, which is read out of /proc rather than
inherited, and the density the ladder last applied is what a page joining an
existing session is told the desktop already has. The readers run against real
processes.
"""

import asyncio
import os
import subprocess
import sys
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

import helpers as H  # noqa: E402
from selkies import display_utils as DU  # noqa: E402

res = H.Results("session-density")

sleeper = subprocess.Popen(["sleep", "30"], env={"PATH": os.environ.get("PATH", ""),
                                                 "SELKIES_PROBE": "density"})
try:
    time.sleep(0.2)
    env = DU._process_environ(sleeper.pid)
    res.check("a process's environment is read back from /proc",
              env.get("SELKIES_PROBE") == "density", str(env))
    pids = asyncio.run(DU._pids_of("sleep"))
    res.check("the processes running a binary are found by name",
              sleeper.pid in pids, str(pids))
finally:
    sleeper.kill()
    sleeper.wait()

res.check("a process that is gone has no environment to read",
          DU._process_environ(sleeper.pid) == {})
res.check("a binary nothing is running has no pids",
          asyncio.run(DU._pids_of("selkies-no-such-binary")) == [])

res.check("no density is reported before one is applied", DU.applied_dpi() is None)
DU._APPLIED_DPI = 192
res.check("the density last applied is what a joining page is told", DU.applied_dpi() == 192)
DU._APPLIED_DPI = None

sys.exit(0 if res.summary() else 1)
