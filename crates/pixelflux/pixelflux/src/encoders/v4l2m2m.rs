//! Hardware H.264 through a generic stateful V4L2 M2M encoder.
//!
//! Boards that carry an encoder behind the kernel's own M2M interface publish no render node the
//! VA-API backend can probe and no vendor library the NVENC one can load, so the ladder reaches
//! software on hardware that encodes. A Raspberry Pi 4 is the case at hand (`bcm2835-codec`), and
//! RK356x and i.MX8M carry the same interface, so the node is discovered by what it offers rather
//! than by the board it sits on.
//!
//! One host frame becomes one access unit: the frame is copied into the output buffer, queued, and
//! the access unit comes back on the capture queue. The copy is the floor here as it is on Tegra —
//! X11 capture hands over host memory — and it costs 3.81 ms for a 1080p frame on a Pi 4, which is
//! the same speed as a copy between two ordinary allocations: the `videobuf2-dma-contig` buffer
//! takes a write stream at full memory bandwidth.
//!
//! Three things about such a device were measured rather than assumed, and each shapes the code:
//!
//! 1. The output queue takes packed 32-bit color and the firmware converts it, so nothing converts
//!    on the CPU. The fourcc names are the opposite way round from the memory a caller holds:
//!    `AB24` takes the BGRA an X11 capture produces, and `BGR4` takes RGBA. Decoding an encoded
//!    chart is what settled that; reading the names would have swapped red and blue.
//! 2. The conversion is BT.601 at full range, and it cannot be steered: a `S_FMT` asking for
//!    REC709 at limited range comes back echoed and changes nothing. The stream also carries no
//!    `video_signal_type`, so what the session declares has to be written into the SPS on the way
//!    out, which `collect` is the place for.
//! 3. Constant bitrate is not available, and asking for it does not merely fail: the driver hands
//!    the firmware a rate control mode it refuses, and the next `STREAMON` answers `ESRCH`. A
//!    control this session cannot set is therefore a refusal, never a warning — the node has to be
//!    closed and reopened to be usable again.

use std::mem::size_of;
use std::os::fd::RawFd;
use std::sync::OnceLock;

use super::codec::{
    Codec, VIDEO_HEADER_LEN, frame_type_from_key, h264_frame_type, h265_frame_type,
    push_video_header, vp8_is_key, vp9_is_key,
};
use super::reference::Reference;
use super::sps::{self, ColorSignal};
use crate::CaptureSettings;

const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
const V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_8000;
const V4L2_CAP_VIDEO_M2M: u32 = 0x0000_4000;

/// Packed 32-bit color. The kernel spells these by channel order in a word and a caller holds
/// bytes, so `AB24` is the BGRA of an X11 capture and `BGR4` is RGBA.
const V4L2_PIX_FMT_BGRA: u32 = 0x3432_4241;
const V4L2_PIX_FMT_RGBA: u32 = 0x3452_4742;
const V4L2_PIX_FMT_H264: u32 = 0x3436_3248;
const V4L2_PIX_FMT_HEVC: u32 = 0x4356_4548;
const V4L2_PIX_FMT_VP8: u32 = 0x3038_5056;
const V4L2_PIX_FMT_VP9: u32 = 0x3039_5056;

/// The capture-queue format for a codec the kernel's interface names, or `None` for one it does
/// not. The queues, the controls, and the input format are the same whichever of these a node
/// carries, so the codec is a parameter rather than a second backend.
pub fn coded_fourcc(codec: Codec) -> Option<u32> {
    match codec {
        Codec::H264 => Some(V4L2_PIX_FMT_H264),
        Codec::H265 => Some(V4L2_PIX_FMT_HEVC),
        Codec::Vp8 => Some(V4L2_PIX_FMT_VP8),
        Codec::Vp9 => Some(V4L2_PIX_FMT_VP9),
        Codec::Av1 | Codec::Jpeg => None,
    }
}

const VIDIOC_QUERYCAP: u64 = 0x8068_5600;
const VIDIOC_ENUM_FMT: u64 = 0xc040_5602;
const VIDIOC_S_FMT: u64 = 0xc0d0_5605;
const VIDIOC_REQBUFS: u64 = 0xc014_5608;
const VIDIOC_QUERYBUF: u64 = 0xc058_5609;
const VIDIOC_QBUF: u64 = 0xc058_560f;
const VIDIOC_DQBUF: u64 = 0xc058_5611;
const VIDIOC_STREAMON: u64 = 0x4004_5612;
const VIDIOC_STREAMOFF: u64 = 0x4004_5613;
const VIDIOC_S_PARM: u64 = 0xc0cc_5616;
const VIDIOC_S_CTRL: u64 = 0xc008_561c;
const VIDIOC_ENUM_FRAMESIZES: u64 = 0xc02c_564a;

const CID_GOP_SIZE: u32 = 0x0099_09cb;
const CID_BITRATE_MODE: u32 = 0x0099_09ce;
const CID_BITRATE: u32 = 0x0099_09cf;
const CID_REPEAT_SEQ_HEADER: u32 = 0x0099_09e2;
const CID_FORCE_KEY_FRAME: u32 = 0x0099_09e5;
const CID_H264_I_PERIOD: u32 = 0x0099_0a66;

/// Variable is the only rate control such a device offers; see the note at the top about what
/// asking for constant does to the session.
const BITRATE_MODE_VBR: i32 = 0;

const FRMSIZE_TYPE_DISCRETE: u32 = 1;
const OUTPUT_BUFFERS: u32 = 1;
const CAPTURE_BUFFERS: usize = 2;
/// Long enough for a frame the hardware is slow on, short enough that a stalled node rebuilds the
/// session rather than holding the stream. A 1080p frame takes about 35 ms on a Pi 4.
const FRAME_TIMEOUT_MS: i32 = 500;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PlaneFormat {
    sizeimage: u32,
    bytesperline: u32,
    reserved: [u16; 6],
}

#[repr(C)]
struct PixFormatMplane {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    colorspace: u32,
    plane_fmt: [PlaneFormat; 8],
    num_planes: u8,
    flags: u8,
    enc: u8,
    quantization: u8,
    xfer_func: u8,
    reserved: [u8; 7],
}

#[repr(C)]
struct Format {
    type_: u32,
    _pad: u32,
    pix_mp: PixFormatMplane,
    _tail: [u8; 8],
}

#[repr(C)]
#[derive(Default)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Plane {
    bytesused: u32,
    length: u32,
    m: u64,
    data_offset: u32,
    reserved: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    _pad: u32,
    timestamp: [i64; 2],
    timecode: [u32; 4],
    sequence: u32,
    memory: u32,
    m: u64,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

/// The union of `v4l2_streamparm` begins one word in, not two: the frame rate lands in the wrong
/// place if this is laid out like `v4l2_format`.
#[repr(C)]
struct StreamParm {
    type_: u32,
    capability: u32,
    outputmode: u32,
    numerator: u32,
    denominator: u32,
    extendedmode: u32,
    buffers: u32,
    reserved: [u32; 4],
    _tail: [u8; 160],
}

