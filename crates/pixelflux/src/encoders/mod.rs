/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Encoder backends and what they share: the codec identities and wire framing, the
//! rate-control policy, which software encoder a build resolves each codec to, and which
//! hardware encoder a render node serves each with.

/// The bit writer the packed headers of the VA-API session are written with.
pub mod bits;
/// The GPU semaphore an X server's blit signals for the encoder, made through Vulkan.
pub(crate) mod blit_semaphore;
/// Codec identities, wire framing, quantizer domains, level ladders, bitstream reads.
pub mod codec;
/// The identity of a dmabuf behind an fd, for the sessions that keep an import of each buffer.
pub(crate) mod dmabuf_identity;
/// A frame rate as the fraction encoder parameters and bitstream timing take.
pub mod frame_rate;
/// Software HEVC: x265 with the `gpl` feature, kvazaar without it.
pub mod hevc;
/// NVIDIA NVENC hardware H.264 / HEVC / AV1 encoder loaded via runtime `libcuda` /
/// `libnvidia-encode`.
pub mod nvenc;
/// Cisco OpenH264 software H.264 encoder (BSD-licensed): the software H.264 encoder of a
/// build without `gpl`, and always built for the test suite.
#[cfg(any(feature = "openh264", test))]
pub mod oh264;
/// The long-term references of an OpenH264 session, read off its own slice headers.
#[cfg(any(feature = "openh264", test))]
pub mod oh264_refs;
/// PNG watermark overlay composited onto frames before encoding.
pub mod overlay;
pub mod reference;
/// What the full-frame software sessions share: planar input, quantizer, rate settings.
pub mod session;
/// The CPU path: striped JPEG and whole-frame software H.264 (libx264 or OpenH264, by build),
/// with per-region change detection.
pub mod software;
/// The color an H.264 stream declares: read from a sequence parameter set, and written into
/// one for a device that converts without saying what it converted with.
pub mod sps;
/// Software AV1 through SVT-AV1.
pub mod svtav1;
/// Tegra hardware video encoding through the vendor V4L2 encoder, loaded at runtime: the only path to a
/// Jetson's encoder, which carries no `libnvidia-encode` and no render node driver. Built for
/// `aarch64` alone — the vendor libraries and the encoder behind them exist on no other
/// architecture, so an x86_64 build carries none of this.
#[cfg(target_arch = "aarch64")]
pub mod tegra;
/// Hardware H.264 through a generic stateful V4L2 M2M encoder: boards whose encoder sits
/// behind the kernel's own interface rather than a vendor library or a render node, such as
/// a Raspberry Pi 4, RK356x, or i.MX8M. Built everywhere, since the interface is the kernel's.
pub mod v4l2m2m;
/// VA-API hardware sessions on a DRM render node, driven through libva directly.
pub mod vaapi;
/// Software VP8 and VP9 through libvpx.
pub mod vpx;

pub use codec::*;

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use nvenc::NvencEncoder;
use smithay::backend::allocator::dmabuf::Dmabuf;

use crate::CaptureSettings;

#[cfg(not(any(feature = "gpl", feature = "openh264")))]
compile_error!(
    "pixelflux needs a software H.264 encoder: enable the `gpl` feature (libx264, the default) or `openh264`."
);

/// A software encoder the build runs for one codec: the library's name as reported to Python
/// and the logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftwareEncoder {
    pub library: &'static str,
}

/// The codecs a render node encodes in hardware, each with the backend's name.
/// The formats past 8-bit 4:2:0 an engine encodes a codec in: 4:4:4 for a `video_fullcolor`
/// session, and 10 bits for a `video_bit_depth` one at 4:2:0 and at 4:4:4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Formats {
    pub fullcolor: bool,
    pub ten_bit: [bool; 2],
}

impl Formats {
    /// Each format as `chroma-depth`, 8-bit 4:2:0 first.
    pub fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for (fullcolor, chroma) in [(false, "420"), (true, "444")] {
            for (ten_bit, depth) in [(false, 8), (true, 10)] {
                let carried = match (fullcolor, ten_bit) {
                    (false, false) => true,
                    (true, false) => self.fullcolor,
                    (_, true) => self.ten_bit[fullcolor as usize],
                };
                if carried {
                    names.push(format!("{chroma}-{depth}"));
                }
            }
        }
        names
    }
}

/// Each video codec an engine on a node encodes, the backend's name, and the formats it
/// encodes the codec in.
pub type HardwareEncoders = Vec<(Codec, &'static str, Formats)>;

/// The hardware backend that serves each video codec on an encode node, as the name a
/// session logs it in lower case (`"nvenc"`, `"vaapi"`, or `"tegra"`), probed once per node and
/// remembered for the life of the process: the ladder picks the backend by the node's
/// driver exactly as `select_frame_encoder` does, and that backend lists the codecs its
/// device has an engine for (`nvenc::probe_codecs`, `vaapi::probe_codecs`). A node whose
/// backend cannot be brought up serves nothing, said once in the log, so a caller offers
/// the codec only where a session would come up on hardware rather than demote. What a
/// session is then refused for (a size past the engine's maximum, a 4:4:4 the engine lacks)
/// is still the ladder's to fall through on.
pub fn hardware_encoders(encode_node_index: i32) -> HardwareEncoders {
    probe_node(encode_node_index).unwrap_or_default()
}

/// An encode node's probe answer: its hardware table, or the refusal of its NVENC or VA-API
/// backend as `(backend, error)`.
type ProbeAnswer = Result<HardwareEncoders, (&'static str, String)>;

/// `hardware_encoders` with the refusal of a node whose backend could not be brought up. A
/// device with no NVENC session to spare (`nvenc::SESSIONS_TAKEN`) says nothing lasting, so
/// that answer is asked again.
fn probe_node(encode_node_index: i32) -> ProbeAnswer {
    static PROBED: OnceLock<Mutex<HashMap<i32, ProbeAnswer>>> = OnceLock::new();
    let node = encode_node_index.max(0);
    let mut probed = PROBED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(answer) = probed.get(&node) {
        return answer.clone();
    }
    #[cfg(target_arch = "aarch64")]
    if tegra::available() {
        let served: HardwareEncoders = tegra::served()
            .into_iter()
            .map(|c| (c, "tegra", Formats::default()))
            .collect();
        let names: Vec<&str> = served.iter().map(|(c, ..)| c.display()).collect();
        println!(
            "[pixelflux] Render node {node} encodes {} on tegra.",
            names.join(", ")
        );
        probed.insert(node, Ok(served.clone()));
        return Ok(served);
    }
    let driver = crate::get_gpu_driver(node);
    let (backend, codecs) = if crate::driver_selects_nvenc(&driver) {
        ("nvenc", nvenc::probe_codecs(node))
    } else {
        ("vaapi", vaapi::probe_codecs(node))
    };
    let answer = match codecs {
        Ok(codecs) => Ok(codecs
            .into_iter()
            .map(|(codec, formats): vaapi::Served| (codec, backend, formats))
            .collect()),
        Err(e) => {
            eprintln!("[pixelflux] No hardware encoder on render node {node} ({backend}): {e}");
            Err((backend, e))
        }
    };
    if answer.as_ref().map_or(true, Vec::is_empty) && v4l2m2m::available() {
        // The node index names nothing here: an M2M encoder is not a render node, and the
        // answer is the same whichever index was asked about. It is cached under the key all
        // the same, so a caller asking twice is answered from the same probe.
        let codecs = v4l2m2m::served();
        let served: HardwareEncoders = codecs
            .iter()
            .map(|&c| (c, "v4l2m2m", Formats::default()))
            .collect();
        let names: Vec<&str> = codecs.iter().map(|c| c.display()).collect();
        println!(
            "[pixelflux] A stateful V4L2 M2M encoder serves {}.",
            names.join(", ")
        );
        probed.insert(node, Ok(served.clone()));
        return Ok(served);
    }
    if let Ok(served) = &answer
        && !served.is_empty()
    {
        let names: Vec<&str> = served.iter().map(|(codec, ..)| codec.display()).collect();
        println!(
            "[pixelflux] Render node {node} encodes {} on {backend}.",
            names.join(", ")
        );
    }
    if !matches!(&answer, Err((_, e)) if e == nvenc::SESSIONS_TAKEN) {
        probed.insert(node, answer.clone());
    }
    answer
}

/// Whether the software encoder of a codec carries a 4:4:4 (`video_fullcolor`) request: x264
/// (High 4:4:4, full range), x265, and libvpx's VP9 (profile 1) do; OpenH264, kvazaar, VP8, and
/// SVT-AV1 encode such a request 4:2:0.
pub fn software_fullcolor(codec: Codec) -> bool {
    match software_encoder(codec) {
        Some(enc) => {
            matches!(enc.library, "x264" | "x265")
                || (codec == Codec::Vp9 && enc.library == "libvpx" && vpx::encodes_444())
        }
        None => false,
    }
}

/// Whether the software encoder of a codec carries a 10-bit `video_bit_depth` request, at
/// whichever chroma it codes: x264 (High 10, and High 4:4:4 Predictive), x265 where the build
/// links its 10-bit encoder, libvpx's VP9 (profiles 2 and 3) where it was built with high bit
/// depth, and SVT-AV1 do; OpenH264, kvazaar, and VP8 encode such a request at 8 bits.
pub fn software_ten_bit(codec: Codec) -> bool {
    match software_encoder(codec) {
        Some(enc) => match (codec, enc.library) {
            #[cfg(feature = "gpl")]
            (Codec::H264, "x264") => software::H264EncoderWrapper::ten_bit(),
            (Codec::H265, "x265") => hevc::ten_bit(),
            (Codec::Vp9, "libvpx") => vpx::encodes_ten_bit(),
            (Codec::Av1, _) => true,
            _ => false,
        },
        None => false,
    }
}

/// The formats the software encoder of a codec codes it in.
pub fn software_formats(codec: Codec) -> Formats {
    let ten_bit = software_ten_bit(codec);
    let fullcolor = software_fullcolor(codec);
    Formats {
        fullcolor,
        ten_bit: [ten_bit, ten_bit && fullcolor],
    }
}

/// The name of the software encoder this build runs for `codec`, for logs; `"none"` for JPEG,
/// which is striped stills.
pub fn software_library(codec: Codec) -> &'static str {
    software_encoder(codec).map_or("none", |enc| enc.library)
}

/// The software encoder this build runs for `codec` on this machine, or `None` for JPEG and for
/// a codec whose encoder does not run here.
///
/// Every one is fixed by the crate features: libx264 and x265 whenever `gpl` is on (it wins
/// even if `openh264` is also enabled), Cisco OpenH264 and kvazaar for a GPL-free build; libvpx
/// serves VP8 and VP9 and SVT-AV1 serves AV1 in both. H.264 is what the CPU path encodes
/// with, the software fallback under NVENC/VA-API included. Each
/// full-frame encoder is opened on a frame in a forked child once, so one that takes its
/// process down on this machine is one the build does not carry here.
pub fn software_encoder(codec: Codec) -> Option<SoftwareEncoder> {
    static ENCODES: OnceLock<[bool; 5]> = OnceLock::new();
    let library = match codec {
        Codec::Jpeg => return None,
        Codec::H264 => {
            if cfg!(feature = "gpl") {
                "x264"
            } else {
                "openh264"
            }
        }
        Codec::H265 => {
            if cfg!(feature = "gpl") {
                "x265"
            } else {
                "kvazaar"
            }
        }
        Codec::Vp8 | Codec::Vp9 => "libvpx",
        Codec::Av1 => "svt-av1",
    };
    let encodes =
        ENCODES.get_or_init(|| Codec::VIDEO.map(|c| c == Codec::H264 || encodes_in_child(c)));
    encodes[Codec::VIDEO.iter().position(|&c| c == codec).unwrap()]
        .then_some(SoftwareEncoder { library })
}

/// Whether opening `codec`'s software session on a small frame and encoding one leaves a
/// process alive, tried in a forked child: a library that faults on this machine, with an
/// instruction the CPU lacks or a register the kernel does not emulate, takes the child down
/// and not a session.
fn encodes_in_child(codec: Codec) -> bool {
    survives_in_child(|| {
        // The child converts on a pool of its own: the parent's rayon workers do not exist in it.
        // The pool is this thread, since a thread the child started would wait on the lock the
        // standard library takes to set a thread up, which a parent thread may have held at the
        // fork.
        let Ok(pool) = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .use_current_thread()
            .build()
        else {
            return;
        };
        pool.install(|| {
            let settings = CaptureSettings {
                width: 256,
                height: 128,
                ..Default::default()
            };
            let Ok(mut session) = software_session(&settings, codec, false) else {
                return;
            };
            let frame = vec![0u8; 256 * 128 * 4];
            for n in 0..64 {
                let packet = session.encode_host(
                    &frame,
                    256 * 4,
                    false,
                    n,
                    settings.video_crf as u32,
                    n == 0,
                );
                if !matches!(packet, Ok(p) if p.is_empty()) {
                    break;
                }
            }
        })
    })
}

/// Whether a forked child outlives `f`: a fatal signal in it is the finding, an exit is not, and
/// neither reaches the caller. The child has a minute, and one that neither returns nor dies
/// in it has stalled, which is as final. What it prints goes nowhere, so a library that
/// announces itself on open speaks for a session and not for the probe. The fork waits until
/// no SVT-AV1 handle is alive (`svtav1::LIFECYCLE`), as at the import-time probe: one being
/// created, released, or run holds process-wide state the child would inherit half-built or
/// locked. One a sandbox refuses to fork counts as alive.
fn survives_in_child(f: impl FnOnce()) -> bool {
    let mut live = svtav1::LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
    while *live > 0 {
        live = svtav1::RELEASED
            .wait(live)
            .unwrap_or_else(|e| e.into_inner());
    }
    let pid = unsafe { libc::fork() };
    drop(live);
    match pid {
        0 => {
            unsafe {
                libc::signal(libc::SIGALRM, libc::SIG_DFL);
                libc::alarm(60);
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                if null >= 0 {
                    libc::dup2(null, 1);
                    libc::dup2(null, 2);
                }
            }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            unsafe { libc::_exit(0) }
        }
        pid if pid > 0 => {
            let mut status = 0;
            while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    return true;
                }
            }
            !libc::WIFSIGNALED(status)
        }
        _ => true,
    }
}

/// Seconds of a constant-rate target a held frame (the cleanup of a still screen) may spend: a
/// held key frame larger than that is coded again at a coarser quantizer (`held_key_retry`), or
/// planned with that budget where the library's own rate control sizes it (x264, SVT-AV1),
/// and a held refresh is coarsened to fit from the rate control's last quantizer
/// (`pipeline::decide_hw_fullframe`), and on NVENC coded again to fit where it comes out past
/// `HELD_REFRESH_LIMIT_S`. A second of the target drains in 0.4 s at the pace the WebRTC pacer
/// holds video to, so a user acting just as the cleanup goes out waits that long at most for
/// it. It holds a 1080p key frame at the paint-over quantizer down to 2 Mbit/s (NVENC 144 kB,
/// x264 182 kB); a quarter second made that key frame coarser than the picture it was cleaning
/// (x264 52 kB at 31.7 dB, NVENC 99 kB at 45.3 dB at 2 Mbit/s).
pub(crate) const HELD_KEY_BUDGET_S: f64 = 1.0;

