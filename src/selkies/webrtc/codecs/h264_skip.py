"""A picture that stands in for an H.264 frame a peer is not sent: every macroblock skipped, so it
repeats the picture before it, under the left-out frame's own frame_num, picture order, reference
list modification, weights, and reference marking. A decoder that refuses a frame_num gap
(OpenH264, which Firefox decodes WebRTC H.264 with where it has no system FFmpeg) then takes the
stream whole, its decoded picture buffer the encoder's in every structure but the pixels of the
left-out frames, which the encoder predicts past (`RTCRtpSender._repair`).

Only the slice header's entropy context index, quantizer, and deblocking change: `cabac_init_idc`
0 and `SKIP_QP` let one arithmetic-coded run of skips serve every slice of a size, and a picture of
skips has no edge to filter. A slice this cannot rewrite (an I slice, slice groups, separate colour
planes, a redundant picture) gets no stand-in.
"""
from typing import Dict, List, Optional, Tuple

# Table 9-44: rangeTabLPS, by pStateIdx and qCodIRangeIdx.
RANGE_LPS = (
    (128, 176, 208, 240), (128, 167, 197, 227), (128, 158, 187, 216), (123, 150, 178, 205),
    (116, 142, 169, 195), (111, 135, 160, 185), (105, 128, 152, 175), (100, 122, 144, 166),
    (95, 116, 137, 158), (90, 110, 130, 150), (85, 104, 123, 142), (81, 99, 117, 135),
    (77, 94, 111, 128), (73, 89, 105, 122), (69, 85, 100, 116), (66, 80, 95, 110),
    (62, 76, 90, 104), (59, 72, 86, 99), (56, 69, 81, 94), (53, 65, 77, 89),
    (51, 62, 73, 85), (48, 59, 69, 80), (46, 56, 66, 76), (43, 53, 63, 72),
    (41, 50, 59, 69), (39, 48, 56, 65), (37, 45, 54, 62), (35, 43, 51, 59),
    (33, 41, 48, 56), (32, 39, 46, 53), (30, 37, 43, 50), (29, 35, 41, 48),
    (27, 33, 39, 45), (26, 31, 37, 43), (24, 30, 35, 41), (23, 28, 33, 39),
    (22, 27, 32, 37), (21, 26, 30, 35), (20, 24, 29, 33), (19, 23, 27, 31),
    (18, 22, 26, 30), (17, 21, 25, 28), (16, 20, 23, 27), (15, 19, 22, 25),
    (14, 18, 21, 24), (14, 17, 20, 23), (13, 16, 19, 22), (12, 15, 18, 21),
    (12, 14, 17, 20), (11, 14, 16, 19), (11, 13, 15, 18), (10, 12, 15, 17),
    (10, 12, 14, 16), (9, 11, 13, 15), (9, 11, 12, 14), (8, 10, 12, 14),
    (8, 9, 11, 13), (7, 9, 11, 12), (7, 9, 10, 12), (7, 8, 10, 11),
    (6, 8, 9, 11), (6, 7, 9, 10), (6, 7, 8, 9), (2, 2, 2, 2),
)
# Table 9-45: transIdxLPS, by pStateIdx.
TRANS_LPS = (
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22,
    23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34, 34, 35, 35, 35, 36, 36,
    36, 37, 37, 37, 38, 38, 63,
)
# Table 9-13: (m, n) of mb_skip_flag's context in a P slice whose neighbours are skipped or absent
# (ctxIdx 11), at cabac_init_idc 0.
SKIP_CONTEXT = (23, 33)
# The quantizer a stand-in declares. It codes no residual, so only the arithmetic coder's start reads it.
SKIP_QP = 26
P_SLICE, I_SLICE = 0, 2
NAL_SLICE, NAL_IDR, NAL_SPS, NAL_PPS = 1, 5, 7, 8
# The bytes of a slice a stand-in is read from: its header, which encoders keep to a few dozen bytes.
# A header running past them gets no stand-in.
HEAD_BYTES = 256


