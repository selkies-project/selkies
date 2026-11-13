//! The session against the stood-in driver: what it asks the driver for, what it renders
//! for a key frame and a delta, how each codec names the frame a picture predicts from once
//! a client has lost one, and what the video processor is told to convert to.

use super::mock::{self, Driver};
use super::*;
use crate::encoders::codec::{FRAME_DELTA, FRAME_KEY, parse_video_type};
use crate::encoders::sps::fixtures::{VCE_SPS, assert_no_reorder};
use crate::encoders::sps::{
    ColorSignal, h264_frame_num_range, h264_max_num_ref_frames, h264_reorder, h264_timing,
    read_color,
};

const W: i32 = 320;
const H: i32 = 240;

fn settings(codec: Codec, cbr: bool) -> CaptureSettings {
    CaptureSettings {
        width: W,
        height: H,
        target_fps: 30.0,
        codec,
        video_crf: 25,
        video_cbr_mode: cbr,
        video_bitrate_kbps: 4000,
        ..Default::default()
    }
}

/// The stood-in driver on a placeholder node.
fn device() -> Arc<Device> {
    let node = std::fs::File::open("/dev/null").unwrap();
    Arc::new(Device::on(&mock::api(), node.into(), "stand-in").unwrap())
}

fn open(codec: Codec, settings: &CaptureSettings) -> Result<VaapiEncoder, String> {
    VaapiEncoder::on_device(device(), settings, codec, Input::Host { rgba: false })
}

fn session(codec: Codec, cbr: bool) -> VaapiEncoder {
    open(codec, &settings(codec, cbr)).unwrap_or_else(|e| panic!("{codec:?}: {e}"))
}

fn frame() -> Vec<u8> {
    vec![0x40; (W * H * 4) as usize]
}

fn encode(enc: &mut VaapiEncoder, t: u64, key: bool) -> Vec<u8> {
    enc.encode_host(&frame(), (W * 4) as usize, false, t, 25, key)
        .unwrap_or_else(|e| panic!("frame {t}: {e}"))
}

/// A most-significant-bit-first reader for the headers the checks parse back.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u(&mut self, count: u32) -> u32 {
        (0..count).fold(0, |acc, _| {
            let bit = (self.bytes[self.pos / 8] >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            (acc << 1) | u32::from(bit)
        })
    }

    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.u(1) == 0 {
            zeros += 1;
        }
        (1 << zeros) - 1 + self.u(zeros)
    }

    fn se(&mut self) -> i32 {
        let k = self.ue() as i32;
        if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) }
    }
}

