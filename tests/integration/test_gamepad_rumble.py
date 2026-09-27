#!/usr/bin/env python3
"""Rumble on both gamepad backends: what an application's force feedback comes
to on the server.

An application finds the virtual pad's evdev node reporting a memless pad's
force feedback (rumble, the periodic waveforms it plays as rumble, gain,
sixteen effects), uploads effects with EVIOCSFF, plays and stops them with
EV_FF writes, and erases them with EVIOCRMFF, exactly as on an Xbox pad the
kernel drives, and the pad's mix changes with each. Under the Input
Interposer each play, stop, and gain reaches the server as a record on the
handle's socket; on the kernel pad the server answers the kernel's upload and
erase requests and reads the plays back from the device. Closing the handle
stops what it played, as closing a kernel device's file does. A backend that
stops reading costs the application one wait of 100 ms, not one per play.

Two applications drive each backend: a raw one that makes every call itself
and checks the refusals, and SDL2 through ctypes (SDL_JoystickRumble, the call
most games make), where a libSDL2 is installed. The kernel block needs a
writable /dev/uinput, and is skipped, saying so, without one.

Usage: test_gamepad_rumble.py [interposer|kernel|all]
"""
import asyncio
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import selkies.input_handler as ih

INTERPOSER = os.path.join(H.REPO, "addons", "input-interposer", "input_interposer.c")

# The raw application: every force-feedback call a game makes on an evdev pad,
# timed on CLOCK_MONOTONIC, which the server's loop clock shares.
RAW_APP = r'''
import errno, fcntl, json, os, struct, sys, time

def ioc(direction, nr, size):
    return (direction << 30) | (size << 16) | (ord("E") << 8) | nr

EV_FF, FF_RUMBLE, FF_PERIODIC, FF_SINE, FF_GAIN = 0x15, 0x50, 0x51, 0x5a, 0x60
FF_SIZE = 16 + (32 if struct.calcsize("P") == 8 else 28)
EVIOCGBIT_FF = ioc(2, 0x20 + EV_FF, 16)
EVIOCSFF = ioc(1, 0x80, FF_SIZE)
EVIOCRMFF = ioc(1, 0x81, 4)
EVIOCGEFFECTS = ioc(2, 0x84, 4)
EVENT = "=qqHHi" if struct.calcsize("P") == 8 else "=llHHi"
out = {"marks": []}

def mark(name):
    out["marks"].append([name, time.monotonic()])

def effect(etype, eid, length, union):
    buf = bytearray(FF_SIZE)
    struct.pack_into("=HhHHHHH", buf, 0, etype, eid, 0, 0, 0, length, 0)
    buf[16:16 + len(union)] = union
    return buf

def upload(buf):
    try:
        fcntl.ioctl(fd, EVIOCSFF, buf, True)
    except OSError as e:
        return -e.errno
    return struct.unpack_from("=h", buf, 2)[0]

def write(code, value):
    os.write(fd, struct.pack(EVENT, 0, 0, EV_FF, code, value))

fd = os.open(sys.argv[1], os.O_RDWR | os.O_NONBLOCK)
bits = bytearray(16)
fcntl.ioctl(fd, EVIOCGBIT_FF, bits, True)
out["ff_bits"] = [b for b in range(128) if bits[b // 8] & (1 << (b % 8))]
n = bytearray(4)
fcntl.ioctl(fd, EVIOCGEFFECTS, n, True)
out["effects"] = struct.unpack("=i", n)[0]

rumble = upload(effect(FF_RUMBLE, -1, 1000, struct.pack("=HH", 0x8000, 0x4000)))
out["rumble_id"] = rumble
mark("play rumble"); write(rumble, 1); time.sleep(0.15)
sine = upload(effect(FF_PERIODIC, -1, 0, struct.pack("=HHh", FF_SINE, 20, 0x2000)))
out["sine_id"] = sine
mark("play sine"); write(sine, 1); time.sleep(0.15)
mark("gain half"); write(FF_GAIN, 0x8000); time.sleep(0.15)
mark("erase sine"); fcntl.ioctl(fd, EVIOCRMFF, sine); time.sleep(0.15)
mark("stop rumble"); write(rumble, 0); time.sleep(0.15)
write(FF_GAIN, 0xffff)
out["constant"] = upload(effect(0x52, -1, 100, struct.pack("=h", 0x1000)))
out["bad_id"] = upload(effect(FF_RUMBLE, 9, 100, struct.pack("=HH", 1, 1)))
ids = []
while len(ids) < 20:
    got = upload(effect(FF_RUMBLE, -1, 100, struct.pack("=HH", 1, 1)))
    if got < 0:
        out["exhausted"] = got
        break
    ids.append(got)
out["more_ids"] = len(ids)
out["short_write"] = None
try:
    os.write(fd, b"\0" * 8)
except OSError as e:
    out["short_write"] = -e.errno
mark("play again"); write(rumble, 1); time.sleep(0.15)
mark("close"); os.close(fd); time.sleep(0.15)
print(json.dumps(out), flush=True)
'''

