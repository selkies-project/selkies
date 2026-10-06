#!/usr/bin/env python3
"""Kernel gamepad end-to-end: a real browser client with a synthetic Gamepad API
drives selkies over each transport; the /dev/uinput emulator records what the
kernel would receive. A `#player2` client repeats it on the sharing path, where
the slot comes from the connection rather than the message. A client with two
local pads, a flight stick whose pots jitter beside the pad in use, drives its
one slot with the pad it takes up, with and without a token that binds it to a
slot. A token holding slots 3 and 4 has the page's two pads drive one each."""
import json
import os
import struct
import sys
import time
import urllib.request
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright
sys.path.insert(0, H.SRC)
import selkies.input_handler as ih


PAD_INIT: str = """
window.__pad = {
  index: 0, id: "Selkies Test Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)",
  mapping: "standard", connected: true, timestamp: 1,
  buttons: Array.from({length: 17}, () => ({pressed: false, touched: false, value: 0})),
  axes: [0, 0, 0, 0],
};
navigator.getGamepads = () => [window.__pad, null, null, null];
window.__padPress = (i, v) => {
  window.__pad.buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
  window.__pad.timestamp = performance.now();
};
window.__padAxis = (i, v) => {
  window.__pad.axes[i] = v;
  window.__pad.timestamp = performance.now();
};
"""

TWO_PADS_INIT: str = """
(() => {
  const blank = (n) => Array.from({length: n}, () => ({pressed: false, touched: false, value: 0}));
  window.__pads = [
    {index: 0, id: "Logitech Extreme 3D (Vendor: 046d Product: c215)", mapping: "", connected: true,
     timestamp: 1, buttons: blank(12), axes: [0.07, -0.06, 0, 0, 0, 0, -1, 0, 0, 1.2857]},
    {index: 1, id: "Xbox Controller (STANDARD GAMEPAD Vendor: 045e Product: 0b13)", mapping: "standard",
     connected: true, timestamp: 1, buttons: blank(17), axes: [0, 0, 0, 0]},
  ];
  navigator.getGamepads = () => [window.__pads[0], window.__pads[1], null, null];
  let t = 0;
  setInterval(() => {
    window.__pads[0].axes[0] = (t++ % 2) ? 0.09 : 0.06;
    window.__pads[0].timestamp = performance.now();
  }, 4);
  window.__padPress = (p, i, v) => {
    window.__pads[p].buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
    window.__pads[p].timestamp = performance.now();
  };
  window.__padAxis = (p, i, v) => {
    window.__pads[p].axes[i] = v;
    window.__pads[p].timestamp = performance.now();
  };
})();
"""
SLOTS_PADS_INIT: str = """
(() => {
  const blank = (n) => Array.from({length: n}, () => ({pressed: false, touched: false, value: 0}));
  const named = (index, name) => ({index, id: name + " (STANDARD GAMEPAD Vendor: 045e Product: 0b13)",
    mapping: "standard", connected: true, timestamp: 1, buttons: blank(17), axes: [0, 0, 0, 0]});
  window.__pads = [named(0, "Pad A"), named(1, "Pad B")];
  navigator.getGamepads = () => [window.__pads[0], window.__pads[1], null, null];
  window.__padPress = (p, i, v) => {
    window.__pads[p].buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
    window.__pads[p].timestamp = performance.now();
  };
})();
"""
MASTER = "e2e-gamepad-master"
SLOT_TOKEN = "e2e-gamepad-slot1-Rk7"
SLOTS_TOKEN = "e2e-gamepad-slots34-Qm2"
# The stick's jitter, 0.06 to 0.09 of full scale on the kernel device's axis.
JITTER = range(1500, 3500)


def decode(path: str) -> list[tuple[int, int, int]]:
    """Decode a uinput-shim event stream into (type, code, value) tuples.

    Args:
        path: Path to the shim's binary event stream file.

    Returns:
        One `(ev_type, ev_code, ev_value)` tuple per 24-byte input_event
        record, timestamps stripped.
    """
    blob = open(path, "rb").read()
    return [struct.unpack("=qqHHi", blob[o:o + 24])[2:] for o in range(0, len(blob) - 23, 24)]

