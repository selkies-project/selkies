/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Pure-Rust fragmented-MP4 (fMP4) muxer for the built-in recorder.
//!
//! Hand-rolled rather than pulled in as a dependency for two reasons: libavformat would be a hard
//! runtime dependency of every build, and the pure-Rust mp4 crates only write moov-trailing
//! progressive files, which lose everything on a crash. Fragmented MP4 needs no trailer and no
//! seeking — each `moof`+`mdat` pair is self-contained — so a file truncated by a crash or SIGKILL
//! stays playable up to the last fragment, and the writer works on any `Write` sink.
//!
//! Timestamps are caller-supplied wall-clock microseconds (damage-driven capture emits sparse,
//! irregular frames), carried at a 90 kHz track timescale with one sample per fragment: `tfdt`
//! anchors every sample at its true capture time, so variable framerate needs no constant-rate
//! lie. The sample duration is only known once the NEXT frame arrives, so one sample is always
//! buffered and flushed a frame behind (a clean stop closes it with the median observed
//! duration); on SIGKILL at most that one buffered frame is lost — every fragment already
//! written remains playable.
//!
//! Codec support is deliberately split: [`annexb_to_sample`] and the H.264-specific parameter-set
//! capture live in [`H264SampleBuilder`], while the fragment/box writer below is codec-agnostic
//! (bytes + sync flag + timestamps + a ready-made `stsd` sample entry). HEVC or AV1 recording
//! later means a new sample builder emitting an `hvc1`/`av01` entry, not a new muxer.
//!
//! An optional Opus audio track (track 2) takes packets with their decode time on the
//! 48 kHz clock and each one's duration from its own TOC byte; they go out in fragments of
//! their own, so a still desktop that yields no video fragment still keeps the file growing.

use std::io::Write;

/// 90 kHz: the conventional H.264 track timescale, exactly representing common frame intervals.
const TIMESCALE: u32 = 90_000;

/// Fallback duration for the final buffered sample when only one frame was ever written
/// (no observed inter-frame delta to take a median of): 1/30 s.
const DEFAULT_LAST_DURATION: u32 = TIMESCALE / 30;

/// Opus in ISOBMFF runs its track at Opus's own 48 kHz whatever the input rate.
pub const OPUS_TIMESCALE: u32 = 48_000;
/// Audio packets buffered before they go out as a fragment of their own: half a second of
/// 20 ms packets.
const AUDIO_FLUSH_SAMPLES: usize = 25;

/// The 48 kHz samples an Opus packet decodes to, from its TOC byte: the configuration's
/// frame duration times the frame count its code carries.
pub fn opus_packet_samples(packet: &[u8]) -> u32 {
    let Some(&toc) = packet.first() else { return 0 };
    let config = toc >> 3;
    let tenths_ms = match config {
        0..=11 => [100, 200, 400, 600][(config % 4) as usize],
        12..=15 => [100, 200][(config % 2) as usize],
        _ => [25, 50, 100, 200][(config % 4) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => packet.get(1).map_or(0, |&b| (b & 0x3f) as u32),
    };
    frames * tenths_ms * OPUS_TIMESCALE / 10_000
}

/// Split an Annex-B elementary stream into NAL payloads (start codes removed, emulation
/// prevention bytes kept — the RBSP layer is only unescaped where a parser needs it).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0usize;
    let mut nal_start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let code_start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            if let Some(s) = nal_start
                && code_start > s
            {
                nals.push(&data[s..code_start]);
            }
            i += 3;
            nal_start = Some(i);
        } else if data[i + 2] == 0 {
            // A zero at i+2 can begin the next start code; only advance one byte.
            i += 1;
        } else {
            i += 3;
        }
    }
    if let Some(s) = nal_start
        && data.len() > s
    {
        nals.push(&data[s..]);
    }
    nals
}

/// Unescape an H.264 RBSP: drop the emulation-prevention byte from every `00 00 03` run.
fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0u32;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// MSB-first bit reader over an unescaped RBSP, with Exp-Golomb decode for SPS parsing.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    /// ue(v): count leading zeros, then read that many bits after the marker one.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some((1u32 << zeros) - 1 + rest)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 0 {
            -(k / 2) as i32
        } else {
            ((k + 1) / 2) as i32
        })
    }
}

/// Coded frame dimensions parsed out of an H.264 SPS NAL (with its NAL header byte).
///
/// Walks every field ahead of `pic_width_in_mbs_minus1` — including the high-profile
/// chroma/bit-depth/scaling-list block — and applies the frame-cropping rectangle with the
/// chroma-format-dependent crop units, so 4:2:0, 4:2:2, and 4:4:4 streams from any of the
/// project's encoders all report their true display size.
pub fn parse_sps_dimensions(sps_nal: &[u8]) -> Option<(u32, u32)> {
    if sps_nal.len() < 4 || sps_nal[0] & 0x1f != 7 {
        return None;
    }
    let profile_idc = sps_nal[1];
    let rbsp = unescape_rbsp(&sps_nal[4..]);
    let mut r = BitReader::new(&rbsp);
    // seq_parameter_set_id
    r.ue()?;

    let mut chroma_format_idc = 1u32;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            // separate_colour_plane_flag
            r.bit()?;
        }
        // bit_depth_luma_minus8, bit_depth_chroma_minus8, qpprime_y_zero_transform_bypass_flag
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut next_scale = 8i32;
                    let mut last_scale = 8i32;
                    for _ in 0..size {
                        if next_scale != 0 {
                            let delta = r.se()?;
                            next_scale = (last_scale + delta + 256) % 256;
                        }
                        if next_scale != 0 {
                            last_scale = next_scale;
                        }
                    }
                }
            }
        }
    }

    // log2_max_frame_num_minus4
    r.ue()?;
    let pic_order_cnt_type = r.ue()?;
    if pic_order_cnt_type == 0 {
        // log2_max_pic_order_cnt_lsb_minus4
        r.ue()?;
    } else if pic_order_cnt_type == 1 {
        // delta_pic_order_always_zero_flag, offset_for_non_ref_pic,
        // offset_for_top_to_bottom_field, then the offset_for_ref_frame list
        r.bit()?;
        r.se()?;
        r.se()?;
        let n = r.ue()?;
        for _ in 0..n {
            r.se()?;
        }
    }
    // max_num_ref_frames, gaps_in_frame_num_value_allowed_flag
    r.ue()?;
    r.bit()?;
    let pic_width_in_mbs = r.ue()? + 1;
    let pic_height_in_map_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        // mb_adaptive_frame_field_flag
        r.bit()?;
    }
    // direct_8x8_inference_flag
    r.bit()?;

    let (mut crop_l, mut crop_r, mut crop_t, mut crop_b) = (0u32, 0u32, 0u32, 0u32);
    if r.bit()? == 1 {
        crop_l = r.ue()?;
        crop_r = r.ue()?;
        crop_t = r.ue()?;
        crop_b = r.ue()?;
    }

    let (sub_w, sub_h) = match chroma_format_idc {
        0 | 3 => (1u32, 1u32),
        2 => (2, 1),
        _ => (2, 2),
    };
    let crop_unit_x = sub_w;
    let crop_unit_y = sub_h * (2 - frame_mbs_only);
    let width = pic_width_in_mbs * 16 - crop_unit_x * (crop_l + crop_r);
    let height =
        (2 - frame_mbs_only) * pic_height_in_map_units * 16 - crop_unit_y * (crop_t + crop_b);
    Some((width, height))
}

