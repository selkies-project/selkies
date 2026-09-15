#!/usr/bin/env python3
"""The FlexFEC repair packet is the byte-for-byte XOR the draft describes.

The repair payload XORs everything after each protected packet's fixed
header, shorter packets padded with zeros to the longest, and the recovery
fields fold in the header bits, the lengths and the timestamps. The builder
is checked against a plain byte loop over packets of unequal lengths, several
interleaved repairs against the receiver's one-missing-at-a-time recovery,
the repair density against the loss it follows, and the NTP clock against the
datetime it replaced.
"""
import os
import random
import sys
import types
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


def recover(repairs: list, received: dict) -> dict:
    """What libwebrtc's `AttemptRecovery` brings back: for each repair whose
    covered packets miss exactly one, XOR the rest against the repair payload
    and the recovery fields, then rescan; stops when no repair can recover."""
    recovered = {}
    pending = list(repairs)
    progress = True
    while progress:
        progress = False
        for fec in list(pending):
            base, mask = unpack("!H", fec[28:30])[0], unpack("!H", fec[30:32])[0] & 0x7FFF
            covered = [base + i for i in range(15) if mask & (1 << (14 - i))]
            missing = [seq for seq in covered if seq not in received]
            if len(missing) != 1:
                if not missing:
                    pending.remove(fec)
                continue
            length = unpack("!H", fec[14:16])[0]
            ts = unpack("!L", fec[16:20])[0]
            first = fec[12] & 0x3F
            second = fec[13]
            payload = int.from_bytes(fec[32:], "big")
            longest = len(fec) - 32
            for seq in covered:
                if seq == missing[0]:
                    continue
                m = received[seq]
                first ^= m[0] & 0x3F
                second ^= m[1]
                length ^= len(m) - 12
                ts ^= unpack("!L", m[4:8])[0]
                payload ^= int.from_bytes(m[12:], "big") << (8 * (longest - len(m) + 12))
            body = payload.to_bytes(longest, "big")[:length]
            packet = bytes([0x80 | first, second]) + pack("!HLL", missing[0], ts, 0x1234) + body
            received[missing[0]] = packet
            recovered[missing[0]] = packet
            pending.remove(fec)
            progress = True
    return recovered


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

    # libwebrtc's receiver recovers a packet from any repair with exactly one
    # covered packet missing and rescans after each recovery: two interleaved
    # repairs bring back a two-packet burst that one repair cannot.
    group = [packet(rng, 100 + i, 777, rng.choice([300, 1200])) for i in range(10)]
    lost = {103, 104}
    received = {seq: pkt for seq, pkt in ((100 + i, p) for i, p in enumerate(group)) if seq not in lost}
    two = [build_flexfec_03(group, 100, 0x1234, 110, 50 + r, 777, 0x5678, range(r, 10, 2)) for r in range(2)]
    one = [build_flexfec_03(group, 100, 0x1234, 110, 60, 777, 0x5678)]
    res.check("interleaved repairs cover alternate packets",
              [unpack("!H", f[30:32])[0] & 0x7FFF for f in two] == [0b101010101000000, 0b010101010100000],
              [bin(unpack("!H", f[30:32])[0] & 0x7FFF) for f in two])
    res.check("two interleaved repairs recover a two-packet burst",
              recover(two, dict(received)) == {seq: group[seq - 100] for seq in lost})
    res.check("one repair recovers neither packet of the burst", recover(one, dict(received)) == {})
    res.check("one repair recovers a single loss",
              recover(one, {seq: pkt for seq, pkt in ((100 + i, p) for i, p in enumerate(group)) if seq != 107}) == {107: group[7]})

    # The density steer is a method over two attributes, driven here on a
    # stand-in for a sender, which needs a transport to be built.
    from selkies.webrtc.rtcrtpsender import RTCRtpSender
    stand_in = types.SimpleNamespace(_fec_loss=0.0, fec_repair_packets=1)
    steps = []
    for loss in (0.0, 0.0, 0.03, 0.03, 0.03, 0.03, 0.15, 0.15, 0.15, 0.15, 0.15, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0):
        RTCRtpSender.steer_fec(stand_in, loss)
        steps.append(stand_in.fec_repair_packets)
    res.check("no loss keeps one repair; a few windows past 2% add a second and past 8% a third; a clean spell takes them back",
              steps[:2] == [1, 1] and 2 in steps[2:6] and steps[10] == 3 and steps[-1] == 1, steps)

    a = clock.datetime_to_ntp(clock.current_datetime())
    b = clock.current_ntp_time()
    res.check("the NTP clock agrees with the datetime conversion",
              abs((b >> 32) - (a >> 32)) <= 1 and abs(b - a) < (1 << 32) // 10, (a, b))
    res.check("the NTP seconds are past the Unix epoch offset",
              (b >> 32) > clock.NTP_UNIX_OFFSET, b >> 32)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
