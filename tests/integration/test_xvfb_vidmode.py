#!/usr/bin/env python3
"""The images' Xvfb reports its screen's rate through XFree86-VidModeExtension.

Mesa answers glXGetMscRateOML from the VidMode mode line, as dot clock * 1000
over the totals' product in 32-bit arithmetic, and Chromium's GPU process asks
ANGLE's GL backend for that rate twice a second, logging a line from Xlib and
one from ANGLE each time it fails (#482). The images' Xvfb reports one mode
per screen: the screen's size, at the rate its fake vblank runs at, from
`-fakescreenfps` or from what `_FAKE_SCREEN_MILLIHZ` or `_FAKE_SCREEN_FPS` on
the root window sets. Read here through libXxf86vm, the library Mesa calls,
the rate is checked the way Mesa computes it, exactly, at the server's start
rate, at 59.94 Hz, at 144 Hz and back, and on a server started at 30.

Usage: python3 tests/integration/test_xvfb_vidmode.py
"""
import ctypes
import ctypes.util
import os
import subprocess
import sys
from fractions import Fraction

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)
import helpers as H  # noqa: E402

SIZE = (1920, 1080)


class ModeLine(ctypes.Structure):
    """libXxf86vm's XF86VidModeModeLine."""
    _fields_ = [(name, ctypes.c_ushort) for name in (
        "hdisplay", "hsyncstart", "hsyncend", "htotal", "hskew",
        "vdisplay", "vsyncstart", "vsyncend", "vtotal")] + [
        ("flags", ctypes.c_uint), ("privsize", ctypes.c_int), ("private", ctypes.c_void_p)]


def libraries() -> tuple:
    """libX11 and libXxf86vm, or a reason they cannot be loaded."""
    names = [ctypes.util.find_library(n) for n in ("X11", "Xxf86vm")]
    if not all(names):
        return None, "libX11 or libXxf86vm is not installed"
    x11, vm = (ctypes.CDLL(n) for n in names)
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    vm.XF86VidModeQueryExtension.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_int),
                                             ctypes.POINTER(ctypes.c_int)]
    vm.XF86VidModeGetModeLine.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.POINTER(ctypes.c_int),
                                          ctypes.POINTER(ModeLine)]
    return (x11, vm), ""


def mode_line(libs: tuple, display_name: str) -> tuple:
    """`(extension present, dot clock in kHz, mode line)` on a fresh connection."""
    x11, vm = libs
    dpy = x11.XOpenDisplay(display_name.encode())
    if not dpy:
        raise RuntimeError(f"cannot open {display_name}")
    try:
        ev, err, dot, line = ctypes.c_int(), ctypes.c_int(), ctypes.c_int(), ModeLine()
        if not vm.XF86VidModeQueryExtension(dpy, ctypes.byref(ev), ctypes.byref(err)):
            return False, 0, line
        if not vm.XF86VidModeGetModeLine(dpy, 0, ctypes.byref(dot), ctypes.byref(line)):
            return True, 0, line
        return True, dot.value, line
    finally:
        x11.XCloseDisplay(dpy)


def mesa_rate(dot: int, line: ModeLine) -> Fraction:
    """The rate as Mesa's glXGetMscRateOML computes it; 0 where its 32 bits overflow."""
    n, d = dot * 1000, line.htotal * line.vtotal
    return Fraction(n, d) if d and n < 2 ** 32 else Fraction(0)


def set_rate(display_name: str, name: str, value) -> None:
    """Set the root window's `name` to `value`, or delete it for None."""
    from selkies.Xlib import Xatom, display as x11_display
    d = x11_display.Display(display_name)
    try:
        root, atom = d.screen().root, d.intern_atom(name)
        if value is None:
            root.delete_property(atom)
        else:
            root.change_property(atom, Xatom.CARDINAL, 32, [value])
        d.sync()
    finally:
        d.close()


def images_xvfb() -> bool:
    """Whether the Xvfb on PATH is the one the images build: its patches add
    the spare outputs' CRTCs to its usage, which build-xvfb.sh checks too."""
    usage = subprocess.run(["Xvfb", "-help"], capture_output=True, text=True, timeout=10)
    return "CRTCs per screen" in usage.stdout + usage.stderr


def check_rates(res: "H.Results", libs: tuple, display_name: str) -> None:
    on, dot, line = mode_line(libs, display_name)
    res.check("the server offers XFree86-VidModeExtension", on)
    res.check("the mode line is the screen's size",
              (line.hdisplay, line.vdisplay) == SIZE, (line.hdisplay, line.vdisplay))
    res.check("its sync pulses sit inside its blanking",
              line.hdisplay <= line.hsyncstart <= line.hsyncend <= line.htotal
              and line.vdisplay < line.vsyncstart <= line.vsyncend < line.vtotal,
              (line.hsyncstart, line.hsyncend, line.htotal, line.vsyncstart, line.vsyncend, line.vtotal))
    rate = mesa_rate(dot, line)
    res.check("it runs at the 60 Hz the server started at", rate == 60, rate)
    for name, value, want, label in (
            ("_FAKE_SCREEN_MILLIHZ", 59940, Fraction(59940, 1000), "59.94 Hz, from _FAKE_SCREEN_MILLIHZ"),
            ("_FAKE_SCREEN_MILLIHZ", None, None, None),
            ("_FAKE_SCREEN_FPS", 144, Fraction(144), "144 Hz, from _FAKE_SCREEN_FPS"),
            ("_FAKE_SCREEN_FPS", None, Fraction(60), "60 Hz again once the properties are gone")):
        set_rate(display_name, name, value)
        if want is None:
            continue
        _, dot, line = mode_line(libs, display_name)
        rate = mesa_rate(dot, line)
        res.check(f"it reads exactly {label}", rate == want, (dot, line.htotal, line.vtotal, rate))


def main() -> bool:
    res = H.Results("xvfb-vidmode")
    libs, why = libraries()
    if not libs:
        res.skip("the server reports its rate through VidMode", why)
        return res.summary()
    if not images_xvfb():
        res.skip("the server reports its rate through VidMode", "the Xvfb on PATH is not the images' own")
        return res.summary()
    xvfb, display_name = H.private_x_server(*SIZE)
    try:
        check_rates(res, libs, display_name)
    finally:
        xvfb.terminate()
        xvfb.wait(timeout=10)
    xvfb, display_name = H.private_x_server(*SIZE, extra_args=("-fakescreenfps", "30"))
    try:
        _, dot, line = mode_line(libs, display_name)
        rate = mesa_rate(dot, line)
        res.check("a server started at -fakescreenfps 30 reads 30 Hz", rate == 30, rate)
    finally:
        xvfb.terminate()
        xvfb.wait(timeout=10)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
