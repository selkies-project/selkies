#!/usr/bin/env python3
"""Selkies' kernel devices name themselves, so a host can keep them off its seat.

A kernel device Selkies registers from inside a container is a device of the
host's kernel, and the host's own session would take it: a desktop at the
host's screen the virtual keyboard and pointer, its games the pad. Each device
reports a physical path the documented udev rule keys on
(docs/components/input-interposer.md, "Keeping the devices off the host's
seat"): a pad the path the interposer reports for its slot, so the two
backends stay alike, and the virtual keyboard and pointer
`selkies/virtinput/<kind>`.

The interposer block reads EVIOCGPHYS through the interposer. The kernel block
needs a writable /dev/uinput and is skipped without one: it reads EVIOCGPHYS
and sysfs on the kernel's nodes and, where udevadm can load an extra rules
directory (systemd 256 and later), what `udevadm test` makes of each node
with the documented rule added to the system's own, which is never installed:
the rule's seat, and no uaccess tag left for the seat's user.

Usage: test_seat_isolation.py [interposer|kernel|all]
"""
import asyncio
import json
import os
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import selkies.input_handler as ih

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_gamepad_rumble import build_interposer  # noqa: E402

DOC = os.path.join(H.REPO, "docs", "components", "input-interposer.md")
RULE_NAME = "72-selkies-seat.rules"

# EVIOCGPHYS(256) on a node, as an application asks it.
PHYS_APP = r'''
import fcntl, json, os, sys
fd = os.open(sys.argv[1], os.O_RDONLY | os.O_NONBLOCK)
buf = bytearray(256)
fcntl.ioctl(fd, (2 << 30) | (256 << 16) | (ord("E") << 8) | 0x07, buf, True)
print(json.dumps({"phys": bytes(buf).split(b"\0", 1)[0].decode()}), flush=True)
'''


def documented_rule() -> str:
    """The rule line the docs give."""
    with open(DOC) as f:
        return next((ln.strip() for ln in f if ln.startswith('SUBSYSTEM=="input", ATTRS{phys}')), "")


def phys_of(node: str, env: dict) -> str:
    out = subprocess.run([sys.executable, "-c", PHYS_APP, node], env=env, capture_output=True,
                         text=True, timeout=30)
    lines = [ln for ln in out.stdout.splitlines() if ln.startswith("{")]
    return json.loads(lines[-1])["phys"] if lines else f"error: {out.stderr.strip()[-200:]}"


def sysfs_phys(node: str) -> str:
    try:
        with open(f"/sys/class/input/{os.path.basename(node)}/device/phys") as f:
            return f.read().strip()
    except OSError as e:
        return f"error: {e}"


def udev_extra_rules() -> bool:
    """Whether this udevadm can add a rules directory to a test run."""
    if not shutil.which("udevadm"):
        return False
    out = subprocess.run(["udevadm", "test", "--help"], capture_output=True, text=True, timeout=30)
    return "--extra-rules-dir" in out.stdout + out.stderr


def udev_view(node: str, rules_dir: str) -> dict:
    """What `udevadm test` makes of a node's add with `rules_dir` added: its
    properties, the current tags split."""
    out = subprocess.run(["udevadm", "test", f"--extra-rules-dir={rules_dir}", "--action=add",
                          f"/sys/class/input/{os.path.basename(node)}"],
                         capture_output=True, text=True, timeout=60)
    props = {}
    for line in out.stdout.splitlines():
        key, sep, value = line.strip().partition("=")
        if sep and key.isupper() and key.replace("_", "").isalnum():
            props[key] = value
    props["CURRENT_TAGS"] = [t for t in props.get("CURRENT_TAGS", "").split(":") if t]
    return props


async def interposer(res: "H.Results", work: str, env: dict) -> None:
    preload = build_interposer(work)
    loop = asyncio.get_running_loop()
    gp = ih.SelkiesGamepad(os.path.join(work, "selkies_js0.sock"),
                           os.path.join(work, "selkies_event1000.sock"), loop)
    gp.set_config(ih.STANDARD_XPAD_CONFIG["name"], len(ih.STANDARD_XPAD_CONFIG["btn_map"]),
                  len(ih.STANDARD_XPAD_CONFIG["axes_map"]))
    server = asyncio.ensure_future(gp.run_servers())
    await asyncio.sleep(0.5)
    try:
        got = await loop.run_in_executor(None, phys_of, "/dev/input/event1000",
                                         dict(env, LD_PRELOAD=preload, SELKIES_JS_SOCKET_PATH=work))
        res.check("interposer: the pad's physical path is the one the rule keys on", got == ih.pad_phys(0), got)
    finally:
        await gp.close()
        server.cancel()