impl Default for StreamParm {
    fn default() -> Self {
        Self {
            type_: 0,
            capability: 0,
            outputmode: 0,
            numerator: 0,
            denominator: 0,
            extendedmode: 0,
            buffers: 0,
            reserved: [0; 4],
            _tail: [0; 160],
        }
    }
}

#[repr(C)]
struct Control {
    id: u32,
    value: i32,
}

#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct FmtDesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct FrameSizeEnum {
    index: u32,
    pixel_format: u32,
    type_: u32,
    bounds: [u32; 6],
    reserved: [u32; 2],
}

/// The sizes the ioctl numbers above encode, checked once so a layout that drifted is a refusal
/// with a name rather than a scribble through a structure.
fn abi_matches() -> Result<(), String> {
    let sizes = [
        ("v4l2_capability", size_of::<Capability>(), 104),
        ("v4l2_fmtdesc", size_of::<FmtDesc>(), 64),
        ("v4l2_format", size_of::<Format>(), 208),
        ("v4l2_requestbuffers", size_of::<RequestBuffers>(), 20),
        ("v4l2_plane", size_of::<Plane>(), 64),
        ("v4l2_buffer", size_of::<Buffer>(), 88),
        ("v4l2_streamparm", size_of::<StreamParm>(), 204),
        ("v4l2_control", size_of::<Control>(), 8),
        ("v4l2_frmsizeenum", size_of::<FrameSizeEnum>(), 44),
    ];
    for (name, got, want) in sizes {
        if got != want {
            return Err(format!(
                "{name} is {got} bytes here, the interface wants {want}"
            ));
        }
    }
    Ok(())
}

