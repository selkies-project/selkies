/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! VA-API hardware sessions on a DRM render node for every video codec, driven through libva
//! directly.
//!
//! The session owns what a hardware encoder needs owned: the surfaces, the parameter buffers,
//! and, for the codecs that take them, the packed headers, so it also owns each picture's
//! reference lists. A frame a client lost is left out of the predictions the way NVENC and
//! libx264 allow (`encoders/reference.rs`): H.264 and HEVC keep the decoded picture buffer
//! the level admits and name the newest surviving frame in the slice header, VP9 and AV1
//! address their eight buffer slots, and VP8 steers its three buffers. A driver that writes its
//! own slice headers predicts from the previous frame alone, so such a session declares a
//! one-frame buffer.
//!
//! Pixels reach the codec on a VA surface -- a Wayland dmabuf imported in place, or a packed
//! host frame uploaded -- and the video processor converts to the surface format on the GPU,
//! so no colorspace conversion happens on the CPU. Chroma follows `video_fullcolor` where the
//! codec carries 4:4:4 (HEVC, and VP9 as profile 1), and the sample depth `video_bit_depth`
//! where the driver encodes the codec's 10-bit profile (HEVC Main 10 and Main 4:4:4 10, VP9
//! profiles 2 and 3, AV1), a session staying at 8 bits where it does not. A 4:4:4 session
//! tries the surface formats
//! the driver allocates and its video processor renders, read through a `VAProfileNone`
//! configuration, until one survives the surface pool, the convert, and the codec open. Every
//! session converts with the BT.709 matrix the sRGB source's own primaries and transfer belong
//! to, at limited range, and declares it; VP8 is held to BT.601, the only matrix its bitstream
//! can name. libva itself is loaded at run time, so the host's copy serves the driver it was
//! built for, and a test can put its own functions behind the same table.

mod av1;
mod h264;
mod h265;
#[cfg(test)]
pub(crate) mod mock;
#[cfg(test)]
mod session_tests;
mod vp8;
mod vp9;

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::sync::{Arc, OnceLock};

use smithay::backend::allocator::{Buffer, dmabuf::Dmabuf};
use va_sys::*;

use super::codec::{
    Codec, Hardware, VIDEO_HEADER_LEN, av1_is_key, frame_type_from_key, h264_frame_type,
    h265_frame_type, push_video_header, vp8_is_key, vp9_is_key,
};
use super::dmabuf_identity::DmaBufIdentity;
use super::frame_rate::FrameRate;
use super::reference::{
    ANCHORS, Invalidation, REFERENCE_FRAMES, Reference, ReferenceReport, ReferenceSlots,
    ReferenceWindow, SlotPlan,
};
use super::session::{RateSettings, check_host_frame};
use super::sps::{NoReorder, h264_frame_num_range};
use crate::CaptureSettings;

/// The slices an H.264 or HEVC picture is cut into, a decoder threading a frame across them.
/// AMD's VCE is the exception for H.264: it codes a picture cut into four slices at less than
/// half its one-slice rate (Radeon Pro VII, 2160p: 44.6 against 20.4 ms a frame, 1080p: 8.7
/// against 5.7), where its HEVC and every other engine measured here cost nothing for them.
/// The low-power H.264 of Skylake and Broxton is the other (`whole_picture_vdenc`), one slice
/// where a session takes it, which it does only where the driver offers no full entry point.
const SLICES: u32 = 4;
/// The bytes a coded buffer holds: the uncompressed picture and some, an upper bound on any
/// frame.
/// The frame budget, in bits a pixel, under which radeonsi's rate control cannot refine a still
/// screen it says no quantizer of, so the cleanup ends on a key frame (`refines`). 1080p text,
/// pixelflux alone, the cleanup through the rate control at 0.004 / 0.008 / 0.016 / 0.03 / 0.12
/// bits a pixel a frame: H.264 18.0 (60 fps 0.5 Mbit/s) / 30.0-38.2 / 40.1-52.6 / 43.4 / 56.4
/// dB, HEVC 29.1 / - / 46.2 / 33.1 / -, each the full 10 s at the session's rate (2.1 MB at 60
/// fps 2 Mbit/s); a key frame at `HELD_KEY_BITS_PER_PIXEL` came to 51.5 dB (H.264) and 47.1
/// (HEVC) on 0.4-0.55 MB.
const REFINE_BITS_PER_PIXEL: f64 = 0.04;

/// The bits a pixel a held key frame of a constant rate is given at least (`held_key_bits`):
/// VCE spends a frame's budget down the picture and codes the rows it runs out on at its
/// coarsest, so a key frame short of what the picture needs at the paint-over quality comes
/// out sharp at the top and smeared below: 1080p text in 0.8 bits a pixel came to 271 kB at
/// 25.5 dB (H.264, 52 dB in the top rows, 18 in the bottom ones) and 307 kB at 39.0 (HEVC), in
/// 1.6 to 463 kB at 51.5 and 380 kB at 47.1. The driver's best quality level for the key frame
/// changed nothing.
const HELD_KEY_BITS_PER_PIXEL: f64 = 1.6;

/// Whether a constant rate of `bps` at `fps` gives a `width` x `height` frame the budget
/// radeonsi's rate control refines a still screen in (`REFINE_BITS_PER_PIXEL`).
fn refines(bps: u32, fps: f64, width: u32, height: u32) -> bool {
    let pixels = f64::from(width) * f64::from(height);
    f64::from(bps) / fps.max(1.0) >= REFINE_BITS_PER_PIXEL * pixels.max(1.0)
}

/// The bits a key frame held at a constant rate of `bps` is given for a `width` x `height`
/// picture: `HELD_KEY_BUDGET_S` of the rate, and at least `HELD_KEY_BITS_PER_PIXEL`, the
/// maintainer's rule that a key frame is capped only where the cap leaves the picture good.
fn held_key_bits(bps: u32, width: u32, height: u32) -> f64 {
    let pixels = f64::from(width) * f64::from(height);
    (f64::from(bps) * crate::encoders::HELD_KEY_BUDGET_S).max(HELD_KEY_BITS_PER_PIXEL * pixels)
}

fn coded_buffer_size(width: u32, height: u32) -> u32 {
    3 * width * height + (1 << 16)
}

/// The libva of this process, loaded once, or why it could not be.
fn libva() -> Result<VaApi, String> {
    static LIBVA: OnceLock<Result<Libva, String>> = OnceLock::new();
    LIBVA
        .get_or_init(|| unsafe { Libva::load() })
        .as_ref()
        .map(|lib| lib.api)
        .map_err(Clone::clone)
}

/// How frames reach a session: a Wayland DRM-PRIME dmabuf, or packed host pixels in B,G,R,A
/// (`rgba: false`) or R,G,B,A (`rgba: true`) byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Dmabuf,
    Host { rgba: bool },
}

/// The 8-bit 4:4:4 surface formats a session tries, in order. The order only breaks a tie
/// on a driver whose video processor renders both: every frame reaches either through the
/// convert, so neither is nearer the host.
const FULLCOLOR_FOURCCS: [u32; 2] = [VA_FOURCC_444P, VA_FOURCC_XYUV];

/// The surface formats a session of this chroma and sample depth tries, in order.
fn surface_fourccs(fullcolor: bool, bit_depth: u32) -> &'static [u32] {
    match (fullcolor, bit_depth) {
        (false, 10) => &[VA_FOURCC_P010],
        (true, 10) => &[VA_FOURCC_Y410],
        (true, _) => &FULLCOLOR_FOURCCS,
        (false, _) => &[VA_FOURCC_NV12],
    }
}

/// The render target format of a chroma and sample depth.
fn rt_format(fullcolor: bool, bit_depth: u32) -> u32 {
    match (fullcolor, bit_depth) {
        (false, 10) => VA_RT_FORMAT_YUV420_10,
        (true, 10) => VA_RT_FORMAT_YUV444_10,
        (true, _) => VA_RT_FORMAT_YUV444,
        (false, _) => VA_RT_FORMAT_YUV420,
    }
}

/// The name of a surface format, for the session log.
pub(crate) fn fourcc_name(fourcc: u32) -> String {
    match fourcc {
        VA_FOURCC_NV12 => "nv12".into(),
        VA_FOURCC_P010 => "p010".into(),
        VA_FOURCC_Y410 => "y410".into(),
        VA_FOURCC_444P => "yuv444p".into(),
        VA_FOURCC_XYUV => "vuyx".into(),
        VA_FOURCC_BGRA => "bgra".into(),
        VA_FOURCC_RGBA => "rgba".into(),
        other => String::from_utf8_lossy(&other.to_le_bytes())
            .trim()
            .to_string(),
    }
}

unsafe extern "C" fn log_error(_user: *mut c_void, message: *const c_char) {
    if !message.is_null() {
        eprintln!(
            "[vaapi] {}",
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .trim_end()
        );
    }
}

unsafe extern "C" fn log_info(_user: *mut c_void, message: *const c_char) {
    if !message.is_null() {
        crate::log::debug!(
            "[vaapi] {}",
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .trim_end()
        );
    }
}

/// What bounds the frames of a constant-rate session, by what the driver's rate control does
/// with each bound.
///
/// A buffer of `vbv_bits` (a frame and a half) with every frame capped at it is the tightest,
/// and the default. Intel's iHD stops coding under it: measured on Intel Arc at 1080p and
/// 60 fps, its H.264 and HEVC rate controls skip every block and pad the frame with zeros up
/// to the target once the buffer or the cap binds, and its AV1 one spends a third of the
/// target, so a scroll sat at 19 to 20 dB at 2, 8, and 30 Mbit/s alike and a still screen was
/// never refined. So on that driver H.264 and HEVC run with no bound of their own, where a
/// still screen of text at 2 Mbit/s reaches 46 to 51 dB, and AV1, which takes no cap, in a
/// buffer of `IHD_AV1_BUFFER_S`, which halved its largest frame. The driver's low-delay frame
/// tolerance holds an H.264 frame to three budgets, but leaves a still screen of dense text
/// at 27 dB, every block skipped; a cap of six budgets cost HEVC 3 dB. Its VP9 refines in
/// the default buffer and keeps it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ConstantRate {
    /// The session's buffer, and the frame cap where the driver takes one.
    Buffer,
    /// No buffer and no cap.
    Unbounded,
    /// A buffer of this many seconds of the target and no cap.
    Seconds(f64),
}

/// Seconds of the target iHD's AV1 rate control is given as a buffer (`ConstantRate`).
const IHD_AV1_BUFFER_S: f64 = 0.1;

/// The rows a reconstruction is compared with its source on (`VaapiEncoder::last_psnr`): one
/// in this many.
const PSNR_ROW_STEP: usize = 8;

/// The PSNR under which a reconstruction is no picture of its source (`picture_psnr`): the
/// coarsest quantizer leaves a screen of text near 19 dB.
const UNREADABLE_DB: f32 = 12.0;

/// Drop the zero bytes a driver's constant rate pads an H.264 or HEVC frame with (iHD fills a
/// frame up to the target after the last slice): trailing zero bytes are no part of a NAL
/// unit, whose last byte carries the stop bit.
fn strip_zero_padding(out: &mut Vec<u8>, from: usize) {
    let end = out[from..]
        .iter()
        .rposition(|&b| b != 0)
        .map_or(from, |i| from + i + 1);
    out.truncate(end);
}

/// A VA display opened on a render node, terminated with the device.
pub(crate) struct Device {
    api: VaApi,
    display: VADisplay,
    _fd: OwnedFd,
    vendor: String,
    /// Whether the device encodes H.264 on AMD's VCE (`amd_vce`).
    vce: bool,
    /// Whether the device's low-power H.264 encoder codes a picture as one slice only
    /// (`whole_picture_vdenc`).
    whole_picture_vdenc: bool,
}

/// Whether the render node `fd` is an Intel part whose low-power H.264 encoder walks a picture
/// once, top to bottom, wherever its slices are cut: Skylake and Broxton, told by the PCI
/// device the kernel names, since their drivers list the entry point, and iHD a slice
/// structure, as they do for the later parts that code the cut. A picture of four slices came
/// out corrupt below the first there (HD 530), and whole from the full entry point.
fn whole_picture_vdenc(fd: c_int) -> bool {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return false;
    }
    let id = |name: &str| {
        let path = format!(
            "/sys/dev/char/{}:{}/device/{name}",
            libc::major(stat.st_rdev),
            libc::minor(stat.st_rdev)
        );
        let text = std::fs::read_to_string(path).ok()?;
        u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
    };
    id("vendor") == Some(0x8086) && id("device").is_some_and(skylake_or_broxton)
}

/// Whether an Intel PCI device id is a Skylake or a Broxton part's.
fn skylake_or_broxton(device: u32) -> bool {
    matches!(
        device,
        0x1900..=0x19ff | 0x0a84 | 0x1a84 | 0x1a85 | 0x5a84 | 0x5a85
    )
}

/// Whether the render node `fd` is an amdgpu device whose video encoder is VCE rather than VCN.
/// The kernel reports the video blocks a device carries (`AMDGPU_INFO_HW_IP_COUNT`, the query
/// Mesa reads to pick its encoder): a VCE-generation part answers for VCE and refuses the
/// query for VCN's encoder. Any other driver is not asked.
fn amd_vce(fd: c_int) -> bool {
    #[repr(C)]
    struct Version {
        major: c_int,
        minor: c_int,
        patch: c_int,
        name_len: usize,
        name: *mut c_char,
        date_len: usize,
        date: *mut c_char,
        desc_len: usize,
        desc: *mut c_char,
    }
    #[repr(C)]
    struct Info {
        return_pointer: u64,
        return_size: u32,
        query: u32,
        ip_type: u32,
        ip_instance: u32,
        reserved: [u32; 2],
    }
    const DRM_IOCTL_VERSION: u64 = 0xc040_6400;
    const DRM_IOCTL_AMDGPU_INFO: u64 = 0x4020_6445;
    const AMDGPU_INFO_HW_IP_COUNT: u32 = 0x03;
    const AMDGPU_HW_IP_VCE: u32 = 4;
    const AMDGPU_HW_IP_VCN_ENC: u32 = 7;
    let mut name = [0u8; 16];
    let mut version = Version {
        major: 0,
        minor: 0,
        patch: 0,
        name_len: name.len(),
        name: name.as_mut_ptr() as *mut c_char,
        date_len: 0,
        date: ptr::null_mut(),
        desc_len: 0,
        desc: ptr::null_mut(),
    };
    if unsafe { libc::ioctl(fd, DRM_IOCTL_VERSION as _, &mut version as *mut Version) } != 0
        || name[..version.name_len.min(name.len())] != *b"amdgpu"
    {
        return false;
    }
    let engines = |ip_type: u32| {
        let mut count: u32 = 0;
        let mut info = Info {
            return_pointer: &mut count as *mut u32 as u64,
            return_size: 4,
            query: AMDGPU_INFO_HW_IP_COUNT,
            ip_type,
            ip_instance: 0,
            reserved: [0; 2],
        };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_AMDGPU_INFO as _, &mut info as *mut Info) } != 0 {
            0
        } else {
            count
        }
    };
    engines(AMDGPU_HW_IP_VCE) > 0 && engines(AMDGPU_HW_IP_VCN_ENC) == 0
}

/// The display is used from the one thread that holds the session.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { (self.api.vaTerminate)(self.display) };
    }
}

