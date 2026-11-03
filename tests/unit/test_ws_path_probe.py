#!/usr/bin/env python3
"""The still-screen path probe over WebSockets (`_probe_path`, `PathProbe`): once a
queue has cut a display's target, a page that announced `_pad,1` whose screen then
sends a trickle is padded by its relay up to the target, past the steer's hold, so
the steer climbs on what the path carries rather than holding the cut rate; a
queue on a padded tick stops the padding and backs the target off, and nothing pads
again for `PathProbe.QUIET_S`. A screen that sends nothing at all, with nothing in
flight, is padded too. A page that never announced `_pad,1` is never padded, and the
steer then climbs on the trickle no further than the capacity the path last showed,
as before.
"""
import logging
import os
import sys
from collections import deque

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.stream_server import PathProbe  # noqa: E402
import selkies.websockets_mode as wm  # noqa: E402
from selkies.websockets_mode import DataStreamingServer, _note_round_trip  # noqa: E402

# The rate a page keeps (`CC_RATE`) is broadcast from a task; no loop runs here.
wm._spawn_background_task = lambda coro, **kw: coro.close()

logging.disable(logging.INFO)
passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ws-path-probe] {label}  {detail}", flush=True)


class Module:
    def __init__(self):
        self.rates = []

    def update_video_bitrate(self, kbps):
        self.rates.append(kbps)

    def update_link_rate(self, kbps):
        pass


class Socket:
    """A page's socket, hashable as aiohttp's is."""


class Relay:
    def __init__(self):
        self.pad_bps, self.pad_until, self.calls = 0.0, 0.0, []

    def set_padding(self, bps, until):
        self.pad_bps, self.pad_until = bps, until
        self.calls.append(bps)


def setup(pad: bool):
    server = DataStreamingServer.__new__(DataStreamingServer)
    module, relay = Module(), Relay()
    ws = Socket()
    if pad:
        ws._ws_pad = True
    server.capture_instances = {"display": {"module": module}}
    server._initial_video_bitrate = 4000
    server.clients = set()
    server.video_relay_groups = {"display": {ws: relay}}
    state = {"video_bitrate": 4000, "backpressure_enabled": True, "sent_bytes": 0, "acked_bytes": 0,
             "rtt_samples": deque(maxlen=20), "ws": ws}
    return server, module, relay, state


def run(server, relay, state, rtt_ms, start, seconds, sent_kbps=None, path_kbps=None):
    """Every 50 ms: the stream sends `sent_kbps` (by default the rate in force), the relay
    the padding it was asked for, and the path delivers it all (or `path_kbps`) acked
    at `rtt_ms`; the steer runs every half second."""
    t, step, n = start, 0.05, 0
    while t < start + seconds - 1e-9:
        t = round(t + step, 3)
        n += 1
        sending = sent_kbps if sent_kbps is not None else server._video_bitrate_kbps(state)
        pad = relay.pad_bps / 1000 if t < relay.pad_until else 0.0
        state["sent_bytes"] += (sending + pad) * 125 * step
        state["pad_bytes"] = state.get("pad_bytes", 0) + pad * 125 * step
        delivered = min(sending + pad, path_kbps) if path_kbps else sending + pad
        state["acked_bytes"] += delivered * 125 * step
        if sending + pad > 0:
            _note_round_trip(state, rtt_ms, state["acked_bytes"], t)
        if abs((t * 2) - round(t * 2)) < 1e-6:
            server._steer_bitrate_to_link("display", state, t)
    return t


server, module, relay, state = setup(pad=True)
t = run(server, relay, state, 20.0, 0.0, 5, path_kbps=3000.0)
t = run(server, relay, state, 320.0, t, 0.5, path_kbps=3000.0)
cut = server._video_bitrate_kbps(state)
check("a queue cuts the target", cut < 3000, cut)
check("and nothing pads while the steer holds it", relay.calls and max(relay.calls) == 0.0, relay.calls[-3:])
t = run(server, relay, state, 20.0, t, 2, sent_kbps=100.0)
check("past the hold, a still screen's trickle is padded up to the target",
      relay.pad_bps > 0 and abs(relay.pad_bps + 100_000 - server._video_bitrate_kbps(state) * 1000) < 50_000,
      (relay.pad_bps, server._video_bitrate_kbps(state)))
t = run(server, relay, state, 20.0, t, 1, sent_kbps=100.0)
check("and the target climbs on what the padded path carries", server._video_bitrate_kbps(state) > cut,
      (cut, server._video_bitrate_kbps(state)))
t = run(server, relay, state, 20.0, t, 12, sent_kbps=100.0)
check("back to the ceiling", server._video_bitrate_kbps(state) == 4000, server._video_bitrate_kbps(state))
check("where the padding stops", relay.pad_bps == 0.0, relay.pad_bps)

server, module, relay, state = setup(pad=True)
t = run(server, relay, state, 20.0, 0.0, 5, path_kbps=2500.0)
t = run(server, relay, state, 320.0, t, 0.5, path_kbps=2500.0)
t = run(server, relay, state, 20.0, t, 3, sent_kbps=100.0, path_kbps=2500.0)
before = server._video_bitrate_kbps(state)
check("a path slower than the ceiling is padded too", relay.pad_bps > 0, relay.pad_bps)
t = run(server, relay, state, 200.0, t, 1, sent_kbps=100.0, path_kbps=2500.0)
check("its queue on a padded tick stops the padding", relay.pad_bps == 0.0, relay.pad_bps)
check("and backs the target off", server._video_bitrate_kbps(state) < before, (before, server._video_bitrate_kbps(state)))
quiet_from = t
t = run(server, relay, state, 20.0, t, PathProbe.QUIET_S - 2, sent_kbps=100.0, path_kbps=2500.0)
check("nothing pads again for QUIET_S", relay.pad_bps == 0.0 and t - quiet_from < PathProbe.QUIET_S, relay.pad_bps)
t = run(server, relay, state, 20.0, t, 6, sent_kbps=100.0, path_kbps=2500.0)
check("after which a probe pads again", any(c > 0 for c in relay.calls[-8:]), relay.calls[-8:])

server, module, relay, state = setup(pad=True)
t = run(server, relay, state, 20.0, 0.0, 5, path_kbps=3000.0)
t = run(server, relay, state, 320.0, t, 0.5, path_kbps=3000.0)
t = run(server, relay, state, 20.0, t, 2, sent_kbps=0.0)
check("a screen that sends nothing, with nothing in flight, is padded too", relay.pad_bps > 0, relay.pad_bps)

server, module, relay, state = setup(pad=False)
t = run(server, relay, state, 20.0, 0.0, 5, path_kbps=3000.0)
t = run(server, relay, state, 320.0, t, 0.5, path_kbps=3000.0)
cut = server._video_bitrate_kbps(state)
t = run(server, relay, state, 20.0, t, 10, sent_kbps=100.0)
check("a page that never announced _pad,1 is never padded", relay.calls == [], relay.calls)
check("and its target climbs on the trickle no further than the capacity the path last showed",
      cut < server._video_bitrate_kbps(state) <= 3000, (cut, server._video_bitrate_kbps(state)))

print(f"[ws-path-probe] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
