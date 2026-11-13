/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What every encoder backend shares about a codec: its name and wire identity, the
//! quantizer domain the session-wide quality index maps onto, the level a stream of a
//! given geometry advertises, and the bitstream reads that label a frame on the wire.

/// The picture codec of a capture session, and of a virtual-camera uplink. `Jpeg` is the
/// striped still-image mode; the rest are video codecs, which stream as whole frames. The
/// discriminants are the codec ids the
/// Python API speaks (`VirtualCamera.CODEC_*`) and the video wire ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Codec {
    Jpeg = 0,
    H264 = 1,
    Vp8 = 2,
    Vp9 = 3,
    Av1 = 4,
    H265 = 5,
}

/// The hardware encoder family a session's quantizer curve is read for
/// (`Codec::hardware_quantizer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hardware {
    Nvenc,
    Vaapi,
    VaapiIntel,
}

use super::reference::{REFERENCE_FRAMES, Reference};

/// Wire tag of a JPEG stripe: `u8 reserved`, `u16 frame id`, `u16 stripe Y`, JPEG data.
pub const WIRE_JPEG: u8 = 0x03;
/// Wire tag of an encoded video frame: a type byte whose low nibble is the frame kind and
/// whose high nibble is the codec's [`Codec::wire_id`], then `u16 frame id`, `u16 y` (the
/// frame's top row, 0), `u16 width`, `u16 height`, `u16 reference` (the frame it predicts
/// from, its own id where it names none), and the codec's own bitstream.
pub const WIRE_VIDEO: u8 = 0x04;
/// Frame kinds in the low nibble of the video type byte: a decode entry point, an
/// intra-coded picture that is not one, and a predicted picture.
pub const FRAME_KEY: u8 = 0x01;
pub const FRAME_INTRA: u8 = 0x02;
pub const FRAME_DELTA: u8 = 0x00;
/// A bit ORed into a delta frame's kind: the session keeps the frame as a long-term reference and
/// it predicts from one every client holds (`ReferenceWindow::settle`), so every client can
/// decode it.
pub const FRAME_ANCHOR: u8 = 0x08;
/// Bytes of the JPEG stripe header and the video frame header.
pub const JPEG_HEADER_LEN: usize = 6;
pub const VIDEO_HEADER_LEN: usize = 12;

impl Codec {
    /// The video codecs, in wire-id order.
    pub const VIDEO: [Codec; 5] = [Codec::H264, Codec::Vp8, Codec::Vp9, Codec::Av1, Codec::H265];

    /// The codec's numeric id, as the Python API and the video wire carry it.
    pub const fn id(self) -> u32 {
        self as u32
    }

    /// The codec a numeric id names.
    pub fn from_id(id: u32) -> Option<Codec> {
        [
            Codec::Jpeg,
            Codec::H264,
            Codec::Vp8,
            Codec::Vp9,
            Codec::Av1,
            Codec::H265,
        ]
        .into_iter()
        .find(|c| c.id() == id)
    }

    /// The codec a settings name selects; `None` for a name no backend serves.
    pub fn parse(name: &str) -> Option<Codec> {
        match name.trim().to_ascii_lowercase().as_str() {
            "jpeg" | "mjpeg" => Some(Codec::Jpeg),
            "h264" | "avc" => Some(Codec::H264),
            "h265" | "hevc" => Some(Codec::H265),
            "vp8" => Some(Codec::Vp8),
            "vp9" => Some(Codec::Vp9),
            "av1" => Some(Codec::Av1),
            _ => None,
        }
    }

