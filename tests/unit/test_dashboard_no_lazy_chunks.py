#!/usr/bin/env python3
"""The dashboard source splits off no lazily loaded chunk.

A chunk loaded by `import()` (or `React.lazy` over one) imports the entry chunk
back. When the page URL carries `user:pass@`, Chrome evaluates that entry a second
time. That copy starts a second core, which evicts the first as the same tab
("a new primary client connected"), and React fails with "useDashboard
needs a DashboardProvider". A chunk that fails to fetch also unmounts the
dashboard. `layouts/index.ts` holds the reasoning.

This scans `addons/selkies-dashboard/src` for both forms, ignoring comments
and type-only `import("...").Name` references.

Usage: python3 tests/unit/test_dashboard_no_lazy_chunks.py
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SRC = os.path.join(ROOT, "addons", "selkies-dashboard", "src")
EXTENSIONS = (".ts", ".tsx", ".js", ".jsx")

COMMENTS = re.compile(r"/\*.*?\*/|//[^\n]*", re.S)
# A type reference (`import("./x").Name`) is not a load; `.then`/`.catch`/`.finally` on one is.
DYNAMIC_IMPORT = re.compile(r"(?<![\w$.])import\s*\((?![^)]*\)\s*\.(?!then\b|catch\b|finally\b))")
LAZY = re.compile(r"(?<![\w$])(?:React\.)?lazy\s*\(")

results = []


def check(label: str, ok: object, detail: object = "") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [dashboard-no-lazy-chunks] {label}  {detail}", flush=True)


def offenders(pattern: "re.Pattern[str]") -> list:
    """Every `path:line` under the dashboard source where `pattern` matches code."""
    found = []
    for dirpath, _dirs, files in os.walk(SRC):
        for name in sorted(files):
            if not name.endswith(EXTENSIONS):
                continue
            path = os.path.join(dirpath, name)
            with open(path, encoding="utf-8") as fh:
                text = fh.read()
            code = COMMENTS.sub(lambda m: "\n" * m.group(0).count("\n"), text)
            for match in pattern.finditer(code):
                line = code.count("\n", 0, match.start()) + 1
                found.append(f"{os.path.relpath(path, ROOT)}:{line}")
    return found


def main() -> bool:
    check("the dashboard source is present", os.path.isdir(SRC), SRC)
    lazy = offenders(LAZY)
    check("no component is React.lazy", not lazy, lazy)
    dynamic = offenders(DYNAMIC_IMPORT)
    check("no module is loaded by import()", not dynamic, dynamic)
    failed = [label for label, ok in results if not ok]
    print(f"[dashboard-no-lazy-chunks] {len(results) - len(failed)}/{len(results)} passed")
    return not failed


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