impl Device {
    /// Open the render node behind `encode_node_index` (`renderD128` when none is named) and
    /// initialize the driver on it.
    pub(crate) fn open(api: VaApi, encode_node_index: i32) -> Result<Self, String> {
        let render_node = format!("/dev/dri/renderD{}", 128 + encode_node_index.max(0));
        // NVIDIA's VA driver only decodes, and its constructor calls cuInit under the loader's
        // lock, while a cuInit in another thread loads libraries under CUDA's: each would wait
        // for the other.
        if crate::get_gpu_driver(encode_node_index.max(0)).contains("nvidia") {
            return Err(format!(
                "{render_node} is NVIDIA's, whose VA driver encodes nothing"
            ));
        }
        let path = CString::new(render_node.clone()).unwrap();
        let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(format!(
                "{render_node}: {}",
                std::io::Error::last_os_error()
            ));
        }
        Self::on(api, unsafe { OwnedFd::from_raw_fd(raw) }, &render_node)
    }

    /// Initialize the driver on the open render node `fd`, with its messages routed to the
    /// log; `render_node` names it in errors.
    pub(crate) fn on(api: VaApi, fd: OwnedFd, render_node: &str) -> Result<Self, String> {
        let display = unsafe { (api.vaGetDisplayDRM)(fd.as_raw_fd()) };
        if display.is_null() {
            return Err(format!("{render_node} carries no VA display"));
        }
        unsafe {
            (api.vaSetErrorCallback)(display, Some(log_error), ptr::null_mut());
            (api.vaSetInfoCallback)(display, Some(log_info), ptr::null_mut());
        }
        let (mut major, mut minor) = (0 as c_int, 0 as c_int);
        let status = unsafe { (api.vaInitialize)(display, &mut major, &mut minor) };
        if status != VA_STATUS_SUCCESS as VAStatus {
            return Err(format!(
                "{render_node}: no VA driver initialized ({})",
                error_text(&api, status)
            ));
        }
        let vendor = unsafe {
            let text = (api.vaQueryVendorString)(display);
            if text.is_null() {
                String::new()
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            }
        };
        let vce = amd_vce(fd.as_raw_fd());
        let whole_picture_vdenc = whole_picture_vdenc(fd.as_raw_fd());
        Ok(Self {
            api,
            display,
            _fd: fd,
            vendor,
            vce,
            whole_picture_vdenc,
        })
    }

    /// Whether the driver's video processor, converting to 4:2:0, reads a linear surface at a
    /// pitch rounded up to 64 bytes rather than the one its import declares, as Intel's does:
    /// such a surface converts sheared unless its pitch is already a multiple of 64.
    fn rounds_linear_pitch(&self) -> bool {
        self.vendor.contains("Intel")
    }

    /// The quantizer curve the driver's sessions read: Intel's has its own where its encoders
    /// were measured apart from AMD's.
    fn hardware(&self) -> Hardware {
        if self.vendor.contains("Intel") {
            Hardware::VaapiIntel
        } else {
            Hardware::Vaapi
        }
    }

    /// Whether an H.264 picture of `entrypoint` is left as one slice (`SLICES`): on AMD's VCE,
    /// and on a low-power encoder that codes no other cut.
    fn one_slice_h264(&self, entrypoint: VAEntrypoint) -> bool {
        self.vce || (self.whole_picture_vdenc && entrypoint == VAEntrypointEncSliceLP)
    }

    /// Whether a session keeps anchors (`ReferenceWindow::with_anchors`): the driver codes a
    /// picture from the reference it is named, as radeonsi's VCE (H.264) and UVD (H.265) were
    /// measured to (`vaapi_predicts_past_a_lost_frame`, `vaapi_predicts_from_an_anchor`); others
    /// are not yet. H.264 keeps one, long-term; H.265 keeps two, listed as short-term pictures in
    /// every reference picture set, as NVENC's are.
    fn keeps_anchors(&self) -> bool {
        self.vendor.contains("radeonsi")
    }

    /// Whether the driver's rate control stops coding in a buffer of a frame or two, as Intel's
    /// iHD does (`ConstantRate`).
    fn starves_in_a_small_buffer(&self) -> bool {
        self.vendor.contains("iHD")
    }

    /// The highest H.264 `level_idc` the driver's sequence parameters may name
    /// (`Negotiated::h264_driver_level`). iHD's level table ends at 5.2: above it its rate
    /// control is handed a maximum frame size of 0 (`GetProfileLevelMaxFrameSize`), and every
    /// constant-rate picture after the key frame comes out undecodable past its first rows a
    /// slice (29 of 30 at 2160p and 75 fps on an Arc A750,
    /// `vaapi_h264_constant_rate_decodes_above_level_5_2`), where a constant quantizer, which
    /// runs no rate control, decodes clean. Told 5.2, its frame cap stays above any frame.
    fn h264_driver_level(&self) -> u32 {
        if self.vendor.contains("iHD") {
            52
        } else {
            u32::MAX
        }
    }

    fn check(&self, status: VAStatus, what: &str) -> Result<(), String> {
        if status == VA_STATUS_SUCCESS as VAStatus {
            Ok(())
        } else {
            Err(format!("{what}: {}", error_text(&self.api, status)))
        }
    }

    /// The profiles the driver lists.
    fn profiles(&self) -> Result<Vec<VAProfile>, String> {
        let mut profiles = vec![
            0 as VAProfile;
            unsafe { (self.api.vaMaxNumProfiles)(self.display) }.max(0) as usize
        ];
        let mut listed: c_int = 0;
        self.check(
            unsafe {
                (self.api.vaQueryConfigProfiles)(self.display, profiles.as_mut_ptr(), &mut listed)
            },
            "vaQueryConfigProfiles",
        )?;
        profiles.truncate(listed.max(0) as usize);
        Ok(profiles)
    }

    /// The encode entry points of `profile`, the low-power one first, since recent Intel
    /// generations expose it as the only one for HEVC, VP9, and AV1 and it is the shorter path
    /// where both exist; a session the low-power one cannot serve, such as a constant-rate one
    /// where it runs constant quantizer only, takes the full one. Empty where the profile
    /// encodes on neither. H.264 on a part whose low-power encoder codes one slice a picture
    /// takes the full one first instead (`on_device`).
    fn encode_entrypoints(&self, profile: VAProfile) -> Vec<VAEntrypoint> {
        let mut entrypoints = vec![
            0 as VAEntrypoint;
            unsafe { (self.api.vaMaxNumEntrypoints)(self.display) }.max(0)
                as usize
        ];
        let mut listed: c_int = 0;
        let status = unsafe {
            (self.api.vaQueryConfigEntrypoints)(
                self.display,
                profile,
                entrypoints.as_mut_ptr(),
                &mut listed,
            )
        };
        if status != VA_STATUS_SUCCESS as VAStatus {
            return Vec::new();
        }
        let listed = &entrypoints[..listed.max(0) as usize];
        [VAEntrypointEncSliceLP, VAEntrypointEncSlice]
            .into_iter()
            .filter(|e| listed.contains(e))
            .collect()
    }

    /// One configuration attribute of a profile and entry point, None where the driver does
    /// not report it.
    fn attribute(
        &self,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        kind: VAConfigAttribType,
    ) -> Option<u32> {
        let mut attrib = VAConfigAttrib {
            type_: kind,
            value: 0,
        };
        let status = unsafe {
            (self.api.vaGetConfigAttributes)(self.display, profile, entrypoint, &mut attrib, 1)
        };
        (status == VA_STATUS_SUCCESS as VAStatus && attrib.value != VA_ATTRIB_NOT_SUPPORTED)
            .then_some(attrib.value)
    }

    /// The surface attributes a configuration reports, asked for the way libva wants: once
    /// for the count, once for the values. Empty where the driver reports none.
    fn surface_attributes(&self, config: VAConfigID) -> Vec<VASurfaceAttrib> {
        let mut count: c_uint = 0;
        let mut attribs: Vec<VASurfaceAttrib> = Vec::new();
        for pass in 0..2 {
            let list = if pass == 0 {
                ptr::null_mut()
            } else {
                attribs.as_mut_ptr()
            };
            if unsafe {
                (self.api.vaQuerySurfaceAttributes)(self.display, config, list, &mut count)
            } != VA_STATUS_SUCCESS as VAStatus
            {
                return Vec::new();
            }
            attribs.resize(count as usize, unsafe { std::mem::zeroed() });
        }
        attribs
    }

    /// The surface formats a configuration renders into, empty where it constrains none.
    fn surface_fourccs(&self, config: VAConfigID) -> Vec<u32> {
        self.surface_attributes(config)
            .iter()
            .filter(|a| a.type_ == VASurfaceAttribPixelFormat)
            .map(|a| unsafe { a.value.value.i } as u32)
            .collect()
    }

    /// The surface alignment a configuration asks for, as `(width, height)`, one where it
    /// asks none.
    fn surface_alignment(&self, config: VAConfigID) -> (u32, u32) {
        self.surface_attributes(config)
            .iter()
            .find(|a| a.type_ == VASurfaceAttribAlignmentSize)
            .map(|a| {
                let v = unsafe { a.value.value.i } as u32;
                (1 << (v & 0xf), 1 << ((v & 0xf0) >> 4))
            })
            .unwrap_or((1, 1))
    }

    /// Surfaces of `fourcc` at `width` x `height`, `count` of them, on the driver's own memory.
    fn create_surfaces(
        &self,
        rt_format: u32,
        fourcc: u32,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Vec<VASurfaceID>, String> {
        let mut attrib: VASurfaceAttrib = unsafe { std::mem::zeroed() };
        attrib.type_ = VASurfaceAttribPixelFormat;
        attrib.flags = VA_SURFACE_ATTRIB_SETTABLE;
        attrib.value.type_ = VAGenericValueTypeInteger;
        attrib.value.value.i = fourcc as i32;
        let mut surfaces = vec![VA_INVALID_SURFACE; count];
        self.check(
            unsafe {
                (self.api.vaCreateSurfaces)(
                    self.display,
                    rt_format,
                    width,
                    height,
                    surfaces.as_mut_ptr(),
                    count as c_uint,
                    &mut attrib,
                    1,
                )
            },
            &format!(
                "this VA-API driver allocates no {} surfaces",
                fourcc_name(fourcc)
            ),
        )?;
        Ok(surfaces)
    }

    fn destroy_surfaces(&self, surfaces: &mut [VASurfaceID]) {
        if !surfaces.is_empty() {
            unsafe {
                (self.api.vaDestroySurfaces)(
                    self.display,
                    surfaces.as_mut_ptr(),
                    surfaces.len() as c_int,
                )
            };
        }
    }

    fn create_buffer(
        &self,
        context: VAContextID,
        kind: VABufferType,
        data: &[u8],
    ) -> Result<VABufferID, String> {
        self.create_buffer_raw(
            context,
            kind,
            data.len() as c_uint,
            data.as_ptr() as *mut c_void,
        )
    }

    /// A buffer the driver fills, created without initial data: iHD refuses a coded buffer
    /// handed any.
    fn create_output_buffer(
        &self,
        context: VAContextID,
        kind: VABufferType,
        size: u32,
    ) -> Result<VABufferID, String> {
        self.create_buffer_raw(context, kind, size as c_uint, ptr::null_mut())
    }

    fn create_buffer_raw(
        &self,
        context: VAContextID,
        kind: VABufferType,
        size: c_uint,
        data: *mut c_void,
    ) -> Result<VABufferID, String> {
        let mut id = VA_INVALID_ID;
        self.check(
            unsafe {
                (self.api.vaCreateBuffer)(self.display, context, kind, size, 1, data, &mut id)
            },
            &format!("vaCreateBuffer(type {kind})"),
        )?;
        Ok(id)
    }
}

fn error_text(api: &VaApi, status: VAStatus) -> String {
    let text = unsafe { (api.vaErrorStr)(status) };
    if text.is_null() {
        format!("VA status {status}")
    } else {
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }
}

/// The video codecs the VA-API driver of the render node behind `encode_node_index` encodes:
/// those one of whose profiles the driver lists with an encode entry point, which is what
/// `vainfo` reports and what a session checks first. The device is opened the way a session
/// opens it and released. An error names the step that failed: no such node, no VA driver on
/// it, or a libva the probe cannot reach.
pub(crate) fn probe_codecs(encode_node_index: i32) -> Result<Vec<Served>, String> {
    probe_codecs_on(&Device::open(libva()?, encode_node_index)?)
}

/// A codec a device encodes, with the formats it encodes the codec in.
pub(crate) type Served = (Codec, super::Formats);

/// `probe_codecs` on an open device: each codec a profile of its 8-bit 4:2:0 ladder encodes,
/// and whether the profile each other format's session opens under encodes and renders that
/// format's surfaces too.
pub(crate) fn probe_codecs_on(device: &Device) -> Result<Vec<Served>, String> {
    let profiles = device.profiles()?;
    let entrypoints = |profile: VAProfile| {
        if profiles.contains(&profile) {
            device.encode_entrypoints(profile)
        } else {
            Vec::new()
        }
    };
    Ok(Codec::VIDEO
        .into_iter()
        .filter(|&codec| {
            profile_ladder(codec, false, 8)
                .into_iter()
                .any(|p| !entrypoints(p).is_empty())
        })
        .map(|codec| {
            let carries = |fullcolor: bool, bit_depth: u32| {
                profile_ladder(codec, fullcolor, bit_depth)
                    .into_iter()
                    .any(|p| {
                        entrypoints(p).into_iter().any(|e| {
                            device
                                .attribute(p, e, VAConfigAttribRTFormat)
                                .is_none_or(|f| f & rt_format(fullcolor, bit_depth) != 0)
                        })
                    })
            };
            (
                codec,
                super::Formats {
                    fullcolor: carries(true, 8),
                    ten_bit: [carries(false, 10), carries(true, 10)],
                },
            )
        })
        .collect())
}

/// The VA profiles a session opens under, in order of preference: for 8-bit 4:2:0, the ones
/// such a session comes up as; for 4:4:4 or 10 bits, the profile that carries the format, or
/// nothing where the codec has none the session serves. AV1's main profile carries both of
/// its depths, told apart by the surface format.
fn profile_ladder(codec: Codec, fullcolor: bool, bit_depth: u32) -> Vec<VAProfile> {
    match (codec, fullcolor, bit_depth) {
        (Codec::H264, false, 8) => vec![
            VAProfileH264High,
            VAProfileH264Main,
            VAProfileH264ConstrainedBaseline,
        ],
        (Codec::H265, false, 8) => vec![VAProfileHEVCMain],
        (Codec::H265, false, 10) => vec![VAProfileHEVCMain10],
        (Codec::H265, true, 8) => vec![VAProfileHEVCMain444],
        (Codec::H265, true, 10) => vec![VAProfileHEVCMain444_10],
        (Codec::Vp8, false, 8) => vec![VAProfileVP8Version0_3],
        (Codec::Vp9, false, 8) => vec![VAProfileVP9Profile0],
        (Codec::Vp9, true, 8) => vec![VAProfileVP9Profile1],
        (Codec::Vp9, false, 10) => vec![VAProfileVP9Profile2],
        (Codec::Vp9, true, 10) => vec![VAProfileVP9Profile3],
        (Codec::Av1, false, 8 | 10) => vec![VAProfileAV1Profile0],
        _ => Vec::new(),
    }
}

/// The parameter buffers of one picture, in the order they are rendered.
pub(super) struct Buffers {
    entries: Vec<(VABufferType, Vec<u8>)>,
}

impl Buffers {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// One parameter structure as a buffer of its type.
    pub(super) fn push<T: Copy>(&mut self, kind: VABufferType, value: &T) {
        let bytes = unsafe {
            std::slice::from_raw_parts(value as *const T as *const u8, std::mem::size_of::<T>())
        };
        self.entries.push((kind, bytes.to_vec()));
    }

    /// One miscellaneous parameter, behind the header naming its type.
    pub(super) fn push_misc<T: Copy>(&mut self, kind: VAEncMiscParameterType, value: &T) {
        let mut bytes = kind.to_ne_bytes().to_vec();
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts(value as *const T as *const u8, std::mem::size_of::<T>())
        });
        self.entries.push((VAEncMiscParameterBufferType, bytes));
    }

    /// One packed header: its parameter buffer, then its bytes.
    pub(super) fn push_packed(&mut self, kind: u32, bytes: &[u8], bit_length: u32) {
        let param = VAEncPackedHeaderParameterBuffer {
            type_: kind,
            bit_length,
            has_emulation_bytes: 1,
            va_reserved: [0; 4],
        };
        self.push(VAEncPackedHeaderParameterBufferType, &param);
        self.entries
            .push((VAEncPackedHeaderDataBufferType, bytes.to_vec()));
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[(VABufferType, Vec<u8>)] {
        &self.entries
    }
}

/// What the session negotiated at open, which the codec arms read.
#[derive(Clone, Copy)]
pub(super) struct Negotiated {
    pub profile: VAProfile,
    pub entrypoint: VAEntrypoint,
    /// The packed headers the driver takes, `VA_ENC_PACKED_HEADER_*` bits.
    pub packed: u32,
    /// The rate-control mode, `VA_RC_CQP` or `VA_RC_CBR`.
    pub rc_mode: u32,
    pub width: u32,
    pub height: u32,
    pub fps: FrameRate,
    pub bits_per_second: u32,
    /// The reference frames the decoded picture buffer holds.
    pub dpb: u32,
    /// The level the decoded picture buffer was sized for, the lowest an H.264 or HEVC
    /// sequence declares.
    pub dpb_level: u32,
    pub fullcolor: bool,
    pub bit_depth: u32,
    /// The quantizer bounds of a constant-rate session in the codec's own domain, 0 for none.
    pub min_qp: u32,
    pub max_qp: u32,
    /// The finest quantizer the entry point codes a constant-quantizer picture at
    /// (`h265::INTEL_LOW_POWER_MIN_QP` on Intel's low-power HEVC encoder, 1 elsewhere).
    pub qp_floor: u32,
    /// Whether the driver's rate control codes its quantizer in every coding unit as a delta
    /// from the slice's (Intel's full HEVC encoder), which the slice's quantizer then has to keep
    /// within the syntax's range (`h265::Arm::configure`).
    pub unit_qp_deltas: bool,
    /// The highest H.264 level the driver is told (`Device::h264_driver_level`); the
    /// packed SPS still declares the level the stream needs.
    pub h264_driver_level: u32,
    /// The regions of interest a frame may name with a quantizer delta
    /// (`VAConfigAttribEncROI`), 0 where the driver takes none, or none under its rate control
    /// at a constant rate.
    pub roi_regions: u32,
}