/// Seconds of a constant-rate target a held refresh of the whole picture, a predicted frame, may
/// spend before it is coded again to fit `HELD_KEY_BUDGET_S` where the encoder can take it back
/// (NVENC): what a cleanup through the rate control spends at most (`pipeline`'s `CONVERGE_S`).
/// A refresh under it stands, since coding it coarser costs the picture more than the time it
/// saves: NVENC's of a 1080p texture at 0.25 and 0.1 Mbit/s, 1.8 and 3.1 s of the target, came
/// out at 1.3 and 1.7 s six to ten steps coarser, 6.4-6.8 dB worse. One of dense text at 0.1
/// Mbit/s at 1080p took 52 s.
pub(crate) const HELD_REFRESH_LIMIT_S: f64 = 10.0;

/// The quality index a held key frame of `len` bytes is coded again at, from `crf`, where it
/// came out larger than a budget of `cap` bytes; `None` where it fits. A quality index is a
/// quantizer on the H.26x scale, where six steps halve the bits.
pub(crate) fn held_key_retry(crf: u32, len: usize, cap: usize) -> Option<u32> {
    (len > cap && cap > 0)
        .then(|| (crf + (6.0 * (len as f64 / cap as f64).log2()).ceil() as u32).min(51))
}

/// The quality index a held key frame of a constant-rate session is first coded at, from `crf`,
/// where the session's last held key frame, `last` = (bytes, quality index), says one at `crf`
/// would come out past a budget of `cap` bytes: as much coarser as `held_key_retry` would take
/// it, so the frame is coded once rather than twice, since a rate control that runs through
/// the held frame (libvpx's) charges its buffer with both. A key frame coded more than twelve
/// steps from `crf` says too little about one at it, and the frame is tried at `crf`.
pub(crate) fn held_key_start(crf: u32, last: Option<(usize, u32)>, cap: usize) -> u32 {
    match last {
        Some((len, q)) if q.abs_diff(crf) <= 12 => {
            let at_crf = len as f64 * 2f64.powf((q as f64 - crf as f64) / 6.0);
            held_key_retry(crf, at_crf.round() as usize, cap).unwrap_or(crf)
        }
        _ => crf,
    }
}

/// Damps visible quality "blinking": the number of consecutive frames a QP *increase* (a
/// quality drop under sustained motion) must be requested before a fixed-QP encoder commits
/// it. Moving the quantizer costs a codec re-open (x265, kvazaar, SVT-AV1) or a full encoder
/// rebuild (OpenH264), and either forces a key frame, so acting on every transient increase —
/// and then reversing it as motion settles — would make the picture pulse. Quality
/// *increases* (a lower QP, e.g. a paint-over refresh) apply at once and never wait. Shared so
/// the fixed-QP encoders cannot disagree about how long a drop must persist.
pub(crate) const QP_HYSTERESIS_LIMIT: u32 = 60;

/// Size the CBR VBV/HRD buffer so rate control has enough slack to hold quality steady
/// without letting end-to-end latency drift upward.
///
/// The size is expressed as a multiple of one frame's bit budget (`bitrate_bps / fps`) rather
/// than a fixed byte count so a live bitrate or framerate change rescales the buffer with it,
/// preserving the same latency behavior at every operating point.
///
/// The 1.5-frame default holds for hardware rate control too: a one-frame buffer measured on
/// NVENC (`gpu_bench_cbr_policy`, a V100) ran an 8 Mbit/s H.264 session at 13.3 to 13.7 Mbit/s
/// on scene cuts at the same PSNR, made the frame after a single cut larger (408 against 288
/// kbit) and bought nothing on steady content. The overshoot is the quarter-resolution first
/// pass misjudging a scene cut with no buffer left to absorb the miss
/// (`gpu_bench_cbr_rate_control`): full-resolution two-pass holds a one-frame buffer at
/// 1.5 ms more per frame, single-pass halves the overshoot at a lower PSNR, and the GOP target
/// and quantizer ceiling change nothing. HEVC holds either buffer.
///
/// # Arguments
///
/// * `bitrate_bps` - Target bitrate in bits per second.
/// * `fps` - Target frames per second.
/// * `keyframe_interval_s` - Seconds between scheduled keyframes; `<= 0` for infinite GOP.
/// * `multiplier` - Explicit buffer multiplier; `<= 0` selects the policy default (1.5 on
///   infinite GOP, 3 when keyframe interval is active).
///
/// # Returns
///
/// VBV buffer size in bits, clamped to `[1, u32::MAX]`.
/// The most frames an H.264 or H.265 session keeps for reference (`video_reference_frames`).
pub fn reference_frames(settings: &CaptureSettings) -> u32 {
    match settings.video_reference_frames {
        0 => reference::REFERENCE_FRAMES,
        n => n.min(reference::REFERENCE_FRAMES),
    }
}

pub fn vbv_bits(bitrate_bps: u32, fps: f64, keyframe_interval_s: f64, multiplier: f64) -> u32 {
    let frame_bits = bitrate_bps as f64 / fps.max(1.0);
    let mult = if multiplier > 0.0 {
        multiplier
    } else if keyframe_interval_s > 0.0 {
        3.0
    } else {
        1.5
    };
    (frame_bits * mult).round().max(1.0).min(u32::MAX as f64) as u32
}

/// The `Colorspace:` field of a stream log line, from what the session negotiated rather than
/// what was asked for: a hardware encoder can refuse 4:4:4, software 4:4:4 of x264's kind
/// carries full range, and a device converting in fixed function picks the range itself.
/// Shared so the X11 and Wayland logs describe an identical session identically.
pub fn colorspace_desc(fullcolor: bool, full_range: bool) -> &'static str {
    match (fullcolor, full_range) {
        (true, true) => "I444 (Full Range)",
        (true, false) => "I444 (Limited Range)",
        (false, true) => "I420 (Full Range)",
        (false, false) => "I420 (Limited Range)",
    }
}

/// The rate-control field of a video stream's log line, as the session applies it: a backend
/// with one rate control (`fixed`, from `FrameEncoder::fixed_rate_control`) runs it at the
/// bitrate target whatever the mode, and every other session runs its mode, a CRF one with no
/// bitrate cap.
pub fn rate_desc(settings: &CaptureSettings, fixed: Option<&str>) -> String {
    match fixed {
        Some(mode) => format!("{mode} {}", settings.video_bitrate_kbps),
        None if settings.video_cbr_mode => format!("CBR {}", settings.video_bitrate_kbps),
        None => format!("CRF: {}", settings.video_crf),
    }
}

/// The paint-over field of a stream's log line, or `None` where paint-over changes nothing. The
/// cleanup reads a still screen from its content, so it acts with Turbo as without. JPEG resends
/// a still stripe at the paint-over quality when it is above the stream's. At a constant quality
/// the cleanup, where the paint-over CRF is below the stream's, moves the session to that CRF for
/// its frames and their burst, except on a backend with one rate control (`fixed`), which takes no
/// quality from the caller and sends them at its own. At a constant rate it acts wherever the rate
/// control codes coarser than the paint-over quantizer: held at it where the session holds a frame
/// at a quantizer (`holds`, `FrameEncoder::holds_quantizer`), and as a refresh and its burst at the
/// rate control's own quality elsewhere.
pub fn paint_over_desc(
    settings: &CaptureSettings,
    fixed: Option<&str>,
    holds: bool,
) -> Option<String> {
    if !settings.cleans_up() {
        return None;
    }
    if !settings.codec.is_video() {
        return (settings.paint_over_jpeg_quality > settings.jpeg_quality).then(|| {
            format!(
                "PaintOver Q: {} (Trigger: {}f)",
                settings.paint_over_jpeg_quality,
                settings.paint_over_trigger()
            )
        });
    }
    let burst = settings.video_paintover_burst_frames;
    let at_paint_over = if settings.video_cbr_mode {
        holds
    } else if settings.video_paintover_crf < settings.video_crf {
        fixed.is_none()
    } else {
        return None;
    };
    Some(if at_paint_over {
        format!(
            "PaintOver CRF: {} (Burst: {burst}f)",
            settings.video_paintover_crf
        )
    } else {
        format!("PaintOver Burst: {burst}f")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rate field says what the session runs: a backend with one rate control runs it at the
    /// bitrate target in either mode, and every other session runs its mode, CRF naming no bitrate.
    #[test]
    fn the_rate_field_names_what_the_backend_applies() {
        let crf = CaptureSettings {
            codec: Codec::H264,
            video_crf: 25,
            video_bitrate_kbps: 8000,
            ..Default::default()
        };
        let cbr = CaptureSettings {
            video_cbr_mode: true,
            ..crf.clone()
        };
        assert_eq!(rate_desc(&crf, None), "CRF: 25");
        assert_eq!(rate_desc(&cbr, None), "CBR 8000");
        for settings in [&crf, &cbr] {
            assert_eq!(rate_desc(settings, Some("VBR")), "VBR 8000");
            assert_eq!(rate_desc(settings, Some("CBR")), "CBR 8000");
        }
    }

    /// Paint-over is named where the cleanup acts, Turbo or not: its CRF where the session codes
    /// the cleanup at it, at a constant quality below the stream's on any backend that takes a
    /// quality from the caller and at a constant rate where the session holds a quantizer; its
    /// burst where the frames come at the backend's or the rate control's own quality; JPEG's
    /// higher quality; and nothing where it cannot fire.
    #[test]
    fn the_paint_over_field_names_what_paint_over_does() {
        let crf = CaptureSettings {
            codec: Codec::H264,
            video_crf: 25,
            video_paintover_crf: 18,
            video_paintover_burst_frames: 5,
            use_paint_over_quality: true,
            video_streaming_mode: false,
            ..Default::default()
        };
        let cbr = CaptureSettings {
            video_cbr_mode: true,
            ..crf.clone()
        };
        let turbo = |s: &CaptureSettings| CaptureSettings {
            video_streaming_mode: true,
            ..s.clone()
        };
        for settings in [&crf, &turbo(&crf)] {
            assert_eq!(
                paint_over_desc(settings, None, true).as_deref(),
                Some("PaintOver CRF: 18 (Burst: 5f)")
            );
            assert_eq!(
                paint_over_desc(settings, None, false).as_deref(),
                Some("PaintOver CRF: 18 (Burst: 5f)")
            );
            assert_eq!(
                paint_over_desc(settings, Some("VBR"), false).as_deref(),
                Some("PaintOver Burst: 5f")
            );
        }
        for settings in [&cbr, &turbo(&cbr)] {
            assert_eq!(
                paint_over_desc(settings, None, true).as_deref(),
                Some("PaintOver CRF: 18 (Burst: 5f)")
            );
            assert_eq!(
                paint_over_desc(settings, None, false).as_deref(),
                Some("PaintOver Burst: 5f")
            );
            assert_eq!(
                paint_over_desc(settings, Some("CBR"), false).as_deref(),
                Some("PaintOver Burst: 5f")
            );
        }
        assert_eq!(
            paint_over_desc(
                &CaptureSettings {
                    use_paint_over_quality: false,
                    ..crf.clone()
                },
                None,
                true
            ),
            None
        );
        // A constant rate cleans up with the paint-over off too, to the same quality.
        let cbr_off = CaptureSettings {
            use_paint_over_quality: false,
            ..cbr.clone()
        };
        assert_eq!(
            paint_over_desc(&cbr_off, None, false),
            paint_over_desc(&cbr, None, false)
        );
        assert_eq!(
            paint_over_desc(&cbr_off, None, true),
            paint_over_desc(&cbr, None, true)
        );
        assert_eq!(
            paint_over_desc(
                &CaptureSettings {
                    video_paintover_crf: 25,
                    ..crf.clone()
                },
                None,
                true
            ),
            None
        );
        assert_eq!(
            paint_over_desc(
                &CaptureSettings {
                    video_paintover_crf: 25,
                    ..cbr.clone()
                },
                None,
                true
            )
            .as_deref(),
            Some("PaintOver CRF: 25 (Burst: 5f)")
        );
        let jpeg = CaptureSettings {
            codec: Codec::Jpeg,
            jpeg_quality: 40,
            paint_over_jpeg_quality: 90,
            paint_over_trigger_frames: 15,
            ..crf.clone()
        };
        assert_eq!(
            paint_over_desc(&jpeg, None, false).as_deref(),
            Some("PaintOver Q: 90 (Trigger: 15f)")
        );
        assert_eq!(
            paint_over_desc(&turbo(&jpeg), None, false).as_deref(),
            Some("PaintOver Q: 90 (Trigger: 15f)")
        );
        assert_eq!(
            paint_over_desc(
                &CaptureSettings {
                    paint_over_jpeg_quality: 40,
                    ..jpeg.clone()
                },
                None,
                false
            ),
            None
        );
        assert_eq!(
            paint_over_desc(
                &CaptureSettings {
                    use_paint_over_quality: false,
                    ..jpeg
                },
                None,
                false
            ),
            None
        );
    }

    /// A held key frame past its budget is coded again six quantizer steps coarser for each
    /// doubling past it, never past 51, and one inside it is kept.
    #[test]
    fn a_held_key_past_its_budget_is_coarsened_by_the_overshoot() {
        assert_eq!(held_key_retry(18, 1000, 1000), None);
        assert_eq!(held_key_retry(18, 2000, 1000), Some(24));
        assert_eq!(held_key_retry(18, 3000, 1000), Some(28));
        assert_eq!(held_key_retry(40, 64_000, 1000), Some(51));
        assert_eq!(held_key_retry(18, 5000, 0), None, "no budget, no retry");
    }

    /// A held key frame starts where the last one says a frame of its size fits the budget: at
    /// the paint-over index where it fit there, as much coarser as a retry would take it where
    /// it did not, and at the paint-over index where there is no last one near enough to say.
    #[test]
    fn a_held_key_starts_where_the_last_one_says_it_fits() {
        assert_eq!(held_key_start(18, None, 1000), 18);
        assert_eq!(held_key_start(18, Some((800, 18)), 1000), 18);
        assert_eq!(held_key_start(18, Some((2000, 18)), 1000), 24);
        assert_eq!(
            held_key_start(18, Some((1000, 24)), 1000),
            24,
            "a retried key's own index"
        );
        assert_eq!(
            held_key_start(18, Some((100, 40)), 1000),
            18,
            "too far to say"
        );
    }

    /// A session lands on the codec it asked for, or on the next video codec this host serves;
    /// JPEG is the ladder's last leg, not its answer to one missing encoder.
    #[test]
    fn a_session_lands_on_a_video_codec_the_host_serves() {
        let mut settings = CaptureSettings {
            width: 256,
            height: 128,
            codec: Codec::Av1,
            use_cpu: true,
            ..Default::default()
        };
        let encoder = select_frame_encoder(
            &mut settings,
            FrameSource::Host { rgba: false },
            None,
            "test",
        );
        println!("asked for av1, landed on {}", settings.codec.display());
        assert_ne!(
            settings.codec,
            Codec::Jpeg,
            "this build encodes video in software"
        );
        assert!(software_encoder(settings.codec).is_some());
        // H.264 comes back as the caller's own CPU path rather than a full-frame session.
        assert!(encoder.is_some() || settings.codec == Codec::H264);
    }

    /// A codec with no path on this host falls through the video codecs it does serve, the
    /// encode node's hardware ones first and most efficient first, the software ones by encode
    /// time, software H.264 leading them.
    #[test]
    fn a_codec_without_a_path_falls_through_the_served_video_codecs() {
        let software: Vec<Codec> = SOFTWARE_ORDER
            .iter()
            .copied()
            .filter(|&codec| codec != Codec::Av1 && software_encoder(codec).is_some())
            .collect();
        assert_eq!(software.first(), Some(&Codec::H264));
        assert_eq!(
            fallback_codecs(Codec::Av1, &[]),
            software,
            "software H.264 leads the software rungs"
        );
        assert_eq!(
            fallback_codecs(Codec::Av1, &[Codec::H264]),
            software,
            "an engine's H.264 leads either way"
        );
        assert!(!fallback_codecs(Codec::Av1, &[Codec::Av1]).contains(&Codec::Av1));
        // An engine's codecs go most efficient first, whatever the build encodes in software.
        let engine = [Codec::H264, Codec::H265, Codec::Av1];
        assert_eq!(
            &fallback_codecs(Codec::Vp8, &engine)[..3],
            &[Codec::Av1, Codec::H265, Codec::H264]
        );
    }

    /// The probe of every software encoder prints nothing where the host sees it, though a
    /// library announces itself on open (kvazaar's preset and SIMD banner, SVT-AV1's
    /// configuration, x265's NUMA warnings in a container).
    #[test]
    fn the_software_probe_prints_nothing() {
        if std::env::var_os("PIXELFLUX_PROBE_CHILD").is_some() {
            for codec in Codec::VIDEO {
                encodes_in_child(codec);
            }
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "encoders::tests::the_software_probe_prints_nothing",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("PIXELFLUX_PROBE_CHILD", "1")
            .output()
            .unwrap();
        assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"));
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "",
            "the probes wrote to the host's stderr"
        );
    }

    /// A capture start takes what its node's probe settled rather than opening the backend
    /// again: node 99 has no render device, so it reads as NVENC's, which has no VP8 engine.
    /// A device whose sessions other processes hold settles nothing, so the probe is asked
    /// until it does before the capture starts.
    #[test]
    fn a_session_takes_the_answer_its_node_probe_settled() {
        let settled = (0..300)
            .find_map(|_| match probe_node(99) {
                Err((_, e)) if e == nvenc::SESSIONS_TAKEN => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    None
                }
                Err((backend, e)) => {
                    Some(format!("{} VP8 did not open: {e}", backend.to_uppercase()))
                }
                Ok(_) => Some("render node 99 has no VP8 engine".to_string()),
            })
            .expect("the device had no NVENC session to spare for 30 s");
        let report = crate::report::StreamReport::new("x11");
        let _scope = crate::report::enter(&report);
        let mut settings = CaptureSettings {
            width: 256,
            height: 128,
            codec: Codec::Vp8,
            encode_node_index: 99,
            ..Default::default()
        };
        let encoder = select_frame_encoder(
            &mut settings,
            FrameSource::Host { rgba: false },
            None,
            "test",
        );
        assert!(
            encoder.is_some_and(|enc| !enc.is_hardware()),
            "VP8 comes up in software"
        );
        assert_eq!(report.info().encoder_reason, settled);
    }

    /// The software encoders that take a 4:4:4 session: x264 and x265, and libvpx for VP9 alone;
    /// AV1 and VP8 encode 4:2:0 whatever is asked.
    #[test]
    fn software_fullcolor_follows_the_library() {
        assert_eq!(
            software_fullcolor(Codec::H264),
            software_library(Codec::H264) == "x264"
        );
        assert_eq!(
            software_fullcolor(Codec::H265),
            software_library(Codec::H265) == "x265"
        );
        assert_eq!(
            software_fullcolor(Codec::Vp9),
            software_library(Codec::Vp9) == "libvpx" && vpx::encodes_444()
        );
        assert!(!software_fullcolor(Codec::Av1) && !software_fullcolor(Codec::Vp8));
    }

    /// A child's fate is the finding: a fatal signal, and only that, reads as not surviving.
    #[test]
    fn a_child_survives_unless_a_signal_takes_it() {
        assert!(survives_in_child(|| {}));
        assert!(survives_in_child(|| unsafe { libc::_exit(3) }));
        assert!(!survives_in_child(|| unsafe {
            libc::raise(libc::SIGILL);
        }));
    }

    /// A probe forked while other threads open and close SVT-AV1 sessions still finds the
    /// encoder. The threads beside it stop opening sessions while one is pending, as nothing
    /// opens one beside the import-time probe: a child forked while they run between sessions
    /// inherits locks their threads hold and no thread of its own releases, and runs out its
    /// minute.
    #[test]
    fn the_av1_probe_survives_sessions_opening_beside_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let settings = CaptureSettings {
            width: 64,
            height: 64,
            target_fps: 30.0,
            codec: Codec::Av1,
            ..Default::default()
        };
        let stop = AtomicBool::new(false);
        let probing = AtomicBool::new(false);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        if probing.load(Ordering::Acquire) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            continue;
                        }
                        drop(software_session(&settings, Codec::Av1, false));
                    }
                });
            }
            let found = (0..4).all(|_| {
                probing.store(true, Ordering::Release);
                let found = encodes_in_child(Codec::Av1);
                probing.store(false, Ordering::Release);
                found
            });
            stop.store(true, Ordering::Relaxed);
            assert!(found);
        });
    }

    /// A probe forked while other threads start and stop still finds the encoder: a thread the
    /// child started would wait on the lock the standard library holds while it sets a thread
    /// up, which another thread of the parent may have held at the fork.
    #[test]
    fn the_av1_probe_survives_threads_starting_beside_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            for _ in 0..16 {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = std::thread::spawn(|| {}).join();
                    }
                });
            }
            let found = (0..200).all(|_| encodes_in_child(Codec::Av1));
            stop.store(true, Ordering::Relaxed);
            assert!(found);
        });
    }

    /// The build serves every video codec in software, JPEG never, and the GPL-free build
    /// names neither x264 nor x265.
    #[test]
    fn software_encoders_follow_the_build() {
        use rayon::prelude::*;
        let _: u32 = (0..8u32).into_par_iter().sum();
        let h264 = software_encoder(Codec::H264).expect("H.264 is always served");
        assert_eq!(
            h264.library,
            if cfg!(feature = "gpl") {
                "x264"
            } else {
                "openh264"
            }
        );
        assert_eq!(software_library(Codec::H264), h264.library);
        assert_eq!(software_library(Codec::Jpeg), "none");
        assert_eq!(software_encoder(Codec::Jpeg), None);
        assert_eq!(
            software_library(Codec::H265),
            if cfg!(feature = "gpl") {
                "x265"
            } else {
                "kvazaar"
            }
        );
        assert_eq!(software_library(Codec::Vp8), "libvpx");
        assert_eq!(software_library(Codec::Vp9), "libvpx");
        assert_eq!(software_library(Codec::Av1), "svt-av1");
        assert_eq!(software_fullcolor(Codec::H264), cfg!(feature = "gpl"));
        assert_eq!(software_fullcolor(Codec::H265), cfg!(feature = "gpl"));
        assert!(!software_fullcolor(Codec::Vp8) && !software_fullcolor(Codec::Av1));
        assert_eq!(software_fullcolor(Codec::Vp9), vpx::encodes_444());
    }

    /// A session's range is the session's to report, not something read off whether its
    /// encoder is hardware. The two part on a 4:2:0 software H.264 session, which is
    /// limited range while the encoder is software, so a description taking the second for
    /// the first names a range the bitstream does not carry. All four pairings have a name
    /// of their own for that reason, rather than 4:2:0 falling through to one.
    #[test]
    fn a_session_reports_the_range_it_converted_at() {
        assert_eq!(colorspace_desc(true, true), "I444 (Full Range)");
        assert_eq!(colorspace_desc(true, false), "I444 (Limited Range)");
        assert_eq!(colorspace_desc(false, true), "I420 (Full Range)");
        assert_eq!(colorspace_desc(false, false), "I420 (Limited Range)");

        let mut settings = CaptureSettings {
            codec: Codec::H264,
            video_fullcolor: false,
            ..Default::default()
        };
        assert!(
            !session_full_range(None, &settings),
            "software H.264 at 4:2:0 converts at limited range"
        );
        assert_eq!(
            colorspace_desc(
                session_fullcolor(None, &settings),
                session_full_range(None, &settings)
            ),
            "I420 (Limited Range)"
        );

        settings.video_fullcolor = true;
        assert_eq!(
            session_full_range(None, &settings),
            software_fullcolor(Codec::H264),
            "software H.264 at 4:4:4 converts at full range where the build carries it"
        );
    }
}