/// The RBSP of one NAL unit of `kind` in an Annex-B stream, unescaped.
fn nal(stream: &[u8], kind: u8, h265: bool) -> Vec<u8> {
    let unit = crate::encoders::codec::annexb_nals(stream)
        .find(|n| {
            (if h265 {
                (n[0] >> 1) & 0x3f
            } else {
                n[0] & 0x1f
            }) == kind
        })
        .unwrap_or_else(|| panic!("no NAL unit of type {kind}"));
    let payload = &unit[if h265 { 2 } else { 1 }..];
    let mut out = Vec::new();
    let mut zeros = 0;
    for &b in payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Every codec comes up on the stood-in driver, is listed by the probe, and asks the driver
/// for what its arm needs: the surface format, the rate-control mode, the packed headers it
/// writes itself, and contexts over the reconstruction surfaces and the converted one.
#[test]
fn every_codec_comes_up_and_asks_for_what_it_needs() {
    for codec in Codec::VIDEO {
        mock::reset(Driver::generous());
        let (served, fullcolor): (Vec<Codec>, Vec<bool>) = probe_codecs_on(&device())
            .unwrap()
            .into_iter()
            .map(|(codec, formats)| (codec, formats.fullcolor))
            .unzip();
        assert_eq!(served, Codec::VIDEO.to_vec());
        assert_eq!(
            fullcolor,
            [false, false, true, false, true],
            "HEVC Main 4:4:4 and VP9 profile 1 carry 4:4:4"
        );
        let enc = session(codec, false);
        assert_eq!(enc.codec(), codec);
        assert!(!enc.is_fullcolor() && !enc.is_full_range());
        assert!(
            enc.low_power(),
            "{codec:?} takes the low-power entry point offered"
        );
        mock::with(|d| {
            let (profile, entrypoint, attribs) = &d.configs[0];
            assert_eq!(*entrypoint, VAEntrypointEncSliceLP);
            assert_eq!(*profile, profile_ladder(codec, false, 8)[0], "{codec:?}");
            let value = |kind| attribs.iter().find(|a| a.type_ == kind).map(|a| a.value);
            assert_eq!(value(VAConfigAttribRTFormat), Some(VA_RT_FORMAT_YUV420));
            assert_eq!(value(VAConfigAttribRateControl), Some(VA_RC_CQP));
            let packed = match codec {
                Codec::H264 | Codec::H265 => {
                    Some(VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_SLICE)
                }
                Codec::Av1 => Some(VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE),
                _ => None,
            };
            assert_eq!(value(VAConfigAttribEncPackedHeaders), packed, "{codec:?}");
            assert_eq!(
                d.configs[1].0, VAProfileNone,
                "the video processor's configuration"
            );
            let recon = if codec == Codec::Vp8 {
                4
            } else {
                REFERENCE_FRAMES as usize + 1
            };
            assert_eq!(
                d.contexts[0].1.len(),
                recon + 1,
                "{codec:?}: the encode context over the reconstruction and converted surfaces"
            );
            assert_eq!(
                d.contexts[1].1.len(),
                1,
                "the processing context over the converted surface"
            );
        });
    }
}

/// Every codec takes the capture's rate as the fraction it names: the frame-rate buffer packs
/// it into its two 16-bit terms, the nearest fraction that fits where the NTSC one does not,
/// and the H.264 and HEVC sequences declare it in their timing.
#[test]
fn the_frame_rate_reaches_every_codec_as_its_fraction() {
    for (num, den) in [(60000u32, 1001u32), (120000, 1001), (144000, 1001), (60, 1)] {
        let fps = f64::from(num) / f64::from(den);
        let fit = FrameRate { num, den }.within(0xffff);
        for codec in Codec::VIDEO {
            mock::reset(Driver::generous());
            let mut enc = open(
                codec,
                &CaptureSettings {
                    target_fps: fps,
                    ..settings(codec, true)
                },
            )
            .unwrap_or_else(|e| panic!("{codec:?}: {e}"));
            encode(&mut enc, 0, true);
            mock::with(|d| {
                let (_, bytes) = d
                    .last_misc()
                    .into_iter()
                    .find(|m| m.0 == VAEncMiscParameterTypeFrameRate)
                    .expect("a frame rate");
                let fr: VAEncMiscParameterFrameRate =
                    unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                assert_eq!(
                    (fr.framerate & 0xffff, fr.framerate >> 16),
                    (fit.num, fit.den),
                    "{codec:?} at {num}/{den}"
                );
                let headers = || {
                    d.last_packed()
                        .into_iter()
                        .find(|p| p.0 == VAEncPackedHeaderSequence)
                        .unwrap()
                        .1
                };
                match codec {
                    Codec::H264 => {
                        let s: VAEncSequenceParameterBufferH264 =
                            d.last_param(VAEncSequenceParameterBufferType).unwrap();
                        assert_eq!(
                            (s.num_units_in_tick, s.time_scale),
                            (den, 2 * num),
                            "{num}/{den}"
                        );
                        assert_eq!(
                            h264_timing(&headers()),
                            Some((den, 2 * num)),
                            "{num}/{den}: the packed SPS"
                        );
                    }
                    Codec::H265 => {
                        let vps = nal(&headers(), 32, true);
                        let mut r = Reader {
                            bytes: &vps,
                            pos: 0,
                        };
                        for _ in 0..4 {
                            r.u(32);
                        }
                        r.u(1);
                        r.ue();
                        r.ue();
                        r.ue();
                        r.u(6);
                        r.ue();
                        assert_eq!(r.u(1), 1, "vps_timing_info_present_flag");
                        assert_eq!((r.u(32), r.u(32)), (den, num), "{num}/{den}: the VPS");
                    }
                    _ => {}
                }
            });
        }
    }
}

/// A key frame renders the sequence parameters, the rate control the session holds, and the
/// packed sequence header; a delta renders the picture alone. Every frame converts first.
#[test]
fn a_key_frame_carries_the_sequence_and_a_delta_does_not() {
    for cbr in [false, true] {
        for codec in Codec::VIDEO {
            mock::reset(Driver::generous());
            let mut enc = session(codec, cbr);
            let first = encode(&mut enc, 0, true);
            assert_eq!(
                parse_video_type(first[1]),
                Some((codec, FRAME_KEY)),
                "{codec:?}"
            );
            mock::with(|d| {
                assert_eq!(d.pictures.len(), 2, "{codec:?}: a convert and an encode");
                assert_ne!(
                    d.pictures[0].0, d.pictures[1].0,
                    "the convert and the encode render on their own contexts"
                );
                assert!(
                    d.contexts[1].1.contains(&d.pictures[0].1),
                    "the convert targets the converted surface"
                );
                assert!(
                    d.contexts[0].1.contains(&d.pictures[1].1),
                    "the encode targets a reconstruction surface"
                );
                assert_eq!(
                    d.last_buffers(VAEncSequenceParameterBufferType).len(),
                    1,
                    "{codec:?}"
                );
                let misc: Vec<u32> = d.last_misc().into_iter().map(|m| m.0).collect();
                let mut wanted = if cbr {
                    vec![VAEncMiscParameterTypeRateControl, VAEncMiscParameterTypeHRD]
                } else {
                    vec![]
                };
                wanted.extend([
                    VAEncMiscParameterTypeFrameRate,
                    VAEncMiscParameterTypeQualityLevel,
                ]);
                assert_eq!(misc, wanted, "{codec:?} cbr={cbr}");
                for (kind, bytes) in d.last_misc() {
                    if kind == VAEncMiscParameterTypeRateControl {
                        let rc: VAEncMiscParameterRateControl =
                            unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                        assert_eq!((rc.bits_per_second, rc.target_percentage), (4_000_000, 100));
                        assert_eq!(rc.window_size, 50, "1.5 frames of VBV at 30 fps, in ms");
                        assert_eq!(unsafe { rc.rc_flags.bits.mb_rate_control() }, 2);
                        assert_eq!(
                            unsafe { rc.rc_flags.bits.disable_bit_stuffing() },
                            1,
                            "no filler data"
                        );
                    }
                    if kind == VAEncMiscParameterTypeHRD {
                        let hrd: VAEncMiscParameterHRD =
                            unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                        assert_eq!(
                            (hrd.buffer_size, hrd.initial_buffer_fullness),
                            (200_000, 200_000)
                        );
                    }
                    if kind == VAEncMiscParameterTypeFrameRate {
                        let fr: VAEncMiscParameterFrameRate =
                            unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                        assert_eq!(fr.framerate, (1 << 16) | 30);
                    }
                    if kind == VAEncMiscParameterTypeQualityLevel {
                        let q: VAEncMiscParameterBufferQualityLevel =
                            unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                        assert_eq!(q.quality_level, 7, "the driver's fastest level");
                    }
                }
                let packed: Vec<u32> = d.last_packed().into_iter().map(|p| p.0).collect();
                let wanted: Vec<u32> = match codec {
                    Codec::H264 | Codec::H265 => [
                        vec![VAEncPackedHeaderSequence],
                        vec![VAEncPackedHeaderSlice; 4],
                    ]
                    .concat(),
                    Codec::Av1 => vec![VAEncPackedHeaderSequence, VAEncPackedHeaderPicture],
                    _ => vec![],
                };
                assert_eq!(packed, wanted, "{codec:?}");
                assert_eq!(d.last_buffers(VAEncPictureParameterBufferType).len(), 1);
                let slices = d.last_buffers(VAEncSliceParameterBufferType).len();
                assert_eq!(
                    slices,
                    match codec {
                        Codec::H264 | Codec::H265 => 4,
                        Codec::Av1 => 1,
                        _ => 0,
                    },
                    "{codec:?}"
                );
            });
            let second = encode(&mut enc, 1, false);
            assert_eq!(
                parse_video_type(second[1]),
                Some((codec, FRAME_DELTA)),
                "{codec:?}"
            );
            mock::with(|d| {
                assert_eq!(d.pictures.len(), 4);
                assert!(
                    d.last_buffers(VAEncSequenceParameterBufferType).is_empty(),
                    "{codec:?}: a delta repeats no sequence"
                );
                assert!(
                    d.last_misc().is_empty(),
                    "{codec:?}: a delta repeats no rate control"
                );
                assert_eq!(d.last_buffers(VAEncPictureParameterBufferType).len(), 1);
            });
        }
    }
}

/// A key frame forced mid-stream, which a client requests after a settings change, starts the
/// counts a codec keeps from the key over again: an H.264 IDR carries `frame_num` 0 and the
/// delta after it counts 1 from there, an HEVC IDR the picture order count 0, an AV1 key
/// frame the order hint 0. A decoder given an IDR numbered from the previous key refuses the
/// prediction that follows it.
#[test]
fn a_key_frame_forced_mid_stream_restarts_the_count() {
    for codec in [Codec::H264, Codec::H265, Codec::Av1] {
        mock::reset(Driver::generous());
        let mut enc = session(codec, false);
        for t in 0..6u64 {
            encode(&mut enc, t, t == 0);
        }
        let out = encode(&mut enc, 6, true);
        assert_eq!(
            parse_video_type(out[1]),
            Some((codec, FRAME_KEY)),
            "{codec:?}"
        );
        assert_eq!(enc.last_reference(), Reference::None);
        mock::with(|d| match codec {
            Codec::H264 => {
                let pic: VAEncPictureParameterBufferH264 =
                    d.last_param(VAEncPictureParameterBufferType).unwrap();
                assert_eq!(
                    (
                        pic.CurrPic.frame_idx,
                        pic.CurrPic.TopFieldOrderCnt,
                        pic.frame_num
                    ),
                    (0, 0, 0)
                );
                let header = d
                    .last_packed()
                    .into_iter()
                    .find(|p| p.0 == VAEncPackedHeaderSlice)
                    .unwrap()
                    .1;
                let rbsp = nal(&header, 5, false);
                let mut r = Reader {
                    bytes: &rbsp,
                    pos: 0,
                };
                assert_eq!(r.ue(), 0, "first_mb_in_slice");
                assert_eq!(r.ue(), 7, "slice_type I");
                assert_eq!(r.ue(), 0, "pps id");
                assert_eq!(r.u(16), 0, "frame_num of an IDR");
                assert_eq!(r.ue(), 1, "idr_pic_id of the second IDR");
            }
            Codec::H265 => {
                let pic: VAEncPictureParameterBufferHEVC =
                    d.last_param(VAEncPictureParameterBufferType).unwrap();
                assert_eq!(pic.decoded_curr_pic.pic_order_cnt, 0);
            }
            _ => {
                let pic: VAEncPictureParameterBufferAV1 =
                    d.last_param(VAEncPictureParameterBufferType).unwrap();
                assert_eq!(pic.order_hint, 0);
            }
        });
        let out = encode(&mut enc, 7, false);
        assert_eq!(
            parse_video_type(out[1]),
            Some((codec, FRAME_DELTA)),
            "{codec:?}"
        );
        assert_eq!(enc.last_reference(), Reference::Frame(6));
        mock::with(|d| match codec {
            Codec::H264 => {
                let pic: VAEncPictureParameterBufferH264 =
                    d.last_param(VAEncPictureParameterBufferType).unwrap();
                assert_eq!((pic.CurrPic.frame_idx, pic.frame_num), (1, 1));
                let held: Vec<u32> = pic
                    .ReferenceFrames
                    .iter()
                    .filter(|r| r.flags != VA_PICTURE_H264_INVALID)
                    .map(|r| r.frame_idx)
                    .collect();
                assert_eq!(held, [0], "only the new IDR is held");
                let header = d
                    .last_packed()
                    .into_iter()
                    .find(|p| p.0 == VAEncPackedHeaderSlice)
                    .unwrap()
                    .1;
                let rbsp = nal(&header, 1, false);
                let mut r = Reader {
                    bytes: &rbsp,
                    pos: 0,
                };
                assert_eq!(r.ue(), 0, "first_mb_in_slice");
                assert_eq!(r.ue(), 5, "slice_type P");
                assert_eq!(r.ue(), 0, "pps id");
                assert_eq!(r.u(16), 1, "frame_num counted from the new IDR");
                assert_eq!(r.u(1), 0, "num_ref_idx_active_override_flag");
                assert_eq!(r.u(1), 0, "the IDR is the newest frame, so the list stands");
            }
            Codec::H265 => {
                let pic: VAEncPictureParameterBufferHEVC =
                    d.last_param(VAEncPictureParameterBufferType).unwrap();
                assert_eq!(pic.decoded_curr_pic.pic_order_cnt, 1);
                let kept: Vec<i32> = pic
                    .reference_frames
                    .iter()
                    .filter(|r| r.flags != VA_PICTURE_HEVC_INVALID)
                    .map(|r| r.pic_order_cnt)
                    .collect();
                assert_eq!(kept, [0]);
            }
            _ => {
                let pic: VAEncPictureParameterBufferAV1 =
                    d.last_param(VAEncPictureParameterBufferType).unwrap();
                assert_eq!(pic.order_hint, 1);
            }
        });
    }
}

/// H.264 keeps the decoded picture buffer the level admits, names the newest surviving frame
/// in the slice header once a client lost one, and codes a key frame once the loss reaches
/// past the buffer. What the session writes into the SPS is what the crate's own reader and
/// the loss bookkeeping read back.
#[test]
fn h264_names_the_newest_surviving_frame_in_its_slice_header() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::H264, false);
    let first = encode(&mut enc, 0, true);
    let stream = &first[VIDEO_HEADER_LEN..];
    assert_eq!(h264_max_num_ref_frames(stream), Some(REFERENCE_FRAMES));
    assert_eq!(h264_frame_num_range(stream), Some(65536));
    assert_no_reorder(stream, "the session's own SPS");
    assert_eq!(
        read_color(
            crate::encoders::codec::annexb_nals(stream)
                .find(|n| n[0] & 0x1f == 7)
                .unwrap()
        ),
        Some(ColorSignal::BT709_LIMITED)
    );
    assert_eq!(enc.last_reference(), Reference::None);
    for t in 1..8u64 {
        encode(&mut enc, t, false);
        assert_eq!(enc.last_reference(), Reference::Frame(t as u16 - 1));
    }
    assert!(enc.invalidate_reference(5));
    let out = encode(&mut enc, 8, false);
    assert_eq!(parse_video_type(out[1]), Some((Codec::H264, FRAME_DELTA)));
    assert_eq!(enc.last_reference(), Reference::Frame(4));
    mock::with(|d| {
        let pic: VAEncPictureParameterBufferH264 =
            d.last_param(VAEncPictureParameterBufferType).unwrap();
        assert_eq!(pic.CurrPic.frame_idx, 8);
        let held: Vec<u32> = pic
            .ReferenceFrames
            .iter()
            .filter(|r| r.flags != VA_PICTURE_H264_INVALID)
            .map(|r| r.frame_idx)
            .collect();
        assert_eq!(
            held,
            [7, 6, 5, 4, 3, 2, 1, 0],
            "every frame the decoder holds, newest first"
        );
        let slices = d.last_buffers(VAEncSliceParameterBufferType);
        assert_eq!(slices.len(), 4);
        let slice: VAEncSliceParameterBufferH264 =
            unsafe { ptr::read_unaligned(slices[0].as_ptr() as *const _) };
        assert_eq!(
            slice.RefPicList0[0].frame_idx, 4,
            "the slice predicts from frame 4"
        );
        assert_eq!(slice.slice_type, 0);
        let header = d
            .last_packed()
            .into_iter()
            .find(|p| p.0 == VAEncPackedHeaderSlice)
            .unwrap()
            .1;
        let rbsp = nal(&header, 1, false);
        let mut r = Reader {
            bytes: &rbsp,
            pos: 0,
        };
        assert_eq!(r.ue(), 0, "first_mb_in_slice");
        assert_eq!(r.ue(), 5, "slice_type P");
        assert_eq!(r.ue(), 0, "pps id");
        assert_eq!(r.u(16), 8, "frame_num");
        assert_eq!(r.u(1), 0, "num_ref_idx_active_override_flag");
        assert_eq!(r.u(1), 1, "ref_pic_list_modification_flag_l0");
        assert_eq!(r.ue(), 0, "modification_of_pic_nums_idc: subtract");
        assert_eq!(r.ue(), 3, "abs_diff_pic_num_minus1: 8 - 4 - 1");
        assert_eq!(r.ue(), 3, "end of the modification");
        assert_eq!(r.u(1), 0, "adaptive_ref_pic_marking_mode_flag");
        assert_eq!(r.ue(), 0, "cabac_init_idc");
        let qp = Codec::H264.hardware_quantizer(Hardware::Vaapi, 25) as i32;
        assert_eq!(r.se(), qp - 26, "slice_qp_delta");
    });
    encode(&mut enc, 9, false);
    assert_eq!(enc.last_reference(), Reference::Frame(8));
    mock::with(|d| {
        let header = d
            .last_packed()
            .into_iter()
            .find(|p| p.0 == VAEncPackedHeaderSlice)
            .unwrap()
            .1;
        let rbsp = nal(&header, 1, false);
        let mut r = Reader {
            bytes: &rbsp,
            pos: 0,
        };
        r.ue();
        r.ue();
        r.ue();
        r.u(16);
        r.u(1);
        assert_eq!(r.u(1), 0, "the previous frame is the default reference");
    });
    for t in 10..20u64 {
        encode(&mut enc, t, false);
    }
    assert!(enc.invalidate_reference(9));
    let out = encode(&mut enc, 20, false);
    assert_eq!(
        parse_video_type(out[1]),
        Some((Codec::H264, FRAME_KEY)),
        "a loss past the buffer costs a key frame"
    );
    assert_eq!(enc.last_reference(), Reference::None);
    mock::with(|d| {
        assert_eq!(
            d.last_buffers(VAEncSequenceParameterBufferType).len(),
            1,
            "the key frame repeats the sequence"
        )
    });
}

