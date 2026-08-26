#!/usr/bin/env python3
"""Byte-level checks of the transport-wide congestion control feedback FCI
(draft-holmer-rmcat-transport-wide-cc-extensions-01) the server answers
browser uplinks with. A malformed FCI is dropped silently by the browser and
its sender starves at the floor bitrate, so the exact bytes are the contract."""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.rtp import pack_twcc_fci  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402


def main() -> int:
    res = H.Results("twcc-feedback")

    fci = pack_twcc_fci(100, [6400.0, None, 6402.0], 0)
    res.check("small deltas and a hole encode to the reference bytes",
              fci == bytes.fromhex("0064000300006400d1000008"), fci.hex())
    res.check("the FCI is 32-bit aligned", len(fci) % 4 == 0, len(fci))

    t0 = 1756200000000.0
    fci = pack_twcc_fci(10, [t0, t0 + 5.0, None, t0 + 33.4], 7)
    units = int(t0 // 64)
    res.check("epoch-scale arrival times keep the masked reference time",
              fci[4:7] == (units & 0xFFFFFF).to_bytes(3, "big"), fci[4:7].hex())
    chunk = int.from_bytes(fci[8:10], "big")
    symbols = [(chunk >> (12 - 2 * j)) & 3 for j in range(4)]
    res.check("epoch-scale deltas stay small, the hole stays a hole",
              symbols == [1, 1, 0, 1], str(symbols))
    res.check("first delta is relative to the reference time",
              0 <= fci[10] <= 255 and fci[10] == round((t0 - units * 64) * 4), fci[10])

    fci = pack_twcc_fci(65535, [128.0, 60.0], 5)
    chunk = int.from_bytes(fci[8:10], "big")
    res.check("a negative delta takes the two-byte symbol",
              (chunk >> 10) & 3 == 2, hex(chunk))
    res.check("and encodes as signed quarter-milliseconds",
              fci[11:13] == (-272 & 0xFFFF).to_bytes(2, "big"), fci[11:13].hex())
    res.check("the wire base sequence wraps to sixteen bits",
              fci[0:2] == b"\xff\xff", fci[0:2].hex())

    res.check("eight packets need two vector chunks",
              len(pack_twcc_fci(0, [1.0] * 8, 0)) == 4 + 4 + 4 + 8, len(pack_twcc_fci(0, [1.0] * 8, 0)))
    res.check("nothing received encodes nothing", pack_twcc_fci(0, [None, None], 0) == b"", "")

    ok = res.summary()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
