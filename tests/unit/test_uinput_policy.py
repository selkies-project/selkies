#!/usr/bin/env python3
"""SELKIES_UINPUT_GAMEPAD decides which gamepad backend a session gets. The
whole matrix is exercised for both device states, so the result does not depend
on whether the machine running the tests has /dev/uinput."""
import os
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
import selkies.input_handler as ih  # noqa: E402

INTERPOSER = "/usr/$LIB/selkies_joystick_interposer.so"
PRELOADED = "/usr/lib/x86_64-linux-gnu/selkies_joystick_interposer.so"

fails = []


def case(label, mode, env, expected):
    saved_preload = os.environ.get("LD_PRELOAD", "")
    os.environ.pop("SELKIES_INTERPOSER", None)
    os.environ["LD_PRELOAD"] = env.get("LD_PRELOAD", "")
    if "SELKIES_INTERPOSER" in env:
        os.environ["SELKIES_INTERPOSER"] = env["SELKIES_INTERPOSER"]
    try:
        got = ih.uinput_gamepads_enabled(mode)
    finally:
        os.environ["LD_PRELOAD"] = saved_preload
        os.environ.pop("SELKIES_INTERPOSER", None)
    ok = got == expected
    if not ok:
        fails.append(label)
    print(f"{'PASS' if ok else 'FAIL'}  {label}  -> {got} (want {expected})")


def run(has_uinput):
    """The device check is the one thing a test host cannot be asked to
    provide, so it is substituted; every other input is real."""
    real = ih.uinput_writable
    ih.uinput_writable = lambda: has_uinput
    state = "with /dev/uinput" if has_uinput else "without /dev/uinput"
    try:
        case(f"[{state}] auto yields to SELKIES_INTERPOSER", "auto",
             {"SELKIES_INTERPOSER": INTERPOSER}, False)
        case(f"[{state}] auto yields to a preloaded interposer", "auto",
             {"LD_PRELOAD": PRELOADED}, False)
        case(f"[{state}] auto keeps uinput when an unrelated library preloads", "auto",
             {"LD_PRELOAD": "/usr/lib/libfakeroot.so"}, has_uinput)
        case(f"[{state}] auto uses uinput when nothing preloads", "auto", {}, has_uinput)
        case(f"[{state}] false never uses uinput", "false", {}, False)
        case(f"[{state}] true ignores the interposer", "true",
             {"SELKIES_INTERPOSER": INTERPOSER}, has_uinput)
        case(f"[{state}] unknown value falls back to auto", "banana", {}, has_uinput)
        case(f"[{state}] empty value falls back to auto", "", {}, has_uinput)
    finally:
        ih.uinput_writable = real


run(True)
run(False)
print("RESULT", "all passed" if not fails else f"FAILED: {fails}")
sys.exit(1 if fails else 0)