/// One frame as the codec arms see it: what it is, where its reconstruction and coded output
/// go, and what it may predict from.
pub(super) struct Frame<'a> {
    pub key: bool,
    /// The session's count of encoded frames.
    pub pts: u64,
    /// The count at the last key frame, where a codec's own counters restart.
    pub key_pts: u64,
    pub recon: VASurfaceID,
    pub coded: VABufferID,
    /// The frame predicted from, by timestamp and reconstruction surface.
    pub reference: Option<(u64, VASurfaceID)>,
    /// Every frame the decoder holds, oldest first, with the surface each was reconstructed
    /// into and whether a client lost it.
    pub held: &'a [(u64, VASurfaceID, bool)],
    /// Whether the session keeps long-term anchors, its key frame the first.
    pub anchored: bool,
    /// The long-term index the frame is marked under, an anchor (`ReferenceWindow::plan_anchor`).
    pub anchor: Option<u8>,
    /// The anchors among `held`, by timestamp, with the long-term index each is marked under.
    pub long_term: &'a [(u64, u8)],
    /// The quantizer, in the codec's domain, of a constant-quantizer session.
    pub qp: u32,
    /// VP8's buffer plan.
    pub slots: SlotPlan,
    /// The surface each VP8 buffer holds, LAST, GOLDEN, ALTREF.
    pub slot_surfaces: [VASurfaceID; 3],
}

/// The codec arm of a session.
enum Arm {
    H264(Box<h264::Arm>),
    H265(h265::Arm),
    Vp8(vp8::Arm),
    Vp9(vp9::Arm),
    Av1(av1::Arm),
}

impl Arm {
    /// The surface alignment the codec's blocks want.
    fn alignment(&self) -> (u32, u32) {
        match self {
            Arm::H264(_) | Arm::Vp8(_) => (16, 16),
            Arm::H265(a) => a.alignment(),
            Arm::Vp9(_) => (64, 64),
            Arm::Av1(a) => a.alignment(),
        }
    }

    /// The packed headers the codec writes itself.
    fn wanted_packed(&self) -> u32 {
        match self {
            Arm::H264(_) | Arm::H265(_) => {
                VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_SLICE
            }
            Arm::Av1(_) => VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE,
            Arm::Vp8(_) | Arm::Vp9(_) => 0,
        }
    }

    /// Whether the session can name what a frame predicts from and leave a lost one out: the
    /// codecs whose headers the session writes need the driver to take them, since a driver
    /// writing its own slice headers names the references it chooses.
    fn tracks_references(&self, packed: u32) -> bool {
        match self {
            Arm::H264(_) | Arm::H265(_) => packed & VA_ENC_PACKED_HEADER_SLICE != 0,
            Arm::Av1(_) => packed & VA_ENC_PACKED_HEADER_PICTURE != 0,
            Arm::Vp8(_) | Arm::Vp9(_) => true,
        }
    }

    fn sequence(&mut self, negotiated: &Negotiated, out: &mut Buffers) {
        match self {
            Arm::H264(a) => a.sequence(negotiated, out),
            Arm::H265(a) => a.sequence(negotiated, out),
            Arm::Vp8(a) => a.sequence(negotiated, out),
            Arm::Vp9(a) => a.sequence(negotiated, out),
            Arm::Av1(a) => a.sequence(negotiated, out),
        }
    }

    fn picture(
        &mut self,
        negotiated: &Negotiated,
        frame: &Frame,
        out: &mut Buffers,
    ) -> Result<(), String> {
        match self {
            Arm::H264(a) => a.picture(negotiated, frame, out),
            Arm::H265(a) => a.picture(negotiated, frame, out),
            Arm::Vp8(a) => a.picture(negotiated, frame, out),
            Arm::Vp9(a) => a.picture(negotiated, frame, out),
            Arm::Av1(a) => a.picture(negotiated, frame, out),
        }
    }
}

/// The references a session keeps: a decoded picture buffer, or VP8's three buffers.
enum References {
    Window(ReferenceWindow),
    Slots(ReferenceSlots),
}

/// The host frame's way onto a surface: through the surface's own image where the driver
/// derives one, else through an image the driver copies in.
struct HostUpload {
    surface: VASurfaceID,
    fourcc: u32,
    derive: bool,
    image: Option<VAImage>,
}

/// The dmabuf imports a zero-copy session keeps, past the few buffers any capture cycles.
const DMABUF_IMPORTS: usize = 8;

/// Quality index steps finer than the paint-over one a refined block is coded at
/// (`VaapiEncoder::refines`): on Intel's iHD (N97) 1080p H.264 text beside a video went from
/// 41.3 dB to 44.1, 45.4, and 47.1 at a constant quantizer at 0, 3, and 6 steps finer, where the
/// driver's cleanup of the still screen reaches 46.2; at a constant rate, held to `REFINE_REACH`,
/// from 31.0 to 33.3 at 8 Mbit/s and from 19.0 to 21.1 at 2, the video unchanged.
const REFINE_FINER: u32 = 3;

/// The most quantizer steps off the frame's own a refined region is coded at
/// (`VaapiEncoder::refines`): on Intel's iHD (N97, H.264 on the low-power entry point, 3 Mbit/s
/// CBR at 1080p60) a region 10 finer came out at a quantizer 7 under the rest with the rate held,
/// 25 finer at 20 under it at 54 Mbit/s, the rate control lost; 5 finer, a delta the driver
/// codes natively, moved nothing.
const REFINE_REACH: u32 = 10;

/// One VA-API encoder session, hardware, for one capture.
pub struct VaapiEncoder {
    codec: Codec,
    input: Input,
    device: Arc<Device>,
    config: VAConfigID,
    context: VAContextID,
    vpp_config: VAConfigID,
    vpp_context: VAContextID,
    /// The color standard the video processor is told to convert from and to.
    vpp_standards: (VAProcColorStandardType, VAProcColorStandardType),
    negotiated: Negotiated,
    fourcc: u32,
    rt_format: u32,
    surface_width: u32,
    surface_height: u32,
    low_power: bool,
    /// The reconstruction surfaces, one more than the frames the decoder holds.
    recon: Vec<VASurfaceID>,
    /// The converted picture the encoder reads.
    converted: Vec<VASurfaceID>,
    host: Option<HostUpload>,
    /// The surface over each dmabuf a zero-copy session encoded, by fd, oldest first, at most
    /// `DMABUF_IMPORTS` (`dmabuf_surface`).
    imports: Vec<(i32, DmaBufIdentity, VASurfaceID)>,
    coded: VABufferID,
    arm: Arm,
    references: Option<References>,
    /// The frames encoded and the count at the last key frame, which a session that tracks no
    /// references keeps itself; a tracked one keeps both in its references.
    frame_count: u64,
    key_count: u64,
    /// The surface each held frame was reconstructed into, by timestamp.
    surfaces_of: HashMap<u64, VASurfaceID>,
    last_reference: Reference,
    rate: RateSettings,
    qp: u32,
    /// The quantizer the next frame is held at (`hold_quantizer`).
    held: Option<u32>,
    /// The blocks the next frame of the rate control's refines (`refine_blocks`), and the
    /// regions of interest of the frame being issued (`roi_parameter`).
    refinement: Option<crate::ledger::Refinement>,
    roi: Vec<VAEncROI>,
    /// Whether the driver caps each coded frame at the size a constant-rate session names.
    frame_cap: bool,
    /// The bytes of the last coded frame, padding aside.
    last_bytes: Option<usize>,
    /// The rate the next frame is coded at, a cleanup frame's (`cap`), where it differs from the session's.
    cleanup_bps: Option<u32>,
    /// The rate the driver was last told, by a sequence or a cleanup frame.
    sent_bps: u32,
    /// The quality index of the average quantizer the driver reported for it.
    last_quality: Option<u32>,
    /// Whether the next frame is measured, the last measurement (`last_psnr`), and whether
    /// the driver's surfaces have been readable so far (`measures`).
    measure: bool,
    last_psnr: Option<f32>,
    measures: bool,
    /// The quality level asked of the driver: the highest it takes, its fastest, where libva's
    /// level 1 is the best quality and the slowest; None where it reports none.
    quality_level: Option<u32>,
    /// Whether the next frame opens a sequence: a key frame carrying the sequence parameters
    /// and the rate control.
    sequence_start: bool,
    omit_headers: bool,
    fresh: bool,
    /// The reorder bound the H.264 stream is held to. A driver may write its own sequence
    /// parameter set in place of the session's (radeonsi's VCE did before Mesa 25.0, declaring no
    /// restriction or a depth of three), and the session submits every picture in the order it
    /// is shown, so the bound is written whatever the driver's picture order count.
    reorder: NoReorder,
}

unsafe impl Send for VaapiEncoder {}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        let api = self.device.api;
        let display = self.device.display;
        unsafe {
            if self.coded != VA_INVALID_ID {
                (api.vaDestroyBuffer)(display, self.coded);
            }
            if let Some(host) = &self.host
                && let Some(image) = host.image
            {
                (api.vaDestroyImage)(display, image.image_id);
            }
            if self.vpp_context != VA_INVALID_ID {
                (api.vaDestroyContext)(display, self.vpp_context);
            }
            if self.context != VA_INVALID_ID {
                (api.vaDestroyContext)(display, self.context);
            }
            let mut surfaces = self.recon.clone();
            surfaces.extend(&self.converted);
            surfaces.extend(self.host.iter().map(|h| h.surface));
            surfaces.extend(self.imports.iter().map(|i| i.2));
            self.device.destroy_surfaces(&mut surfaces);
            if self.vpp_config != VA_INVALID_ID {
                (api.vaDestroyConfig)(display, self.vpp_config);
            }
            if self.config != VA_INVALID_ID {
                (api.vaDestroyConfig)(display, self.config);
            }
        }
    }
}

impl VaapiEncoder {
    /// Stand up a session for `codec` on the render node the settings select, fed through
    /// `input`, with the libva of this process.
    pub fn new(settings: &CaptureSettings, codec: Codec, input: Input) -> Result<Self, String> {
        if !codec.is_video() {
            return Err("JPEG has no VA-API encoder".into());
        }
        Self::on_device(
            Arc::new(Device::open(libva()?, settings.encode_node_index)?),
            settings,
            codec,
            input,
        )
    }

