#!/usr/bin/env python3
"""The derived `scaling_dpi` default is the same number on all three sides.

Three copies exist: the cores' shared rule in lib/stream-density.js and one per
dashboard, which cannot import it. They have to agree, because they do not
merely display the same value: on connect the core sends its derived default,
and the dashboard posts *its* over the top whenever the two differ (the
`willPostDerived` gate). A formula changed on one side alone would leave the
desktop taking one DPI and the picker showing another, then quietly overwrite
the one the core applied.

The rules are JavaScript, so what is compared here is what each source
declares: the stops offered, the rows a 96 DPI desktop is for, and the two
expressions a density is read from -- a manual resolution's shorter side, or
the local display scaling.

Usage: python3 tests/unit/test_dpi_default_mirror.py
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ADDONS = os.path.join(ROOT, "addons")

SOURCES = {
    "lib": "selkies-web-core/lib/stream-density.js",
    "dashboard": "selkies-dashboard/src/components/Sidebar.jsx",
    "wish": "selkies-dashboard-wish/src/components/dashboard/settings.tsx",
}
# The density each side reads off a manual resolution, and off the display when
# there is none. Written the same way in all three, so a change to either rule
# has to be made in all three.
EXPRESSIONS = ("96 * rows / DPI_UNITY_ROWS", "Math.round(dpr * 4) * 24")

results = []


def check(label: str, ok: object, detail: object = "") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [dpi-default-mirror] {label}  {detail}", flush=True)


def read(rel: str) -> str:
    with open(os.path.join(ADDONS, rel), encoding="utf-8") as fh:
        return fh.read()


def stops(text: str) -> list:
    """The DPI stops the source offers, from either shape they are written in."""
    listed = re.search(r"DPI_STOPS\s*=\s*\[([^\]]*)\]", text)
    if listed:
        return [int(n) for n in re.findall(r"\d+", listed.group(1))]
    options = re.search(r"dpiScalingOptions\s*(?::[^=]*)?=\s*\[(.*?)\n\]", text, re.S)
    return [int(n) for n in re.findall(r"value:\s*(\d+)", options.group(1))] if options else []


def main() -> bool:
    texts = {name: read(rel) for name, rel in SOURCES.items()}
    reference = stops(texts["lib"])
    check("the lib names the stops", len(reference) > 1, reference)
    for name in ("dashboard", "wish"):
        got = stops(texts[name])
        check(f"{name} offers the same stops", got == reference, f"{got} vs {reference}")

    rows = {name: re.findall(r"DPI_UNITY_ROWS\s*(?::\s*number)?\s*=\s*(\d+)", text)
            for name, text in texts.items()}
    check("every side reads a resolution against the same rows",
          len({tuple(v) for v in rows.values()}) == 1 and rows["lib"], rows)

    for expression in EXPRESSIONS:
        missing = [name for name, text in texts.items() if expression not in text]
        check(f"every side derives from `{expression}`", not missing, f"missing in {missing}")

    failed = [label for label, ok in results if not ok]
    print(f"[dpi-default-mirror] {len(results) - len(failed)}/{len(results)} passed")
    return not failed


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