/// On a driver that codes a picture from the reference it is named (radeonsi), an H.264 session
/// keeps a long-term anchor: the key frame goes out marked long-term under index 0, the anchor
/// the schedule marks later under the same index by memory management operation 6, and a frame
/// predicting past a loss older than the recent frames names it in its reference list
/// modification and in `RefPicList0`, both long-term, as `ReferenceFrames` lists it.
#[test]
fn h264_keeps_a_long_term_anchor_where_the_driver_takes_one() {
    let mut driver = Driver::generous();
    driver.vendor = Some(c"Mesa Gallium driver for AMD Radeon Pro VII (radeonsi, vega20)");
    mock::reset(driver);
    let mut enc = session(Codec::H264, false);
    let slice_header = |kind: u8| {
        mock::with(|d| {
            let header = d
                .last_packed()
                .into_iter()
                .find(|p| p.0 == VAEncPackedHeaderSlice)
                .unwrap()
                .1;
            nal(&header, kind, false)
        })
    };
    encode(&mut enc, 0, true);
    let rbsp = slice_header(5);
    let mut r = Reader {
        bytes: &rbsp,
        pos: 0,
    };
    assert_eq!((r.ue(), r.ue(), r.ue(), r.u(16)), (0, 7, 0, 0));
    r.ue();
    assert_eq!(r.u(1), 0, "no_output_of_prior_pics_flag");
    assert_eq!(
        r.u(1),
        1,
        "long_term_reference_flag: the key frame is the first anchor"
    );

    encode(&mut enc, 1, false);
    assert_eq!(enc.last_reference(), Reference::Frame(0));
    let rbsp = slice_header(1);
    let mut r = Reader {
        bytes: &rbsp,
        pos: 0,
    };
    assert_eq!((r.ue(), r.ue(), r.ue(), r.u(16), r.u(1)), (0, 5, 0, 1, 0));
    assert_eq!(
        (r.u(1), r.ue(), r.ue(), r.ue()),
        (1, 2, 0, 3),
        "the key frame named by its long-term index"
    );
    assert_eq!(r.u(1), 0, "adaptive_ref_pic_marking_mode_flag");

    for t in 2..=48u64 {
        encode(&mut enc, t, false);
    }
    let rbsp = slice_header(1);
    let mut r = Reader {
        bytes: &rbsp,
        pos: 0,
    };
    assert_eq!((r.ue(), r.ue(), r.ue(), r.u(16), r.u(1)), (0, 5, 0, 48, 0));
    assert_eq!(
        r.u(1),
        0,
        "48 predicts from 47, the newest short-term frame"
    );
    assert_eq!(
        (r.u(1), r.ue(), r.ue(), r.ue()),
        (1, 6, 0, 0),
        "48 marked long-term under index 0, the key frame leaving"
    );
    mock::with(|d| {
        let pic: VAEncPictureParameterBufferH264 =
            d.last_param(VAEncPictureParameterBufferType).unwrap();
        assert_eq!(
            (pic.CurrPic.flags, pic.CurrPic.frame_idx),
            (VA_PICTURE_H264_LONG_TERM_REFERENCE, 0)
        );
    });

    for t in 49..=60u64 {
        encode(&mut enc, t, false);
    }
    assert!(enc.invalidate_reference(50));
    encode(&mut enc, 61, false);
    assert_eq!(
        enc.last_reference(),
        Reference::Frame(48),
        "a loss older than the recent frames is predicted past from the anchor"
    );
    let rbsp = slice_header(1);
    let mut r = Reader {
        bytes: &rbsp,
        pos: 0,
    };
    assert_eq!((r.ue(), r.ue(), r.ue(), r.u(16), r.u(1)), (0, 5, 0, 61, 0));
    assert_eq!((r.u(1), r.ue(), r.ue(), r.ue()), (1, 2, 0, 3));
    assert_eq!(r.u(1), 0, "adaptive_ref_pic_marking_mode_flag");
    mock::with(|d| {
        let pic: VAEncPictureParameterBufferH264 =
            d.last_param(VAEncPictureParameterBufferType).unwrap();
        let long_term: Vec<u32> = pic
            .ReferenceFrames
            .iter()
            .filter(|r| r.flags == VA_PICTURE_H264_LONG_TERM_REFERENCE)
            .map(|r| r.frame_idx)
            .collect();
        assert_eq!(long_term, [0], "the anchor, by its long-term index");
        let short_term = pic
            .ReferenceFrames
            .iter()
            .filter(|r| r.flags == VA_PICTURE_H264_SHORT_TERM_REFERENCE)
            .count();
        assert_eq!(short_term, REFERENCE_FRAMES as usize - 1);
        let slices = d.last_buffers(VAEncSliceParameterBufferType);
        let slice: VAEncSliceParameterBufferH264 =
            unsafe { ptr::read_unaligned(slices[0].as_ptr() as *const _) };
        assert_eq!(
            (slice.RefPicList0[0].flags, slice.RefPicList0[0].frame_idx),
            (VA_PICTURE_H264_LONG_TERM_REFERENCE, 0)
        );
    });
}

/// On radeonsi an H.265 session keeps two anchors as short-term pictures: a frame predicting
/// past a loss older than the recent frames names the newest anchor before it in `RefPicList0`,
/// and every picture lists the anchors in its reference frames (the slice header's reference
/// picture set). Elsewhere the session keeps the recent frames alone, and such a loss is a key
/// frame.
#[test]
fn h265_keeps_two_anchors_where_the_driver_takes_them() {
    for (vendor, anchored) in [
        (
            Some(c"Mesa Gallium driver for AMD Radeon Pro VII (radeonsi, vega20)"),
            true,
        ),
        (None, false),
    ] {
        let mut driver = Driver::generous();
        driver.vendor = vendor;
        mock::reset(driver);
        let mut enc = session(Codec::H265, false);
        for t in 0..=60u64 {
            encode(&mut enc, t, t == 0);
        }
        let listed = || {
            mock::with(|d| {
                let pic: VAEncPictureParameterBufferHEVC =
                    d.last_param(VAEncPictureParameterBufferType).unwrap();
                let mut pocs: Vec<i32> = pic
                    .reference_frames
                    .iter()
                    .filter(|r| r.picture_id != VA_INVALID_ID)
                    .map(|r| r.pic_order_cnt)
                    .collect();
                pocs.sort_unstable();
                pocs
            })
        };
        assert_eq!(
            listed().contains(&48),
            anchored,
            "{vendor:?}: 48, an anchor, listed at 60: {:?}",
            listed()
        );
        enc.invalidate_reference(50);
        let out = encode(&mut enc, 61, false);
        let (_, kind) = parse_video_type(out[1]).unwrap();
        assert_eq!(
            kind == FRAME_KEY,
            !anchored,
            "{vendor:?}: a loss older than the recent frames"
        );
        if !anchored {
            continue;
        }
        assert_eq!(
            enc.last_reference(),
            Reference::Frame(48),
            "predicted past from the newest anchor before the loss"
        );
        mock::with(|d| {
            let slices = d.last_buffers(VAEncSliceParameterBufferType);
            let slice: VAEncSliceParameterBufferHEVC =
                unsafe { ptr::read_unaligned(slices[0].as_ptr() as *const _) };
            assert_eq!(
                (
                    slice.ref_pic_list0[0].pic_order_cnt,
                    slice.ref_pic_list0[0].flags
                ),
                (48, VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE)
            );
        });
        let pocs = listed();
        assert!(
            pocs.contains(&48) && !pocs.iter().any(|&p| (50..=60).contains(&p)),
            "the anchor kept, the lost frames and those after them left out: {pocs:?}"
        );
    }
}

/// A session told to keep one reference frame (`video_reference_frames`) declares a decoded
/// picture buffer of one in H.264 and H.265, and keeps no anchor even where the driver takes one.
#[test]
fn a_session_keeps_the_reference_frames_it_is_given() {
    let mut driver = Driver::generous();
    driver.vendor = Some(c"Mesa Gallium driver for AMD Radeon Pro VII (radeonsi, vega20)");
    mock::reset(driver);
    let mut s = settings(Codec::H264, false);
    s.video_reference_frames = 1;
    let mut enc = open(Codec::H264, &s).unwrap();
    let first = encode(&mut enc, 0, true);
    assert_eq!(h264_max_num_ref_frames(&first[VIDEO_HEADER_LEN..]), Some(1));
    let rbsp = mock::with(|d| {
        let header = d
            .last_packed()
            .into_iter()
            .find(|p| p.0 == VAEncPackedHeaderSlice)
            .unwrap()
            .1;
        nal(&header, 5, false)
    });
    let mut r = Reader {
        bytes: &rbsp,
        pos: 0,
    };
    assert_eq!((r.ue(), r.ue(), r.ue(), r.u(16)), (0, 7, 0, 0));
    r.ue();
    assert_eq!(r.u(1), 0, "no_output_of_prior_pics_flag");
    assert_eq!(
        r.u(1),
        0,
        "no long-term key frame: one frame leaves no room for an anchor"
    );
    let mut s = settings(Codec::H265, false);
    s.video_reference_frames = 1;
    let mut enc = open(Codec::H265, &s).unwrap();
    encode(&mut enc, 0, true);
    assert_eq!(enc.negotiated.dpb, 1, "sps_max_dec_pic_buffering_minus1");
}

/// HEVC lists the frames the decoder keeps in every slice header's reference picture set,
/// the one it predicts from marked as used, and drops a lost frame from the set.
#[test]
fn hevc_lists_the_kept_frames_in_its_reference_picture_set() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::H265, false);
    for t in 0..8u64 {
        let out = encode(&mut enc, t, t == 0);
        assert_eq!(
            parse_video_type(out[1]),
            Some((Codec::H265, if t == 0 { FRAME_KEY } else { FRAME_DELTA }))
        );
    }
    assert!(enc.invalidate_reference(5));
    encode(&mut enc, 8, false);
    assert_eq!(enc.last_reference(), Reference::Frame(4));
    mock::with(|d| {
        let pic: VAEncPictureParameterBufferHEVC =
            d.last_param(VAEncPictureParameterBufferType).unwrap();
        assert_eq!(pic.decoded_curr_pic.pic_order_cnt, 8);
        let kept: Vec<(i32, u32)> = pic
            .reference_frames
            .iter()
            .filter(|r| r.flags != VA_PICTURE_HEVC_INVALID)
            .map(|r| (r.pic_order_cnt, r.flags))
            .collect();
        assert_eq!(
            kept,
            [
                (4, VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE),
                (3, 0),
                (2, 0),
                (1, 0),
                (0, 0)
            ],
            "the lost frames are not kept"
        );
        let slices = d.last_buffers(VAEncSliceParameterBufferType);
        let slice: VAEncSliceParameterBufferHEVC =
            unsafe { ptr::read_unaligned(slices[0].as_ptr() as *const _) };
        assert_eq!(slice.ref_pic_list0[0].pic_order_cnt, 4);
        assert_eq!(slice.slice_type, 1);
        let header = d
            .last_packed()
            .into_iter()
            .find(|p| p.0 == VAEncPackedHeaderSlice)
            .unwrap()
            .1;
        let rbsp = nal(&header, 1, true);
        let mut r = Reader {
            bytes: &rbsp,
            pos: 0,
        };
        assert_eq!(r.u(1), 1, "first_slice_segment_in_pic_flag");
        assert_eq!(r.ue(), 0, "pps id");
        assert_eq!(r.ue(), 1, "slice_type P");
        assert_eq!(r.u(12), 8, "poc lsb");
        assert_eq!(r.u(1), 0, "short_term_ref_pic_set_sps_flag");
        assert_eq!(r.ue(), 5, "num_negative_pics");
        assert_eq!(r.ue(), 0, "num_positive_pics");
        let mut previous = 8;
        for (poc, used) in [(4, 1), (3, 0), (2, 0), (1, 0), (0, 0)] {
            assert_eq!(
                r.ue(),
                (previous - poc - 1) as u32,
                "delta_poc_s0_minus1 of {poc}"
            );
            assert_eq!(r.u(1), used, "used_by_curr_pic_s0_flag of {poc}");
            previous = poc;
        }
    });
}

/// VP9 refreshes the slot of each frame's timestamp and predicts from the slot of the newest
/// surviving frame, every frame error-resilient.
#[test]
fn vp9_addresses_its_slots_by_timestamp() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::Vp9, false);
    for t in 0..8u64 {
        encode(&mut enc, t, t == 0);
        mock::with(|d| {
            let pic: VAEncPictureParameterBufferVP9 =
                d.last_param(VAEncPictureParameterBufferType).unwrap();
            assert_eq!(
                pic.refresh_frame_flags,
                if t == 0 { 0xff } else { 1 << t },
                "frame {t}"
            );
            assert_eq!(unsafe { pic.pic_flags.bits.error_resilient_mode() }, 1);
            if t > 0 {
                assert_eq!(unsafe { pic.ref_flags.bits.ref_last_idx() }, t as u32 - 1);
                assert_ne!(pic.reference_frames[t as usize - 1], VA_INVALID_SURFACE);
            }
        });
    }
    assert!(enc.invalidate_reference(5));
    encode(&mut enc, 8, false);
    assert_eq!(enc.last_reference(), Reference::Frame(4));
    mock::with(|d| {
        let pic: VAEncPictureParameterBufferVP9 =
            d.last_param(VAEncPictureParameterBufferType).unwrap();
        assert_eq!(unsafe { pic.ref_flags.bits.ref_last_idx() }, 4);
        assert_eq!(pic.refresh_frame_flags, 1 << 0, "timestamp 8 takes slot 0");
        assert_eq!(
            pic.luma_ac_qindex,
            Codec::Vp9.hardware_quantizer(Hardware::Vaapi, 25) as u8
        );
    });
}

