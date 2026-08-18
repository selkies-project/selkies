"""The kernel gamepad backend, driven in-process against the /dev/uinput
emulator: what the kernel would receive has to match the evdev stream the
Joystick Interposer serves, byte for byte, and the device has to be set up with
the same identity and axis ranges."""
import asyncio
import os
import shutil
import struct
import sys
import tempfile
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import selkies.input_handler as ih

if "UINPUT_SHIM_STREAM" not in os.environ:
    # The emulator has to be preloaded before this process opens /dev/uinput
    shim_env, _, _ = H.uinput_shim_env("backend")
    sockets = os.path.join(H.WORKDIR, "backend-sockets")
    shutil.rmtree(sockets, ignore_errors=True)
    os.makedirs(sockets)
    os.execve(sys.executable, [sys.executable, os.path.abspath(__file__)],
              {**os.environ, **shim_env, "SELKIES_JS_SOCKET_PATH": sockets})

SOCK = os.environ["SELKIES_JS_SOCKET_PATH"]
STREAM = os.environ["UINPUT_SHIM_STREAM"]
SHIMLOG = os.environ["UINPUT_SHIM_LOG"]
EV_SIZE = 24

# Entries are (client index, value, is_button).
SCRIPT: list[tuple] = [
    # A press / release.
    (0, 1, True), (0, 0, True),
    # Left trigger, half travel.
    (6, 0.5, True),
    # Dpad up press / release.
    (12, 1, True), (12, 0, True),
    # Left stick.
    (0, -1.0, False), (1, 0.25, False),
    # Guide button.
    (16, 1, True), (16, 0, True),
]

def unpack_stream(blob: bytes) -> list[tuple[int, int, int]]:
    """Decode raw input_event records into (type, code, value) tuples."""
    out = []
    for off in range(0, len(blob) - EV_SIZE + 1, EV_SIZE):
        _s, _u, t, c, v = struct.unpack("=qqHHi", blob[off:off + EV_SIZE])
        out.append((t, c, v))
    return out

async def evdev_client(path: str, collected: list, ready: asyncio.Event) -> None:
    """Connect to the interposer's evdev socket and collect its event bytes.

    Args:
        path: Unix socket path of the interposer's evdev endpoint.
        collected: List that each raw 24-byte event record is appended to.
        ready: Set once the handshake completes and collection is live.
    """
    reader, writer = await asyncio.open_unix_connection(path)
    await reader.readexactly(ih.EXPECTED_C_STRUCT_SIZE)
    writer.write(struct.pack("=B", 8))
    await writer.drain()
    ready.set()
    try:
        while True:
            collected.append(await reader.readexactly(EV_SIZE))
    except (asyncio.IncompleteReadError, ConnectionResetError):
        pass

COLLECTED: list[bytes] = []

async def main() -> None:
    """Run the gamepad servers, attach an evdev client, and play the script."""
    gp = ih.SelkiesGamepad(os.path.join(SOCK, "selkies_js0.sock"),
                           os.path.join(SOCK, "selkies_event1000.sock"),
                           asyncio.get_running_loop(), uinput_enabled=True)
    gp.set_config("Test Pad", 16, 4)
    asyncio.create_task(gp.run_servers())
    await asyncio.sleep(0.5)
    collected, ready = COLLECTED, asyncio.Event()
    task = asyncio.create_task(evdev_client(os.path.join(SOCK, "selkies_event1000.sock"), collected, ready))
    await asyncio.wait_for(ready.wait(), 5)
    # Give the server a beat to register the client writer.
    await asyncio.sleep(0.3)
    gp.ensure_uinput()
    for idx, value, is_button in SCRIPT:
        gp.send_event(idx, value, is_button)
        await asyncio.sleep(0.05)
    await asyncio.sleep(0.4)
    task.cancel()
    await gp.close()

asyncio.run(main())

# ---- assertions ----
fails: list[str] = []
def check(name: str, cond, detail: str = "") -> None:
    """Record and print one pass/fail result."""
    print(f"{'PASS' if cond else 'FAIL'}  {name}  {detail}")
    if not cond:
        fails.append(name)

log = open(SHIMLOG).read().splitlines()
uinput_events = unpack_stream(open(STREAM, "rb").read())