unsafe fn ioctl<T>(fd: RawFd, request: u64, arg: *mut T) -> std::io::Result<()> {
    // The request takes `as _` rather than a named type: `libc::Ioctl` is `c_ulong` on
    // glibc and `c_int` on musl, so naming either one breaks the other's build.
    let rc = unsafe { libc::ioctl(fd, request as _, arg as *mut libc::c_void) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn empty_format(type_: u32) -> Format {
    Format {
        type_,
        _pad: 0,
        pix_mp: PixFormatMplane {
            width: 0,
            height: 0,
            pixelformat: 0,
            field: V4L2_FIELD_NONE,
            colorspace: 0,
            plane_fmt: [PlaneFormat::default(); 8],
            num_planes: 1,
            flags: 0,
            enc: 0,
            quantization: 0,
            xfer_func: 0,
            reserved: [0; 7],
        },
        _tail: [0; 8],
    }
}

/// The fourcc for the bytes a caller holds. The kernel spells these by channel order in a
/// word, so the name of each is the reverse of the memory it takes, and a session that picks
/// by name rather than by byte order swaps red and blue without failing.
fn input_format(rgba: bool) -> u32 {
    if rgba {
        V4L2_PIX_FMT_RGBA
    } else {
        V4L2_PIX_FMT_BGRA
    }
}

/// What an encode node offers, read once for the process.
struct NodeInfo {
    path: String,
    driver: String,
    /// The coded formats its capture queue enumerates, which is what it encodes.
    coded: Vec<u32>,
    min_width: u32,
    max_width: u32,
    step_width: u32,
    min_height: u32,
    max_height: u32,
    step_height: u32,
}

impl NodeInfo {
    /// Whether this node encodes `codec`.
    fn serves(&self, codec: Codec) -> bool {
        coded_fourcc(codec).is_some_and(|f| self.coded.contains(&f))
    }

    /// Whether a session of this size can come up, answered before anything is built: a refusal
    /// here falls through to software, a refusal later leaves a black stream.
    fn fits(&self, width: u32, height: u32) -> bool {
        let in_range = |value: u32, min: u32, max: u32, step: u32| {
            value >= min && value <= max && (step == 0 || (value - min).is_multiple_of(step))
        };
        in_range(width, self.min_width, self.max_width, self.step_width)
            && in_range(height, self.min_height, self.max_height, self.step_height)
    }
}

fn enumerates(fd: RawFd, type_: u32, wanted: u32) -> bool {
    for index in 0..64u32 {
        let mut desc = FmtDesc {
            index,
            type_,
            flags: 0,
            description: [0; 32],
            pixelformat: 0,
            mbus_code: 0,
            reserved: [0; 3],
        };
        if unsafe { ioctl(fd, VIDIOC_ENUM_FMT, &mut desc) }.is_err() {
            return false;
        }
        if desc.pixelformat == wanted {
            return true;
        }
    }
    false
}

fn frame_sizes(fd: RawFd, path: &str, coded: u32) -> Option<NodeInfo> {
    let mut sizes = FrameSizeEnum {
        index: 0,
        pixel_format: coded,
        type_: 0,
        bounds: [0; 6],
        reserved: [0; 2],
    };
    unsafe { ioctl(fd, VIDIOC_ENUM_FRAMESIZES, &mut sizes) }.ok()?;
    let (min_w, max_w, step_w, min_h, max_h, step_h) = if sizes.type_ == FRMSIZE_TYPE_DISCRETE {
        let (w, h) = (sizes.bounds[0], sizes.bounds[1]);
        (w, w, 0, h, h, 0)
    } else {
        (
            sizes.bounds[0],
            sizes.bounds[1],
            sizes.bounds[2],
            sizes.bounds[3],
            sizes.bounds[4],
            sizes.bounds[5],
        )
    };
    Some(NodeInfo {
        path: path.to_string(),
        driver: String::new(),
        coded: Vec::new(),
        min_width: min_w,
        max_width: max_w,
        step_width: step_w,
        min_height: min_h,
        max_height: max_h,
        step_height: step_h,
    })
}

/// The first node that encodes H.264 from packed color, found by asking rather than by name: the
/// number a board gives its encoder is not a contract, and the same interface appears on boards
/// with entirely different numbering.
fn node() -> Option<&'static NodeInfo> {
    static NODE: OnceLock<Option<NodeInfo>> = OnceLock::new();
    NODE.get_or_init(|| {
        if let Err(e) = abi_matches() {
            crate::log::debug!("[pixelflux] V4L2 M2M unusable: {e}");
            return None;
        }
        for index in 0..64 {
            let path = format!("/dev/video{index}");
            let Ok(c_path) = std::ffi::CString::new(path.clone()) else {
                continue;
            };
            let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
            if fd < 0 {
                continue;
            }
            let found = (|| {
                let mut caps: Capability = unsafe { std::mem::zeroed() };
                unsafe { ioctl(fd, VIDIOC_QUERYCAP, &mut caps) }.ok()?;
                if caps.device_caps & (V4L2_CAP_VIDEO_M2M_MPLANE | V4L2_CAP_VIDEO_M2M) == 0 {
                    return None;
                }
                let coded: Vec<u32> = [Codec::H264, Codec::H265, Codec::Vp8, Codec::Vp9]
                    .into_iter()
                    .filter_map(coded_fourcc)
                    .filter(|&f| enumerates(fd, V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, f))
                    .collect();
                if coded.is_empty() {
                    return None;
                }
                if !enumerates(fd, V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, V4L2_PIX_FMT_BGRA) {
                    return None;
                }
                let driver = String::from_utf8_lossy(&caps.driver)
                    .trim_end_matches('\0')
                    .to_string();
                frame_sizes(fd, &path, coded[0]).map(|info| NodeInfo {
                    driver,
                    coded,
                    ..info
                })
            })();
            unsafe { libc::close(fd) };
            if let Some(info) = found {
                println!(
                    "[pixelflux] V4L2 M2M encoder on {} up to {}x{}.",
                    info.path, info.max_width, info.max_height
                );
                return Some(info);
            }
        }
        None
    })
    .as_ref()
}

/// The rates to try, in order: what the session asked for, then the ones a device that refuses
/// it is likely to take. A device is asked at its own rate first, so one that can do it is never
/// slowed, and the steps below are the rates video hardware is built around rather than a search.
fn fallback_rates(wanted: f64) -> Vec<f64> {
    let mut rates = vec![wanted];
    for step in [30.0, 15.0] {
        if step < wanted - 0.01 {
            rates.push(step);
        }
    }
    rates
}

/// What a device is known to convert RGB with when it declares nothing itself. Only a device
/// whose behavior is on record earns an entry: a Raspberry Pi with firmware older than August
/// 2024 converts to full range BT.601 and writes no `video_signal_type`, which the driver's
/// author states in the firmware tracker and a decoded chart confirms. Guessing for a device not
/// on this list would put a claim in the stream that nobody has checked, and a wrong declaration
/// colors a picture worse than an absent one.
fn known_conversion(driver: &str) -> Option<ColorSignal> {
    match driver {
        "bcm2835-codec" => Some(ColorSignal::BT601_FULL),
        _ => None,
    }
}

/// What the session has learned about the color its device produces.
enum Color {
    /// No access unit has come back yet, and nothing is on record for this device either, so
    /// what the stream will say is not known until one arrives.
    Unknown,
    /// The device declares its own color, whatever it is, and the stream is left alone.
    Declared(ColorSignal),
    /// The device declares nothing and its conversion is on record, so every sequence parameter
    /// set is written with it on the way out.
    Writing(ColorSignal),
    /// The device declares nothing and nothing is known about it, so nothing is claimed.
    Untagged,
}

/// Whether this machine carries an M2M encode node at all.
pub fn available() -> bool {
    node().is_some()
}

/// Whether a session of this codec and size would come up on it, so the ladder can fall through
/// to software before a session exists rather than after one has failed.
pub fn encodes(codec: Codec, width: i32, height: i32) -> bool {
    match node() {
        Some(info) if width > 0 && height > 0 => {
            info.serves(codec) && info.fits(width as u32, height as u32)
        }
        _ => false,
    }
}

/// The codecs the node encodes, for the report a caller reads before any session exists.
pub fn served() -> Vec<Codec> {
    match node() {
        Some(info) => [Codec::H264, Codec::H265, Codec::Vp8, Codec::Vp9]
            .into_iter()
            .filter(|&c| info.serves(c))
            .collect(),
        None => Vec::new(),
    }
}

pub struct V4l2M2mEncoder {
    fd: RawFd,
    codec: Codec,
    coded: u32,
    width: i32,
    height: i32,
    row_bytes: usize,
    bytesperline: usize,
    output_size: usize,
    output_map: *mut libc::c_void,
    capture: [(*mut libc::c_void, usize); CAPTURE_BUFFERS],
    bitrate_bps: u32,
    fps: f64,
    omit_headers: bool,
    color: Color,
    /// What to declare when the device declares nothing, or `None` for a device off the record.
    fallback: Option<ColorSignal>,
    /// A sequence parameter set arrives with every key frame and is identical each time, so it is
    /// written once and matched by bytes after that.
    written_sps: Option<(Vec<u8>, Vec<u8>)>,
    /// Whether the first access unit has settled what the stream says.
    decided: bool,
    /// The reorder bound every H.264 set is held to. The session queues one frame and waits for
    /// its unit before the next, so pictures leave in the order they are shown whatever the
    /// device's picture order count: a device that reordered would wait for a frame never queued.
    reorder: sps::NoReorder,
}

unsafe impl Send for V4l2M2mEncoder {}

impl V4l2M2mEncoder {
    pub fn new(codec: Codec, settings: &CaptureSettings, rgba: bool) -> Result<Self, String> {
        let info = node().ok_or("no V4L2 M2M encode node")?;
        let coded = coded_fourcc(codec)
            .filter(|f| info.coded.contains(f))
            .ok_or_else(|| format!("{} encodes no {}", info.path, codec.display()))?;
        let (width, height) = (settings.width, settings.height);
        if width <= 0 || height <= 0 {
            return Err(format!(
                "the encoder needs positive dimensions, got {width}x{height}"
            ));
        }
        if !info.fits(width as u32, height as u32) {
            return Err(format!(
                "{} encodes {}x{} to {}x{}, not {width}x{height}",
                info.path, info.min_width, info.min_height, info.max_width, info.max_height
            ));
        }
        let path = std::ffi::CString::new(info.path.clone()).map_err(|e| e.to_string())?;
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return Err(format!(
                "{} did not open: {}",
                info.path,
                std::io::Error::last_os_error()
            ));
        }
        let mut encoder = Self {
            fd,
            codec,
            coded,
            width,
            height,
            row_bytes: width as usize * 4,
            bytesperline: 0,
            output_size: 0,
            output_map: libc::MAP_FAILED,
            capture: [(libc::MAP_FAILED, 0); CAPTURE_BUFFERS],
            bitrate_bps: (settings.video_bitrate_kbps.max(1) as u32).saturating_mul(1000),
            fps: settings.target_fps.max(1.0),
            omit_headers: settings.omit_stripe_headers,
            // A device on record is described from the start rather than from its first access
            // unit: the session line is printed before a frame exists, and a line saying limited
            // range while the stream will say full is a line that misleads whoever reads it.
            color: match known_conversion(&info.driver) {
                Some(signal) => Color::Writing(signal),
                None => Color::Unknown,
            },
            fallback: known_conversion(&info.driver),
            written_sps: None,
            decided: false,
            reorder: sps::NoReorder::new("The M2M encoder", true),
        };
        // A device answers "not at that rate" only by refusing, and only once the port is
        // enabled: the frame rate reaches the firmware at `STREAMON`, not at the ioctl that
        // carried it, and `VIDIOC_ENUM_FRAMEINTERVALS` is not implemented to ask beforehand. A
        // Raspberry Pi 4 takes 1080p at 30 and refuses it at 60, which is the rate a session
        // asks for by default, so a single attempt would leave a capable device unused.
        let mut last = String::new();
        for rate in fallback_rates(settings.target_fps.max(1.0)) {
            encoder.fps = rate;
            match encoder.setup(settings, rgba) {
                Ok(()) => {
                    if rate < settings.target_fps.max(1.0) {
                        println!(
                            "[pixelflux] The M2M encoder refused {:.0} frames a second and took {:.0}.",
                            settings.target_fps.max(1.0),
                            rate
                        );
                    }
                    return Ok(encoder);
                }
                Err(e) => {
                    last = e;
                    encoder.release();
                    let reopened = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
                    if reopened < 0 {
                        return Err(last);
                    }
                    encoder.fd = reopened;
                }
            }
        }
        Err(last)
    }

    /// Everything about the session that a refused `STREAMON` leaves behind, so the next attempt
    /// starts on a node that carries none of it. The descriptor itself is reopened by the caller:
    /// a device that refused a parameter answers `ESRCH` to every later call on that descriptor.
    fn release(&mut self) {
        let _ = self.stream(
            VIDIOC_STREAMOFF,
            V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            "STREAMOFF output",
        );
        let _ = self.stream(
            VIDIOC_STREAMOFF,
            V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            "STREAMOFF capture",
        );
        if self.output_map != libc::MAP_FAILED {
            unsafe { libc::munmap(self.output_map, self.output_size) };
            self.output_map = libc::MAP_FAILED;
        }
        for slot in 0..CAPTURE_BUFFERS {
            let (map, length) = self.capture[slot];
            if map != libc::MAP_FAILED {
                unsafe { libc::munmap(map, length) };
                self.capture[slot] = (libc::MAP_FAILED, 0);
            }
        }
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }

    fn setup(&mut self, settings: &CaptureSettings, rgba: bool) -> Result<(), String> {
        let input = input_format(rgba);
        let mut output_format = empty_format(V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE);
        output_format.pix_mp.width = self.width as u32;
        output_format.pix_mp.height = self.height as u32;
        output_format.pix_mp.pixelformat = input;
        output_format.pix_mp.plane_fmt[0].bytesperline = self.row_bytes as u32;
        output_format.pix_mp.plane_fmt[0].sizeimage =
            (self.row_bytes * self.height as usize) as u32;
        self.call(VIDIOC_S_FMT, &mut output_format, "S_FMT output")?;
        self.bytesperline = output_format.pix_mp.plane_fmt[0].bytesperline as usize;
        self.output_size = output_format.pix_mp.plane_fmt[0].sizeimage as usize;
        if self.bytesperline < self.row_bytes
            || self.output_size < self.bytesperline * self.height as usize
        {
            return Err(format!(
                "the node offered {} bytes per line and {} per frame for {}x{}",
                self.bytesperline, self.output_size, self.width, self.height
            ));
        }

        let mut capture_format = empty_format(V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        capture_format.pix_mp.width = self.width as u32;
        capture_format.pix_mp.height = self.height as u32;
        capture_format.pix_mp.pixelformat = self.coded;
        capture_format.pix_mp.plane_fmt[0].sizeimage =
            ((self.width * self.height) as u32).max(2 << 20);
        self.call(VIDIOC_S_FMT, &mut capture_format, "S_FMT capture")?;

        self.set_frame_rate(self.fps)?;
        let seconds = if settings.keyframe_interval_s > 0.0 {
            settings.keyframe_interval_s
        } else {
            10.0
        };
        let keyframe = ((self.fps * seconds) as i32).clamp(1, 600);
        self.set_control(CID_BITRATE_MODE, BITRATE_MODE_VBR, "rate control mode")?;
        self.set_control(CID_BITRATE, self.bitrate_bps as i32, "bitrate")?;
        self.set_control(
            CID_REPEAT_SEQ_HEADER,
            1,
            "sequence header with every key frame",
        )?;
        self.set_control(CID_H264_I_PERIOD, keyframe, "key frame period")?;
        self.set_control(CID_GOP_SIZE, keyframe, "GOP size")?;

        self.request_buffers(V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, OUTPUT_BUFFERS)?;
        self.request_buffers(V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, CAPTURE_BUFFERS as u32)?;
        self.output_map = self.map_buffer(V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, 0)?.0;
        for index in 0..CAPTURE_BUFFERS {
            self.capture[index] =
                self.map_buffer(V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, index as u32)?;
        }

        self.stream(
            VIDIOC_STREAMON,
            V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            "STREAMON capture",
        )?;
        self.stream(
            VIDIOC_STREAMON,
            V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            "STREAMON output",
        )?;
        for index in 0..CAPTURE_BUFFERS {
            self.queue(V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, index as u32, 0)?;
        }
        Ok(())
    }

    fn call<T>(&self, request: u64, arg: &mut T, what: &str) -> Result<(), String> {
        unsafe { ioctl(self.fd, request, arg) }.map_err(|e| format!("{what} failed: {e}"))
    }

    /// A control this device refuses is a refusal of the session. It is not a warning to carry on
    /// past: a rejected rate control mode leaves the node answering `ESRCH` to `STREAMON` until it
    /// is closed, so a session that shrugged here would fail later and in a place that says
    /// nothing about the cause.
    fn set_control(&self, id: u32, value: i32, what: &str) -> Result<(), String> {
        let mut control = Control { id, value };
        self.call(VIDIOC_S_CTRL, &mut control, &format!("setting {what}"))
    }

    fn set_frame_rate(&mut self, fps: f64) -> Result<(), String> {
        let mut parm = StreamParm {
            type_: V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            numerator: 1000,
            denominator: (fps * 1000.0).round().max(1000.0) as u32,
            ..Default::default()
        };
        self.call(VIDIOC_S_PARM, &mut parm, "S_PARM frame rate")?;
        self.fps = fps;
        Ok(())
    }

    fn request_buffers(&self, type_: u32, count: u32) -> Result<(), String> {
        let mut request = RequestBuffers {
            count,
            type_,
            memory: V4L2_MEMORY_MMAP,
            ..Default::default()
        };
        self.call(VIDIOC_REQBUFS, &mut request, "REQBUFS")?;
        if request.count < count {
            return Err(format!(
                "the node gave {} buffers of the {count} asked for",
                request.count
            ));
        }
        Ok(())
    }

    fn map_buffer(&self, type_: u32, index: u32) -> Result<(*mut libc::c_void, usize), String> {
        let mut planes = [Plane::default(); 1];
        let mut buffer = Buffer {
            index,
            type_,
            memory: V4L2_MEMORY_MMAP,
            m: planes.as_mut_ptr() as u64,
            length: 1,
            ..Default::default()
        };
        self.call(VIDIOC_QUERYBUF, &mut buffer, "QUERYBUF")?;
        let length = planes[0].length as usize;
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.fd,
                planes[0].m as libc::off_t,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(format!(
                "mapping a buffer failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok((map, length))
    }

    fn queue(&self, type_: u32, index: u32, bytesused: u32) -> Result<(), String> {
        let mut planes = [Plane::default(); 1];
        planes[0].bytesused = bytesused;
        let mut buffer = Buffer {
            index,
            type_,
            memory: V4L2_MEMORY_MMAP,
            m: planes.as_mut_ptr() as u64,
            length: 1,
            ..Default::default()
        };
        self.call(VIDIOC_QBUF, &mut buffer, "QBUF")
    }

    fn dequeue(&self, type_: u32) -> Result<(u32, usize), String> {
        let mut planes = [Plane::default(); 1];
        let mut buffer = Buffer {
            type_,
            memory: V4L2_MEMORY_MMAP,
            m: planes.as_mut_ptr() as u64,
            length: 1,
            ..Default::default()
        };
        self.call(VIDIOC_DQBUF, &mut buffer, "DQBUF")?;
        Ok((buffer.index, planes[0].bytesused as usize))
    }

    fn stream(&self, request: u64, type_: u32, what: &str) -> Result<(), String> {
        let mut argument = type_ as i32;
        self.call(request, &mut argument, what)
    }

    /// Wait for the access unit of the frame just queued. The node is opened blocking, so this
    /// exists to bound the wait: a device that stopped answering has to become an error the ladder
    /// can act on, not a stream that never returns.
    fn wait_for_frame(&self) -> Result<(), String> {
        let mut poll = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, FRAME_TIMEOUT_MS) };
        match ready {
            1 => Ok(()),
            0 => Err(format!(
                "the encoder produced nothing in {FRAME_TIMEOUT_MS} ms"
            )),
            _ => Err(format!(
                "waiting for a frame failed: {}",
                std::io::Error::last_os_error()
            )),
        }
    }

    fn fill_output(&self, pixels: &[u8], stride: usize) -> Result<(), String> {
        let height = self.height as usize;
        let needed = stride * (height - 1) + self.row_bytes;
        if stride < self.row_bytes || pixels.len() < needed {
            return Err("input buffer too small".into());
        }
        let destination = self.output_map as *mut u8;
        if stride == self.bytesperline {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr(),
                    destination,
                    self.bytesperline * height,
                )
            };
            return Ok(());
        }
        for row in 0..height {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(row * stride),
                    destination.add(row * self.bytesperline),
                    self.row_bytes,
                );
            }
        }
        Ok(())
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// The wire picture type of one access unit, read with the codec's own syntax: an H.265 NAL
    /// header is two bytes where H.264's is one, and VP8 and VP9 carry no NAL units at all.
    fn frame_type(&self, bytes: &[u8]) -> u8 {
        match self.codec {
            Codec::H265 => h265_frame_type(bytes),
            Codec::Vp8 => frame_type_from_key(vp8_is_key(bytes)),
            Codec::Vp9 => frame_type_from_key(vp9_is_key(bytes)),
            _ => h264_frame_type(bytes),
        }
    }

    /// The firmware converts to 4:2:0 and the encoder takes nothing else.
    pub fn is_fullcolor(&self) -> bool {
        false
    }

    /// What the stream says about its color, once an access unit has settled it.
    fn signal(&self) -> Option<ColorSignal> {
        match self.color {
            Color::Declared(signal) | Color::Writing(signal) => Some(signal),
            Color::Unknown | Color::Untagged => None,
        }
    }

    /// The kernel's stateful interface names no reference and takes no invalidation.
    pub fn last_reference(&self) -> Reference {
        Reference::Untracked
    }

    pub fn invalidate_reference(&mut self, _frame_id: u16) -> bool {
        false
    }

    /// The range the stream declares, which is the device's to decide: a session that says
    /// nothing is described as limited, since that is what a decoder given nothing assumes.
    pub fn is_full_range(&self) -> bool {
        self.signal().is_some_and(|signal| signal.full_range)
    }

    /// Learn what the first sequence parameter set says, and write one into every later set when
    /// the device said nothing and its conversion is on record; then hold every set to a reorder
    /// bound of zero. Only H.264 carries either here: an H.265 set names its own reordering, and
    /// VP8 and VP9 carry no parameter sets.
    fn tag_sequence(&mut self, unit: &[u8]) -> Option<Vec<u8>> {
        if self.codec != Codec::H264 {
            return None;
        }
        let sets = sps::sequence_parameter_sets(unit);
        if sets.is_empty() {
            return None;
        }
        if !self.decided {
            self.decided = true;
            let (start, end) = sets[0];
            self.color = match sps::read_color(&unit[start..end]) {
                Some(signal) => {
                    println!(
                        "[pixelflux] The M2M encoder declares its own color: matrix {}, {} range.",
                        signal.matrix,
                        if signal.full_range { "full" } else { "limited" }
                    );
                    Color::Declared(signal)
                }
                None => match self.fallback {
                    Some(signal) => {
                        println!(
                            "[pixelflux] The M2M encoder declares no color; writing matrix {}, {} range into its headers.",
                            signal.matrix,
                            if signal.full_range { "full" } else { "limited" }
                        );
                        Color::Writing(signal)
                    }
                    None => {
                        eprintln!(
                            "[pixelflux] The M2M encoder declares no color and none is on record for it; the stream carries none, and a decoder will guess from the frame size."
                        );
                        Color::Untagged
                    }
                },
            };
        }

        let mut out: Option<Vec<u8>> = None;
        let mut copied = 0;
        for (start, end) in sets {
            let original = &unit[start..end];
            let colored = match self.color {
                Color::Writing(signal) => self.write_color(original, signal),
                _ => None,
            };
            let set = colored.as_deref().unwrap_or(original);
            let Some(replacement) = self.reorder.set(set).or(colored) else {
                continue;
            };
            let out = out.get_or_insert_with(|| Vec::with_capacity(unit.len() + 16 * 4));
            out.extend_from_slice(&unit[copied..start]);
            out.extend_from_slice(&replacement);
            copied = end;
        }
        let mut out = out?;
        out.extend_from_slice(&unit[copied..]);
        Some(out)
    }

    /// `original` declaring `signal`, or None where the set cannot be written, in which case it
    /// is left as it came: a stream that carries no color is watchable, and one carrying a set
    /// built wrong is not.
    fn write_color(&mut self, original: &[u8], signal: ColorSignal) -> Option<Vec<u8>> {
        if let Some((was, now)) = &self.written_sps
            && was == original
        {
            return Some(now.clone());
        }
        match sps::write_color(original, signal) {
            Ok(written) => {
                self.written_sps = Some((original.to_vec(), written.clone()));
                Some(written)
            }
            Err(e) => {
                if self.written_sps.is_none() {
                    eprintln!("[pixelflux] The sequence parameter set was left as it came: {e}");
                    self.color = Color::Untagged;
                }
                None
            }
        }
    }

    /// Apply a live bitrate or frame rate change. Both are writable while streaming on this path,
    /// and the frame rate goes down with the bitrate: left behind, it would budget a new bitrate
    /// against an old rate.
    pub fn reconfigure_rate(&mut self, settings: &CaptureSettings) -> Result<(), String> {
        let wanted = (settings.video_bitrate_kbps.max(1) as u32).saturating_mul(1000);
        let fps = settings.target_fps.max(1.0);
        if (fps - self.fps).abs() > 0.01 {
            self.set_frame_rate(fps)?;
        }
        if wanted != self.bitrate_bps {
            self.set_control(CID_BITRATE, wanted as i32, "bitrate")?;
            self.bitrate_bps = wanted;
        }
        Ok(())
    }

    pub fn encode_host(
        &mut self,
        pixels: &[u8],
        stride: usize,
        _rgba: bool,
        frame_number: u64,
        _qp: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        self.fill_output(pixels, stride)?;
        if force_idr {
            self.set_control(CID_FORCE_KEY_FRAME, 1, "force key frame")?;
        }
        self.queue(
            V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            0,
            self.output_size as u32,
        )?;
        self.wait_for_frame()?;
        let (index, length) = self.dequeue(V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE)?;
        let slot = index as usize;
        if slot >= CAPTURE_BUFFERS {
            return Err(format!("the node returned capture buffer {slot}"));
        }
        let (data, _) = self.capture[slot];
        let unit = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
        let out = self.frame(unit, frame_number);
        self.queue(V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE, index, 0)?;
        self.dequeue(V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE)?;
        Ok(out)
    }

    /// One access unit as the wire carries it: its sequence parameter sets tagged, behind a
    /// header naming the session's codec unless the caller frames the stream itself.
    fn frame(&mut self, unit: &[u8], frame_number: u64) -> Vec<u8> {
        let tagged = self.tag_sequence(unit);
        let bytes = tagged.as_deref().unwrap_or(unit);
        let mut out = Vec::with_capacity(VIDEO_HEADER_LEN + bytes.len());
        if !self.omit_headers {
            push_video_header(
                &mut out,
                self.codec,
                self.frame_type(bytes),
                frame_number as u16,
                0,
                self.width as u16,
                self.height as u16,
                Reference::Untracked,
            );
        }
        out.extend_from_slice(bytes);
        out
    }
}

