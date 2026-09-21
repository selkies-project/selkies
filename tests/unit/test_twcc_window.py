#!/usr/bin/env python3
"""What a transport-cc decision is allowed to be measured over.

One feedback packet covers a few tens of RTP packets over a few tens of
milliseconds. Its loss fraction and its goodput are noise at that size: five
lost out of twenty-two reads as 22.7% loss, and a 17 ms receive span turns a
still second screen's trickle into tens of Mbps. The failure this pins down:
the congestion loop read whichever single window happened to land before its
tick and never aged it, so one such window backed a display off 30% every
second with no further feedback at all -- 40000 to 9604 kbps in four ticks --
and the CBR encoder starved to a quarter of its target macroblocked while it
climbed back.

A control interval's worth of feedback is the measurement, and an interval
that carried none steers nothing. A link that really is losing packets must
still be backed off exactly as before.
"""
import os
import struct
from types import SimpleNamespace
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.rtcdtlstransport import RTCDtlsTransport  # noqa: E402
from selkies.webrtc.rtp import pack_twcc_fci  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

PACKET_BYTES = 1200


def feedback(base_seq: int, received: int, lost: int, delta_us: int = 1000) -> bytes:
    """One transport-cc FCI: `received` acked, then `lost` missing, as run chunks."""
    fci = struct.pack("!HH", base_seq, received + lost)
    fci += bytes(3) + bytes(1)                              # reference time, feedback count
    if received:
        fci += struct.pack("!H", (1 << 13) | received)      # run of "received, small delta"
    if lost:
        fci += struct.pack("!H", lost)                      # run of "not received"
    return fci + bytes([delta_us // 250]) * received


def transport() -> RTCDtlsTransport:
    """A transport with only the send-side congestion state a feedback needs."""
    tr = object.__new__(RTCDtlsTransport)
    tr._twcc_seq = 0
    tr._twcc_history = {}
    tr._twcc_pruned_at = 0.0
    tr.twcc_estimate = None
    tr._pacer = None
    tr._twcc_window = RTCDtlsTransport._twcc_window_zero()
    return tr


def deliver(tr: RTCDtlsTransport, base_seq: int, received: int, lost: int) -> None:
    """Send `received + lost` packets, then feed back what the receiver saw."""
    for i in range(received + lost):
        tr._twcc_history[(base_seq + i) & 0xFFFF] = (PACKET_BYTES, 0.0)
    tr._twcc_process_feedback(feedback(base_seq, received, lost))


def main() -> int:
    res = H.Results("twcc-window")

    tr = transport()
    deliver(tr, 100, 17, 5)
    res.check("one window is too small to measure loss",
              round(tr.twcc_estimate["loss_fraction"], 3) == 0.227,
              tr.twcc_estimate["loss_fraction"])

    window = tr.take_twcc_window()
    res.check("the interval reports what the interval carried",
              window["received"] == 17 and window["lost"] == 5,
              window)
    res.check("draining leaves nothing to apply again",
              tr.take_twcc_window() is None)

    # A second of a still screen's feedback: 20 windows, one of them the one
    # above. Over the interval that is 5 packets in 440, not 5 in 22.
    tr = transport()
    for i in range(20):
        base = 1000 + i * 22
        deliver(tr, base, 17, 5) if i == 19 else deliver(tr, base, 22, 0)
    window = tr.take_twcc_window()
    res.check("a second of windows measures the second",
              window["received"] + window["lost"] == 440 and window["lost"] == 5,
              window)
    res.check("and reads far below the backoff threshold",
              window["loss_fraction"] < 0.02, window["loss_fraction"])
    res.check("its goodput spans the interval, not one window",
              window["goodput_bps"] > 0
              and window["bytes_acked"] == window["received"] * PACKET_BYTES,
              window)

    tr = transport()
    for i in range(20):
        deliver(tr, 5000 + i * 22, 16, 6)
    window = tr.take_twcc_window()
    res.check("a link that really is lossy still reads lossy",
              window["loss_fraction"] > 0.10, window["loss_fraction"])

    tr = transport()
    deliver(tr, 7000, 0, 22)
    window = tr.take_twcc_window()
    res.check("a fully lost interval reads as total loss with no goodput",
              window["loss_fraction"] == 1.0 and window["goodput_bps"] == 0,
              window)

    # The deltas chain arrival times from the feedback's reference time, so
    # where a train sits inside the 64 ms reference grid must not move its rate.
    def arrivals(tr: RTCDtlsTransport, times: list, fed: list) -> dict:
        for i in range(len(times)):
            tr._twcc_history[i] = (PACKET_BYTES, 0.0)
        tr._pacer = SimpleNamespace(set_goodput_bps=fed.append)
        tr._twcc_process_feedback(pack_twcc_fci(0, times, 0))
        return tr.twcc_estimate

    rates = set()
    for offset_ms in (0, 16, 32, 48, 63):
        rates.add(arrivals(transport(), [6400.0 + offset_ms + i for i in range(20)], [])["goodput_bps"])
    res.check("goodput is measured between the arrivals, wherever the reference time falls",
              rates == {9_600_000}, rates)
    fed: list = []
    estimate = arrivals(transport(), [100.0, 103.0, 101.0, 102.0], fed)
    res.check("reordered arrivals span from the earliest to the latest, without the earliest's bytes",
              round(estimate["recv_span_s"], 6) == 0.003 and estimate["goodput_bps"] == 9_600_000,
              estimate)
    res.check("a window the wire delivered whole sizes no brake", fed == [], fed)
    fed = []
    arrivals(transport(), [100.0 + i if i % 5 else None for i in range(20)], fed)
    res.check("a window the wire cut in several places does", len(fed) == 1 and fed[0] > 0, fed)
    fed = []
    arrivals(transport(), [100.0 + i if not 4 <= i < 12 else None for i in range(20)], fed)
    res.check("one run of loss, however long, is an outage and sizes no brake", fed == [], fed)
    tr = transport()
    for _ in range(3000):
        tr._twcc_next(PACKET_BYTES)
    res.check("a storm of sent packets waits in the history for its feedback, whatever its count",
              len(tr._twcc_history) == 3000, len(tr._twcc_history))
    tr._twcc_history = {seq: (PACKET_BYTES, at - 3.0) for seq, (_, at) in tr._twcc_history.items()}
    tr._twcc_pruned_at = 0.0
    tr._twcc_next(PACKET_BYTES)
    res.check("and is let go once older than the feedback could be", len(tr._twcc_history) == 1, len(tr._twcc_history))
    estimate = arrivals(transport(), [100.0 + i for i in range(10)] + [900.0 + i for i in range(10)], [])
    res.check("an outage inside a window is silence, not a rate: the stretches on either side measure",
              round(estimate["recv_span_s"], 6) == 0.018 and estimate["goodput_bps"] == 9_600_000, estimate)
    tr = transport()
    for i in range(20):
        tr._twcc_history[i] = (PACKET_BYTES, 0.0)
    for i in (5, 6, 7):
        tr._twcc_dropped(i)
    tr._twcc_process_feedback(pack_twcc_fci(0, [100.0 + i if i not in (5, 6, 7, 12) else None for i in range(20)], 0))
    res.check("packets the pacer dropped are not the wire's loss when the receiver reports them missing",
              tr.twcc_estimate["lost"] == 1 and round(tr.twcc_estimate["loss_fraction"], 3) == round(1 / 17, 3)
              and tr.take_twcc_window()["lost"] == 1, tr.twcc_estimate)
    fed = []
    estimate = arrivals(transport(), [100.0], fed)
    res.check("one arrival spans nothing: its bytes count, no rate is published and the pacer is not fed",
              estimate["bytes_acked"] == PACKET_BYTES and estimate["goodput_bps"] == 0 and fed == [],
              (estimate, fed))

    tr = transport()
    for i in range(20):
        tr._twcc_history[i] = (PACKET_BYTES, 0.0)
    fci = pack_twcc_fci(0, [6400.0 + i for i in range(20)], 0)
    tr._twcc_process_feedback(fci[:-8])
    res.check("a feedback whose deltas run short consumes no history and publishes nothing",
              len(tr._twcc_history) == 20 and tr.twcc_estimate is None, (len(tr._twcc_history), tr.twcc_estimate))
    tr._twcc_process_feedback(struct.pack("!HH", 0, 20) + bytes(4))
    res.check("one whose chunks run short is dropped the same way",
              len(tr._twcc_history) == 20 and tr.twcc_estimate is None, len(tr._twcc_history))
    tr._twcc_process_feedback(fci)
    res.check("the whole feedback then consumes exactly the packets it acknowledges",
              len(tr._twcc_history) == 0 and tr.twcc_estimate["received"] == 20, len(tr._twcc_history))
    tr = transport()
    for i in range(20):
        tr._twcc_history[i] = (PACKET_BYTES, 0.0)
    header = struct.pack("!HH", 0, 20) + bytes(4)
    tr._twcc_process_feedback(header + struct.pack("!H", (3 << 13) | 20))
    tr._twcc_process_feedback(header + struct.pack("!HH", 0xC000 | (3 << 12) | 0x555, (1 << 13) | 13) + bytes(19))
    res.check("the reserved status symbol, in a run or a vector chunk, is malformed feedback the same way",
              len(tr._twcc_history) == 20 and tr.twcc_estimate is None and tr.take_twcc_window() is None,
              (len(tr._twcc_history), tr.twcc_estimate))

    # A packet the receiver has no history for was reported before, and stretched
    # the interval its bytes never counted in.
    tr = transport()
    tr._twcc_history = {1: (PACKET_BYTES, 0.0), 2: (PACKET_BYTES, 0.0)}
    tr._twcc_process_feedback(pack_twcc_fci(0, [6400.0, 6405.0, 6406.0], 0))
    res.check("an arrival with no send history lies outside the interval",
              round(tr.twcc_estimate["recv_span_s"], 6) == 0.001 and tr.twcc_estimate["goodput_bps"] == 9_600_000,
              tr.twcc_estimate)
    # A browser that sees a late packet moves its window back over it and
    # reports the packets behind it again, arrival times and all.
    tr = transport()
    for i in range(30):
        tr._twcc_history[i] = (PACKET_BYTES, 0.0)
    tr._twcc_process_feedback(pack_twcc_fci(0, [100.0 + i if i != 15 else None for i in range(20)], 0))
    tr._twcc_process_feedback(pack_twcc_fci(15, [121.0] + [116.0 + i for i in range(14)], 1))
    res.check("a window moved back over a late packet measures only the packets reported for the first time",
              round(tr.twcc_estimate["recv_span_s"], 6) == 0.009 and tr.twcc_estimate["goodput_bps"] == 9_600_000
              and tr.twcc_estimate["bytes_acked"] == 10 * PACKET_BYTES and tr.twcc_estimate["lost"] == 0
              and len(tr._twcc_history) == 0, tr.twcc_estimate)

    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
