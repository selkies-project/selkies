#!/usr/bin/env python3
"""The FlexFEC repair packet is the byte-for-byte XOR the draft describes.

The repair payload XORs everything after each protected packet's fixed
header, shorter packets padded with zeros to the longest, and the recovery
fields fold in the header bits, the lengths and the timestamps. The builder
is checked against a plain byte loop over packets of unequal lengths, and
the NTP clock against the datetime it replaced.
"""
import os
import random
import sys
from struct import pack, unpack

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.webrtc import clock
from selkies.webrtc.rtp import build_flexfec_03


def reference(media_packets, first_seq, protected_ssrc, pt, seq, ts, ssrc) -> bytes:
    recovery = bytearray(2)
    length_recovery = 0
    ts_recovery = 0
    longest = max(len(p) for p in media_packets) - 12
    payload = bytearray(longest)
    mask = 0
    for offset, media in enumerate(media_packets):
        recovery[0] ^= media[0] & 0x3F
        recovery[1] ^= media[1]
        length_recovery ^= len(media) - 12
        ts_recovery ^= unpack("!L", media[4:8])[0]
        for i, b in enumerate(media[12:]):
            payload[i] ^= b
        mask |= 1 << (14 - offset)
    header = bytes([0x80, pt & 0x7F]) + pack("!HLL", seq, ts, ssrc)
    fec = bytes([recovery[0] & 0x3F, recovery[1]]) + pack("!H", length_recovery) + pack("!L", ts_recovery)
    fec += bytes([1, 0, 0, 0]) + pack("!L", protected_ssrc) + pack("!H", first_seq) + pack("!H", 0x8000 | mask)
    return header + fec + bytes(payload)


def packet(rng: random.Random, seq: int, ts: int, size: int) -> bytes:
    head = bytes([0x80 | rng.randrange(0x40), rng.randrange(256)]) + pack("!HLL", seq, ts, 0x1234)
    return head + bytes(rng.getrandbits(8) for _ in range(size))


def main() -> int:
    res = H.Results("flexfec-repair")
    rng = random.Random(7)
    ok = True
    worst = None
    for trial in range(40):
        count = rng.randrange(1, 11)
        seq0 = rng.randrange(65536 - count)
        ts = rng.getrandbits(32)
        packets = [packet(rng, seq0 + i, ts, rng.choice([0, 1, 7, 100, 1188, 1200]))
                   for i in range(count)]
        args = (packets, seq0, 0x1234, 110, rng.randrange(65536), ts, 0x5678)
        got, want = build_flexfec_03(*args), reference(*args)
        if got != want:
            ok = False
            worst = (trial, count, [len(p) for p in packets])
            break
    res.check("forty groups of unequal packets match the byte loop", ok, worst)
    single = [packet(rng, 10, 99, 1200)]
    got = build_flexfec_03(single, 10, 1, 110, 5, 99, 2)
    res.check("a lone packet's repair payload is the packet's own payload",
              got[-1200:] == single[0][12:] and got[12] == single[0][0] & 0x3F)
    res.check("the recovery length is the longest payload",
              len(build_flexfec_03([packet(rng, 1, 0, 5), packet(rng, 2, 0, 900)], 1, 1, 110, 5, 0, 2)) == 12 + 20 + 900)

    a = clock.datetime_to_ntp(clock.current_datetime())
    b = clock.current_ntp_time()
    res.check("the NTP clock agrees with the datetime conversion",
              abs((b >> 32) - (a >> 32)) <= 1 and abs(b - a) < (1 << 32) // 10, (a, b))
    res.check("the NTP seconds are past the Unix epoch offset",
              (b >> 32) > clock.NTP_UNIX_OFFSET, b >> 32)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