impl Drop for V4l2M2mEncoder {
    fn drop(&mut self) {
        let _ = self.stream(
            VIDIOC_STREAMOFF,
            V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            "STREAMOFF output",
        );
        let _ = self.stream(
            VIDIOC_STREAMOFF,
            V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            "STREAMOFF capture",
        );
        if self.output_map != libc::MAP_FAILED {
            unsafe { libc::munmap(self.output_map, self.output_size) };
        }
        for (map, length) in self.capture {
            if map != libc::MAP_FAILED {
                unsafe { libc::munmap(map, length) };
            }
        }
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoders::sps::fixtures::{PI4_SPS, VCE_SPS, reorder_of, without};

    /// A session of `codec` on a device of `driver` that never opened a node, for what a session
    /// does with the units a device hands back.
    fn stand_in(codec: Codec, driver: &str) -> V4l2M2mEncoder {
        V4l2M2mEncoder {
            fd: -1,
            codec,
            coded: coded_fourcc(codec).unwrap_or(0),
            width: 1280,
            height: 720,
            row_bytes: 1280 * 4,
            bytesperline: 0,
            output_size: 0,
            output_map: libc::MAP_FAILED,
            capture: [(libc::MAP_FAILED, 0); CAPTURE_BUFFERS],
            bitrate_bps: 8_000_000,
            fps: 30.0,
            omit_headers: true,
            color: match known_conversion(driver) {
                Some(signal) => Color::Writing(signal),
                None => Color::Unknown,
            },
            fallback: known_conversion(driver),
            written_sps: None,
            decided: false,
            reorder: sps::NoReorder::new("The M2M encoder", true),
        }
    }

    /// An access unit as a device hands it back: the set, a PPS, and an IDR slice.
    fn unit(sps: &[u8]) -> Vec<u8> {
        [
            &[0, 0, 0, 1][..],
            sps,
            &[0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80],
            &[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x21],
        ]
        .concat()
    }

    /// The first set of an access unit.
    fn first_set(unit: &[u8]) -> Vec<u8> {
        let (start, end) = sps::sequence_parameter_sets(unit)[0];
        unit[start..end].to_vec()
    }

    /// Every H.264 set a device hands back leaves bounded at zero: the session queues a frame
    /// and waits for its unit, so nothing it encodes can reorder whatever picture order count
    /// the device writes. A device that declares its own color keeps it.
    #[test]
    fn a_device_set_leaves_bounded_at_zero() {
        let mut session = stand_in(Codec::H264, "unknown-codec");
        let tagged = session
            .tag_sequence(&unit(VCE_SPS))
            .expect("a set to bound");
        assert_eq!(reorder_of(&first_set(&tagged)), Some((0, 2)));
        assert_eq!(
            sps::read_color(&first_set(&tagged)),
            sps::read_color(VCE_SPS),
            "the device's own color moved"
        );
        assert!(
            tagged.ends_with(&unit(VCE_SPS)[4 + VCE_SPS.len()..]),
            "the units after the set changed"
        );
        assert_eq!(
            session.tag_sequence(&unit(VCE_SPS)),
            Some(tagged),
            "the next key frame's set differs"
        );
        assert_eq!(
            session.tag_sequence(&[0, 0, 0, 1, 0x41, 0x9a, 0x02]),
            None,
            "a delta without a set was touched"
        );
    }

    /// A Raspberry Pi 4 on an old firmware declares no color and bounds its own stream: the
    /// color on record is written and the bound it declared survives the write; without its own
    /// bound it is given one next to the color.
    #[test]
    fn the_pi_color_and_the_bound_are_written_together() {
        let mut session = stand_in(Codec::H264, "bcm2835-codec");
        let tagged = first_set(
            &session
                .tag_sequence(&unit(PI4_SPS))
                .expect("a color to write"),
        );
        assert_eq!(sps::read_color(&tagged), Some(ColorSignal::BT601_FULL));
        assert_eq!(reorder_of(&tagged), Some((0, 1)));
        let mut session = stand_in(Codec::H264, "bcm2835-codec");
        let bare = without(PI4_SPS, false);
        let tagged = first_set(&session.tag_sequence(&unit(&bare)).expect("a set to write"));
        assert_eq!(sps::read_color(&tagged), Some(ColorSignal::BT601_FULL));
        assert_eq!(reorder_of(&tagged), Some((0, 1)));
    }

    /// The wire header names the codec the node encodes, which is what a client builds its
    /// decoder from: an H.265 or VP8 unit labeled H.264 is decoded as H.264.
    #[test]
    fn the_header_names_the_session_codec() {
        use crate::encoders::codec::{FRAME_KEY, parse_video_type};
        let units: [(Codec, Vec<u8>); 3] = [
            (Codec::H264, unit(VCE_SPS)),
            (
                Codec::H265,
                vec![
                    0, 0, 0, 1, 0x40, 0x01, 0x0c, 0, 0, 0, 1, 0x26, 0x01, 0xaf, 0x10,
                ],
            ),
            (
                Codec::Vp8,
                vec![0x50, 0x2a, 0x00, 0x9d, 0x01, 0x2a, 0x00, 0x05, 0xd0, 0x02],
            ),
        ];
        for (codec, bytes) in units {
            let mut session = stand_in(codec, "unknown-codec");
            session.omit_headers = false;
            let framed = session.frame(&bytes, 7);
            assert_eq!(
                parse_video_type(framed[1]),
                Some((codec, FRAME_KEY)),
                "{codec:?}"
            );
            assert_eq!(&framed[2..4], &[0, 7], "{codec:?}: the frame number");
        }
    }

    /// Only H.264 carries these sets: bytes of another codec that happen to look like one are
    /// passed through untouched.
    #[test]
    fn other_codecs_are_passed_through() {
        for codec in [Codec::H265, Codec::Vp8, Codec::Vp9] {
            let mut session = stand_in(codec, "bcm2835-codec");
            assert_eq!(session.tag_sequence(&unit(VCE_SPS)), None, "{codec:?}");
        }
    }

    /// The structures are laid out for the sizes the ioctl numbers encode, so a layout that
    /// drifted is caught here rather than by a device scribbling through a field.
    #[test]
    fn layouts_match_the_ioctl_numbers() {
        abi_matches().expect("the V4L2 structures are laid out as their ioctl numbers say");
    }

    fn node(min: u32, max: u32, step: u32) -> NodeInfo {
        NodeInfo {
            path: "/dev/videoX".to_string(),
            driver: "test".to_string(),
            coded: vec![V4L2_PIX_FMT_H264],
            min_width: min,
            max_width: max,
            step_width: step,
            min_height: min,
            max_height: max,
            step_height: step,
        }
    }

    /// A node is taken for what its capture queue enumerates, so one that encodes more than
    /// H.264 serves more than H.264 and one that encodes less is not asked for it.
    #[test]
    fn a_node_serves_the_codecs_it_enumerates() {
        let mut info = node(16, 1920, 2);
        info.coded = vec![V4L2_PIX_FMT_H264, V4L2_PIX_FMT_HEVC];
        assert!(info.serves(Codec::H264) && info.serves(Codec::H265));
        assert!(
            !info.serves(Codec::Vp8),
            "a format it does not enumerate is not served"
        );
        assert!(
            !info.serves(Codec::Av1),
            "and one the interface cannot name is never served"
        );
        info.coded = vec![V4L2_PIX_FMT_VP9];
        assert!(
            !info.serves(Codec::H264),
            "a node without H.264 is not asked for it"
        );
        assert!(info.serves(Codec::Vp9));
    }

    /// The fourccs are the kernel's own, and an interface that cannot name a codec says so
    /// rather than guessing one.
    #[test]
    fn a_codec_maps_to_the_fourcc_the_kernel_names() {
        assert_eq!(
            coded_fourcc(Codec::H264),
            Some(u32::from_le_bytes(*b"H264"))
        );
        assert_eq!(
            coded_fourcc(Codec::H265),
            Some(u32::from_le_bytes(*b"HEVC"))
        );
        assert_eq!(coded_fourcc(Codec::Vp8), Some(u32::from_le_bytes(*b"VP80")));
        assert_eq!(coded_fourcc(Codec::Vp9), Some(u32::from_le_bytes(*b"VP90")));
        assert_eq!(coded_fourcc(Codec::Av1), None);
        assert_eq!(coded_fourcc(Codec::Jpeg), None);
    }

    #[test]
    fn a_node_takes_the_sizes_inside_its_range_and_on_its_step() {
        let stepwise = node(32, 1920, 2);
        assert!(stepwise.fits(1920, 1080), "1080p is inside 32..1920 by 2");
        assert!(
            stepwise.fits(1920, 1200),
            "the ceiling is square, so 1920x1200 fits"
        );
        assert!(
            !stepwise.fits(2560, 1440),
            "a size past the ceiling does not fit"
        );
        assert!(
            !stepwise.fits(30, 30),
            "a size below the floor does not fit"
        );
        assert!(
            !stepwise.fits(1921, 1080),
            "an odd width is off a step of two"
        );
    }

    /// A device that publishes one size answers for that size alone, which a step of zero says.
    #[test]
    fn a_discrete_node_takes_only_the_size_it_publishes() {
        let discrete = node(1280, 1280, 0);
        assert!(discrete.fits(1280, 1280));
        assert!(!discrete.fits(1281, 1280));
    }

    #[test]
    fn the_input_format_follows_the_bytes_the_caller_holds() {
        assert_eq!(
            input_format(false),
            V4L2_PIX_FMT_BGRA,
            "an X11 capture hands over B,G,R,A"
        );
        assert_eq!(
            input_format(true),
            V4L2_PIX_FMT_RGBA,
            "an rgba caller hands over R,G,B,A"
        );
        assert_ne!(V4L2_PIX_FMT_BGRA, V4L2_PIX_FMT_RGBA);
    }
}

/// Tests that need the device, in the spirit of the `gpu_` ones: run them with
/// `cargo test v4l2_ -- --ignored --test-threads=1` on a board that carries an M2M encode node.
/// They are serial by nature, since a node of one context refuses a second session.
#[cfg(test)]
mod hardware_tests {
    use super::*;