/// VP8 follows the buffer plan: LAST every frame, the golden and altref anchors on their
/// schedules, no entropy update ever, and a recovery from an anchor once LAST is lost.
#[test]
fn vp8_follows_the_slot_plan() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::Vp8, false);
    let mut refreshes = Vec::new();
    for t in 0..25u64 {
        encode(&mut enc, t, t == 0);
        mock::with(|d| {
            let pic: VAEncPictureParameterBufferVP8 =
                d.last_param(VAEncPictureParameterBufferType).unwrap();
            let p = unsafe { pic.pic_flags.bits };
            assert_eq!(p.refresh_entropy_probs(), 0, "frame {t}");
            refreshes.push((
                p.refresh_last(),
                p.refresh_golden_frame(),
                p.refresh_alternate_frame(),
            ));
            if t > 0 {
                let r = unsafe { pic.ref_flags.bits };
                assert_eq!(
                    (r.no_ref_last(), r.no_ref_gf(), r.no_ref_arf()),
                    (0, 1, 1),
                    "frame {t} predicts from LAST"
                );
            }
        });
    }
    assert_eq!(
        refreshes[0],
        (1, 1, 1),
        "the key frame refreshes every buffer"
    );
    assert_eq!(refreshes[1], (1, 0, 0));
    assert_eq!(refreshes[12], (1, 0, 1), "frame 12 is an altref anchor");
    assert_eq!(refreshes[24], (1, 1, 0), "frame 24 is a golden anchor");
    assert!(enc.invalidate_reference(12));
    let out = encode(&mut enc, 25, false);
    assert_eq!(
        parse_video_type(out[1]),
        Some((Codec::Vp8, FRAME_KEY)),
        "a loss from frame 12 on takes LAST and GOLDEN (24) and ALTREF (12)"
    );
    for t in 26..40u64 {
        encode(&mut enc, t, false);
    }
    assert!(enc.invalidate_reference(38));
    encode(&mut enc, 40, false);
    assert_eq!(
        enc.last_reference(),
        Reference::Frame(37),
        "the altref anchor of frame 37, older than the loss"
    );
    mock::with(|d| {
        let pic: VAEncPictureParameterBufferVP8 =
            d.last_param(VAEncPictureParameterBufferType).unwrap();
        let r = unsafe { pic.ref_flags.bits };
        let p = unsafe { pic.pic_flags.bits };
        assert_eq!(
            (r.no_ref_last(), r.no_ref_gf(), r.no_ref_arf()),
            (1, 1, 0),
            "the recovery predicts from ALTREF alone"
        );
        assert_eq!(
            (
                p.refresh_last(),
                p.refresh_golden_frame(),
                p.refresh_alternate_frame()
            ),
            (1, 1, 1),
            "the recovery refreshes every buffer"
        );
    });
}

/// AV1 names the slot of the frame it predicts from as every reference and as the source of
/// its probability contexts, in the picture parameters and in the frame header it packs.
#[test]
fn av1_names_its_reference_slot_in_the_frame_header() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::Av1, true);
    for t in 0..8u64 {
        let out = encode(&mut enc, t, t == 0);
        assert_eq!(
            parse_video_type(out[1]),
            Some((Codec::Av1, if t == 0 { FRAME_KEY } else { FRAME_DELTA })),
            "frame {t}"
        );
    }
    assert!(enc.invalidate_reference(5));
    encode(&mut enc, 8, false);
    assert_eq!(enc.last_reference(), Reference::Frame(4));
    mock::with(|d| {
        let pic: VAEncPictureParameterBufferAV1 =
            d.last_param(VAEncPictureParameterBufferType).unwrap();
        assert_eq!(pic.ref_frame_idx, [4; 7]);
        assert_eq!(pic.primary_ref_frame, 0);
        assert_eq!(pic.refresh_frame_flags, 1 << 0);
        assert_eq!(pic.order_hint, 8);
        assert_ne!(pic.reference_frames[4], VA_INVALID_SURFACE);
        assert_eq!(
            pic.byte_offset_frame_hdr_obu_size, 1,
            "a delta's frame header is first in its picture"
        );
        let header = d
            .last_packed()
            .into_iter()
            .find(|p| p.0 == VAEncPackedHeaderPicture)
            .unwrap()
            .1;
        assert_eq!(header[0], 0x1a, "a frame header OBU with a size field");
        let mut r = Reader {
            bytes: &header,
            pos: 8 * (1 + 4),
        };
        assert_eq!(r.u(1), 0, "show_existing_frame");
        assert_eq!(r.u(2), 1, "frame_type inter");
        assert_eq!(r.u(1), 1, "show_frame");
        assert_eq!(r.u(1), 0, "error_resilient_mode");
        assert_eq!(r.u(1), 0, "disable_cdf_update");
        assert_eq!(r.u(1), 0, "frame_size_override_flag");
        assert_eq!(r.u(8), 8, "order_hint");
        assert_eq!(r.u(3), 0, "primary_ref_frame");
        assert_eq!(r.u(8), 1, "refresh_frame_flags");
        assert_eq!(r.u(1), 0, "frame_refs_short_signaling");
        for i in 0..7 {
            assert_eq!(r.u(3), 4, "ref_frame_idx[{i}]");
        }
        assert_eq!(r.u(1), 1, "render_and_frame_size_different");
        assert_eq!(r.u(16), (W - 1) as u32, "render_width_minus_1");
        assert_eq!(r.u(16), (H - 1) as u32, "render_height_minus_1");
        assert_eq!(
            pic.bit_offset_qindex as usize,
            r.pos + 1 + 1 + 2 + 1 + 1 + 1 + 1 + 1,
            "the quantizer follows allow_high_precision_mv, is_filter_switchable, interpolation_filter, \
             is_motion_mode_switchable, disable_frame_end_update_cdf, uniform_tile_spacing_flag, and one tile increment bit each for \
             columns and rows, with no use_ref_frame_mvs in a sequence without reference motion vectors"
        );
        r.pos = pic.bit_offset_qindex as usize;
        assert_eq!(
            r.u(8),
            128,
            "a constant-rate session's quantizer, which the driver rewrites"
        );
        assert_eq!(pic.size_in_bits_frame_hdr_obu as usize, 8 * header.len());
    });
    mock::reset(Driver::generous());
    let mut enc = session(Codec::Av1, true);
    encode(&mut enc, 0, true);
    mock::with(|d| {
        let pic: VAEncPictureParameterBufferAV1 =
            d.last_param(VAEncPictureParameterBufferType).unwrap();
        let sequence = d
            .last_packed()
            .into_iter()
            .find(|p| p.0 == VAEncPackedHeaderSequence)
            .unwrap()
            .1;
        assert_eq!(
            pic.byte_offset_frame_hdr_obu_size as usize,
            sequence.len() + 1,
            "a key frame's header follows the sequence header"
        );
        assert_eq!(sequence[0], 0x0a, "a sequence header OBU with a size field");
        assert_eq!(pic.refresh_frame_flags, 0xff);
        assert_eq!(pic.primary_ref_frame, 7);
    });
}

/// The video processor is told the sRGB source it converts from and the matrix and range the
/// session declares, explicitly where the driver takes it, with chroma sited at the block
/// center.
#[test]
fn the_video_processor_is_told_the_declared_color() {
    for codec in [Codec::H264, Codec::Vp8] {
        mock::reset(Driver::generous());
        let mut enc = session(codec, false);
        encode(&mut enc, 0, true);
        mock::with(|d| {
            let convert = &d.pictures[0];
            let bytes = &d.buffers[convert.2[0]];
            assert_eq!(bytes.1, VAProcPipelineParameterBufferType);
            let p: VAProcPipelineParameterBuffer =
                unsafe { ptr::read_unaligned(bytes.2.as_ptr() as *const _) };
            assert_eq!(
                (p.surface_color_standard, p.output_color_standard),
                (VAProcColorStandardExplicit, VAProcColorStandardExplicit)
            );
            assert_eq!(
                (
                    p.input_color_properties.color_range,
                    p.input_color_properties.matrix_coefficients
                ),
                (VA_SOURCE_RANGE_FULL as u8, 0)
            );
            assert_eq!(p.input_color_properties.colour_primaries, 1);
            assert_eq!(
                p.output_color_properties.color_range,
                VA_SOURCE_RANGE_REDUCED as u8
            );
            assert_eq!(
                p.output_color_properties.matrix_coefficients,
                if codec == Codec::Vp8 { 6 } else { 1 },
                "{codec:?}"
            );
            assert_eq!(
                p.output_color_properties.chroma_sample_location,
                (VA_CHROMA_SITING_VERTICAL_CENTER | VA_CHROMA_SITING_HORIZONTAL_CENTER) as u8
            );
            assert_eq!(p.filter_flags, VA_FRAME_PICTURE);
        });
    }
    let mut classic = Driver::generous();
    classic.color_standards = vec![
        VAProcColorStandardBT601,
        VAProcColorStandardBT709,
        VAProcColorStandardSMPTE170M,
    ];
    mock::reset(classic);
    let mut enc = session(Codec::Vp8, false);
    encode(&mut enc, 0, true);
    mock::with(|d| {
        let bytes = &d.buffers[d.pictures[0].2[0]];
        let p: VAProcPipelineParameterBuffer =
            unsafe { ptr::read_unaligned(bytes.2.as_ptr() as *const _) };
        assert_eq!(
            (p.surface_color_standard, p.output_color_standard),
            (VAProcColorStandardBT709, VAProcColorStandardBT601)
        );
    });
}

/// A still screen's cleanup frame capped at `bytes` (`VaapiEncoder::cap`) is coded at the rate
/// that spends them in the cleanup's frame time, its buffer the session's in time, with the
/// picture and no new sequence; the frame after goes back to the session's rate, and the one
/// after that carries none.
#[test]
fn a_capped_cleanup_frame_carries_its_rate_and_the_next_the_sessions() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::H264, true);
    let rate_control = || {
        mock::with(|d| {
            d.last_misc()
                .into_iter()
                .find(|m| m.0 == VAEncMiscParameterTypeRateControl)
                .map(|m| unsafe {
                    ptr::read_unaligned(m.1.as_ptr() as *const VAEncMiscParameterRateControl)
                })
        })
    };
    encode(&mut enc, 0, true);
    let session_rc = rate_control().expect("a key frame carries the rate control");
    encode(&mut enc, 1, false);
    assert!(rate_control().is_none());
    enc.cap(400_000);
    let out = encode(&mut enc, 2, false);
    assert_eq!(parse_video_type(out[1]), Some((Codec::H264, FRAME_DELTA)));
    let cleanup_rc = rate_control().expect("the capped frame carries its rate");
    assert_eq!(cleanup_rc.bits_per_second, 16_000_000);
    assert_eq!(cleanup_rc.window_size, session_rc.window_size);
    encode(&mut enc, 3, false);
    assert_eq!(
        rate_control().map(|rc| rc.bits_per_second),
        Some(session_rc.bits_per_second)
    );
    encode(&mut enc, 4, false);
    assert!(rate_control().is_none());
}

/// A constant-rate target that moved alone reaches the next picture with its rate control and no
/// new sequence; a frame-rate change opens a new sequence, a key frame carrying the new rate
/// control; a quality change in constant-quantizer mode reaches the next frame as it is.
#[test]
fn a_target_change_rides_the_next_picture_and_a_frame_rate_change_restarts_the_sequence() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::H264, true);
    let rate_control = || {
        mock::with(|d| {
            d.last_misc()
                .into_iter()
                .find(|m| m.0 == VAEncMiscParameterTypeRateControl)
                .map(|m| unsafe {
                    ptr::read_unaligned(m.1.as_ptr() as *const VAEncMiscParameterRateControl)
                })
        })
    };
    encode(&mut enc, 0, true);
    encode(&mut enc, 1, false);
    let mut s = settings(Codec::H264, true);
    s.video_bitrate_kbps = 8000;
    enc.reconfigure_rate(&s).unwrap();
    let out = encode(&mut enc, 2, false);
    assert_eq!(parse_video_type(out[1]), Some((Codec::H264, FRAME_DELTA)));
    assert_eq!(rate_control().map(|rc| rc.bits_per_second), Some(8_000_000));
    encode(&mut enc, 3, false);
    assert!(rate_control().is_none());
    s.target_fps = 60.0;
    enc.reconfigure_rate(&s).unwrap();
    let out = encode(&mut enc, 4, false);
    assert_eq!(parse_video_type(out[1]), Some((Codec::H264, FRAME_KEY)));
    assert_eq!(rate_control().map(|rc| rc.bits_per_second), Some(8_000_000));
    mock::reset(Driver::generous());
    let mut enc = session(Codec::H264, false);
    encode(&mut enc, 0, true);
    let out = enc
        .encode_host(&frame(), (W * 4) as usize, false, 1, 40, false)
        .unwrap();
    assert_eq!(parse_video_type(out[1]), Some((Codec::H264, FRAME_DELTA)));
    mock::with(|d| {
        let header = d
            .last_packed()
            .into_iter()
            .find(|p| p.0 == VAEncPackedHeaderSlice)
            .unwrap()
            .1;
        let rbsp = nal(&header, 1, false);
        let mut r = Reader {
            bytes: &rbsp,
            pos: 0,
        };
        r.ue();
        r.ue();
        r.ue();
        r.u(16);
        r.u(1);
        r.u(1);
        r.u(1);
        r.ue();
        let qp = Codec::H264.hardware_quantizer(Hardware::Vaapi, 40) as i32;
        assert_eq!(r.se(), qp - 26, "the new quantizer reaches the slice");
    });
}

