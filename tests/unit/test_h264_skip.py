#!/usr/bin/env python3
"""The stand-in for an H.264 frame a peer is not sent (`h264_skip`).

A stand-in keeps every slice header field that shapes the decoder's reference
structure as the left-out frame had it (frame_num, picture order, the list
modification, weights, and reference marking) and codes each macroblock of the
slice as skipped: read back here with a CABAC decoder of its own (9.3.1.2,
9.3.3.2) and an exp-Golomb reader, each slice of a two-slice frame covers its own
macroblocks, and a frame no stand-in can replace (an I slice, an IDR, parameter
sets never seen) gets none.
"""
import os
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))

from types import SimpleNamespace  # noqa: E402

from selkies.webrtc.codecs.h264 import h264_depayload  # noqa: E402
from selkies.webrtc.codecs.h264_skip import (  # noqa: E402
    RANGE_LPS, SKIP_CONTEXT, SKIP_QP, TRANS_LPS, SkipPictures, _Reader, _Writer,
)
from selkies.webrtc.rtcrtpsender import RTCEncodedFrame, RTCRtpSender  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

res = H.Results("h264-skip")
WIDTH_MBS, HEIGHT_MBS = 80, 45


def sps(poc_type: int) -> bytes:
    w = _Writer()
    w.u(8, 100)
    w.u(16, 32)
    w.ue(0)
    w.ue(1)
    w.ue(0)
    w.ue(0)
    w.u(1, 0)
    w.u(1, 0)
    w.ue(4)
    w.ue(poc_type)
    if poc_type == 0:
        w.ue(4)
    w.ue(8)
    w.u(1, 0)
    w.ue(WIDTH_MBS - 1)
    w.ue(HEIGHT_MBS - 1)
    w.u(1, 1)
    w.u(1, 1)
    w.u(1, 0)
    w.u(1, 0)
    w.raw("1")
    return w.nal(0x67)


def pps(cabac: int, weighted: int) -> bytes:
    w = _Writer()
    w.ue(0)
    w.ue(0)
    w.u(1, cabac)
    w.u(1, 0)
    w.ue(0)
    w.ue(0)
    w.ue(0)
    w.u(1, weighted)
    w.u(2, 0)
    w.se(-3)
    w.se(0)
    w.se(0)
    w.u(1, 1)
    w.u(1, 0)
    w.u(1, 0)
    w.raw("1")
    return w.nal(0x68)


def p_slice(first_mb: int, poc_type: int, cabac: int, weighted: int, slice_type: int = 5) -> bytes:
    """A P slice whose header carries an override of two references, a list modification,
    long-term marking, a non-zero cabac_init_idc, quantizer, and deblocking offsets, and whose
    data is arbitrary bytes."""
    w = _Writer()
    w.ue(first_mb)
    w.ue(slice_type)
    w.ue(0)
    w.u(8, 7)
    if poc_type == 0:
        w.u(8, 14)
    w.u(1, 1)
    w.ue(1)
    w.u(1, 1)
    w.ue(0)
    w.ue(4)
    w.ue(2)
    w.ue(1)
    w.ue(3)
    if weighted:
        w.ue(5)
        w.ue(4)
        for luma, chroma in ((1, 0), (0, 1)):
            w.u(1, luma)
            if luma:
                w.se(3)
                w.se(-2)
            w.u(1, chroma)
            if chroma:
                for v in (1, -1, 2, -2):
                    w.se(v)
    w.u(1, 1)
    w.ue(6)
    w.ue(1)
    w.ue(0)
    if cabac:
        w.ue(2)
    w.se(5)
    w.ue(0)
    w.se(1)
    w.se(-1)
    w.raw("1" * (-w.length() % 8) if cabac else "")
    w.raw("1011001110001111" * 8)
    return w.nal(0x61)


