#!/usr/bin/env python3
"""The session's keyboard and pointer, published as input devices.

Applications that read evdev directly see nothing of the X or compositor
injection a Selkies session runs on, so the same events are carried on devices
they can open. Where /dev/uinput is writable the kernel serves them; otherwise
the Input Interposer's dynamic pool does, which is what this checks: the
descriptor identifies the device, fake-udev derives its class from the
capability bits, and the event stream arrives byte for byte.

Usage: test_virtual_input_devices.py [publish|stream|all]
"""
import os
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

INTERPOSER_DIR = os.path.join(H.REPO, "addons", "input-interposer")
sys.path.insert(0, os.path.join(H.REPO, "src"))
from selkies.input_handler import UINPUT_PATH  # noqa: E402


def build_interposer(workdir: str) -> str:
    so = os.path.join(workdir, "selkies_input_interposer.so")
    subprocess.run(
        ["gcc", "-O2", "-shared", "-fPIC", "-o", so,
         os.path.join(INTERPOSER_DIR, "input_interposer.c"), "-ldl", "-pthread"],
        check=True, capture_output=True, text=True)
    return so


PUBLISHER = r'''
import asyncio, os, sys
sys.path.insert(0, %r)
import selkies.input_handler as ih
SOCK = os.environ["SOCKDIR"]
async def main():
    kbd = ih.VirtualInputDevice("Selkies Virtual Keyboard", 0x1D6B, 0x0001,
                                [ih.EV_KEY], list(range(1, ih.BTN_MISC)), [], sock_dir=SOCK)
    ptr = ih.VirtualInputDevice("Selkies Virtual Pointer", 0x1D6B, 0x0002,
                                [ih.EV_KEY, ih.EV_REL],
                                [ih.BTN_LEFT, ih.BTN_RIGHT, ih.BTN_MIDDLE],
                                [ih.REL_X, ih.REL_Y, ih.REL_WHEEL, ih.REL_HWHEEL], sock_dir=SOCK)
    assert await kbd.open() and await ptr.open()
    print(kbd.node, ptr.node, flush=True)
    open(SOCK + "/ready", "w").close()
    while not os.path.exists(SOCK + "/reader"):
        await asyncio.sleep(0.05)
    await asyncio.sleep(0.4)
    for args in ((ih.EV_KEY, 30, 1), (ih.EV_KEY, 30, 0)):
        kbd.emit(*args); await asyncio.sleep(0.05)
    for args in ((ih.EV_REL, ih.REL_X, 7), (ih.EV_KEY, ih.BTN_LEFT, 1), (ih.EV_KEY, ih.BTN_LEFT, 0)):
        ptr.emit(*args); await asyncio.sleep(0.05)
    await asyncio.sleep(1.0)
    await kbd.close(); await ptr.close()
asyncio.run(main())
'''

READER = r'''
import os, select, struct, sys
SOCK = os.environ["SOCKDIR"]
open(SOCK + "/reader", "w").close()
fmt, out = "=qqHHi", {}
for node, count in ((%r, 4), (%r, 6)):
    fd, got, buf = os.open(node, os.O_RDONLY), [], b""
    while len(got) < count:
        if not select.select([fd], [], [], 6)[0]:
            break
        data = os.read(fd, struct.calcsize(fmt) * 8)
        if not data:
            break
        buf += data
        while len(buf) >= struct.calcsize(fmt):
            _, _, t, c, v = struct.unpack(fmt, buf[:struct.calcsize(fmt)])
            buf = buf[struct.calcsize(fmt):]
            got.append((t, c, v))
    os.close(fd)
    out[node] = got
print(out)
'''


def run(res: "H.Results", preload: str, work: str) -> None:
    sock = os.path.join(work, "sock")
    os.makedirs(sock, exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k != "LD_PRELOAD"}
    env.update({"SOCKDIR": sock, "SELKIES_JS_SOCKET_PATH": sock})
    pub = subprocess.Popen([sys.executable, "-c", PUBLISHER % os.path.join(H.REPO, "src")],
                           env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    try:
        nodes = pub.stdout.readline().split()
        res.check("keyboard and pointer are published",
                  nodes == ["/dev/input/event3000", "/dev/input/event3001"], nodes)
        res.check("descriptor and socket exist for both",
                  all(os.path.exists(os.path.join(sock, f"selkies_event{n}{s}"))
                      for n in (3000, 3001) for s in (".sock", ".desc")),
                  sorted(os.listdir(sock)))
        interposed = all(n.startswith("/dev/input/event3") for n in nodes)
        res.check("the backend matches what this host offers",
                  interposed != os.access(UINPUT_PATH, os.W_OK), nodes)
        out = subprocess.run([sys.executable, "-c", READER % tuple(nodes)],
                             env={**env, **({"LD_PRELOAD": preload} if interposed else {})},
                             capture_output=True, text=True, timeout=60).stdout.strip()
        got = eval(out) if out.startswith("{") else {}
        res.check("the keyboard stream arrives byte for byte",
                  got.get(nodes[0]) == [(1, 30, 1), (0, 0, 0), (1, 30, 0), (0, 0, 0)],
                  got.get(nodes[0]))
        res.check("the pointer stream arrives byte for byte",
                  got.get(nodes[1]) ==
                  [(2, 0, 7), (0, 0, 0), (1, 272, 1), (0, 0, 0), (1, 272, 0), (0, 0, 0)],
                  got.get(nodes[1]))
        pub.wait(timeout=30)
    finally:
        if pub.poll() is None:
            pub.terminate()
            pub.wait(timeout=10)
    res.check("both nodes are withdrawn when their publisher retires them",
              not any(os.path.exists(os.path.join(sock, f"selkies_event{n}.sock"))
                      for n in (3000, 3001)), sorted(os.listdir(sock)))


def main() -> "H.Results":
    res = H.Results("virtual-input-devices")
    with tempfile.TemporaryDirectory(prefix="selkies-vid-") as work:
        run(res, build_interposer(work), work)
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
