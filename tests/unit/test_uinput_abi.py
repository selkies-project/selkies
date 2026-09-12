#!/usr/bin/env python3
"""The kernel gamepad backend encodes the uinput ioctls and packs its structs in
pure Python. This compares every one of them against what <linux/uinput.h> and
<linux/joystick.h> define on this machine, which is the only thing that makes
the backend correct: the ioctl encodings and the evdev and joydev structs, and
the axis values the backend writes into them, which have to land on the ends of
the range it declares to uinput and still fit the joydev event's signed 16-bit
field.
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


def kernel_truth() -> dict:
    """Constants from `<linux/uinput.h>`, via the compiled truth tool (built
    on demand)."""
    if not os.path.exists(TRUTH):
        subprocess.run(["make", "-C", TOOLS, "uinput_abi_truth"], check=True,
                       stdout=subprocess.DEVNULL)
    out = subprocess.run([TRUTH], capture_output=True, text=True, check=True).stdout
    return {k: int(v) for k, v in (line.split() for line in out.splitlines())}


def main() -> bool:
    truth = kernel_truth()
    fails = []

    def check(name: str, got: int, want: int, source: str = "kernel") -> None:
        ok = got == want
        print(f"{'PASS' if ok else 'FAIL'}  {name}: {got} ({source} {want})")
        if not ok:
            fails.append(name)

    def declared(name: str, got: int, want: int) -> None:
        """A value the backend derives, against the range it declares itself."""
        check(name, got, want, "declared")

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
    # struct js_event, as packed by the joydev writer
    check("sizeof(struct js_event)",
          len(ih.get_js_event_packed(ih.JS_EVENT_BUTTON, 0, 0)), truth["sizeof_js_event"])
    check("offsetof(js_event, value)", struct.calcsize("=I"), truth["off_js_value"])
    check("offsetof(js_event, type)", struct.calcsize("=Ih"), truth["off_js_type"])
    check("offsetof(js_event, number)", struct.calcsize("=IhB"), truth["off_js_number"])
    check("JS_EVENT_BUTTON", ih.JS_EVENT_BUTTON, truth["JS_EVENT_BUTTON"])
    check("JS_EVENT_AXIS", ih.JS_EVENT_AXIS, truth["JS_EVENT_AXIS"])
    check("JS_EVENT_INIT", ih.JS_EVENT_INIT, truth["JS_EVENT_INIT"])
    # The synthetic events that open a joydev stream carry JS_EVENT_INIT in the
    # type byte, which an unsigned pack is what holds.
    _, value, ev_type, number = struct.unpack(
        "=IhBB", ih.get_js_event_packed(ih.JS_EVENT_BUTTON | ih.JS_EVENT_INIT, 5, 1))
    check("js_event type with the init flag set", ev_type,
          truth["JS_EVENT_BUTTON"] | truth["JS_EVENT_INIT"])
    check("js_event number", number, 5)
    check("js_event value", value, 1)

    # The axis values the backend writes into those events, against the range it
    # declares to uinput: sticks span it, triggers span it too (joydev and evdev
    # consumers read them as ordinary analog axes), and a hat is -1/0/1 on evdev
    # but full-scale on joydev.
    declared("stick low", ih.normalize_axis_value(-1.0, False, False), ih.ABS_MIN_VAL)
    declared("stick centre", ih.normalize_axis_value(0.0, False, False), 0)
    declared("stick high", ih.normalize_axis_value(1.0, False, False), ih.ABS_MAX_VAL)
    declared("trigger released", ih.normalize_axis_value(0.0, True, False), ih.ABS_MIN_VAL)
    declared("trigger pressed", ih.normalize_axis_value(1.0, True, False), ih.ABS_MAX_VAL)
    declared("hat left (evdev)", ih.normalize_axis_value(-1.0, False, True), ih.ABS_HAT_MIN_VAL)
    declared("hat centre (evdev)", ih.normalize_axis_value(0.0, False, True), 0)
    declared("hat right (evdev)", ih.normalize_axis_value(1.0, False, True), ih.ABS_HAT_MAX_VAL)
    declared("hat past its end is clamped", ih.normalize_axis_value(7.0, False, True),
          ih.ABS_HAT_MAX_VAL)
    declared("hat left (joydev)", ih.normalize_axis_value(-1.0, False, True, True), -ih.ABS_MAX_VAL)
    declared("hat right (joydev)", ih.normalize_axis_value(1.0, False, True, True), ih.ABS_MAX_VAL)
    for label, client_value in (("lowest", -1.0), ("highest", 1.0)):
        _, value, _, _ = struct.unpack("=IhBB", ih.get_js_event_packed(
            ih.JS_EVENT_AXIS, 0, ih.normalize_axis_value(client_value, False, False)))
        declared(f"the {label} axis value survives the joydev event", value,
              ih.normalize_axis_value(client_value, False, False))

    print("RESULT", "all passed" if not fails else f"FAILED: {fails}")
    return not fails


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
