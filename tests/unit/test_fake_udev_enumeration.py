#!/usr/bin/env python3
"""fake-udev adds the pads the interposer is serving to what the real libudev reports.

A udev consumer that opens a node the server is not serving pays the
interposer's full connect timeout before the open fails, once per unbound
slot, so fake-udev lists a js or event node only while its socket is bound,
the way the interposer's own directory listing does. A pad bound later reaches
the consumer through the monitor's inotify add. Everything outside the pads
passes through to the system library, so a consumer asking for another
subsystem (KWin for its render nodes) sees the same devices as without the
preload, and the host's own input devices stay listed beside the pads.

Builds fake-udev and the udevscan tool into a scratch directory, then scans
under LD_PRELOAD with sockets present for none, one and all four slots, scans
a subsystem the pads do not live in, and scans with passthrough disabled.
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


def scan(lib: str, tool: str, socket_dir: str, subsystem: str = "input",
         passthrough: bool = True) -> dict:
    """Counts udevscan reports under fake-udev with the sockets in `socket_dir`.

    The devnodes it listed come back under "nodes".
    """
    env = {**os.environ, "SELKIES_JS_SOCKET_PATH": socket_dir}
    if lib:
        env["LD_PRELOAD"] = lib
    if not passthrough:
        env["SELKIES_REAL_LIBUDEV"] = "none"
    out = subprocess.run([tool, subsystem], env=env, capture_output=True, text=True,
                         timeout=30).stdout
    line = next((ln for ln in out.splitlines() if ln.startswith("RESULT")), "")
    counts = dict(kv.split("=") for kv in line.split()[1:]) if line else {}
    result = {k: int(v) for k, v in counts.items()}
    result["nodes"] = [ln.split()[2] for ln in out.splitlines() if ln.startswith("DEV ")]
    return result


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
    # The host's own input devices, which stay visible beside the pads; a real
    # node that shares a pad's name is hidden, so it is left out of the baseline.
    baseline = scan(None, tool, sockets)
    hidden = {"/dev/input/js%d" % i for i in range(4)} | {"/dev/input/event%d" % (1000 + i) for i in range(4)}
    host = [n for n in baseline["nodes"] if n not in hidden]
    empty = scan(lib, tool, sockets)
    res.check("no bound socket, no pad enumerated",
              empty.get("virtual") == 0 and empty.get("input_devs") == len(host), empty)
    bind(sockets, 0)
    one = scan(lib, tool, sockets)
    res.check("one bound slot enumerates its js and event nodes only",
              one.get("virtual") == 2 and one.get("input_devs") == len(host) + 2, one)
    for slot in (1, 2, 3):
        bind(sockets, slot)
    four = scan(lib, tool, sockets)
    res.check("all four bound slots enumerate eight nodes",
              four.get("virtual") == 8 and four.get("input_devs") == len(host) + 8, four)
    res.check("every pad node reports ID_INPUT_JOYSTICK",
              four.get("joydevs", 0) >= 8, four)
    os.remove(os.path.join(sockets, "selkies_js2.sock"))
    partial = scan(lib, tool, sockets)
    res.check("a slot serving only its event socket lists just that node",
              partial.get("virtual") == 7 and partial.get("input_devs") == len(host) + 7, partial)
    real_mem = scan(None, tool, sockets, "mem")
    faked_mem = scan(lib, tool, sockets, "mem")
    res.check("another subsystem passes through to the real libudev",
              faked_mem["nodes"] == real_mem["nodes"] and "/dev/null" in faked_mem["nodes"],
              faked_mem)
    isolated = scan(lib, tool, sockets, passthrough=False)
    res.check("with passthrough disabled only the pads remain",
              isolated.get("virtual") == 7 and isolated.get("input_devs") == 7, isolated)
    res.check("with passthrough disabled another subsystem is empty",
              scan(lib, tool, sockets, "mem", passthrough=False).get("input_devs") == 0)

sys.exit(0 if res.summary() else 1)
