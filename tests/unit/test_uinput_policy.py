#!/usr/bin/env python3
"""SELKIES_UINPUT_GAMEPAD decides which gamepad backend a session gets. The
whole matrix is exercised for both device states, so the result does not depend
on whether the machine running the tests has /dev/uinput; the device check that
feeds it runs for real against paths standing in for what the node can do.

The keyboard and pointer the session publishes stay off the kernel wherever the
X server would read them back, which it tells by listing a hotplugged device
with its node; that rule is checked against the device lists each server kind
answers."""
import os
import sys
import tempfile
import threading
from types import SimpleNamespace as NS

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
import selkies.input_handler as ih  # noqa: E402

INTERPOSER = "/usr/$LIB/selkies_input_interposer.so"
PRELOADED = "/usr/lib/x86_64-linux-gnu/selkies_input_interposer.so"

fails = []


def case(label: str, mode: str, env: dict, expected: bool) -> None:
    """Resolve the backend under `env` and record whether it matches."""
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


def probe_case(label: str, path: str, expected: bool) -> None:
    """Run the real device check against `path` off-thread: an open that never
    returns has to read as a failure here rather than hang the suite."""
    ih.UINPUT_PATH = path
    answer: list = []
    worker = threading.Thread(target=lambda: answer.append(ih.uinput_writable()), daemon=True)
    worker.start()
    worker.join(2)
    got = answer[0] if answer else "blocked"
    ok = got == expected
    if not ok:
        fails.append(label)
    print(f"{'PASS' if ok else 'FAIL'}  {label}  -> {got} (want {expected})")


def probe() -> None:
    """What the device check answers for each way the node can behave."""
    real_path = ih.UINPUT_PATH
    try:
        with tempfile.TemporaryDirectory() as tmp:
            node = os.path.join(tmp, "node")
            open(node, "wb").close()
            refused = os.path.join(tmp, "refused")
            open(refused, "wb").close()
            os.chmod(refused, 0o444)
            reader_less = os.path.join(tmp, "reader-less")
            os.mkfifo(reader_less)
            probe_case("[device] a node this process can open for writing", node, True)
            probe_case("[device] no node at all", os.path.join(tmp, "absent"), False)
            probe_case("[device] a node that refuses the open", refused, False)
            probe_case("[device] a node a blocking open would never return from",
                       reader_less, False)
    finally:
        ih.UINPUT_PATH = real_path


def run(has_uinput: bool) -> None:
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


def x_server(node_atom: int, atoms_by_device: dict) -> NS:
    """An X connection answering the device list and properties given."""
    return NS(
        get_atom=lambda name, only_if_exists=False: node_atom,
        xinput_query_device=lambda which: NS(devices=[NS(deviceid=d) for d in atoms_by_device]),
        xinput_list_device_properties=lambda d: NS(atoms=atoms_by_device[d]),
    )


def readback_case(label: str, xdisplay: object, expected: bool) -> None:
    got = ih.WebRTCInput._display_reads_kernel_input(NS(xdisplay=xdisplay))
    ok = got == expected
    if not ok:
        fails.append(label)
    print(f"{'PASS' if ok else 'FAIL'}  {label}  -> {got} (want {expected})")


def readback() -> None:
    """Which X servers read kernel input devices back into the session."""
    readback_case("[display] no X connection (Wayland)", None, False)
    readback_case("[display] no Device Node atom (Xvfb)", x_server(0, {4: [120], 5: [120]}), False)
    readback_case("[display] a hotplugged slave listed with its node (Xorg)",
                  x_server(240, {4: [120], 6: [120, 240]}), True)
    readback_case("[display] the atom interned but on no device",
                  x_server(240, {4: [120], 5: []}), False)


probe()
run(True)
run(False)
readback()
print("RESULT", "all passed" if not fails else f"FAILED: {fails}")
sys.exit(1 if fails else 0)