    fn settings(width: i32, height: i32) -> CaptureSettings {
        CaptureSettings {
            width,
            height,
            codec: Codec::H264,
            video_bitrate_kbps: 8000,
            target_fps: 30.0,
            keyframe_interval_s: 2.0,
            omit_stripe_headers: true,
            ..Default::default()
        }
    }

    /// A strip of primaries in the byte order a capture hands over: B, G, R, A.
    fn chart(width: usize, height: usize) -> Vec<u8> {
        let colors = [
            [255u8, 255, 255],
            [0, 0, 0],
            [0, 0, 255],
            [0, 255, 0],
            [255, 0, 0],
        ];
        let mut frame = Vec::with_capacity(width * height * 4);
        for _ in 0..height {
            for x in 0..width {
                let [b, g, r] = colors[(x * colors.len()) / width];
                frame.extend_from_slice(&[b, g, r, 255]);
            }
        }
        frame
    }

    /// The NAL unit types an Annex B stream carries, in order.
    fn nal_types(stream: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 3 < stream.len() {
            if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
                types.push(stream[i + 3] & 0x1f);
                i += 4;
            } else {
                i += 1;
            }
        }
        types
    }

    #[test]
    #[ignore]
    fn v4l2_node_answers_for_the_sizes_it_encodes() {
        assert!(available(), "no M2M encode node on this machine");
        assert!(
            encodes(Codec::H264, 1920, 1080),
            "1080p is inside every such device's range"
        );
        assert!(!encodes(Codec::H264, 0, 0), "a zero size is not encodable");
    }