def launch(pw, mode: str, fragment: str = "", init: str = PAD_INIT):
    """Launch Chromium with the synthetic pad injected and open the stream page.

    Args:
        pw: Active Playwright instance.
        mode: Transport mode, ``websockets`` or ``webrtc``.
        fragment: Fragment to open the page with ("#player2", "#token=..."), or "".
        init: The synthetic Gamepad API to inject.

    Returns:
        Tuple of (browser, page, console-error list).
    """
    browser = C.chromium_launch(pw)
    ctx = browser.new_context(viewport={"width": 1280, "height": 720})
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(C.WIRE_TAP_JS)
    ctx.add_init_script(init)
    page = ctx.new_page()
    errors = []
    page.on("console", lambda m: errors.append(m.text) if m.type == "error" else None)
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(H.BASE_URL + "/" + fragment, wait_until="load")
    return browser, page, errors

def run(mode: str, results: "H.Results") -> None:
    """Drive pad input over one transport and verify the kernel-side record.

    Args:
        mode: Transport mode, ``websockets`` or ``webrtc``.
        results: Results accumulator shared across both transports.
    """
    shim_env, STREAM, SHIMLOG = H.uinput_shim_env(f"e2e-{mode}")
    H.server_start(mode=mode, extra_env=shim_env)
    try:
        with sync_playwright() as pw:
            browser, page, errors = launch(pw, mode)
            video = C.wait_wr_video(page) if mode == "webrtc" else C.wait_ws_video(page)
            results.check(f"{mode}: video flowing", bool(video), str(video))
            for action in ("__padPress(0, 1)", "__padPress(0, 0)",
                           "__padPress(12, 1)", "__padPress(12, 0)",
                           "__padAxis(0, -1)", "__padPress(6, 1)"):
                page.evaluate(f"window.{action}")
                time.sleep(0.25)
            time.sleep(0.5)
            real_errors, _ = C.benign_console(errors, [])
            results.check(f"{mode}: console clean", not real_errors, str(real_errors[:2]))
            browser.close()
    finally:
        H.server_stop()

    log = open(SHIMLOG).read()
    events = decode(STREAM)
    results.check(f"{mode}: kernel device created",
                  H.shim_created(SHIMLOG, ih.STANDARD_XPAD_CONFIG["name"]) == 1)
    results.check(f"{mode}: device is the standard pad",
                  "vendor=0x045e product=0x028e" in log and "Microsoft X-Box 360 pad" in log)
    for name, event in (("A press", (ih.EV_KEY, ih.BTN_A, 1)),
                        ("A release", (ih.EV_KEY, ih.BTN_A, 0)),
                        ("dpad up", (ih.EV_ABS, ih.ABS_HAT0Y, -1)),
                        ("dpad release", (ih.EV_ABS, ih.ABS_HAT0Y, 0)),
                        ("stick left", (ih.EV_ABS, ih.ABS_X, -32767)),
                        ("left trigger", (ih.EV_ABS, ih.ABS_Z, 32767))):
        results.check(f"{mode}: {name} reached the kernel device", event in events)
    print(f"    {mode} events:", [e for e in events if e[0] != 4])
    synced = all(events[i + 1] == (ih.EV_SYN, 0, 0)
                 for i in range(0, len(events) - 1, 2)) and len(events) % 2 == 0
    results.check(f"{mode}: every event is framed by SYN_REPORT", synced, f"{len(events)} events")