/// A host frame lands on the surface through its derived image where the driver derives one,
/// else through an image the driver copies in.
#[test]
fn host_frames_upload_through_the_derived_image_or_a_put() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::H264, false);
    encode(&mut enc, 0, true);
    encode(&mut enc, 1, false);
    mock::with(|d| {
        assert_eq!(d.puts, 0);
        assert_eq!(
            d.images.len(),
            3,
            "the derive probe and one derived image per frame"
        );
    });
    let mut copying = Driver::generous();
    copying.derive = false;
    mock::reset(copying);
    let mut enc = session(Codec::H264, false);
    encode(&mut enc, 0, true);
    encode(&mut enc, 1, false);
    mock::with(|d| {
        assert_eq!(d.puts, 2);
        assert_eq!(d.images.len(), 1, "one image created and put twice");
    });
}

/// A driver that writes its own slice headers names the references it chooses, so the
/// session tracks none and refuses an invalidation; one without the codec refuses the
/// session.
#[test]
fn a_driver_writing_its_own_headers_tracks_no_reference() {
    let mut own_headers = Driver::generous();
    own_headers
        .attributes
        .retain(|a| a.0 != VAConfigAttribEncPackedHeaders);
    mock::reset(own_headers);
    for codec in [Codec::H264, Codec::H265] {
        let mut enc = session(codec, false);
        let first = encode(&mut enc, 0, true);
        assert_eq!(parse_video_type(first[1]), Some((codec, FRAME_KEY)));
        let second = encode(&mut enc, 1, false);
        assert_eq!(parse_video_type(second[1]), Some((codec, FRAME_DELTA)));
        assert_eq!(enc.last_reference(), Reference::Untracked);
        assert!(!enc.invalidate_reference(0));
        mock::with(|d| {
            assert!(
                d.last_packed().is_empty(),
                "no packed header goes to a driver that takes none"
            )
        });
    }
    let mut enc = session(Codec::Vp9, false);
    encode(&mut enc, 0, true);
    encode(&mut enc, 1, false);
    assert_eq!(
        enc.last_reference(),
        Reference::Frame(0),
        "VP9's references need no packed header"
    );

    let mut no_av1 = Driver::generous();
    no_av1.profiles.retain(|&p| p != VAProfileAV1Profile0);
    mock::reset(no_av1);
    let refused = open(Codec::Av1, &settings(Codec::Av1, false));
    assert!(refused.err().unwrap().contains("encodes no AV1"));
    let served: Vec<Codec> = probe_codecs_on(&device())
        .unwrap()
        .into_iter()
        .map(|(codec, ..)| codec)
        .collect();
    assert_eq!(served, [Codec::H264, Codec::Vp8, Codec::Vp9, Codec::H265]);
}

/// A 4:4:4 session takes the planar surface where the driver renders it, the packed one
/// where that is all it renders, and refuses where it renders neither.
#[test]
fn fullcolor_takes_the_surface_the_driver_renders() {
    for (rendered, wanted) in [
        (
            vec![
                VA_FOURCC_NV12,
                VA_FOURCC_444P,
                VA_FOURCC_XYUV,
                VA_FOURCC_BGRA,
            ],
            Some("yuv444p"),
        ),
        (
            vec![VA_FOURCC_NV12, VA_FOURCC_XYUV, VA_FOURCC_BGRA],
            Some("vuyx"),
        ),
        (vec![VA_FOURCC_NV12, VA_FOURCC_BGRA], None),
    ] {
        let mut driver = Driver::generous();
        driver.surface_fourccs = rendered;
        mock::reset(driver);
        let mut s = settings(Codec::H265, false);
        s.video_fullcolor = true;
        match open(Codec::H265, &s) {
            Ok(enc) => {
                assert!(enc.is_fullcolor());
                assert_eq!(Some(enc.surface_format_name().as_str()), wanted);
                mock::with(|d| assert_eq!(d.configs[0].0, VAProfileHEVCMain444));
            }
            Err(e) => assert!(wanted.is_none(), "{e}"),
        }
    }
}

/// A session at `width` x `height` and `fps` on the stood-in driver.
fn sized(codec: Codec, cbr: bool, width: i32, height: i32, fps: f64) -> CaptureSettings {
    CaptureSettings {
        width,
        height,
        target_fps: fps,
        ..settings(codec, cbr)
    }
}

fn encode_sized(enc: &mut VaapiEncoder, width: i32, height: i32, t: u64, key: bool) -> Vec<u8> {
    let pixels = vec![0x40; (width * height * 4) as usize];
    enc.encode_host(&pixels, (width * 4) as usize, false, t, 25, key)
        .unwrap_or_else(|e| panic!("frame {t}: {e}"))
}

/// A driver that writes its own slice headers still takes each picture's number, order count,
/// reconstruction surface, and reference from the session, so a session that tracks no
/// references counts its frames all the same: both counts advance every frame, the previous
/// frame's surface is the reference, and a forced key frame restarts the counts. Such a driver
/// predicts from the previous frame alone, so the session declares a one-frame buffer and
/// alternates two reconstruction surfaces.
#[test]
fn a_session_tracking_no_references_counts_its_frames() {
    for codec in [Codec::H264, Codec::H265] {
        let mut own_slices = Driver::generous();
        own_slices
            .attributes
            .retain(|a| a.0 != VAConfigAttribEncPackedHeaders);
        own_slices.attributes.push((
            VAConfigAttribEncPackedHeaders,
            VA_ENC_PACKED_HEADER_SEQUENCE,
        ));
        mock::reset(own_slices);
        let mut enc = session(codec, false);
        let mut recon: Vec<VASurfaceID> = Vec::new();
        for t in 0..24u64 {
            let key = t == 0 || t == 20;
            let out = encode(&mut enc, t, key);
            assert_eq!(
                parse_video_type(out[1]),
                Some((codec, if key { FRAME_KEY } else { FRAME_DELTA })),
                "{codec:?} frame {t}"
            );
            let count = if t >= 20 { t - 20 } else { t };
            let (current, reference) = mock::with(|d| {
                let slices = d.last_buffers(VAEncSliceParameterBufferType);
                if codec == Codec::H264 {
                    let pic: VAEncPictureParameterBufferH264 =
                        d.last_param(VAEncPictureParameterBufferType).unwrap();
                    let slice: VAEncSliceParameterBufferH264 =
                        unsafe { ptr::read_unaligned(slices[0].as_ptr() as *const _) };
                    assert_eq!(
                        (
                            u64::from(pic.frame_num),
                            pic.CurrPic.TopFieldOrderCnt as u64
                        ),
                        (count, 2 * count),
                        "frame {t}: frame_num and order count"
                    );
                    (
                        pic.CurrPic.picture_id,
                        (!key).then(|| {
                            (
                                slice.RefPicList0[0].picture_id,
                                u64::from(slice.RefPicList0[0].frame_idx),
                            )
                        }),
                    )
                } else {
                    let pic: VAEncPictureParameterBufferHEVC =
                        d.last_param(VAEncPictureParameterBufferType).unwrap();
                    let slice: VAEncSliceParameterBufferHEVC =
                        unsafe { ptr::read_unaligned(slices[0].as_ptr() as *const _) };
                    assert_eq!(
                        pic.decoded_curr_pic.pic_order_cnt as u64, count,
                        "frame {t}: order count"
                    );
                    (
                        pic.decoded_curr_pic.picture_id,
                        (!key).then(|| {
                            (
                                slice.ref_pic_list0[0].picture_id,
                                slice.ref_pic_list0[0].pic_order_cnt as u64,
                            )
                        }),
                    )
                }
            });
            if let Some(reference) = reference {
                assert_eq!(
                    reference,
                    (recon[t as usize - 1], count - 1),
                    "{codec:?} frame {t} predicts from the previous frame"
                );
            }
            assert_ne!(
                recon.last(),
                Some(&current),
                "{codec:?} frame {t} reconstructs into a surface of its own"
            );
            recon.push(current);
            assert_eq!(enc.last_reference(), Reference::Untracked);
        }
        let pool: std::collections::HashSet<_> = recon.iter().collect();
        assert_eq!(
            pool.len(),
            2,
            "{codec:?}: two reconstruction surfaces alternate"
        );
        assert_eq!(enc.negotiated.dpb, 1, "{codec:?}: a one-frame buffer");
    }
}

/// The AV1 sequence header carries the frame size in as many bits as the size needs, since a
/// decoder takes each picture's size from it; a field a bit short wraps to another size.
#[test]
fn av1_sequence_header_carries_the_frame_size() {
    for (w, h) in [
        (1920, 1080),
        (1280, 720),
        (1366, 768),
        (1024, 512),
        (320, 240),
    ] {
        mock::reset(Driver::generous());
        let mut enc = open(Codec::Av1, &sized(Codec::Av1, false, w, h, 30.0)).unwrap();
        encode_sized(&mut enc, w, h, 0, true);
        let sequence = mock::with(|d| {
            d.last_packed()
                .into_iter()
                .find(|p| p.0 == VAEncPackedHeaderSequence)
                .unwrap()
                .1
        });
        let mut r = Reader {
            bytes: &sequence,
            pos: 8 * (1 + 4),
        };
        assert_eq!(r.u(3), 0, "seq_profile");
        assert_eq!(
            r.u(4),
            0,
            "still picture, reduced header, timing info, and display delay"
        );
        assert_eq!(r.u(5), 0, "operating_points_cnt_minus_1");
        assert_eq!(r.u(12), 0, "operating_point_idc");
        if r.u(5) > 7 {
            r.u(1);
        }
        let (wbits, hbits) = (r.u(4) + 1, r.u(4) + 1);
        assert_eq!(
            (r.u(wbits) + 1, r.u(hbits) + 1),
            (w as u32, h as u32),
            "{w}x{h}"
        );
    }
}

/// VP8 reconstructs every frame into a surface none of its three buffers holds, and names
/// each buffer by the surface its frame was reconstructed into, so a golden or altref anchor
/// older than the pool is still intact when a frame predicts from it, as the recovery from a
/// lost frame does.
#[test]
fn vp8_never_reconstructs_into_a_buffer_it_holds() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::Vp8, false);
    let mut holds = [(VA_INVALID_SURFACE, 0u64); 3];
    let mut content: HashMap<VASurfaceID, u64> = HashMap::new();
    for t in 0..48u64 {
        if t == 30 {
            assert!(enc.invalidate_reference(29));
        }
        encode(&mut enc, t, t == 0);
        mock::with(|d| {
            let pic: VAEncPictureParameterBufferVP8 =
                d.last_param(VAEncPictureParameterBufferType).unwrap();
            let p = unsafe { pic.pic_flags.bits };
            let key = p.frame_type() == 0;
            if !key {
                let named = [pic.ref_last_frame, pic.ref_gf_frame, pic.ref_arf_frame];
                for (b, &(surface, frame)) in holds.iter().enumerate() {
                    assert_eq!(
                        named[b], surface,
                        "frame {t}: buffer {b} is named by the surface frame {frame} was reconstructed into"
                    );
                    assert_eq!(
                        content.get(&surface),
                        Some(&frame),
                        "frame {t}: buffer {b}'s surface still holds frame {frame}"
                    );
                }
                assert!(
                    !named.contains(&pic.reconstructed_frame),
                    "frame {t} reconstructs into a surface a buffer holds"
                );
            }
            content.insert(pic.reconstructed_frame, t);
            let refresh = [
                p.refresh_last(),
                p.refresh_golden_frame(),
                p.refresh_alternate_frame(),
            ];
            for (b, held) in holds.iter_mut().enumerate() {
                if key || refresh[b] == 1 {
                    *held = (pic.reconstructed_frame, t);
                }
            }
        });
        if t == 30 {
            assert_eq!(
                enc.last_reference(),
                Reference::Frame(24),
                "the recovery predicts from the golden anchor"
            );
        }
    }
}