fn mk_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + payload.len());
    b.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    b.extend_from_slice(fourcc);
    b.extend_from_slice(payload);
    b
}

fn mk_full_box(fourcc: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + payload.len());
    p.push(version);
    p.extend_from_slice(&flags.to_be_bytes()[1..]);
    p.extend_from_slice(payload);
    mk_box(fourcc, &p)
}

/// The unity transformation matrix `mvhd` and `tkhd` carry: 0x00010000, 0x00010000, and
/// 0x40000000 on the diagonal (16.16 fixed point for the first two, 2.30 for the last).
const MATRIX_IDENTITY: [u8; 36] = {
    let mut m = [0u8; 36];
    m[1] = 0x01;
    m[17] = 0x01;
    m[32] = 0x40;
    m
};

/// A codec's contribution to the init segment: its `stsd` sample entry (with decoder
/// configuration record) plus the display dimensions for `tkhd`.
pub struct TrackConfig {
    pub sample_entry: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The audio track's `stsd` sample entry.
pub struct AudioTrackConfig {
    pub sample_entry: Vec<u8>,
}

/// The `Opus` sample entry with its `dOps` record, the head's own fields.
pub fn opus_sample_entry(head: &super::ogg::OpusHead) -> Vec<u8> {
    // AudioSampleEntry in field order: reserved, data_reference_index, reserved,
    // channelcount, samplesize, pre_defined, reserved, samplerate as 16.16.
    let mut entry_p = Vec::new();
    entry_p.extend_from_slice(&[0u8; 6]);
    entry_p.extend_from_slice(&1u16.to_be_bytes());
    entry_p.extend_from_slice(&[0u8; 8]);
    entry_p.extend_from_slice(&(head.channels as u16).to_be_bytes());
    entry_p.extend_from_slice(&16u16.to_be_bytes());
    entry_p.extend_from_slice(&[0u8; 4]);
    entry_p.extend_from_slice(&(OPUS_TIMESCALE << 16).to_be_bytes());
    // dOps in field order: Version, OutputChannelCount, PreSkip, InputSampleRate,
    // OutputGain, ChannelMappingFamily, then the stream and coupled counts and the
    // channel table for any family but 0.
    let mut dops_p = vec![0u8, head.channels];
    dops_p.extend_from_slice(&head.pre_skip.to_be_bytes());
    dops_p.extend_from_slice(&head.input_sample_rate.to_be_bytes());
    dops_p.extend_from_slice(&head.output_gain.to_be_bytes());
    dops_p.push(head.mapping_family);
    dops_p.extend_from_slice(&head.mapping);
    entry_p.extend_from_slice(&mk_box(b"dOps", &dops_p));
    mk_box(b"Opus", &entry_p)
}

/// One buffered access unit awaiting its duration (known when the next one arrives).
struct PendingSample {
    data: Vec<u8>,
    sync: bool,
    dts: u64,
}

/// Aggregate counters reported when the writer finishes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mp4Stats {
    pub samples: u64,
    pub sync_samples: u64,
    pub audio_samples: u64,
    pub bytes: u64,
    /// End of whichever track runs longer.
    pub duration_us: u64,
}

/// Codec-agnostic fMP4 fragment writer: `ftyp`+`moov` once, then one `moof`+`mdat` pair
/// per video sample, each anchored at its wall-clock decode time via `tfdt`, and one per
/// batch of audio samples.
pub struct FragmentWriter<W: Write> {
    out: W,
    seq: u32,
    wrote_init: bool,
    pending: Option<PendingSample>,
    last_dts: Option<u64>,
    /// Every flushed inter-frame duration, kept so the final buffered sample (whose
    /// successor never arrives) can close on the MEDIAN — damage-driven capture makes the
    /// last observed delta an outlier as often as not (a long static gap, a burst pair).
    durations: Vec<u32>,
    audio: Option<AudioTrackConfig>,
    /// Pending audio packets with their durations; contiguous from `audio_batch_dts`.
    audio_pending: Vec<(Vec<u8>, u32)>,
    audio_batch_dts: u64,
    audio_next_dts: u64,
    stats: Mp4Stats,
}

/// The data reference every track shares: an empty `url ` entry flagged self-contained.
fn dinf() -> Vec<u8> {
    let url = mk_full_box(b"url ", 0, 1, &[]);
    let mut dref_p = 1u32.to_be_bytes().to_vec();
    dref_p.extend_from_slice(&url);
    mk_box(b"dinf", &mk_full_box(b"dref", 0, 0, &dref_p))
}

