#!/usr/bin/env python3
"""The kernel gamepad backend encodes the uinput ioctls and packs its structs in
pure Python. This compares every one of them against what <linux/uinput.h>
defines on this machine, which is the only thing that makes the backend correct.
"""
import os
import struct
import subprocess
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
import selkies.input_handler as ih  # noqa: E402

TOOLS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "tools")
TRUTH = os.path.join(TOOLS, "uinput_abi_truth")


def kernel_truth():
    if not os.path.exists(TRUTH):
        subprocess.run(["make", "-C", TOOLS, "uinput_abi_truth"], check=True,
                       stdout=subprocess.DEVNULL)
    out = subprocess.run([TRUTH], capture_output=True, text=True, check=True).stdout
    return {k: int(v) for k, v in (line.split() for line in out.splitlines())}


def main():
    truth = kernel_truth()
    fails = []

    def check(name, got, want):
        ok = got == want
        print(f"{'PASS' if ok else 'FAIL'}  {name}: {got} (kernel {want})")
        if not ok:
            fails.append(name)

    check("UI_DEV_CREATE", ih.UI_DEV_CREATE, truth["UI_DEV_CREATE"])
    check("UI_DEV_DESTROY", ih.UI_DEV_DESTROY, truth["UI_DEV_DESTROY"])
    check("UI_DEV_SETUP", ih.UI_DEV_SETUP, truth["UI_DEV_SETUP"])
    check("UI_ABS_SETUP", ih.UI_ABS_SETUP, truth["UI_ABS_SETUP"])
    check("UI_SET_EVBIT", ih.UI_SET_EVBIT, truth["UI_SET_EVBIT"])
    check("UI_SET_KEYBIT", ih.UI_SET_KEYBIT, truth["UI_SET_KEYBIT"])
    check("UI_SET_ABSBIT", ih.UI_SET_ABSBIT, truth["UI_SET_ABSBIT"])
    check("UI_GET_SYSNAME", ih.UI_GET_SYSNAME, truth["UI_GET_SYSNAME"])
    check("UINPUT_MAX_NAME_SIZE", ih.UINPUT_MAX_NAME_SIZE, truth["UINPUT_MAX_NAME_SIZE"])
    check("BUS_USB", ih.BUS_USB, truth["BUS_USB"])

    # struct uinput_setup: input_id, name[80], ff_effects_max
    check("sizeof(struct uinput_setup)", struct.calcsize(ih.UINPUT_SETUP_FMT),
          truth["sizeof_uinput_setup"])
    check("offsetof(uinput_setup, name)", struct.calcsize("=HHHH"), truth["off_setup_name"])
    check("offsetof(uinput_setup, ff_effects_max)",
          struct.calcsize("=HHHH80s"), truth["off_setup_ff"])
    # struct uinput_abs_setup: code, absinfo (value, min, max, fuzz, flat, res)
    check("sizeof(struct uinput_abs_setup)", struct.calcsize(ih.UINPUT_ABS_SETUP_FMT),
          truth["sizeof_uinput_abs_setup"])
    check("offsetof(uinput_abs_setup, absinfo)", struct.calcsize("=H2x"),
          truth["off_abs_setup_absinfo"])
    # struct input_event, as packed by the shared evdev writer
    check("sizeof(struct input_event)",
          len(ih.get_evdev_events_packed(ih.EV_KEY, ih.BTN_A, 1, 64)) // 2,
          truth["sizeof_input_event"])

    print("RESULT", "all passed" if not fails else f"FAILED: {fails}")
    return not fails


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