keybits = [int(l.split()[1], 16) for l in log if l.startswith("SET_KEYBIT")]
absbits = [int(l.split()[1], 16) for l in log if l.startswith("SET_ABSBIT")]
evbits = [int(l.split()[1], 16) for l in log if l.startswith("SET_EVBIT")]
abs_setup = {}
for line in log:
    if line.startswith("ABS_SETUP"):
        f = dict(p.split("=") for p in line.split()[1:])
        abs_setup[int(f["code"], 16)] = (int(f["min"]), int(f["max"]), int(f["fuzz"]), int(f["flat"]), int(f["res"]))
setup = next((l for l in log if l.startswith("DEV_SETUP")), "")

cfg = ih.STANDARD_XPAD_CONFIG
check("EV_KEY and EV_ABS advertised", sorted(evbits) == [ih.EV_KEY, ih.EV_ABS], str(evbits))
check("every button advertised", keybits == list(cfg["btn_map"]), f"{len(keybits)} codes")
check("every axis advertised", absbits == list(cfg["axes_map"]), f"{len(absbits)} codes")
check("device identity matches the interposer's",
      f"vendor=0x{cfg['vendor_id']:04x}" in setup and f"product=0x{cfg['product_id']:04x}" in setup
      and f"version=0x{cfg['version']:04x}" in setup and setup.endswith(cfg["name"]) and "bus=0x03" in setup,
      setup)
expected_abs = {code: (ih.ABS_HAT_MIN_VAL, ih.ABS_HAT_MAX_VAL, 0, 0, 0) if code in (ih.ABS_HAT0X, ih.ABS_HAT0Y)
                else (ih.ABS_MIN_VAL, ih.ABS_MAX_VAL, 16, 128, 1) for code in cfg["axes_map"]}
check("axis ranges match the interposer's EVIOCGABS", abs_setup == expected_abs,
      f"{len(abs_setup)} axes")
check("device created once, after setup", log.count("DEV_CREATE") == 1 and log.index("DEV_CREATE") > max(
      i for i, l in enumerate(log) if l.startswith(("SET_", "ABS_SETUP", "DEV_SETUP"))))
check("device destroyed on close", log.count("DEV_DESTROY") == 1)
check("sysname queried", any(l.startswith("GET_SYSNAME") for l in log))

socket_events = unpack_stream(b"".join(COLLECTED))
check("kernel stream equals the evdev interposer stream", uinput_events == socket_events,
      f"{len(uinput_events)} vs {len(socket_events)} events")

expected = [
    (ih.EV_KEY, ih.BTN_A, 1), (ih.EV_SYN, 0, 0),
    (ih.EV_KEY, ih.BTN_A, 0), (ih.EV_SYN, 0, 0),
    (ih.EV_ABS, ih.ABS_Z, 0), (ih.EV_SYN, 0, 0),
    (ih.EV_ABS, ih.ABS_HAT0Y, -1), (ih.EV_SYN, 0, 0),
    (ih.EV_ABS, ih.ABS_HAT0Y, 0), (ih.EV_SYN, 0, 0),
    (ih.EV_ABS, ih.ABS_X, -32767), (ih.EV_SYN, 0, 0),
    (ih.EV_ABS, ih.ABS_Y, 8191), (ih.EV_SYN, 0, 0),
    (ih.EV_KEY, ih.BTN_MODE, 1), (ih.EV_SYN, 0, 0),
    (ih.EV_KEY, ih.BTN_MODE, 0), (ih.EV_SYN, 0, 0),
]
check("event semantics", uinput_events == expected,
      "" if uinput_events == expected else f"got {uinput_events}")

# Node resolution against a synthetic sysfs tree.
with tempfile.TemporaryDirectory() as tmp:
    os.makedirs(os.path.join(tmp, "input999", "event42"))
    os.makedirs(os.path.join(tmp, "input999", "js3"))
    os.makedirs(os.path.join(tmp, "input999", "capabilities"))
    ih.UINPUT_SYSFS_BASE = tmp
    dev = ih.UInputGamepad("selkies_js0.sock")
    nodes = dev.create()
    dev.destroy()
    check("kernel nodes resolved from sysfs", nodes == ["/dev/input/event42", "/dev/input/js3"], str(nodes))

print("RESULT", "all passed" if not fails else f"FAILED: {fails}")
sys.exit(1 if fails else 0)