/// One full-frame encoder session, whichever backend produced it, so the render and delivery
/// code passes "the frame encoder" around without caring which vendor path or library
/// produced the frames.
#[allow(clippy::large_enum_variant)]
pub enum FrameEncoder {
    Nvenc(NvencEncoder),
    /// A VA-API session on a render node, hardware.
    Vaapi(vaapi::VaapiEncoder),
    /// libvpx VP8 or VP9, software.
    Vpx(vpx::VpxEncoder),
    /// x265 or kvazaar, software.
    Hevc(hevc::HevcEncoder),
    /// SVT-AV1, software.
    Av1(svtav1::SvtAv1Encoder),
    /// Tegra's encoder, reached through the vendor V4L2 library.
    #[cfg(target_arch = "aarch64")]
    Tegra(tegra::TegraEncoder),
    /// A stateful V4L2 M2M encoder, driven through the kernel interface directly.
    V4l2m2m(v4l2m2m::V4l2M2mEncoder),
}

/// Apply one expression to the session behind whichever variant `self` is.
macro_rules! each {
    ($self:expr, $enc:ident => $e:expr) => {
        match $self {
            FrameEncoder::Nvenc($enc) => $e,
            FrameEncoder::Vaapi($enc) => $e,
            FrameEncoder::Vpx($enc) => $e,
            FrameEncoder::Hevc($enc) => $e,
            FrameEncoder::Av1($enc) => $e,
            #[cfg(target_arch = "aarch64")]
            FrameEncoder::Tegra($enc) => $e,
            FrameEncoder::V4l2m2m($enc) => $e,
        }
    };
}

impl FrameEncoder {
    /// The codec the session emits.
    pub fn codec(&self) -> Codec {
        each!(self, enc => enc.codec())
    }

    /// Whether the session encodes on a GPU or a hardware engine.
    pub fn is_hardware(&self) -> bool {
        !matches!(
            self,
            FrameEncoder::Vpx(_) | FrameEncoder::Hevc(_) | FrameEncoder::Av1(_)
        )
    }