# SDL2 through ctypes: open the first pad and rumble it for 250 ms, pumping
# SDL's own update, which stops the rumble when its duration has run.
SDL_APP = r'''
import ctypes, json, os, time
out = {"marks": []}
try:
    sdl = ctypes.CDLL(os.environ.get("SDL2_LIB") or "libSDL2-2.0.so.0")
except OSError as e:
    print(json.dumps({"unavailable": str(e)}), flush=True)
    raise SystemExit(0)
sdl.SDL_JoystickOpen.restype = ctypes.c_void_p
sdl.SDL_JoystickName.restype = ctypes.c_char_p
sdl.SDL_JoystickName.argtypes = [ctypes.c_void_p]
sdl.SDL_JoystickRumble.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_uint16, ctypes.c_uint32]
sdl.SDL_JoystickHasRumble.argtypes = [ctypes.c_void_p]
sdl.SDL_JoystickHasRumbleTriggers.argtypes = [ctypes.c_void_p]
sdl.SDL_JoystickClose.argtypes = [ctypes.c_void_p]
sdl.SDL_GetError.restype = ctypes.c_char_p
out["init"] = sdl.SDL_Init(0x200)
out["pads"] = sdl.SDL_NumJoysticks()
joy = sdl.SDL_JoystickOpen(0) if out["pads"] > 0 else None
if joy:
    out["name"] = sdl.SDL_JoystickName(joy).decode()
    out["has_rumble"] = sdl.SDL_JoystickHasRumble(joy)
    out["has_rumble_triggers"] = sdl.SDL_JoystickHasRumbleTriggers(joy)
    out["marks"].append(["rumble", time.monotonic()])
    out["rumble"] = sdl.SDL_JoystickRumble(joy, 0x8000, 0x4000, 250)
    out["error"] = sdl.SDL_GetError().decode()
    end = time.monotonic() + 0.6
    while time.monotonic() < end:
        sdl.SDL_JoystickUpdate()
        time.sleep(0.005)
    sdl.SDL_JoystickClose(joy)
sdl.SDL_Quit()
print(json.dumps(out), flush=True)
'''


