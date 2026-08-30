#!/usr/bin/env python3
"""The X11 selection monitor's read discipline under INCR.

Anything a toolkit copies above about 64 KiB -- every real screenshot -- comes
back over INCR, one property at a time, at whatever pace the owning
application manages. Three things have to hold for that content to reach a
client intact: a transfer that keeps arriving is never abandoned for taking
longer than a fixed clock, a transfer that stops is discarded rather than
handed on as the truncation it is, and a reply that arrives after its caller
gave up never answers the next conversion -- which is how image bytes get
published as clipboard text.

Runs against its own X server, since it drives selection ownership.

Usage: python3 tests/integration/test_clipboard_incr.py
"""
import os
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))) + "/src")
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

# Under the classic request-size limit, and what a toolkit's own INCR loop uses.
CHUNK = 60000


def incr_owner(display_name: str, payload: bytes, image_mime: str,
               delay: float = 0.0, stall_after: int = -1, text: bytes = None,
               first_delay: float = 0.0, second: tuple = None) -> tuple:
    """Own CLIPBOARD and serve `payload` over INCR, one chunk per property delete.

    Args:
        display_name: X display to own the selection on.
        payload: Bytes served under `image_mime`.
        image_mime: Target name the payload is offered as.
        delay: Seconds to wait before each chunk, i.e. how slow the owner is.
        stall_after: Chunk count after which the owner stops answering; -1
            never stalls.
        text: Offered under UTF8_STRING as well when set, so a caller can see
            which target a reply is attributed to.
        first_delay: Seconds the owner spends before answering the request at
            all, as one that re-encodes the image per request does.
        second: `(mime, bytes)` served whole under a second image target,
            offered after the first, so a reply misattributed across targets
            is visible in the bytes that come back.

    Returns:
        `(display, state)`; setting `state["flag"]` ends the serving thread and
        `state["sent"]` counts the chunks handed over.
    """
    from selkies.Xlib import display as xdisp, X
    from selkies.Xlib.protocol import event as xevent
    d = xdisp.Display(display_name)
    scr = d.screen()
    win = scr.root.create_window(0, 0, 1, 1, 0, scr.root_depth, window_class=X.InputOutput)
    clip = d.get_atom("CLIPBOARD")
    targets = d.get_atom("TARGETS")
    incr = d.get_atom("INCR")
    image = d.get_atom(image_mime)
    second_atom = d.get_atom(second[0]) if second else None
    utf8 = d.get_atom("UTF8_STRING")
    win.set_selection_owner(clip, X.CurrentTime)
    d.flush()
    state = {"flag": False, "sent": 0}

    def answer(ev, prop):
        ev.requestor.send_event(xevent.SelectionNotify(
            time=ev.time, requestor=ev.requestor, selection=ev.selection,
            target=ev.target, property=prop), propagate=False)

    def serve():
        transfer = None
        try:
            deadline = time.monotonic() + 120.0
            while not state["flag"] and time.monotonic() < deadline:
                if not d.pending_events():
                    time.sleep(0.005)
                    continue
                ev = d.next_event()
                if isinstance(ev, xevent.SelectionRequest):
                    if ev.target == targets:
                        offered = [targets, image]
                        if second_atom is not None:
                            offered.append(second_atom)
                        if text is not None:
                            offered.append(utf8)
                        ev.requestor.change_property(ev.property, targets, 32, offered)
                        answer(ev, ev.property)
                    elif second_atom is not None and ev.target == second_atom:
                        ev.requestor.change_property(ev.property, second_atom, 8, second[1])
                        answer(ev, ev.property)
                    elif text is not None and ev.target == utf8:
                        ev.requestor.change_property(ev.property, utf8, 8, text)
                        answer(ev, ev.property)
                    elif ev.target == image:
                        if first_delay:
                            time.sleep(first_delay)
                        ev.requestor.change_attributes(event_mask=X.PropertyChangeMask)
                        ev.requestor.change_property(ev.property, incr, 32, [len(payload)])
                        answer(ev, ev.property)
                        transfer = [ev.requestor, ev.property, 0]
                    else:
                        answer(ev, X.NONE)
                    d.flush()
                elif ev.type == X.PropertyNotify and ev.state == X.PropertyDelete and transfer:
                    if 0 <= stall_after <= state["sent"]:
                        continue
                    if delay:
                        time.sleep(delay)
                    requestor, prop, offset = transfer
                    piece = payload[offset:offset + CHUNK]
                    requestor.change_property(prop, image, 8, piece)
                    d.flush()
                    transfer[2] = offset + len(piece)
                    state["sent"] += 1
                    if not piece:
                        transfer = None
        finally:
            try:
                d.close()
            except Exception:
                pass

    threading.Thread(target=serve, daemon=True).start()
    return d, state