    /// The rate control the session runs whatever mode it was given, for a backend that has one
    /// alone: a V4L2 M2M device offers variable bitrate only, and Tegra's encoder is driven at a
    /// constant one. `None` where the session runs the mode it was given.
    pub fn fixed_rate_control(&self) -> Option<&'static str> {
        match self {
            #[cfg(target_arch = "aarch64")]
            FrameEncoder::Tegra(_) => Some("CBR"),
            FrameEncoder::V4l2m2m(_) => Some("VBR"),
            _ => None,
        }
    }

    /// The backend as the logs name it: `NVENC`, `VAAPI`, or the software library.
    pub fn backend_name(&self) -> &'static str {
        match self {
            FrameEncoder::Nvenc(_) => "NVENC",
            FrameEncoder::Vaapi(_) => "VAAPI",
            FrameEncoder::Vpx(enc) => enc.library(),
            FrameEncoder::Hevc(enc) => enc.library(),
            FrameEncoder::Av1(enc) => enc.library(),
            #[cfg(target_arch = "aarch64")]
            FrameEncoder::Tegra(_) => "TEGRA",
            FrameEncoder::V4l2m2m(_) => "V4L2M2M",
        }
    }

    /// Whether the session negotiated 4:4:4 chroma.
    pub fn is_fullcolor(&self) -> bool {
        each!(self, enc => enc.is_fullcolor())
    }

    /// The bits per sample the session negotiated: 10 on a VA-API session that opened a
    /// 10-bit profile and on a software one whose library codes them, 8 everywhere else.
    pub fn bit_depth(&self) -> u32 {
        match self {
            FrameEncoder::Nvenc(enc) => enc.bit_depth(),
            FrameEncoder::Vaapi(enc) => enc.bit_depth(),
            FrameEncoder::Vpx(enc) => enc.bit_depth(),
            FrameEncoder::Hevc(enc) => enc.bit_depth(),
            FrameEncoder::Av1(enc) => enc.bit_depth(),
            _ => 8,
        }
    }

    /// Whether the session signals full range: a software 4:4:4 of x264's kind, and the V4L2
    /// M2M sessions whose firmware converts at full range and offers no way to ask for another.
    pub fn is_full_range(&self) -> bool {
        each!(self, enc => enc.is_full_range())
    }

    /// Apply a live bitrate / VBV / frame-rate change. `Err` means the session lost its codec
    /// context and has to be rebuilt.
    pub fn reconfigure_rate(&mut self, settings: &CaptureSettings) -> Result<(), String> {
        match self {
            FrameEncoder::Nvenc(enc) => {
                enc.reconfigure_rate(settings);
                Ok(())
            }
            FrameEncoder::Vaapi(enc) => enc.reconfigure_rate(settings),
            FrameEncoder::Vpx(enc) => enc.reconfigure_rate(settings),
            FrameEncoder::Hevc(enc) => enc.reconfigure_rate(settings),
            FrameEncoder::Av1(enc) => enc.reconfigure_rate(settings),
            #[cfg(target_arch = "aarch64")]
            FrameEncoder::Tegra(enc) => enc.reconfigure_rate(settings),
            FrameEncoder::V4l2m2m(enc) => enc.reconfigure_rate(settings),
        }
    }

    /// Encode one packed host frame (`stride` bytes per row; `rgba` names R,G,B,A byte order,
    /// otherwise B,G,R,A) at quantizer `qp`, as a key frame when `force_idr`.
    pub fn encode_host(
        &mut self,
        pixels: &[u8],
        stride: usize,
        rgba: bool,
        frame_number: u64,
        qp: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        match self {
            FrameEncoder::Nvenc(enc) => {
                enc.encode_cpu_packed(pixels, stride, rgba, frame_number, qp, force_idr)
            }
            FrameEncoder::Vaapi(enc) => {
                enc.encode_host(pixels, stride, rgba, frame_number, qp, force_idr)
            }
            FrameEncoder::Vpx(enc) => {
                enc.encode_host(pixels, stride, rgba, frame_number, qp, force_idr)
            }
            FrameEncoder::Hevc(enc) => {
                enc.encode_host(pixels, stride, rgba, frame_number, qp, force_idr)
            }
            FrameEncoder::Av1(enc) => {
                enc.encode_host(pixels, stride, rgba, frame_number, qp, force_idr)
            }
            #[cfg(target_arch = "aarch64")]
            FrameEncoder::Tegra(enc) => {
                enc.encode_host(pixels, stride, rgba, frame_number, qp, force_idr)
            }
            FrameEncoder::V4l2m2m(enc) => {
                enc.encode_host(pixels, stride, rgba, frame_number, qp, force_idr)
            }
        }
    }

    /// Hash the next host frame's bands of `rows` rows where the encoder holds it, beside its
    /// encode (NVENC: `NvencEncoder::hash_next_upload`); false where it cannot, which leaves the
    /// hash to the caller.
    pub fn hash_next_upload(&mut self, rows: u32) -> bool {
        match self {
            FrameEncoder::Nvenc(enc) => enc.hash_next_upload(rows),
            _ => false,
        }
    }

    /// The band hashes `hash_next_upload` asked for, once the frame is coded.
    pub fn take_upload_hashes(&mut self) -> Option<Vec<u64>> {
        match self {
            FrameEncoder::Nvenc(enc) => enc.take_upload_hashes(),
            _ => None,
        }
    }

    /// The quality index (the H.26x quantizer scale `video_crf` uses) the last frame was coded at,
    /// where the session reports its quantizer: what a constant-rate session's picture is worth
    /// against the paint-over quality.
    pub fn last_quality(&self) -> Option<u32> {
        match self {
            FrameEncoder::Nvenc(enc) => enc.last_quality(),
            FrameEncoder::Vaapi(enc) => enc.last_quality(),
            FrameEncoder::Vpx(enc) => enc.last_quality(),
            FrameEncoder::Hevc(enc) => enc.last_quality(),
            FrameEncoder::Av1(enc) => enc.last_quality(),
            _ => None,
        }
    }

    /// The bytes of the last frame the rate control coded, held frames aside, where the session
    /// reports them (NVENC, VA-API, x265 and kvazaar, libvpx's VP9): what tells a constant-rate
    /// cleanup that runs through the rate control whether it has converged on a still screen
    /// (`pipeline::decide_hw_fullframe`), with `last_quality` where the session reports that
    /// too.
    pub fn last_size(&self) -> Option<usize> {
        match self {
            FrameEncoder::Nvenc(enc) => enc.last_size(),
            FrameEncoder::Vaapi(enc) => enc.last_size(),
            FrameEncoder::Vpx(enc) => enc.last_size(),
            FrameEncoder::Hevc(enc) => enc.last_size(),
            _ => None,
        }
    }

    /// Have the next frame's reconstruction measured against its source, where the session
    /// can (`last_psnr`).
    pub fn measure(&mut self) {
        if let FrameEncoder::Vaapi(enc) = self {
            enc.measure();
        }
    }

    /// Whether `measure` has the session measure a frame: VA-API, which reads its
    /// reconstruction back, where the driver's surfaces let it.
    pub fn measures(&self) -> bool {
        match self {
            FrameEncoder::Vaapi(enc) => enc.measures(),
            _ => false,
        }
    }

    /// The luma PSNR of the last frame measured (`measure`), where the session reads its
    /// reconstruction back: VA-API. What a constant-rate cleanup through the rate control
    /// ends on where it has one, since a picture that stopped improving needs no more frames
    /// whatever quantizer the driver reports.
    pub fn last_psnr(&self) -> Option<f32> {
        match self {
            FrameEncoder::Vaapi(enc) => enc.last_psnr(),
            _ => None,
        }
    }

    /// Take the next frame back where it comes out past `bytes` and code it again within them:
    /// a band held at a quantizer (`hold_quantizer`) in a shorter band, a frame of the rate
    /// control's at the coarsest quantizer, refining nothing. NVENC, which leaves the frame out of
    /// the predictions as one a client lost, does. VA-API codes the frame at the rate that spends
    /// `bytes` in the cleanup's frame time instead (`follows_cap`); elsewhere the frame stands as
    /// coded.
    pub fn cap(&mut self, bytes: usize) {
        match self {
            FrameEncoder::Nvenc(enc) => enc.cap(bytes),
            FrameEncoder::Vaapi(enc) => enc.cap(bytes),
            FrameEncoder::Vpx(enc) => enc.cap(bytes),
            _ => {}
        }
    }

    /// Whether a still screen's cleanup frame is coded at the rate its cap runs at (`cap`), so
    /// the cleanup judges it against that rate's budget: VA-API at a constant rate, which takes
    /// a new target with a picture, and libvpx's VP9, which takes one live. NVENC takes a frame
    /// back instead.
    pub fn follows_cap(&self) -> bool {
        match self {
            FrameEncoder::Vaapi(enc) => enc.follows_cap(),
            FrameEncoder::Vpx(enc) => enc.follows_cap(),
            _ => false,
        }
    }

    /// The quantizer steps finer than the paint-over one at which the rate control's frames come
    /// to the paint-over quality, where a constant-rate cleanup runs through it: the software
    /// HEVC encoders' predicted frames at a quantizer leave the motion's coarse blocks as they
    /// are, and x265's cleanup ended at 45-46 dB at the paint-over quantizer where its key frame
    /// there decodes at 50 (`HevcEncoder::reach_finer`). Elsewhere none.
    pub fn reach_finer(&self) -> u32 {
        match self {
            FrameEncoder::Hevc(enc) => enc.reach_finer(),
            _ => 0,
        }
    }

    /// Whether a constant-rate cleanup through the rate control ends on a key frame held at the
    /// paint-over quantizer after a large change and a longer stillness: where the rate control
    /// cannot refine a still screen in the frames' budget and the session says nothing of how far
    /// it got (`VaapiEncoder::ends_cleanup_on_key`). Elsewhere a refresh or the rate control's
    /// own frames do, and a key frame would only cost more (libvpx's VP9 key frame of 1080p text
    /// came out at 231 kB and 30.9 dB at its coarsest).
    pub fn ends_cleanup_on_key(&self) -> bool {
        match self {
            FrameEncoder::Vaapi(enc) => enc.ends_cleanup_on_key(),
            _ => false,
        }
    }

    /// Where the last frame was taken back and coded again (`cap`): how far its held band then
    /// reached, or 0 for a frame of the rate control's.
    pub fn refit(&self) -> Option<f64> {
        match self {
            FrameEncoder::Nvenc(enc) => enc.refit(),
            _ => None,
        }
    }

    /// Whether a frame left a held band out of every prediction with no band of its own to take
    /// it back into since this was last asked: NVENC, whose anchors predict from the newest frame
    /// every client was sent (`NvencEncoder::take_lost_cleanup`).
    pub fn take_lost_cleanup(&mut self) -> bool {
        match self {
            FrameEncoder::Nvenc(enc) => enc.take_lost_cleanup(),
            _ => false,
        }
    }

    /// The bytes of the last frame held at a quantizer (0 before one), where the session holds a
    /// band of a frame at it (`hold_quantizer`): NVENC's H.264, HEVC and AV1 sessions, through a
    /// QP delta map, and libvpx's VP8, through a region-of-interest map.
    pub fn band_size(&self) -> Option<usize> {
        match self {
            FrameEncoder::Nvenc(enc) => enc.band_size(),
            FrameEncoder::Vpx(enc) => enc.band_size(),
            _ => None,
        }
    }

    /// How the session refines blocks of a frame of its rate control's at a quality of their
    /// own (`refine_blocks`, `ledger::Refiner`), so a refined block comes to what the region's
    /// own cleanup reaches; `None` where it does not: NVENC's H.264, HEVC, and AV1 through the
    /// QP delta map (`nvenc::REFINE_FINER`), libvpx's VP8 through its region-of-interest map
    /// (`vpx::REFINE_FINER`, which says why not VP9), x265 through its per-block quantizer
    /// offsets and kvazaar through its per-tree-block map (`HevcEncoder::refine_finer`). x264's
    /// session refines within `software::encode_cpu` (`ledger::REFINE_FINER`).
    pub fn refines(&self) -> Option<crate::ledger::Refiner> {
        use crate::ledger::Refiner;
        match self {
            FrameEncoder::Nvenc(enc) => {
                enc.refines().then_some(Refiner::finer(nvenc::REFINE_FINER))
            }
            FrameEncoder::Vpx(enc) => {
                (enc.codec() == Codec::Vp8).then_some(Refiner::finer(vpx::REFINE_FINER))
            }
            FrameEncoder::Hevc(enc) => enc.refines().then(|| Refiner::finer(enc.refine_finer())),
            FrameEncoder::Vaapi(enc) => enc.refines(),
            _ => None,
        }
    }

    /// Code the blocks `refinement` names in the next frame, where it is the rate control's, at
    /// its target quality (`ledger`), where `refines`.
    pub fn refine_blocks(&mut self, refinement: &crate::ledger::Refinement) {
        match self {
            FrameEncoder::Nvenc(enc) => enc.refine_blocks(refinement),
            FrameEncoder::Vpx(enc) => enc.refine_blocks(refinement),
            FrameEncoder::Hevc(enc) => enc.refine_blocks(refinement),
            FrameEncoder::Vaapi(enc) => enc.refine_blocks(refinement),
            _ => {}
        }
    }

    /// Whether `hold_quantizer` holds a frame at the quantizer asked for under the session's rate
    /// control: NVENC, libvpx's VP8, and SVT-AV1 at a constant rate (where the release takes a
    /// new target with a picture) do; x265, VA-API, and libvpx's VP9 only at a constant
    /// quantizer (their `holds_quantizer` and `hold_quantizer` say why not at a constant rate,
    /// VP9's that its rate control refines a still screen within the rate); kvazaar, Tegra,
    /// and a stateful V4L2 device take no quantizer from the caller. The cleanup of a
    /// constant-quality session moves the session's quality instead of holding a frame
    /// (`pipeline::decide_constant_quality`), so this is read at a constant rate.
    pub fn holds_quantizer(&self) -> bool {
        match self {
            FrameEncoder::Nvenc(_) => true,
            FrameEncoder::Vpx(enc) => enc.holds_quantizer(),
            FrameEncoder::Vaapi(enc) => !enc.is_cbr(),
            FrameEncoder::Hevc(enc) => enc.holds_quantizer(),
            FrameEncoder::Av1(enc) => enc.holds_quantizer(),
            #[cfg(target_arch = "aarch64")]
            FrameEncoder::Tegra(_) => false,
            FrameEncoder::V4l2m2m(_) => false,
        }
    }

    /// Whether a change of the session's constant quality re-opens it, so the frame it applies
    /// to is a key frame: x265, kvazaar, and SVT-AV1 take no new quality while they run. The
    /// cleanup of a constant-quality session counts such a refresh as its key frame.
    pub fn reopens_on_quality(&self) -> bool {
        matches!(self, FrameEncoder::Hevc(_) | FrameEncoder::Av1(_))
    }

    /// Whether a constant-rate cleanup ends on a key frame after a large change and a longer
    /// stillness (`pipeline::decide_hw_fullframe`). SVT-AV1 holds only a key frame, so that is its
    /// cleanup. libvpx's VP8 holds the refresh before it, predicted frames that restore the
    /// picture whole (in bands where the rate leaves room, `VpxEncoder::band_size`), and its key
    /// frame only sent the picture again: 1080p text at 0.25 Mbit/s
    /// went quiet on 1.3 MB without it against 2.7, at the same 42 dB, since the key frame came
    /// out capped coarser than the picture (216 kB at 21.6 dB) and a second refresh restored it.
    pub fn cleans_up_with_key(&self) -> bool {
        !matches!(self, FrameEncoder::Vpx(enc) if enc.codec() == Codec::Vp8)
    }

    /// Encode the next frame at the quantizer the quality index `crf` selects whatever the rate
    /// control, and leave the session's own rate control and quality as they were for the frame
    /// after: the cleanup of a still screen, where `holds_quantizer`. A session whose engine takes
    /// no quantizer from the caller (Tegra, a stateful V4L2 device) codes that frame under its own
    /// rate control. `band`, the share of the picture from and to in raster order, confines the
    /// quantizer to that band where `band_size` says the session holds one, the rest of the frame
    /// held at the coarsest quantizer.
    pub fn hold_quantizer(&mut self, crf: u32, band: Option<(f64, f64)>) {
        match self {
            FrameEncoder::Nvenc(enc) => enc.hold_quantizer(crf, band),
            FrameEncoder::Vaapi(enc) => enc.hold_quantizer(crf),
            FrameEncoder::Vpx(enc) => enc.hold_quantizer(crf, band),
            FrameEncoder::Hevc(enc) => enc.hold_quantizer(crf),
            FrameEncoder::Av1(enc) => enc.hold_quantizer(crf),
            #[cfg(target_arch = "aarch64")]
            FrameEncoder::Tegra(_) => {}
            FrameEncoder::V4l2m2m(_) => {}
        }
    }

    /// The data one call returned, cut at the units it carries, each with its own frame's id and
    /// reference: whole and labeled `encoded` from a session that hands back the frame it
    /// encoded, one unit per earlier frame from one that hands them back late (Tegra), so what a
    /// consumer reports lost is the unit it dropped and nothing else.
    pub fn delivered_units(
        &self,
        data: Vec<u8>,
        encoded: u16,
    ) -> Vec<(Vec<u8>, u16, reference::Reference)> {
        #[cfg(target_arch = "aarch64")]
        if let FrameEncoder::Tegra(enc) = self {
            match enc.delivered_units() {
                [] => {}
                &[(id, reference, _)] => return vec![(data, id, reference)],
                units => {
                    let mut start = 0;
                    return units
                        .iter()
                        .map(|&(id, reference, end)| {
                            let unit = data[start..end].to_vec();
                            start = end;
                            (unit, id, reference)
                        })
                        .collect();
                }
            }
        }
        vec![(data, encoded, self.last_reference())]
    }

    /// The frame the last delivered frame predicted from.
    /// The access unit a still screen leaves inside an engine that emits a frame only once the
    /// next is queued: empty from every other backend, whose units come with their frame.
    #[cfg_attr(not(target_arch = "aarch64"), allow(unused_variables))]
    pub fn push_held(&mut self, frame_number: u64) -> Result<Vec<u8>, String> {
        match self {
            #[cfg(target_arch = "aarch64")]
            FrameEncoder::Tegra(enc) => enc.push_held(frame_number),
            _ => Ok(Vec::new()),
        }
    }

    pub fn holds_frame(&self) -> bool {
        match self {
            #[cfg(target_arch = "aarch64")]
            FrameEncoder::Tegra(enc) => enc.holds_frame(),
            _ => false,
        }
    }

    pub fn last_reference(&self) -> reference::Reference {
        each!(self, enc => enc.last_reference())
    }

    /// Leave frame `frame_id` and every frame after it out of the predictions, so the next
    /// frame decodes for a client that lost it. False when the session cannot, and the caller
    /// codes a key frame instead.
    pub fn invalidate_reference(&mut self, frame_id: u16) -> bool {
        each!(self, enc => enc.invalidate_reference(frame_id))
    }

    /// Apply what the consumers say of a frame: a loss as `invalidate_reference`, a frame every
    /// one of them holds or was sent to a session keeping long-term references
    /// (`ReferenceWindow::acknowledge`). False where the session codes a key frame instead.
    pub fn take_report(&mut self, report: reference::ReferenceReport) -> bool {
        match (self, report) {
            (FrameEncoder::Nvenc(enc), report) => enc.take_report(report),
            (FrameEncoder::Vaapi(enc), report) => enc.take_report(report),
            (FrameEncoder::Vpx(enc), reference::ReferenceReport::Held(id)) => {
                enc.acknowledge_reference(id, true);
                true
            }
            (FrameEncoder::Vpx(enc), reference::ReferenceReport::Sent(id)) => {
                enc.acknowledge_reference(id, false);
                true
            }
            (FrameEncoder::Av1(enc), reference::ReferenceReport::Held(id)) => {
                enc.acknowledge_reference(id, true);
                true
            }
            (FrameEncoder::Av1(enc), reference::ReferenceReport::Sent(id)) => {
                enc.acknowledge_reference(id, false);
                true
            }
            (
                enc,
                reference::ReferenceReport::Lost(frame_id)
                | reference::ReferenceReport::Withheld(frame_id),
            ) => enc.invalidate_reference(frame_id),
            _ => true,
        }
    }

    /// Encode one Wayland dmabuf in place (a zero-copy session).
    pub fn encode_dmabuf(
        &mut self,
        dmabuf: &Dmabuf,
        frame_number: u64,
        qp: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        match self {
            FrameEncoder::Nvenc(enc) => enc.encode(dmabuf, frame_number, qp, force_idr),
            FrameEncoder::Vaapi(enc) => enc.encode_dmabuf(dmabuf, frame_number, qp, force_idr),
            _ => Err(format!(
                "the {} session takes host frames",
                self.backend_name()
            )),
        }
    }

    /// `encode_dmabuf`, for a buffer the X server signals the session's blit semaphore after
    /// blitting into (`blit_semaphore_fd`): the frame's GPU work waits on it on the GPU.
    pub fn encode_dmabuf_after_blit(
        &mut self,
        dmabuf: &Dmabuf,
        frame_number: u64,
        qp: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        match self {
            FrameEncoder::Nvenc(enc) => enc.encode_after_blit(dmabuf, frame_number, qp, force_idr),
            _ => Err(format!(
                "the {} session waits on no blit semaphore",
                self.backend_name()
            )),
        }
    }

    /// A semaphore for the X server to signal after each blit, as the fd it imports, for
    /// `encode_dmabuf_after_blit` to wait on; None for a session that cannot wait on one (NVENC
    /// alone can, through CUDA).
    pub fn blit_semaphore_fd(&mut self) -> Option<Result<std::os::fd::OwnedFd, String>> {
        match self {
            FrameEncoder::Nvenc(enc) => Some(enc.blit_semaphore_fd()),
            _ => None,
        }
    }
}