# An application that plays one effect over and over on a backend that has
# stopped reading, until the socket's buffer is full and a write is held up,
# then 20 more, timing each; then, told the backend reads again, it plays a
# second effect.
DEAF_APP = r'''
import fcntl, json, os, struct, sys, time
def ioc(direction, nr, size):
    return (direction << 30) | (size << 16) | (ord("E") << 8) | nr
FF_SIZE = 16 + (32 if struct.calcsize("P") == 8 else 28)
EVENT = "=qqHHi" if struct.calcsize("P") == 8 else "=llHHi"
fd = os.open("/dev/input/event1000", os.O_RDWR | os.O_NONBLOCK)
def upload(strong):
    buf = bytearray(FF_SIZE)
    struct.pack_into("=HhHHHHH", buf, 0, 0x50, -1, 0, 0, 0, 100, 0)
    struct.pack_into("=HH", buf, 16, strong, 0)
    fcntl.ioctl(fd, ioc(1, 0x80, FF_SIZE), buf, True)
    return struct.unpack_from("=h", buf, 2)[0]
play = struct.pack(EVENT, 0, 0, 0x15, upload(0x1000), 1)
plays, held, after = 0, [], []
while plays < 4000000 and len(after) < 20:
    t = time.monotonic()
    os.write(fd, play)
    took = time.monotonic() - t
    plays += 1
    if held:
        after.append(took)
    elif took > 0.08:
        held.append(took)
print(json.dumps({"plays": plays, "held": held, "after": after}), flush=True)
sys.stdin.readline()
os.write(fd, struct.pack(EVENT, 0, 0, 0x15, upload(0x7777), 1))
os.close(fd)
'''


def deaf_backend(path: str, config: bytes, drain: threading.Event, got: list) -> None:
    """The backend end of one evdev handle that answers the handshake, then
    reads nothing until `drain` is set, and after it everything to the end."""
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(1)
    srv.settimeout(30)
    data = b""
    try:
        conn, _ = srv.accept()
        conn.settimeout(30)
        conn.sendall(config)
        conn.recv(1)
        drain.wait(60)
        while True:
            chunk = conn.recv(65536)
            if not chunk:
                break
            data += chunk
        conn.close()
    except OSError:
        pass
    finally:
        srv.close()
    got.extend(struct.iter_unpack(ih.FF_RECORD_FMT, data[:len(data) - len(data) % ih.FF_RECORD_SIZE]))