/// The video processor writes the picture at its own size onto the aligned surface the
/// encoder reads, rather than stretching it over the alignment rows the stream crops away.
#[test]
fn the_video_processor_writes_the_picture_unscaled() {
    mock::reset(Driver::generous());
    let (w, h) = (320, 232);
    let mut enc = open(Codec::H264, &sized(Codec::H264, false, w, h, 30.0)).unwrap();
    encode_sized(&mut enc, w, h, 0, true);
    mock::with(|d| {
        assert!(
            d.surfaces.iter().any(|s| s.3 == 240),
            "the surfaces align to whole macroblocks"
        );
        let whole = Some((0, 0, w as u16, h as u16));
        assert_eq!(
            d.regions,
            [(whole, whole)],
            "the source and output rectangles"
        );
    });
}

/// A low-power entry point without the rate control a session asks for leaves it to the full
/// entry point, as the constant-rate sessions of Intel parts whose low-power encoder runs
/// constant quantizer only need; a session the low-power one serves stays there.
#[test]
fn a_session_falls_back_to_the_full_entry_point_for_its_rate_control() {
    let driver = || {
        let mut d = Driver::generous();
        d.entrypoints = vec![VAEntrypointEncSliceLP, VAEntrypointEncSlice];
        d.entrypoint_attributes =
            vec![(VAEntrypointEncSliceLP, VAConfigAttribRateControl, VA_RC_CQP)];
        d
    };
    mock::reset(driver());
    let enc = session(Codec::H264, true);
    assert!(
        !enc.low_power(),
        "the constant-rate session takes the full entry point"
    );
    mock::with(|d| {
        let (_, entrypoint, attribs) = &d.configs[0];
        assert_eq!(*entrypoint, VAEntrypointEncSlice);
        assert_eq!(
            attribs
                .iter()
                .find(|a| a.type_ == VAConfigAttribRateControl)
                .map(|a| a.value),
            Some(VA_RC_CBR)
        );
    });
    mock::reset(driver());
    assert!(
        session(Codec::H264, false).low_power(),
        "the constant-quantizer session stays on the low-power entry point"
    );
}

/// HEVC Main 4:4:4 declares the constraint flags Table A.2 gives the profile: at most 8 bits,
/// with neither the 4:2:2 nor the 4:2:0 constraint.
#[test]
fn hevc_main_444_declares_its_profile_constraints() {
    mock::reset(Driver::generous());
    let mut s = settings(Codec::H265, false);
    s.video_fullcolor = true;
    let mut enc = open(Codec::H265, &s).unwrap();
    assert!(enc.is_fullcolor());
    let first = encode(&mut enc, 0, true);
    let rbsp = nal(&first[VIDEO_HEADER_LEN..], 33, true);
    let mut r = Reader {
        bytes: &rbsp,
        pos: 8,
    };
    assert_eq!(
        (r.u(2), r.u(1), r.u(5)),
        (0, 1, 4),
        "profile space, tier, and Main 4:4:4"
    );
    r.pos += 32 + 4;
    let flags: Vec<u32> = (0..9).map(|_| r.u(1)).collect();
    assert_eq!(
        flags,
        [1, 1, 1, 0, 0, 0, 0, 0, 1],
        "max 12-bit, 10-bit, 8-bit, 4:2:2, 4:2:0, monochrome, intra, one picture, lower bit rate"
    );
}

/// A frame-rate change re-declares the stream at the level its new rate asks for, never below
/// the one the decoded picture buffer was sized for at open: a lower level admits fewer
/// reference frames than the buffer the stream keeps.
#[test]
fn a_rate_change_keeps_the_level_the_buffer_needs() {
    let (w, h) = (1920, 1080);
    for codec in [Codec::H264, Codec::H265] {
        mock::reset(Driver::generous());
        let mut enc = open(codec, &sized(codec, false, w, h, 120.0)).unwrap();
        encode_sized(&mut enc, w, h, 0, true);
        enc.reconfigure_rate(&sized(codec, false, w, h, 60.0))
            .unwrap();
        let out = encode_sized(&mut enc, w, h, 1, false);
        assert_eq!(
            parse_video_type(out[1]),
            Some((codec, FRAME_KEY)),
            "{codec:?}: the new rate opens a sequence"
        );
        let stream = &out[VIDEO_HEADER_LEN..];
        if codec == Codec::H264 {
            let level = u32::from(nal(stream, 7, false)[2]);
            let refs = h264_max_num_ref_frames(stream).unwrap();
            assert!(
                crate::encoders::codec::h264_dpb_frames(level, w as u32, h as u32) >= refs,
                "level_idc {level} admits {refs} reference frames"
            );
        } else {
            let rbsp = nal(stream, 33, true);
            let mut r = Reader {
                bytes: &rbsp,
                pos: 8 + 88,
            };
            let level = r.u(8);
            r.ue();
            assert_eq!(r.ue(), 1, "chroma_format_idc");
            r.ue();
            r.ue();
            if r.u(1) == 1 {
                (0..4).for_each(|_| {
                    r.ue();
                });
            }
            r.ue();
            r.ue();
            r.ue();
            assert_eq!(r.u(1), 0, "sps_sub_layer_ordering_info_present_flag");
            let buffered = r.ue() + 1;
            assert!(
                crate::encoders::codec::h265_dpb_frames(level, w as u32, h as u32) + 1 >= buffered,
                "general_level_idc {level} admits {buffered} buffered pictures"
            );
        }
    }
}

/// A constant-rate target past what the declared level admits opens a new sequence at the
/// level the new rate needs, since a decoder learns the level only from a sequence header.
#[test]
fn a_target_past_the_declared_level_restarts_the_sequence_at_a_higher_level() {
    let (w, h) = (1920, 1080);
    mock::reset(Driver::generous());
    let mut s = sized(Codec::H264, true, w, h, 30.0);
    let mut enc = open(Codec::H264, &s).unwrap();
    let first = encode_sized(&mut enc, w, h, 0, true);
    let declared = nal(&first[VIDEO_HEADER_LEN..], 7, false)[2];
    s.video_bitrate_kbps = 100_000;
    enc.reconfigure_rate(&s).unwrap();
    let out = encode_sized(&mut enc, w, h, 1, false);
    assert_eq!(parse_video_type(out[1]), Some((Codec::H264, FRAME_KEY)));
    let raised = nal(&out[VIDEO_HEADER_LEN..], 7, false)[2];
    assert!(raised > declared, "level_idc {declared} -> {raised}");
}

/// Once a sequence has declared the level a constant-rate target needs, the target can fall and
/// come back to it with the next picture each time, since a level only has to be high enough.
#[test]
fn a_target_back_within_the_declared_level_rides_the_next_picture() {
    let (w, h) = (1920, 1080);
    mock::reset(Driver::generous());
    let mut s = sized(Codec::H264, true, w, h, 30.0);
    let mut enc = open(Codec::H264, &s).unwrap();
    encode_sized(&mut enc, w, h, 0, true);
    s.video_bitrate_kbps = 100_000;
    enc.reconfigure_rate(&s).unwrap();
    let out = encode_sized(&mut enc, w, h, 1, false);
    assert_eq!(parse_video_type(out[1]), Some((Codec::H264, FRAME_KEY)));
    for (n, kbps) in [(2, 8_000), (3, 100_000)] {
        s.video_bitrate_kbps = kbps;
        enc.reconfigure_rate(&s).unwrap();
        let out = encode_sized(&mut enc, w, h, n, false);
        assert_eq!(
            parse_video_type(out[1]),
            Some((Codec::H264, FRAME_DELTA)),
            "{kbps} kbit/s"
        );
    }
}

/// A session whose host upload cannot be set up leaves no surface behind.
#[test]
fn a_session_that_fails_to_open_frees_its_surfaces() {
    let mut copying = Driver::generous();
    copying.derive = false;
    copying.image_fails = true;
    mock::reset(copying);
    assert!(open(Codec::H264, &settings(Codec::H264, false)).is_err());
    mock::with(|d| {
        let leaked: Vec<VASurfaceID> = d
            .surfaces
            .iter()
            .map(|s| s.0)
            .filter(|id| !d.destroyed.contains(id))
            .collect();
        assert!(leaked.is_empty(), "surfaces {leaked:?} outlive the session");
    });
}

/// A session asked for bare frames returns each frame's coded bytes alone, the bytes a framed
/// session puts after its header.
#[test]
fn a_session_without_headers_returns_the_coded_bytes_alone() {
    for codec in Codec::VIDEO {
        mock::reset(Driver::generous());
        let mut framed = session(codec, false);
        let mut bare = open(
            codec,
            &CaptureSettings {
                omit_stripe_headers: true,
                ..settings(codec, false)
            },
        )
        .unwrap();
        for t in 0..3u64 {
            let with_header = encode(&mut framed, t, t == 0);
            assert_eq!(
                encode(&mut bare, t, t == 0),
                with_header[VIDEO_HEADER_LEN..],
                "{codec:?} frame {t}"
            );
        }
    }
}

/// Where the driver writes its own SPS, as radeonsi does, `frame_num` wraps where that SPS says,
/// so a loss covering the frame that carries `frame_num` 0 there is answered with a key frame
/// rather than a prediction across the wrap.
#[test]
fn a_loss_across_the_wrap_of_the_drivers_frame_num_costs_a_key_frame() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::H264, false);
    let radeonsi_key = [
        &[
            0, 0, 0, 1, 0x67, 0x64, 0x0c, 0x2a, 0xac, 0x23, 0x28, 0x0f, 0x00, 0x44, 0xfc, 0xb3,
            0x50, 0x10, 0x10, 0x14, 0x00, 0x00, 0x03,
        ][..],
        &[
            0x00, 0x04, 0x00, 0x00, 0x03, 0x01, 0xe2, 0x3c, 0x22, 0x11, 0x96,
        ],
        &[0, 0, 0, 1, 0x68, 0xee, 0x38, 0x30],
        &[0, 0, 0, 1, 0x65, 0x88, 0x80, 0x43],
    ]
    .concat();
    assert_eq!(
        h264_frame_num_range(&radeonsi_key),
        Some(128),
        "radeonsi's SPS: log2_max_frame_num_minus4 3"
    );
    mock::with(|d| d.coded = Some(radeonsi_key));
    encode(&mut enc, 0, true);
    mock::with(|d| d.coded = None);
    for t in 1..130u64 {
        encode(&mut enc, t, false);
    }
    assert!(enc.invalidate_reference(127));
    let out = encode(&mut enc, 130, false);
    assert_eq!(
        parse_video_type(out[1]),
        Some((Codec::H264, FRAME_KEY)),
        "frame 128 carried frame_num 0"
    );
}

/// A driver that writes its own SPS in place of the session's, as radeonsi's VCE firmware did
/// before Mesa 25.0, drops the reorder bound; the session writes it back, whatever picture order
/// count the driver chose, and passes every other unit of the frame as the driver coded it.
#[test]
fn a_bound_the_driver_drops_is_written_back() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::H264, false);
    let rest = [
        &[0, 0, 0, 1, 0x68, 0xee, 0x38, 0x30][..],
        &[0, 0, 0, 1, 0x65, 0x88, 0x80, 0x43],
    ]
    .concat();
    let driver_key = [&[0, 0, 0, 1][..], VCE_SPS, &rest].concat();
    assert_eq!(
        h264_reorder(&driver_key),
        None,
        "the driver's SPS declares no bound"
    );
    mock::with(|d| d.coded = Some(driver_key));
    let key = encode(&mut enc, 0, true);
    assert_no_reorder(&key[VIDEO_HEADER_LEN..], "the driver's SPS, bounded");
    assert!(key.ends_with(&rest), "the PPS and the slice changed");
    assert_eq!(
        h264_frame_num_range(&key[VIDEO_HEADER_LEN..]),
        Some(128),
        "the driver's frame_num range"
    );
    let delta = [0, 0, 0, 1, 0x41, 0x9a, 0x02, 0x04];
    mock::with(|d| d.coded = Some(delta.to_vec()));
    assert_eq!(
        encode(&mut enc, 1, false)[VIDEO_HEADER_LEN..],
        delta,
        "a delta came back changed"
    );
    mock::with(|d| d.coded = None);
}