/// The full-frame software session of `codec` on host frames in the byte order `rgba` names.
pub fn software_session(
    settings: &CaptureSettings,
    codec: Codec,
    rgba: bool,
) -> Result<FrameEncoder, String> {
    Ok(match codec {
        Codec::Vp8 | Codec::Vp9 => FrameEncoder::Vpx(vpx::VpxEncoder::new(settings, codec, rgba)?),
        Codec::H265 => FrameEncoder::Hevc(hevc::HevcEncoder::new(settings, rgba)?),
        Codec::Av1 => FrameEncoder::Av1(svtav1::SvtAv1Encoder::new(settings, rgba)?),
        Codec::H264 | Codec::Jpeg => {
            return Err(format!("{} takes the CPU path", codec.display()));
        }
    })
}

/// The chroma sampling of a session as the logs name it.
pub fn chroma_name(fullcolor: bool) -> &'static str {
    if fullcolor { "4:4:4" } else { "4:2:0" }
}

/// The kernel driver's name out of the sysfs link `get_gpu_driver` read, `unknown` without one.
pub fn driver_name(driver: &str) -> &str {
    match driver.rsplit('/').next() {
        Some(name) if !name.is_empty() => name,
        _ => "unknown",
    }
}

/// How a session's frames arrive: Wayland dmabufs (with the EGL display NVENC imports them
/// through), or packed host pixels in R,G,B,A (`rgba`) or B,G,R,A byte order.
#[derive(Clone, Copy)]
pub enum FrameSource {
    Dmabuf { egl_display: *const c_void },
    Host { rgba: bool },
}

/// How many threads a software session encodes on, for its `Encoder:` line.
fn threads_desc(threads: i32) -> String {
    if threads == 1 {
        "1 thread".to_string()
    } else {
        format!("{threads} threads")
    }
}

/// Choose and build the full-frame encoder of a session, or `None` when the CPU path
/// (`software::encode_cpu`) serves it: JPEG, or H.264 through the build's software encoder. One ladder for X11, Wayland zero-copy, and Wayland readback, so the three
/// cannot pick differently for the same settings:
///
/// 1. Unless software encoding is forced (`use_cpu`, or encode node `-1`), the hardware
///    backend the encode node's driver selects — NVENC on the NVIDIA driver, VA-API otherwise,
///    and on a Jetson the vendor V4L2 encoder before either, since that board publishes no
///    render node driver to select on. A compatible NVENC session handed over in `prior` is
///    reconfigured in place instead of rebuilt. A hardware refusal is logged and falls through.
/// 2. A dmabuf source stops here: software cannot read dmabufs, and the caller's readback path
///    then runs this ladder again with host frames.
/// 3. The software encoder of the codec, except JPEG and H.264, whose software path is the CPU
///    path's.
/// 4. Where the codec has no path at all, the other video codecs this host serves, the encode
///    node's hardware ones before the build's software ones, software H.264 among the latter,
///    and JPEG only past all of them. `settings.codec` names what came up.
pub fn select_frame_encoder(
    settings: &mut CaptureSettings,
    source: FrameSource,
    prior: Option<FrameEncoder>,
    tag: &str,
) -> Option<FrameEncoder> {
    let requested = settings.codec;
    if !requested.is_video() {
        return None;
    }
    if let Some(enc) = select_for_codec(settings, source, prior, tag) {
        return Some(enc);
    }
    // A dmabuf source stops here, its readback path running the ladder again on host frames,
    // and H.264 has come up on the CPU path the caller encodes itself.
    let FrameSource::Host { .. } = source else {
        return None;
    };
    if requested == Codec::H264 {
        return None;
    }
    let hardware: Vec<Codec> = if settings.use_cpu || settings.encode_node_index == -1 {
        Vec::new()
    } else {
        hardware_encoders(settings.encode_node_index)
            .into_iter()
            .map(|(codec, ..)| codec)
            .collect()
    };
    eprintln!(
        "[{tag}] No {} path on this host; trying the video codecs it serves.",
        requested.display()
    );
    for codec in fallback_codecs(requested, &hardware) {
        settings.codec = codec;
        if let Some(enc) = select_for_codec(settings, source, None, tag) {
            return Some(enc);
        }
        // Software H.264 is the caller's own path.
        if codec == Codec::H264 && software_encoder(Codec::H264).is_some() {
            return None;
        }
    }
    eprintln!("[{tag}] No video encoder on this host. Encoding JPEG instead.");
    settings.codec = Codec::Jpeg;
    None
}

/// The video codecs by compression efficiency: the order a fallthrough tries the ones an engine
/// on the encode node carries, where CPU is no constraint.
const HARDWARE_ORDER: [Codec; 5] = [Codec::Av1, Codec::H265, Codec::Vp9, Codec::H264, Codec::Vp8];

/// The video codecs by the measured time per frame of their software encoders, x264 to libvpx
/// VP9: the order a fallthrough tries the ones the build encodes in software.
const SOFTWARE_ORDER: [Codec; 5] = [Codec::H264, Codec::Av1, Codec::Vp8, Codec::H265, Codec::Vp9];

/// The video codecs a capture falls through to where the one it asked for has no path on this
/// host: those an engine on the encode node carries, named in `hardware`, in `HARDWARE_ORDER`,
/// then those the build encodes in software, in `SOFTWARE_ORDER`.
fn fallback_codecs(requested: Codec, hardware: &[Codec]) -> Vec<Codec> {
    let mut codecs: Vec<Codec> = HARDWARE_ORDER
        .iter()
        .copied()
        .filter(|&codec| codec != requested && hardware.contains(&codec))
        .collect();
    codecs.extend(SOFTWARE_ORDER.iter().copied().filter(|&codec| {
        codec != requested && !hardware.contains(&codec) && software_encoder(codec).is_some()
    }));
    codecs
}

/// Whether a 10-bit request is the software encoder's to serve: the node's engine encodes the
/// codec without 10 bits at the chroma it would run, and the build's software encoder codes
/// them, so the session takes the software path as a 4:4:4 the engine lacks does rather than
/// streaming 8 bits on hardware.
fn hardware_lacks_ten_bit(served: &HardwareEncoders, settings: &CaptureSettings) -> bool {
    let codec = settings.codec;
    if settings.video_bit_depth < 10 || !software_ten_bit(codec) {
        return false;
    }
    served
        .iter()
        .find(|&&(c, ..)| c == codec)
        .is_some_and(|&(_, _, formats)| {
            let fullcolor = settings.video_fullcolor && codec.fullcolor() && formats.fullcolor;
            !formats.ten_bit[fullcolor as usize]
        })
}

/// `hardware_lacks_ten_bit` for the encode node `settings` names, for a capture path that opens
/// its engine's session itself (NvFBC).
pub(crate) fn ten_bit_is_softwares(settings: &CaptureSettings) -> bool {
    probe_node(settings.encode_node_index.max(0))
        .is_ok_and(|served| hardware_lacks_ten_bit(&served, settings))
}

/// Whether an engine may take the codec `settings` names at another size: software is not
/// forced, and the encode node's probe found an engine for it, at the depth asked, or found every
/// session taken. A session the ladder left in software where this holds was refused for
/// something a resize can change (an engine's maximum size), so it runs the ladder again.
pub(crate) fn engine_may_serve(settings: &CaptureSettings) -> bool {
    if settings.use_cpu || settings.encode_node_index == -1 {
        return false;
    }
    #[cfg(target_arch = "aarch64")]
    if tegra::available() {
        return tegra::served().contains(&settings.codec);
    }
    match probe_node(settings.encode_node_index) {
        Ok(served) => {
            served.iter().any(|&(c, ..)| c == settings.codec)
                && !hardware_lacks_ten_bit(&served, settings)
        }
        Err((_, e)) => e == nvenc::SESSIONS_TAKEN,
    }
}

/// The ladder for the one codec `settings` names; `None` where no backend of it opened, and for
/// H.264 on host frames, whose software path the caller encodes itself.
fn select_for_codec(
    settings: &mut CaptureSettings,
    source: FrameSource,
    prior: Option<FrameEncoder>,
    tag: &str,
) -> Option<FrameEncoder> {
    let codec = settings.codec;
    let software_forced = settings.use_cpu || settings.encode_node_index == -1;
    #[cfg(target_arch = "aarch64")]
    if let (false, Some(_), FrameSource::Host { rgba }) =
        (software_forced, tegra::coded_fourcc(codec), source)
    {
        // Tegra publishes no render node driver to probe and carries no libnvidia-encode, so the
        // vendor library is the only way to its encoder and this step comes before both; a
        // codec it does not serve, or refuses, goes straight to software, past the two backends
        // the board lacks.
        if tegra::available() {
            drop(prior);
            if tegra::served().contains(&codec) {
                match tegra::TegraEncoder::new(codec, settings, rgba) {
                    Ok(enc) => {
                        println!(
                            "[{tag}] Encoder: TEGRA {} {} on the vendor V4L2 encoder.",
                            codec.display(),
                            chroma_name(enc.is_fullcolor())
                        );
                        return Some(FrameEncoder::Tegra(enc));
                    }
                    Err(e) => {
                        eprintln!("[{tag}] Failed to init the Tegra encoder: {e}");
                        crate::report::encoder_reason(&format!(
                            "Tegra {} did not open: {e}",
                            codec.display()
                        ));
                    }
                }
            } else {
                crate::report::encoder_reason(&format!(
                    "the Tegra encoder has no {} engine",
                    codec.display()
                ));
            }
            return software_fallback(settings, rgba, tag);
        }
    }
    if !software_forced {
        let node = settings.encode_node_index.max(0);
        let driver = crate::get_gpu_driver(node);
        crate::log::debug!("[{tag}] Encode node {node}, driver {driver}.");
        // What the node's probe settled holds for every capture start: a backend it could not
        // bring up, or a device with no engine for the codec, is not opened again.
        let settled = match probe_node(node) {
            Err((backend, e)) if e != nvenc::SESSIONS_TAKEN => Some(format!(
                "{} {} did not open: {e}",
                backend.to_uppercase(),
                codec.display()
            )),
            Ok(served)
                if !served.iter().any(|&(c, backend, ..)| {
                    c == codec && matches!(backend, "nvenc" | "vaapi")
                }) =>
            {
                Some(format!(
                    "render node {node} has no {} engine",
                    codec.display()
                ))
            }
            Ok(served) if hardware_lacks_ten_bit(&served, settings) => Some(format!(
                "render node {node} encodes no 10-bit {}, which the software encoder does",
                codec.display()
            )),
            _ => None,
        };
        if let Some(reason) = settled {
            drop(prior);
            crate::report::encoder_reason(&reason);
        } else if crate::driver_selects_nvenc(&driver) {
            if let Some(FrameEncoder::Nvenc(mut enc)) = prior {
                match enc.reconfigure_resolution(settings) {
                    Ok(resized) => {
                        if resized {
                            crate::log::debug!("[{tag}] NVENC session reconfigured in place.");
                        }
                        crate::report::hardware_encoder(
                            enc.device_name(),
                            driver_name(&driver),
                            node,
                        );
                        return Some(FrameEncoder::Nvenc(enc));
                    }
                    Err(e) => eprintln!(
                        "[{tag}] NVENC in-place reconfigure unavailable ({e}); rebuilding."
                    ),
                }
            }
            let egl_display = match source {
                FrameSource::Dmabuf { egl_display } => egl_display,
                FrameSource::Host { .. } => std::ptr::null(),
            };
            match NvencEncoder::new(settings, egl_display) {
                Ok(enc) => {
                    println!(
                        "[{tag}] Encoder: NVENC {} {} {}-bit on {} (render node {node}, {} driver), {}.",
                        codec.display(),
                        chroma_name(enc.is_fullcolor()),
                        enc.bit_depth(),
                        enc.device_name(),
                        driver_name(&driver),
                        enc.split_summary()
                    );
                    crate::report::hardware_encoder(enc.device_name(), driver_name(&driver), node);
                    return Some(FrameEncoder::Nvenc(enc));
                }
                Err(e) => {
                    eprintln!("[{tag}] Failed to init NVENC {}: {e}", codec.display());
                    crate::report::encoder_reason(&format!(
                        "NVENC {} did not open: {e}",
                        codec.display()
                    ));
                }
            }
        } else {
            // Nothing below reconfigures a session in place, so the previous one is released
            // here: a single-context M2M node refuses to open while its own last session is
            // alive, and a window resize would then demote a working hardware path.
            drop(prior);
            let input = match source {
                FrameSource::Dmabuf { .. } => vaapi::Input::Dmabuf,
                FrameSource::Host { rgba } => vaapi::Input::Host { rgba },
            };
            match vaapi::VaapiEncoder::new(settings, codec, input) {
                Ok(enc) => {
                    println!(
                        "[{tag}] Encoder: VAAPI {} {} {}-bit on {} surfaces (render node {node}, {} driver).",
                        codec.display(),
                        chroma_name(enc.is_fullcolor()),
                        enc.bit_depth(),
                        enc.surface_format_name(),
                        driver_name(&driver)
                    );
                    crate::report::hardware_encoder("", driver_name(&driver), node);
                    return Some(FrameEncoder::Vaapi(enc));
                }
                Err(e) => {
                    eprintln!("[{tag}] Failed to init VAAPI {}: {e}", codec.display());
                    crate::report::encoder_reason(&format!(
                        "VAAPI {} did not open: {e}",
                        codec.display()
                    ));
                }
            }
        }
        // A stateful M2M encoder publishes no render node driver to select on, so it is
        // reached only once the two backends that do have refused. The size is checked before
        // the node is opened: a refusal here falls through to software, a refusal later would
        // leave a session that came up and produces nothing.
        if let (Some(_), FrameSource::Host { rgba }) = (v4l2m2m::coded_fourcc(codec), source)
            && v4l2m2m::encodes(codec, settings.width, settings.height)
        {
            match v4l2m2m::V4l2M2mEncoder::new(codec, settings, rgba) {
                Ok(enc) => {
                    println!(
                        "[{tag}] Encoder: V4L2M2M {} {} on a stateful M2M node.",
                        codec.display(),
                        chroma_name(enc.is_fullcolor())
                    );
                    return Some(FrameEncoder::V4l2m2m(enc));
                }
                Err(e) => eprintln!("[{tag}] Failed to init the V4L2 M2M encoder: {e}"),
            }
        }
    } else {
        crate::log::debug!("[{tag}] Software encoding selected (use_cpu or encode_node_index -1).");
        crate::report::encoder_reason("software encoding selected");
    }
    let FrameSource::Host { rgba } = source else {
        return None;
    };
    software_fallback(settings, rgba, tag)
}

