#!/usr/bin/env python3
"""What the client puts on the wire for a modifier chord, across the engines.

The browsers describe the same physical keyboard differently, and the
Alt-position key is where they disagree: macOS Option is a level-3 shift that
Gecko reports as AltGraph, Blink only as altKey and WebKit as a Meta key, while
a PC AltGr reports as AltGraph, or as its Ctrl+Alt pair on an older engine.
Deciding from those flags whether a chord picked a character or named a
shortcut therefore answers differently per browser -- Option+Z reaching the
server as Alt+z instead of the omega it produced. The checks drive one physical
action through the real handler once per engine and require the same wire from
each, so an engine that describes the keyboard in a new way fails here rather
than losing someone's keystrokes. They live in
tests/tools/keyboard_chord_audit.mjs, because the path under test is JavaScript.
"""
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(TESTS, "tools", "keyboard_chord_audit.mjs")

node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite.
    print("SKIP node not found, so the keyboard chord audit cannot run", flush=True)
    # helpers.SKIP_EXIT, without importing the e2e helper module
    sys.exit(77)

r = subprocess.run([node, AUDIT], capture_output=True, text=True, timeout=120)
lines = [ln for ln in r.stdout.splitlines() if ln.startswith(("PASS", "FAIL"))]
for line in lines:
    print(line, flush=True)
if not lines:
    print(f"FAIL  [keyboard-chords] audit ran  {r.stderr.strip()[:400]}", flush=True)
    sys.exit(1)

passed = sum(1 for ln in lines if ln.startswith("PASS"))
print(f"[keyboard-chords] {passed}/{len(lines)} passed")
sys.exit(r.returncode)
