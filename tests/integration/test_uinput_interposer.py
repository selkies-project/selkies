#!/usr/bin/env python3
"""An application under the interposer with no kernel /dev/uinput still creates
virtual input devices: its open("/dev/uinput"), the UI_* setup ioctls and the
event writes are served in userspace, the created device appears as a
/dev/input/eventN backed by a socket, and a sibling process reads it as an
ordinary evdev device. On a host with a writable /dev/uinput the path is not
taken -- the kernel node is opened for real.

  create   one process builds a device, another opens the node and checks its
           EVIOCG* identity and that the event stream arrives byte for byte
  udev     a gamepad-capable device is discovered through fake-udev with its
           ID_INPUT_JOYSTICK class, and vanishes once its creator exits

Usage: test_uinput_interposer.py [create|udev|all]
"""
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

INTERPOSER_DIR = os.path.join(H.REPO, "addons", "js-interposer")
FAKE_UDEV_DIR = os.path.join(H.REPO, "addons", "fake-udev")
TOOLS = os.path.join(H.REPO, "tests", "tools")


def build_interposer(workdir: str) -> str:
    so = os.path.join(workdir, "selkies_input_interposer.so")
    subprocess.run(
        ["gcc", "-O2", "-shared", "-fPIC", "-o", so,
         os.path.join(INTERPOSER_DIR, "joystick_interposer.c"), "-ldl", "-pthread"],
        check=True, capture_output=True, text=True)
    return so


def build_tool(name: str) -> str:
    subprocess.run(["make", "-s", "-C", TOOLS, name], check=True, capture_output=True, text=True)
    return os.path.join(TOOLS, name)


def clean_env() -> dict:
    return {k: v for k, v in os.environ.items() if k not in ("LD_PRELOAD", "SDL_JOYSTICK_DEVICE")}


def create_check(res: "H.Results", preload: str, work: str) -> None:
    """Build a device in one process, read it back in another; both preloaded."""
    tool = build_tool("uinput_create_consume")
    sockdir = os.path.join(work, "create")
    os.makedirs(sockdir)
    env = clean_env()
    env.update({"LD_PRELOAD": preload, "SELKIES_JS_SOCKET_PATH": sockdir})
    out = subprocess.run([tool], env=env, capture_output=True, text=True, timeout=60)
    line = out.stdout.strip()
    res.check("create-consume-round-trip", line.startswith("PASS"),
              (line + " | " + out.stderr.strip().replace("\n", " ; "))[:300])


def udev_check(res: "H.Results", preload: str, work: str) -> None:
    """A gamepad device the interposer serves is enumerated by fake-udev as a
    joystick, and disappears when its creator exits."""
    try:
        subprocess.run(["make", "-s", "-C", FAKE_UDEV_DIR, "libudev.so.1"],
                       check=True, capture_output=True, text=True)
    except subprocess.CalledProcessError as e:
        res.skip("udev-dynamic-enumeration", f"fake-udev build failed: {e.stderr[:120]}")
        return
    fake = os.path.join(FAKE_UDEV_DIR, "libudev.so.1")
    scan = os.path.join(work, "udevscan")
    src = os.path.join(TOOLS, "gamepad", "udevscan.c")
    if subprocess.run(["gcc", "-O2", "-o", scan, src, "-ludev"], capture_output=True).returncode != 0:
        res.skip("udev-dynamic-enumeration", "no libudev development files")
        return
    tool = build_tool("uinput_create_consume")
    sockdir = os.path.join(work, "udev")
    os.makedirs(sockdir)
    creator_env = clean_env()
    creator_env.update({"LD_PRELOAD": preload, "SELKIES_JS_SOCKET_PATH": sockdir})
    hold = subprocess.Popen([tool, "hold"], env=creator_env, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, text=True)
    node = ""
    for _ in range(100):
        line = hold.stdout.readline().strip()
        if line:
            node = line
            break
        time.sleep(0.05)

    scan_env = clean_env()
    scan_env.update({"LD_PRELOAD": fake, "SELKIES_JS_SOCKET_PATH": sockdir, "SELKIES_REAL_LIBUDEV": "none"})
    listed = subprocess.run([scan, "input"], env=scan_env, capture_output=True, text=True, timeout=30).stdout
    seen = node and any(node in ln for ln in listed.splitlines())
    joystick = "joystick:" in listed and node and f"/{node}" in listed.split("joystick:", 1)[1].splitlines()[0]
    res.check("udev-dynamic-enumeration", bool(seen and joystick),
              f"node={node} listed={seen} joystick={joystick}")

    hold.terminate()
    try:
        hold.wait(timeout=5)
    except subprocess.TimeoutExpired:
        hold.kill()
    time.sleep(0.2)
    gone = subprocess.run([scan, "input"], env=scan_env, capture_output=True, text=True, timeout=30).stdout
    res.check("udev-node-vanishes-with-creator", node and not any(node in ln for ln in gone.splitlines()),
              f"node={node} still_listed={any(node in ln for ln in gone.splitlines())}")


def main() -> bool:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    res = H.Results("uinput-interposer")
    work = os.path.join(H.WORKDIR, "uinput-interposer")
    subprocess.run(["rm", "-rf", work], check=False)
    os.makedirs(work)
    try:
        preload = build_interposer(work)
    except subprocess.CalledProcessError as e:
        res.check("build-interposer", False, e.stderr[:200])
        return res.summary()
    if which in ("create", "all"):
        create_check(res, preload, work)
    if which in ("udev", "all"):
        udev_check(res, preload, work)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
