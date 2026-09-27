#!/usr/bin/env python3
"""Force feedback on the server: what a pad's mix of playing effects comes to,
and how a kernel pad's effect requests are answered.

Applications play rumble on a virtual pad the way they do on an Xbox pad the
kernel drives through its memless force-feedback layer: effects are uploaded,
then played for their length times a repeat count, or until stopped when the
length is 0, after an optional delay, and the motors take the sum of everything
playing, scaled by the gain and capped. The server mixes them per slot and
hands the result on as a level for each motor and how long to hold it; an
effect without an end is handed on for a lease at a time and renewed while it
plays, so a client that stops hearing stops shaking.

The kernel backend answers each upload and erase an application makes through
its device (UI_BEGIN/END_FF_UPLOAD, UI_BEGIN/END_FF_ERASE) and reads the plays
back as EV_FF events. Those are driven here over a socketpair in place of the
device, with the ioctls answered as the kernel would; the encodings and
layouts themselves are checked against the kernel headers in
test_uinput_abi.py. The interposer servers, which now read each connection
for its records, must still close while applications hold the pad open.
"""
import asyncio
import os
import shutil
import socket
import struct
import sys
import tempfile
from typing import Any, List

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
import selkies.input_handler as ih  # noqa: E402

results: List[tuple] = []


def check(label: str, ok: Any, detail: Any = "") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [gamepad-rumble] {label}  {detail}", flush=True)


def play(eid: int, strong: int, weak: int, length: int, delay: int = 0, count: int = 1) -> tuple:
    return (ih.FF_RECORD_PLAY, 0, eid, strong, weak, length, delay, count, 0)


def stop(eid: int) -> tuple:
    return (ih.FF_RECORD_STOP, 0, eid, 0, 0, 0, 0, 0, 0)


def gain(value: int) -> tuple:
    return (ih.FF_RECORD_GAIN, 0, 0, 0, 0, 0, 0, value, 0)