async def deaf(res: "H.Results", work: str, preload: str, env: dict, config: bytes) -> None:
    """A backend that stops reading holds an application's writes up once, not
    at every play, and hears it again once it reads."""
    where = os.path.join(work, "deaf")
    os.makedirs(where, exist_ok=True)
    drain, got = threading.Event(), []
    backend = threading.Thread(target=deaf_backend, daemon=True,
                               args=(os.path.join(where, "selkies_event1000.sock"), config, drain, got))
    backend.start()
    await asyncio.sleep(0.2)
    proc = await asyncio.create_subprocess_exec(
        sys.executable, "-c", DEAF_APP, "1000", env=dict(env, LD_PRELOAD=preload, SELKIES_JS_SOCKET_PATH=where),
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    out, err = None, b""
    try:
        line = await asyncio.wait_for(proc.stdout.readline(), timeout=120)
        if line.startswith(b"{"):
            out = json.loads(line)
            drain.set()
            await asyncio.sleep(0.3)
            proc.stdin.write(b"go\n")
            await proc.stdin.drain()
        _, err = await asyncio.wait_for(proc.communicate(), timeout=60)
    finally:
        drain.set()
        if proc.returncode is None:
            proc.kill()
    await asyncio.get_running_loop().run_in_executor(None, backend.join, 10)
    if out is None:
        res.check("interposer: the application ran against a backend that stopped reading", False,
                  err.decode()[-300:])
        return
    res.check("interposer: a backend that stops reading holds the application up once, not at every play",
              len(out["held"]) == 1 and len(out["after"]) == 20 and max(out["after"]) < 0.01,
              f"held {[round(t * 1000) for t in out['held']]} ms after {out['plays'] - 21} plays filled "
              f"the socket, the next 20 took at most {max(out['after'] or [0]) * 1000:.1f} ms")
    res.check("interposer: and it is heard again once the backend reads",
              got and got[-1][0] == ih.FF_RECORD_PLAY and got[-1][3] == 0x7777, got[-1:] if got else got)


def build_interposer(workdir: str) -> str:
    so = os.path.join(workdir, "selkies_input_interposer.so")
    subprocess.run(["gcc", "-O2", "-shared", "-fPIC", "-o", so, INTERPOSER, "-ldl", "-pthread"],
                   check=True, capture_output=True, text=True)
    return so


async def run_app(source: str, env: dict, *args: str) -> dict:
    proc = await asyncio.create_subprocess_exec(
        sys.executable, "-c", source, *args, env=env,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    out, err = await asyncio.wait_for(proc.communicate(), timeout=60)
    lines = [ln for ln in out.decode().splitlines() if ln.startswith("{")]
    if not lines:
        return {"crashed": err.decode()[-400:]}
    return json.loads(lines[-1])


def mix_after(sent: list, t: float) -> tuple:
    """The first mix the server handed on at or after `t`, as (strong, weak, ms)."""
    hit = next((s for s in sent if s[0] >= t), None)
    return (hit[1], hit[2], hit[3]) if hit else None


async def drive(res: "H.Results", label: str, node: str, env: dict, sent: list) -> None:
    """Both applications against one backend's node, and what the mix did."""
    out = await run_app(RAW_APP, env, node)
    if "crashed" in out:
        res.check(f"{label}: the application ran", False, out["crashed"])
        return
    marks = dict(out["marks"])
    res.check(f"{label}: EV_FF reports rumble, periodic, its waveforms, and gain",
              out["ff_bits"] == sorted(ih.FF_MEMLESS_BITS), out["ff_bits"])
    res.check(f"{label}: sixteen effects at once", out["effects"] == ih.FF_EFFECTS_MAX, out["effects"])
    res.check(f"{label}: a new effect takes the first free id", out["rumble_id"] == 0 and out["sine_id"] == 1,
              f"{out['rumble_id']} {out['sine_id']}")
    got = mix_after(sent, marks["play rumble"])
    res.check(f"{label}: a rumble play reaches the mix as its two magnitudes, for its length",
              got and got[:2] == (0.5, 0.25) and 950 <= got[2] <= 1000, got)
    lag = next((s[0] for s in sent if s[0] >= marks["play rumble"]), None)
    res.check(f"{label}: within a few milliseconds of the write",
              lag is not None and lag - marks["play rumble"] < 0.02,
              f"{(lag - marks['play rumble']) * 1000:.2f} ms" if lag else "never")
    got = mix_after(sent, marks["play sine"])
    res.check(f"{label}: a periodic effect adds twice its magnitude to both motors",
              got and got[:2] == (0.75, 0.5), got)
    got = mix_after(sent, marks["gain half"])
    res.check(f"{label}: the gain halves the mix", got and got[:2] == (0.375, 0.25), got)
    got = mix_after(sent, marks["erase sine"])
    res.check(f"{label}: erasing an effect stops it", got and got[:2] == (0.25, 0.125), got)
    got = mix_after(sent, marks["stop rumble"])
    res.check(f"{label}: a play with a count of 0 stops the rest", got == (0.0, 0.0, 0), got)
    res.check(f"{label}: a constant-force effect is refused", out["constant"] == -22, out["constant"])
    res.check(f"{label}: an id the handle never got is refused", out["bad_id"] == -22, out["bad_id"])
    res.check(f"{label}: the seventeenth effect is refused for want of room",
              out.get("exhausted") == -28 and out["more_ids"] == 15, out)
    res.check(f"{label}: a write shorter than an event is refused", out["short_write"] == -22,
              out["short_write"])
    got = mix_after(sent, marks["close"])
    again = mix_after(sent, marks["play again"])
    res.check(f"{label}: closing the handle stops what it played",
              again and again[:2] == (0.5, 0.25) and got == (0.0, 0.0, 0), f"{again} {got}")

    sent.clear()
    sdl_env = dict(env, SDL_JOYSTICK_DISABLE_UDEV="1", SDL_JOYSTICK_HIDAPI="0")
    out = await run_app(SDL_APP, sdl_env)
    if "unavailable" in out:
        res.skip(f"{label}: sdl-rumble", f"no libSDL2 to load: {out['unavailable']}")
    elif "crashed" in out or not out.get("pads"):
        res.check(f"{label}: SDL2 finds the pad", False, out)
    else:
        t = dict(out["marks"])["rumble"]
        res.check(f"{label}: SDL2 finds the pad and says it can rumble",
                  out["has_rumble"] == 1 and out["rumble"] == 0, out)
        # evdev carries no trigger rumble, so a game has none to send a client
        # whose pad (an Xbox One's) could play one.
        res.check(f"{label}: and no trigger rumble, which no Linux pad takes",
                  out["has_rumble_triggers"] == 0, out.get("has_rumble_triggers"))
        got = mix_after(sent, t)
        res.check(f"{label}: SDL_JoystickRumble reaches the mix at its two levels, held for a lease",
                  got == (0.5, 0.25, ih.FF_LEASE_MS), f"{got} {out}")
        stop = next((s for s in sent if s[0] > t + 0.1 and s[1:3] == (0.0, 0.0)), None)
        res.check(f"{label}: and SDL's own stop when its 250 ms have run lands too",
                  stop is not None and 0.2 <= stop[0] - t <= 0.45,
                  f"{round((stop[0] - t) * 1000) if stop else None} ms, {sent}")


async def main(which: str) -> "H.Results":
    res = H.Results("gamepad-rumble")
    work = tempfile.mkdtemp(prefix="rumble-", dir=H.WORKDIR if os.path.isdir(H.WORKDIR) else None)
    loop = asyncio.get_running_loop()
    sent: list = []
    env = {k: v for k, v in os.environ.items() if k not in ("LD_PRELOAD",)}

    if which in ("interposer", "all"):
        preload = build_interposer(work)
        gp = ih.SelkiesGamepad(os.path.join(work, "selkies_js0.sock"),
                               os.path.join(work, "selkies_event1000.sock"), loop)
        gp.set_config(ih.STANDARD_XPAD_CONFIG["name"], len(ih.STANDARD_XPAD_CONFIG["btn_map"]),
                      len(ih.STANDARD_XPAD_CONFIG["axes_map"]))
        gp.on_rumble = lambda s, w, ms: sent.append((loop.time(), round(s, 3), round(w, 3), ms))
        server = asyncio.ensure_future(gp.run_servers())
        await asyncio.sleep(0.5)
        try:
            await drive(res, "interposer", "/dev/input/event1000",
                        dict(env, LD_PRELOAD=preload, SELKIES_JS_SOCKET_PATH=work), sent)
            await deaf(res, work, preload, env, gp.config_payload_cache)
        finally:
            await gp.close()
            server.cancel()

    if which in ("kernel", "all"):
        sent.clear()
        if not ih.uinput_writable():
            res.skip("kernel", f"{ih.UINPUT_PATH} is missing or not writable here")
        else:
            gp = ih.SelkiesGamepad(os.path.join(work, "k_js0.sock"), os.path.join(work, "k_event.sock"),
                                   loop, uinput_enabled=True)
            gp.set_config(ih.STANDARD_XPAD_CONFIG["name"], len(ih.STANDARD_XPAD_CONFIG["btn_map"]),
                          len(ih.STANDARD_XPAD_CONFIG["axes_map"]))
            gp.on_rumble = lambda s, w, ms: sent.append((loop.time(), round(s, 3), round(w, 3), ms))
            gp.running = True
            gp.ensure_uinput()
            nodes = [n for n in (gp.uinput.device_nodes if gp.uinput else []) if "event" in n]
            try:
                if not nodes:
                    res.check("kernel: the pad's event node exists", False, gp.uinput and gp.uinput.device_nodes)
                else:
                    await asyncio.sleep(0.5)
                    await drive(res, "kernel", nodes[0], env, sent)
            finally:
                await gp.close()
    return res


if __name__ == "__main__":
    r = asyncio.run(main(sys.argv[1] if len(sys.argv) > 1 else "all"))
    sys.exit(0 if r.summary() else 1)