/// The codec's software encoder; `None` for H.264, whose software path is the CPU path the
/// caller encodes itself, and where the build carries none for the codec, which the ladder then
/// falls through on.
fn software_fallback(
    settings: &mut CaptureSettings,
    rgba: bool,
    tag: &str,
) -> Option<FrameEncoder> {
    let codec = settings.codec;
    if codec == Codec::H264 {
        println!(
            "[{tag}] Encoder: software {} ({}) {} {}-bit, {}.",
            codec.display(),
            software_library(Codec::H264),
            chroma_name(session_fullcolor(None, settings)),
            session_bit_depth(None, settings),
            threads_desc(session::h264_fullframe_threads(settings))
        );
        return None;
    }
    let session = software_encoder(codec)
        .ok_or_else(|| {
            format!(
                "the {} encoder does not run on this machine",
                codec.display()
            )
        })
        .and_then(|_| software_session(settings, codec, rgba));
    match session {
        Ok(enc) => {
            println!(
                "[{tag}] Encoder: software {} ({}) {} {}-bit, {}.",
                codec.display(),
                enc.backend_name(),
                chroma_name(enc.is_fullcolor()),
                enc.bit_depth(),
                threads_desc(session::encode_threads(settings))
            );
            Some(enc)
        }
        Err(e) => {
            eprintln!("[{tag}] No {} encoder available: {e}.", codec.display());
            None
        }
    }
}

#[cfg(test)]
mod software_tests {
    //! The full-frame software sessions, driven end to end through the one ladder step that
    //! builds them: every frame they emit is wire-framed for its codec and decodes back to the
    //! picture that went in, key frames come on request and are self-contained, the quantizer
    //! and the CBR target reach the encoder, a live rate or quality change keeps the stream
    //! decodable, and each stream declares the matrix it was converted with.
    use super::codec::{FRAME_DELTA, FRAME_KEY, WIRE_VIDEO, parse_video_type};
    use super::*;
    use crate::webcam::convert::I420View;
    use crate::webcam::decode::{ColorTags, Decoder, VideoDecoder};
    use std::cell::Cell;
    use std::sync::{Mutex, MutexGuard};

    const W: usize = 320;
    const H: usize = 240;

    /// The video codecs the software ladder serves whole-frame.
    const SOFTWARE: [Codec; 4] = [Codec::H265, Codec::Vp8, Codec::Vp9, Codec::Av1];

    /// One AV1 session at a time across the suite: SVT-AV1 before 2.x faults while a second
    /// session is live, and the whole life of the session is the unsafe window. Every other
    /// codec stays parallel, and a thread already holding the turn keeps it, since one check
    /// holds two sessions at once.
    static ONE_AV1: Mutex<()> = Mutex::new(());

    thread_local! {
        static AV1_DEPTH: Cell<u32> = const { Cell::new(0) };
    }

    /// The AV1 turn, held for as long as the session it was taken for.
    pub(super) struct Turn {
        _guard: Option<MutexGuard<'static, ()>>,
        counted: bool,
    }

    impl Drop for Turn {
        fn drop(&mut self) {
            if self.counted {
                AV1_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
            }
        }
    }

    /// Take the turn for `codec`, which is a no-op for anything but AV1.
    pub(super) fn turn(codec: Codec) -> Turn {
        let counted = codec == Codec::Av1;
        let first = counted
            && AV1_DEPTH.with(|depth| {
                let held = depth.get();
                depth.set(held + 1);
                held == 0
            });
        Turn {
            _guard: first.then(|| ONE_AV1.lock().unwrap_or_else(|e| e.into_inner())),
            counted,
        }
    }

    /// A software session holding the AV1 turn for as long as it lives.
    pub(super) struct Session {
        encoder: FrameEncoder,
        _turn: Turn,
    }

    impl std::ops::Deref for Session {
        type Target = FrameEncoder;
        fn deref(&self) -> &FrameEncoder {
            &self.encoder
        }
    }

    impl std::ops::DerefMut for Session {
        fn deref_mut(&mut self) -> &mut FrameEncoder {
            &mut self.encoder
        }
    }

    pub(super) fn settings(codec: Codec) -> CaptureSettings {
        CaptureSettings {
            width: W as i32,
            height: H as i32,
            target_fps: 30.0,
            codec,
            video_crf: 25,
            use_cpu: true,
            ..Default::default()
        }
    }

    pub(super) fn session(codec: Codec, s: &CaptureSettings, rgba: bool) -> Session {
        let _turn = turn(codec);
        let encoder = software_session(s, codec, rgba)
            .unwrap_or_else(|e| panic!("{codec:?} software session: {e}"));
        Session { encoder, _turn }
    }

    /// Frames a fresh session takes before its first packet, zero where one frame in is one
    /// picture out. The wire ids and the latency budget both assume zero; SVT-AV1 gives that
    /// only from 2.3.0, where its packet call became blocking for low delay, and before it
    /// fills two frames that it neither reports nor lets a caller shorten.
    fn pipeline_depth(codec: Codec) -> usize {
        let s = settings(codec);
        let mut enc = session(codec, &s, false);
        for t in 0..8usize {
            let out = enc
                .encode_host(&frame(t), W * 4, false, t as u64, 25, t == 0)
                .unwrap_or_default();
            if !out.is_empty() {
                return t;
            }
        }
        panic!("{codec:?}: no packet after eight frames");
    }

    /// The software codecs whose encoder answers each frame with that frame's own picture,
    /// which is what the checks below read a frame id back from. One that pipelines is named
    /// rather than skipped silently, since the delay is the session's latency as well as the
    /// test's.
    fn lockstep_codecs() -> Vec<Codec> {
        SOFTWARE
            .into_iter()
            .filter(|&codec| {
                let depth = pipeline_depth(codec);
                if depth > 0 {
                    println!("[pipeline] {codec:?}: {depth} frames deep, frame-id checks skipped");
                }
                depth == 0
            })
            .collect()
    }

    /// A desktop-like BGRA frame: a diagonal gradient with a grid of dark glyph cells and a
    /// bright block that moves with `t`, so inter frames carry real motion.
    pub(super) fn frame(t: usize) -> Vec<u8> {
        let mut f = vec![0u8; W * H * 4];
        let (bx, by) = ((t * 9) % (W - 40), (t * 5) % (H - 30));
        for y in 0..H {
            for x in 0..W {
                let i = (y * W + x) * 4;
                let g = ((x * 255) / W) as u8;
                let cell = (x / 8 + y / 12) % 3 == 0 && x % 8 < 6 && y % 12 < 9;
                let (b, gr, r) = if x >= bx && x < bx + 40 && y >= by && y < by + 30 {
                    (40, 220, 250)
                } else if cell {
                    (30, 30, 30)
                } else {
                    (g, 200 - g / 2, 120)
                };
                f[i] = b;
                f[i + 1] = gr;
                f[i + 2] = r;
                f[i + 3] = 255;
            }
        }
        f
    }

    /// Incompressible content, so rate control has to spend its whole budget.
    fn noise(t: usize) -> Vec<u8> {
        let mut f = vec![255u8; W * H * 4];
        let mut s = (t as u32).wrapping_mul(2654435761).wrapping_add(7);
        for px in f.as_chunks_mut::<4>().0 {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            px[0] = (s >> 24) as u8;
            px[1] = (s >> 16) as u8;
            px[2] = (s >> 8) as u8;
        }
        f
    }

    /// Luma PSNR of a decoded frame against the BGRA source it came from, with the source's
    /// luma derived by the BT.709 limited-range formula the encoder's conversion uses.
    fn luma_psnr(decoded: &I420View<'_>, bgra: &[u8]) -> f64 {
        assert_eq!((decoded.width, decoded.height), (W, H));
        let mut mse = 0f64;
        for y in 0..H {
            for x in 0..W {
                let i = (y * W + x) * 4;
                let (b, g, r) = (bgra[i] as f64, bgra[i + 1] as f64, bgra[i + 2] as f64);
                let luma = 16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0;
                let d = decoded.y[y * decoded.y_stride + x] as f64 - luma;
                mse += d * d;
            }
        }
        mse /= (W * H) as f64;
        if mse <= 0.0 {
            99.0
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        }
    }

    fn decode_one(dec: &mut VideoDecoder, packet: &[u8]) -> bool {
        dec.decode(&packet[VIDEO_HEADER_LEN..])
            .unwrap_or_else(|e| panic!("decode: {e:?}"))
    }

    /// Whether the session takes a new bitrate without a key frame: libvpx, and SVT-AV1 where the
    /// release takes one with a picture. A new quantizer reaches libvpx alone live.
    fn takes_a_live_rate(enc: &FrameEncoder) -> bool {
        match enc.backend_name() {
            "libvpx" => true,
            "svt-av1" => codec_sys::svtav1::HAS_EVENTS,
            _ => false,
        }
    }

    /// Every codec's frames carry its own wire id and kind, decode back to the source picture,
    /// and a key frame forced mid-stream starts a fresh decoder on its own.
    #[test]
    fn software_frames_decode_back_to_the_source() {
        for codec in lockstep_codecs() {
            let s = settings(codec);
            let mut enc = session(codec, &s, false);
            let mut dec = VideoDecoder::new(codec).expect("decoder");
            for t in 0..6usize {
                let src = frame(t);
                let out = enc
                    .encode_host(&src, W * 4, false, t as u64, 25, t == 0)
                    .unwrap_or_else(|e| panic!("{codec:?} encode {t}: {e}"));
                assert!(out.len() > VIDEO_HEADER_LEN, "{codec:?} frame {t} is empty");
                assert_eq!(out[0], WIRE_VIDEO);
                let kind = if t == 0 { FRAME_KEY } else { FRAME_DELTA };
                assert_eq!(
                    parse_video_type(out[1]),
                    Some((codec, kind)),
                    "{codec:?} frame {t}"
                );
                assert_eq!(u16::from_be_bytes([out[2], out[3]]) as usize, t);
                assert_eq!(
                    &out[4..10],
                    &[0, 0, (W >> 8) as u8, W as u8, (H >> 8) as u8, H as u8]
                );
                assert!(
                    decode_one(&mut dec, &out),
                    "{codec:?} frame {t} decoded nothing"
                );
                let psnr = luma_psnr(&dec.frame().unwrap(), &src);
                assert!(psnr > 28.0, "{codec:?} frame {t}: luma PSNR {psnr:.1} dB");
            }
            let src = frame(6);
            let key = enc
                .encode_host(&src, W * 4, false, 6, 25, true)
                .expect("forced key");
            assert_eq!(
                parse_video_type(key[1]),
                Some((codec, FRAME_KEY)),
                "{codec:?} forced key"
            );
            let mut fresh = VideoDecoder::new(codec).expect("decoder");
            assert!(
                decode_one(&mut fresh, &key),
                "{codec:?}: a forced key frame must decode alone"
            );
            assert!(luma_psnr(&fresh.frame().unwrap(), &src) > 28.0);
            let next = enc
                .encode_host(&frame(7), W * 4, false, 7, 25, false)
                .expect("delta after key");
            assert_eq!(parse_video_type(next[1]), Some((codec, FRAME_DELTA)));
            assert!(decode_one(&mut fresh, &next));
        }
    }

    /// A key frame asked for mid-stream comes back as one on every codec, through whatever
    /// pipeline the encoder keeps, carrying the id of the frame it was asked for, and a fresh
    /// decoder starts on it. SVT-AV1 before 2.0 re-opens for it and hands it back two frames
    /// later, as kvazaar does.
    #[test]
    fn a_key_frame_asked_for_mid_stream_starts_a_fresh_decoder() {
        for codec in SOFTWARE {
            let s = settings(codec);
            let mut enc = session(codec, &s, false);
            for t in 0..6usize {
                enc.encode_host(&frame(t), W * 4, false, t as u64, 25, t == 0)
                    .unwrap_or_else(|e| panic!("{codec:?} encode {t}: {e}"));
            }
            let mut out = enc
                .encode_host(&frame(6), W * 4, false, 6, 25, true)
                .expect("forced key");
            let mut t = 7usize;
            while out.get(1).and_then(|&kind| parse_video_type(kind)) != Some((codec, FRAME_KEY)) {
                assert!(
                    t < 22,
                    "{codec:?}: no key frame came back within fifteen frames of the request"
                );
                out = enc
                    .encode_host(&frame(t), W * 4, false, t as u64, 25, false)
                    .expect("delta");
                t += 1;
            }
            assert_eq!(
                u16::from_be_bytes([out[2], out[3]]),
                6,
                "{codec:?}: the key frame carries the id it was asked for"
            );
            let mut fresh = VideoDecoder::new(codec).expect("decoder");
            assert!(
                decode_one(&mut fresh, &out),
                "{codec:?}: the key frame must decode alone"
            );
            assert!(
                luma_psnr(&fresh.frame().unwrap(), &frame(6)) > 28.0,
                "{codec:?}"
            );
        }
    }

    /// A session whose library keeps its references to itself says so instead of pretending:
    /// it names no reference on any frame and refuses the request, which is what leaves the
    /// caller a key frame to code. x265, kvazaar, and a constant-quality SVT-AV1 session offer
    /// nothing to steer; libvpx does, and its sessions track every frame.
    #[test]
    fn a_session_that_cannot_invalidate_names_no_reference() {
        use super::reference::Reference;
        for codec in [Codec::H265, Codec::Av1] {
            let s = settings(codec);
            let mut enc = session(codec, &s, false);
            for t in 0..4usize {
                enc.encode_host(&frame(t), W * 4, false, t as u64, 25, t == 0)
                    .unwrap_or_else(|e| panic!("{codec:?} encode {t}: {e}"));
                assert_eq!(
                    enc.last_reference(),
                    Reference::Untracked,
                    "{codec:?} frame {t}"
                );
            }
            assert!(
                !enc.invalidate_reference(2),
                "{codec:?}: the refusal is what asks for the key frame"
            );
        }
        for codec in [Codec::Vp8, Codec::Vp9] {
            let s = settings(codec);
            let mut enc = session(codec, &s, false);
            enc.encode_host(&frame(0), W * 4, false, 0, 25, true)
                .unwrap();
            assert_eq!(enc.last_reference(), Reference::None, "{codec:?}");
            enc.encode_host(&frame(1), W * 4, false, 1, 25, false)
                .unwrap();
            assert_eq!(enc.last_reference(), Reference::Frame(0), "{codec:?}");
            assert!(enc.invalidate_reference(1), "{codec:?}");
        }
    }