def run() -> "H.Results":
    res = H.Results("clipboard-incr")
    proc, display = H.private_x_server(640, 480)
    owners = []
    try:
        import selkies.input_handler as ih

        monitor = ih._X11ClipboardMonitor(display)
        # Distinct in every chunk, so a truncation cannot pass as the whole.
        payload = bytes(range(256)) * 8192

        def read_with(**kw):
            d, state = incr_owner(display, payload, "image/png", **kw)
            owners.append((d, state))
            time.sleep(0.3)
            started = time.monotonic()
            data, mime = monitor.read(use_binary=True)
            elapsed = time.monotonic() - started
            state["flag"] = True
            time.sleep(0.2)
            return data, mime, elapsed

        data, mime, elapsed = read_with()
        res.check("an INCR image reads back whole",
                  data == payload and mime == "image/png",
                  f"{mime} {len(data) if data else 0} bytes in {elapsed:.2f}s")

        # Slower per chunk than the idle bound, and far longer overall: the
        # read follows the transfer rather than a clock started at its request.
        data, mime, elapsed = read_with(delay=0.2)
        res.check("a slow owner's image is not abandoned part-way",
                  data == payload and mime == "image/png",
                  f"{mime} {len(data) if data else 0} bytes in {elapsed:.2f}s")

        # An owner that goes quiet mid-transfer has delivered a fragment; half
        # an image is not content, and offering it as one is worse than failing.
        data, mime, elapsed = read_with(stall_after=3)
        res.check("a stalled transfer fails rather than truncating",
                  data is None, f"{mime} {len(data) if data else 0} bytes")

        # A timed-out conversion's reply is still in flight when the next one
        # goes out; answering that one with it publishes an image as text.
        d, state = incr_owner(display, payload, "image/png", delay=0.2,
                              text=b"THE ADVERTISED TEXT")
        owners.append((d, state))
        time.sleep(0.3)
        data, mime = monitor.read(use_binary=True)
        state["flag"] = True
        res.check("a reply never answers the conversion after it",
                  (mime == "image/png" and data == payload)
                  or (mime == "text/plain" and data == "THE ADVERTISED TEXT"),
                  f"{mime} {len(data) if data else 0} bytes")

        # An owner holding the image decoded re-encodes it per request and
        # says nothing meanwhile. Cutting that wait short reads whatever the
        # next target offers instead of the image that was copied.
        data, mime, elapsed = read_with(first_delay=monitor._READ_TIMEOUT_S + 3)
        res.check("an owner that answers only after seconds is still read",
                  data == payload and mime == "image/png",
                  f"{mime} {len(data) if data else 0} bytes in {elapsed:.2f}s")

        # Once the reader has given up, the answer it stopped waiting for is
        # still coming; it must not be handed over as the next target's.
        decoy = b"NOT THE IMAGE" * 977
        d, state = incr_owner(display, payload, "image/png", first_delay=2.0,
                              second=("image/jpeg", decoy))
        owners.append((d, state))
        time.sleep(0.3)
        monitor._FIRST_REPLY_TIMEOUT_S = 0.5
        try:
            data, mime = monitor.read(use_binary=True)
        finally:
            del monitor._FIRST_REPLY_TIMEOUT_S
        state["flag"] = True
        res.check("a late answer is not attributed to the next target",
                  mime != "image/jpeg" or data == decoy,
                  f"{mime} {len(data) if data else 0} bytes")

        # Waiting on a slow owner must not hold the content a client pasted:
        # the read is of the selection that paste replaces.
        d, state = incr_owner(display, payload, "image/png", first_delay=20.0)
        owners.append((d, state))
        time.sleep(0.3)
        threading.Thread(target=monitor.read, args=(True,), daemon=True).start()
        time.sleep(0.5)
        started = time.monotonic()
        took = monitor.offer(b"what the client pasted", "text/plain")
        elapsed = time.monotonic() - started
        state["flag"] = True
        res.check("a write does not wait out a slow read",
                  took and elapsed < 5.0, f"offer took {elapsed:.2f}s")

        # A paste landing while an owner is mid-stream -- chunks still coming --
        # is served between chunks rather than waiting the transfer out.
        d, state = incr_owner(display, payload, "image/png", delay=0.4)
        owners.append((d, state))
        time.sleep(0.3)
        threading.Thread(target=monitor.read, args=(True,), daemon=True).start()
        time.sleep(0.6)
        started = time.monotonic()
        took = monitor.offer(b"pasted mid-stream", "text/plain")
        elapsed = time.monotonic() - started
        state["flag"] = True
        res.check("a paste is served between a streaming owner's chunks",
                  took and elapsed < 2.0, f"offer took {elapsed:.2f}s, ok={took}")
        res.check("the mid-stream paste took the selection",
                  monitor.owns_selection(), str(monitor.owns_selection()))
        monitor.close()
    finally:
        for _d, state in owners:
            state["flag"] = True
        H.stop_x_server(proc, display)
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