def run_player_slot(mode: str, results: "H.Results") -> None:
    """A `#player2` link drives player 2's pad and no other.

    The slot a client may drive is the one its own connection carries, which
    each transport learns differently: the websockets handshake reads it off the
    query, and the signaling HELLO carries it to the WebRTC gate. Driving a real
    pad through to the kernel device is what proves that path end to end.

    Args:
        mode: Transport mode, ``websockets`` or ``webrtc``.
        results: Results accumulator shared across both transports.
    """
    shim_env, STREAM, SHIMLOG = H.uinput_shim_env(f"e2e-player2-{mode}")
    H.server_start(mode=mode, extra_env=shim_env)
    try:
        with sync_playwright() as pw:
            browser, page, errors = launch(pw, mode, fragment="#player2")
            video = C.wait_wr_video(page) if mode == "webrtc" else C.wait_ws_video(page)
            results.check(f"{mode}: player-2 video flowing", bool(video), str(video))
            for action in ("__padPress(0, 1)", "__padPress(0, 0)"):
                page.evaluate(f"window.{action}")
                time.sleep(0.25)
            time.sleep(0.5)
            # A pad announced before the server has registered this link's slot
            # is dropped by the slot gate, which knows of no slot to allow yet,
            # and the button sends that follow still route by index -- so the
            # pad drives its slot with no association recorded. The client's own
            # repair is to announce again, which is what a re-attach does.
            if "virtual gamepad slot" not in H.server_log():
                page.evaluate("window.webrtcInput && window.webrtcInput.resyncGamepads"
                              " && window.webrtcInput.resyncGamepads()")
                for _ in range(20):
                    if "virtual gamepad slot" in H.server_log():
                        break
                    time.sleep(0.25)
            # What the page sent for its pad, so a missing association on the
            # server can be told from an announcement the client never made.
            sent = [m for m in page.evaluate("window.__wireSent || []")
                    if isinstance(m, str) and m.startswith("js,")]
            browser.close()
    finally:
        server_log = H.server_log()
        H.server_stop()

    events = decode(STREAM)
    results.check(f"{mode}: a player-2 link is given slot 1",
                  "virtual gamepad slot 1" in server_log
                  and "virtual gamepad slot 0" not in server_log,
                  f"client sent {sent[:3]}")
    results.check(f"{mode}: its pad reaches the kernel device",
                  (ih.EV_KEY, ih.BTN_A, 1) in events, f"{len(events)} events")
    results.check(f"{mode}: no other slot was driven",
                  H.shim_created(SHIMLOG, ih.STANDARD_XPAD_CONFIG["name"]) == 1)


