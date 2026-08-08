#!/usr/bin/env python3
"""Fail a CI run in which any suite skipped.

A skip means a dependency the runner was supposed to provide went missing, and
it reads as green. This turns that back into a red run, and additionally lists
the per-check skips suites record internally, which never reach the JUnit
report at all.

Usage: assert_no_skips.py <junit-xml> [...]
"""
import sys
import xml.etree.ElementTree as ET

skipped, per_check = [], []
for path in sys.argv[1:]:
    for case in ET.parse(path).getroot().iter("testcase"):
        name = case.get("name", "?")
        for sk in case.iter("skipped"):
            skipped.append(f"{name}: {(sk.get('message') or '').strip()[:160]}")
        for out in case.iter("system-out"):
            for line in (out.text or "").splitlines():
                if line.startswith("SKIP  ["):
                    per_check.append(f"{name}: {line[6:].strip()[:160]}")

for line in per_check:
    print(f"note: check not observed here -- {line}")
if not skipped:
    print(f"no skipped suites ({len(per_check)} unobserved checks)")
    sys.exit(0)

print(f"\n{len(skipped)} suite(s) skipped, which this run does not allow:")
for line in skipped:
    print(f"  {line}")
sys.exit(1)