    #[test]
    #[ignore]
    fn v4l2_encodes_host_frames_to_h264() {
        let mut encoder = V4l2M2mEncoder::new(Codec::H264, &settings(1280, 720), false)
            .expect("the session comes up");
        let frame = chart(1280, 720);
        let first = encoder
            .encode_host(&frame, 1280 * 4, false, 0, 26, true)
            .expect("first frame");
        let types = nal_types(&first);
        assert!(!first.is_empty(), "the first access unit is empty");
        assert!(
            types.contains(&7),
            "the key frame carries no SPS: {types:?}"
        );
        assert!(
            types.contains(&5),
            "the first frame is not an IDR: {types:?}"
        );

        let second = encoder
            .encode_host(&frame, 1280 * 4, false, 1, 26, false)
            .expect("second");
        assert!(!second.is_empty(), "the second access unit is empty");
        assert!(
            second.len() < first.len(),
            "an unchanged frame coded no smaller than the key frame it follows: {} vs {}",
            second.len(),
            first.len()
        );
    }

    /// The live rate change a dashboard slider makes, which a session must survive: a refusal
    /// here demotes the stream to software for good on the second occurrence.
    #[test]
    #[ignore]
    fn v4l2_takes_a_live_rate_change() {
        let mut encoder = V4l2M2mEncoder::new(Codec::H264, &settings(1280, 720), false)
            .expect("the session comes up");
        let frame = chart(1280, 720);
        encoder
            .encode_host(&frame, 1280 * 4, false, 0, 26, true)
            .expect("first frame");
        let mut lowered = settings(1280, 720);
        lowered.video_bitrate_kbps = 1000;
        lowered.target_fps = 15.0;
        encoder
            .reconfigure_rate(&lowered)
            .expect("the device takes a live rate change");
        encoder
            .encode_host(&frame, 1280 * 4, false, 1, 26, false)
            .expect("a frame after it");
    }

