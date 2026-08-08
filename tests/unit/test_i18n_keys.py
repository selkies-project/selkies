#!/usr/bin/env python3
"""Every translation key the dashboards ask for has to resolve.

Both translators return the key itself when a lookup misses, so a misspelt or
absent key ships looking fine and renders as "clipboard.uploadImage" to the
user. The resolution itself lives in tests/tools/i18n_audit.mjs, because the
dictionaries are JavaScript.
"""
import json
import os
import shutil
import subprocess
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
ADDONS = os.path.join(REPO, "addons")
AUDIT = os.path.join(TESTS, "tools", "i18n_audit.mjs")

passed = failed = 0


def check(label, ok, detail=""):
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [i18n] {label}  {detail}", flush=True)


node = shutil.which("node")
if not node:
    # Reported as a skip, never as a pass: the audit is the whole suite, so
    # exiting 0 here would announce that every key resolves without having
    # looked at one.
    print("SKIP node not found, so the dashboard translation audit cannot run",
          flush=True)
    sys.exit(77)  # helpers.SKIP_EXIT, without importing the e2e helper module

r = subprocess.run([node, AUDIT, ADDONS], capture_output=True, text=True, timeout=120)
if r.returncode != 0:
    print(f"FAIL  [i18n] audit ran  {r.stderr.strip()[:400]}")
    print("[i18n] 0/1 passed")
    sys.exit(1)

report = json.loads(r.stdout)
for name, res in report.items():
    unresolved = res["unresolved"]
    detail = ", ".join(
        f"{k} ({v[0]})" for k, v in list(unresolved.items())[:4]) or f"{res['keys']} keys"
    check(f"{name}: every key resolves", not unresolved, detail)

    gaps = res["gaps"]
    detail = ", ".join(
        f"{lc}: {len(m)} missing ({m[0]})" for lc, m in list(gaps.items())[:4]
    ) or f"{res['enKeys']} keys x {res['locales']} locales"
    check(f"{name}: no locale is missing a key English has", not gaps, detail)

print(f"[i18n] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
