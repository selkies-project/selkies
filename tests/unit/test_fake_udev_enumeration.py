#!/usr/bin/env python3
"""fake-udev enumerates only the pads the interposer is serving.

A udev consumer that opens a node the server is not serving pays the
interposer's full connect timeout before the open fails, once per unbound
slot, so fake-udev lists a js or event node only while its socket is bound,
the way the interposer's own directory listing does. A pad bound later reaches
the consumer through the monitor's inotify add.

Builds fake-udev and the udevscan tool into a scratch directory, then scans
under LD_PRELOAD with sockets present for none, one and all four slots.
"""
import os
import shutil
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, TESTS)
import helpers as H  # noqa: E402

res = H.Results("fake-udev-enumeration")


def build(scratch: str) -> tuple:
    """The fake libudev and the scan tool, built off the tree."""
    src = os.path.join(scratch, "fake-udev")
    shutil.copytree(os.path.join(REPO, "addons", "fake-udev"), src,
                    ignore=shutil.ignore_patterns("*.o", "*.so*"))
    subprocess.run(["make", "-s", "-C", src, "libudev.so.1"], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    tool = os.path.join(scratch, "udevscan")
    subprocess.run(["gcc", "-O2", "-o", tool,
                    os.path.join(TESTS, "tools", "gamepad", "udevscan.c"), "-ludev"],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return os.path.join(src, "libudev.so.1"), tool


def scan(lib: str, tool: str, socket_dir: str) -> dict:
    """Counts udevscan reports under fake-udev with the sockets in `socket_dir`."""
    env = {**os.environ, "LD_PRELOAD": lib, "SELKIES_JS_SOCKET_PATH": socket_dir}
    out = subprocess.run([tool], env=env, capture_output=True, text=True, timeout=30).stdout
    line = next((ln for ln in out.splitlines() if ln.startswith("RESULT")), "")
    counts = dict(kv.split("=") for kv in line.split()[1:]) if line else {}
    return {k: int(v) for k, v in counts.items()}


def bind(socket_dir: str, slot: int) -> None:
    """Stand-ins for the two sockets the interposer binds for `slot`."""
    for name in (f"js{slot}", f"event{1000 + slot}"):
        open(os.path.join(socket_dir, f"selkies_{name}.sock"), "w").close()


with tempfile.TemporaryDirectory(prefix="selkies-fake-udev-") as scratch:
    if shutil.which("gcc") is None or shutil.which("make") is None:
        res.skip("fake-udev enumeration follows the bound sockets", "no gcc/make")
        sys.exit(0 if res.summary() else 1)
    try:
        lib, tool = build(scratch)
    except subprocess.CalledProcessError as e:
        res.skip("fake-udev enumeration follows the bound sockets",
                 f"build failed: {e.stderr.decode(errors='replace')[-200:]}")
        sys.exit(0 if res.summary() else 1)
    sockets = os.path.join(scratch, "sockets")
    os.makedirs(sockets)
    empty = scan(lib, tool, sockets)
    res.check("no bound socket, no pad enumerated",
              empty.get("input_devs") == 0 and empty.get("joydevs") == 0, empty)
    bind(sockets, 0)
    one = scan(lib, tool, sockets)
    res.check("one bound slot enumerates its js and event nodes only",
              one.get("input_devs") == 2 and one.get("joydevs") == 2, one)
    for slot in (1, 2, 3):
        bind(sockets, slot)
    four = scan(lib, tool, sockets)
    res.check("all four bound slots enumerate eight nodes",
              four.get("input_devs") == 8 and four.get("joydevs") == 8, four)
    os.remove(os.path.join(sockets, "selkies_js2.sock"))
    partial = scan(lib, tool, sockets)
    res.check("a slot serving only its event socket lists just that node",
              partial.get("input_devs") == 7 and partial.get("joydevs") == 7, partial)

sys.exit(0 if res.summary() else 1)