    /// What the device costs at 1080p, printed to quote rather than asserted: the frame rate it
    /// sustains one frame in one out, and the CPU that rate spends. The software figure it is
    /// worth against comes from a session, not from here — this path takes BGRA and the software
    /// one takes planes, so a comparison made here would leave out the conversion the software
    /// path pays for and measure two different jobs.
    #[test]
    #[ignore]
    fn v4l2_bench_1080p() {
        const FRAMES: u64 = 90;
        let (width, height) = (1920, 1080);
        let (w, h) = (width as usize, height as usize);
        // Three contents, because one number would misrepresent the device: a still desktop is
        // what it is best at, a desktop with a window moving is what it is asked for, and noise
        // is the worst case no screen produces.
        let contents: [(&str, Vec<Vec<u8>>); 3] = [
            ("still  ", vec![chart(w, h)]),
            ("moving ", (0..4).map(|n| shifted_chart(w, h, n)).collect()),
            ("noise  ", (0..4).map(|n| moving(w, h, n)).collect()),
        ];
        for (name, frames) in contents {
            let mut encoder = V4l2M2mEncoder::new(Codec::H264, &settings(width, height), false)
                .expect("the session comes up");
            let cpu_before = process_cpu_seconds();
            let started = std::time::Instant::now();
            let mut bytes = 0usize;
            for n in 0..FRAMES {
                let frame = &frames[(n as usize) % frames.len()];
                bytes += encoder
                    .encode_host(frame, w * 4, false, n, 26, n == 0)
                    .expect("a frame")
                    .len();
            }
            let wall = started.elapsed().as_secs_f64();
            let cpu = process_cpu_seconds() - cpu_before;
            println!(
                "[v4l2-bench] {width}x{height} {name}: {:.1} fps serial, {:.2} ms per frame, \
                 CPU {:.2} ms per frame, {:.3} of a core at 30 fps, {:.0} kbit/s",
                FRAMES as f64 / wall,
                wall * 1000.0 / FRAMES as f64,
                cpu * 1000.0 / FRAMES as f64,
                cpu / FRAMES as f64 * 30.0,
                bytes as f64 * 8.0 / 1000.0 / (FRAMES as f64 / 30.0)
            );
        }
    }