/// A sample table holding only the sample entry: every sample lives in a fragment.
fn stbl(sample_entry: &[u8]) -> Vec<u8> {
    let mut stsd_p = 1u32.to_be_bytes().to_vec();
    stsd_p.extend_from_slice(sample_entry);
    let stsd = mk_full_box(b"stsd", 0, 0, &stsd_p);
    let stts = mk_full_box(b"stts", 0, 0, &0u32.to_be_bytes());
    let stsc = mk_full_box(b"stsc", 0, 0, &0u32.to_be_bytes());
    let stsz = mk_full_box(b"stsz", 0, 0, &[0u8; 8]);
    let stco = mk_full_box(b"stco", 0, 0, &0u32.to_be_bytes());
    mk_box(b"stbl", &[stsd, stts, stsc, stsz, stco].concat())
}

/// The `tkhd` fields that differ by media: a video track states its display size, an audio
/// track its volume.
enum TrakMedia {
    Video { width: u32, height: u32 },
    Audio { volume: u16 },
}

/// One `trak`: `tkhd` (enabled, in_movie) with the display size or, for audio, the volume;
/// `mdhd` at `timescale`; the handler; then the media header given and the sample table.
fn trak(
    track_id: u32,
    timescale: u32,
    handler: &[u8; 4],
    media_header: Vec<u8>,
    sample_entry: &[u8],
    media: TrakMedia,
) -> Vec<u8> {
    let (width, height, volume) = match media {
        TrakMedia::Video { width, height } => (width, height, 0),
        TrakMedia::Audio { volume } => (0, 0, volume),
    };
    // tkhd payload in field order: creation/modification time, track_ID, reserved, duration,
    // reserved, layer, alternate_group, volume, reserved, the unity matrix, then the 16.16
    // display width and height.
    let mut tkhd_p = Vec::new();
    tkhd_p.extend_from_slice(&[0u8; 8]);
    tkhd_p.extend_from_slice(&track_id.to_be_bytes());
    tkhd_p.extend_from_slice(&[0u8; 4]);
    tkhd_p.extend_from_slice(&0u32.to_be_bytes());
    tkhd_p.extend_from_slice(&[0u8; 12]);
    tkhd_p.extend_from_slice(&volume.to_be_bytes());
    tkhd_p.extend_from_slice(&[0u8; 2]);
    tkhd_p.extend_from_slice(&MATRIX_IDENTITY);
    tkhd_p.extend_from_slice(&(width << 16).to_be_bytes());
    tkhd_p.extend_from_slice(&(height << 16).to_be_bytes());
    let tkhd = mk_full_box(b"tkhd", 0, 3, &tkhd_p);

    // mdhd payload in field order: creation/modification time, timescale, duration,
    // language (0x55c4 = "und"), pre_defined.
    let mut mdhd_p = Vec::new();
    mdhd_p.extend_from_slice(&[0u8; 8]);
    mdhd_p.extend_from_slice(&timescale.to_be_bytes());
    mdhd_p.extend_from_slice(&0u32.to_be_bytes());
    mdhd_p.extend_from_slice(&0x55c4u16.to_be_bytes());
    mdhd_p.extend_from_slice(&[0u8; 2]);
    let mdhd = mk_full_box(b"mdhd", 0, 0, &mdhd_p);

    // hdlr payload in field order: pre_defined, the handler type, reserved, the name.
    let mut hdlr_p = Vec::new();
    hdlr_p.extend_from_slice(&[0u8; 4]);
    hdlr_p.extend_from_slice(handler);
    hdlr_p.extend_from_slice(&[0u8; 12]);
    hdlr_p.extend_from_slice(b"pixelflux\0");
    let hdlr = mk_full_box(b"hdlr", 0, 0, &hdlr_p);

    let minf = mk_box(
        b"minf",
        &[media_header, dinf(), stbl(sample_entry)].concat(),
    );
    let mdia = mk_box(b"mdia", &[mdhd, hdlr, minf].concat());
    mk_box(b"trak", &[tkhd, mdia].concat())
}

/// A `trex` whose fragments state their own durations, sizes, and flags unless
/// `default_duration` is set, which the audio track's fixed packet length is.
fn trex(track_id: u32, default_duration: u32) -> Vec<u8> {
    let mut trex_p = Vec::new();
    trex_p.extend_from_slice(&track_id.to_be_bytes());
    trex_p.extend_from_slice(&1u32.to_be_bytes());
    trex_p.extend_from_slice(&default_duration.to_be_bytes());
    trex_p.extend_from_slice(&[0u8; 8]);
    mk_full_box(b"trex", 0, 0, &trex_p)
}