    /// The settings and log name of the codec.
    pub fn name(self) -> &'static str {
        match self {
            Codec::Jpeg => "jpeg",
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Vp8 => "vp8",
            Codec::Vp9 => "vp9",
            Codec::Av1 => "av1",
        }
    }

    /// The name a stream line prints.
    pub fn display(self) -> &'static str {
        match self {
            Codec::Jpeg => "JPEG",
            Codec::H264 => "H264",
            Codec::H265 => "H265",
            Codec::Vp8 => "VP8",
            Codec::Vp9 => "VP9",
            Codec::Av1 => "AV1",
        }
    }

    /// Whether the session is a video stream rather than striped stills.
    pub fn is_video(self) -> bool {
        self != Codec::Jpeg
    }

    /// Whether a frame of this codec is cut into independently encoded stripes: the
    /// still-image mode is, and every video codec streams whole frames.
    pub fn stripes(self) -> bool {
        self == Codec::Jpeg
    }

    /// The codec's identity in the high nibble of the video type byte: its id.
    pub fn wire_id(self) -> u8 {
        self.id() as u8
    }

    /// The video codec a wire id names.
    pub fn from_wire_id(id: u8) -> Option<Codec> {
        Codec::from_id(u32::from(id)).filter(|c| c.is_video())
    }

    /// The codec tag the Python `StripeFrame` carries: the codec id.
    pub fn data_type(self) -> i32 {
        self.id() as i32
    }

    /// Whether frames depend on earlier ones, so a dropped frame forces a wait for a key
    /// frame.
    pub fn is_inter_coded(self) -> bool {
        self.is_video()
    }

    /// Whether the codec can carry 4:4:4 chroma at all (H.264 High 4:4:4, H.265 RExt, VP9
    /// profile 1); whether a given backend does is negotiated by that backend.
    pub fn fullcolor(self) -> bool {
        matches!(self, Codec::H264 | Codec::H265 | Codec::Vp9)
    }

    /// Whether the codec has a 10-bit profile a session here can ask for (H.265 Main 10 and
    /// its 4:4:4, VP9 profiles 2 and 3, AV1); whether a given backend does is negotiated by
    /// that backend.
    pub fn high_bit_depth(self) -> bool {
        matches!(self, Codec::H265 | Codec::Vp9 | Codec::Av1)
    }

    /// Whether a forced key frame of this codec repeats the stream's parameter sets, so
    /// a joining decoder can start there. Every backend is configured to, so the answer
    /// is the same for all of them.
    pub fn keyframes_self_contained(self) -> bool {
        self.is_video()
    }

    /// The top of the codec's quantizer domain: H.26x QP, the VP8 quantizer index, or
    /// the VP9 / AV1 `base_q_idx`. The value a backend programs never exceeds it.
    pub fn quantizer_max(self) -> u32 {
        match self {
            Codec::Jpeg => 100,
            Codec::H264 | Codec::H265 => 51,
            Codec::Vp8 => 127,
            Codec::Vp9 | Codec::Av1 => 255,
        }
    }

    /// The quantizer, in this codec's own domain, that a session quality index (`video_crf`,
    /// on the H.26x QP scale) selects.
    ///
    /// H.26x takes the index as its QP. The VP8, VP9, and AV1 quantizer indices come from
    /// tables measured on the software encoders against libx264 (ultrafast, zerolatency, four
    /// threads) at the same index, every encoder at the settings pixelflux gives it — VP8 at
    /// libvpx speed 16, VP9 at speed 8 with screen tuning, AV1 at SVT-AV1 preset 11 in its
    /// real-time mode — interpolated in between. An AV1 point is the quantizer whose SSIM
    /// matches on a scrolling text desktop. A VP8 or VP9 point is the one whose PSNR matches,
    /// the finest of a text desktop held still, the same scrolled, and a gradient wallpaper,
    /// which agree within two libvpx levels for VP9; VP8 reaches 43 dB at its finest, so
    /// below index 25 it has nothing left to match. Each table ends at the codec's maximum
    /// quantizer at index 51, since past its last matched point the encoder cannot degrade
    /// as far as x264 does and there is nothing left to match. A hardware session reads
    /// `hardware_quantizer`. An AV1 index of zero is lossless, so its floor is one; the
    /// SVT-AV1 session floors its level at ten, below which its real-time mode miscodes.
    pub fn quantizer(self, crf: i32) -> u32 {
        let crf = crf.clamp(0, 51) as u32;
        match self {
            Codec::Jpeg => crf,
            Codec::H264 | Codec::H265 => crf,
            Codec::Vp8 => interpolate(&VP8_QINDEX, crf),
            Codec::Vp9 => interpolate(&VP9_QINDEX, crf),
            Codec::Av1 => interpolate(&AV1_QINDEX, crf).max(1),
        }
    }

    /// The quality index a capped constant quality's refresh codes a block at
    /// (`ledger::BlockLedger::refresh`), the session's being `quality`: `ledger::REFRESH_FINER`
    /// steps finer, and VP8's no coarser than its quantizer `VP8_REFRESH_QUANTIZER` where
    /// `quality` is at or under `VP8_FINEST_MATCHED`.
    pub fn refresh_index(self, quality: i32) -> i32 {
        let finer = quality - crate::ledger::REFRESH_FINER as i32;
        if self != Codec::Vp8
            || quality > VP8_FINEST_MATCHED
            || self.quantizer(finer) <= VP8_REFRESH_QUANTIZER
        {
            return finer;
        }
        (0..finer)
            .rev()
            .find(|&i| self.quantizer(i) <= VP8_REFRESH_QUANTIZER)
            .unwrap_or(0)
    }

    /// The quantizer a CBR QP clamp (`video_min_qp` / `video_max_qp`, on the H.26x scale)
    /// becomes in this codec's domain; zero stays zero, meaning "the encoder's default".
    pub fn quantizer_bound(self, qp: i32) -> u32 {
        if qp <= 0 { 0 } else { self.quantizer(qp) }
    }

    /// `quantizer` for a hardware session, matched to libx264 the same way. H.264 is measured on
    /// NVENC (Pascal and Ada) and on AMD's VA-API, which agree within a step, each coding a
    /// quantizer about three finer than the index to match x264; AMD's VA-API H.265 follows
    /// the same curve within a step, where NVENC's H.265 is within two steps of the index from
    /// 22 up and keeps it. Intel's VA-API H.265 matches x264 on a still frame at that curve but
    /// refines a moving picture well past it for fewer bytes (Arc and Alder Lake-N), so it reads
    /// one two steps coarser from index 25 up; below 20 its still frames of text fall 3 to 4 dB
    /// short of x264's at that curve, so there it reads one finer, down to the floor of 10 its
    /// low-power encoder codes (`vaapi::h265::INTEL_LOW_POWER_MIN_QP`). AV1 has a curve per backend:
    /// NVENC's is measured on Ada, and VA-API's on Intel (Arc) with the in-loop filters its
    /// constant-quantizer frames carry, on a
    /// text desktop and a gradient wallpaper, taking the finer match of the two. VA-API's VP9
    /// is matched the same way on Intel (Arc), finer than libvpx at the same index on text.
    /// VP8 keeps the software table. NVENC refuses an AV1 index of zero, so the floor stays one.
    pub fn hardware_quantizer(self, hw: Hardware, crf: i32) -> u32 {
        let crf = crf.clamp(0, 51) as u32;
        match (self, hw) {
            (Codec::H264, _) | (Codec::H265, Hardware::Vaapi) => {
                interpolate(&H26X_HARDWARE_QP, crf)
            }
            (Codec::H265, Hardware::VaapiIntel) => interpolate(&H265_INTEL_QP, crf),
            (Codec::Vp9, Hardware::Vaapi | Hardware::VaapiIntel) => {
                interpolate(&VP9_VAAPI_QINDEX, crf)
            }
            (Codec::Av1, Hardware::Nvenc) => interpolate(&AV1_NVENC_QINDEX, crf).max(1),
            (Codec::Av1, Hardware::Vaapi | Hardware::VaapiIntel) => {
                interpolate(&AV1_VAAPI_QINDEX, crf).max(1)
            }
            _ => self.quantizer(crf as i32),
        }
    }

    /// `quantizer_bound` for a hardware session.
    pub fn hardware_quantizer_bound(self, hw: Hardware, qp: i32) -> u32 {
        if qp <= 0 {
            0
        } else {
            self.hardware_quantizer(hw, qp)
        }
    }

    /// The session quality index whose quantizer (`quantizer`) is nearest `q`, a quantizer of
    /// this codec's domain: what a rate-controlled frame's quantizer amounts to on the H.26x
    /// scale the quality settings use.
    pub fn quality_index(self, q: u32) -> u32 {
        nearest_index(q, |crf| self.quantizer(crf))
    }

    /// `quality_index` for a hardware session.
    pub fn hardware_quality_index(self, hw: Hardware, q: u32) -> u32 {
        nearest_index(q, |crf| self.hardware_quantizer(hw, crf))
    }
}

/// The quality index in `0..=51` whose quantizer is nearest `q`, the lowest on a tie.
fn nearest_index(q: u32, quantizer: impl Fn(i32) -> u32) -> u32 {
    (0..=51)
        .min_by_key(|&crf| (i64::from(quantizer(crf)) - i64::from(q)).abs())
        .unwrap_or(0) as u32
}