    /// A constant-rate AV1 session names its references where the release takes reference
    /// commands: a frame a client lost is predicted past from an anchor, a decoder that never saw
    /// the lost frames decodes what follows exactly as one that saw everything, and a key frame
    /// asked for still comes. On an earlier release the session names none and refuses.
    #[test]
    fn a_constant_rate_av1_session_predicts_past_a_lost_frame() {
        use super::reference::Reference;
        let mut s = settings(Codec::Av1);
        s.video_cbr_mode = true;
        s.video_bitrate_kbps = 2000;
        let mut enc = session(Codec::Av1, &s, false);
        let mut frames: Vec<Vec<u8>> = (0..8usize)
            .map(|t| {
                enc.encode_host(&frame(t), W * 4, false, t as u64, 25, t == 0)
                    .unwrap()
            })
            .collect();
        if !codec_sys::svtav1::HAS_EVENTS {
            assert_eq!(enc.last_reference(), Reference::Untracked);
            assert!(!enc.invalidate_reference(5));
            return;
        }
        assert_eq!(enc.last_reference(), Reference::Frame(6));
        // Frame 5 is reported lost once 6 and 7 have gone out: GOLDEN holds 6, so the next frame
        // predicts from ALTREF, the key frame.
        assert!(enc.invalidate_reference(5));
        for t in 8..10usize {
            frames.push(
                enc.encode_host(&frame(t), W * 4, false, t as u64, 25, false)
                    .unwrap(),
            );
            assert_eq!(
                parse_video_type(frames[t][1]),
                Some((Codec::Av1, FRAME_DELTA)),
                "frame {t}"
            );
            assert_eq!(
                enc.last_reference(),
                Reference::Frame(if t == 8 { 0 } else { 8 }),
                "frame {t}"
            );
        }
        let (mut whole, mut lossy) = (
            VideoDecoder::new(Codec::Av1).unwrap(),
            VideoDecoder::new(Codec::Av1).unwrap(),
        );
        for (i, f) in frames.iter().enumerate() {
            assert!(decode_one(&mut whole, f), "frame {i}");
            if !(5..8).contains(&i) {
                assert!(decode_one(&mut lossy, f), "frame {i} without frames 5-7");
            }
        }
        let (a, b) = (whole.frame().unwrap(), lossy.frame().unwrap());
        let same =
            a.y.chunks(a.y_stride)
                .zip(b.y.chunks(b.y_stride))
                .take(H)
                .all(|(x, y)| x[..W] == y[..W]);
        assert!(
            same,
            "the decoder that lost frames 5-7 shows frame 9 unlike the one that saw them"
        );
        let key = enc
            .encode_host(&frame(10), W * 4, false, 10, 25, true)
            .unwrap();
        assert_eq!(parse_video_type(key[1]), Some((Codec::Av1, FRAME_KEY)));
        assert_eq!(enc.last_reference(), Reference::None);
        assert!(
            decode_one(&mut VideoDecoder::new(Codec::Av1).unwrap(), &key),
            "the key frame decodes alone"
        );
    }

    /// A constant-rate AV1 session predicting a frame from an anchor alone codes what the picture
    /// changed since the anchor: test_slow_page's scene at 1280x720, a 160-pixel bar moving 12
    /// pixels a frame over flat color, with the frame four back reported lost every 13, decodes
    /// within 20 levels of its source in every 64-pixel block above its noise, where preset 11
    /// left blocks of the bar standing where they were in the anchor, 41 off.
    #[test]
    fn an_anchor_prediction_leaves_no_block_as_it_stood_in_the_anchor() {
        use super::reference::ReferenceReport;
        if !codec_sys::svtav1::HAS_EVENTS {
            return;
        }
        let (w, h) = (1280usize, 720usize);
        let mut noise = vec![0u8; w * 240 * 4];
        let mut seed = 1u64;
        for b in noise.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = seed as u8;
        }
        let scene = |t: usize| {
            let bar = (t * 12) % (w - 160);
            let mut f = vec![0u8; w * h * 4];
            for (i, px) in f.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (i % w, i / w);
                let bgr: [u8; 3] = if y >= 480 {
                    let n = ((t % 4) * w * 240 + (y - 480) * w + x) * 4 % noise.len();
                    [noise[n], noise[n + 1], noise[n + 2]]
                } else if (96..160).contains(&y) {
                    [if ((x + t * 4) / 16 + (y - 96) / 16).is_multiple_of(2) {
                        255
                    } else {
                        0
                    }; 3]
                } else if y >= 160 && (bar..bar + 160).contains(&x) {
                    [0x28, 0x3c, 0xdc]
                } else {
                    [0x78, 0x28, 0x1e]
                };
                px[..3].copy_from_slice(&bgr);
            }
            f
        };
        let mut s = settings(Codec::Av1);
        s.width = w as i32;
        s.height = h as i32;
        s.target_fps = 60.0;
        s.video_cbr_mode = true;
        s.video_bitrate_kbps = 8000;
        let mut enc = session(Codec::Av1, &s, false);
        let mut dec = VideoDecoder::new(Codec::Av1).unwrap();
        let (mut previous, mut anchored) = (None, 0);
        for t in 0..84usize {
            if t > 10 && t % 13 == 0 {
                assert!(enc.take_report(ReferenceReport::Lost(t as u16 - 4)));
            }
            let src = scene(t);
            let out = enc
                .encode_host(&src, w * 4, false, t as u64, 25, t == 0)
                .unwrap();
            let id = u16::from_be_bytes([out[2], out[3]]);
            let reference = u16::from_be_bytes([out[10], out[11]]);
            assert_eq!(id as usize, t);
            if reference != id && Some(reference) != previous {
                anchored += 1;
            }
            previous = Some(id);
            assert!(decode_one(&mut dec, &out), "frame {id}");
            let pic = dec.frame().unwrap();
            for (bx, by) in (0..480)
                .step_by(64)
                .flat_map(|y| (0..w).step_by(64).map(move |x| (x, y)))
            {
                let (mut sum, mut n) = (0f64, 0);
                for y in (by..(by + 64).min(480)).step_by(2) {
                    for x in (bx..bx + 64).step_by(2) {
                        let i = (y * w + x) * 4;
                        let (b, g, r) = (src[i] as f64, src[i + 1] as f64, src[i + 2] as f64);
                        let luma = 16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0;
                        sum += (pic.y[y * pic.y_stride + x] as f64 - luma).abs();
                        n += 1;
                    }
                }
                let off = sum / n as f64;
                assert!(
                    off <= 20.0,
                    "frame {id} (from {reference}): the block at ({bx}, {by}) is {off:.1} off its source"
                );
            }
        }
        assert!(anchored >= 5, "{anchored} frames predicted from an anchor");
    }

    /// The byte order a session is built for reaches the conversion: a red picture handed as
    /// B,G,R,A and as R,G,B,A decodes to the same red on both.
    #[test]
    fn host_byte_order_is_honored() {
        for codec in lockstep_codecs() {
            let s = settings(codec);
            let mut means = Vec::new();
            for rgba in [false, true] {
                let mut px = [0u8; 4];
                if rgba {
                    px[0] = 220
                } else {
                    px[2] = 220
                }
                px[3] = 255;
                let src: Vec<u8> = px.repeat(W * H);
                let mut enc = session(codec, &s, rgba);
                let out = enc
                    .encode_host(&src, W * 4, rgba, 0, 20, true)
                    .expect("encode");
                let mut dec = VideoDecoder::new(codec).expect("decoder");
                assert!(decode_one(&mut dec, &out));
                let f = dec.frame().unwrap();
                let cw = f.chroma_width();
                let ch = f.chroma_height();
                let v: f64 = (0..ch)
                    .flat_map(|y| (0..cw).map(move |x| (x, y)))
                    .map(|(x, y)| f.v[y * f.uv_stride + x] as f64)
                    .sum::<f64>()
                    / (cw * ch) as f64;
                means.push(v);
            }
            assert!(
                means[0] > 180.0,
                "{codec:?}: red must land high in V, got {:.0}",
                means[0]
            );
            assert!(
                (means[0] - means[1]).abs() < 6.0,
                "{codec:?}: BGRA {:.0} vs RGBA {:.0}",
                means[0],
                means[1]
            );
        }
    }

    /// A higher session quality index (a coarser quantizer) shrinks the stream, and CBR holds a
    /// noise stream near its bitrate target.
    #[test]
    fn quantizer_and_bitrate_reach_the_encoder() {
        for codec in SOFTWARE {
            let run = |crf: i32| -> usize {
                let mut s = settings(codec);
                s.video_crf = crf;
                let mut enc = session(codec, &s, false);
                (0..12usize)
                    .map(|t| {
                        enc.encode_host(&frame(t), W * 4, false, t as u64, crf as u32, t == 0)
                            .unwrap()
                            .len()
                    })
                    .sum()
            };
            let (fine, coarse) = (run(15), run(45));
            assert!(
                coarse * 2 < fine,
                "{codec:?}: crf 45 = {coarse} bytes vs crf 15 = {fine}"
            );

            const KBPS: i32 = 800;
            let mut s = settings(codec);
            s.video_cbr_mode = true;
            s.video_bitrate_kbps = KBPS;
            let mut enc = session(codec, &s, false);
            let mut bytes = 0usize;
            for t in 0..90usize {
                let out = enc
                    .encode_host(&noise(t), W * 4, false, t as u64, 25, t == 0)
                    .unwrap();
                // A constant-rate encoder is free to answer a frame with nothing -- SVT-AV1
                // drops one rather than overshoot its buffer -- and that frame carries no
                // payload to count rather than a negative one.
                if t >= 30 && out.len() > VIDEO_HEADER_LEN {
                    bytes += out.len() - VIDEO_HEADER_LEN;
                }
            }
            let kbps = bytes as f64 * 8.0 * 30.0 / 60.0 / 1000.0;
            println!("{codec:?} CBR {KBPS} kbps on noise: {kbps:.0} kbps");
            assert!(
                kbps > KBPS as f64 * 0.5 && kbps < KBPS as f64 * 1.6,
                "{codec:?}: {kbps:.0} kbps"
            );
        }
    }

    /// A quality increase applies at once and keeps the stream decodable, a decrease waits out
    /// the hysteresis where a change costs a re-open, and a frame-rate change keeps the stream
    /// decodable too. A library that takes the change live spends no key frame on it; one that
    /// has to re-open starts the fresh encoder with one.
    #[test]
    fn live_quality_and_rate_changes_keep_the_stream_decodable() {
        for codec in lockstep_codecs() {
            let mut s = settings(codec);
            s.video_crf = 40;
            let mut enc = session(codec, &s, false);
            let mut dec = VideoDecoder::new(codec).expect("decoder");
            let coarse = enc
                .encode_host(&frame(0), W * 4, false, 0, 40, true)
                .unwrap();
            assert!(decode_one(&mut dec, &coarse));
            let fine = enc
                .encode_host(&frame(1), W * 4, false, 1, 15, false)
                .unwrap();
            let live = enc.backend_name() == "libvpx";
            let kind = if live { FRAME_DELTA } else { FRAME_KEY };
            assert_eq!(
                parse_video_type(fine[1]),
                Some((codec, kind)),
                "{codec:?}: a quality change {}",
                if live {
                    "is live"
                } else {
                    "re-opens with a key frame"
                }
            );
            assert!(decode_one(&mut dec, &fine));
            let held = enc
                .encode_host(&frame(2), W * 4, false, 2, 40, false)
                .unwrap();
            assert_eq!(
                parse_video_type(held[1]),
                Some((codec, FRAME_DELTA)),
                "{codec:?}: a single decrease waits out the hysteresis"
            );
            assert!(decode_one(&mut dec, &held));
            s.target_fps = 15.0;
            enc.reconfigure_rate(&s).expect("rate reconfigure");
            let after = enc
                .encode_host(&frame(3), W * 4, false, 3, 15, false)
                .unwrap();
            let kind = if enc.backend_name() == "libvpx" {
                FRAME_DELTA
            } else {
                FRAME_KEY
            };
            assert_eq!(
                parse_video_type(after[1]),
                Some((codec, kind)),
                "{codec:?}: a frame-rate change"
            );
            assert!(decode_one(&mut dec, &after));
            assert!(luma_psnr(&dec.frame().unwrap(), &frame(3)) > 28.0);
        }
    }

    /// A finer quantizer reaches a running constant-quality session, live where the library takes
    /// one, and the frames after it spend more.
    #[test]
    fn a_quality_change_reaches_a_running_session() {
        for codec in lockstep_codecs() {
            let mut s = settings(codec);
            s.video_crf = 40;
            let mut enc = session(codec, &s, false);
            let (mut spent, mut changed) = ([0usize; 2], None);
            for t in 0..12usize {
                let crf = if t < 6 { 40 } else { 15 };
                let out = enc
                    .encode_host(&frame(t), W * 4, false, t as u64, crf, t == 0)
                    .unwrap();
                if t == 6 {
                    changed = parse_video_type(out[1]);
                }
                if (1..6).contains(&t) {
                    spent[0] += out.len();
                } else if t >= 7 {
                    spent[1] += out.len();
                }
            }
            assert!(
                spent[1] * 2 > spent[0] * 3,
                "{codec:?}: bytes over five deltas before and after {spent:?}"
            );
            let kind = if enc.backend_name() == "libvpx" {
                FRAME_DELTA
            } else {
                FRAME_KEY
            };
            assert_eq!(
                changed,
                Some((codec, kind)),
                "{codec:?}: the frame after the change"
            );
        }
    }

    /// A new bitrate reaches a running constant-rate session, live where the library takes one,
    /// and the stream stays decodable across it.
    #[test]
    fn a_bitrate_change_reaches_a_running_session() {
        for codec in lockstep_codecs() {
            let mut s = settings(codec);
            s.video_cbr_mode = true;
            s.video_bitrate_kbps = 1500;
            let mut enc = session(codec, &s, false);
            let mut dec = VideoDecoder::new(codec).expect("decoder");
            let (mut spent, mut changed) = ([0usize; 2], None);
            for t in 0..60usize {
                if t == 30 {
                    s.video_bitrate_kbps = 4500;
                    enc.reconfigure_rate(&s).expect("rate reconfigure");
                }
                let out = enc
                    .encode_host(&noise(t), W * 4, false, t as u64, 25, t == 0)
                    .unwrap();
                if t == 30 {
                    changed = parse_video_type(out[1]);
                }
                if out.len() > VIDEO_HEADER_LEN {
                    assert!(decode_one(&mut dec, &out), "{codec:?} frame {t}");
                }
                if (15..30).contains(&t) {
                    spent[0] += out.len();
                } else if t >= 45 {
                    spent[1] += out.len();
                }
            }
            assert!(
                spent[1] > spent[0] * 2,
                "{codec:?}: bytes over fifteen frames before and after {spent:?}"
            );
            let kind = if takes_a_live_rate(&enc) {
                FRAME_DELTA
            } else {
                FRAME_KEY
            };
            assert_eq!(
                changed,
                Some((codec, kind)),
                "{codec:?}: the frame after the change"
            );
        }
    }

    /// Every session declares the BT.709 matrix it converts with, at limited range for 4:2:0
    /// and full range for the x265 4:4:4 one, like x264. VP8 reads back as BT.470BG whatever it
    /// is handed: its keyframe header holds one color-space bit and BT.601 is its only defined
    /// value, so the transports carry the real matrix for that codec themselves.
    #[test]
    fn sessions_declare_the_matrix_they_convert_with() {
        for codec in lockstep_codecs() {
            let mut s = settings(codec);
            let mut enc = session(codec, &s, false);
            let out = enc
                .encode_host(&frame(0), W * 4, false, 0, 25, true)
                .expect("encode");
            let mut dec = VideoDecoder::new(codec).expect("decoder");
            assert!(decode_one(&mut dec, &out));
            let want = if codec == Codec::Vp8 {
                ColorTags::BT470BG_LIMITED
            } else {
                ColorTags::BT709_LIMITED
            };
            assert_eq!(dec.color_tags(), Some(want), "{codec:?}");
            if software_fullcolor(codec) {
                s.video_fullcolor = true;
                let mut enc = session(codec, &s, false);
                assert!(enc.is_fullcolor(), "{codec:?} carries the 4:4:4 request");
                let out = enc
                    .encode_host(&frame(0), W * 4, false, 0, 25, true)
                    .expect("encode");
                let mut dec = VideoDecoder::new(codec).expect("decoder");
                assert!(decode_one(&mut dec, &out));
                let want = if codec == Codec::Vp9 {
                    ColorTags::BT709_LIMITED
                } else {
                    ColorTags::BT709_FULL
                };
                assert_eq!(dec.color_tags(), Some(want), "{codec:?} 4:4:4");
            }
        }
    }

    /// 4:4:4 is carried only where the software encoder does (x265, VP9), never quietly
    /// elsewhere.
    #[test]
    fn fullcolor_follows_the_software_encoder() {
        for codec in lockstep_codecs() {
            let mut s = settings(codec);
            s.video_fullcolor = true;
            let mut enc = session(codec, &s, false);
            assert_eq!(enc.is_fullcolor(), software_fullcolor(codec), "{codec:?}");
            let out = enc
                .encode_host(&frame(0), W * 4, false, 0, 25, true)
                .unwrap();
            let mut dec = VideoDecoder::new(codec).expect("decoder");
            assert!(decode_one(&mut dec, &out));
        }
    }

    /// 10 bits are coded only where the software encoder codes them, at either chroma it
    /// carries, and the session says which it runs.
    #[test]
    fn ten_bit_follows_the_software_encoder() {
        for codec in lockstep_codecs() {
            for fullcolor in [false, true] {
                let mut s = settings(codec);
                s.video_bit_depth = 10;
                s.video_fullcolor = fullcolor;
                let mut enc = session(codec, &s, false);
                let want = if software_ten_bit(codec) { 10 } else { 8 };
                assert_eq!(enc.bit_depth(), want, "{codec:?}");
                assert_eq!(
                    enc.is_fullcolor(),
                    fullcolor && software_fullcolor(codec),
                    "{codec:?}"
                );
                assert_eq!(
                    software_formats(codec).ten_bit,
                    [want == 10, want == 10 && software_fullcolor(codec)],
                    "{codec:?}"
                );
                for t in 0..3 {
                    let out = enc
                        .encode_host(&frame(t), W * 4, false, t as u64, 25, t == 0)
                        .unwrap();
                    assert!(t > 0 || out.len() > VIDEO_HEADER_LEN, "{codec:?} key frame");
                }
            }
        }
    }

    /// Four colors whose 2x2 average is gray, tiled: a decoded block's chroma comes out
    /// neutral only where the session sited chroma at the center of the block, and saturated
    /// wherever it kept one pixel, row, or column of it — the color a browser then shows along
    /// the glyph edges of subpixel-antialiased text.
    #[test]
    fn decoded_chroma_is_neutral_on_a_tile_that_averages_to_gray() {
        const N: usize = 128;
        let bgra = chroma_siting::bgra(N, N);
        let settings = CaptureSettings {
            width: N as i32,
            height: N as i32,
            target_fps: 30.0,
            video_crf: 20,
            ..Default::default()
        };
        for codec in SOFTWARE {
            let mut enc = session(codec, &settings, false);
            let out = drain_first(&mut enc, &bgra, N * 4, 20);
            assert!(!out.is_empty(), "{codec:?} encoded nothing");
            let mut dec = VideoDecoder::new(codec).expect("decoder");
            assert!(
                dec.decode(&out[VIDEO_HEADER_LEN..]).unwrap_or(false),
                "{codec:?} decoded nothing"
            );
            let worst = chroma_siting::worst(&dec.frame().expect("frame"));
            println!("[chroma-siting] {codec:?}: worst |C-128| {worst:.1}");
            assert!(
                worst <= 8.0,
                "{codec:?} sites chroma {worst:.1} off neutral"
            );
        }
    }

    /// The eight-patch chart, encoded and decoded, comes back as the color that was painted
    /// when a receiver inverts the matrix the session declares — the check a client's
    /// presentation path performs on every frame. A convert or a declaration that name
    /// different matrices leaves the neutrals exact and the saturated patches tens of levels
    /// out, which is what the browsers show as washed-out or shifted color.
    #[test]
    fn the_chart_decodes_to_the_color_that_was_painted() {
        const N: usize = 256;
        let bgra = chroma_siting::chart_bgra(N, N / 2);
        let settings = CaptureSettings {
            width: N as i32,
            height: (N / 2) as i32,
            target_fps: 30.0,
            video_crf: 20,
            ..Default::default()
        };
        for codec in SOFTWARE {
            let mut enc = session(codec, &settings, false);
            let out = drain_first(&mut enc, &bgra, N * 4, 20);
            let mut dec = VideoDecoder::new(codec).expect("decoder");
            assert!(
                dec.decode(&out[VIDEO_HEADER_LEN..]).unwrap_or(false),
                "{codec:?}"
            );
            let declared = codec != Codec::Vp8;
            let (k, other, other_name) = if declared {
                (chroma_siting::BT709, chroma_siting::BT601, "BT.601")
            } else {
                (chroma_siting::BT601, chroma_siting::BT709, "BT.709")
            };
            let frame = dec.frame().expect("frame");
            let worst = chroma_siting::chart_error(&frame, k);
            let under_other = chroma_siting::chart_error(&frame, other);
            println!(
                "[chart] {codec:?}: worst |dRGB| {worst:.1} against the declared matrix, {under_other:.1} against {other_name}"
            );
            assert!(
                worst <= 12.0,
                "{codec:?} paints {worst:.1} off the chart against the matrix it declares, and {under_other:.1} against {other_name}: {}",
                if under_other < worst {
                    "it converted with that one and declared the other"
                } else {
                    "neither matrix explains it"
                }
            );
        }
    }

    /// The first packet of a fresh session, feeding `bgra` until one arrives: an encoder that
    /// pipelines answers the opening frames with nothing.
    pub(super) fn drain_first(
        enc: &mut FrameEncoder,
        bgra: &[u8],
        stride: usize,
        qp: u32,
    ) -> Vec<u8> {
        for t in 0..8u64 {
            let out = enc
                .encode_host(bgra, stride, false, t, qp, t == 0)
                .unwrap_or_else(|e| panic!("encode: {e}"));
            if !out.is_empty() {
                return out;
            }
        }
        Vec::new()
    }
}