    /// CPU charged to this process, user and system, across every thread.
    fn process_cpu_seconds() -> f64 {
        let mut spec = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut spec) };
        spec.tv_sec as f64 + spec.tv_nsec as f64 / 1e9
    }

    /// The chart with its bars walked sideways, which is what a window being dragged looks like
    /// to an encoder: flat areas that move rather than pixels that change everywhere.
    fn shifted_chart(width: usize, height: usize, step: usize) -> Vec<u8> {
        let colors = [
            [255u8, 255, 255],
            [0, 0, 0],
            [0, 0, 255],
            [0, 255, 0],
            [255, 0, 0],
        ];
        let mut frame = Vec::with_capacity(width * height * 4);
        for _ in 0..height {
            for x in 0..width {
                let shifted = (x + step * 64) % width;
                let [b, g, r] = colors[(shifted * colors.len()) / width];
                frame.extend_from_slice(&[b, g, r, 255]);
            }
        }
        frame
    }

    /// A frame whose content moves, so the encoder has something to spend bits on.
    fn moving(width: usize, height: usize, step: usize) -> Vec<u8> {
        let mut frame = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let value = ((x + step * 37) ^ (y + step * 11)) as u8;
                frame.extend_from_slice(&[
                    value,
                    value.wrapping_mul(3),
                    value.wrapping_mul(7),
                    255,
                ]);
            }
        }
        frame
    }

    /// A rate the device will not take does not cost the session its hardware. A Raspberry Pi 4
    /// refuses 1080p at 60 and takes it at 30, and it says so only by refusing `STREAMON` on a
    /// descriptor that answers nothing afterwards, so the backend reopens and asks for less.
    #[test]
    #[ignore]
    fn v4l2_steps_down_from_a_rate_the_device_refuses() {
        let mut asked = settings(1920, 1080);
        asked.target_fps = 60.0;
        let encoder = V4l2M2mEncoder::new(Codec::H264, &asked, false)
            .expect("a session came up at some rate the device takes");
        assert!(
            encoder.fps <= 60.0,
            "the session reports a rate it never asked for"
        );
        assert!(encoder.fps >= 15.0, "the session stepped below the floor");
    }

    /// What the stream declares, read back by a decoder that is not ours: OpenH264 decodes the
    /// stream and the sequence parameter set the session carries reports the signal in it.
    /// This is the check that a written set is a set a decoder accepts, rather than one our
    /// own reader agrees with.
    #[test]
    #[ignore]
    fn v4l2_the_color_reads_back_through_a_decoder() {
        use crate::webcam::decode::{ColorTags, Decoder as _, VideoDecoder};

        let mut encoder = V4l2M2mEncoder::new(Codec::H264, &settings(1280, 720), false)
            .expect("the session comes up");
        let frame = chart(1280, 720);
        let unit = encoder
            .encode_host(&frame, 1280 * 4, false, 0, 26, true)
            .expect("a frame");
        let mut decoder = VideoDecoder::new(Codec::H264).expect("a decoder");
        assert!(
            decoder.decode(&unit).expect("the stream decodes"),
            "no frame came back"
        );

        let Some(signal) = encoder.signal() else {
            assert_eq!(
                decoder.color_tags().map(|tags| tags.matrix),
                Some(2),
                "the session claims nothing, so the stream must claim nothing either"
            );
            return;
        };
        assert_eq!(
            decoder.color_tags(),
            Some(ColorTags {
                matrix: signal.matrix,
                full_range: signal.full_range
            }),
            "the decoder reads a different signal than the session declares"
        );
    }

    #[test]
    #[ignore]
    fn v4l2_refuses_a_size_past_the_ceiling() {
        let info = node().expect("a node");
        let too_wide = (info.max_width + info.step_width.max(2)) as i32;
        assert!(
            !encodes(Codec::H264, too_wide, 1080),
            "a size past the ceiling reported as encodable"
        );
        let error = match V4l2M2mEncoder::new(Codec::H264, &settings(too_wide, 1080), false) {
            Ok(_) => panic!("a session past the ceiling came up"),
            Err(e) => e,
        };
        assert!(
            error.contains("encodes"),
            "the refusal does not name the range: {error}"
        );
    }
}