/// Session quality index → VP8 quantizer index (0..=127) breakpoints.
const VP8_QINDEX: [(u32, u32); 10] = [
    (0, 0),
    (15, 1),
    (20, 3),
    (25, 5),
    (30, 24),
    (35, 49),
    (40, 73),
    (45, 106),
    (50, 127),
    (51, 127),
];
/// The finest session quality index `VP8_QINDEX` matches against x264, under which a step of the
/// index moves VP8's quantizer by one or none.
const VP8_FINEST_MATCHED: i32 = 25;
/// The VP8 quantizer a capped CRF's refresh codes a block at, at the coarsest, where the session's
/// quality index is at or under `VP8_FINEST_MATCHED` (`Codec::refresh_index`), where three index
/// steps move VP8's quantizer by one or two. Refreshed three steps finer, a 320x240 page of text a
/// capped session coded bound came out 0.5 dB under the uncapped session at indices 22 to 24, and
/// a 1280x720 page of text and UI 0.11 under at 22 and 23; at quantizer 2 they came within 0.06 dB
/// at 22 and 23 and level or past at 24 and 25, at 4% fewer to 8% more bytes. Quantizer 3 came out
/// under 4 (43.95 dB against 44.40 at 25), and 1 under 2 (44.68 against 44.79, at 4% more bytes);
/// nine index steps at every index took those past 26 to quantizers 1 to 9, 4 to 17 dB past the
/// session's quality at up to twice the bytes.
const VP8_REFRESH_QUANTIZER: u32 = 2;
/// Session quality index → VP9 `base_q_idx` (0..=255) breakpoints.
const VP9_QINDEX: [(u32, u32); 12] = [
    (0, 0),
    (5, 1),
    (10, 6),
    (15, 18),
    (20, 32),
    (25, 66),
    (30, 108),
    (35, 140),
    (40, 176),
    (45, 208),
    (50, 236),
    (51, 255),
];
/// Session quality index → VA-API VP9 `base_q_idx` breakpoints.
const VP9_VAAPI_QINDEX: [(u32, u32); 12] = [
    (0, 0),
    (5, 2),
    (10, 5),
    (15, 11),
    (20, 19),
    (25, 48),
    (30, 88),
    (35, 124),
    (40, 163),
    (45, 200),
    (50, 228),
    (51, 240),
];
/// Session quality index → AV1 `base_q_idx` (0..=255) breakpoints.
const AV1_QINDEX: [(u32, u32); 10] = [
    (0, 0),
    (10, 12),
    (15, 43),
    (20, 88),
    (25, 134),
    (30, 168),
    (35, 190),
    (40, 215),
    (45, 253),
    (51, 255),
];
/// Session quality index → hardware H.264 QP breakpoints, and VA-API H.265's.
const H26X_HARDWARE_QP: [(u32, u32); 11] = [
    (0, 0),
    (10, 9),
    (15, 14),
    (20, 17),
    (25, 22),
    (30, 27),
    (35, 32),
    (40, 37),
    (45, 43),
    (50, 47),
    (51, 48),
];
/// Session quality index → Intel VA-API H.265 QP breakpoints.
const H265_INTEL_QP: [(u32, u32); 11] = [
    (0, 0),
    (10, 8),
    (15, 12),
    (20, 17),
    (25, 24),
    (30, 29),
    (35, 34),
    (40, 39),
    (45, 45),
    (50, 49),
    (51, 50),
];
/// Session quality index → VA-API AV1 `base_q_idx` breakpoints.
const AV1_VAAPI_QINDEX: [(u32, u32); 12] = [
    (0, 1),
    (5, 3),
    (10, 10),
    (15, 26),
    (20, 40),
    (25, 80),
    (30, 106),
    (35, 136),
    (40, 171),
    (45, 205),
    (50, 236),
    (51, 244),
];
/// Session quality index → NVENC AV1 `base_q_idx` breakpoints.
const AV1_NVENC_QINDEX: [(u32, u32); 12] = [
    (0, 1),
    (5, 20),
    (10, 26),
    (15, 44),
    (20, 66),
    (25, 105),
    (30, 135),
    (35, 163),
    (40, 195),
    (45, 219),
    (50, 241),
    (51, 244),
];

/// Piecewise-linear lookup of `x` in ascending `(x, y)` breakpoints, clamped at both ends.
fn interpolate(points: &[(u32, u32)], x: u32) -> u32 {
    let (first, last) = (points[0], points[points.len() - 1]);
    if x <= first.0 {
        return first.1;
    }
    if x >= last.0 {
        return last.1;
    }
    for pair in points.windows(2) {
        let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
        if x >= x0 && x <= x1 {
            let span = i64::from(x1 - x0);
            let along = i64::from(x - x0);
            let delta = i64::from(y1) - i64::from(y0);
            return (i64::from(y0) + (delta * along + span / 2) / span) as u32;
        }
    }
    last.1
}

/// `base_q_idx` of each libvpx / libaom quantizer level 0..=63, as those libraries map
/// their `--min-q` / `--max-q` / `--cq-level` arguments.
pub const VPX_QINDEX: [u8; 64] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 64, 68, 72, 76, 80, 84, 88, 92,
    96, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, 148, 152, 156, 160, 164, 168,
    172, 176, 180, 184, 188, 192, 196, 200, 204, 208, 212, 216, 220, 224, 228, 232, 236, 240, 244,
    249, 255,
];
/// VP8 quantizer index of each libvpx VP8 quantizer level 0..=63.
pub const VP8_LEVEL_QINDEX: [u8; 64] = [
    0, 1, 2, 3, 4, 5, 7, 8, 9, 10, 12, 13, 15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 27, 28, 29, 30,
    31, 33, 35, 37, 39, 41, 43, 45, 47, 49, 51, 53, 55, 57, 59, 61, 64, 67, 70, 73, 76, 79, 82, 85,
    88, 91, 94, 97, 100, 103, 106, 109, 112, 115, 118, 121, 124, 127,
];

/// The libvpx / SVT-AV1 quantizer level (0..=63) closest to a quantizer index of
/// `codec`'s domain: the software encoders take the level, the hardware ones the index.
pub fn vpx_level(codec: Codec, qindex: u32) -> u32 {
    let table: &[u8; 64] = if codec == Codec::Vp8 {
        &VP8_LEVEL_QINDEX
    } else {
        &VPX_QINDEX
    };
    let target = i64::from(qindex.min(codec.quantizer_max()));
    let mut best = 0usize;
    for (level, &q) in table.iter().enumerate() {
        if (i64::from(q) - target).abs() < (i64::from(table[best]) - target).abs() {
            best = level;
        }
    }
    best as u32
}