async def mixer() -> None:
    loop = asyncio.get_running_loop()
    gp = ih.SelkiesGamepad("/nonexistent/js0.sock", "/nonexistent/event1000.sock", loop)
    sent: List[tuple] = []
    gp.on_rumble = lambda s, w, ms: sent.append((round(loop.time(), 3), round(s, 3), round(w, 3), ms))

    t0 = loop.time()
    gp.ff_record("a", play(0, 0x8000, 0x4000, 100))
    check("a played effect reaches the motors at once, for its length",
          sent and sent[0][1:] == (0.5, 0.25, 100) and sent[0][0] - t0 < 0.01, sent)
    await asyncio.sleep(0.25)
    # A loop on a loaded machine wakes late, never early: the upper bounds allow for it.
    check("and stops when its length has run", len(sent) == 2 and sent[1][1:] == (0.0, 0.0, 0)
          and 0.09 <= sent[1][0] - t0 <= 0.2, sent)

    sent.clear()
    gp.ff_record("a", play(0, 0xC000, 0, 200))
    gp.ff_record("b", play(3, 0x8000, 0x2000, 200))
    check("effects playing together add up, capped at full",
          sent[-1][1:3] == (1.0, 0.125), sent)
    gp.ff_record("b", stop(3))
    check("stopping one leaves the other", sent[-1][1:3] == (0.75, 0.0), sent)
    gp.ff_record("a", play(0, 0x4000, 0x4000, 200))
    check("a replay of the same effect replaces it", sent[-1][1:3] == (0.25, 0.25), sent)
    gp.ff_record("a", gain(0x8000))
    check("the gain scales the mix", sent[-1][1:3] == (0.125, 0.125), sent)
    gp.ff_record("a", gain(0xffff))
    gp.ff_forget("a")
    check("a closed handle's effects stop", sent[-1][1:] == (0.0, 0.0, 0), sent)

    sent.clear()
    t0 = loop.time()
    gp.ff_record("a", play(1, 0x8000, 0x8000, 60, delay=80))
    check("a delayed effect waits out its delay", not sent, sent)
    await asyncio.sleep(0.18)
    check("then plays for its length", sent and sent[0][1:] == (0.5, 0.5, 60)
          and 0.075 <= sent[0][0] - t0 <= 0.17, sent)
    await asyncio.sleep(0.15)
    check("and stops", sent[-1][1:] == (0.0, 0.0, 0), sent)

    sent.clear()
    t0 = loop.time()
    gp.ff_record("a", play(2, 0x8000, 0, 50, count=3))
    check("a repeat count plays the length that many times", sent and sent[0][3] == 150, sent)
    await asyncio.sleep(0.3)
    check("and then stops", sent[-1][1:] == (0.0, 0.0, 0) and 0.14 <= sent[-1][0] - t0 <= 0.25, sent)

    sent.clear()
    t0 = loop.time()
    gp.ff_record("a", play(4, 0xffff, 0xffff, 0))
    check("an effect without an end is held for a lease", sent and sent[0][1:] == (1.0, 1.0, ih.FF_LEASE_MS),
          sent)
    await asyncio.sleep(ih.FF_LEASE_MS / 1000.0 * 0.5 + 0.1)
    check("renewed at half the lease while it plays",
          len(sent) == 2 and sent[1][1:] == (1.0, 1.0, ih.FF_LEASE_MS)
          and -0.01 < sent[1][0] - t0 - ih.FF_LEASE_MS / 2000.0 < 0.1, sent)
    n = len(sent)
    gp.ff_replay()
    check("a client taking the slot is handed the mix at once, not at the next renewal",
          len(sent) == n + 1 and sent[-1][1:] == (1.0, 1.0, ih.FF_LEASE_MS), sent[n:])
    gp.ff_record("a", stop(4))
    check("until stopped", sent[-1][1:] == (0.0, 0.0, 0), sent)
    n = len(sent)
    gp.ff_replay()
    check("and a quiet pad hands it nothing", len(sent) == n, sent[n:])
    gp.ff_record("a", play(5, 0x8000, 0, 65535))
    check("one longer than the lease is held for a lease too, the Gamepad API's limit being 5 s",
          sent[-1][1:] == (0.5, 0.0, ih.FF_LEASE_MS), sent)
    gp.ff_record("a", stop(5))
    await asyncio.sleep(ih.FF_LEASE_MS / 1000.0)
    check("and nothing is sent once everything is quiet", len(sent) == 6, sent)


class FakeKernel:
    """The uinput ioctls the kernel backend answers requests with, answered as
    the kernel does: a BEGIN fills in the request's effect (or effect id), an
    END records what the owner answered."""

    def __init__(self) -> None:
        self.effects: dict = {}
        self.erase_ids: dict = {}
        self.answers: List[tuple] = []

    def ioctl(self, fd: int, request: int, arg: Any = 0, mutate: bool = False) -> int:
        if request == ih.UI_BEGIN_FF_UPLOAD:
            request_id = struct.unpack_from("=I", arg, 0)[0]
            etype, eid, length, delay, union = self.effects[request_id]
            struct.pack_into(ih.FF_EFFECT_HEAD_FMT, arg, 8, etype, eid, 0, 0, 0, length, delay)
            arg[8 + 16:8 + 16 + len(union)] = union
        elif request == ih.UI_END_FF_UPLOAD:
            self.answers.append(("upload", struct.unpack_from("=Ii", arg, 0)))
        elif request == ih.UI_BEGIN_FF_ERASE:
            request_id = struct.unpack_from("=I", arg, 0)[0]
            struct.pack_into("=I", arg, 8, self.erase_ids[request_id])
        elif request == ih.UI_END_FF_ERASE:
            self.answers.append(("erase", struct.unpack_from("=Ii", arg, 0)))
        return 0