impl<W: Write> FragmentWriter<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            seq: 0,
            wrote_init: false,
            pending: None,
            last_dts: None,
            durations: Vec::new(),
            audio: None,
            audio_pending: Vec::new(),
            audio_batch_dts: 0,
            audio_next_dts: 0,
            stats: Mp4Stats::default(),
        }
    }

    /// Declare the audio track; must precede `write_init`.
    pub fn with_audio(mut self, cfg: AudioTrackConfig) -> Self {
        self.audio = Some(cfg);
        self
    }

    pub fn init_written(&self) -> bool {
        self.wrote_init
    }

    /// Counters for the fragments written so far (the buffered pending sample is not yet
    /// included; `finish` folds it in).
    pub fn stats(&self) -> Mp4Stats {
        self.stats
    }

    /// Write `ftyp` + `moov` (the video track, the audio track when declared, and the
    /// `mvex`/`trex` marking the movie fragmented). Must be called once, before the first
    /// sample.
    pub fn write_init(&mut self, cfg: &TrackConfig) -> std::io::Result<()> {
        let mut ftyp_p = Vec::new();
        ftyp_p.extend_from_slice(b"isom");
        ftyp_p.extend_from_slice(&0x200u32.to_be_bytes());
        for brand in [b"isom", b"iso5", b"iso6", b"avc1", b"mp41"] {
            ftyp_p.extend_from_slice(brand);
        }
        let ftyp = mk_box(b"ftyp", &ftyp_p);

        // mvhd payload in field order: creation/modification time, timescale, duration
        // (0 = unknown, as it must be in a fragmented movie), rate 1.0, volume 1.0, reserved,
        // the unity matrix, pre_defined, next_track_ID.
        let mut mvhd_p = Vec::new();
        mvhd_p.extend_from_slice(&[0u8; 8]);
        mvhd_p.extend_from_slice(&TIMESCALE.to_be_bytes());
        mvhd_p.extend_from_slice(&0u32.to_be_bytes());
        mvhd_p.extend_from_slice(&0x00010000u32.to_be_bytes());
        mvhd_p.extend_from_slice(&0x0100u16.to_be_bytes());
        mvhd_p.extend_from_slice(&[0u8; 10]);
        mvhd_p.extend_from_slice(&MATRIX_IDENTITY);
        mvhd_p.extend_from_slice(&[0u8; 24]);
        mvhd_p.extend_from_slice(&(if self.audio.is_some() { 3u32 } else { 2u32 }).to_be_bytes());
        let mvhd = mk_full_box(b"mvhd", 0, 0, &mvhd_p);

        let vmhd = mk_full_box(b"vmhd", 0, 1, &[0u8; 8]);
        let mut traks = trak(
            1,
            TIMESCALE,
            b"vide",
            vmhd,
            &cfg.sample_entry,
            TrakMedia::Video {
                width: cfg.width,
                height: cfg.height,
            },
        );
        let mut mvex_p = trex(1, 0);
        if let Some(audio) = &self.audio {
            let smhd = mk_full_box(b"smhd", 0, 0, &[0u8; 4]);
            traks.extend(trak(
                2,
                OPUS_TIMESCALE,
                b"soun",
                smhd,
                &audio.sample_entry,
                TrakMedia::Audio { volume: 0x0100 },
            ));
            mvex_p.extend(trex(2, 0));
        }
        let mvex = mk_box(b"mvex", &mvex_p);

        let moov = mk_box(b"moov", &[mvhd, traks, mvex].concat());

        self.out.write_all(&ftyp)?;
        self.out.write_all(&moov)?;
        self.stats.bytes += (ftyp.len() + moov.len()) as u64;
        self.wrote_init = true;
        Ok(())
    }

    /// Queue one sample at `pts_us` (wall-clock microseconds since recording start),
    /// flushing the previously buffered sample with its now-known duration. Timestamps are
    /// clamped strictly monotonic so a repeated or reordered clock can never emit a
    /// zero/negative duration.
    pub fn push_sample(&mut self, data: Vec<u8>, sync: bool, pts_us: u64) -> std::io::Result<()> {
        let mut dts = pts_us * (TIMESCALE as u64 / 1000) / 1000;
        if let Some(last) = self.last_dts
            && dts <= last
        {
            dts = last + 1;
        }
        self.last_dts = Some(dts);
        if let Some(prev) = self.pending.take() {
            let duration = (dts - prev.dts).min(u32::MAX as u64) as u32;
            self.durations.push(duration);
            self.write_fragment(&prev, duration)?;
        }
        self.pending = Some(PendingSample { data, sync, dts });
        Ok(())
    }

    /// Median of the observed inter-frame durations (default 1/30 s when fewer than two
    /// frames were pushed), used to close the final sample.
    fn median_duration(&self) -> u32 {
        if self.durations.is_empty() {
            return DEFAULT_LAST_DURATION;
        }
        let mut sorted = self.durations.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    fn write_fragment(&mut self, s: &PendingSample, duration: u32) -> std::io::Result<()> {
        self.seq += 1;

        // tfhd carries only the track_ID; its flags set default-base-is-moof, so sample offsets
        // are relative to the start of this moof.
        let mut tfhd_p = Vec::new();
        tfhd_p.extend_from_slice(&1u32.to_be_bytes());
        let tfhd = mk_full_box(b"tfhd", 0, 0x020000, &tfhd_p);

        let mut tfdt_p = Vec::new();
        tfdt_p.extend_from_slice(&s.dts.to_be_bytes());
        let tfdt = mk_full_box(b"tfdt", 1, 0, &tfdt_p);

        // sample flags: sync = "depends on nothing"; non-sync also sets the non-sync bit.
        let sample_flags: u32 = if s.sync { 0x0200_0000 } else { 0x0101_0000 };
        // trun payload in field order: sample_count, a data_offset placeholder patched in below,
        // then this sample's duration, size, and flags — exactly the fields its flags select
        // (data-offset | sample-duration | sample-size | sample-flags).
        let mut trun_p = Vec::new();
        trun_p.extend_from_slice(&1u32.to_be_bytes());
        trun_p.extend_from_slice(&0i32.to_be_bytes());
        trun_p.extend_from_slice(&duration.to_be_bytes());
        trun_p.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
        trun_p.extend_from_slice(&sample_flags.to_be_bytes());
        let mut trun = mk_full_box(b"trun", 0, 0x000701, &trun_p);

        let traf_len = 8 + tfhd.len() + tfdt.len() + trun.len();
        // A moof box header is 8 bytes and the mfhd inside it 16.
        let moof_len = 8 + 16 + traf_len;
        // First sample byte sits just past the mdat header, relative to moof start.
        let data_offset = (moof_len + 8) as i32;
        // The data_offset field sits in the trun payload directly after sample_count.
        let off_pos = trun.len() - trun_p.len() + 4;
        trun[off_pos..off_pos + 4].copy_from_slice(&data_offset.to_be_bytes());

        let mfhd = mk_full_box(b"mfhd", 0, 0, &self.seq.to_be_bytes());
        let traf = mk_box(b"traf", &[tfhd, tfdt, trun].concat());
        let moof = mk_box(b"moof", &[mfhd, traf].concat());
        debug_assert_eq!(moof.len(), moof_len);

        self.out.write_all(&moof)?;
        self.out
            .write_all(&((8 + s.data.len()) as u32).to_be_bytes())?;
        self.out.write_all(b"mdat")?;
        self.out.write_all(&s.data)?;
        self.out.flush()?;

        self.stats.samples += 1;
        if s.sync {
            self.stats.sync_samples += 1;
        }
        self.stats.bytes += (moof.len() + 8 + s.data.len()) as u64;
        self.stats.duration_us = self
            .stats
            .duration_us
            .max((s.dts + duration as u64) * 1000 / (TIMESCALE as u64 / 1000));
        Ok(())
    }

    /// Queue one audio packet decoding at `dts` on the 48 kHz clock. A packet that does
    /// not follow the pending ones directly (a gap in the stream) closes their fragment
    /// first, so every fragment's samples are contiguous from its `tfdt`.
    pub fn push_audio(&mut self, data: Vec<u8>, dts: u64) -> std::io::Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        let samples = opus_packet_samples(&data);
        if !self.audio_pending.is_empty() && dts != self.audio_next_dts {
            self.flush_audio()?;
        }
        if self.audio_pending.is_empty() {
            self.audio_batch_dts = dts;
        }
        self.audio_pending.push((data, samples));
        self.audio_next_dts = dts + samples as u64;
        if self.audio_pending.len() >= AUDIO_FLUSH_SAMPLES {
            self.flush_audio()?;
        }
        Ok(())
    }

    /// Write the pending audio packets as one fragment of track 2, every sample a sync
    /// sample with its own duration and size.
    fn flush_audio(&mut self) -> std::io::Result<()> {
        if self.audio_pending.is_empty() {
            return Ok(());
        }
        self.seq += 1;
        let samples = std::mem::take(&mut self.audio_pending);

        // tfhd payload: track_ID and the default sample flags (sync) its flags select,
        // with default-base-is-moof.
        let mut tfhd_p = 2u32.to_be_bytes().to_vec();
        tfhd_p.extend_from_slice(&0x0200_0000u32.to_be_bytes());
        let tfhd = mk_full_box(b"tfhd", 0, 0x020020, &tfhd_p);
        let tfdt = mk_full_box(b"tfdt", 1, 0, &self.audio_batch_dts.to_be_bytes());
        // trun payload in field order: sample_count, a data_offset placeholder patched in
        // below, then each sample's duration and size (data-offset | sample-duration |
        // sample-size).
        let mut trun_p = Vec::new();
        trun_p.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        trun_p.extend_from_slice(&0i32.to_be_bytes());
        for (s, duration) in &samples {
            trun_p.extend_from_slice(&duration.to_be_bytes());
            trun_p.extend_from_slice(&(s.len() as u32).to_be_bytes());
        }
        let mut trun = mk_full_box(b"trun", 0, 0x000301, &trun_p);

        let traf_len = 8 + tfhd.len() + tfdt.len() + trun.len();
        let moof_len = 8 + 16 + traf_len;
        let data_offset = (moof_len + 8) as i32;
        let off_pos = trun.len() - trun_p.len() + 4;
        trun[off_pos..off_pos + 4].copy_from_slice(&data_offset.to_be_bytes());

        let mfhd = mk_full_box(b"mfhd", 0, 0, &self.seq.to_be_bytes());
        let traf = mk_box(b"traf", &[tfhd, tfdt, trun].concat());
        let moof = mk_box(b"moof", &[mfhd, traf].concat());
        debug_assert_eq!(moof.len(), moof_len);

        let data_len: usize = samples.iter().map(|(s, _)| s.len()).sum();
        self.out.write_all(&moof)?;
        self.out.write_all(&((8 + data_len) as u32).to_be_bytes())?;
        self.out.write_all(b"mdat")?;
        for (s, _) in &samples {
            self.out.write_all(s)?;
        }
        self.out.flush()?;
        self.stats.audio_samples += samples.len() as u64;
        self.stats.bytes += (moof.len() + 8 + data_len) as u64;
        let end = self.audio_batch_dts + samples.iter().map(|(_, d)| *d as u64).sum::<u64>();
        self.stats.duration_us = self
            .stats
            .duration_us
            .max(end * 1_000_000 / OPUS_TIMESCALE as u64);
        Ok(())
    }

    /// Flush the pending audio and the final buffered video sample, closed with the MEDIAN
    /// observed inter-frame duration (its successor never arrives), and return the
    /// aggregate counters.
    pub fn finish(mut self) -> std::io::Result<Mp4Stats> {
        self.flush_audio()?;
        if let Some(prev) = self.pending.take() {
            let d = self.median_duration();
            self.write_fragment(&prev, d)?;
        }
        self.out.flush()?;
        Ok(self.stats)
    }
}

