#!/usr/bin/env python3
"""Every Markdown table in the repository renders as a table.

A GFM table ends at a blank line, so a paragraph written directly under the
last row is absorbed into it: each of its lines becomes a one-cell row padded
with empty cells. The source reads correctly and only the published page is
wrong, which is how a licensing paragraph spent a release inside the table
above it. A row whose cell count differs from the header is the same class of
defect, silently dropping or shifting a column.
"""
import glob
import os
import re
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DELIMITER = re.compile(r"^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)*\|?\s*$")
SKIP = ("node_modules", os.path.join("docs", "reference"))

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [docs-tables] {label}  {detail}", flush=True)


def cells(line: str) -> list:
    """The row's cells, split on pipes that are neither escaped nor in a code span."""
    out, cur, ticks, i = [], "", 0, 0
    while i < len(line):
        if line[i] == "\\" and i + 1 < len(line):
            cur += line[i:i + 2]
            i += 2
            continue
        if line[i] == "`":
            run = len(line[i:]) - len(line[i:].lstrip("`"))
            ticks = run if ticks == 0 else (0 if ticks == run else ticks)
            cur += line[i:i + run]
            i += run
            continue
        if line[i] == "|" and ticks == 0:
            out.append(cur)
            cur = ""
            i += 1
            continue
        cur += line[i]
        i += 1
    out.append(cur)
    if out and not out[0].strip():
        out = out[1:]
    if out and not out[-1].strip():
        out = out[:-1]
    return out


def problems(path: str) -> list:
    lines = open(path, encoding="utf-8").read().split("\n")
    found, fenced, i = [], False, 0
    while i < len(lines):
        stripped = lines[i].strip()
        if stripped.startswith(("```", "~~~")):
            fenced = not fenced
            i += 1
            continue
        if fenced or not stripped or "|" not in stripped:
            i += 1
            continue
        if not (i + 1 < len(lines) and "|" in lines[i + 1] and DELIMITER.match(lines[i + 1])):
            i += 1
            continue
        width = len(cells(lines[i]))
        if len(cells(lines[i + 1])) != width:
            found.append(f"{i + 2}: delimiter row does not match the header's {width} columns")
        j = i + 2
        while j < len(lines) and lines[j].strip():
            row = lines[j].strip()
            if row.startswith(("```", "~~~")):
                break
            if "|" not in row:
                found.append(f"{j + 1}: prose absorbed as a table row: {row[:60]!r}")
            elif len(cells(lines[j])) != width:
                found.append(f"{j + 1}: row has {len(cells(lines[j]))} cells, header has {width}")
            j += 1
        i = j
    return found


patterns = ["*.md", "docs/*.md", "website/*.md", "tests/*.md", "scripts/*.md",
            "infra/**/*.md", "addons/**/*.md", ".github/**/*.md"]
paths = sorted({p for pattern in patterns
                for p in glob.glob(os.path.join(REPO, pattern), recursive=True)
                if not any(s in p for s in SKIP)})
check("the documentation was found", len(paths) > 10, f"{len(paths)} files")
for path in paths:
    found = problems(path)
    check(f"{os.path.relpath(path, REPO)} tables render", not found, "; ".join(found))

print(f"[docs-tables] {passed}/{passed + failed} passed", flush=True)
sys.exit(1 if failed else 0)