def kernel() -> None:
    kern = FakeKernel()
    saved = ih.fcntl.ioctl
    ih.fcntl.ioctl = kern.ioctl
    ours, app = socket.socketpair()
    ours.setblocking(False)
    dev = ih.UInputGamepad("rumble-test")
    dev.fd = ours.fileno()

    def events(*evs: tuple) -> None:
        app.sendall(b"".join(struct.pack(ih.LOCAL_EVDEV_EVENT_FMT, 0, 0, t, c, v) for t, c, v in evs))

    try:
        kern.effects[7] = (ih.FF_RUMBLE, 0, 500, 20, struct.pack("=HH", 0x8000, 0x4000))
        events((ih.EV_UINPUT, ih.UI_FF_UPLOAD, 7))
        out = dev.service_ff()
        check("an upload is answered with its request id and success",
              kern.answers == [("upload", (7, 0))] and not out, kern.answers)
        check("and the effect kept", dev.effects.get(0) == (0x8000, 0x4000, 500, 20), dev.effects)

        # A periodic effect plays as rumble of its magnitude on both motors.
        kern.effects[8] = (ih.FF_PERIODIC, 1, 300, 0, struct.pack("=HHh", ih.FF_SINE, 50, 0x3000))
        # And anything else is refused, as a memless pad refuses it.
        kern.effects[9] = (0x52, 2, 300, 0, struct.pack("=h", 0x3000))
        events((ih.EV_UINPUT, ih.UI_FF_UPLOAD, 8), (ih.EV_UINPUT, ih.UI_FF_UPLOAD, 9))
        dev.service_ff()
        check("a periodic effect is kept at twice its magnitude on both motors",
              dev.effects.get(1) == (0x6000, 0x6000, 300, 0), dev.effects)
        check("a constant-force effect is refused", ("upload", (9, -ih.errno.EINVAL)) in kern.answers
              and 2 not in dev.effects, kern.answers)

        events((ih.EV_FF, 0, 1), (ih.EV_FF, 1, 2), (ih.EV_FF, 0, 0), (ih.EV_FF, ih.FF_GAIN, 0x4000),
               (ih.EV_FF, 5, 1))
        out = dev.service_ff()
        check("plays, stops, and the gain come back as records, an unknown effect's play dropped",
              out == [play(0, 0x8000, 0x4000, 500, 20, 1), play(1, 0x6000, 0x6000, 300, 0, 2),
                      stop(0), gain(0x4000)], out)

        kern.erase_ids[10] = 1
        events((ih.EV_UINPUT, ih.UI_FF_ERASE, 10))
        out = dev.service_ff()
        check("an erase is answered, and stops the effect",
              kern.answers[-1] == ("erase", (10, 0)) and out == [stop(1)] and 1 not in dev.effects,
              f"{kern.answers[-1]} {out}")
        check("nothing left to read is no record", dev.service_ff() == [])
    finally:
        ih.fcntl.ioctl = saved
        dev.fd = None
        ours.close()
        app.close()


async def closing() -> None:
    """The servers close while applications still hold the pad open: each
    connection's handler waits in a read of it, which the close must end."""
    where = tempfile.mkdtemp(prefix="rumble-")
    gp = ih.SelkiesGamepad(os.path.join(where, "js0.sock"), os.path.join(where, "event1000.sock"),
                           asyncio.get_running_loop())
    gp.set_config("Test Pad", 16, 4)
    server = asyncio.ensure_future(gp.run_servers())
    await asyncio.sleep(0.3)
    held = []
    for path in (gp.js_sock_path, gp.evdev_sock_path):
        reader, writer = await asyncio.open_unix_connection(path)
        await reader.readexactly(ih.EXPECTED_C_STRUCT_SIZE)
        writer.write(struct.pack("=B", 8))
        await writer.drain()
        held.append(writer)
    await asyncio.sleep(0.2)
    try:
        await asyncio.wait_for(gp.close(), 3)
        check("the servers close while applications still hold the pad open", True)
    except asyncio.TimeoutError:
        check("the servers close while applications still hold the pad open", False, "still closing after 3 s")
    finally:
        server.cancel()
        for writer in held:
            writer.close()
        shutil.rmtree(where, ignore_errors=True)


asyncio.run(mixer())
kernel()
asyncio.run(closing())
failed = [r for r in results if not r[1]]
print(f"[gamepad-rumble] {len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