/// The fixture the chroma-siting checks of every backend share.
#[cfg(test)]
pub(crate) mod chroma_siting {
    /// Four colors averaging to gray, of which no pixel, row pair, or column pair does: the
    /// chroma of a block comes out neutral only where all four were averaged. A 4:2:0 convert
    /// that keeps one pixel of the block, or one row or column of it, leaves the saturation
    /// subpixel-antialiased text carries on its glyph edges in the picture as visible color.
    pub const TILE: [[u8; 3]; 4] = [[0, 0, 128], [0, 255, 0], [128, 128, 255], [255, 0, 0]];

    /// `TILE` laid out as a `w`x`h` BGRA frame.
    pub fn bgra(w: usize, h: usize) -> Vec<u8> {
        let mut buf = vec![255u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = TILE[(y % 2) * 2 + (x % 2)];
                buf[(y * w + x) * 4..][..3].copy_from_slice(&[p[2], p[1], p[0]]);
            }
        }
        buf
    }

    /// The luma weights `(Kr, Kb)` of the matrix every session converts with and declares.
    pub const BT709: (f64, f64) = (0.2126, 0.0722);

    /// BT.601's weights, the other matrix a receiver might invert, which the checks use as the
    /// contrast: a mis-declared stream lands tens of levels away under them.
    pub const BT601: (f64, f64) = (0.299, 0.114);

    /// Limited-range Y/Cb/Cr of an RGB triple under the matrix `k` names.
    pub fn ycbcr(rgb: [f64; 3], k: (f64, f64)) -> [f64; 3] {
        let ([r, g, b], (kr, kb)) = (rgb, k);
        let y = kr * r + (1.0 - kr - kb) * g + kb * b;
        [
            16.0 + 219.0 * y / 255.0,
            128.0 + 224.0 * (b - y) / (2.0 * (1.0 - kb) * 255.0),
            128.0 + 224.0 * (r - y) / (2.0 * (1.0 - kr) * 255.0),
        ]
    }

    /// Its chroma pair alone.
    pub fn chroma(rgb: [f64; 3], k: (f64, f64)) -> (f64, f64) {
        let c = ycbcr(rgb, k);
        (c[1], c[2])
    }

    /// The inverse: the RGB a receiver paints from a limited-range Y/Cb/Cr under the matrix `k`
    /// names, which is what a client's presentation path computes.
    pub fn rgb(ycc: [f64; 3], k: (f64, f64)) -> [f64; 3] {
        let (kr, kb) = k;
        let (y, cb, cr) = (
            (ycc[0] - 16.0) / 219.0,
            (ycc[1] - 128.0) / 224.0,
            (ycc[2] - 128.0) / 224.0,
        );
        let r = y + 2.0 * (1.0 - kr) * cr;
        let b = y + 2.0 * (1.0 - kb) * cb;
        let g = (y - kr * r - kb * b) / (1.0 - kr - kb);
        [r, g, b].map(|c| (c * 255.0).clamp(0.0, 255.0))
    }

    /// The eight-patch color chart the matrix checks paint: the neutrals, whose chroma a wrong
    /// matrix leaves alone, and the saturated corners, which it moves by tens of levels.
    pub const CHART: [[u8; 3]; 8] = [
        [255, 255, 255],
        [128, 128, 128],
        [0, 0, 0],
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [255, 255, 0],
        [0, 255, 255],
    ];

    /// `CHART` as a `w`x`h` BGRA frame of eight columns.
    pub fn chart_bgra(w: usize, h: usize) -> Vec<u8> {
        let mut buf = vec![255u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = CHART[(x * CHART.len() / w).min(CHART.len() - 1)];
                buf[(y * w + x) * 4..][..3].copy_from_slice(&[p[2], p[1], p[0]]);
            }
        }
        buf
    }

    /// The worst channel error, over `CHART`'s patches, between the RGB a receiver paints from a
    /// decoded frame — inverting the matrix `k` names — and the RGB that was painted. Each
    /// patch is sampled well inside its column, so neither the 4:2:0 chroma edges nor the
    /// encoder's ringing at the boundaries counts.
    pub fn chart_error(f: &crate::webcam::convert::I420View<'_>, k: (f64, f64)) -> f64 {
        let cols = CHART.len();
        let mut worst = 0.0f64;
        for (i, want) in CHART.iter().enumerate() {
            let (x0, x1) = (f.width * i / cols, f.width * (i + 1) / cols);
            let (lo, hi) = (x0 + (x1 - x0) / 4, x1 - (x1 - x0) / 4);
            let (mut acc, mut n) = ([0.0f64; 3], 0.0f64);
            for y in f.height / 4..f.height * 3 / 4 {
                for x in lo..hi {
                    let ycc = [
                        f64::from(f.y[y * f.y_stride + x]),
                        f64::from(f.u[(y / 2) * f.uv_stride + x / 2]),
                        f64::from(f.v[(y / 2) * f.uv_stride + x / 2]),
                    ];
                    let got = rgb(ycc, k);
                    for c in 0..3 {
                        acc[c] += got[c];
                    }
                    n += 1.0;
                }
            }
            for c in 0..3 {
                worst = worst.max((acc[c] / n - f64::from(want[c])).abs());
            }
        }
        worst
    }

    /// The worst distance from neutral chroma over a decoded frame's chroma planes.
    pub fn worst(f: &crate::webcam::convert::I420View<'_>) -> f64 {
        let mut worst = 0.0f64;
        for r in 0..f.chroma_height() {
            for c in 0..f.chroma_width() {
                let i = r * f.uv_stride + c;
                worst = worst.max((f64::from(f.u[i]) - 128.0).hypot(f64::from(f.v[i]) - 128.0));
            }
        }
        worst
    }

    /// The tile is a fixture, so its premise is checked where it lives: the four chromas cancel,
    /// and every partial average a wrong siting would take is far from neutral.
    #[test]
    fn the_tile_separates_the_sitings() {
        let c: Vec<(f64, f64)> = TILE
            .iter()
            .map(|&p| chroma(p.map(f64::from), BT709))
            .collect();
        let mean = |of: &[usize]| {
            let (u, v) = of
                .iter()
                .fold((0.0, 0.0), |(u, v), &i| (u + c[i].0, v + c[i].1));
            let n = of.len() as f64;
            (u / n - 128.0).hypot(v / n - 128.0)
        };
        let all = mean(&[0, 1, 2, 3]);
        assert!(
            all < 0.5,
            "the whole tile must average to neutral chroma, off by {all:.1}"
        );
        for part in [
            vec![0],
            vec![1],
            vec![2],
            vec![3],
            vec![0, 1],
            vec![2, 3],
            vec![0, 2],
            vec![1, 3],
        ] {
            let d = mean(&part);
            assert!(
                d > 40.0,
                "pixels {part:?} average to chroma only {d:.1} from neutral"
            );
        }
    }
}

/// The chroma format a session actually carries, which is not always the one requested: a
/// hardware session only when the device carries it, the software path only when the build's
/// encoder for the codec does. Every consumer of "is this stream 4:4:4" reads it from here.
pub fn session_fullcolor(encoder: Option<&FrameEncoder>, settings: &CaptureSettings) -> bool {
    match encoder {
        Some(enc) => enc.is_fullcolor(),
        None => settings.video_fullcolor && software_fullcolor(settings.codec),
    }
}

/// The bits per sample a session carries, which is not always what was asked for: a
/// full-frame session's own answer, and for the CPU path (`None`) what its H.264 encoder
/// opens at.
pub fn session_bit_depth(encoder: Option<&FrameEncoder>, settings: &CaptureSettings) -> u32 {
    match encoder {
        Some(enc) => enc.bit_depth(),
        None => software::h264_bit_depth(settings),
    }
}

/// Whether a session signals full range: a software 4:4:4 session of x264's kind, which the
/// CPU path (`None`) is whenever it carries 4:4:4, or a device that converted in fixed
/// function at a range it chose. The one answer every description of a session reads, so no
/// two of them can disagree.
pub fn session_full_range(encoder: Option<&FrameEncoder>, settings: &CaptureSettings) -> bool {
    match encoder {
        Some(enc) => enc.is_full_range(),
        None => session_fullcolor(None, settings),
    }
}

#[cfg(test)]
mod hardware_encoder_tests {
    //! The hardware encoder table of a render node, on a host with an engine behind its first
    //! node. Ignored by default; run with `cargo test gpu_ -- --ignored --test-threads=1`.
    use super::*;

    /// The first node serves H.264 on the backend its driver selects, every entry names one of
    /// the two backends, and the second read is the remembered first.
    #[test]
    #[ignore]
    fn gpu_hardware_encoders_serve_h264_once_probed() {
        let served = hardware_encoders(0);
        assert!(
            served.iter().any(|(codec, ..)| *codec == Codec::H264),
            "node 0 serves {served:?}"
        );
        assert!(
            served
                .iter()
                .all(|(_, backend, ..)| matches!(*backend, "nvenc" | "vaapi"))
        );
        assert_eq!(hardware_encoders(0), served);
    }
}