    /// `new` on an open device.
    pub(crate) fn on_device(
        device: Arc<Device>,
        settings: &CaptureSettings,
        codec: Codec,
        input: Input,
    ) -> Result<Self, String> {
        let fullcolor = settings.video_fullcolor && codec.fullcolor();
        let depths: &[u32] = if settings.video_bit_depth >= 10 && codec.high_bit_depth() {
            &[10, 8]
        } else {
            &[8]
        };
        let listed = device.profiles()?;
        let mut last = None;
        for &bit_depth in depths {
            let served = profile_ladder(codec, fullcolor, bit_depth)
                .into_iter()
                .filter(|p| listed.contains(p))
                .map(|p| (p, device.encode_entrypoints(p)))
                .find(|(_, entrypoints)| !entrypoints.is_empty());
            let Some((profile, mut entrypoints)) = served else {
                last.get_or_insert_with(|| {
                    format!(
                        "this VA-API driver encodes no {} {} at {bit_depth} bits",
                        codec.display(),
                        super::chroma_name(fullcolor)
                    )
                });
                continue;
            };
            if codec == Codec::H264 && device.whole_picture_vdenc {
                entrypoints.sort_by_key(|&e| e == VAEntrypointEncSliceLP);
                crate::log::debug!(
                    "[vaapi] This device's low-power H.264 encoder codes one slice a picture: {}",
                    if entrypoints[0] == VAEntrypointEncSliceLP {
                        "the driver offers no full entry point, so the session codes one slice."
                    } else {
                        "H.264 tries the full entry point first."
                    }
                );
            }
            for entrypoint in entrypoints {
                for &fourcc in surface_fourccs(fullcolor, bit_depth) {
                    match Self::open(
                        &device, settings, codec, input, profile, entrypoint, fourcc, fullcolor,
                        bit_depth,
                    ) {
                        Ok(session) => return Ok(session),
                        Err(e) => last = Some(e),
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| "no VA-API surface format to try".into()))
    }

    /// One bring-up on `fourcc` surfaces: the encode configuration and its attributes, the
    /// surfaces, the encode and processing contexts, and the codec arm.
    #[allow(clippy::too_many_arguments)]
    fn open(
        device: &Arc<Device>,
        settings: &CaptureSettings,
        codec: Codec,
        input: Input,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        fourcc: u32,
        fullcolor: bool,
        bit_depth: u32,
    ) -> Result<Self, String> {
        let api = device.api;
        let rate = RateSettings::new(settings);
        let rc_mode = if rate.cbr { VA_RC_CBR } else { VA_RC_CQP };
        let rt_format = rt_format(fullcolor, bit_depth);
        let width = settings.width.max(1) as u32;
        let height = settings.height.max(1) as u32;
        let fps = rate.fps;
        let bits_per_second = if rate.cbr {
            rate.bps().min(u32::MAX as u64) as u32
        } else {
            0
        };

        let mut attribs = Vec::new();
        if let Some(formats) = device.attribute(profile, entrypoint, VAConfigAttribRTFormat) {
            if formats & rt_format == 0 {
                return Err(format!(
                    "the {} entry point renders no {} {bit_depth}-bit surfaces",
                    fourcc_name(fourcc),
                    super::chroma_name(fullcolor)
                ));
            }
            attribs.push(VAConfigAttrib {
                type_: VAConfigAttribRTFormat,
                value: rt_format,
            });
        }
        match device.attribute(profile, entrypoint, VAConfigAttribRateControl) {
            Some(modes) if modes & rc_mode != 0 => attribs.push(VAConfigAttrib {
                type_: VAConfigAttribRateControl,
                value: rc_mode,
            }),
            Some(_) => {
                return Err(format!(
                    "this VA-API driver has no {} rate control for {}",
                    if rate.cbr {
                        "constant-rate"
                    } else {
                        "constant-quantizer"
                    },
                    codec.display()
                ));
            }
            None if rate.cbr => {
                return Err("this VA-API driver reports no rate control modes".into());
            }
            None => {}
        }
        let ref_l0 = device
            .attribute(profile, entrypoint, VAConfigAttribEncMaxRefFrames)
            .map_or(0, |v| v & 0xffff);
        if ref_l0 < 1 {
            return Err("this VA-API driver takes no reference frames".into());
        }
        let level_bitrate = bits_per_second as u64;
        let dpb_level = match codec {
            Codec::H264 => super::codec::h264_level(width, height, fps.ceil(), level_bitrate),
            Codec::H265 => super::codec::h265_level(width, height, fps.ceil(), level_bitrate, true),
            _ => 0,
        };
        let dpb = match codec {
            Codec::H264 => super::codec::h264_dpb_frames(dpb_level, width, height)
                .min(super::reference_frames(settings)),
            Codec::H265 => super::codec::h265_dpb_frames(dpb_level, width, height)
                .min(super::reference_frames(settings)),
            _ => REFERENCE_FRAMES,
        };
        let unit_qp_deltas = codec == Codec::H265
            && device.hardware() == Hardware::VaapiIntel
            && entrypoint == VAEntrypointEncSlice;
        let mut arm = match codec {
            Codec::H264 => Arm::H264(Box::new(h264::Arm::new(profile))),
            Codec::H265 => Arm::H265(h265::Arm::new(
                profile,
                device.attribute(profile, entrypoint, VAConfigAttribEncHEVCFeatures),
                device.attribute(profile, entrypoint, VAConfigAttribEncHEVCBlockSizes),
                device.attribute(profile, entrypoint, VAConfigAttribPredictionDirection),
                unit_qp_deltas,
            )),
            Codec::Vp8 => Arm::Vp8(vp8::Arm::new()),
            Codec::Vp9 => Arm::Vp9(vp9::Arm::new()),
            Codec::Av1 => Arm::Av1(av1::Arm::new(
                device.attribute(profile, entrypoint, VAConfigAttribEncAV1),
                device.attribute(profile, entrypoint, VAConfigAttribEncAV1Ext1),
                device
                    .attribute(profile, entrypoint, VAConfigAttribEncAV1Ext2)
                    .ok_or("this VA-API driver reports no AV1 encoder attributes")?,
            )),
            Codec::Jpeg => unreachable!(),
        };
        let packed = device
            .attribute(profile, entrypoint, VAConfigAttribEncPackedHeaders)
            .map_or(0, |v| v & arm.wanted_packed());
        if packed != 0 {
            attribs.push(VAConfigAttrib {
                type_: VAConfigAttribEncPackedHeaders,
                value: packed,
            });
        }
        let dpb = if arm.tracks_references(packed) {
            dpb
        } else {
            1
        };
        let quality_range = device.attribute(profile, entrypoint, VAConfigAttribEncQualityRange);
        let slice_caps = (
            device.attribute(profile, entrypoint, VAConfigAttribEncMaxSlices),
            device.attribute(profile, entrypoint, VAConfigAttribEncSliceStructure),
        );

        let mut config = VA_INVALID_ID;
        device.check(
            unsafe {
                (api.vaCreateConfig)(
                    device.display,
                    profile,
                    entrypoint,
                    attribs.as_mut_ptr(),
                    attribs.len() as c_int,
                    &mut config,
                )
            },
            &format!(
                "no VA-API encode configuration for {} on {} surfaces",
                codec.display(),
                fourcc_name(fourcc)
            ),
        )?;
        let mut me = Self {
            codec,
            input,
            device: Arc::clone(device),
            config,
            context: VA_INVALID_ID,
            vpp_config: VA_INVALID_ID,
            vpp_context: VA_INVALID_ID,
            vpp_standards: (VAProcColorStandardNone, VAProcColorStandardNone),
            negotiated: Negotiated {
                profile,
                entrypoint,
                packed,
                rc_mode,
                width,
                height,
                fps,
                bits_per_second,
                dpb,
                dpb_level,
                fullcolor,
                bit_depth,
                min_qp: codec.hardware_quantizer_bound(device.hardware(), rate.min_qp),
                max_qp: codec.hardware_quantizer_bound(device.hardware(), rate.max_qp),
                qp_floor: if codec == Codec::H265
                    && device.hardware() == Hardware::VaapiIntel
                    && entrypoint == VAEntrypointEncSliceLP
                {
                    h265::INTEL_LOW_POWER_MIN_QP
                } else {
                    1
                },
                unit_qp_deltas,
                h264_driver_level: device.h264_driver_level(),
                roi_regions: device
                    .attribute(profile, entrypoint, VAConfigAttribEncROI)
                    .map_or(0, |v| {
                        let qp_deltas_under_rate_control = v & (1 << 9) != 0;
                        if rate.cbr && !qp_deltas_under_rate_control {
                            0
                        } else {
                            v & 0xff
                        }
                    }),
            },
            fourcc,
            rt_format,
            surface_width: 0,
            surface_height: 0,
            low_power: entrypoint == VAEntrypointEncSliceLP,
            recon: Vec::new(),
            converted: Vec::new(),
            host: None,
            coded: VA_INVALID_ID,
            arm: Arm::Vp8(vp8::Arm::new()),
            references: None,
            frame_count: 0,
            key_count: 0,
            surfaces_of: HashMap::new(),
            imports: Vec::new(),
            last_reference: Reference::Untracked,
            rate,
            qp: codec.hardware_quantizer(device.hardware(), settings.video_crf),
            held: None,
            refinement: None,
            roi: Vec::new(),
            frame_cap: false,
            last_bytes: None,
            cleanup_bps: None,
            sent_bps: 0,
            last_quality: None,
            measure: false,
            last_psnr: None,
            measures: true,
            quality_level: None,
            sequence_start: true,
            omit_headers: settings.omit_stripe_headers,
            fresh: true,
            reorder: NoReorder::new("The VA-API driver", true),
        };
        let (align_w, align_h) = {
            let (aw, ah) = arm.alignment();
            let (dw, dh) = device.surface_alignment(config);
            (aw.max(dw), ah.max(dh))
        };
        me.surface_width = width.div_ceil(align_w) * align_w;
        me.surface_height = height.div_ceil(align_h) * align_h;
        let rendered = device.surface_fourccs(config);
        if !rendered.is_empty() && !rendered.contains(&fourcc) {
            return Err(format!(
                "the {} encoder takes no {} surfaces",
                codec.display(),
                fourcc_name(fourcc)
            ));
        }
        let wanted_slices = if matches!(arm, Arm::H264(_)) && device.one_slice_h264(entrypoint) {
            1
        } else {
            SLICES
        };
        let slices = match (&arm, slice_caps) {
            (Arm::H264(_) | Arm::H265(_), (Some(max), Some(structure))) => Some(slice_layout(
                structure,
                max,
                me.surface_height.div_ceil(arm_block(&arm)),
                wanted_slices,
            )?),
            (Arm::H264(_) | Arm::H265(_), _) => {
                Some((1, me.surface_height.div_ceil(arm_block(&arm))))
            }
            _ => None,
        };
        match &mut arm {
            Arm::H264(a) => a.configure(
                &me.negotiated,
                me.surface_width,
                me.surface_height,
                slices.unwrap(),
            ),
            Arm::H265(a) => a.configure(
                &me.negotiated,
                me.surface_width,
                me.surface_height,
                slices.unwrap(),
            ),
            Arm::Vp8(a) => a.configure(&me.negotiated),
            Arm::Vp9(a) => a.configure(&me.negotiated),
            Arm::Av1(a) => a.configure(&me.negotiated, me.surface_width, me.surface_height)?,
        }
        me.arm = arm;
        me.references = me.arm.tracks_references(packed).then(|| match me.arm {
            Arm::Vp8(_) => References::Slots(ReferenceSlots::new()),
            Arm::H264(_) if me.device.keeps_anchors() && dpb >= 3 => {
                let mut w = ReferenceWindow::with_anchors(dpb, 1);
                if settings.acknowledge_references {
                    w.set_acknowledged();
                }
                References::Window(w)
            }
            Arm::H265(_) if me.device.keeps_anchors() && dpb as usize >= ANCHORS + 2 => {
                let mut w = ReferenceWindow::with_anchors(dpb, ANCHORS);
                if settings.acknowledge_references {
                    w.set_acknowledged();
                }
                References::Window(w)
            }
            _ => References::Window(ReferenceWindow::new(dpb)),
        });
        if let Some(References::Window(w)) = &mut me.references
            && let Arm::H264(a) = &me.arm
        {
            w.set_frame_num_range(a.frame_num_range());
        }
        me.quality_level = quality_range;
        me.frame_cap = device
            .attribute(profile, entrypoint, VAConfigAttribMaxFrameSize)
            .is_some_and(|v| v & 1 != 0);

        let recon_count = match me.arm {
            Arm::Vp8(_) => 4,
            _ => dpb as usize + 1,
        };
        me.recon = device.create_surfaces(
            rt_format,
            fourcc,
            me.surface_width,
            me.surface_height,
            recon_count,
        )?;
        me.converted =
            device.create_surfaces(rt_format, fourcc, me.surface_width, me.surface_height, 1)?;
        let mut render_targets = me.recon.clone();
        render_targets.extend(&me.converted);
        device.check(
            unsafe {
                (api.vaCreateContext)(
                    device.display,
                    config,
                    me.surface_width as c_int,
                    me.surface_height as c_int,
                    VA_PROGRESSIVE as c_int,
                    render_targets.as_mut_ptr(),
                    render_targets.len() as c_int,
                    &mut me.context,
                )
            },
            "no VA-API encode context",
        )?;
        me.coded = device.create_output_buffer(
            me.context,
            VAEncCodedBufferType,
            coded_buffer_size(me.surface_width, me.surface_height),
        )?;
        me.open_vpp(device, input)?;
        Ok(me)
    }
}

/// The block a codec's slices are counted in: a macroblock, or the coding tree unit.
fn arm_block(arm: &Arm) -> u32 {
    match arm {
        Arm::H265(a) => a.ctu_size(),
        _ => 16,
    }
}

/// How many slices a picture of `rows` block rows is cut into and how many rows each
/// takes, for `wanted` slices under the driver's slice structure and at most `max_slices`:
/// arbitrary rows as asked, equal rows with a shorter last slice where the driver wants them
/// equal (iHD's H.264), a power of two of rows where that is all it takes, one row each
/// where it takes only that. A slice a row is 68 slices at 1080p, none predicting from the
/// row above and each restarting the entropy coder: on Intel Arc a scroll of text at one
/// quantizer cost 12.4 kB a frame so cut and 8.6 kB in four slices, at the same PSNR.
fn slice_layout(
    structure: u32,
    max_slices: u32,
    rows: u32,
    wanted: u32,
) -> Result<(u32, u32), String> {
    let max_slices = max_slices.max(1);
    let wanted = wanted.min(rows).min(max_slices).max(1);
    let (count, size) = if structure
        & (VA_ENC_SLICE_STRUCTURE_ARBITRARY_ROWS | VA_ENC_SLICE_STRUCTURE_ARBITRARY_MACROBLOCKS)
        != 0
    {
        (wanted, rows / wanted)
    } else if structure & VA_ENC_SLICE_STRUCTURE_POWER_OF_TWO_ROWS != 0 {
        let mut k = 1;
        while (wanted > 1 && 2 * k * (wanted - 1) + 1 < rows) || rows.div_ceil(k) > max_slices {
            k *= 2;
        }
        (rows.div_ceil(k), k)
    } else if structure & VA_ENC_SLICE_STRUCTURE_EQUAL_MULTI_ROWS != 0 {
        let size = rows.div_ceil(wanted);
        (rows.div_ceil(size), size)
    } else if structure & VA_ENC_SLICE_STRUCTURE_EQUAL_ROWS != 0 {
        (rows, 1)
    } else {
        return Err(format!(
            "this VA-API driver supports no usable slice structure ({structure:#x})"
        ));
    };
    if count > max_slices {
        return Err(format!(
            "this VA-API driver encodes at most {max_slices} slices, not {count}"
        ));
    }
    Ok((count, size))
}

impl VaapiEncoder {
    /// The video processor: a configuration and a context on the converted surface, and the
    /// color standards it is told to convert between, chosen the way libavfilter's `scale_vaapi`
    /// chooses them: explicit color properties where the driver takes them, else the standard
    /// nearest to the matrix and range asked for.
    fn open_vpp(&mut self, device: &Device, input: Input) -> Result<(), String> {
        let api = device.api;
        device.check(
            unsafe {
                (api.vaCreateConfig)(
                    device.display,
                    VAProfileNone,
                    VAEntrypointVideoProc,
                    ptr::null_mut(),
                    0,
                    &mut self.vpp_config,
                )
            },
            "no VA-API video processing configuration",
        )?;
        let rendered = device.surface_fourccs(self.vpp_config);
        if !rendered.is_empty() && !rendered.contains(&self.fourcc) {
            return Err(format!(
                "this VA-API driver's video processor renders no {} surfaces",
                fourcc_name(self.fourcc)
            ));
        }
        device.check(
            unsafe {
                (api.vaCreateContext)(
                    device.display,
                    self.vpp_config,
                    self.surface_width as c_int,
                    self.surface_height as c_int,
                    VA_PROGRESSIVE as c_int,
                    self.converted.as_mut_ptr(),
                    self.converted.len() as c_int,
                    &mut self.vpp_context,
                )
            },
            "no VA-API video processing context",
        )?;
        let mut caps: VAProcPipelineCaps = unsafe { std::mem::zeroed() };
        device.check(
            unsafe {
                (api.vaQueryVideoProcPipelineCaps)(
                    device.display,
                    self.vpp_context,
                    ptr::null_mut(),
                    0,
                    &mut caps,
                )
            },
            "vaQueryVideoProcPipelineCaps",
        )?;
        let standards =
            |list: *mut VAProcColorStandardType, count: u32| -> Vec<VAProcColorStandardType> {
                if list.is_null() {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(list, count as usize) }.to_vec()
                }
            };
        let input_standards = standards(caps.input_color_standards, caps.num_input_color_standards);
        let output_standards =
            standards(caps.output_color_standards, caps.num_output_color_standards);
        self.vpp_standards = (
            color_standard(&input_standards, ColorDescription::SRGB_SOURCE),
            color_standard(&output_standards, self.declared_color()),
        );
        if let Input::Host { rgba } = input {
            let fourcc = if rgba { VA_FOURCC_RGBA } else { VA_FOURCC_BGRA };
            let surface = device.create_surfaces(
                VA_RT_FORMAT_RGB32,
                fourcc,
                self.negotiated.width,
                self.negotiated.height,
                1,
            )?[0];
            let mut derived: VAImage = unsafe { std::mem::zeroed() };
            let derive = unsafe { (api.vaDeriveImage)(device.display, surface, &mut derived) }
                == VA_STATUS_SUCCESS as VAStatus
                && {
                    let same = derived.format.fourcc == fourcc;
                    unsafe { (api.vaDestroyImage)(device.display, derived.image_id) };
                    same
                };
            let host = self.host.insert(HostUpload {
                surface,
                fourcc,
                derive,
                image: None,
            });
            if !derive {
                let mut format = image_format(device, fourcc).ok_or_else(|| {
                    format!(
                        "this VA-API driver has no {} image format",
                        fourcc_name(fourcc)
                    )
                })?;
                let mut image: VAImage = unsafe { std::mem::zeroed() };
                device.check(
                    unsafe {
                        (api.vaCreateImage)(
                            device.display,
                            &mut format,
                            self.negotiated.width as c_int,
                            self.negotiated.height as c_int,
                            &mut image,
                        )
                    },
                    "vaCreateImage",
                )?;
                host.image = Some(image);
            }
        }
        Ok(())
    }

    /// The color the session declares: BT.709, whose primaries and transfer the sRGB desktop
    /// source already carries, at limited range; VP8 is held to BT.601, the only matrix its
    /// keyframe header's one color-space bit can name.
    fn declared_color(&self) -> ColorDescription {
        ColorDescription {
            primaries: 1,
            transfer: 1,
            matrix: if self.codec == Codec::Vp8 { 6 } else { 1 },
            full_range: false,
            rgb: false,
        }
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Whether this session negotiated 4:4:4 chroma.
    pub fn is_fullcolor(&self) -> bool {
        self.negotiated.fullcolor
    }

    /// The bits per sample the session opened at.
    pub fn bit_depth(&self) -> u32 {
        self.negotiated.bit_depth
    }

    /// The name of the surface format frames reach the codec in, for the session log.
    pub fn surface_format_name(&self) -> String {
        fourcc_name(self.fourcc)
    }

    /// A hardware session converts at limited range.
    pub fn is_full_range(&self) -> bool {
        false
    }

    /// The driver behind the session, as libva names it.
    pub fn vendor(&self) -> &str {
        &self.device.vendor
    }

    /// Whether the codec runs on the low-power entry point.
    pub fn low_power(&self) -> bool {
        self.low_power
    }

    /// The frame the last encoded frame predicted from.
    pub fn last_reference(&self) -> Reference {
        self.last_reference
    }

    /// Apply what the consumers say of a frame: a loss as `invalidate_reference`, a frame every
    /// one of them holds or was sent (`ReferenceWindow::acknowledge`, `ReferenceSlots::acknowledge`).
    pub fn take_report(&mut self, report: ReferenceReport) -> bool {
        let (frame_id, held) = match report {
            ReferenceReport::Lost(frame_id) | ReferenceReport::Withheld(frame_id) => {
                return self.invalidate_reference(frame_id);
            }
            ReferenceReport::Held(frame_id) => (frame_id, true),
            ReferenceReport::Sent(frame_id) => (frame_id, false),
        };
        match &mut self.references {
            Some(References::Window(w)) => {
                w.acknowledge(frame_id, held);
            }
            Some(References::Slots(s)) => {
                s.acknowledge(frame_id, held);
            }
            None => {}
        }
        true
    }

    /// Leave frame `frame_id` and every frame after it out of the predictions. False where the
    /// session tracks none, and the caller codes a key frame instead.
    pub fn invalidate_reference(&mut self, frame_id: u16) -> bool {
        match &mut self.references {
            Some(References::Window(w)) => {
                w.invalidate(frame_id);
                true
            }
            Some(References::Slots(s)) => {
                s.invalidate(frame_id);
                true
            }
            None => false,
        }
    }

    /// Take a rate or frame-rate change: the next frame opens a sequence with the new rate
    /// control, as a key frame.
    pub fn reconfigure_rate(&mut self, settings: &CaptureSettings) -> Result<(), String> {
        let Some(rate) = self.rate.changed(settings) else {
            return Ok(());
        };
        self.rate = rate;
        self.negotiated.fps = rate.fps;
        self.negotiated.bits_per_second = if rate.cbr {
            rate.bps().min(u32::MAX as u64) as u32
        } else {
            0
        };
        self.negotiated.min_qp = self
            .codec
            .hardware_quantizer_bound(self.device.hardware(), rate.min_qp);
        self.negotiated.max_qp = self
            .codec
            .hardware_quantizer_bound(self.device.hardware(), rate.max_qp);
        match &mut self.arm {
            Arm::H264(a) => a.configure(
                &self.negotiated,
                self.surface_width,
                self.surface_height,
                a.slices(),
            ),
            Arm::H265(a) => a.configure(
                &self.negotiated,
                self.surface_width,
                self.surface_height,
                a.slices(),
            ),
            Arm::Vp8(a) => a.configure(&self.negotiated),
            Arm::Vp9(a) => a.configure(&self.negotiated),
            Arm::Av1(a) => {
                a.configure(&self.negotiated, self.surface_width, self.surface_height)?
            }
        }
        self.sequence_start = true;
        Ok(())
    }

    /// Whether the session runs a constant rate.
    pub fn is_cbr(&self) -> bool {
        self.rate.cbr
    }

    /// The bytes of the last frame coded, the driver's padding aside: what tells a
    /// constant-rate cleanup the rate control has nothing left to refine.
    pub fn last_size(&self) -> Option<usize> {
        self.last_bytes
    }

    /// The quality index the rate control last coded a frame at: for H.264 and HEVC where the
    /// driver reports a picture's average quantizer with its coded buffer (iHD does), for AV1
    /// and VP9 the `base_q_idx` it wrote into the frame header. A picture that is mostly skipped
    /// blocks reports near the quantizer its slices open at, so the figure is trusted for
    /// having reached a quality, never for the lack of it.
    pub fn last_quality(&self) -> Option<u32> {
        self.last_quality
    }

    /// Whether `measure` yields a measurement: until a surface turns out unreadable, as the
    /// reconstruction of iHD's packed 4:4:4 does, kept at another geometry than its source.
    pub fn measures(&self) -> bool {
        self.measures
    }

    /// Measure the next frame's reconstruction against its source (`last_psnr`).
    pub fn measure(&mut self) {
        self.measure = true;
    }

    /// The luma PSNR, in dB, of the last frame measured: its reconstruction against the
    /// converted picture it was coded from, both read back on every `PSNR_ROW_STEP`th row.
    /// What tells a constant-rate cleanup whether the driver's rate control is still refining
    /// a still screen, whatever the driver says of its quantizer; None where a surface cannot
    /// be read as the picture's rows.
    pub fn last_psnr(&self) -> Option<f32> {
        self.last_psnr
    }

    /// The sampled luma rows of `surface`, as 8-bit samples (the high eight bits of a 10-bit
    /// one), with the format and the rows the driver keeps it in.
    fn luma_rows(&self, surface: VASurfaceID) -> Option<(u32, u16, Vec<u8>)> {
        let api = self.device.api;
        let display = self.device.display;
        let mut image: VAImage = unsafe { std::mem::zeroed() };
        if unsafe { (api.vaDeriveImage)(display, surface, &mut image) }
            != VA_STATUS_SUCCESS as VAStatus
        {
            return None;
        }
        let fourcc = image.format.fourcc;
        let (bytes, luma) = match fourcc {
            VA_FOURCC_NV12 | VA_FOURCC_444P => (1, 0),
            VA_FOURCC_P010 => (2, 1),
            VA_FOURCC_XYUV | VA_FOURCC_Y410 => (4, 2),
            _ => (0, 0),
        };
        let mut address: *mut c_void = ptr::null_mut();
        let mut rows = None;
        if bytes > 0
            && image.height as u32 >= self.negotiated.height
            && unsafe { (api.vaMapBuffer)(display, image.buf, &mut address) }
                == VA_STATUS_SUCCESS as VAStatus
        {
            let width = (self.negotiated.width as usize).min(image.width as usize);
            let height = self.negotiated.height as usize;
            let (offset, pitch) = (image.offsets[0] as usize, image.pitches[0] as usize);
            let mut out = Vec::with_capacity(width * height.div_ceil(PSNR_ROW_STEP));
            for y in (0..height).step_by(PSNR_ROW_STEP) {
                let row = unsafe {
                    std::slice::from_raw_parts(
                        (address as *const u8).add(offset + y * pitch),
                        width * bytes,
                    )
                };
                if fourcc == VA_FOURCC_Y410 {
                    out.extend(
                        row.as_chunks::<4>()
                            .0
                            .iter()
                            .map(|p| ((u32::from_le_bytes(*p) >> 12) & 0xff) as u8),
                    );
                } else {
                    out.extend(row.iter().skip(luma).step_by(bytes));
                }
            }
            unsafe { (api.vaUnmapBuffer)(display, image.buf) };
            rows = Some((fourcc, image.height, out));
        }
        unsafe { (api.vaDestroyImage)(display, image.image_id) };
        rows
    }

    /// The luma PSNR of `recon` against the converted picture, over the sampled rows; None
    /// where the driver keeps the reconstruction in another layout than the source, which
    /// would read as noise: iHD's of 10-bit HEVC and VP9 reports `P016` for `p010`, its packed
    /// 4:4:4 one fewer rows, and either measured under `UNREADABLE_DB`.
    fn picture_psnr(&self, recon: VASurfaceID) -> Option<f32> {
        let (format, rows, source) = self.luma_rows(self.converted[0])?;
        let (coded_format, coded_rows, coded) = self.luma_rows(recon)?;
        if (format, rows) != (coded_format, coded_rows) || source.is_empty() {
            return None;
        }
        let sse: u64 = source
            .iter()
            .zip(&coded)
            .map(|(&a, &b)| {
                let d = a as i64 - b as i64;
                (d * d) as u64
            })
            .sum();
        let mse = (sse as f64 / source.len() as f64).max(1e-3);
        Some((10.0 * (255.0 * 255.0 / mse).log10()) as f32).filter(|&db| db >= UNREADABLE_DB)
    }

    /// How this driver's rate control is bounded (`ConstantRate`).
    fn constant_rate(&self) -> ConstantRate {
        if !self.device.starves_in_a_small_buffer() {
            return ConstantRate::Buffer;
        }
        match self.arm {
            Arm::H264(_) | Arm::H265(_) => ConstantRate::Unbounded,
            Arm::Av1(_) => ConstantRate::Seconds(IHD_AV1_BUFFER_S),
            Arm::Vp8(_) | Arm::Vp9(_) => ConstantRate::Buffer,
        }
    }

    /// Whether a constant-rate cleanup ends on a key frame held at the paint-over quantizer
    /// (`FrameEncoder::ends_cleanup_on_key`): where the driver says no quantizer of its H.264 or
    /// HEVC frames (radeonsi) and the session's frame budget is under
    /// `REFINE_BITS_PER_PIXEL` (`refines`).
    pub fn ends_cleanup_on_key(&self) -> bool {
        self.rate.cbr
            && matches!(self.arm, Arm::H264(_) | Arm::H265(_))
            && self.last_bytes.is_some()
            && self.last_quality.is_none()
            && !refines(
                self.negotiated.bits_per_second,
                self.negotiated.fps.fps(),
                self.negotiated.width,
                self.negotiated.height,
            )
    }

    /// Whether a still screen's cleanup frame is coded at the rate its cap runs at (`cap`): at
    /// a constant rate, where the driver takes a new target with a picture.
    pub fn follows_cap(&self) -> bool {
        self.rate.cbr
    }

    /// Code the next frame, a still screen's cleanup, at the rate that spends `bytes` in
    /// `pipeline::CLEANUP_FRAME_S`, and its buffer as long as the session's
    /// (`rate_control_at`); the frame after goes back to the session's rate. The session's own
    /// rate holds a 1080p page at 60 fps 0.5 Mbit/s to frames of 1 kB, which refine nothing
    /// (18 dB), where a cleanup at a 2 Mbit/s path's rate reaches 52 dB.
    pub fn cap(&mut self, bytes: usize) {
        if self.rate.cbr {
            let bps =
                (bytes as f64 * 8.0 / crate::pipeline::CLEANUP_FRAME_S).min(u32::MAX as f64) as u32;
            self.cleanup_bps = Some(bps.max(self.negotiated.bits_per_second));
        }
    }

    /// Encode the next frame at the quantizer the quality index `crf` selects, and leave the
    /// session's own quantizer for the frame after: the cleanup of a still screen, at a constant
    /// quantizer. A constant-rate session codes the frame under its rate control: radeonsi drops
    /// per-frame quantizer bounds (a key frame held by them came out starved in the session's
    /// small buffer, 26 dB), and a key frame given a budget of its own through a restarted rate
    /// control came out coarser than the picture the driver's rate control refines a still
    /// screen to by itself (51 against 61 dB at 8 Mbit/s), so `FrameEncoder::holds_quantizer`
    /// says no there. A key frame held there, the cleanup of a session whose rate control
    /// cannot refine (`ends_cleanup_on_key`), is coded with the budget `held_key_bits` gives it,
    /// sent with the picture.
    /// How the session refines blocks of a frame of its rate control's (`refine_blocks`): an
    /// H.264 session whose driver takes regions of interest with a quantizer delta, one region a
    /// run of blocks along a row (`Negotiated::roi_regions`), each at most `REFINE_REACH` steps
    /// off the frame's quality. HEVC takes none: on Intel's iHD (N97) refined regions took 1080p
    /// text beside a video from 41.4 dB to 25.4 at 8 Mbit/s and the video from 34.7 to 30.3, and
    /// from 47.6 to 44.5 at a constant quantizer, where the driver's HEVC path carries the
    /// regions as a delta map it lays out wrong.
    pub fn refines(&self) -> Option<crate::ledger::Refiner> {
        (self.codec == Codec::H264 && self.negotiated.roi_regions > 0).then_some(
            crate::ledger::Refiner {
                finer: REFINE_FINER,
                reach: REFINE_REACH,
                runs: self.negotiated.roi_regions as usize,
            },
        )
    }

    /// Code the blocks `refinement` names in the next frame of the rate control's at its target
    /// quality, as regions of interest (`roi_parameter`).
    pub fn refine_blocks(&mut self, refinement: &crate::ledger::Refinement) {
        self.refinement = self.refines().map(|_| refinement.clone());
    }

    /// The regions of interest of `refinement` into `out`: a rectangle a run of its blocks
    /// along a row, each at the quantizer delta from the rate control's last to the target,
    /// within `REFINE_REACH`. The driver reads the rectangles through the parameter's pointer
    /// when the picture is issued, so they live in the session until then.
    fn roi_parameter(&mut self, refinement: &crate::ledger::Refinement, out: &mut Buffers) {
        use crate::ledger::BLOCK;
        let hw = |index: i32| self.codec.hardware_quantizer(self.device.hardware(), index) as i32;
        let delta =
            (hw(refinement.target) - hw(refinement.from)).clamp(-(REFINE_REACH as i32), 0) as i8;
        let plan = &refinement.plan;
        let (width, height) = (self.negotiated.width as i32, self.negotiated.height as i32);
        self.roi.clear();
        for row in 0..plan.rows {
            let mut col = 0;
            while col < plan.cols {
                if !plan.refines(col, row) {
                    col += 1;
                    continue;
                }
                let start = col;
                while col < plan.cols && plan.refines(col, row) {
                    col += 1;
                }
                let (x, y) = (start as i32 * BLOCK, row as i32 * BLOCK);
                self.roi.push(VAEncROI {
                    roi_rectangle: VARectangle {
                        x: x as i16,
                        y: y as i16,
                        width: ((col as i32 * BLOCK).min(width) - x) as u16,
                        height: (BLOCK.min(height - y)) as u16,
                    },
                    roi_value: delta,
                });
            }
        }
        self.roi.truncate(self.negotiated.roi_regions as usize);
        let mut parameter: VAEncMiscParameterBufferROI = unsafe { std::mem::zeroed() };
        parameter.num_roi = self.roi.len() as u32;
        parameter.max_delta_qp = 51;
        parameter.min_delta_qp = -51;
        parameter.roi = self.roi.as_mut_ptr();
        unsafe { parameter.roi_flags.bits.set_roi_value_is_qp_delta(1) };
        out.push_misc(VAEncMiscParameterTypeROI, &parameter);
    }

    pub fn hold_quantizer(&mut self, crf: u32) {
        self.held = Some(
            self.codec
                .hardware_quantizer(self.device.hardware(), crf as i32),
        );
    }

    /// The rate control of a sequence: the target, buffer, and frame rate a constant-rate
    /// session holds, with no filler data up to the target and each frame capped at the buffer
    /// where the driver takes a cap and codes under one (`ConstantRate`), and the frame rate
    /// and quality level of any.
    fn rate_control_at(&self, out: &mut Buffers, bps: u32) {
        if self.rate.cbr {
            // The session's buffer spans the same time at a cleanup frame's rate.
            let vbv = (self.rate.vbv() as u64 * bps as u64
                / self.negotiated.bits_per_second.max(1) as u64)
                .min(u32::MAX as u64) as u32;
            let mut rc: VAEncMiscParameterRateControl = unsafe { std::mem::zeroed() };
            rc.bits_per_second = bps;
            rc.target_percentage = 100;
            rc.window_size = (vbv as u64 * 1000 / bps.max(1) as u64) as u32;
            rc.initial_qp = 0;
            rc.min_qp = if matches!(self.arm, Arm::H264(_)) {
                self.negotiated.min_qp.max(h264::MIN_QP)
            } else {
                self.negotiated.min_qp
            };
            // Intel's full encoder takes 0 for no bound, under which a coding unit can name a
            // quantizer past 51; its low-power one reads 0 as 51 itself.
            rc.max_qp = if self.negotiated.unit_qp_deltas && self.negotiated.max_qp == 0 {
                51
            } else {
                self.negotiated.max_qp
            };
            rc.basic_unit_size = 0;
            rc.ICQ_quality_factor = 1;
            rc.quality_factor = 0;
            let bound = self.constant_rate();
            unsafe {
                rc.rc_flags.bits.set_mb_rate_control(2);
                rc.rc_flags.bits.set_disable_bit_stuffing(1);
            }
            out.push_misc(VAEncMiscParameterTypeRateControl, &rc);
            let buffer = match bound {
                ConstantRate::Buffer => Some(vbv),
                ConstantRate::Seconds(s) => Some((bps as f64 * s) as u32),
                ConstantRate::Unbounded => None,
            };
            if let Some(bits) = buffer {
                let hrd = VAEncMiscParameterHRD {
                    initial_buffer_fullness: bits,
                    buffer_size: bits,
                    va_reserved: [0; 4],
                };
                out.push_misc(VAEncMiscParameterTypeHRD, &hrd);
            }
            if self.frame_cap && bound == ConstantRate::Buffer {
                let cap = VAEncMiscParameterBufferMaxFrameSize {
                    type_: VAEncMiscParameterTypeMaxFrameSize,
                    max_frame_size: vbv,
                    va_reserved: [0; 4],
                };
                out.push_misc(VAEncMiscParameterTypeMaxFrameSize, &cap);
            }
        }
        let fps = self.negotiated.fps.within(0xffff);
        let mut frame_rate: VAEncMiscParameterFrameRate = unsafe { std::mem::zeroed() };
        frame_rate.framerate = (fps.den << 16) | fps.num;
        out.push_misc(VAEncMiscParameterTypeFrameRate, &frame_rate);
        if let Some(level) = self.quality_level {
            let quality = VAEncMiscParameterBufferQualityLevel {
                quality_level: level,
                va_reserved: [0; 4],
            };
            out.push_misc(VAEncMiscParameterTypeQualityLevel, &quality);
        }
    }

    /// Encode one packed host frame (`stride` bytes per row, in the byte order the session
    /// was built for) at the quality index `crf`: uploaded straight from the caller's rows
    /// onto the host surface and converted on the GPU.
    pub fn encode_host(
        &mut self,
        pixels: &[u8],
        stride: usize,
        rgba: bool,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        let Input::Host { rgba: built } = self.input else {
            return Err("this session takes dmabufs".into());
        };
        if built != rgba {
            return Err("this session was built for the other byte order".into());
        }
        let (width, height) = (
            self.negotiated.width as usize,
            self.negotiated.height as usize,
        );
        check_host_frame(pixels, stride, width, height)?;
        let host = self.host.as_ref().ok_or("no host upload surface")?;
        let api = self.device.api;
        let display = self.device.display;
        unsafe {
            let mut image: VAImage = std::mem::zeroed();
            if host.derive {
                self.device.check(
                    (api.vaDeriveImage)(display, host.surface, &mut image),
                    "vaDeriveImage",
                )?;
            } else {
                image = host.image.unwrap();
            }
            let mut address: *mut c_void = ptr::null_mut();
            let mapped = self.device.check(
                (api.vaMapBuffer)(display, image.buf, &mut address),
                "vaMapBuffer",
            );
            if let Err(e) = mapped {
                if host.derive {
                    (api.vaDestroyImage)(display, image.image_id);
                }
                return Err(e);
            }
            let pitch = image.pitches[0] as usize;
            let dst = (address as *mut u8).add(image.offsets[0] as usize);
            for row in 0..height {
                ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(row * stride),
                    dst.add(row * pitch),
                    width * 4,
                );
            }
            (api.vaUnmapBuffer)(display, image.buf);
            if host.derive {
                (api.vaDestroyImage)(display, image.image_id);
            } else {
                self.device.check(
                    (api.vaPutImage)(
                        display,
                        host.surface,
                        image.image_id,
                        0,
                        0,
                        width as c_uint,
                        height as c_uint,
                        0,
                        0,
                        width as c_uint,
                        height as c_uint,
                    ),
                    "vaPutImage",
                )?;
            }
        }
        let surface = host.surface;
        self.encode_surface(surface, frame_number, crf, force_idr)
    }

    /// Encode one Wayland DRM-PRIME dmabuf: imported as a VA surface in place for the frame and
    /// converted on the GPU.
    pub fn encode_dmabuf(
        &mut self,
        dmabuf: &Dmabuf,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        if self.input != Input::Dmabuf {
            return Err("this session takes host frames".into());
        }
        let surface = self.dmabuf_surface(dmabuf)?;
        let result = self.encode_surface(surface, frame_number, crf, force_idr);
        if result.is_err() {
            self.forget_import(surface);
        }
        result
    }

    /// The VA surface over `dmabuf`, imported once and kept while its fd names the same buffer
    /// (`DmaBufIdentity`). A capture cycles a few buffers, and a surface made anew each frame
    /// costs the driver a fresh bind of the buffer into the GPU's address space on every
    /// submission: on a discrete Arc at 1080p and 240 fps, a quarter of the capture thread's CPU
    /// and 0.3 ms of each encode. The surface holds its own reference to the buffer, so a dropped
    /// capture buffer is released once its entry is evicted or the session ends.
    fn dmabuf_surface(&mut self, dmabuf: &Dmabuf) -> Result<VASurfaceID, String> {
        let fd = dmabuf
            .handles()
            .next()
            .map(|h| h.as_raw_fd())
            .ok_or("the dmabuf carries no object")?;
        let identity = DmaBufIdentity::probe(
            fd,
            u64::from(dmabuf.format().modifier),
            dmabuf.width(),
            dmabuf.height(),
        );
        if let Some(at) = self.imports.iter().position(|i| i.0 == fd) {
            if self.imports[at].1 == identity {
                return Ok(self.imports[at].2);
            }
            let (_, _, stale) = self.imports.remove(at);
            self.device.destroy_surfaces(&mut [stale]);
        }
        let surface = self.import_dmabuf(dmabuf)?;
        if self.imports.len() == DMABUF_IMPORTS {
            let (_, _, oldest) = self.imports.remove(0);
            self.device.destroy_surfaces(&mut [oldest]);
        }
        self.imports.push((fd, identity, surface));
        Ok(surface)
    }

    /// Release the import behind `surface`, which a failed encode leaves in doubt.
    fn forget_import(&mut self, surface: VASurfaceID) {
        if let Some(at) = self.imports.iter().position(|i| i.2 == surface) {
            self.imports.remove(at);
            self.device.destroy_surfaces(&mut [surface]);
        }
    }

    /// A VA surface over a dmabuf: through the PRIME 2 descriptor, which carries the format
    /// modifier, else the older external-buffer one.
    fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> Result<VASurfaceID, String> {
        let api = self.device.api;
        let display = self.device.display;
        let (width, height) = (self.negotiated.width, self.negotiated.height);
        let handles: Vec<i32> = dmabuf.handles().map(|h| h.as_raw_fd()).collect();
        if handles.len() != 1 {
            return Err("VA-API maps only a dmabuf made of a single object".into());
        }
        let size = unsafe { libc::lseek(handles[0], 0, libc::SEEK_END) };
        if size <= 0 {
            return Err("the dmabuf object reports no size".into());
        }
        let drm_format = dmabuf.format().code as u32;
        let fourcc = match drm_format {
            0x34325241 => VA_FOURCC_BGRA,
            0x34325258 => VA_FOURCC_BGRX,
            0x34324241 => VA_FOURCC_RGBA,
            0x34324258 => VA_FOURCC_RGBX,
            other => return Err(format!("DRM format {other:#x} is not one VA-API maps")),
        };
        let planes: Vec<(u32, u32)> = dmabuf.strides().zip(dmabuf.offsets()).collect();
        let modifier = u64::from(dmabuf.format().modifier);
        if modifier == 0
            && !self.negotiated.fullcolor
            && self.device.rounds_linear_pitch()
            && !planes[0].0.is_multiple_of(64)
        {
            return Err(format!(
                "this VA-API driver reads a linear surface at a 64-byte pitch, not the {} of this dmabuf",
                planes[0].0
            ));
        }
        let mut surface = VA_INVALID_SURFACE;
        let mut attribs: [VASurfaceAttrib; 2] = unsafe { std::mem::zeroed() };
        attribs[0].type_ = VASurfaceAttribMemoryType;
        attribs[0].flags = VA_SURFACE_ATTRIB_SETTABLE;
        attribs[0].value.type_ = VAGenericValueTypeInteger;
        attribs[1].type_ = VASurfaceAttribExternalBufferDescriptor;
        attribs[1].flags = VA_SURFACE_ATTRIB_SETTABLE;
        attribs[1].value.type_ = VAGenericValueTypePointer;
        if modifier != 0x00ff_ffff_ffff_ffff {
            let mut desc: VADRMPRIMESurfaceDescriptor = unsafe { std::mem::zeroed() };
            desc.fourcc = fourcc;
            desc.width = width;
            desc.height = height;
            desc.num_objects = 1;
            desc.objects[0].fd = handles[0];
            desc.objects[0].size = size as u32;
            desc.objects[0].drm_format_modifier = modifier;
            desc.num_layers = 1;
            desc.layers[0].drm_format = drm_format;
            desc.layers[0].num_planes = planes.len() as u32;
            for (i, &(pitch, offset)) in planes.iter().enumerate().take(4) {
                desc.layers[0].object_index[i] = 0;
                desc.layers[0].offset[i] = offset;
                desc.layers[0].pitch[i] = pitch;
            }
            attribs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32;
            attribs[1].value.value.p = &mut desc as *mut VADRMPRIMESurfaceDescriptor as *mut c_void;
            let status = unsafe {
                (api.vaCreateSurfaces)(
                    display,
                    VA_RT_FORMAT_RGB32,
                    width,
                    height,
                    &mut surface,
                    1,
                    attribs.as_mut_ptr(),
                    2,
                )
            };
            if status == VA_STATUS_SUCCESS as VAStatus {
                return Ok(surface);
            }
        }
        let mut handle = handles[0] as usize;
        let mut desc: VASurfaceAttribExternalBuffers = unsafe { std::mem::zeroed() };
        desc.pixel_format = fourcc;
        desc.width = width;
        desc.height = height;
        desc.data_size = size as u32;
        desc.buffers = &mut handle;
        desc.num_buffers = 1;
        desc.num_planes = planes.len() as u32;
        for (i, &(pitch, offset)) in planes.iter().enumerate().take(4) {
            desc.pitches[i] = pitch;
            desc.offsets[i] = offset;
        }
        attribs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME as i32;
        attribs[1].value.value.p = &mut desc as *mut VASurfaceAttribExternalBuffers as *mut c_void;
        self.device.check(
            unsafe {
                (api.vaCreateSurfaces)(
                    display,
                    VA_RT_FORMAT_RGB32,
                    width,
                    height,
                    &mut surface,
                    1,
                    attribs.as_mut_ptr(),
                    2,
                )
            },
            "no VA surface over the dmabuf",
        )?;
        Ok(surface)
    }

    /// Convert `source` onto the encoder's surface: BT.709 primaries and transfer in, full
    /// range, the declared matrix out at limited range, chroma sited at the center of each
    /// block, which is where the software convert puts it. The picture lands unscaled at the
    /// origin of the aligned surface, whose margin past it the stream crops.
    fn convert(&mut self, source: VASurfaceID) -> Result<(), String> {
        let api = self.device.api;
        let display = self.device.display;
        let region = VARectangle {
            x: 0,
            y: 0,
            width: self.negotiated.width as u16,
            height: self.negotiated.height as u16,
        };
        let declared = self.declared_color();
        let mut params: VAProcPipelineParameterBuffer = unsafe { std::mem::zeroed() };
        params.surface = source;
        params.surface_region = &region;
        params.output_region = &region;
        params.output_background_color = 0xff00_0000;
        params.pipeline_flags = 0;
        params.filter_flags = VA_FRAME_PICTURE;
        params.rotation_state = VA_ROTATION_NONE;
        params.mirror_state = VA_MIRROR_NONE;
        params.surface_color_standard = self.vpp_standards.0;
        params.output_color_standard = self.vpp_standards.1;
        params.input_color_properties = ColorDescription::SRGB_SOURCE.properties();
        params.output_color_properties = declared.properties();
        let buffer = self.device.create_buffer(
            self.vpp_context,
            VAProcPipelineParameterBufferType,
            unsafe {
                std::slice::from_raw_parts(
                    &params as *const VAProcPipelineParameterBuffer as *const u8,
                    std::mem::size_of::<VAProcPipelineParameterBuffer>(),
                )
            },
        )?;
        let mut buffers = [buffer];
        let result = (|| {
            self.device.check(
                unsafe { (api.vaBeginPicture)(display, self.vpp_context, self.converted[0]) },
                "vaBeginPicture (video processing)",
            )?;
            let rendered = self.device.check(
                unsafe {
                    (api.vaRenderPicture)(display, self.vpp_context, buffers.as_mut_ptr(), 1)
                },
                "vaRenderPicture (video processing)",
            );
            let ended = self.device.check(
                unsafe { (api.vaEndPicture)(display, self.vpp_context) },
                "vaEndPicture (video processing)",
            );
            rendered.and(ended)
        })();
        unsafe { (api.vaDestroyBuffer)(display, buffer) };
        result
    }

    /// Encode the picture on `source`: convert it, then issue the frame with its parameter
    /// buffers and packed headers, wait for it, and frame the coded bytes for the wire.
    fn encode_surface(
        &mut self,
        source: VASurfaceID,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        if !self.rate.cbr {
            self.qp = self
                .codec
                .hardware_quantizer(self.device.hardware(), crf as i32);
        }
        let held_qp = self.held.take();
        self.convert(source)?;
        let frame_id = frame_number as u16;
        let has_reference = match &self.references {
            Some(References::Window(w)) => w.has_reference(),
            Some(References::Slots(s)) => s.has_reference(),
            None => !self.fresh,
        };
        let mut key = force_idr || self.sequence_start || !has_reference;
        let (mut anchor, mut shared) = (None, false);
        if let Some(References::Window(w)) = &mut self.references
            && w.anchored()
        {
            if !key && w.plan_anchor(false).is_some() && w.settle() == Some(Invalidation::KeyFrame)
            {
                key = true;
            }
            if !key && w.forget_stale() == Some(Invalidation::KeyFrame) {
                key = true;
            }
            anchor = w.plan_anchor(key);
            shared = w.acknowledging() && w.predicts_from_shared();
        }
        let pts = match &self.references {
            Some(References::Window(w)) => w.next_pts(),
            Some(References::Slots(s)) => s.next_pts(),
            None => self.frame_count,
        };
        let held: Vec<(u64, VASurfaceID, bool)> = match &self.references {
            Some(References::Window(w)) => w
                .held()
                .filter_map(|(_, p, lost)| self.surfaces_of.get(&p).map(|&s| (p, s, lost)))
                .collect(),
            _ => Vec::new(),
        };
        let (anchored, long_term): (bool, Vec<(u64, u8)>) = match &self.references {
            Some(References::Window(w)) if w.anchored() => (
                true,
                held.iter()
                    .filter_map(|h| w.anchor_slot(h.0).map(|slot| (h.0, slot)))
                    .collect(),
            ),
            _ => (false, Vec::new()),
        };
        let (reference, key_pts, slots, slot_surfaces) = match &self.references {
            Some(References::Window(w)) => (
                if key {
                    None
                } else {
                    w.newest_valid()
                        .and_then(|(_, p)| self.surfaces_of.get(&p).map(|&s| (p, s)))
                },
                if key { pts } else { w.key_pts() },
                SlotPlan::KEY,
                [VA_INVALID_SURFACE; 3],
            ),
            Some(References::Slots(s)) => {
                let plan = s.plan(key);
                let surfaces = [1u8, 2, 4].map(|slot| {
                    s.slot(slot)
                        .and_then(|(_, p, _)| self.surfaces_of.get(&p).copied())
                        .unwrap_or(VA_INVALID_SURFACE)
                });
                let reference = (plan.predict_from != 0)
                    .then(|| s.slot(plan.predict_from))
                    .flatten()
                    .and_then(|(_, p, _)| self.surfaces_of.get(&p).map(|&surf| (p, surf)));
                (reference, 0, plan, surfaces)
            }
            None => (
                if key {
                    None
                } else {
                    self.surfaces_of.get(&(pts - 1)).map(|&s| (pts - 1, s))
                },
                if key { pts } else { self.key_count },
                SlotPlan::KEY,
                [VA_INVALID_SURFACE; 3],
            ),
        };
        let recon = match &self.references {
            Some(References::Slots(_)) => *self
                .recon
                .iter()
                .find(|s| !slot_surfaces.contains(s))
                .ok_or("no VP8 reconstruction surface is free")?,
            Some(References::Window(_)) => *self
                .recon
                .iter()
                .find(|s| !held.iter().any(|h| h.1 == **s))
                .ok_or("no reconstruction surface is free")?,
            None => self.recon[(pts % self.recon.len() as u64) as usize],
        };
        let qp = if self.rate.cbr {
            self.qp
        } else {
            held_qp.unwrap_or(self.qp)
        };
        let frame = Frame {
            key,
            pts,
            key_pts,
            recon,
            coded: self.coded,
            reference,
            held: &held,
            anchored,
            anchor,
            long_term: &long_term,
            qp,
            slots,
            slot_surfaces,
        };
        let mut out = Buffers::new();
        let mut bps = self
            .cleanup_bps
            .take()
            .unwrap_or(self.negotiated.bits_per_second);
        if self.rate.cbr && key && held_qp.is_some() {
            // A held key frame's budget, as one frame's of a rate sent with it.
            let fps = self.negotiated.fps.fps().max(1.0);
            let bits = held_key_bits(
                self.negotiated.bits_per_second,
                self.negotiated.width,
                self.negotiated.height,
            );
            bps = (bits * fps).min(u32::MAX as f64) as u32;
        }
        if key {
            self.arm.sequence(&self.negotiated, &mut out);
            self.rate_control_at(&mut out, bps);
            self.sent_bps = bps;
        } else if self.rate.cbr && bps != self.sent_bps {
            // radeonsi's VCE takes a new target with a picture, no new sequence.
            self.rate_control_at(&mut out, bps);
            self.sent_bps = bps;
        }
        self.arm.picture(&self.negotiated, &frame, &mut out)?;
        let refinement = self.refinement.take().filter(|_| !key && held_qp.is_none());
        if let Some(refinement) = &refinement {
            self.roi_parameter(refinement, &mut out);
        }
        let header_len = if self.omit_headers {
            0
        } else {
            VIDEO_HEADER_LEN
        };
        let mut output = vec![0; header_len];
        self.issue(&out, &mut output)?;
        if std::mem::take(&mut self.measure) {
            self.last_psnr = self.picture_psnr(recon);
            self.measures = self.last_psnr.is_some();
        }
        if key
            && self.codec == Codec::H264
            && let Some(bounded) = self.reorder.apply(&output[header_len..])
        {
            output.truncate(header_len);
            output.extend_from_slice(&bounded);
        }
        self.fresh = false;
        self.sequence_start = false;
        self.surfaces_of.insert(pts, recon);
        let coded = &output[header_len..];
        let frame_type = match self.codec {
            Codec::H264 => h264_frame_type(coded),
            Codec::H265 => h265_frame_type(coded),
            Codec::Vp8 => frame_type_from_key(vp8_is_key(coded)),
            Codec::Vp9 => frame_type_from_key(vp9_is_key(coded)),
            _ => frame_type_from_key(av1_is_key(coded)),
        };
        let is_key = frame_type != super::codec::FRAME_DELTA;
        if is_key
            && self.codec == Codec::H264
            && let Some(References::Window(w)) = &mut self.references
            && let Some(range) = h264_frame_num_range(coded)
        {
            w.set_frame_num_range(range);
        }
        self.last_reference = match &mut self.references {
            Some(References::Window(w)) => w.record_marked(frame_id, is_key, anchor),
            Some(References::Slots(s)) => {
                let plan = if is_key { s.plan(true) } else { slots };
                s.record(frame_id, plan)
            }
            None => {
                self.frame_count = pts + 1;
                if is_key {
                    self.key_count = pts;
                }
                Reference::Untracked
            }
        };
        match &self.references {
            Some(References::Slots(s)) => {
                let held: Vec<u64> = [1u8, 2, 4]
                    .iter()
                    .filter_map(|&slot| s.slot(slot).map(|(_, p, _)| p))
                    .collect();
                self.surfaces_of.retain(|p, _| held.contains(p));
            }
            Some(References::Window(w)) => {
                let held: Vec<u64> = w.held().map(|(_, p, _)| p).collect();
                self.surfaces_of.retain(|p, _| held.contains(p));
            }
            None => self
                .surfaces_of
                .retain(|&p, _| p + self.recon.len() as u64 > pts),
        }
        if !self.omit_headers {
            let mut header = Vec::with_capacity(VIDEO_HEADER_LEN);
            let anchor_flag = if anchor.is_some() && !is_key && shared {
                super::codec::FRAME_ANCHOR
            } else {
                0
            };
            push_video_header(
                &mut header,
                self.codec,
                frame_type | anchor_flag,
                frame_id,
                0,
                self.negotiated.width as u16,
                self.negotiated.height as u16,
                self.last_reference,
            );
            output[..VIDEO_HEADER_LEN].copy_from_slice(&header);
        }
        Ok(output)
    }

    /// Render `buffers` as one picture on the encode context, wait for it, and append the
    /// coded bytes to `out`.
    fn issue(&mut self, buffers: &Buffers, out: &mut Vec<u8>) -> Result<(), String> {
        let api = self.device.api;
        let display = self.device.display;
        let mut ids = Vec::with_capacity(buffers.entries.len());
        let mut created = Ok(());
        for (kind, bytes) in &buffers.entries {
            match self.device.create_buffer(self.context, *kind, bytes) {
                Ok(id) => ids.push(id),
                Err(e) => {
                    created = Err(e);
                    break;
                }
            }
        }
        let result = created.and_then(|()| {
            self.device.check(
                unsafe { (api.vaBeginPicture)(display, self.context, self.converted[0]) },
                "vaBeginPicture",
            )?;
            let rendered = self.device.check(
                unsafe {
                    (api.vaRenderPicture)(
                        display,
                        self.context,
                        ids.as_mut_ptr(),
                        ids.len() as c_int,
                    )
                },
                "vaRenderPicture",
            );
            let ended = self.device.check(
                unsafe { (api.vaEndPicture)(display, self.context) },
                "vaEndPicture",
            );
            rendered.and(ended)
        });
        for id in ids {
            unsafe { (api.vaDestroyBuffer)(display, id) };
        }
        result?;
        let synced = match api.vaSyncBuffer {
            Some(sync) => {
                let status = unsafe { sync(display, self.coded, u64::MAX) };
                if status == VA_STATUS_ERROR_UNIMPLEMENTED as VAStatus {
                    None
                } else {
                    Some(self.device.check(status, "vaSyncBuffer"))
                }
            }
            None => None,
        };
        match synced {
            Some(result) => result?,
            None => self.device.check(
                unsafe { (api.vaSyncSurface)(display, self.converted[0]) },
                "vaSyncSurface",
            )?,
        }
        let mut list: *mut c_void = ptr::null_mut();
        self.device.check(
            unsafe { (api.vaMapBuffer)(display, self.coded, &mut list) },
            "vaMapBuffer (coded)",
        )?;
        let segments = || {
            std::iter::successors(
                unsafe { (list as *const VACodedBufferSegment).as_ref() },
                |s| unsafe { (s.next as *const VACodedBufferSegment).as_ref() },
            )
            .filter(|s| !s.buf.is_null())
        };
        let from = out.len();
        let average_qp = segments()
            .next()
            .map_or(0, |s| s.status & VA_CODED_BUF_STATUS_PICTURE_AVE_QP_MASK);
        out.reserve(segments().map(|s| s.size as usize).sum());
        for s in segments() {
            out.extend_from_slice(unsafe {
                std::slice::from_raw_parts(s.buf as *const u8, s.size as usize)
            });
        }
        unsafe { (api.vaUnmapBuffer)(display, self.coded) };
        if self.rate.cbr && matches!(self.arm, Arm::H264(_) | Arm::H265(_)) {
            strip_zero_padding(out, from);
        }
        self.last_bytes = Some(out.len() - from);
        let quantizer = match &self.arm {
            Arm::H264(_) | Arm::H265(_) => Some(average_qp).filter(|&q| q > 0),
            Arm::Av1(a) => a.coded_qindex(&out[from..]),
            Arm::Vp9(_) => super::codec::vp9_base_q_idx(&out[from..]),
            Arm::Vp8(_) => None,
        };
        self.last_quality = quantizer
            .filter(|_| self.rate.cbr)
            .map(|q| self.codec.hardware_quality_index(self.device.hardware(), q));
        Ok(())
    }
}

/// A color description as the video processor is told it: the ISO/IEC 23091-2 code points
/// and the range, and whether the picture is RGB, whose matrix is fixed.
#[derive(Clone, Copy)]
struct ColorDescription {
    primaries: u8,
    transfer: u8,
    matrix: u8,
    full_range: bool,
    rgb: bool,
}

impl ColorDescription {
    /// The sRGB desktop picture every session starts from: BT.709 primaries and transfer,
    /// full range.
    const SRGB_SOURCE: Self = Self {
        primaries: 1,
        transfer: 1,
        matrix: 0,
        full_range: true,
        rgb: true,
    };