def header(nal: bytes, poc_type: int, cabac: int, weighted: int) -> tuple:
    """A slice header read back, and the reader left where its data begins."""
    r = _Reader(nal)
    f = {"first_mb": r.ue(), "slice_type": r.ue(), "pps": r.ue(), "frame_num": r.u(8)}
    if poc_type == 0:
        f["poc_lsb"] = r.u(8)
    f["override"] = r.u(1)
    f["refs"] = r.ue() + 1 if f["override"] else 1
    mods = []
    if r.u(1):
        while True:
            idc = r.ue()
            if idc == 3:
                break
            mods.append((idc, r.ue()))
    f["mods"] = mods
    if weighted:
        f["denoms"] = (r.ue(), r.ue())
        weights = []
        for _ in range(f["refs"]):
            luma = r.u(1)
            weights.append(("l", r.se(), r.se()) if luma else ("l",))
            chroma = r.u(1)
            weights.append(("c",) + tuple(r.se() for _ in range(4)) if chroma else ("c",))
        f["weights"] = weights
    mmco = []
    if r.u(1):
        while True:
            op = r.ue()
            if op == 0:
                break
            mmco.append((op, r.ue()))
    f["mmco"] = mmco
    if cabac:
        f["cabac_init_idc"] = r.ue()
    f["qp"] = 23 + r.se()
    f["deblocking"] = r.ue()
    if f["deblocking"] != 1:
        f["offsets"] = (r.se(), r.se())
    return f, r


def cabac_skips(r: _Reader, mbs: int) -> tuple:
    """Decode `mbs` macroblocks of a P slice as CABAC does (cabac_init_idc 0 at SKIP_QP):
    each one's mb_skip_flag and end_of_slice_flag, then what follows the terminating bin."""
    while r.pos % 8:
        if r.u(1) != 1:
            return "alignment bit not 1", None
    m, n = SKIP_CONTEXT
    pre = max(1, min(126, ((m * SKIP_QP) >> 4) + n))
    state, mps = (63 - pre, 0) if pre <= 63 else (pre - 64, 1)
    rng, offset = 510, r.u(9)
    skips = ends = 0
    for i in range(mbs):
        lps = RANGE_LPS[state][(rng >> 6) & 3]
        rng -= lps
        if offset >= rng:
            binval = 1 - mps
            offset -= rng
            rng = lps
            if state == 0:
                mps = 1 - mps
            state = TRANS_LPS[state]
        else:
            binval = mps
            state = min(state + 1, 62)
        while rng < 256:
            rng <<= 1
            offset = (offset << 1) | r.u(1)
        skips += binval
        rng -= 2
        if offset >= rng:
            ends += 1
            if i != mbs - 1:
                return f"slice ended at macroblock {i}", None
            break
        while rng < 256:
            rng <<= 1
            offset = (offset << 1) | r.u(1)
    rest = r.size - r.pos
    tail = r.u(rest) if rest else 0
    return (skips, ends), (rest, tail)


cases = [("CABAC, picture order type 2", 2, 1, 0), ("CABAC, type 0, weighted prediction", 0, 1, 1),
         ("CAVLC, picture order type 2", 2, 0, 0)]
for label, poc_type, cabac, weighted in cases:
    skips = SkipPictures()
    skips.learn([sps(poc_type), pps(cabac, weighted)])
    original = p_slice(0, poc_type, cabac, weighted)
    out = skips.stand_in([original])
    res.check(f"{label}: one stand-in slice for the slice", out is not None and len(out) == 1, out)
    if not out:
        continue
    before, _ = header(original, poc_type, cabac, weighted)
    after, r = header(out[0], poc_type, cabac, weighted)
    kept = ("first_mb", "slice_type", "pps", "frame_num", "poc_lsb", "override", "refs", "mods", "weights", "mmco")
    res.check(f"{label}: the header keeps frame_num, picture order, list modification, weights, and marking",
              all(before.get(k) == after.get(k) for k in kept) and out[0][0] == original[0],
              {k: (before.get(k), after.get(k)) for k in kept if before.get(k) != after.get(k)})
    res.check(f"{label}: and declares the skip quantizer, no deblocking, cabac_init_idc 0",
              after["qp"] == SKIP_QP and after["deblocking"] == 1 and after.get("cabac_init_idc", 0) == 0, after)
    mbs = WIDTH_MBS * HEIGHT_MBS
    if cabac:
        counts, tail = cabac_skips(r, mbs)
        res.check(f"{label}: every macroblock decodes skipped and the last ends the slice",
                  counts == (mbs, 1), counts)
        res.check(f"{label}: and nothing but alignment follows the stop bit",
                  tail is not None and tail[0] < 8 and tail[1] == 0, tail)
    else:
        run = r.ue()
        stop = r.u(1)
        rest = r.size - r.pos
        res.check(f"{label}: one skip run covers the picture, then the stop bit",
                  run == mbs and stop == 1 and rest < 8 and r.u(rest) == 0, (run, stop, rest))

