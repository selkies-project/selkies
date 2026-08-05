"""Test configuration.

The suites are programs, not pytest modules: they run servers and browsers at
import time and report through their exit status. test_suites.py drives them, so
pytest collects that module alone and leaves the suite directories to it.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

collect_ignore_glob = ["unit/*.py", "integration/*.py", "e2e/*.py",
                       "perf/*.py", "soak/*.py", "tools/*.py", "tools/*/*.py"]