/// On AMD's VCE an H.264 picture is one slice, the cut that encoder codes at twice the rate of
/// four; its HEVC, and every other device, keep four.
#[test]
fn an_h264_picture_on_vce_is_one_slice() {
    for (vce, codec, wanted) in [
        (true, Codec::H264, 1),
        (true, Codec::H265, 4),
        (false, Codec::H264, 4),
    ] {
        mock::reset(Driver::generous());
        let node = std::fs::File::open("/dev/null").unwrap();
        let mut device = Device::on(&mock::api(), node.into(), "stand-in").unwrap();
        assert!(
            !device.vce,
            "a node the kernel does not answer for is not VCE"
        );
        device.vce = vce;
        let mut enc = VaapiEncoder::on_device(
            Arc::new(device),
            &settings(codec, false),
            codec,
            Input::Host { rgba: false },
        )
        .unwrap();
        encode(&mut enc, 0, true);
        assert_eq!(
            mock::with(|d| d.last_buffers(VAEncSliceParameterBufferType).len()),
            wanted,
            "{codec:?} on VCE {vce}"
        );
    }
}

/// A part whose low-power H.264 encoder codes one slice a picture encodes H.264 on the full
/// entry point, in four slices, where the driver offers one, and on the low-power one in a
/// single slice where it does not; its HEVC, and every other part, stay on the low-power one.
#[test]
fn h264_avoids_a_low_power_encoder_that_codes_one_slice() {
    let both = vec![VAEntrypointEncSliceLP, VAEntrypointEncSlice];
    let low_power_only = vec![VAEntrypointEncSliceLP];
    for (whole_picture, entrypoints, codec, low_power, slices) in [
        (true, &both, Codec::H264, false, 4),
        (true, &low_power_only, Codec::H264, true, 1),
        (true, &both, Codec::H265, true, 4),
        (false, &both, Codec::H264, true, 4),
    ] {
        let mut driver = Driver::generous();
        driver.entrypoints = entrypoints.clone();
        mock::reset(driver);
        let node = std::fs::File::open("/dev/null").unwrap();
        let mut device = Device::on(&mock::api(), node.into(), "stand-in").unwrap();
        assert!(
            !device.whole_picture_vdenc,
            "a node the kernel names no PCI device for is not such a part"
        );
        device.whole_picture_vdenc = whole_picture;
        let mut enc = VaapiEncoder::on_device(
            Arc::new(device),
            &settings(codec, false),
            codec,
            Input::Host { rgba: false },
        )
        .unwrap();
        let case = format!("{codec:?}, whole-picture {whole_picture}, {entrypoints:?}");
        assert_eq!(enc.low_power(), low_power, "{case}");
        encode(&mut enc, 0, true);
        assert_eq!(
            mock::with(|d| d.last_buffers(VAEncSliceParameterBufferType).len()),
            slices,
            "{case}"
        );
    }
}

/// The PCI ids the kernel lists for Skylake and Broxton, and none of a later part's.
#[test]
fn skylake_and_broxton_are_told_by_their_pci_ids() {
    for id in [0x1902, 0x1912, 0x1916, 0x193b, 0x0a84, 0x5a85] {
        assert!(skylake_or_broxton(id), "{id:#x}");
    }
    for id in [0x5912, 0x3e92, 0x3185, 0x9a49, 0x46d1, 0x56a0] {
        assert!(!skylake_or_broxton(id), "{id:#x}");
    }
}

/// A driver taking fewer slices than a session asks for gets as many as it takes, rather than
/// no session at all.
#[test]
fn a_session_cuts_no_more_slices_than_the_driver_takes() {
    for codec in [Codec::H264, Codec::H265] {
        for max in [1, 2] {
            let mut few = Driver::generous();
            few.attributes.retain(|a| a.0 != VAConfigAttribEncMaxSlices);
            few.attributes.push((VAConfigAttribEncMaxSlices, max));
            mock::reset(few);
            let mut enc = open(codec, &settings(codec, false))
                .unwrap_or_else(|e| panic!("{codec:?} at most {max}: {e}"));
            encode(&mut enc, 0, true);
            assert_eq!(
                mock::with(|d| d.last_buffers(VAEncSliceParameterBufferType).len()),
                max as usize,
                "{codec:?}"
            );
        }
    }
}

/// An H.264 picture is coded no finer than the floor radeonsi's VCN 1 codes sharp edges at, at a
/// constant quantizer and as the bound of a constant rate; an HEVC picture no finer than Intel's
/// low-power encoder codes, which leaves a slice naming a finer quantizer coded at its floor,
/// and keeps the quantizer asked, read off the driver's curve, on any other entry point or
/// driver.
#[test]
fn a_picture_is_coded_no_finer_than_the_floor() {
    let intel = Some(c"Intel iHD driver for Intel(R) Gen Graphics - 25.2.3 ()");
    for (codec, cbr, vendor, entrypoint, wanted) in [
        (Codec::H264, false, None, VAEntrypointEncSliceLP, 7),
        (Codec::H264, true, None, VAEntrypointEncSliceLP, 7),
        (Codec::H265, false, None, VAEntrypointEncSliceLP, 5),
        (Codec::H265, false, intel, VAEntrypointEncSliceLP, 10),
        (Codec::H265, false, intel, VAEntrypointEncSlice, 4),
    ] {
        let mut driver = Driver::generous();
        driver.vendor = vendor;
        driver.entrypoints = vec![entrypoint];
        mock::reset(driver);
        let mut enc = session(codec, cbr);
        assert_eq!(enc.low_power(), entrypoint == VAEntrypointEncSliceLP);
        enc.encode_host(&frame(), (W * 4) as usize, false, 0, 5, true)
            .unwrap();
        mock::with(|d| {
            if cbr {
                let (_, bytes) = d
                    .last_misc()
                    .into_iter()
                    .find(|m| m.0 == VAEncMiscParameterTypeRateControl)
                    .unwrap();
                let rc: VAEncMiscParameterRateControl =
                    unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                assert_eq!(rc.min_qp, wanted, "{codec:?} constant rate");
            } else if codec == Codec::H264 {
                let pic: VAEncPictureParameterBufferH264 =
                    d.last_param(VAEncPictureParameterBufferType).unwrap();
                let slice: VAEncSliceParameterBufferH264 =
                    d.last_param(VAEncSliceParameterBufferType).unwrap();
                assert_eq!(
                    i32::from(pic.pic_init_qp) + i32::from(slice.slice_qp_delta),
                    wanted as i32,
                    "{codec:?}"
                );
            } else {
                let slice: VAEncSliceParameterBufferHEVC =
                    d.last_param(VAEncSliceParameterBufferType).unwrap();
                let pic: VAEncPictureParameterBufferHEVC =
                    d.last_param(VAEncPictureParameterBufferType).unwrap();
                assert_eq!(
                    i32::from(pic.pic_init_qp) + i32::from(slice.slice_qp_delta),
                    wanted as i32,
                    "{codec:?}"
                );
            }
        });
    }
}

/// Intel's full HEVC encoder, whose rate control codes its quantizer in every coding unit as a
/// delta from the slice's, gets the headers FFmpeg codes there: where it reports no block sizes
/// (Skylake's), blocks from 8x8 to 32x32 on surfaces still aligned to 16, and one delta per tree
/// block; under a constant rate, a slice naming the middle of the quantizer range at the
/// session's depth and a rate control bounded at 51, so every delta stays within the syntax's
/// range. The low-power encoder and other drivers keep 16x16 blocks, a delta per block, 30, and
/// the bounds asked.
#[test]
fn intels_full_hevc_encoder_gets_the_headers_ffmpeg_codes_there() {
    let intel = Some(c"Intel iHD driver for Intel(R) Gen Graphics - 26.1.2 ()");
    // A height of 8 lines past a multiple of 16: the surface shows the alignment.
    let height = 232;
    for (vendor, entrypoint, block_sizes, bit_depth, init_qp, max_qp, min_cb, depth, surface) in [
        (intel, VAEntrypointEncSlice, false, 8, 26, 51, 8u32, 0, 240),
        (intel, VAEntrypointEncSlice, false, 10, 20, 51, 8, 0, 240),
        (intel, VAEntrypointEncSlice, true, 8, 26, 51, 8, 0, 232),
        (intel, VAEntrypointEncSliceLP, false, 8, 30, 0, 16, 1, 240),
        (None, VAEntrypointEncSlice, false, 8, 30, 0, 16, 1, 240),
    ] {
        let mut driver = ten_bit_driver();
        driver.vendor = vendor;
        driver.entrypoints = vec![entrypoint];
        if !block_sizes {
            driver.attributes.retain(|a| {
                a.0 != VAConfigAttribEncHEVCBlockSizes && a.0 != VAConfigAttribEncHEVCFeatures
            });
        }
        mock::reset(driver);
        let mut s = settings(Codec::H265, true);
        s.height = height;
        s.video_bit_depth = bit_depth;
        let mut enc = open(Codec::H265, &s).unwrap();
        assert_eq!(enc.bit_depth(), bit_depth as u32);
        let frame = vec![0x40; (W * height * 4) as usize];
        enc.encode_host(&frame, (W * 4) as usize, false, 0, 25, true)
            .unwrap();
        let case = format!("{vendor:?} {entrypoint} block sizes {block_sizes} at {bit_depth} bits");
        mock::with(|d| {
            let seq: VAEncSequenceParameterBufferHEVC =
                d.last_param(VAEncSequenceParameterBufferType).unwrap();
            let min_cb_minus3 = min_cb.trailing_zeros() - 3;
            assert_eq!(
                (
                    u32::from(seq.log2_min_luma_coding_block_size_minus3),
                    u32::from(seq.log2_diff_max_min_luma_coding_block_size),
                    i32::from(seq.pic_height_in_luma_samples),
                ),
                (min_cb_minus3, 2 - min_cb_minus3, surface),
                "blocks from {min_cb} to 32 on a surface of {surface} lines, {case}"
            );
            let pic: VAEncPictureParameterBufferHEVC =
                d.last_param(VAEncPictureParameterBufferType).unwrap();
            assert_eq!(u32::from(pic.pic_init_qp), init_qp, "{case}");
            assert_eq!(u32::from(pic.diff_cu_qp_delta_depth), depth, "{case}");
            unsafe {
                assert_eq!(pic.pic_fields.bits.cu_qp_delta_enabled_flag(), 1, "{case}");
            }
            let slice: VAEncSliceParameterBufferHEVC =
                d.last_param(VAEncSliceParameterBufferType).unwrap();
            assert_eq!(slice.slice_qp_delta, 0, "{case}");
            let headers = d
                .last_packed()
                .into_iter()
                .find(|p| p.0 == VAEncPackedHeaderSequence)
                .unwrap()
                .1;
            let rbsp = nal(&headers, 34, true);
            let mut r = Reader {
                bytes: &rbsp,
                pos: 0,
            };
            assert_eq!(r.ue(), 0, "pps id");
            assert_eq!(r.ue(), 0, "sps id");
            r.u(1 + 1 + 3 + 1 + 1);
            r.ue();
            r.ue();
            assert_eq!(r.se(), init_qp as i32 - 26, "init_qp_minus26, {case}");
            r.u(1 + 1);
            assert_eq!(r.u(1), 1, "cu_qp_delta_enabled_flag, {case}");
            assert_eq!(r.ue(), depth, "diff_cu_qp_delta_depth, {case}");
            let (_, bytes) = d
                .last_misc()
                .into_iter()
                .find(|m| m.0 == VAEncMiscParameterTypeRateControl)
                .unwrap();
            let rc: VAEncMiscParameterRateControl =
                unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
            assert_eq!((rc.min_qp, rc.max_qp), (0, max_qp), "{case}");
        });
    }
}