    fn properties(self) -> VAProcColorProperties {
        VAProcColorProperties {
            chroma_sample_location: (VA_CHROMA_SITING_VERTICAL_CENTER
                | VA_CHROMA_SITING_HORIZONTAL_CENTER) as u8,
            color_range: if self.full_range {
                VA_SOURCE_RANGE_FULL
            } else {
                VA_SOURCE_RANGE_REDUCED
            } as u8,
            colour_primaries: self.primaries,
            transfer_characteristics: self.transfer,
            matrix_coefficients: self.matrix,
            reserved: [0; 3],
        }
    }
}

/// The color standard a video processor is told for `wanted`: the explicit one where the
/// driver takes it, since the properties then say everything; else the standard whose
/// matrix, transfer, and primaries come closest, weighted four, two, and one, or none when
/// nothing matches at all.
fn color_standard(
    offered: &[VAProcColorStandardType],
    wanted: ColorDescription,
) -> VAProcColorStandardType {
    if offered.contains(&VAProcColorStandardExplicit) {
        return VAProcColorStandardExplicit;
    }
    const TABLE: [(VAProcColorStandardType, u8, u8, u8); 12] = [
        (VAProcColorStandardBT601, 5, 6, 5),
        (VAProcColorStandardBT601, 6, 6, 6),
        (VAProcColorStandardBT709, 1, 1, 1),
        (VAProcColorStandardBT470M, 4, 4, 4),
        (VAProcColorStandardBT470BG, 5, 5, 5),
        (VAProcColorStandardSMPTE170M, 6, 6, 6),
        (VAProcColorStandardSMPTE240M, 7, 7, 7),
        (VAProcColorStandardGenericFilm, 8, 1, 1),
        (VAProcColorStandardSRGB, 1, 13, 0),
        (VAProcColorStandardXVYCC601, 1, 11, 5),
        (VAProcColorStandardXVYCC709, 1, 11, 1),
        (VAProcColorStandardBT2020, 9, 14, 9),
    ];
    let matrix_counts = !wanted.rgb;
    let worst = 4 * matrix_counts as u32 + 2 + 1;
    let mut best = (worst, VAProcColorStandardNone);
    for &standard in offered {
        for &(_, primaries, transfer, matrix) in TABLE.iter().filter(|t| t.0 == standard) {
            let score = 4 * (matrix_counts && wanted.matrix != matrix) as u32
                + 2 * (wanted.transfer != transfer) as u32
                + (wanted.primaries != primaries) as u32;
            if score < best.0 {
                best = (score, standard);
            }
        }
    }
    best.1
}

/// The driver's image format of `fourcc`, with the masks it wants an image created with.
fn image_format(device: &Device, fourcc: u32) -> Option<VAImageFormat> {
    let api = device.api;
    let mut formats: Vec<VAImageFormat> = vec![
        unsafe { std::mem::zeroed() };
        unsafe { (api.vaMaxNumImageFormats)(device.display) }.max(0)
            as usize
    ];
    let mut count: c_int = 0;
    if unsafe { (api.vaQueryImageFormats)(device.display, formats.as_mut_ptr(), &mut count) }
        != VA_STATUS_SUCCESS as VAStatus
    {
        return None;
    }
    formats[..count.max(0) as usize]
        .iter()
        .find(|f| f.fourcc == fourcc)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1080p frame is refined from a twenty-fifth of a bit a pixel on: 60 fps at 5 Mbit/s,
    /// 8 fps at 0.7, not 60 fps at 2 Mbit/s or 8 fps at 0.5.
    #[test]
    fn radeonsi_refines_from_a_twenty_fifth_of_a_bit_a_pixel() {
        assert!(refines(5_000_000, 60.0, 1920, 1080));
        assert!(refines(700_000, 8.0, 1920, 1080));
        assert!(!refines(2_000_000, 60.0, 1920, 1080));
        assert!(!refines(500_000, 8.0, 1920, 1080));
    }

    /// A held key frame gets a second of the rate, and never less than 1.6 bits a pixel.
    #[test]
    fn a_held_key_frame_gets_a_second_of_the_rate_and_enough_for_the_picture() {
        assert_eq!(held_key_bits(500_000, 1920, 1080), 1.6 * 1920.0 * 1080.0);
        assert_eq!(held_key_bits(8_000_000, 1920, 1080), 8_000_000.0);
    }

    /// The planar surface is tried first wherever a device both allocates and renders it, the
    /// packed one after it, and the profile ladder names the 4:4:4 profiles only where a
    /// codec carries them.
    #[test]
    fn surface_and_profile_ladders() {
        assert_eq!(FULLCOLOR_FOURCCS, [VA_FOURCC_444P, VA_FOURCC_XYUV]);
        assert_eq!(fourcc_name(VA_FOURCC_XYUV), "vuyx");
        assert_eq!(fourcc_name(VA_FOURCC_444P), "yuv444p");
        assert!(
            profile_ladder(Codec::H264, true, 8).is_empty(),
            "no 4:4:4 H.264 profile is served"
        );
        assert_eq!(profile_ladder(Codec::H265, true, 8), [VAProfileHEVCMain444]);
        assert_eq!(profile_ladder(Codec::Vp9, true, 8), [VAProfileVP9Profile1]);
        assert!(profile_ladder(Codec::Av1, true, 8).is_empty());
        assert_eq!(
            profile_ladder(Codec::H265, false, 10),
            [VAProfileHEVCMain10]
        );
        assert_eq!(
            profile_ladder(Codec::H265, true, 10),
            [VAProfileHEVCMain444_10]
        );
        assert_eq!(
            profile_ladder(Codec::Vp9, false, 10),
            [VAProfileVP9Profile2]
        );
        assert_eq!(profile_ladder(Codec::Vp9, true, 10), [VAProfileVP9Profile3]);
        assert_eq!(
            profile_ladder(Codec::Av1, false, 10),
            [VAProfileAV1Profile0]
        );
        assert!(profile_ladder(Codec::H264, false, 10).is_empty());
        assert!(profile_ladder(Codec::Vp8, false, 10).is_empty());
        for codec in Codec::VIDEO {
            assert!(!profile_ladder(codec, false, 8).is_empty(), "{codec:?}");
        }
    }

    /// The color standard follows libavfilter's choice: explicit wherever the driver takes
    /// it, else the closest standard by matrix, transfer, and primaries, with an RGB source
    /// scored on transfer and primaries alone and a tie going to the one the driver lists
    /// first.
    #[test]
    fn the_color_standard_is_the_nearest_the_driver_offers() {
        let explicit = [VAProcColorStandardBT601, VAProcColorStandardExplicit];
        assert_eq!(
            color_standard(&explicit, ColorDescription::SRGB_SOURCE),
            VAProcColorStandardExplicit
        );
        let classic = [
            VAProcColorStandardBT601,
            VAProcColorStandardBT709,
            VAProcColorStandardSMPTE170M,
            VAProcColorStandardSRGB,
        ];
        assert_eq!(
            color_standard(&classic, ColorDescription::SRGB_SOURCE),
            VAProcColorStandardBT709
        );
        let bt709 = ColorDescription {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
            rgb: false,
        };
        assert_eq!(color_standard(&classic, bt709), VAProcColorStandardBT709);
        let bt601 = ColorDescription { matrix: 6, ..bt709 };
        assert_eq!(
            color_standard(&classic, bt601),
            VAProcColorStandardBT601,
            "the driver's first of two equal matches"
        );
        assert_eq!(
            color_standard(&classic[2..], bt601),
            VAProcColorStandardSMPTE170M
        );
        assert_eq!(
            color_standard(&[VAProcColorStandardBT2020], bt709),
            VAProcColorStandardNone,
            "a total mismatch names no standard"
        );
    }

    /// Slices follow the driver's structure: as many rows as asked where rows are free, equal
    /// rows where the driver wants them equal, a power of two of rows where that is all it
    /// takes, and one row each where only that is.
    #[test]
    fn slice_layout_follows_the_driver() {
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_ARBITRARY_ROWS, 32, 68, 4),
            Ok((4, 17))
        );
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_POWER_OF_TWO_ROWS, 32, 68, 4),
            Ok((5, 16))
        );
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_EQUAL_ROWS, 128, 68, 4),
            Ok((68, 1))
        );
        assert!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_EQUAL_ROWS, 32, 68, 4).is_err(),
            "more slices than the driver takes"
        );
        assert_eq!(
            slice_layout(
                VA_ENC_SLICE_STRUCTURE_EQUAL_ROWS | VA_ENC_SLICE_STRUCTURE_EQUAL_MULTI_ROWS,
                256,
                68,
                4
            ),
            Ok((4, 17)),
            "equal rows of more than one where the driver offers both"
        );
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_EQUAL_MULTI_ROWS, 32, 68, 4),
            Ok((4, 17))
        );
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_EQUAL_MULTI_ROWS, 32, 67, 4),
            Ok((4, 17)),
            "the last slice is the shorter one"
        );
        assert!(slice_layout(0, 32, 68, 4).is_err());
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_ARBITRARY_ROWS, 32, 2, 4),
            Ok((2, 1)),
            "no more slices than rows"
        );
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_ARBITRARY_ROWS, 2, 68, 4),
            Ok((2, 34)),
            "no more slices than the driver takes"
        );
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_ARBITRARY_ROWS, 1, 68, 4),
            Ok((1, 68))
        );
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_POWER_OF_TWO_ROWS, 4, 68, 4),
            Ok((3, 32))
        );
        assert_eq!(
            slice_layout(VA_ENC_SLICE_STRUCTURE_POWER_OF_TWO_ROWS, 1, 68, 4),
            Ok((1, 128))
        );
    }

    /// Construction either stands a session up or says why it could not; a half-built
    /// encoder must never reach a caller, and a session never quietly changes chroma. Runs
    /// everywhere: a host without a VA-API device exercises the error path.
    #[test]
    fn construction_answers_or_refuses() {
        let mut settings = CaptureSettings {
            width: 128,
            height: 128,
            codec: Codec::H264,
            video_fullcolor: true,
            ..Default::default()
        };
        for codec in Codec::VIDEO {
            settings.codec = codec;
            match VaapiEncoder::new(&settings, codec, Input::Host { rgba: false }) {
                Ok(enc) => assert_eq!(
                    enc.is_fullcolor(),
                    matches!(codec, Codec::H265 | Codec::Vp9),
                    "{codec:?}"
                ),
                Err(e) => assert!(!e.is_empty(), "refusal must carry a reason"),
            }
        }
        settings.video_fullcolor = false;
        match VaapiEncoder::new(&settings, Codec::H264, Input::Host { rgba: false }) {
            Ok(enc) => assert!(!enc.is_fullcolor(), "4:2:0 session reports 4:4:4"),
            Err(e) => assert!(!e.is_empty(), "refusal must carry a reason"),
        }
        assert!(VaapiEncoder::new(&settings, Codec::Jpeg, Input::Host { rgba: false }).is_err());
    }

    /// On a VA-API device (`cargo test vaapi_ -- --ignored --nocapture`): an AMD VCE device is
    /// told from a VCN one by the kernel, and its H.264 key frame is one slice, as a Skylake's
    /// or a Broxton's is on the low-power entry point.
    #[test]
    #[ignore]
    fn vaapi_vce_h264_is_one_slice() {
        let (w, h) = (1920usize, 1080usize);
        let settings = CaptureSettings {
            width: w as i32,
            height: h as i32,
            codec: Codec::H264,
            omit_stripe_headers: true,
            ..Default::default()
        };
        let mut enc = VaapiEncoder::new(&settings, Codec::H264, Input::Host { rgba: false })
            .expect("a VA-API H.264 session");
        let frame: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % w) as u8, (i / w) as u8, 0x80, 0xff])
            .collect();
        let key = enc
            .encode_host(&frame, w * 4, false, 0, 25, true)
            .expect("encode");
        let slices = super::super::codec::annexb_nals(&key)
            .filter(|n| matches!(n[0] & 0x1f, 1 | 5))
            .count();
        println!(
            "{}: VCE {}, {slices} slice(s) in the key frame",
            enc.vendor(),
            enc.device.vce
        );
        let wanted = if enc.device.one_slice_h264(enc.negotiated.entrypoint) {
            1
        } else {
            SLICES as usize
        };
        assert_eq!(slices, wanted);
    }

    /// A 1280x720 BGRA picture that moves: a bar crossing 12 pixels a frame, a checkerboard band
    /// 4, and fresh noise below row 480.
    fn moving_picture(t: usize) -> Vec<u8> {
        let (w, h) = (1280usize, 720usize);
        let noise = |n: usize| {
            let mut x = (n as u64 ^ 0x9e37_79b9_7f4a_7c15).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            x ^= x >> 31;
            x as u8
        };
        let bar = (t * 12) % (w - 160);
        let mut f = vec![0u8; w * h * 4];
        for (i, px) in f.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            let bgr: [u8; 3] = if y >= 480 {
                let n = (t % 4) * w * 240 + (y - 480) * w + x;
                [noise(3 * n), noise(3 * n + 1), noise(3 * n + 2)]
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
            px[3] = 0xff;
        }
        f
    }

    /// Encode `moving_picture` for 120 frames, reporting a loss `depth` frames back every
    /// `every` frames from frame `from`, and decode it twice, once without the frames lost:
    /// the frames predicted past a loss, those from an anchor among them, the key frames after
    /// the first, how far the second decoder's pictures come from the first's at most, the
    /// worst average distance of a picture from its source, and whether the device keeps
    /// anchors (`keeps_anchors`).
    fn predict_past_losses(
        codec: Codec,
        from: usize,
        every: usize,
        depth: usize,
    ) -> Option<(usize, usize, usize, f64, f64, bool)> {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (1280usize, 720usize);
        let settings = CaptureSettings {
            width: w as i32,
            height: h as i32,
            codec,
            target_fps: 60.0,
            video_cbr_mode: true,
            video_bitrate_kbps: 8000,
            omit_stripe_headers: true,
            ..Default::default()
        };
        let mut enc = match VaapiEncoder::new(&settings, codec, Input::Host { rgba: false }) {
            Ok(enc) => enc,
            Err(e) => {
                println!("{codec:?}: {e}");
                return None;
            }
        };
        println!("{codec:?} on {}", enc.vendor());
        let (mut whole, mut lossy) = (
            VideoDecoder::new(codec).expect("decoder"),
            VideoDecoder::new(codec).expect("decoder"),
        );
        let reported = |r: usize| r >= from && (r - from).is_multiple_of(every);
        let (mut past, mut anchored, mut keys, mut apart, mut worst) = (0, 0, 0, 0f64, 0f64);
        for t in 0..120usize {
            if reported(t) {
                assert!(enc.invalidate_reference((t - depth) as u16), "{codec:?}");
            }
            let from_anchor = matches!(&enc.references,
                Some(References::Window(win)) if win.predicting_anchor().is_some());
            let src = moving_picture(t);
            let out = enc
                .encode_host(&src, w * 4, false, t as u64, 25, t == 0)
                .expect("encode");
            match enc.last_reference() {
                Reference::None if t > 0 => keys += 1,
                Reference::Frame(r) if r as usize + 1 != t => {
                    past += 1;
                    anchored += usize::from(from_anchor);
                }
                _ => {}
            }
            assert!(whole.decode(&out).expect("decode"), "{codec:?}: frame {t}");
            if (t + 1..=t + depth).any(reported) {
                continue;
            }
            assert!(
                lossy.decode(&out).expect("decode"),
                "{codec:?}: lossy frame {t}"
            );
            let (a, b) = (whole.frame().unwrap(), lossy.frame().unwrap());
            let mut sum = 0f64;
            for y in (0..480).step_by(2) {
                for x in (0..w).step_by(2) {
                    let i = (y * w + x) * 4;
                    let (bb, g, r) = (src[i] as f64, src[i + 1] as f64, src[i + 2] as f64);
                    let luma = 16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * bb) * 219.0 / 255.0;
                    let (pa, pb) = (
                        a.y[y * a.y_stride + x] as f64,
                        b.y[y * b.y_stride + x] as f64,
                    );
                    sum += (pb - luma).abs();
                    apart = apart.max((pa - pb).abs());
                }
            }
            worst = worst.max(sum / (240.0 * (w / 2) as f64));
        }
        println!(
            "{codec:?}: {past} frames predicted past a loss ({anchored} from an anchor), {keys} key frames; the lossy decoder at most {apart:.0} levels from the whole one, {worst:.1} on average from the source at worst"
        );
        Some((
            past,
            anchored,
            keys,
            apart,
            worst,
            enc.device.keeps_anchors(),
        ))
    }

    /// On a VA-API device (`cargo test vaapi_ -- --ignored --nocapture`): a loss reported every
    /// thirteen frames, four frames deep, is predicted past from the frame before it: a decoder
    /// that never saw the lost frames shows every later picture as one that saw them all does.
    #[test]
    #[ignore]
    fn vaapi_predicts_past_a_lost_frame() {
        for codec in [Codec::H264, Codec::H265] {
            if let Some((past, _, keys, apart, worst, _)) = predict_past_losses(codec, 13, 13, 4) {
                assert!(past >= 5, "{codec:?}: {past} predicted past a loss");
                assert_eq!(keys, 0, "{codec:?}");
                assert!(
                    apart < 2.0,
                    "{codec:?}: the decoder without the lost frames is {apart} off"
                );
                assert!(worst < 20.0, "{codec:?}: {worst} off the source");
            }
        }
    }

    /// On a VA-API device that keeps anchors (`keeps_anchors`): a loss older than the recent
    /// frames, twelve frames deep after the anchors at 48 and 96 (H.264; H.265 marks one every
    /// twelve), is predicted past from an anchor, and an H.264 one four frames deep covering the
    /// anchor at 48 from the recent frame before it, the lost anchor listed long-term while held;
    /// either way the decoder that never saw the lost frames shows every later picture as one
    /// that saw them all does. (Deeper, a gap FFmpeg fills would push that frame out of a
    /// browser's decoder, and the window codes a key frame: `forget_stale`.) On one that keeps
    /// none (iHD) the loss older than the recent frames is answered with a key frame, and the
    /// one four frames deep is predicted past from a recent frame, with no key frame.
    #[test]
    #[ignore]
    fn vaapi_predicts_from_an_anchor() {
        for (codec, from, every, depth, want) in [
            (Codec::H264, 70, 40, 12, 2),
            (Codec::H264, 52, 1000, 4, 0),
            (Codec::H265, 70, 40, 12, 2),
        ] {
            if let Some((past, anchored, keys, apart, worst, anchors)) =
                predict_past_losses(codec, from, every, depth)
            {
                if anchors {
                    assert!(
                        past >= 1 && anchored == want.min(past),
                        "{codec:?}: {anchored} of {past} from an anchor"
                    );
                    assert_eq!(keys, 0);
                } else if want > 0 {
                    assert!(
                        anchored == 0 && keys >= 1,
                        "{codec:?}: a loss past the recent frames took {keys} key frames"
                    );
                } else {
                    assert!(
                        past >= 1 && keys == 0,
                        "{codec:?}: {past} predicted past the loss, {keys} key frames"
                    );
                }
                assert!(
                    apart < 2.0,
                    "the decoder without the lost frames is {apart} off"
                );
                assert!(worst < 20.0, "{worst} off the source");
            }
        }
    }

    /// A node the kernel answers no DRM query on is not VCE.
    #[test]
    fn a_node_that_is_not_amdgpu_is_not_vce() {
        let node = std::fs::File::open("/dev/null").unwrap();
        assert!(!amd_vce(node.as_raw_fd()));
    }

    /// On a VA-API device (`cargo test vaapi_ -- --ignored --nocapture`): whichever sequence
    /// parameter set the driver writes, the key frame bounds reordering at zero and decodes.
    #[test]
    #[ignore]
    fn vaapi_h264_bounds_reordering_at_zero() {
        use crate::encoders::sps::fixtures::assert_no_reorder;
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (1280usize, 720usize);
        let settings = CaptureSettings {
            width: w as i32,
            height: h as i32,
            codec: Codec::H264,
            video_cbr_mode: true,
            video_bitrate_kbps: 4000,
            omit_stripe_headers: true,
            ..Default::default()
        };
        let mut enc = VaapiEncoder::new(&settings, Codec::H264, Input::Host { rgba: false })
            .expect("a VA-API H.264 session");
        let frame: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % w) as u8, (i / w) as u8, 0x80, 0xff])
            .collect();
        let key = enc
            .encode_host(&frame, w * 4, false, 0, 25, true)
            .expect("encode");
        assert_no_reorder(&key, enc.vendor());
        let mut dec = VideoDecoder::new(Codec::H264).expect("decoder");
        assert!(
            dec.decode(&key).expect("the bounded key frame decodes"),
            "no picture"
        );
    }

    /// On a VA-API device (`cargo test vaapi_ -- --ignored --nocapture`): a constant-rate H.264
    /// session at 2160p decodes close to its source at 60 fps, after a change to 75 fps, and
    /// back, its SPS declaring the level each rate needs (5.2, 6.0, 5.2) whatever level the
    /// driver is told (`h264_driver_level`). OpenH264, the decoder here, refuses an SPS above
    /// 5.2, so the copy it decodes declares 5.2; the pictures are the encoder's.
    #[test]
    #[ignore]
    fn vaapi_h264_constant_rate_decodes_above_level_5_2() {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h, tile_w, tile_h) = (3840usize, 2160usize, 1280usize, 720usize);
        let mut settings = CaptureSettings {
            width: w as i32,
            height: h as i32,
            codec: Codec::H264,
            target_fps: 60.0,
            video_cbr_mode: true,
            video_bitrate_kbps: 39000,
            omit_stripe_headers: true,
            ..Default::default()
        };
        let mut enc = VaapiEncoder::new(&settings, Codec::H264, Input::Host { rgba: false })
            .expect("a VA-API H.264 session");
        let mut dec = VideoDecoder::new(Codec::H264).expect("decoder");
        let mut phases = Vec::new();
        for (phase, fps, level) in [(0usize, 60.0, 52u8), (1, 75.0, 60), (2, 60.0, 52)] {
            if phase > 0 {
                settings.target_fps = fps;
                enc.reconfigure_rate(&settings).expect("reconfigure");
            }
            let (mut declared, mut failed, mut worst) = (None, 0, 0f64);
            for t in phase * 30..phase * 30 + 30 {
                let tile = moving_picture(t);
                let src: Vec<u8> = (0..h)
                    .flat_map(|y| {
                        let row = &tile[(y % tile_h) * tile_w * 4..][..tile_w * 4];
                        [row, row, row].concat()
                    })
                    .collect();
                let out = enc
                    .encode_host(&src, w * 4, false, t as u64, 25, t == 0)
                    .expect("encode");
                let sps = super::super::codec::annexb_nals(&out)
                    .find(|n| n[0] & 0x1f == 7 && n.len() > 3)
                    .map(|n| n.as_ptr() as usize - out.as_ptr() as usize + 3);
                let mut fed = out.clone();
                if let Some(at) = sps {
                    declared = Some(out[at]);
                    fed[at] = fed[at].min(52);
                }
                if !dec.decode(&fed).unwrap_or(false) {
                    failed += 1;
                    continue;
                }
                let p = dec.frame().unwrap();
                let mut sum = 0f64;
                for y in (0..h).step_by(4) {
                    for x in (0..w).step_by(4) {
                        let i = (y * w + x) * 4;
                        let (b, g, r) = (src[i] as f64, src[i + 1] as f64, src[i + 2] as f64);
                        let luma = 16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0;
                        sum += (p.y[y * p.y_stride + x] as f64 - luma).abs();
                    }
                }
                worst = worst.max(sum / ((h / 4) * (w / 4)) as f64);
            }
            println!(
                "{} {fps} fps: level {declared:?}, {failed} frames undecodable, {worst:.1} on average from the source at worst",
                enc.vendor()
            );
            phases.push((fps, level, declared, failed, worst));
        }
        for (fps, level, declared, failed, worst) in phases {
            assert_eq!(declared, Some(level), "{fps} fps");
            assert_eq!(failed, 0, "{fps} fps");
            assert!(worst < 20.0, "{fps} fps: {worst} off the source");
        }
    }

    /// On a VA-API device (`cargo test vaapi_ -- --ignored --nocapture`, its render node in
    /// `PIXELFLUX_TEST_RENDER_NODE`): a zero-copy session keeps its import of each recurring
    /// dmabuf (`dmabuf_surface`), so a buffer repainted between frames, with another buffer's
    /// frames in between, decodes to what was painted last, never to the picture it held when
    /// it was first imported.
    #[test]
    #[ignore]
    fn vaapi_dmabuf_import_follows_the_buffer() {
        use crate::encoders::nvenc::gpu_tests::{
            assert_painted, block_rect, decoded_means, gpu_render, paint_dmabuf, painted_dmabuf,
        };
        use crate::webcam::decode::VideoDecoder;
        let (w, h) = (1280u32, 720u32);
        let Some((gbm, mut renderer)) = gpu_render() else {
            return;
        };
        let settings = CaptureSettings {
            width: w as i32,
            height: h as i32,
            codec: Codec::H264,
            target_fps: 60.0,
            video_crf: 25,
            ..Default::default()
        };
        let mut enc = VaapiEncoder::new(&settings, Codec::H264, Input::Dmabuf)
            .expect("a zero-copy VA-API H.264 session");
        let mut dec = VideoDecoder::new(Codec::H264).expect("decoder");
        let (_a_bo, mut a) = painted_dmabuf(&gbm, &mut renderer, w, h, 1);
        let (_b_bo, mut b) = painted_dmabuf(&gbm, &mut renderer, w, h, 2);
        for frame in 0..8u32 {
            let seed = frame + 1;
            let (name, buffer) = if frame % 2 == 0 {
                ("A", &mut a)
            } else {
                ("B", &mut b)
            };
            if frame >= 2 {
                paint_dmabuf(&mut renderer, buffer, w, h, seed);
            }
            let out = enc
                .encode_dmabuf(buffer, u64::from(frame), 25, frame == 0)
                .expect("encode");
            let (block, bg) = decoded_means(&mut dec, &out, block_rect(w, h, seed));
            println!(
                "{} frame {frame} (buffer {name}): block {block:.1?}, background {bg:.1?}",
                enc.vendor()
            );
            assert_painted(&format!("frame {frame}, buffer {name}"), block, bg, 6.0);
        }
        assert_eq!(enc.imports.len(), 2, "one import a buffer");
    }
}