class _Reader:
    """Exp-Golomb and fixed-width reads over a NAL unit's payload, emulation prevention removed."""

    def __init__(self, nal: bytes) -> None:
        rbsp, zeros = bytearray(), 0
        for byte in nal[1:]:
            if zeros >= 2 and byte == 3:
                zeros = 0
                continue
            zeros = zeros + 1 if byte == 0 else 0
            rbsp.append(byte)
        self.value = int.from_bytes(rbsp, "big") if rbsp else 0
        self.size = len(rbsp) * 8
        self.pos = 0

    def u(self, n: int) -> int:
        if n == 0:
            return 0
        if self.pos + n > self.size:
            raise ValueError("read past the end of the NAL unit")
        self.pos += n
        return (self.value >> (self.size - self.pos)) & ((1 << n) - 1)

    def ue(self) -> int:
        zeros = 0
        while self.u(1) == 0:
            zeros += 1
            if zeros > 31:
                raise ValueError("exp-Golomb code too long")
        return (1 << zeros) - 1 + self.u(zeros)

    def se(self) -> int:
        k = self.ue()
        return (k + 1) // 2 if k % 2 else -(k // 2)


class _Writer:
    """Bits gathered into an RBSP, written out as a NAL unit with emulation prevention."""

    def __init__(self) -> None:
        self.bits: List[str] = []

    def u(self, n: int, v: int) -> None:
        if n:
            self.bits.append(format(v, f"0{n}b"))

    def ue(self, v: int) -> None:
        code = v + 1
        self.bits.append("0" * (code.bit_length() - 1) + format(code, "b"))

    def se(self, v: int) -> None:
        self.ue(2 * v - 1 if v > 0 else -2 * v)

    def raw(self, bits: str) -> None:
        self.bits.append(bits)

    def length(self) -> int:
        return sum(len(b) for b in self.bits)

    def nal(self, header: int) -> bytes:
        bits = "".join(self.bits)
        bits += "0" * (-len(bits) % 8)
        rbsp = int(bits, 2).to_bytes(len(bits) // 8, "big") if bits else b""
        out, zeros = bytearray([header]), 0
        for byte in rbsp:
            if zeros >= 2 and byte <= 3:
                out.append(3)
                zeros = 0
            out.append(byte)
            zeros = zeros + 1 if byte == 0 else 0
        return bytes(out)


def _skip_scaling_list(r: _Reader, size: int) -> None:
    last = following = 8
    for _ in range(size):
        if following:
            following = (last + r.se() + 256) % 256
        last = following or last


def parse_sps(nal: bytes) -> Tuple[int, dict]:
    """The fields of a sequence parameter set a slice header's syntax depends on, by its id."""
    r = _Reader(nal)
    profile = r.u(8)
    r.u(16)
    sps_id = r.ue()
    sps = {"chroma_format_idc": 1, "separate_colour_plane": 0}
    if profile in (100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135):
        sps["chroma_format_idc"] = r.ue()
        if sps["chroma_format_idc"] == 3:
            sps["separate_colour_plane"] = r.u(1)
        r.ue()
        r.ue()
        r.u(1)
        if r.u(1):
            for i in range(8 if sps["chroma_format_idc"] != 3 else 12):
                if r.u(1):
                    _skip_scaling_list(r, 16 if i < 6 else 64)
    sps["log2_max_frame_num"] = r.ue() + 4
    sps["poc_type"] = r.ue()
    if sps["poc_type"] == 0:
        sps["log2_max_poc_lsb"] = r.ue() + 4
    elif sps["poc_type"] == 1:
        sps["delta_pic_order_always_zero"] = r.u(1)
        r.se()
        r.se()
        for _ in range(r.ue()):
            r.se()
    r.ue()
    r.u(1)
    width = r.ue() + 1
    height_units = r.ue() + 1
    sps["frame_mbs_only"] = r.u(1)
    sps["mbs"] = width * height_units * (2 - sps["frame_mbs_only"])
    return sps_id, sps


def parse_pps(nal: bytes) -> Tuple[int, dict]:
    """The fields of a picture parameter set a slice header's syntax depends on, by its id."""
    r = _Reader(nal)
    pps_id = r.ue()
    pps = {"sps_id": r.ue(), "cabac": r.u(1), "bottom_field_poc": r.u(1), "slice_groups": r.ue() + 1}
    if pps["slice_groups"] > 1:
        return pps_id, pps
    pps["num_ref_idx_l0_default"] = r.ue() + 1
    r.ue()
    pps["weighted_pred"] = r.u(1)
    r.u(2)
    pps["pic_init_qp"] = 26 + r.se()
    r.se()
    r.se()
    pps["deblocking_control"] = r.u(1)
    r.u(1)
    pps["redundant_pic_cnt"] = r.u(1)
    return pps_id, pps


def _cabac_skips(mbs: int) -> str:
    """The slice data, from its first byte, of `mbs` macroblocks each skipped, the last ending the
    slice: mb_skip_flag 1 and end_of_slice_flag 0 for each, the arithmetic coder flushed after the
    last end_of_slice_flag 1 (9.3.4), its stop bit included."""
    m, n = SKIP_CONTEXT
    pre = max(1, min(126, ((m * SKIP_QP) >> 4) + n))
    state, mps = (63 - pre, 0) if pre <= 63 else (pre - 64, 1)
    low, rng, outstanding, first = 0, 510, 0, True
    bits: List[str] = []

    def put(bit: int) -> None:
        nonlocal first, outstanding
        if first:
            first = False
        else:
            bits.append("1" if bit else "0")
        if outstanding:
            bits.append(("0" if bit else "1") * outstanding)
            outstanding = 0

    def renorm() -> None:
        nonlocal low, rng, outstanding
        while rng < 256:
            if low < 256:
                put(0)
            elif low >= 512:
                low -= 512
                put(1)
            else:
                low -= 256
                outstanding += 1
            rng <<= 1
            low <<= 1

    for i in range(mbs):
        lps = RANGE_LPS[state][(rng >> 6) & 3]
        rng -= lps
        if mps != 1:
            low += rng
            rng = lps
            if state == 0:
                mps = 1 - mps
            state = TRANS_LPS[state]
        else:
            state = min(state + 1, 62)
        renorm()
        rng -= 2
        if i < mbs - 1:
            renorm()
    low += rng
    rng = 2
    renorm()
    put((low >> 9) & 1)
    bits.append(format(((low >> 7) & 3) | 1, "02b"))
    return "".join(bits)


class SkipPictures:
    """Stand-ins for the frames of one stream a peer is not sent (`stand_in`), read against the
    parameter sets its key frames carried (`learn`)."""

    def __init__(self) -> None:
        self._sps: Dict[int, dict] = {}
        self._pps: Dict[int, dict] = {}
        self._cabac: Dict[int, str] = {}

    def learn(self, nals: List[bytes]) -> None:
        """Take the SPS and PPS among a frame's NAL units."""
        for nal in nals:
            kind = nal[0] & 0x1F if nal else 0
            try:
                if kind == NAL_SPS:
                    sps_id, sps = parse_sps(nal)
                    self._sps[sps_id] = sps
                elif kind == NAL_PPS:
                    pps_id, pps = parse_pps(nal)
                    self._pps[pps_id] = pps
            except ValueError:
                continue

    def stand_in(self, nals: List[bytes]) -> Optional[List[bytes]]:
        """A skipped slice for each slice of the frame `nals`, or None where one cannot be made.
        Only a slice's first HEAD_BYTES are read, so the caller may pass no more."""
        slices = [nal[:HEAD_BYTES] for nal in nals if nal and nal[0] & 0x1F == NAL_SLICE]
        if not slices or any(nal[0] & 0x1F == NAL_IDR for nal in nals):
            return None
        try:
            starts = [_Reader(nal).ue() for nal in slices]
            out = []
            for k, nal in enumerate(slices):
                built = self._skip_slice(nal, starts[k], starts[k + 1] if k + 1 < len(slices) else None)
                if built is None:
                    return None
                out.append(built)
            return out
        except (ValueError, KeyError, IndexError):
            return None

    def _skip_slice(self, nal: bytes, first_mb: int, next_mb: Optional[int]) -> Optional[bytes]:
        r, w = _Reader(nal), _Writer()
        w.ue(r.ue())
        slice_type = r.ue()
        if slice_type % 5 != P_SLICE:
            return None
        w.ue(slice_type)
        pps_id = r.ue()
        pps = self._pps[pps_id]
        if pps["slice_groups"] > 1:
            return None
        sps = self._sps[pps["sps_id"]]
        if sps["separate_colour_plane"]:
            return None
        w.ue(pps_id)
        w.u(sps["log2_max_frame_num"], r.u(sps["log2_max_frame_num"]))
        field = 0
        if not sps["frame_mbs_only"]:
            field = r.u(1)
            w.u(1, field)
            if field:
                w.u(1, r.u(1))
        if sps["poc_type"] == 0:
            w.u(sps["log2_max_poc_lsb"], r.u(sps["log2_max_poc_lsb"]))
            if pps["bottom_field_poc"] and not field:
                w.se(r.se())
        elif sps["poc_type"] == 1 and not sps["delta_pic_order_always_zero"]:
            w.se(r.se())
            if pps["bottom_field_poc"] and not field:
                w.se(r.se())
        if pps["redundant_pic_cnt"]:
            redundant = r.ue()
            if redundant:
                return None
            w.ue(redundant)
        refs = pps["num_ref_idx_l0_default"]
        override = r.u(1)
        w.u(1, override)
        if override:
            refs = r.ue() + 1
            w.ue(refs - 1)
        modified = r.u(1)
        w.u(1, modified)
        while modified:
            idc = r.ue()
            w.ue(idc)
            if idc in (0, 1, 2):
                w.ue(r.ue())
            elif idc == 3:
                break
            else:
                return None
        if pps["weighted_pred"]:
            chroma = sps["chroma_format_idc"] != 0
            w.ue(r.ue())
            if chroma:
                w.ue(r.ue())
            for _ in range(refs):
                flag = r.u(1)
                w.u(1, flag)
                if flag:
                    w.se(r.se())
                    w.se(r.se())
                if chroma:
                    flag = r.u(1)
                    w.u(1, flag)
                    if flag:
                        for _ in range(4):
                            w.se(r.se())
        if nal[0] & 0x60:
            adaptive = r.u(1)
            w.u(1, adaptive)
            while adaptive:
                op = r.ue()
                w.ue(op)
                if op == 0:
                    break
                if op in (1, 3):
                    w.ue(r.ue())
                if op == 2:
                    w.ue(r.ue())
                if op in (3, 6):
                    w.ue(r.ue())
                if op == 4:
                    w.ue(r.ue())
        if pps["cabac"]:
            r.ue()
            w.ue(0)
        r.se()
        w.se(SKIP_QP - pps["pic_init_qp"])
        if pps["deblocking_control"]:
            if r.ue() != 1:
                r.se()
                r.se()
            w.ue(1)
        mbs = (next_mb if next_mb is not None else sps["mbs"]) - first_mb
        if mbs <= 0:
            return None
        if pps["cabac"]:
            w.raw("1" * (-w.length() % 8))
            data = self._cabac.get(mbs)
            if data is None:
                data = self._cabac[mbs] = _cabac_skips(mbs)
            w.raw(data)
        else:
            w.ue(mbs)
            w.raw("1")
        return w.nal(nal[0])