/// H.264-specific front end: captures SPS/PPS from the stream, gates output on the first
/// IDR, converts Annex-B access units to AVCC samples, and builds the `avc1` sample entry.
pub struct H264SampleBuilder {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

/// One converted access unit ready for the fragment writer.
pub struct BuiltSample {
    pub data: Vec<u8>,
    pub sync: bool,
}

impl Default for H264SampleBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl H264SampleBuilder {
    pub fn new() -> Self {
        Self {
            sps: None,
            pps: None,
        }
    }

    pub fn have_parameter_sets(&self) -> bool {
        self.sps.is_some() && self.pps.is_some()
    }

    /// Convert one Annex-B access unit into a length-prefixed AVCC sample, harvesting
    /// SPS/PPS on the way. Returns `None` for an AU with no slice data (e.g. bare parameter
    /// sets). `sync` is true when the AU contains an IDR slice.
    pub fn build_sample(&mut self, annexb: &[u8]) -> Option<BuiltSample> {
        let nals = split_annexb(annexb);
        let mut data = Vec::with_capacity(annexb.len() + 8);
        let mut sync = false;
        let mut has_slice = false;
        for nal in nals {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1f {
                7 => {
                    if self.sps.as_deref() != Some(nal) {
                        self.sps = Some(nal.to_vec());
                    }
                }
                8 => {
                    if self.pps.as_deref() != Some(nal) {
                        self.pps = Some(nal.to_vec());
                    }
                }
                5 => {
                    sync = true;
                    has_slice = true;
                }
                1 => has_slice = true,
                _ => {}
            }
            data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            data.extend_from_slice(nal);
        }
        if !has_slice {
            return None;
        }
        Some(BuiltSample { data, sync })
    }