/// A constant-rate session caps each coded frame at its buffer where the driver takes a cap,
/// and sends none where it does not.
#[test]
fn a_constant_rate_session_caps_each_frame_at_its_buffer() {
    for offered in [true, false] {
        let mut driver = Driver::generous();
        if offered {
            driver.attributes.push((VAConfigAttribMaxFrameSize, 1));
        }
        mock::reset(driver);
        let mut enc = session(Codec::H264, true);
        encode(&mut enc, 0, true);
        mock::with(|d| {
            let cap = d
                .last_misc()
                .into_iter()
                .find(|m| m.0 == VAEncMiscParameterTypeMaxFrameSize);
            match cap {
                Some((_, bytes)) => {
                    assert!(offered, "a cap the driver does not take");
                    let cap: VAEncMiscParameterBufferMaxFrameSize =
                        unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                    assert_eq!(
                        cap.max_frame_size, 200_000,
                        "the buffer, 1.5 frames at 4 Mbps and 30 fps, in bits"
                    );
                }
                None => assert!(!offered, "no cap where the driver takes one"),
            }
        });
    }
}

/// The HEVC sequence declares a finite intra period, matching the range its picture order count
/// wraps in, rather than i32::MAX, whose next power of two overflows radeonsi's UVD max_poc and
/// hangs the encoder; a decoder derives the same order-count width from either.
#[test]
fn the_hevc_sequence_declares_a_finite_intra_period() {
    mock::reset(Driver::generous());
    let mut enc = session(Codec::H265, false);
    encode(&mut enc, 0, true);
    mock::with(|d| {
        let seq: VAEncSequenceParameterBufferHEVC =
            d.last_param(VAEncSequenceParameterBufferType).unwrap();
        let poc_lsb = 1u32 << (4 + 8);
        assert_eq!(
            seq.intra_period, poc_lsb,
            "the intra period is the picture-order-count range"
        );
        assert_eq!(seq.intra_idr_period, poc_lsb);
        assert!(
            u64::from(seq.intra_period).next_power_of_two() <= u64::from(u32::MAX),
            "its next power of two does not overflow"
        );
    });
}

/// A driver that also lists the 10-bit profiles and renders their surfaces.
fn ten_bit_driver() -> Driver {
    let mut driver = Driver::generous();
    driver.profiles.extend([
        VAProfileHEVCMain10,
        VAProfileHEVCMain444_10,
        VAProfileVP9Profile2,
        VAProfileVP9Profile3,
    ]);
    for attribute in &mut driver.attributes {
        if attribute.0 == VAConfigAttribRTFormat {
            attribute.1 |= VA_RT_FORMAT_YUV420_10 | VA_RT_FORMAT_YUV444_10;
        }
    }
    driver
        .surface_fourccs
        .extend([VA_FOURCC_P010, VA_FOURCC_Y410]);
    driver
}

/// A 10-bit request opens the codec's 10-bit profile on that depth's surfaces where the
/// driver lists one and declares it in the stream, and comes up at 8 bits where the driver
/// lists none or the codec has none.
#[test]
fn ten_bit_follows_the_driver() {
    for codec in Codec::VIDEO {
        mock::reset(Driver::generous());
        let mut s = settings(codec, false);
        s.video_bit_depth = 10;
        let enc = open(codec, &s).unwrap_or_else(|e| panic!("{codec:?}: {e}"));
        assert_eq!(enc.bit_depth(), 8, "{codec:?} on a driver without 10 bits");
    }
    for (codec, fullcolor, profile, format, surfaces) in [
        (
            Codec::H265,
            false,
            VAProfileHEVCMain10,
            VA_RT_FORMAT_YUV420_10,
            "p010",
        ),
        (
            Codec::H265,
            true,
            VAProfileHEVCMain444_10,
            VA_RT_FORMAT_YUV444_10,
            "y410",
        ),
        (
            Codec::Vp9,
            false,
            VAProfileVP9Profile2,
            VA_RT_FORMAT_YUV420_10,
            "p010",
        ),
        (
            Codec::Vp9,
            true,
            VAProfileVP9Profile3,
            VA_RT_FORMAT_YUV444_10,
            "y410",
        ),
        (
            Codec::Av1,
            false,
            VAProfileAV1Profile0,
            VA_RT_FORMAT_YUV420_10,
            "p010",
        ),
    ] {
        mock::reset(ten_bit_driver());
        let mut s = settings(codec, false);
        s.video_bit_depth = 10;
        s.video_fullcolor = fullcolor;
        let mut enc = open(codec, &s).unwrap_or_else(|e| panic!("{codec:?}: {e}"));
        assert_eq!(enc.bit_depth(), 10, "{codec:?}");
        assert_eq!(enc.is_fullcolor(), fullcolor, "{codec:?}");
        assert_eq!(enc.surface_format_name(), surfaces, "{codec:?}");
        mock::with(|d| {
            let (opened, _, attribs) = &d.configs[0];
            assert_eq!(*opened, profile, "{codec:?}");
            let rt = attribs
                .iter()
                .find(|a| a.type_ == VAConfigAttribRTFormat)
                .map(|a| a.value);
            assert_eq!(rt, Some(format), "{codec:?}");
        });
        if codec == Codec::H265 {
            let key = encode(&mut enc, 0, true);
            let sps = nal(&key[VIDEO_HEADER_LEN..], 33, true);
            let mut r = Reader {
                bytes: &sps,
                pos: 0,
            };
            r.u(4 + 3 + 1 + 2 + 1);
            assert_eq!(
                r.u(5),
                if fullcolor { 4 } else { 2 },
                "the profile declared"
            );
        }
    }
    for codec in [Codec::H264, Codec::Vp8] {
        mock::reset(ten_bit_driver());
        let mut s = settings(codec, false);
        s.video_bit_depth = 10;
        assert_eq!(
            open(codec, &s).unwrap().bit_depth(),
            8,
            "{codec:?} has no 10 bits"
        );
    }
    mock::reset(ten_bit_driver());
    let ten_bit: Vec<(Codec, [bool; 2])> = probe_codecs_on(&device())
        .unwrap()
        .into_iter()
        .map(|(codec, formats)| (codec, formats.ten_bit))
        .collect();
    assert_eq!(
        ten_bit,
        [
            (Codec::H264, [false, false]),
            (Codec::Vp8, [false, false]),
            (Codec::Vp9, [true, true]),
            (Codec::Av1, [true, false]),
            (Codec::H265, [true, true]),
        ]
    );
}

/// A driver that asks for both reference lists of a predicted slice gets a B slice naming
/// the one reference in each, in the slice header and in the slice parameters; one that asks
/// nothing gets the P slice.
#[test]
fn hevc_predicts_in_the_direction_the_driver_takes() {
    for (direction, slice_type) in [
        (None, 1u32),
        (Some(VA_PREDICTION_DIRECTION_PREVIOUS), 1),
        (
            Some(
                VA_PREDICTION_DIRECTION_PREVIOUS
                    | VA_PREDICTION_DIRECTION_FUTURE
                    | VA_PREDICTION_DIRECTION_BI_NOT_EMPTY,
            ),
            0,
        ),
    ] {
        let mut driver = Driver::generous();
        if let Some(direction) = direction {
            driver
                .attributes
                .push((VAConfigAttribPredictionDirection, direction));
        }
        mock::reset(driver);
        let mut enc = session(Codec::H265, false);
        encode(&mut enc, 0, true);
        let delta = encode(&mut enc, 1, false);
        let slice = nal(&delta[VIDEO_HEADER_LEN..], 1, true);
        let mut r = Reader {
            bytes: &slice,
            pos: 0,
        };
        assert_eq!(r.u(1), 1, "the first slice of the picture");
        assert_eq!(r.ue(), 0);
        assert_eq!(r.ue(), slice_type, "{direction:?}");
        mock::with(|d| {
            for bytes in d.last_buffers(VAEncSliceParameterBufferType) {
                let s: VAEncSliceParameterBufferHEVC =
                    unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const _) };
                assert_eq!(u32::from(s.slice_type), slice_type, "{direction:?}");
                assert_ne!(s.ref_pic_list0[0].picture_id, VA_INVALID_ID);
                assert_eq!(
                    s.ref_pic_list1[0].picture_id != VA_INVALID_ID,
                    slice_type == 0,
                    "the second list is named only for a B slice"
                );
                if slice_type == 0 {
                    assert_eq!(s.ref_pic_list1[0].picture_id, s.ref_pic_list0[0].picture_id);
                }
            }
        });
    }
}

/// A driver that takes regions of interest with a quantizer delta under its rate control
/// (`VAConfigAttribEncROI`) is handed a refinement as a region a run of blocks along a row, each
/// at the delta to the target within `REFINE_REACH`; one that takes none under a constant rate
/// refines nothing, and a key frame carries no regions.
#[test]
fn a_refinement_reaches_the_driver_as_regions_of_interest() {
    use crate::ledger::{BlockLedger, Refinement};
    let regions = || {
        mock::with(|d| {
            d.last_misc()
                .into_iter()
                .find(|m| m.0 == VAEncMiscParameterTypeROI)
                .map(|m| unsafe {
                    let p = ptr::read_unaligned(m.1.as_ptr() as *const VAEncMiscParameterBufferROI);
                    std::slice::from_raw_parts(p.roi, p.num_roi as usize)
                        .iter()
                        .map(|r| {
                            let rect = r.roi_rectangle;
                            (rect.x, rect.y, rect.width, rect.height, r.roi_value)
                        })
                        .collect::<Vec<_>>()
                })
        })
    };
    let mut driver = Driver::generous();
    driver.attributes.push((VAConfigAttribEncROI, 0x210));
    mock::reset(driver);
    let mut enc = session(Codec::H264, true);
    let refiner = enc.refines().expect("the driver takes regions");
    assert_eq!(refiner.runs, 16);
    let mut ledger = BlockLedger::new(W, H);
    ledger.frame([&smithay::utils::Rectangle::new(
        (32, 0).into(),
        (16, H).into(),
    )]);
    ledger.frame([&smithay::utils::Rectangle::new(
        (32, 0).into(),
        (16, H).into(),
    )]);
    let plan = ledger.plan(1, 5, refiner.runs).unwrap();
    let refinement = Refinement {
        plan,
        target: 12,
        from: 40,
        reach: refiner.reach,
    };
    enc.refine_blocks(&refinement);
    encode(&mut enc, 0, true);
    assert_eq!(regions(), None, "a key frame carries none");
    enc.refine_blocks(&refinement);
    encode(&mut enc, 1, false);
    assert_eq!(
        regions(),
        Some(vec![(0, 0, 32, 16, -10), (48, 0, 48, 16, -10)]),
        "two runs, the delta held to the reach"
    );
    encode(&mut enc, 2, false);
    assert_eq!(regions(), None, "a frame not refining carries none");

    let mut driver = Driver::generous();
    driver.attributes.push((VAConfigAttribEncROI, 0x010));
    mock::reset(driver);
    assert!(
        session(Codec::H264, true).refines().is_none(),
        "no quantizer delta under the rate control"
    );
    assert!(session(Codec::H264, false).refines().is_some());
}

/// A driver offering HEVC temporal motion-vector prediction is told it on predicted frames, with
/// the reference it takes the co-located picture from, and never on a key frame, whose reference
/// list holds none: iHD reads the reference the index names whatever the frame's type, past the
/// end of its table where it is invalid.
#[test]
fn an_hevc_key_frame_names_no_collocated_picture() {
    let mut driver = Driver::generous();
    for (kind, value) in driver.attributes.iter_mut() {
        if *kind == VAConfigAttribEncHEVCFeatures {
            let mut features = VAConfigAttribValEncHEVCFeatures { value: *value };
            unsafe { features.bits.set_temporal_mvp(1) };
            *value = unsafe { features.value };
        }
    }
    mock::reset(driver);
    let mut enc = session(Codec::H265, true);
    let coded = |enc: &mut VaapiEncoder, t: u64, key: bool| {
        encode(enc, t, key);
        mock::with(|d| {
            let pic: VAEncPictureParameterBufferHEVC = d
                .last_param(VAEncPictureParameterBufferType)
                .expect("a picture");
            let slice: VAEncSliceParameterBufferHEVC = d
                .last_param(VAEncSliceParameterBufferType)
                .expect("a slice");
            let mvp = unsafe { slice.slice_fields.bits.slice_temporal_mvp_enabled_flag() };
            (pic.collocated_ref_pic_index, mvp)
        })
    };
    assert_eq!(coded(&mut enc, 0, true), (0xff, 0), "the key frame");
    assert_eq!(coded(&mut enc, 1, false), (0, 1), "a predicted frame");
}
