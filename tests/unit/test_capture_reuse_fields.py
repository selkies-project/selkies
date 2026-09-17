#!/usr/bin/env python3
"""The settings the capture-reuse check compares.

A reconfiguration keeps a running capture when nothing structural changed, and
decides that by comparing named fields on `pixelflux.CaptureSettings`. The
comparison is guarded, so a name that no longer exists does not fail loudly: it
raises, the guard reads it as "not reusable", and every reconfiguration rebuilds
the pipeline instead. That is invisible from the outside, which is how
`output_mode` outlived the field it named.

Pins each name against the installed pixelflux, so a renamed field is a failing
check rather than a silent teardown.
"""
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [capture-reuse] {label}  {detail}", flush=True)


def main() -> int:
    try:
        from pixelflux import CaptureSettings
    except ImportError as exc:
        print(f"SKIP  [capture-reuse] pixelflux is not installed  {exc}", flush=True)
        return 0
    from selkies.websockets_mode import STRUCTURAL_CAPTURE_SETTINGS

    settings = CaptureSettings()
    check("the check names at least the codec and the encoder path",
          {"codec", "use_cpu"} <= set(STRUCTURAL_CAPTURE_SETTINGS),
          STRUCTURAL_CAPTURE_SETTINGS)
    for name in STRUCTURAL_CAPTURE_SETTINGS:
        check(f"{name} is a field of CaptureSettings", hasattr(settings, name))

    # The comparison the reconfiguration runs, against two settings that differ
    # in nothing: it has to answer "reusable" rather than raise.
    try:
        same = all(getattr(CaptureSettings(), k) == getattr(settings, k)
                   for k in STRUCTURAL_CAPTURE_SETTINGS)
        check("two unchanged settings compare as reusable", same is True, same)
    except Exception as exc:  # noqa: BLE001 - the point is that nothing raises
        check("two unchanged settings compare as reusable", False,
              f"{type(exc).__name__}: {exc}")

    print(f"[capture-reuse] {passed}/{passed + failed} passed", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