    /// Build the `avc1` sample entry + `avcC` record from the captured parameter sets, with
    /// the display dimensions parsed from the SPS.
    pub fn track_config(&self) -> Option<TrackConfig> {
        let sps = self.sps.as_deref()?;
        let pps = self.pps.as_deref()?;
        let (width, height) = parse_sps_dimensions(sps)?;

        // avcC header in field order: configurationVersion, AVCProfileIndication,
        // profile_compatibility, and AVCLevelIndication taken straight from the SPS,
        // lengthSizeMinusOne = 3 (the 4-byte NAL lengths this muxer writes), and
        // numOfSequenceParameterSets = 1. The SPS then the PPS follow, each length-prefixed.
        let mut avcc_p = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        avcc_p.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc_p.extend_from_slice(sps);
        // numOfPictureParameterSets
        avcc_p.push(1);
        avcc_p.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc_p.extend_from_slice(pps);
        let avcc = mk_box(b"avcC", &avcc_p);

        // avc1 sample entry in field order: reserved, data_reference_index, the
        // pre_defined/reserved block, width and height, horizontal and vertical resolution
        // (72 dpi), reserved, frame_count, compressorname, depth 24, pre_defined.
        let mut entry_p = Vec::new();
        entry_p.extend_from_slice(&[0u8; 6]);
        entry_p.extend_from_slice(&1u16.to_be_bytes());
        entry_p.extend_from_slice(&[0u8; 16]);
        entry_p.extend_from_slice(&(width as u16).to_be_bytes());
        entry_p.extend_from_slice(&(height as u16).to_be_bytes());
        entry_p.extend_from_slice(&0x0048_0000u32.to_be_bytes());
        entry_p.extend_from_slice(&0x0048_0000u32.to_be_bytes());
        entry_p.extend_from_slice(&[0u8; 4]);
        entry_p.extend_from_slice(&1u16.to_be_bytes());
        entry_p.extend_from_slice(&[0u8; 32]);
        entry_p.extend_from_slice(&0x0018u16.to_be_bytes());
        entry_p.extend_from_slice(&(-1i16).to_be_bytes());
        entry_p.extend_from_slice(&avcc);
        let entry = mk_box(b"avc1", &entry_p);

        Some(TrackConfig {
            sample_entry: entry,
            width,
            height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // x264 SPS at 1284x722 (High 4:2:0: cropping in both axes on odd-macroblock dims).
    const SPS_HIGH_1284X722: &str = "67640020acd9405105de788c0440000003004000000f03c60c6580";
    // x264 SPS at 640x360 (Constrained Baseline).
    const SPS_BASE_640X360: &str = "6742c01ed900a02ff970110000030001000003003c0f162e48";
    // x264 SPS at 1920x1080 (High 4:4:4 Predictive: chroma_format_idc == 3 path).
    const SPS_444_1920X1080: &str = "67f40028919b280f0044fc4e0220000003002000000781e30632c0";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Both start-code lengths delimit NALs; payloads come back exactly, with no framing.
    #[test]
    fn split_annexb_handles_mixed_start_codes() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 1, 0x67, 0xAA, 0xBB]);
        data.extend_from_slice(&[0, 0, 1, 0x68, 0xCC]);
        data.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x00, 0x00, 0x03, 0x01, 0xDD]);
        let nals = split_annexb(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 0xAA, 0xBB]);
        assert_eq!(nals[1], &[0x68, 0xCC]);
        assert_eq!(nals[2], &[0x65, 0x00, 0x00, 0x03, 0x01, 0xDD]);
    }

    /// Nothing to split yields no NALs, and a stream cut right after a start code
    /// yields no empty one either -- an empty payload would be written as a
    /// zero-length sample.
    #[test]
    fn split_annexb_emits_no_empty_nals() {
        assert!(split_annexb(&[]).is_empty());
        assert!(split_annexb(&[0x67, 0xAA, 0xBB]).is_empty());
        assert!(split_annexb(&[0, 0, 0, 1]).is_empty());
        assert_eq!(
            split_annexb(&[0, 0, 1, 0x68, 0, 0, 0, 1]),
            vec![&[0x68u8][..]]
        );
    }

    /// Dimensions from real x264 SPS across the profiles the project's encoders emit,
    /// including the frame-cropping and 4:4:4 chroma paths.
    #[test]
    fn sps_dimensions_across_profiles() {
        assert_eq!(
            parse_sps_dimensions(&hex(SPS_HIGH_1284X722)),
            Some((1284, 722))
        );
        assert_eq!(
            parse_sps_dimensions(&hex(SPS_BASE_640X360)),
            Some((640, 360))
        );
        assert_eq!(
            parse_sps_dimensions(&hex(SPS_444_1920X1080)),
            Some((1920, 1080))
        );
    }

    /// Annex-B -> AVCC: every NAL is length-prefixed, IDR marks sync, parameter sets are
    /// harvested, and a parameter-set-only AU yields no sample.
    #[test]
    fn annexb_to_avcc_sample() {
        let sps = hex(SPS_BASE_640X360);
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(&sps);
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 1, 2, 3, 4]);

        let mut b = H264SampleBuilder::new();
        assert!(
            b.build_sample(&[0u8, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1e, 0xd9])
                .is_none()
        );
        let s = b.build_sample(&au).expect("IDR AU builds a sample");
        assert!(s.sync);
        assert!(b.have_parameter_sets());
        // Sample = 3 length-prefixed NALs, sizes preserved.
        let mut off = 0usize;
        let mut sizes = Vec::new();
        while off < s.data.len() {
            let n = u32::from_be_bytes(s.data[off..off + 4].try_into().unwrap()) as usize;
            sizes.push(n);
            off += 4 + n;
        }
        assert_eq!(off, s.data.len());
        assert_eq!(sizes, vec![sps.len(), 4, 5]);

        let p = b.build_sample(&[0u8, 0, 0, 1, 0x41, 9, 9]).unwrap();
        assert!(!p.sync);
    }

    fn top_level(buf: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
            out.push((
                String::from_utf8_lossy(&buf[at + 4..at + 8]).into_owned(),
                buf[at + 8..at + size].to_vec(),
            ));
            at += size;
        }
        assert_eq!(at, buf.len(), "boxes tile the buffer");
        out
    }

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    fn idr_au() -> Vec<u8> {
        let mut au = vec![0, 0, 0, 1];
        au.extend_from_slice(&hex(SPS_BASE_640X360));
        au.extend_from_slice(&[
            0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80, 0, 0, 0, 1, 0x65, 1, 2, 3, 4,
        ]);
        au
    }

    /// Opus packet lengths from the TOC: 20 ms CELT fullband, 2.5 ms, a 60 ms SILK, and
    /// code-3 packets counting their frames.
    #[test]
    fn opus_packet_durations() {
        assert_eq!(opus_packet_samples(&[0xfc]), 960);
        assert_eq!(opus_packet_samples(&[0xe0]), 120);
        assert_eq!(opus_packet_samples(&[0x18]), 2880);
        assert_eq!(opus_packet_samples(&[0xfd]), 1920);
        assert_eq!(opus_packet_samples(&[0xff, 0x03]), 2880);
        assert_eq!(opus_packet_samples(&[]), 0);
    }

    /// With an audio track declared the init carries two `trak`s and two `trex`es and the
    /// `Opus` entry's `dOps` repeats the head; packets go out contiguously in audio
    /// fragments at the flush size, a gap in the stream closes a fragment early, and the
    /// fragments' decode times are the packets' own.
    #[test]
    fn audio_track_fragments() {
        let head = super::super::ogg::OpusHead {
            channels: 2,
            pre_skip: 312,
            input_sample_rate: 48000,
            output_gain: 0,
            mapping_family: 0,
            mapping: vec![],
        };
        let entry = opus_sample_entry(&head);
        assert_eq!(&entry[4..8], b"Opus");
        assert_eq!(count(&entry, b"dOps"), 1);
        let dops = entry.windows(4).position(|w| w == b"dOps").unwrap() + 4;
        assert_eq!(&entry[dops..dops + 4], &[0, 2, 0x01, 0x38]);

        let mut b = H264SampleBuilder::new();
        let sample = b.build_sample(&idr_au()).unwrap();
        let mut out = Vec::new();
        {
            let mut w = FragmentWriter::new(&mut out).with_audio(AudioTrackConfig {
                sample_entry: entry,
            });
            w.write_init(&b.track_config().unwrap()).unwrap();
            for i in 0..AUDIO_FLUSH_SAMPLES as u64 + 2 {
                w.push_audio(vec![0xfc, i as u8], 1000 + i * 960).unwrap();
            }
            // A gap of a second: the two pending packets close, the next opens a fragment.
            w.push_audio(vec![0xfc, 0xee], 1000 + 28 * 960 + 48_000)
                .unwrap();
            w.push_sample(sample.data.clone(), true, 0).unwrap();
            w.push_sample(sample.data, true, 33_000).unwrap();
            let st = w.finish().unwrap();
            assert_eq!(st.audio_samples, AUDIO_FLUSH_SAMPLES as u64 + 3);
            assert_eq!(st.samples, 2);
            assert_eq!(
                st.duration_us,
                (1000 + 28 * 960 + 48_000 + 960) * 1_000_000 / 48_000
            );
        }
        let boxes = top_level(&out);
        let kinds: Vec<&str> = boxes.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds[..2], ["ftyp", "moov"]);
        assert_eq!(count(&boxes[1].1, b"trak"), 2);
        assert_eq!(count(&boxes[1].1, b"trex"), 2);
        assert_eq!(count(&boxes[1].1, b"soun"), 1);
        assert_eq!(
            kinds.iter().filter(|k| **k == "moof").count(),
            5,
            "{kinds:?}"
        );
        let tfdts: Vec<u64> = boxes
            .iter()
            .filter(|(k, _)| k == "moof")
            .map(|(_, m)| {
                let at = m.windows(4).position(|w| w == b"tfdt").unwrap() + 8;
                u64::from_be_bytes(m[at..at + 8].try_into().unwrap())
            })
            .collect();
        // Audio: the full batch, then the two before the gap when the one after it
        // arrives; the first video sample goes out when the second arrives; the packet
        // after the gap and the last video sample at finish.
        assert_eq!(
            tfdts,
            vec![
                1000,
                1000 + AUDIO_FLUSH_SAMPLES as u64 * 960,
                0,
                1000 + 28 * 960 + 48_000,
                2970
            ]
        );
        let first_audio = &boxes[2].1;
        let trun = first_audio.windows(4).position(|w| w == b"trun").unwrap() + 8;
        assert_eq!(
            u32::from_be_bytes(first_audio[trun..trun + 4].try_into().unwrap()),
            AUDIO_FLUSH_SAMPLES as u32
        );
        assert_eq!(
            u32::from_be_bytes(first_audio[trun + 8..trun + 12].try_into().unwrap()),
            960
        );
    }

    /// Walk the top-level boxes of a finished two-sample stream: init once, then one
    /// moof+mdat pair per sample, with sizes that exactly tile the buffer.
    #[test]
    fn fragment_stream_box_layout() {
        let mut b = H264SampleBuilder::new();
        let mut au = Vec::new();
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(&hex(SPS_BASE_640X360));
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 1, 2, 3, 4]);
        let s1 = b.build_sample(&au).unwrap();
        let s2 = b.build_sample(&[0u8, 0, 0, 1, 0x41, 5, 6, 7]).unwrap();

        let mut buf = Vec::new();
        let mut w = FragmentWriter::new(&mut buf);
        w.write_init(&b.track_config().unwrap()).unwrap();
        w.push_sample(s1.data, s1.sync, 0).unwrap();
        w.push_sample(s2.data, s2.sync, 33_000).unwrap();
        let stats = w.finish().unwrap();
        assert_eq!(stats.samples, 2);
        assert_eq!(stats.sync_samples, 1);
        assert_eq!(stats.bytes as usize, buf.len());

        let mut kinds = Vec::new();
        let mut off = 0usize;
        while off < buf.len() {
            let size = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            kinds.push(buf[off + 4..off + 8].to_vec());
            assert!(size >= 8 && off + size <= buf.len());
            off += size;
        }
        assert_eq!(off, buf.len());
        let names: Vec<&str> = kinds
            .iter()
            .map(|k| std::str::from_utf8(k).unwrap())
            .collect();
        assert_eq!(names, vec!["ftyp", "moov", "moof", "mdat", "moof", "mdat"]);
    }

    /// Every trun sample_duration in stream order (the writer emits one sample per trun).
    fn trun_durations(buf: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        let mut off = 0usize;
        while off + 24 <= buf.len() {
            if &buf[off + 4..off + 8] == b"trun" {
                // [size][fourcc][ver+flags][sample_count][data_offset][duration]
                out.push(u32::from_be_bytes(
                    buf[off + 20..off + 24].try_into().unwrap(),
                ));
            }
            off += 1;
        }
        out
    }

    /// Each flushed sample's duration is the pts delta to its successor, and the final
    /// buffered sample closes with the MEDIAN observed duration — not the last delta, which
    /// under damage-driven capture is an outlier as often as not.
    #[test]
    fn sample_durations_are_pts_deltas_with_median_tail() {
        let mut buf = Vec::new();
        let mut w = FragmentWriter::new(&mut buf);
        let cfg = {
            let mut b = H264SampleBuilder::new();
            let mut au = vec![0, 0, 0, 1];
            au.extend_from_slice(&hex(SPS_BASE_640X360));
            au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80]);
            au.extend_from_slice(&[0, 0, 0, 1, 0x65, 1]);
            b.build_sample(&au);
            b.track_config().unwrap()
        };
        w.write_init(&cfg).unwrap();
        // 33 ms, 33 ms, then a 300 ms static gap before the final frame.
        w.push_sample(vec![0, 0, 0, 1, 0x65], true, 0).unwrap();
        w.push_sample(vec![0, 0, 0, 1, 0x41], false, 33_000)
            .unwrap();
        w.push_sample(vec![0, 0, 0, 1, 0x41], false, 66_000)
            .unwrap();
        w.push_sample(vec![0, 0, 0, 1, 0x41], false, 366_000)
            .unwrap();
        let stats = w.finish().unwrap();
        assert_eq!(stats.samples, 4);
        // 90 kHz ticks: 33 ms = 2970. The tail closes at the median (2970), not 27000.
        assert_eq!(trun_durations(&buf), vec![2970, 2970, 27_000, 2970]);
    }

    /// A single-frame recording still closes with a sane nonzero duration.
    #[test]
    fn single_sample_uses_default_duration() {
        let mut buf = Vec::new();
        let mut w = FragmentWriter::new(&mut buf);
        let cfg = {
            let mut b = H264SampleBuilder::new();
            let mut au = vec![0, 0, 0, 1];
            au.extend_from_slice(&hex(SPS_BASE_640X360));
            au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80]);
            au.extend_from_slice(&[0, 0, 0, 1, 0x65, 1]);
            b.build_sample(&au);
            b.track_config().unwrap()
        };
        w.write_init(&cfg).unwrap();
        w.push_sample(vec![0, 0, 0, 1, 0x65], true, 0).unwrap();
        let stats = w.finish().unwrap();
        assert_eq!(stats.samples, 1);
        assert_eq!(trun_durations(&buf), vec![DEFAULT_LAST_DURATION]);
    }

    /// Non-monotonic wall-clock input is clamped to strictly increasing decode times, so no
    /// fragment can carry a zero or negative duration.
    #[test]
    fn pts_clamped_strictly_monotonic() {
        let mut buf = Vec::new();
        let mut w = FragmentWriter::new(&mut buf);
        let cfg = {
            let mut b = H264SampleBuilder::new();
            let mut au = vec![0, 0, 0, 1];
            au.extend_from_slice(&hex(SPS_BASE_640X360));
            au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80]);
            au.extend_from_slice(&[0, 0, 0, 1, 0x65, 1]);
            b.build_sample(&au);
            b.track_config().unwrap()
        };
        w.write_init(&cfg).unwrap();
        w.push_sample(vec![0, 0, 0, 1, 0x65], true, 1000).unwrap();
        // The second sample repeats the clock and the third winds it backwards.
        w.push_sample(vec![0, 0, 0, 1, 0x41], false, 1000).unwrap();
        w.push_sample(vec![0, 0, 0, 1, 0x41], false, 500).unwrap();
        let stats = w.finish().unwrap();
        assert_eq!(stats.samples, 3);

        // Extract each tfdt baseMediaDecodeTime and check strict monotonicity.
        let mut times = Vec::new();
        let mut off = 0usize;
        while off + 8 <= buf.len() {
            if &buf[off + 4..off + 8] == b"tfdt" {
                let t = u64::from_be_bytes(buf[off + 12..off + 20].try_into().unwrap());
                times.push(t);
            }
            off += 1;
        }
        assert_eq!(times.len(), 3);
        assert!(
            times.windows(2).all(|w| w[1] > w[0]),
            "tfdt times: {times:?}"
        );
    }
}
