#!/usr/bin/env python3
"""The arithmetic a resize passes through before any display is touched.

A requested size is fitted into the bounds the server allows (`fit_res`),
optionally snapped to the encoder's macroblocks (`align_dims_16`), and a
second display is placed beside the first (`compute_dual_layout`). All three
are shared by the X11 and Wayland resize paths and by both transports, so the
numbers they return are the geometry a desktop ends up at: an aspect ratio
that drifts, an odd dimension reaching an encoder, or a framebuffer width
xrandr rounds differently is a broken desktop rather than a wrong number.

Usage: python3 tests/unit/test_display_geometry.py
"""
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

from selkies.display_utils import align_dims_16, compute_dual_layout, fit_res  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [display-geometry] {label}  {detail}", flush=True)


def fits() -> None:
    """A size within the bounds is the size asked for; anything larger comes
    back inside them with its aspect kept and both sides even."""
    check("a size inside the bounds is returned untouched",
          fit_res(1280, 720, 1920, 1080) == (1280, 720), fit_res(1280, 720, 1920, 1080))
    check("an odd size inside the bounds is not rounded either",
          fit_res(1279, 721, 1920, 1080) == (1279, 721), fit_res(1279, 721, 1920, 1080))
    check("the bounds themselves fit", fit_res(1920, 1080, 1920, 1080) == (1920, 1080))

    w, h = fit_res(3840, 2160, 1920, 1080)
    check("a too-wide size is scaled to the bound, not cropped",
          (w, h) == (1920, 1080), (w, h))
    w, h = fit_res(1920, 1080, 1280, 1280)
    check("width alone can bind", (w, h) == (1280, 720), (w, h))
    w, h = fit_res(1000, 2000, 1920, 1080)
    check("height alone can bind", (w, h) == (540, 1080), (w, h))
    w, h = fit_res(4000, 1000, 1920, 900)
    check("the tighter bound wins: a wide size is not stretched to the height",
          (w, h) == (1920, 480), (w, h))
    w, h = fit_res(4000, 3000, 1920, 1200)
    check("a size that still overflows after the first bound is fitted to the second",
          (w, h) == (1600, 1200), (w, h))
    check("the fitted aspect is the requested one",
          abs((w / h) - (4000 / 3000)) < 0.001, w / h)

    w, h = fit_res(1000, 1001, 999, 1000)
    check("a fitted size is rounded down to even on both axes",
          (w, h) == (998, 998) and w % 2 == 0 and h % 2 == 0, (w, h))


def aligns() -> None:
    """16-pixel macroblock alignment, and the sizes it refuses to collapse."""
    check("an unaligned size is rounded down to the macroblock grid",
          align_dims_16(1920, 1080) == (1920, 1072), align_dims_16(1920, 1080))
    check("an already-aligned size is unchanged",
          align_dims_16(1920, 1088) == (1920, 1088), align_dims_16(1920, 1088))
    check("the smallest aligned size survives", align_dims_16(16, 16) == (16, 16))
    check("a size that would align to nothing is kept as it is",
          align_dims_16(15, 300) == (15, 300), align_dims_16(15, 300))
    check("neither axis may collapse alone",
          align_dims_16(1920, 15) == (1920, 15), align_dims_16(1920, 15))


def dual() -> None:
    """Where a second display lands, and the framebuffer that has to hold both."""
    primary, secondary = (1000, 700), (900, 600)
    for position, want in (
        ("right", {"primary": {"x": 0, "y": 0, "w": 1000, "h": 700},
                   "secondary": {"x": 1000, "y": 0, "w": 900, "h": 600}}),
        ("left", {"secondary": {"x": 0, "y": 0, "w": 900, "h": 600},
                  "primary": {"x": 900, "y": 0, "w": 1000, "h": 700}}),
        ("up", {"secondary": {"x": 0, "y": 0, "w": 900, "h": 600},
                "primary": {"x": 0, "y": 600, "w": 1000, "h": 700}}),
        ("down", {"primary": {"x": 0, "y": 0, "w": 1000, "h": 700},
                  "secondary": {"x": 0, "y": 700, "w": 900, "h": 600}}),
    ):
        layouts, total_w, total_h = compute_dual_layout(primary, secondary, position)
        check(f"{position}: both displays sit where the position says",
              layouts == want, layouts)
        side_by_side = position in ("left", "right")
        check(f"{position}: the framebuffer covers both",
              total_w >= (1900 if side_by_side else 1000)
              and total_h >= (700 if side_by_side else 1300), (total_w, total_h))

    layouts, total_w, total_h = compute_dual_layout(primary, secondary, "sideways")
    check("an unknown position is placed to the right, not dropped",
          layouts["secondary"]["x"] == 1000 and layouts["primary"]["x"] == 0, layouts)

    _, total_w, total_h = compute_dual_layout(primary, secondary, "right")
    check("the framebuffer width is rounded up to the multiple of 8 xrandr takes",
          total_w == 1904 and total_w % 8 == 0, total_w)
    check("the height is left exactly as the displays need it",
          total_h == 700, total_h)
    _, stacked_w, stacked_h = compute_dual_layout(primary, secondary, "down")
    check("a stacked pair rounds its width and sums its height",
          (stacked_w, stacked_h) == (1000, 1300), (stacked_w, stacked_h))


def main() -> bool:
    fits()
    aligns()
    dual()
    print(f"\n[display-geometry] {passed} passed, {failed} failed")
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