/// Append the header of one encoded video frame: the tag, the codec and frame kind, then the
/// frame id, its top row, its width and height, and the id of the frame it
/// predicts from, all big-endian u16. A frame that predicts from none, or whose encoder does
/// not say, names itself there.
#[allow(clippy::too_many_arguments)]
pub fn push_video_header(
    out: &mut Vec<u8>,
    codec: Codec,
    frame_type: u8,
    frame_id: u16,
    y_start: u16,
    width: u16,
    height: u16,
    reference: Reference,
) {
    out.push(WIRE_VIDEO);
    out.push((frame_type & 0x0f) | (codec.wire_id() << 4));
    out.extend_from_slice(&frame_id.to_be_bytes());
    out.extend_from_slice(&y_start.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    let reference = match reference {
        Reference::Frame(id) => id,
        Reference::None | Reference::Untracked => frame_id,
    };
    out.extend_from_slice(&reference.to_be_bytes());
}

/// Append the header of one JPEG stripe.
pub fn push_jpeg_header(out: &mut Vec<u8>, frame_id: u16, y_start: u16) {
    out.push(WIRE_JPEG);
    out.push(0);
    out.extend_from_slice(&frame_id.to_be_bytes());
    out.extend_from_slice(&y_start.to_be_bytes());
}

/// The codec and frame kind a video type byte carries.
pub fn parse_video_type(type_byte: u8) -> Option<(Codec, u8)> {
    Codec::from_wire_id(type_byte >> 4).map(|c| (c, type_byte & 0x0f & !FRAME_ANCHOR))
}

/// The frame kind of a key/delta answer an encoder reports without a bitstream parse.
pub fn frame_type_from_key(key: bool) -> u8 {
    if key { FRAME_KEY } else { FRAME_DELTA }
}

/// Lowest H.264 level whose Annex-A Table A-1 limits admit a `width` x `height` stream at
/// `fps` carrying up to `bitrate_bps` (0 where no rate is declared), as level_idc (41 = 4.1,
/// 52 = 5.2, 62 = 6.2).
///
/// A frame is charged two ways: **MaxFS**, its size in macroblocks, and **MaxMBPS**, that
/// size times the frame rate; a declared rate is charged against **MaxBR** at the High
/// profile's factor of 1.25, which every session here encodes with (4:4:4 allows more), since
/// an encoder that validates the level against the rate refuses a CBR target past it.
/// Advertising the lowest fitting level asks the least of a decoder, so clients that gate
/// hardware decode on the level accept the widest range of streams. One shared ladder keeps
/// every backend agreeing on the same geometry; it starts at 4.1 and a caller needing more
/// raises the floor itself. Above 6.2's limits there is no higher level, so it returns 62 as
/// a best effort.
pub fn h264_level(width: u32, height: u32, fps: u32, bitrate_bps: u64) -> u32 {
    let mbs = u64::from(width).div_ceil(16) * u64::from(height).div_ceil(16);
    let mbps = mbs * u64::from(fps.max(1));
    const LEVELS: [(u32, u64, u64, u64); 8] = [
        (41, 8192, 245760, 50_000_000),
        (42, 8704, 522240, 50_000_000),
        (50, 22080, 589824, 135_000_000),
        (51, 36864, 983040, 240_000_000),
        (52, 36864, 2073600, 240_000_000),
        (60, 139264, 4177920, 240_000_000),
        (61, 139264, 8355840, 480_000_000),
        (62, 139264, 16711680, 800_000_000),
    ];
    for &(level, max_fs, max_mbps, max_br) in &LEVELS {
        if mbs <= max_fs && mbps <= max_mbps && bitrate_bps <= max_br / 4 * 5 {
            return level;
        }
    }
    62
}

/// The decoded picture buffer, in frames, an H.264 stream at `level` may declare for a
/// `width` x `height` picture: the level's MaxDpbMbs over the picture's macroblocks, at most
/// `REFERENCE_FRAMES`.
pub fn h264_dpb_frames(level: u32, width: u32, height: u32) -> u32 {
    let mbs = u64::from(width).div_ceil(16) * u64::from(height).div_ceil(16);
    let max_dpb_mbs: u64 = match level {
        41 => 32768,
        42 => 34816,
        50 => 110400,
        51 | 52 => 184320,
        _ => 696320,
    };
    ((max_dpb_mbs / mbs.max(1)) as u32).clamp(1, REFERENCE_FRAMES)
}

/// The level an x264 session declares so its decoded picture buffer holds `REFERENCE_FRAMES`:
/// the lowest from `level` up to 5.2 whose buffer admits them at `width` x `height`, else 5.2,
/// or `level` itself above 5.2.
#[cfg(any(feature = "gpl", test))]
pub fn h264_reference_level(level: u32, width: u32, height: u32) -> u32 {
    if level > 52 {
        return level;
    }
    [41, 42, 50, 51, 52]
        .into_iter()
        .filter(|&l| l >= level)
        .find(|&l| h264_dpb_frames(l, width, height) == REFERENCE_FRAMES)
        .unwrap_or(52)
}

/// Lowest H.265 level whose Annex-A Tables A.8 and A.9 limits admit a `width` x `height`
/// stream at `fps` carrying up to `bitrate_bps` (0 where no rate is declared) at the High
/// tier or the Main one, as general_level_idc (123 = 4.1, 156 = 5.2, 186 = 6.2).
///
/// A frame is charged by **MaxLumaPs**, its luma samples, and **MaxLumaSr**, those times the
/// frame rate; a declared rate by the tier's **MaxBR**, which an encoder that validates the
/// level against the rate refuses a CBR target past. The ladder starts at 4.1 like the H.264
/// one and answers 6.2 for anything beyond its limits.
pub fn h265_level(width: u32, height: u32, fps: u32, bitrate_bps: u64, high_tier: bool) -> u32 {
    let ps = u64::from(width) * u64::from(height);
    let sr = ps * u64::from(fps.max(1));
    const LEVELS: [(u32, u64, u64, u64, u64); 7] = [
        (123, 2_228_224, 133_693_440, 20_000_000, 50_000_000),
        (150, 8_912_896, 267_386_880, 25_000_000, 100_000_000),
        (153, 8_912_896, 534_773_760, 40_000_000, 160_000_000),
        (156, 8_912_896, 1_069_547_520, 60_000_000, 240_000_000),
        (180, 35_651_584, 1_069_547_520, 60_000_000, 240_000_000),
        (183, 35_651_584, 2_139_095_040, 120_000_000, 480_000_000),
        (186, 35_651_584, 4_278_190_080, 240_000_000, 800_000_000),
    ];
    for &(level, max_ps, max_sr, main_br, high_br) in &LEVELS {
        if ps <= max_ps && sr <= max_sr && bitrate_bps <= if high_tier { high_br } else { main_br }
        {
            return level;
        }
    }
    186
}

/// The reference frames an H.265 stream at `level` may hold for a `width` x `height`
/// picture: the decoded picture buffer A.4.2 sizes (six pictures at the level's full
/// MaxLumaPs, more for a smaller picture) less the picture being decoded, at most
/// `REFERENCE_FRAMES`.
pub fn h265_dpb_frames(level: u32, width: u32, height: u32) -> u32 {
    let max_luma_ps: u64 = match level {
        ..=123 => 2_228_224,
        150..=156 => 8_912_896,
        _ => 35_651_584,
    };
    let picture = u64::from(width) * u64::from(height);
    let pictures = if picture <= max_luma_ps / 4 {
        16
    } else if picture <= max_luma_ps / 2 {
        12
    } else if picture <= max_luma_ps * 3 / 4 {
        8
    } else {
        6
    };
    (pictures - 1).min(REFERENCE_FRAMES)
}

/// The tier an H.265 session declares at `level` (general_level_idc): High (1) from level 4.0
/// up, where Annex A defines one and every decoder of those levels takes it, so a CBR target
/// above the level's Main-tier MaxBR is not refused by an encoder that validates the two
/// together; Main (0) below, where no High tier exists.
pub fn h265_tier(level: u32) -> u32 {
    if level >= 120 { 1 } else { 0 }
}

/// Lowest AV1 level whose Annex A limits admit a `width` x `height` stream at `fps` carrying
/// up to `bitrate_bps` (0 where no rate is declared), as seq_level_idx (8 = 4.0, 13 = 5.1,
/// 19 = 6.3).
///
/// A frame is charged by **MaxPicSize** and the per-axis **MaxHSize** / **MaxVSize**, its
/// rate by **MaxDisplayRate**, and a declared bitrate by the Main tier's **MaxBitrate**, the
/// tier every hardware encoder declares, which an encoder that validates the level against
/// the rate refuses a CBR target past. MaxPicSize is its own limit, far below the product of
/// the axis maxima -- no level admits a picture that is both its widest and its tallest -- so
/// it is carried per level rather than derived. The ladder starts at 4.0 and answers 6.3 for
/// anything beyond its limits.
pub fn av1_level(width: u32, height: u32, fps: u32, bitrate_bps: u64) -> u32 {
    let ps = u64::from(width) * u64::from(height);
    let rate = ps * u64::from(fps.max(1));
    const LEVELS: [(u32, u64, u32, u32, u64, u64); 10] = [
        (8, 2_359_296, 6144, 3456, 70_778_880, 12_000_000),
        (9, 2_359_296, 6144, 3456, 141_557_760, 20_000_000),
        (12, 8_912_896, 8192, 4352, 267_386_880, 30_000_000),
        (13, 8_912_896, 8192, 4352, 534_773_760, 40_000_000),
        (14, 8_912_896, 8192, 4352, 1_069_547_520, 60_000_000),
        (15, 8_912_896, 8192, 4352, 1_069_547_520, 60_000_000),
        (16, 35_651_584, 16384, 8704, 1_069_547_520, 60_000_000),
        (17, 35_651_584, 16384, 8704, 2_139_095_040, 100_000_000),
        (18, 35_651_584, 16384, 8704, 4_278_190_080, 160_000_000),
        (19, 35_651_584, 16384, 8704, 4_278_190_080, 160_000_000),
    ];
    for &(level, max_ps, max_w, max_h, max_rate, max_br) in &LEVELS {
        if ps <= max_ps
            && width <= max_w
            && height <= max_h
            && rate <= max_rate
            && bitrate_bps <= max_br
        {
            return level;
        }
    }
    19
}

/// Wire frame kind of one encoded H.264 access unit, read from the bitstream itself:
/// IDR = key, intra-only non-IDR = intra, predicted = delta. The video header labels
/// real decode entry points, so the NAL and slice types the encoder actually produced
/// decide, never what was requested of it. An access unit whose slices cannot be parsed
/// is labeled delta: a false delta costs one skipped entry point, a false key breaks the
/// decoder.
pub fn h264_frame_type(au: &[u8]) -> u8 {
    let mut intra = false;
    for nal in annexb_nals(au) {
        match nal[0] & 0x1f {
            5 => return FRAME_KEY,
            1 => match h264_slice_is_intra(&nal[1..]) {
                Some(true) => intra = true,
                _ => return FRAME_DELTA,
            },
            _ => {}
        }
    }
    if intra { FRAME_INTRA } else { FRAME_DELTA }
}

/// Wire frame kind of one encoded H.265 access unit: an IRAP picture (BLA, IDR, or CRA,
/// which every backend here emits as an IDR with its parameter sets) is a key, anything
/// else a delta.
pub fn h265_frame_type(au: &[u8]) -> u8 {
    for nal in annexb_nals(au) {
        let nal_type = (nal[0] >> 1) & 0x3f;
        if (16..=23).contains(&nal_type) {
            return FRAME_KEY;
        }
    }
    FRAME_DELTA
}

/// Whether a VP8 frame is a key frame: the frame tag's lowest bit is clear.
pub fn vp8_is_key(frame: &[u8]) -> bool {
    frame.first().is_some_and(|b| b & 1 == 0)
}

/// Whether a VP9 frame is a key frame, read from its uncompressed header: not a shown existing
/// frame, and `frame_type` KEY_FRAME. Profile 3 carries one reserved bit ahead of those two.
pub fn vp9_is_key(frame: &[u8]) -> bool {
    let Some(&b) = frame.first() else {
        return false;
    };
    if b >> 6 != 0b10 {
        return false;
    }
    let profile = ((b >> 5) & 1) | (((b >> 4) & 1) << 1);
    let (show_existing, frame_type) = if profile == 3 {
        (b & 0x04, b & 0x02)
    } else {
        (b & 0x08, b & 0x04)
    };
    show_existing == 0 && frame_type == 0
}

/// The `base_q_idx` of a VP9 frame, read from its uncompressed header past the color
/// configuration, the frame size, the references, and the loop filter; None for a shown
/// existing frame or a header cut short.
pub fn vp9_base_q_idx(frame: &[u8]) -> Option<u32> {
    let mut pos = 0usize;
    let mut bits = |n: usize| -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            let byte = *frame.get(pos / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - pos % 8)) & 1);
            pos += 1;
        }
        Some(value)
    };
    if bits(2)? != 2 {
        return None;
    }
    let profile = bits(1)? | (bits(1)? << 1);
    if profile == 3 {
        bits(1)?;
    }
    if bits(1)? == 1 {
        return None;
    }
    let key = bits(1)? == 0;
    let show_frame = bits(1)? == 1;
    let error_resilient = bits(1)? == 1;
    let color_config = |bits: &mut dyn FnMut(usize) -> Option<u32>| -> Option<()> {
        if profile >= 2 {
            bits(1)?;
        }
        if bits(3)? != 7 {
            bits(1)?;
            if profile & 1 == 1 {
                bits(3)?;
            }
        } else if profile & 1 == 1 {
            bits(1)?;
        }
        Some(())
    };
    let sizes = |bits: &mut dyn FnMut(usize) -> Option<u32>| -> Option<()> {
        bits(32)?;
        if bits(1)? == 1 {
            bits(32)?;
        }
        Some(())
    };
    if key {
        bits(24)?;
        color_config(&mut bits)?;
        sizes(&mut bits)?;
    } else {
        let intra_only = !show_frame && bits(1)? == 1;
        if !error_resilient {
            bits(2)?;
        }
        if intra_only {
            bits(24)?;
            if profile > 0 {
                color_config(&mut bits)?;
            }
            bits(8)?;
            sizes(&mut bits)?;
        } else {
            bits(8)?;
            bits(12)?;
            let mut found = false;
            for _ in 0..3 {
                if bits(1)? == 1 {
                    found = true;
                    break;
                }
            }
            if !found {
                bits(32)?;
            }
            if bits(1)? == 1 {
                bits(32)?;
            }
            bits(1)?;
            if bits(1)? == 0 {
                bits(2)?;
            }
        }
    }
    if !error_resilient {
        bits(2)?;
    }
    bits(2)?;
    bits(9)?;
    if bits(1)? == 1 && bits(1)? == 1 {
        for _ in 0..6 {
            if bits(1)? == 1 {
                bits(7)?;
            }
        }
    }
    bits(8)
}

