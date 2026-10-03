#!/usr/bin/env python3
"""A queue that stands longer than the delay floor's window still reads as one.

One-way delay is read against the path's own, and the path's own was the least
delay of the last 30 s: a queue that stood through all of them became the
path, and the steer read nothing of it from then on while the loss it
overflowed into went on. The path's own delay now rises only as fast as two
clocks drift apart, so such a queue keeps reading, and a path that really grew
longer is told from a queue by what the stream's own rate does to it: delay
that holds through intervals sent under what the path delivered, with nothing
lost, is the path's.

Pinned here: a queue standing for minutes with loss keeps reading; a queue the
stream drains goes; a route change onto a longer path stops reading as a queue
within a few intervals of the rate cut that answers it, and within the old
window where nothing cuts the rate; clock drift never reads as a queue; a
still screen's trickle moves nothing but downward.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from types import SimpleNamespace  # noqa: E402

from selkies.webrtc import rtcdtlstransport as T  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

CLOCK = [1000.0]
T.time = SimpleNamespace(monotonic=lambda: CLOCK[0], time=lambda: CLOCK[0])


def transport() -> T.RTCDtlsTransport:
    return T.RTCDtlsTransport(SimpleNamespace(), [SimpleNamespace()])


def tick(tr, delay_ms: float, sent_kbps: float = 8000.0, delivered_kbps: float = 8000.0,
         loss: float = 0.0, feedbacks: int = 10):
    """One control interval a second long: every feedback's least delay at `delay_ms`."""
    CLOCK[0] += 1.0
    packets = 600
    lost = int(packets * loss)
    delay = delay_ms / 1000.0
    tr._twcc_window = {
        "received": packets - lost, "lost": lost,
        "bytes_acked": int(delivered_kbps * 125) + 1200,
        "first_arrival": (0, 0), "first_bytes": 1200, "last_arrival_us": 1_000_000,
        "delay_min": delay, "delay_last": delay, "feedback_mins": [delay] * feedbacks,
        "bytes_sent": int(sent_kbps * 125), "opened": CLOCK[0] - 1.0,
    }
    return tr.take_twcc_window()


def main() -> int:
    res = H.Results("twcc-standing-queue")
    base = 40.0

    tr = transport()
    for _ in range(3):
        tick(tr, base)
    readings = [tick(tr, base + 500.0, sent_kbps=8000, delivered_kbps=5000, loss=0.4)["queue_ms"]
                for _ in range(300)]
    res.check("a queue standing five minutes with loss reads as a queue throughout",
              min(readings) > 400.0, f"least {min(readings):.0f} ms")

    tr = transport()
    for _ in range(3):
        tick(tr, base)
    readings = [tick(tr, base + 200.0, sent_kbps=5000, delivered_kbps=5000)["queue_ms"] for _ in range(120)]
    res.check("a queue held full at the path's rate with nothing lost reads through the old window",
              min(readings[:28]) > 150.0, f"least {min(readings[:28]):.0f} ms in 28 s")

    tr = transport()
    for _ in range(3):
        tick(tr, base)
    first = tick(tr, base + 300.0, sent_kbps=8000, delivered_kbps=5000)["queue_ms"]
    drained = [tick(tr, base + d, sent_kbps=3000, delivered_kbps=5000)["queue_ms"] for d in (180.0, 60.0, 0.0, 0.0)]
    res.check("a queue the cut drains reads less each interval and then none",
              first > 250.0 and drained[0] > drained[1] > drained[2] and abs(drained[3]) < 1.0, f"{first:.0f} {drained}")
    again = tick(tr, base + 100.0, sent_kbps=8000, delivered_kbps=5000)["queue_ms"]
    res.check("and the next one reads against the same path", 90.0 < again < 110.0, f"{again:.0f} ms")

    tr = transport()
    for _ in range(3):
        tick(tr, base)
    route = base + 60.0
    seen = [tick(tr, route, sent_kbps=8000, delivered_kbps=8000)["queue_ms"]]
    for sent in (6500, 5300, 4300, 4300, 4900, 5600):
        seen.append(tick(tr, route, sent_kbps=sent, delivered_kbps=sent)["queue_ms"])
    res.check("a longer path reads as a queue until the rate has been under what it delivered for three intervals",
              all(q > 50.0 for q in seen[:3]) and all(abs(q) < 1.0 for q in seen[4:]),
              [round(q) for q in seen])

    tr = transport()
    for _ in range(3):
        tick(tr, base)
    seen = [tick(tr, route)["queue_ms"] for _ in range(40)]
    res.check("a longer path nothing cuts the rate on is taken for the path after the floor's window",
              seen[20] > 50.0 and abs(seen[-1]) < 1.0 and 28 <= next(i for i, q in enumerate(seen) if abs(q) < 1.0) <= 32,
              f"cleared at {next(i for i, q in enumerate(seen) if abs(q) < 1.0)} s")

    tr = transport()
    for _ in range(3):
        tick(tr, base)
    seen = [tick(tr, route, sent_kbps=8000, delivered_kbps=5000, loss=0.1)["queue_ms"] for _ in range(60)]
    res.check("delay that stands with loss is never taken for the path", min(seen) > 40.0, f"least {min(seen):.0f} ms")

    tr = transport()
    drift = [tick(tr, base + 0.1 * i)["queue_ms"] for i in range(600)]
    res.check("clocks drifting apart at 100 ppm for ten minutes read no queue",
              max(drift) < 1.0, f"most {max(drift):.2f} ms")
    back = [tick(tr, base + 60.0 - 0.1 * i)["queue_ms"] for i in range(600)]
    res.check("and drifting together reads none either", max(back) < 1.0, f"most {max(back):.2f} ms")

    tr = transport()
    for _ in range(3):
        tick(tr, base)
    for _ in range(40):
        thin = tick(tr, base + 45.0, sent_kbps=50, delivered_kbps=50, feedbacks=2)
    res.check("a still screen's trickle reads no queue", thin["queue_ms"] is None, thin["queue_ms"])
    after = tick(tr, base + 45.0, sent_kbps=8000, delivered_kbps=8000)["queue_ms"]
    res.check("and does not raise the path's delay by its jitter", 30.0 < after < 46.0, f"{after:.0f} ms")
    lower = tick(tr, base - 5.0, sent_kbps=50, delivered_kbps=50, feedbacks=2)
    below = tick(tr, base - 5.0)["queue_ms"]
    res.check("while a lower delay it shows lowers it", lower["queue_ms"] is None and abs(below) < 1.0, f"{below:.1f} ms")

    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
