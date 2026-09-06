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
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.rtcdtlstransport import RTCDtlsTransport  # noqa: E402

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

    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