/// Whether an AV1 temporal unit opens with a key frame: the first frame header or frame
/// OBU's `frame_type`, read past any temporal delimiter and sequence header. A show
/// existing frame, or a unit without a frame, is a delta.
pub fn av1_is_key(tu: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos < tu.len() {
        let header = tu[pos];
        let obu_type = (header >> 3) & 0x0f;
        let has_extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let mut i = pos + 1 + usize::from(has_extension);
        let size = if has_size {
            let mut value = 0u64;
            let mut shift = 0u32;
            loop {
                if i >= tu.len() || shift > 56 {
                    return false;
                }
                let byte = tu[i];
                i += 1;
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            value as usize
        } else {
            tu.len().saturating_sub(i)
        };
        if matches!(obu_type, 3 | 6) {
            let Some(&first) = tu.get(i) else {
                return false;
            };
            let show_existing_frame = first & 0x80 != 0;
            let frame_type = (first >> 5) & 0x03;
            return !show_existing_frame && frame_type == 0;
        }
        pos = i.saturating_add(size);
    }
    false
}

pub(crate) use super::bits::annexb_nals;

/// Whether a non-IDR H.264 slice header opens an I/SI slice. Reads `first_mb_in_slice`
/// and `slice_type` (both ue(v)) from the start of the slice RBSP, undoing emulation
/// prevention bytes; `None` when the header is truncated or malformed.
fn h264_slice_is_intra(payload: &[u8]) -> Option<bool> {
    let mut rbsp = [0u8; 12];
    let mut n = 0usize;
    let mut zeros = 0usize;
    for &b in payload {
        if n == rbsp.len() {
            break;
        }
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp[n] = b;
        n += 1;
    }
    let mut pos = 0usize;
    let bit = |p: &mut usize| -> Option<u8> {
        if *p / 8 >= n {
            return None;
        }
        let b = (rbsp[*p / 8] >> (7 - *p % 8)) & 1;
        *p += 1;
        Some(b)
    };
    let ue = |p: &mut usize| -> Option<u32> {
        let mut leading = 0u32;
        while bit(p)? == 0 {
            leading += 1;
            if leading > 24 {
                return None;
            }
        }
        let mut val = 0u32;
        for _ in 0..leading {
            val = (val << 1) | u32::from(bit(p)?);
        }
        Some((1u32 << leading) - 1 + val)
    };
    ue(&mut pos)?;
    let slice_type = ue(&mut pos)?;
    if slice_type > 9 {
        return None;
    }
    Some(matches!(slice_type % 5, 2 | 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capped CRF's refresh codes VP8 no coarser than its quantizer 2 where the session's index
    /// is at or under the finest its table matches, at the coarsest index that reaches it, and a
    /// refresh's steps finer elsewhere and on every other codec.
    #[test]
    fn a_vp8_refresh_comes_to_its_quantizer_two_at_the_finest_indices() {
        for quality in 0..=51 {
            let finer = quality - crate::ledger::REFRESH_FINER as i32;
            for codec in [Codec::H264, Codec::H265, Codec::Vp9, Codec::Av1] {
                assert_eq!(
                    codec.refresh_index(quality),
                    finer,
                    "{codec:?} at {quality}"
                );
            }
            let vp8 = Codec::Vp8.refresh_index(quality);
            if quality > VP8_FINEST_MATCHED {
                assert_eq!(vp8, finer, "VP8 at {quality}");
            } else {
                assert!(vp8 <= finer, "VP8 at {quality}: {vp8}");
                assert!(Codec::Vp8.quantizer(vp8) <= VP8_REFRESH_QUANTIZER);
                assert!(vp8 == finer || Codec::Vp8.quantizer(vp8 + 1) > VP8_REFRESH_QUANTIZER);
            }
        }
        assert_eq!(Codec::Vp8.refresh_index(25), 18);
        assert_eq!(Codec::Vp8.refresh_index(26), 23);
    }

    /// High tier exists from level 4.0 up and is declared there; the ladder never answers below
    /// 4.1, so every level it names carries one.
    #[test]
    fn hevc_tier_follows_the_level() {
        assert_eq!(h265_tier(93), 0);
        assert_eq!(h265_tier(120), 1);
        assert_eq!(h265_tier(h265_level(1280, 720, 30, 0, true)), 1);
        assert_eq!(h265_tier(h265_level(3840, 2160, 60, 0, true)), 1);
    }

    /// Names round-trip through the parser, and the aliases land on the same codec.
    #[test]
    fn names_round_trip() {
        for codec in [
            Codec::Jpeg,
            Codec::H264,
            Codec::H265,
            Codec::Vp8,
            Codec::Vp9,
            Codec::Av1,
        ] {
            assert_eq!(Codec::parse(codec.name()), Some(codec));
        }
        assert_eq!(Codec::parse("HEVC"), Some(Codec::H265));
        assert_eq!(Codec::parse(" avc "), Some(Codec::H264));
        assert_eq!(Codec::parse("mjpeg"), Some(Codec::Jpeg));
        assert_eq!(Codec::parse("h266"), None);
    }

    /// Every video codec has a distinct wire id that reads back, and the type byte packs
    /// the frame kind beside it.
    #[test]
    fn wire_ids_are_distinct_and_packed() {
        let mut seen = std::collections::HashSet::new();
        for codec in Codec::VIDEO {
            assert!(seen.insert(codec.wire_id()));
            assert_eq!(Codec::from_wire_id(codec.wire_id()), Some(codec));
            let mut out = Vec::new();
            push_video_header(
                &mut out,
                codec,
                FRAME_KEY,
                0x1234,
                7,
                1920,
                1080,
                Reference::None,
            );
            assert_eq!(out.len(), VIDEO_HEADER_LEN);
            assert_eq!(out[0], WIRE_VIDEO);
            assert_eq!(parse_video_type(out[1]), Some((codec, FRAME_KEY)));
            assert_eq!(
                &out[2..],
                &[0x12, 0x34, 0, 7, 0x07, 0x80, 0x04, 0x38, 0x12, 0x34]
            );
            out.clear();
            push_video_header(
                &mut out,
                codec,
                FRAME_DELTA,
                0x1235,
                7,
                1920,
                1080,
                Reference::Frame(0x1230),
            );
            assert_eq!(
                &out[10..],
                &[0x12, 0x30],
                "a predicted frame names its reference"
            );
        }
        assert_eq!(
            Codec::from_wire_id(0),
            None,
            "JPEG never rides the video tag"
        );
        assert_eq!(Codec::from_wire_id(9), None);
        for id in 0..6 {
            assert_eq!(Codec::from_id(id).map(|c| c.id()), Some(id));
        }
        assert_eq!(Codec::from_id(6), None);
        assert_eq!(Codec::H264.data_type(), 1);
        assert_eq!(Codec::Jpeg.data_type(), 0);
        let mut jpeg = Vec::new();
        push_jpeg_header(&mut jpeg, 3, 64);
        assert_eq!(jpeg, [WIRE_JPEG, 0, 0, 3, 0, 64]);
    }

    /// Only the still-image mode stripes; every video codec is a whole-frame stream.
    #[test]
    fn striping_and_chroma_by_codec() {
        assert!(Codec::Jpeg.stripes());
        for codec in Codec::VIDEO {
            assert!(!codec.stripes(), "{codec:?}");
            assert!(codec.is_video());
        }
        assert!(Codec::H264.fullcolor() && Codec::H265.fullcolor() && Codec::Vp9.fullcolor());
        assert!(!Codec::Vp8.fullcolor() && !Codec::Av1.fullcolor());
    }

    /// The quantizer maps are monotonic, span their domain, clamp the index, and keep AV1
    /// off the zero NVENC refuses; H.26x takes the index as it is in software, and hardware
    /// H.264, and VA-API's H.265, a quantizer about three finer.
    #[test]
    fn quantizer_maps_are_monotonic_and_bounded() {
        for codec in Codec::VIDEO {
            let mut last = 0;
            for crf in 0..=51 {
                let q = codec.quantizer(crf);
                assert!(q >= last, "{codec:?} crf {crf}: {q} < {last}");
                for hw in [Hardware::Nvenc, Hardware::Vaapi, Hardware::VaapiIntel] {
                    let h = codec.hardware_quantizer(hw, crf);
                    assert!(h >= codec.hardware_quantizer(hw, crf.max(1) - 1));
                    assert!(h <= codec.quantizer_max());
                }
                assert!(q <= codec.quantizer_max());
                last = q;
            }
            assert_eq!(codec.quantizer(51), codec.quantizer_max());
            assert_eq!(codec.quantizer(99), codec.quantizer(51));
            assert_eq!(codec.quantizer(-4), codec.quantizer(0));
            assert_eq!(codec.quantizer_bound(0), 0);
            assert_eq!(codec.quantizer_bound(30), codec.quantizer(30));
        }
        assert_eq!(Codec::H264.quantizer(25), 25);
        assert_eq!(Codec::H265.quantizer(25), 25);
        assert_eq!(Codec::Av1.quantizer(0), 1);
        assert_eq!(Codec::H264.hardware_quantizer(Hardware::Nvenc, 25), 22);
        assert_eq!(Codec::H264.hardware_quantizer(Hardware::Vaapi, 25), 22);
        assert_eq!(Codec::H265.hardware_quantizer(Hardware::Nvenc, 25), 25);
        assert_eq!(Codec::H265.hardware_quantizer(Hardware::Vaapi, 25), 22);
        assert_eq!(Codec::Av1.hardware_quantizer(Hardware::Vaapi, 25), 80);
        assert_eq!(Codec::H265.hardware_quantizer(Hardware::VaapiIntel, 25), 24);
        assert_eq!(Codec::H264.hardware_quantizer(Hardware::VaapiIntel, 25), 22);
        assert_eq!(Codec::Av1.hardware_quantizer(Hardware::VaapiIntel, 25), 80);
        assert_eq!(Codec::Av1.hardware_quantizer(Hardware::Nvenc, 0), 1);
        assert_eq!(Codec::Vp9.quantizer(25), 66);
        assert_eq!(Codec::Vp9.hardware_quantizer(Hardware::Vaapi, 25), 48);
        assert_eq!(Codec::Vp9.hardware_quantizer(Hardware::VaapiIntel, 25), 48);
        assert_eq!(
            Codec::Vp8.hardware_quantizer(Hardware::Vaapi, 25),
            Codec::Vp8.quantizer(25)
        );
        assert_eq!(Codec::H264.hardware_quantizer_bound(Hardware::Nvenc, 0), 0);
        assert_eq!(Codec::H264.hardware_quality_index(Hardware::Nvenc, 22), 25);
        assert_eq!(interpolate(&[(0, 0), (10, 100)], 5), 50);
        assert_eq!(interpolate(&[(0, 0), (10, 100)], 3), 30);
    }

    /// The libvpx quantizer level found for an index is the closest table entry, at both
    /// ends of both tables.
    #[test]
    fn vpx_levels_invert_the_tables() {
        assert_eq!(vpx_level(Codec::Vp9, 0), 0);
        assert_eq!(vpx_level(Codec::Vp9, 255), 63);
        assert_eq!(vpx_level(Codec::Vp9, 104), 26);
        assert_eq!(vpx_level(Codec::Av1, 249), 62);
        assert_eq!(vpx_level(Codec::Vp8, 0), 0);
        assert_eq!(vpx_level(Codec::Vp8, 127), 63);
        assert_eq!(
            vpx_level(Codec::Vp8, 52),
            36,
            "a tie resolves to the lower level"
        );
        for level in 0..64u32 {
            assert_eq!(
                vpx_level(Codec::Vp9, u32::from(VPX_QINDEX[level as usize])),
                level
            );
            assert_eq!(
                vpx_level(Codec::Vp8, u32::from(VP8_LEVEL_QINDEX[level as usize])),
                level
            );
        }
    }

    /// The decoded picture buffer follows the level and the picture: the reference frames a
    /// session asks for where the level admits them, fewer where it does not.
    #[test]
    fn the_reference_level_holds_every_reference_up_to_5_2() {
        assert_eq!(h264_reference_level(41, 1280, 720), 41);
        assert_eq!(h264_reference_level(42, 1920, 1080), 50);
        assert_eq!(h264_dpb_frames(50, 1920, 1080), REFERENCE_FRAMES);
        assert_eq!(h264_reference_level(51, 2560, 1440), 51);
        assert_eq!(h264_reference_level(52, 3840, 2160), 52);
        assert_eq!(h264_dpb_frames(52, 3840, 2160), 5);
        assert_eq!(h264_reference_level(60, 7680, 4320), 60);
    }

    #[test]
    fn dpb_follows_the_level() {
        assert_eq!(h264_dpb_frames(41, 1280, 720), REFERENCE_FRAMES);
        assert_eq!(h264_dpb_frames(42, 1920, 1080), 4);
        assert_eq!(h264_dpb_frames(50, 2560, 1440), 7);
        assert_eq!(h264_dpb_frames(52, 3840, 2160), 5);
        assert_eq!(h264_dpb_frames(62, 7680, 4320), 5);
        assert_eq!(h265_dpb_frames(123, 1920, 1080), 5);
        assert_eq!(h265_dpb_frames(123, 1280, 720), REFERENCE_FRAMES);
        assert_eq!(h265_dpb_frames(156, 3840, 2160), 5);
        assert_eq!(h265_dpb_frames(180, 3840, 2160), REFERENCE_FRAMES);
    }

    /// The level ladders start at their floors, step through the standard limits, and
    /// saturate rather than fail past the top.
    #[test]
    fn level_ladders() {
        assert_eq!(h264_level(1920, 1080, 60, 0), 42);
        assert_eq!(h264_level(1280, 720, 30, 0), 41);
        assert_eq!(h264_level(3840, 2160, 60, 0), 52);
        assert_eq!(h264_level(7680, 4320, 60, 0), 61);
        assert_eq!(h264_level(16384, 16384, 240, 0), 62);

        assert_eq!(h265_level(1920, 1080, 60, 0, true), 123);
        assert_eq!(h265_level(1920, 1080, 65, 0, true), 150);
        assert_eq!(h265_level(3840, 2160, 60, 0, true), 153);
        assert_eq!(h265_level(3840, 2160, 120, 0, true), 156);
        assert_eq!(h265_level(7680, 4320, 30, 0, true), 180);
        assert_eq!(h265_level(7680, 4320, 60, 0, true), 183);
        assert_eq!(h265_level(7680, 4320, 120, 0, true), 186);
        assert_eq!(h265_level(16384, 16384, 240, 0, true), 186);

        assert_eq!(av1_level(1280, 720, 30, 0), 8);
        assert_eq!(av1_level(1920, 1080, 60, 0), 9);
        assert_eq!(av1_level(3840, 2160, 60, 0), 13);
        assert_eq!(av1_level(3840, 2160, 120, 0), 14);
        assert_eq!(
            av1_level(4096, 2304, 30, 0),
            16,
            "larger than 5.x's MaxPicSize"
        );
        assert_eq!(av1_level(7680, 4320, 60, 0), 17);
        assert_eq!(av1_level(7680, 4320, 120, 0), 18);
        // Both axes fit 5.x, the picture does not: MaxPicSize is not the product of the axes.
        assert_eq!(av1_level(8192, 4352, 60, 0), 17);
        assert_eq!(av1_level(8192, 4352, 240, 0), 19);
    }

    /// A declared bitrate raises a level the picture alone would not: past 4.2's 62.5 Mbit/s
    /// (High profile) for H.264 at 1080p60, past the tier's ceiling for HEVC, past the Main
    /// tier's for AV1; a rate within the ceiling leaves the level where the picture put it.
    #[test]
    fn level_ladders_carry_the_bitrate() {
        assert_eq!(h264_level(1920, 1080, 60, 62_500_000), 42);
        assert_eq!(h264_level(1920, 1080, 60, 100_000_000), 50);
        assert_eq!(h264_level(1920, 1080, 60, 300_000_000), 51);
        assert_eq!(h264_level(1920, 1080, 60, 1_000_000_000), 62);

        assert_eq!(h265_level(1920, 1080, 60, 50_000_000, true), 123);
        assert_eq!(h265_level(1920, 1080, 60, 60_000_000, true), 150);
        assert_eq!(h265_level(1920, 1080, 60, 60_000_000, false), 156);
        assert_eq!(h265_level(1920, 1080, 60, 20_000_000, false), 123);
        assert_eq!(h265_level(3840, 2160, 60, 200_000_000, true), 156);

        assert_eq!(av1_level(1920, 1080, 60, 20_000_000), 9);
        assert_eq!(av1_level(1920, 1080, 60, 25_000_000), 12);
        assert_eq!(av1_level(3840, 2160, 60, 45_000_000), 14);
        assert_eq!(av1_level(1280, 720, 30, 100_000_000), 17);
        assert_eq!(av1_level(1280, 720, 30, 1_000_000_000), 19);
    }

    /// H.264 labels follow the NAL and slice types: an IDR is a key, an I slice in a
    /// non-IDR is intra, a P slice is delta, and a truncated slice is delta.
    #[test]
    fn h264_labels_from_the_bitstream() {
        let idr = [
            0, 0, 0, 1, 0x67, 0x64, 0, 0x1f, 0, 0, 1, 0x65, 0x88, 0x84, 0,
        ];
        assert_eq!(h264_frame_type(&idr), FRAME_KEY);
        // slice_type 7 (I, all slices) encoded ue: first_mb=0 -> '1', slice_type 7 -> '0001000'
        let intra = [0, 0, 1, 0x41, 0b1000_1000, 0b0000_0000];
        assert_eq!(h264_frame_type(&intra), FRAME_INTRA);
        // slice_type 0 (P): first_mb '1', slice_type '1'
        let p = [0, 0, 1, 0x41, 0b1100_0000];
        assert_eq!(h264_frame_type(&p), FRAME_DELTA);
        assert_eq!(h264_frame_type(&[0, 0, 1, 0x41]), FRAME_DELTA);
        assert_eq!(h264_frame_type(&[]), FRAME_DELTA);
    }

    /// H.265 labels follow the NAL type: an IDR (19) or CRA (21) is a key, a TRAIL_R (1)
    /// picture behind parameter sets is a delta, and a stream of parameter sets alone is
    /// a delta.
    #[test]
    fn h265_labels_from_the_bitstream() {
        let idr = [0, 0, 0, 1, 0x40, 0x01, 0x0c, 0, 0, 1, 0x26, 0x01, 0xaf];
        assert_eq!(h265_frame_type(&idr), FRAME_KEY);
        let cra = [0, 0, 1, 0x2a, 0x01, 0xaf];
        assert_eq!(h265_frame_type(&cra), FRAME_KEY);
        let trail = [0, 0, 1, 0x02, 0x01, 0xd0];
        assert_eq!(h265_frame_type(&trail), FRAME_DELTA);
        let headers_only = [0, 0, 1, 0x40, 0x01, 0, 0, 1, 0x42, 0x01];
        assert_eq!(h265_frame_type(&headers_only), FRAME_DELTA);
    }

    /// VP8 keys are read from the frame tag, VP9 keys from the uncompressed header in every
    /// profile, with a shown existing frame never a key.
    #[test]
    fn vpx_labels_from_the_bitstream() {
        assert!(vp8_is_key(&[0x50, 0x42, 0x00]));
        assert!(!vp8_is_key(&[0x51, 0x42, 0x00]));
        assert!(!vp8_is_key(&[]));
        assert!(vp9_is_key(&[0x82, 0x49, 0x83]));
        assert!(!vp9_is_key(&[0x86, 0x00]));
        assert!(!vp9_is_key(&[0x8a, 0x00]));
        assert!(vp9_is_key(&[0xb0, 0x00]), "profile 3");
        assert!(!vp9_is_key(&[0xb2, 0x00]), "profile 3 inter");
        assert!(!vp9_is_key(&[0x42, 0x00]), "bad frame marker");
    }

    /// AV1 labels come from the first frame's header past the temporal delimiter and
    /// sequence header: frame_type 0 is a key, 1 (inter) or a shown existing frame a
    /// delta, and a unit that ends before a frame, or a malformed size, is a delta.
    #[test]
    fn av1_labels_from_the_temporal_unit() {
        // TD (type 2, has_size, size 0), SEQ (type 1, size 1), FRAME (type 6, size 2) key.
        let key = [0x12, 0x00, 0x0a, 0x01, 0x00, 0x32, 0x02, 0x10, 0x00];
        assert!(av1_is_key(&key));
        let inter = [0x12, 0x00, 0x32, 0x02, 0x30, 0x00];
        assert!(!av1_is_key(&inter));
        let existing = [0x32, 0x01, 0x80];
        assert!(!av1_is_key(&existing));
        let frame_header_only = [0x1a, 0x01, 0x00];
        assert!(av1_is_key(&frame_header_only));
        assert!(!av1_is_key(&[0x12, 0x00]));
        assert!(!av1_is_key(&[
            0x32, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
        ]));
        assert!(!av1_is_key(&[]));
    }
}
