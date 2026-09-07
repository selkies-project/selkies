#!/usr/bin/env python3
"""The RTP sender answers a NACK from at least a second of what it sent.

A NACK reaches the sender about a round trip after the loss, so the
retransmission history is bounded in time: a fixed count of packets covers a
shrinking slice of the stream as the bitrate rises, and at tens of megabits a
count that once spanned seconds spans a tenth of one. The cap on packets
bounds what a burst can hold. Driven with bare packets and a manual clock.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.webrtc.rtp import RTP_HISTORY_MAX_PACKETS, RTP_HISTORY_S, RtpHistory, RtpPacket

# A 40 Mbit/s stream in 1200-byte packets.
PACKETS_PER_S = 4170


def main() -> int:
    res = H.Results("rtp-history")
    history = RtpHistory()
    # Three seconds of stream that cross the 16-bit wrap in their last half second.
    seq = (65536 - PACKETS_PER_S * 3 + PACKETS_PER_S // 4) & 0xFFFF
    now = 0.0
    sent = []
    for i in range(PACKETS_PER_S * 3):
        now = i / PACKETS_PER_S
        packet = RtpPacket(payload_type=96, sequence_number=seq, timestamp=i)
        history.add(packet, now)
        sent.append((now, seq))
        seq = (seq + 1) & 0xFFFF

    age = lambda s: now - [t for t, q in sent if q == s][-1]
    recent = sent[-int(PACKETS_PER_S * 0.9)][1]
    res.check("a packet sent 0.9 s ago is still there",
              history.get(recent) is not None, f"age {age(recent):.2f}s")
    old_count = sent[-600][1]
    res.check("six hundred packets back is well inside the window",
              history.get(old_count) is not None, f"age {age(old_count):.3f}s")
    expired = sent[-int(PACKETS_PER_S * 1.2)][1]
    res.check("a packet sent 1.2 s ago has been let go",
              history.get(expired) is None, f"age {age(expired):.2f}s")
    res.check("the window holds about a second of the stream",
              abs(len(history) - PACKETS_PER_S * RTP_HISTORY_S) < PACKETS_PER_S * 0.02, len(history))
    wrapped = [q for _, q in sent[-int(PACKETS_PER_S * 0.5):] if q < 200]
    res.check("lookups work across the sequence number wrap",
              wrapped and all(history.get(q) is not None for q in wrapped), wrapped[:3])
    res.check("a sequence number never sent finds nothing",
              history.get((seq + 1000) & 0xFFFF) is None)

    burst = RtpHistory()
    for i in range(RTP_HISTORY_MAX_PACKETS + 500):
        burst.add(RtpPacket(payload_type=96, sequence_number=i & 0xFFFF, timestamp=i), 5.0)
    res.check("a burst inside the window is capped in packets",
              len(burst) == RTP_HISTORY_MAX_PACKETS, len(burst))
    res.check("and the cap lets the oldest packets go first",
              burst.get(499) is None and burst.get(500) is not None)

    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