def post_slot_token(token: str = SLOT_TOKEN, slot=1) -> int:
    """Provision a controller token bound to player slot 1, or to `slot`; returns the HTTP status."""
    req = urllib.request.Request(
        H.BASE_URL + "/api/tokens", method="POST",
        data=json.dumps({token: {"role": "controller", "slot": slot}}).encode(),
        headers={"Authorization": f"Bearer {MASTER}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as resp:
        return resp.status


def run_two_pads(mode: str, results: "H.Results", token: bool) -> None:
    """Two local pads drive the client's one slot with the pad taken up.

    A flight stick at the first index jitters past the stick deadzone, parks
    its throttle at an end and rests its hat off-center, beside a standard pad
    the user presses. The pad in use must drive the slot alone: its press and
    its held stick reach the kernel device, and nothing of the stick does. With
    `token`, the page holds a controller token bound to slot 1, so the server
    gives it the slot only with its verdict, after the page first saw its pads.

    Args:
        mode: Transport mode, ``websockets`` or ``webrtc``.
        results: Results accumulator shared across both transports.
        token: Whether the client holds a slot-bound token (secure mode).
    """
    label = f"{mode}{' with a slot token' if token else ''}"
    shim_env, STREAM, SHIMLOG = H.uinput_shim_env(f"e2e-two-{mode}-{int(token)}")
    extra = dict(shim_env, **({"SELKIES_MASTER_TOKEN": MASTER} if token else {}))
    H.server_start(mode=mode, extra_env=extra)
    try:
        if token:
            results.check(f"{label}: token provisioned", post_slot_token() == 200)
        with sync_playwright() as pw:
            browser, page, errors = launch(pw, mode, fragment=f"#token={SLOT_TOKEN}" if token else "",
                                           init=TWO_PADS_INIT)
            video = C.wait_wr_video(page) if mode == "webrtc" else C.wait_ws_video(page)
            results.check(f"{label}: video flowing", bool(video), str(video))
            time.sleep(1.0)
            for action in ("__padPress(1, 0, 1)", "__padPress(1, 0, 0)"):
                page.evaluate(f"window.{action}")
                time.sleep(0.25)
            mark = len(decode(STREAM))
            page.evaluate("window.__padAxis(1, 0, -1)")
            time.sleep(1.0)
            held = decode(STREAM)[mark:]
            page.evaluate("window.__padAxis(1, 0, 0)")
            time.sleep(0.5)
            browser.close()
    finally:
        server_log = H.server_log()
        H.server_stop()

    events = decode(STREAM)
    xs = [v for (t, c, v) in held if (t, c) == (ih.EV_ABS, ih.ABS_X)]
    results.check(f"{label}: the pad in use is the slot's controller",
                  "'Xbox Controller (STANDARD GAMEPAD Vendor: 045e Product: 0b13)' (17b, 4a) is now associated"
                  " with persistent virtual gamepad slot 0" in server_log)
    results.check(f"{label}: its press reached the kernel device",
                  (ih.EV_KEY, ih.BTN_A, 1) in events and (ih.EV_KEY, ih.BTN_A, 0) in events)
    results.check(f"{label}: its stick stayed where it was held", bool(xs) and all(v == -32767 for v in xs),
                  f"ABS_X while held: {xs[:6]}{'...' if len(xs) > 6 else ''} ({len(xs)})")
    results.check(f"{label}: nothing of the flight stick reached it",
                  not any(t == ih.EV_ABS and abs(v) in JITTER for (t, c, v) in events),
                  f"{sum(1 for (t, c, v) in events if t == ih.EV_ABS and abs(v) in JITTER)} jitter events")


def run_slot_list(mode: str, results: "H.Results") -> None:
    """A token holding slots 3 and 4: the page's pad 0 drives slot 3 and its pad 1 slot 4.

    The token table takes a list where it took a number, and the verdict hands
    the page that list; the page announces each of its pads on its own slot, in
    the browser's order, and the gate lets both through.

    Args:
        mode: Transport mode, ``websockets`` or ``webrtc``.
        results: Results accumulator shared across both transports.
    """
    label = f"{mode} with slots 3 and 4"
    shim_env, STREAM, SHIMLOG = H.uinput_shim_env(f"e2e-slots-{mode}")
    H.server_start(mode=mode, extra_env=dict(shim_env, SELKIES_MASTER_TOKEN=MASTER))
    try:
        results.check(f"{label}: token provisioned", post_slot_token(SLOTS_TOKEN, [3, 4]) == 200)
        with sync_playwright() as pw:
            browser, page, errors = launch(pw, mode, fragment=f"#token={SLOTS_TOKEN}", init=SLOTS_PADS_INIT)
            video = C.wait_wr_video(page) if mode == "webrtc" else C.wait_ws_video(page)
            results.check(f"{label}: video flowing", bool(video), str(video))
            time.sleep(1.0)
            for action in ("__padPress(0, 0, 1)", "__padPress(0, 0, 0)", "__padPress(1, 1, 1)", "__padPress(1, 1, 0)"):
                page.evaluate(f"window.{action}")
                time.sleep(0.25)
            browser.close()
    finally:
        server_log = H.server_log()
        H.server_stop()

    events = decode(STREAM)
    associated = lambda name, slot: (f"'{name} (STANDARD GAMEPAD Vendor: 045e Product: 0b13)' (17b, 4a) is now associated"
                                     f" with persistent virtual gamepad slot {slot}") in server_log
    results.check(f"{label}: pad 0 drives slot 3 and pad 1 slot 4",
                  associated("Pad A", 2) and associated("Pad B", 3))
    results.check(f"{label}: no other slot was driven",
                  not any(f"virtual gamepad slot {i}." in server_log for i in (0, 1)))
    results.check(f"{label}: both pads' presses reached the kernel",
                  (ih.EV_KEY, ih.BTN_A, 1) in events and (ih.EV_KEY, ih.BTN_B, 1) in events)


results = H.Results("uinput")
for mode in ("websockets", "webrtc"):
    run(mode, results)
    run_player_slot(mode, results)
    run_two_pads(mode, results, token=False)
    run_two_pads(mode, results, token=True)
    run_slot_list(mode, results)
sys.exit(0 if results.summary() else 1)