async def kernel(res: "H.Results", work: str, env: dict) -> None:
    if not ih.uinput_writable():
        res.skip("kernel", f"{ih.UINPUT_PATH} is missing or not writable here")
        return
    loop = asyncio.get_running_loop()
    gp = ih.SelkiesGamepad(os.path.join(work, "selkies_js1.sock"), os.path.join(work, "selkies_event1001.sock"),
                           loop, uinput_enabled=True)
    gp.set_config(ih.STANDARD_XPAD_CONFIG["name"], len(ih.STANDARD_XPAD_CONFIG["btn_map"]),
                  len(ih.STANDARD_XPAD_CONFIG["axes_map"]))
    gp.running = True
    gp.ensure_uinput()
    devices = []
    for kind, name, product, evbits, keybits, relbits in (
            ("keyboard", "Selkies Virtual Keyboard", 0x0001, [ih.EV_KEY], list(range(1, ih.BTN_MISC)), []),
            ("pointer", "Selkies Virtual Pointer", 0x0002, [ih.EV_KEY, ih.EV_REL],
             [ih.BTN_LEFT, ih.BTN_RIGHT, ih.BTN_MIDDLE], [ih.REL_X, ih.REL_Y])):
        device = ih.VirtualInputDevice(name, 0x1D6B, product, evbits, keybits, relbits, sock_dir=work,
                                       phys=f"selkies/virtinput/{kind}")
        if await device.open(kernel=True):
            devices.append((kind, device))
    try:
        await asyncio.sleep(0.5)
        pad = next((n for n in (gp.uinput.device_nodes if gp.uinput else []) if "event" in n), None)
        nodes = [("pad", pad, ih.pad_phys(1))] + [
            (kind, device.node, f"selkies/virtinput/{kind}") for kind, device in devices]
        for kind, node, want in nodes:
            if not node or not node.startswith("/dev/input/event"):
                res.check(f"kernel: the {kind}'s event node exists", False, node)
                continue
            got = await loop.run_in_executor(None, phys_of, node, env)
            res.check(f"kernel: the {kind} reports its physical path ({node})", got == want, got)
            res.check(f"kernel: and sysfs names the {kind} the same", sysfs_phys(node) == want, sysfs_phys(node))
        if not udev_extra_rules():
            res.skip("kernel: udev", "no udevadm that takes --extra-rules-dir (systemd 256 or later)")
            return
        rules = os.path.join(work, "rules")
        os.makedirs(rules, exist_ok=True)
        with open(os.path.join(rules, RULE_NAME), "w") as f:
            f.write(documented_rule() + "\n")
        for kind, node, _ in nodes:
            if not node or not node.startswith("/dev/input/event"):
                continue
            view = await loop.run_in_executor(None, udev_view, node, rules)
            res.check(f"kernel: with the rule, udev puts the {kind} on a seat of its own, tagged for no seat user",
                      view.get("ID_SEAT") == "seat-selkies" and "uaccess" not in view["CURRENT_TAGS"],
                      f"ID_SEAT={view.get('ID_SEAT')} CURRENT_TAGS={view['CURRENT_TAGS']}")
    finally:
        for _, device in devices:
            await device.close()
        await gp.close()


async def main(which: str) -> "H.Results":
    res = H.Results("seat-isolation")
    res.check("the docs give the rule", documented_rule().startswith('SUBSYSTEM=="input"'), documented_rule())
    work = tempfile.mkdtemp(prefix="seat-", dir=H.WORKDIR if os.path.isdir(H.WORKDIR) else None)
    env = {k: v for k, v in os.environ.items() if k != "LD_PRELOAD"}
    if which in ("interposer", "all"):
        await interposer(res, work, env)
    if which in ("kernel", "all"):
        await kernel(res, work, env)
    return res


if __name__ == "__main__":
    r = asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else "all"))
    sys.exit(0 if r.summary() else 1)