skips = SkipPictures()
skips.learn([sps(2), pps(1, 0)])
half = WIDTH_MBS * HEIGHT_MBS // 2
two = skips.stand_in([p_slice(0, 2, 1, 0), p_slice(half, 2, 1, 0)])
covered = []
for nal in two or []:
    f, r = header(nal, 2, 1, 0)
    counts, _ = cabac_skips(r, WIDTH_MBS * HEIGHT_MBS - f["first_mb"] if f["first_mb"] else half)
    covered.append((f["first_mb"], counts))
res.check("two slices: each stand-in covers its own macroblocks",
          covered == [(0, (half, 1)), (half, (half, 1))], covered)
res.check("an I slice gets no stand-in", skips.stand_in([p_slice(0, 2, 1, 0, slice_type=7)]) is None)
res.check("nor an IDR", skips.stand_in([sps(2), pps(1, 0), b"\x65" + p_slice(0, 2, 1, 0)[1:]]) is None)
res.check("nor a slice read against parameter sets never seen", SkipPictures().stand_in([p_slice(0, 2, 1, 0)]) is None)


def peer(mime: str = "video/H264"):
    """A sender's stand-in state alone, for a peer without the descriptor."""
    s = SimpleNamespace(_RTCRtpSender__skips=None, stand_ins=0,
                        _RTCRtpSender__send_codec=SimpleNamespace(mimeType=mime))
    for name in ("_numbered", "_learn_parameter_sets", "_stand_in"):
        setattr(s, name, getattr(RTCRtpSender, name).__get__(s))
    return s


def annex_b(nals: list) -> bytes:
    return b"".join(b"\x00\x00\x00\x01" + nal for nal in nals)


key = RTCEncodedFrame([], 0, None, True, None, (1, None),
                      data=annex_b([sps(2), pps(1, 0), b"\x65" + b"\x88" * 16]))
delta = RTCEncodedFrame([], 3000, None, False, (10, 20, 30), (2, 1), data=annex_b([p_slice(0, 2, 1, 0)]))
s = peer()
res.check("a peer sent no key frame yet gets no stand-in", s._stand_in(delta) is None)
s._learn_parameter_sets(key)
out = s._stand_in(delta)
expected = SkipPictures()
expected.learn([sps(2), pps(1, 0)])
want = expected.stand_in([p_slice(0, 2, 1, 0)])
got = b"".join(h264_depayload(payload) for payload in out.payloads) if out else b""
res.check("once one came, a frame left out goes as its stand-in: the frame's timestamp and timing, no key frame",
          out is not None and out.timestamp == 3000 and out.timing == (10, 20, 30) and not out.keyframe
          and s.stand_ins == 1, out and vars(out))
res.check("packed as any frame, it depacks to the stand-in", want is not None and got.endswith(want[0]),
          (len(got), want and len(want[0])))
vp8 = peer("video/VP8")
vp8._learn_parameter_sets(key)
res.check("a VP8 peer gets none: it names its pictures", vp8._stand_in(delta) is None)

sys.exit(0 if res.summary() else 1)
